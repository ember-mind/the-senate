//! Mission use cases: create and shape the plan, start packages as child
//! runs, and keep package state in step with committed run evidence.
//!
//! The service owns ordering and persistence; the aggregate owns the rules.
//! Every mutation loads the mission, applies one domain operation, and
//! commits the aggregate with its event batch under compare-and-swap.
//! Run state is never inferred from anything but the run store.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::domain::{
    DecisionAuthor, DecisionId, IntegrationEvidence, Mission, MissionChange, MissionError,
    MissionId, PlanChange, RunId, RunStatus, WorkPackage, WorkPackageContract, WorkPackageId,
    WorkPackageStatus, WorkflowKind,
};
use crate::git::GitRepository;
use crate::store::{
    LoadedMission, MissionHandoffRecord, MissionInput, MissionRevision, SqliteStore,
    contract_sha256, database_file, sha256_hex, worktree_root,
};
use crate::workspace::WorkspaceStatus;

use super::mission_lead::{self, LeadAnswer, LeadTurn};
use super::mission_query::{self, MissionDetails, MissionListItem};
use super::mission_result;
use super::provider_factory::ProviderFactory;
use super::{
    AppError, ApplyOutcome, EffortRequest, ExecutionReport, ExecutionSelection,
    ImageGenerationPlan, RunService, StartProgress, query,
};

/// Use-case boundary for missions. Opens the store per call, like
/// [`RunService`], so several processes can share one database.
pub struct MissionService {
    database: PathBuf,
    /// Root of the managed worktrees, read to capture a delivered run's
    /// delta; never written from here.
    worktrees: PathBuf,
}

/// One package as the lead or user describes it before it exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewWorkPackage {
    pub id: WorkPackageId,
    pub contract: WorkPackageContract,
    pub dependencies: Vec<WorkPackageId>,
}

/// How a delivered package goes back to work on its own run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rework {
    /// Answer the run's own review findings.
    Fix,
    /// Carry an operator instruction.
    Continue(String),
}

impl MissionService {
    #[must_use]
    pub const fn new(database: PathBuf, worktrees: PathBuf) -> Self {
        Self {
            database,
            worktrees,
        }
    }

    /// Resolves the database and worktree root from the environment, as the
    /// CLI does.
    ///
    /// # Errors
    /// Returns path resolution errors.
    pub fn from_environment() -> Result<Self, AppError> {
        Ok(Self::new(database_file()?, worktree_root()?))
    }

    /// Creates one mission over a Git checkout, recording where it began.
    ///
    /// # Errors
    /// Returns repository discovery, input validation, or persistence errors.
    pub fn create_mission(
        &self,
        title: impl Into<String>,
        goal: impl Into<String>,
        repository_path: impl AsRef<Path>,
    ) -> Result<MissionDetails, AppError> {
        self.create_mission_with_packages(title, goal, repository_path, Vec::new())
    }

    /// Creates a mission and its initial packages in one atomic store commit.
    ///
    /// # Errors
    /// Returns repository, contract, dependency, or persistence errors. Invalid
    /// packages leave no mission behind.
    pub fn create_mission_with_packages(
        &self,
        title: impl Into<String>,
        goal: impl Into<String>,
        repository_path: impl AsRef<Path>,
        packages: Vec<NewWorkPackage>,
    ) -> Result<MissionDetails, AppError> {
        let repository = GitRepository::discover(repository_path)?;
        let now = now();
        let id = MissionId::new();
        let mut mission = Mission::new(id, now);
        let mut events = mission.created_events();
        for package in packages {
            events.extend(
                mission
                    .add_package(package.id, package.contract, package.dependencies, now)?
                    .events,
            );
        }
        let input = MissionInput::new(
            id,
            title,
            goal,
            repository.source_path().to_string_lossy().into_owned(),
            repository.head_commit(),
            now,
        )?;
        let mut store = SqliteStore::open(&self.database)?;
        let revision = store.create_mission(&mission, &input, &events)?;
        mission_query::details(
            &mut store,
            &LoadedMission {
                mission,
                input,
                revision,
            },
        )
    }

    /// Every mission, newest first, with package counts. Each mission's
    /// runs are observed first, as on every other read, so the counts a
    /// row shows are current.
    ///
    /// # Errors
    /// Returns persistence errors.
    pub fn list_missions(&self) -> Result<Vec<MissionListItem>, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        mission_query::list(&mut store, |store, id| {
            observe_runs(store, &self.worktrees, id)
        })
    }

    /// One mission with its packages in dependency order.
    ///
    /// Before projecting, the committed status of every package's current
    /// run is observed, so a run that finished or stopped since the last
    /// look is reflected. That is the one write a read performs, and it
    /// derives only from run state already committed by whoever drives it.
    ///
    /// # Errors
    /// Returns not-found or persistence errors.
    pub fn inspect_mission(&self, mission_id: MissionId) -> Result<MissionDetails, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let loaded = observe_runs(&mut store, &self.worktrees, mission_id)?;
        mission_query::details(&mut store, &loaded)
    }

    /// One exchange with the mission's lead. The first message starts the
    /// lead session as a run over the mission's checkout; every later one
    /// appends a turn to it. Each turn's instruction is the brief rendered
    /// from the mission's current state plus the message, so the lead
    /// always answers over the truth and never over a chat log. `selection`
    /// and `effort` route the session when it starts; a later turn runs on
    /// the session's own sealed configuration and ignores them.
    ///
    /// # Errors
    /// Returns an error when the lead is still at work on an earlier turn,
    /// or when the run service, provider, or store refuse.
    pub fn ask_lead<F>(
        &self,
        runs: &RunService<F>,
        mission_id: MissionId,
        message: &str,
        selection: Option<ExecutionSelection>,
        effort: EffortRequest,
    ) -> Result<(LeadTurn, MissionDetails), AppError>
    where
        F: ProviderFactory,
    {
        let mut store = SqliteStore::open(&self.database)?;
        let loaded = observe_runs(&mut store, &self.worktrees, mission_id)?;
        let details = mission_query::details(&mut store, &loaded)?;
        let instruction = mission_lead::turn(&mission_lead::brief(&details), message);
        let repository = PathBuf::from(loaded.input.source_repo_path());
        let current = match store.mission_lead(mission_id)? {
            Some(binding) => {
                let status = store.load_run(binding.run_id)?.run.status();
                match status {
                    RunStatus::Completed => Some(binding.run_id),
                    // A lead that failed or was discarded is history; the
                    // next message opens a fresh session.
                    RunStatus::Failed | RunStatus::Discarded => None,
                    status => {
                        return Err(AppError::LeadBusy {
                            mission_id,
                            run_id: binding.run_id,
                            status,
                        });
                    }
                }
            }
            None => None,
        };
        drop(store);
        let report = if let Some(run_id) = current {
            runs.request_lead_turn(run_id, &instruction)?
        } else {
            let bound: RefCell<Option<Result<(), AppError>>> = RefCell::new(None);
            let report = runs.start_run_observed(
                WorkflowKind::Lead,
                instruction,
                &repository,
                selection,
                effort,
                &ImageGenerationPlan::disabled(),
                &|progress| {
                    if let StartProgress::PreparingWorkspace(run_id) = progress
                        && bound.borrow().is_none()
                    {
                        let outcome = SqliteStore::open(&self.database)
                            .and_then(|mut store| {
                                store.bind_mission_lead(mission_id, run_id, &now())
                            })
                            .map_err(AppError::from);
                        *bound.borrow_mut() = Some(outcome);
                    }
                },
            )?;
            match bound.into_inner() {
                Some(Ok(())) => {}
                Some(Err(error)) => {
                    return Err(AppError::LeadRunUnbound {
                        mission_id,
                        run_id: report.details.id,
                        reason: error.to_string(),
                    });
                }
                None => {
                    return Err(AppError::LeadRunUnbound {
                        mission_id,
                        run_id: report.details.id,
                        reason: "the run never reported being persisted".to_owned(),
                    });
                }
            }
            report
        };
        let mut store = SqliteStore::open(&self.database)?;
        let answer = mission_lead::latest_answer(&mut store, report.details.id)?;
        drop(store);
        let details = self.inspect_mission(mission_id)?;
        Ok((LeadTurn { report, answer }, details))
    }

    /// The lead's latest finished answer, with the proposals it carries.
    ///
    /// # Errors
    /// Returns persistence or artifact integrity errors.
    pub fn latest_lead_answer(
        &self,
        mission_id: MissionId,
    ) -> Result<Option<LeadAnswer>, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        match store.mission_lead(mission_id)? {
            Some(binding) => mission_lead::latest_answer(&mut store, binding.run_id),
            None => Ok(None),
        }
    }

    /// Applies the lead's latest proposals to the plan, all in one commit:
    /// either every change lands or none does.
    ///
    /// # Errors
    /// Returns an error when there is no answer, when the answer's section
    /// cannot be read, or when a change is refused by the plan.
    pub fn apply_lead_proposals(&self, mission_id: MissionId) -> Result<MissionDetails, AppError> {
        let answer = self
            .latest_lead_answer(mission_id)?
            .ok_or(AppError::NoLeadAnswer(mission_id))?;
        let changes = answer
            .proposals
            .map_err(|source| AppError::LeadProposalUnreadable {
                run_id: answer.run_id,
                stage_id: answer.stage_id.clone(),
                source,
            })?;
        self.apply_plan_changes(mission_id, &changes)
    }

    /// Applies a list of plan changes atomically, in order.
    ///
    /// # Errors
    /// Returns the first change the plan refuses; nothing is committed.
    pub fn apply_plan_changes(
        &self,
        mission_id: MissionId,
        changes: &[PlanChange],
    ) -> Result<MissionDetails, AppError> {
        self.mutate(mission_id, |mission, now| {
            let mut events = Vec::new();
            for change in changes {
                let change = match change.clone() {
                    PlanChange::AddPackage {
                        id,
                        contract,
                        dependencies,
                    } => mission.add_package(id, contract, dependencies, now)?,
                    PlanChange::RevisePackage {
                        id,
                        title,
                        goal,
                        rationale,
                        scope,
                        acceptance_criteria,
                        verification,
                        workflow,
                        dependencies,
                    } => {
                        let current = mission
                            .package(&id)
                            .ok_or_else(|| MissionError::PackageNotFound(mission.id(), id.clone()))?
                            .contract()
                            .clone();
                        let contract = WorkPackageContract {
                            title: title.unwrap_or_else(|| current.title.clone()),
                            goal: goal.unwrap_or_else(|| current.goal.clone()),
                            rationale: rationale.unwrap_or_else(|| current.rationale.clone()),
                            scope: scope.unwrap_or_else(|| current.scope.clone()),
                            acceptance_criteria: acceptance_criteria
                                .unwrap_or_else(|| current.acceptance_criteria.clone()),
                            verification: verification
                                .unwrap_or_else(|| current.verification.clone()),
                            workflow: workflow.unwrap_or(current.workflow),
                        };
                        let mut change = if contract == current {
                            MissionChange { events: Vec::new() }
                        } else {
                            mission.revise_contract(&id, contract, now)?
                        };
                        if let Some(dependencies) = dependencies {
                            change
                                .events
                                .extend(mission.set_dependencies(&id, dependencies, now)?.events);
                        }
                        change
                    }
                    PlanChange::CancelPackage { id, reason } => {
                        mission.cancel_package(&id, reason, now)?
                    }
                    PlanChange::RecordDecision { title, rationale } => mission.record_decision(
                        DecisionId::new(),
                        title,
                        rationale,
                        DecisionAuthor::Lead,
                        now,
                    )?,
                };
                events.extend(change.events);
            }
            Ok(MissionChange { events })
        })
    }

    /// Adds one package to the plan.
    ///
    /// # Errors
    /// Returns domain rule violations or persistence errors.
    pub fn add_package(
        &self,
        mission_id: MissionId,
        package: NewWorkPackage,
    ) -> Result<MissionDetails, AppError> {
        self.mutate(mission_id, |mission, now| {
            mission.add_package(package.id, package.contract, package.dependencies, now)
        })
    }

    /// Replaces the contract of a package nothing has run for.
    ///
    /// # Errors
    /// Returns domain rule violations or persistence errors.
    pub fn revise_package(
        &self,
        mission_id: MissionId,
        package_id: &WorkPackageId,
        contract: WorkPackageContract,
    ) -> Result<MissionDetails, AppError> {
        self.mutate(mission_id, |mission, now| {
            mission.revise_contract(package_id, contract, now)
        })
    }

    /// Replaces the dependencies of a package nothing has run for.
    ///
    /// # Errors
    /// Returns domain rule violations or persistence errors.
    pub fn set_dependencies(
        &self,
        mission_id: MissionId,
        package_id: &WorkPackageId,
        dependencies: Vec<WorkPackageId>,
    ) -> Result<MissionDetails, AppError> {
        self.mutate(mission_id, |mission, now| {
            mission.set_dependencies(package_id, dependencies, now)
        })
    }

    /// Starts a child run for one ready package and binds it.
    ///
    /// The run's immutable input is the package handoff rendered from the
    /// mission: goal, contract, integrated dependencies, and decisions.
    /// The run is bound to the package the moment it is persisted, before
    /// its workspace is prepared, so a drive that stops early still leaves
    /// the package pointing at its run. The run then proceeds exactly as
    /// `senate <workflow>` would, and the package is observed once the
    /// drive reaches quiescence.
    ///
    /// # Errors
    /// Returns domain rule violations, run start failures, or persistence
    /// errors. When the run started but could not be bound, the error names
    /// it so it can be attached by hand.
    pub fn start_package<F>(
        &self,
        runs: &RunService<F>,
        mission_id: MissionId,
        package_id: &WorkPackageId,
        selection: Option<ExecutionSelection>,
        effort: EffortRequest,
    ) -> Result<(ExecutionReport, MissionDetails), AppError>
    where
        F: ProviderFactory,
    {
        self.start_package_observed(runs, mission_id, package_id, selection, effort, &|_| {})
    }

    /// [`Self::start_package`], reporting each step of the start to
    /// `observe` as the run service does, so an interface can show the wait.
    ///
    /// # Errors
    /// As [`Self::start_package`].
    pub fn start_package_observed<F>(
        &self,
        runs: &RunService<F>,
        mission_id: MissionId,
        package_id: &WorkPackageId,
        selection: Option<ExecutionSelection>,
        effort: EffortRequest,
        observe: &dyn Fn(StartProgress),
    ) -> Result<(ExecutionReport, MissionDetails), AppError>
    where
        F: ProviderFactory,
    {
        self.start_package_with(
            runs, mission_id, package_id, selection, effort, false, observe,
        )
    }

    /// [`Self::start_package_observed`] with the run armed to approve
    /// grantable permission requests from the moment it exists, before its
    /// first stage can ask. The flag is written on the starting thread, in
    /// the step that binds the run, not by whoever observes the progress.
    ///
    /// # Errors
    /// As [`Self::start_package`].
    #[allow(
        clippy::too_many_arguments,
        reason = "one start, every choice it takes"
    )]
    pub fn start_package_with<F>(
        &self,
        runs: &RunService<F>,
        mission_id: MissionId,
        package_id: &WorkPackageId,
        selection: Option<ExecutionSelection>,
        effort: EffortRequest,
        auto_approve: bool,
        observe: &dyn Fn(StartProgress),
    ) -> Result<(ExecutionReport, MissionDetails), AppError>
    where
        F: ProviderFactory,
    {
        let mut store = SqliteStore::open(&self.database)?;
        let loaded = store.load_mission(mission_id)?;
        let package = loaded
            .mission
            .package(package_id)
            .ok_or_else(|| AppError::PackageNotFound(mission_id, package_id.clone()))?;
        if package.status() != WorkPackageStatus::Ready {
            return Err(crate::domain::MissionError::InvalidPackageTransition {
                package_id: package_id.clone(),
                status: package.status(),
                action: "starting a run",
                expected: "a ready package",
            }
            .into());
        }
        let task = handoff_task(&loaded, package);
        // The run stores its input trimmed; hash exactly what it will hold.
        let stored_task = task.trim();
        let handoff = MissionHandoffRecord {
            run_id: RunId::from_u128(0),
            mission_id,
            package_id: package_id.clone(),
            contract_sha256: contract_sha256(package.contract())?,
            task_sha256: sha256_hex(stored_task.as_bytes()),
            task_size: stored_task.len() as u64,
            dependencies: package.dependencies().to_vec(),
            decision_ids: loaded
                .mission
                .decisions()
                .iter()
                .map(|decision| decision.id)
                .collect(),
            created_at: now(),
        };
        let workflow = package.contract().workflow;
        let repository = PathBuf::from(loaded.input.source_repo_path());
        drop(store);

        let bound: RefCell<Option<Result<(), AppError>>> = RefCell::new(None);
        let report = runs
            .start_run_observed(
                workflow,
                task,
                &repository,
                selection,
                effort,
                &ImageGenerationPlan::disabled(),
                &|progress| {
                    observe(progress.clone());
                    if let StartProgress::PreparingWorkspace(run_id) = progress
                        && bound.borrow().is_none()
                    {
                        let handoff = MissionHandoffRecord {
                            run_id,
                            ..handoff.clone()
                        };
                        let outcome = self
                            .bind_run(mission_id, package_id, run_id, Some(&handoff))
                            .map(|_| ())
                            .and_then(|()| {
                                if auto_approve {
                                    runs.set_run_auto_approve(run_id, true)
                                } else {
                                    Ok(())
                                }
                            });
                        *bound.borrow_mut() = Some(outcome);
                    }
                },
            )
            .map_err(|error| match error {
                AppError::DirtySourceRepository => AppError::MissionIntegrationNeedsCommit {
                    mission_id,
                    package_id: package_id.clone(),
                },
                error => error,
            })?;
        match bound.into_inner() {
            Some(Ok(())) => {}
            Some(Err(error)) => {
                return Err(AppError::PackageRunUnbound {
                    mission_id,
                    package_id: package_id.clone(),
                    run_id: report.details.id,
                    reason: error.to_string(),
                });
            }
            None => {
                return Err(AppError::PackageRunUnbound {
                    mission_id,
                    package_id: package_id.clone(),
                    run_id: report.details.id,
                    reason: "the run never reported being persisted".to_owned(),
                });
            }
        }
        let details = self.inspect_mission(mission_id)?;
        Ok((report, details))
    }

    /// Binds an existing run to a ready package, for work started outside
    /// the mission. The run must belong to the mission's repository.
    ///
    /// # Errors
    /// Returns domain rule violations, repository mismatches, or persistence
    /// errors.
    pub fn attach_run(
        &self,
        mission_id: MissionId,
        package_id: &WorkPackageId,
        run_id: RunId,
    ) -> Result<MissionDetails, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let loaded = store.load_mission(mission_id)?;
        let run = query::inspect(&mut store, run_id)?;
        let expected = Path::new(loaded.input.source_repo_path());
        if run.repository.as_deref() != Some(expected) {
            return Err(AppError::MissionRepositoryMismatch {
                run_id,
                expected: expected.to_path_buf(),
                actual: run.repository,
            });
        }
        drop(store);
        self.bind_run(mission_id, package_id, run_id, None)?;
        self.inspect_mission(mission_id)
    }

    /// Sends a delivered package's run back to work: a fix cycle, or a
    /// continue cycle carrying an instruction. The run grows its cycle
    /// exactly as `senate fix` / the TUI's continue would; the package
    /// reads in progress again on the same run and delivers afresh.
    ///
    /// # Errors
    /// Returns run-side refusals (no decision stage, not completed) and
    /// persistence errors.
    pub fn rework_package<F>(
        &self,
        runs: &RunService<F>,
        mission_id: MissionId,
        package_id: &WorkPackageId,
        rework: Rework,
    ) -> Result<(ExecutionReport, MissionDetails), AppError>
    where
        F: ProviderFactory,
    {
        let mut store = SqliteStore::open(&self.database)?;
        let loaded = observe_runs(&mut store, &self.worktrees, mission_id)?;
        let package = loaded
            .mission
            .package(package_id)
            .ok_or_else(|| AppError::PackageNotFound(mission_id, package_id.clone()))?;
        if package.status() != WorkPackageStatus::Delivered {
            return Err(crate::domain::MissionError::InvalidPackageTransition {
                package_id: package_id.clone(),
                status: package.status(),
                action: "reworking",
                expected: "a delivered package",
            }
            .into());
        }
        let run_id = package
            .current_run()
            .ok_or_else(|| AppError::PackageNotFound(mission_id, package_id.clone()))?;
        drop(store);
        // The run grows its cycle first; the next observation sees either a
        // run at work (the package is reworked and in progress) or a run that
        // finished with more stages than the result covers (re-delivered from
        // a fresh capture). Either way the old result never stands for the
        // run's current state.
        let report = match rework {
            Rework::Fix => runs.request_fix(run_id)?,
            Rework::Continue(instruction) => runs.request_continue(run_id, instruction)?,
        };
        let details = self.inspect_mission(mission_id)?;
        Ok((report, details))
    }

    /// Resumes every package run that is prepared, suspended, or interrupted,
    /// then observes the mission: the fan-in for packages started with
    /// native providers, whose runs outlive the command that started them.
    ///
    /// # Errors
    /// Returns the first run-side failure, or persistence errors.
    pub fn resume_mission<F>(
        &self,
        runs: &RunService<F>,
        mission_id: MissionId,
    ) -> Result<(Vec<ExecutionReport>, MissionDetails), AppError>
    where
        F: ProviderFactory,
    {
        let mut store = SqliteStore::open(&self.database)?;
        let loaded = observe_runs(&mut store, &self.worktrees, mission_id)?;
        let active: Vec<RunId> = loaded
            .mission
            .packages()
            .iter()
            .filter(|package| package.status().has_active_run())
            .filter_map(WorkPackage::current_run)
            .collect();
        drop(store);
        let mut reports = Vec::new();
        for run_id in active {
            let status = SqliteStore::open(&self.database)?
                .load_run(run_id)?
                .run
                .status();
            if matches!(
                status,
                RunStatus::Ready | RunStatus::Running | RunStatus::Paused | RunStatus::Interrupted
            ) {
                reports.push(runs.resume_run(run_id)?);
            }
        }
        let details = self.inspect_mission(mission_id)?;
        Ok((reports, details))
    }

    /// Binds one run to one package in a single commit, with its handoff
    /// when the mission rendered the run's task.
    fn bind_run(
        &self,
        mission_id: MissionId,
        package_id: &WorkPackageId,
        run_id: RunId,
        handoff: Option<&MissionHandoffRecord>,
    ) -> Result<MissionRevision, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let mut loaded = store.load_mission(mission_id)?;
        let now = now().max(*loaded.mission.updated_at());
        let change = loaded.mission.start_package(package_id, run_id, now)?;
        Ok(store.commit_mission_update_with(
            &loaded.mission,
            loaded.revision,
            &change.events,
            handoff,
        )?)
    }

    /// Records that a delivered package's change reached the source
    /// checkout. The evidence is read from the run store: the run is
    /// `Applied`, or it completed with an empty delta against its base and
    /// there was nothing to transfer. A completed run whose delta is still
    /// in its worktree is not integrated, whatever anyone says about it.
    ///
    /// # Errors
    /// Returns domain rule violations, missing evidence, or persistence
    /// errors.
    pub fn integrate_package<F>(
        &self,
        runs: &RunService<F>,
        mission_id: MissionId,
        package_id: &WorkPackageId,
    ) -> Result<MissionDetails, AppError>
    where
        F: ProviderFactory,
    {
        let mut store = SqliteStore::open(&self.database)?;
        let loaded = observe_runs(&mut store, &self.worktrees, mission_id)?;
        let package = loaded
            .mission
            .package(package_id)
            .ok_or_else(|| AppError::PackageNotFound(mission_id, package_id.clone()))?;
        let run_id = package
            .current_run()
            .ok_or_else(|| AppError::PackageNotFound(mission_id, package_id.clone()))?;
        let status = store.load_run(run_id)?.run.status();
        let workspace = store
            .load_workspace(run_id)?
            .map(|workspace| workspace.status());
        let evidence = match status {
            RunStatus::Applied => IntegrationEvidence::Applied,
            // A completed run's worktree is released by exactly one path:
            // `apply` finding an empty delta. So a completed run without a
            // worktree already went through apply and had nothing to move;
            // one that still has its worktree is asked for its delta now.
            RunStatus::Completed if workspace == Some(WorkspaceStatus::Removed) => {
                IntegrationEvidence::NoChanges
            }
            // A completed run with its worktree still in place is applied
            // here: integrating is bringing the change in, and apply is the
            // one path that moves it, with its own verification gate.
            RunStatus::Completed if workspace == Some(WorkspaceStatus::Ready) => {
                drop(store);
                match runs.apply_run(run_id)?.0 {
                    ApplyOutcome::Applied => IntegrationEvidence::Applied,
                    ApplyOutcome::NoChanges => IntegrationEvidence::NoChanges,
                }
            }
            RunStatus::Completed => {
                return Err(AppError::PackageWorkspaceUnavailable { run_id, workspace });
            }
            other => {
                return Err(AppError::PackageNotIntegrated {
                    run_id,
                    status: other,
                });
            }
        };
        self.mutate(mission_id, |mission, now| {
            mission.integrate_package(package_id, evidence, now)
        })
    }

    /// Returns a failed package to the plan so a new run can serve it.
    ///
    /// # Errors
    /// Returns domain rule violations or persistence errors.
    pub fn retry_package(
        &self,
        mission_id: MissionId,
        package_id: &WorkPackageId,
    ) -> Result<MissionDetails, AppError> {
        self.mutate(mission_id, |mission, now| {
            mission.retry_package(package_id, now)
        })
    }

    /// Cancels one package nothing depends on and no run is serving.
    ///
    /// # Errors
    /// Returns domain rule violations or persistence errors.
    pub fn cancel_package(
        &self,
        mission_id: MissionId,
        package_id: &WorkPackageId,
        reason: impl Into<String>,
    ) -> Result<MissionDetails, AppError> {
        let reason = reason.into();
        self.mutate(mission_id, |mission, now| {
            mission.cancel_package(package_id, reason, now)
        })
    }

    /// Records one design decision.
    ///
    /// # Errors
    /// Returns domain rule violations or persistence errors.
    pub fn record_decision(
        &self,
        mission_id: MissionId,
        title: impl Into<String>,
        rationale: impl Into<String>,
        author: DecisionAuthor,
    ) -> Result<MissionDetails, AppError> {
        let title = title.into();
        let rationale = rationale.into();
        self.mutate(mission_id, |mission, now| {
            mission.record_decision(DecisionId::new(), title, rationale, author, now)
        })
    }

    /// Completes a mission whose every package is integrated or cancelled.
    ///
    /// # Errors
    /// Returns domain rule violations or persistence errors.
    pub fn complete_mission(&self, mission_id: MissionId) -> Result<MissionDetails, AppError> {
        self.mutate(mission_id, Mission::complete)
    }

    /// Cancels a mission and every open package; running packages must be
    /// stopped first.
    ///
    /// # Errors
    /// Returns domain rule violations or persistence errors.
    pub fn cancel_mission(
        &self,
        mission_id: MissionId,
        reason: impl Into<String>,
    ) -> Result<MissionDetails, AppError> {
        let reason = reason.into();
        self.mutate(mission_id, |mission, now| mission.cancel(reason, now))
    }

    /// Observes runs, applies one domain operation, commits, and projects.
    fn mutate(
        &self,
        mission_id: MissionId,
        operation: impl FnOnce(
            &mut Mission,
            DateTime<Utc>,
        ) -> Result<MissionChange, crate::domain::MissionError>,
    ) -> Result<MissionDetails, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let mut loaded = observe_runs(&mut store, &self.worktrees, mission_id)?;
        let now = now().max(*loaded.mission.updated_at());
        let change = operation(&mut loaded.mission, now)?;
        if change.changed() {
            store.commit_mission_update(&loaded.mission, loaded.revision, &change.events)?;
            // The operation may have bound a run that already has a
            // committed outcome (attach); observe once more so the
            // projection never shows a package behind its run.
            loaded = observe_runs(&mut store, &self.worktrees, mission_id)?;
        }
        mission_query::details(&mut store, &loaded)
    }
}

fn now() -> DateTime<Utc> {
    std::time::SystemTime::now().into()
}

/// Reports every package's current run status to the mission and commits
/// whatever moved. Reads run state only; never drives a run. Delivery
/// evidence is captured from the run store at the moment a completion is
/// first observed, and only then.
fn observe_runs(
    store: &mut SqliteStore,
    worktrees: &Path,
    mission_id: MissionId,
) -> Result<LoadedMission, AppError> {
    let mut loaded = store.load_mission(mission_id)?;
    if loaded.mission.status().is_closed() {
        return Ok(loaded);
    }
    let observed: Vec<(WorkPackageId, RunId, WorkPackageStatus)> = loaded
        .mission
        .packages()
        .iter()
        .filter(|package| {
            package.status().has_active_run() || package.status() == WorkPackageStatus::Delivered
        })
        .filter_map(|package| {
            Some((
                package.id().clone(),
                package.current_run()?,
                package.status(),
            ))
        })
        .collect();
    let mut events = Vec::new();
    for (package_id, run_id, package_status) in observed {
        let run = store.load_run(run_id)?.run;
        let status = run.status();
        let reason = match status {
            RunStatus::NeedsUser => run
                .attention_requests()
                .iter()
                .find(|request| request.status().is_pending())
                .map(|request| format!("{:?}: {}", request.kind(), request.summary())),
            RunStatus::Failed => query::inspect(store, run_id)?.failure_reason,
            _ => None,
        };
        let now = now().max(*loaded.mission.updated_at());
        let delivered_stages = loaded
            .mission
            .package(&package_id)
            .and_then(WorkPackage::result)
            .map(|result| result.stage_count);
        let result = match status {
            RunStatus::Completed | RunStatus::Applied
                if package_status != WorkPackageStatus::Delivered
                    || delivered_stages != Some(run.stages().len()) =>
            {
                Some(mission_result::capture(store, worktrees, run_id, now)?)
            }
            _ => None,
        };
        let change = loaded.mission.observe_run(
            &package_id,
            run_id,
            status,
            reason.as_deref(),
            result,
            now,
        )?;
        events.extend(change.events);
    }
    if !events.is_empty() {
        loaded.revision = store.commit_mission_update(&loaded.mission, loaded.revision, &events)?;
    }
    Ok(loaded)
}

/// The immutable task a child run receives: everything the package's
/// worker needs, rendered from canonical mission state rather than typed
/// by hand. Deterministic for a given mission and package.
pub(crate) fn handoff_task(loaded: &LoadedMission, package: &WorkPackage) -> String {
    let contract = package.contract();
    let mut text = String::new();
    let _ = write!(
        text,
        "# Work package: {} ({})\n\nPart of mission \"{}\": {}\n\n",
        contract.title,
        package.id(),
        loaded.input.title(),
        loaded.input.goal()
    );
    text.push_str("## Goal\n\n");
    text.push_str(&contract.goal);
    text.push_str("\n\n");
    if !contract.rationale.is_empty() {
        text.push_str("## Why this exists\n\n");
        text.push_str(&contract.rationale);
        text.push_str("\n\n");
    }
    if !contract.scope.is_empty() {
        text.push_str("## Scope\n\n");
        text.push_str(&contract.scope);
        text.push_str("\n\n");
    }
    if !contract.acceptance_criteria.is_empty() {
        text.push_str("## Acceptance criteria\n\n");
        for criterion in &contract.acceptance_criteria {
            let _ = writeln!(text, "- {criterion}");
        }
        text.push('\n');
    }
    if !contract.verification.is_empty() {
        text.push_str("## Verification expectations\n\n");
        text.push_str(&contract.verification);
        text.push_str("\n\n");
    }
    let dependencies: Vec<&WorkPackage> = package
        .dependencies()
        .iter()
        .filter_map(|id| loaded.mission.package(id))
        .collect();
    if !dependencies.is_empty() {
        text.push_str("## Already integrated before this package\n\n");
        for dependency in dependencies {
            let _ = writeln!(
                text,
                "- {} ({}): {}",
                dependency.contract().title,
                dependency.id(),
                dependency.contract().goal
            );
        }
        text.push('\n');
    }
    if !loaded.mission.decisions().is_empty() {
        text.push_str("## Decisions to respect\n\n");
        for decision in loaded.mission.decisions() {
            let _ = writeln!(text, "- {}: {}", decision.title, decision.rationale);
        }
        text.push('\n');
    }
    text.push_str(
        "Stay within this package. Work another package needs belongs to that package; \
         note it under `## Bottom line` instead of doing it here.\n",
    );
    text
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use chrono::TimeZone;
    use tempfile::TempDir;

    use super::*;
    use crate::app::{DevelopmentFakeProviderFactory, UniformProvider};
    use crate::domain::{MissionStatus, StageId, StageKind, WorkflowKind};

    struct Fixture {
        temp: TempDir,
        repo: PathBuf,
        database: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let repo = temp.path().join("repo");
            fs::create_dir(&repo).unwrap();
            git(&repo, &["init", "-q"]);
            git(&repo, &["config", "user.email", "test@example.com"]);
            git(&repo, &["config", "user.name", "Test"]);
            fs::write(repo.join("README.md"), "baseline\n").unwrap();
            git(&repo, &["add", "README.md"]);
            git(&repo, &["commit", "-qm", "initial"]);
            let database = temp.path().join("data/senate.db");
            Self {
                temp,
                repo,
                database,
            }
        }

        fn missions(&self) -> MissionService {
            MissionService::new(
                self.database.clone(),
                self.temp.path().join("data/worktrees"),
            )
        }

        fn runs(&self) -> RunService<DevelopmentFakeProviderFactory> {
            RunService::new(
                self.database.clone(),
                self.temp.path().join("data/worktrees"),
                DevelopmentFakeProviderFactory::new(self.temp.path().join("runs")),
            )
        }
    }

    fn git(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn contract(title: &str, workflow: WorkflowKind) -> WorkPackageContract {
        WorkPackageContract {
            title: title.to_owned(),
            goal: format!("deliver {title}"),
            rationale: String::new(),
            scope: String::new(),
            acceptance_criteria: vec![],
            verification: String::new(),
            workflow,
        }
    }

    fn package(id: &str, title: &str, dependencies: &[&str]) -> NewWorkPackage {
        NewWorkPackage {
            id: WorkPackageId::new(id).unwrap(),
            contract: contract(title, WorkflowKind::Fast),
            dependencies: dependencies
                .iter()
                .map(|dependency| WorkPackageId::new(*dependency).unwrap())
                .collect(),
        }
    }

    #[test]
    fn initial_packages_are_atomic_and_restart_safe() {
        let fixture = Fixture::new();
        let missions = fixture.missions();
        // A later invalid package must not persist the earlier valid one,
        // or even create the store.
        assert!(
            missions
                .create_mission_with_packages(
                    "Imported",
                    "goal",
                    &fixture.repo,
                    vec![
                        package("APP-1", "First", &[]),
                        package("APP-1", "Duplicate", &[])
                    ]
                )
                .is_err()
        );
        assert!(!fixture.database.exists());
        let imported = missions
            .create_mission_with_packages(
                "Imported",
                "goal",
                &fixture.repo,
                vec![
                    package("APP-1", "First", &[]),
                    package("APP-2", "Second", &["APP-1"]),
                ],
            )
            .unwrap();
        let reopened = fixture.missions().inspect_mission(imported.id).unwrap();
        assert_eq!(reopened.packages.len(), 2);
        assert_eq!(
            reopened
                .package(&WorkPackageId::new("APP-1").unwrap())
                .unwrap()
                .status,
            WorkPackageStatus::Ready
        );
        assert_eq!(
            reopened
                .package(&WorkPackageId::new("APP-2").unwrap())
                .unwrap()
                .status,
            WorkPackageStatus::Planned
        );
        assert_eq!(missions.list_missions().unwrap().len(), 1);
    }

    /// A start carrying the mission's auto-approve arms the run in the step
    /// that binds it, so the flag is on the run before its first stage.
    #[test]
    fn an_armed_start_leaves_the_run_auto_approving() {
        let fixture = Fixture::new();
        let missions = fixture.missions();
        let runs = fixture.runs();
        let mission = missions
            .create_mission("JEV", "goal", &fixture.repo)
            .unwrap();
        let core = WorkPackageId::new("core").unwrap();
        missions
            .add_package(mission.id, package("core", "Core", &[]))
            .unwrap();
        let (report, _) = missions
            .start_package_with(
                &runs,
                mission.id,
                &core,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortRequest::ProfileDefault,
                true,
                &|_| {},
            )
            .unwrap();
        assert!(runs.inspect_run(report.details.id).unwrap().auto_approve);
    }

    /// Writes one Markdown artifact for a lead turn straight into the run
    /// store, as the fake provider writes none: real content behind a real
    /// record, through the same integrity-checked read path.
    fn write_lead_artifact(fixture: &Fixture, run_id: RunId, stage_id: &StageId, text: &str) {
        let path = fixture.temp.path().join(format!("{run_id}-{stage_id}.md"));
        fs::write(&path, text).unwrap();
        let created_at = now();
        let metadata = crate::domain::ArtifactMetadata::new(
            crate::domain::ArtifactId::new(),
            run_id,
            stage_id.clone(),
            crate::domain::ArtifactKind::Lead,
            crate::domain::Role::EngineeringLead,
            crate::domain::ArtifactStatus::Complete,
            created_at,
        )
        .with_provider(crate::domain::ProviderId::new("fake").unwrap(), None);
        let bytes = text.as_bytes();
        let artifact = crate::providers::ArtifactRecord::new(
            metadata,
            1,
            path,
            sha256_hex(bytes),
            u64::try_from(bytes.len()).unwrap(),
            created_at,
        )
        .unwrap();
        SqliteStore::open(&fixture.database)
            .unwrap()
            .insert_artifact(&artifact)
            .unwrap();
    }

    /// The lead is one run per mission: the first question starts it, each
    /// later one appends a turn, and its answer's proposals land on the plan
    /// only when applied, all together or not at all.
    #[test]
    #[allow(clippy::too_many_lines, reason = "one conversation, start to finish")]
    fn the_lead_answers_over_one_run_and_its_proposals_apply_atomically() {
        let fixture = Fixture::new();
        let missions = fixture.missions();
        let runs = fixture.runs();
        let mission = missions
            .create_mission("JEV", "characters keep living", &fixture.repo)
            .unwrap();
        let fake = Some(ExecutionSelection::Uniform(UniformProvider::Fake));

        let (turn, details) = missions
            .ask_lead(
                &runs,
                mission.id,
                "How should we split this?",
                fake.clone(),
                EffortRequest::ProfileDefault,
            )
            .unwrap();
        let lead_run = turn.report.details.id;
        assert_eq!(turn.report.details.workflow, WorkflowKind::Lead);
        assert_eq!(turn.report.details.status, RunStatus::Completed);
        assert!(
            turn.answer.is_none(),
            "the fake provider writes no artifact"
        );
        assert!(
            turn.report
                .details
                .task
                .as_deref()
                .unwrap()
                .contains("# Mission brief: JEV")
        );
        let lead = details.lead.expect("the mission has a lead now");
        assert_eq!(lead.run_id, lead_run);
        assert_eq!(lead.turns, 1);
        assert!(missions.latest_lead_answer(mission.id).unwrap().is_none());
        assert!(matches!(
            missions.apply_lead_proposals(mission.id),
            Err(AppError::NoLeadAnswer(_))
        ));

        // The second question is a turn on the same run.
        let (turn, details) = missions
            .ask_lead(
                &runs,
                mission.id,
                "Go on.",
                fake.clone(),
                EffortRequest::ProfileDefault,
            )
            .unwrap();
        assert_eq!(turn.report.details.id, lead_run);
        assert_eq!(details.lead.unwrap().turns, 2);
        let lead_2 = StageId::new("lead_2").unwrap();
        assert!(
            turn.report
                .details
                .stages
                .iter()
                .any(|stage| stage.id == lead_2 && stage.kind == StageKind::Lead)
        );

        write_lead_artifact(
            &fixture,
            lead_run,
            &lead_2,
            "## Bottom line\n\nTwo packages, one decision.\n\n## Why\n\nSplit persistence from memory.\n\n## Plan changes\n\n- add `persistence`: Persistence\n  goal: persist every character between sessions\n  accept: a character survives a restart\n  workflow: standard\n- add `memory`: Memory\n  goal: characters remember what happened\n  depends on: persistence\n- decide: Persist before memory\n  why: memory needs a durable substrate\n",
        );
        let answer = missions.latest_lead_answer(mission.id).unwrap().unwrap();
        assert_eq!(answer.turn, 2);
        assert_eq!(
            answer.bottom_line.as_deref(),
            Some("Two packages, one decision.")
        );
        assert_eq!(answer.proposals.as_ref().unwrap().len(), 3);
        assert!(!answer.prose().contains("## Plan changes"));
        assert!(answer.prose().contains("Split persistence from memory."));

        let details = missions.apply_lead_proposals(mission.id).unwrap();
        let persistence = WorkPackageId::new("persistence").unwrap();
        let memory = WorkPackageId::new("memory").unwrap();
        assert_eq!(
            details.package(&persistence).unwrap().status,
            WorkPackageStatus::Ready
        );
        assert_eq!(
            details.package(&memory).unwrap().status,
            WorkPackageStatus::Planned
        );
        assert_eq!(details.decisions.len(), 1);
        assert_eq!(details.decisions[0].author, DecisionAuthor::Lead);

        // A third turn whose proposals include one the plan refuses lands
        // nothing at all.
        let (turn, _) = missions
            .ask_lead(
                &runs,
                mission.id,
                "And?",
                fake,
                EffortRequest::ProfileDefault,
            )
            .unwrap();
        let lead_3 = StageId::new("lead_3").unwrap();
        write_lead_artifact(
            &fixture,
            turn.report.details.id,
            &lead_3,
            "## Plan changes\n\n- add `journal`: Journal\n  goal: keep a journal\n- cancel `nope`: never existed\n",
        );
        assert!(missions.apply_lead_proposals(mission.id).is_err());
        let details = missions.inspect_mission(mission.id).unwrap();
        assert!(
            details
                .package(&WorkPackageId::new("journal").unwrap())
                .is_none(),
            "a refused batch applies none of its changes"
        );

        // The lead run belongs to the mission; it cannot be deleted.
        runs.set_run_archived(lead_run, true).unwrap();
        assert!(matches!(
            runs.purge_run(lead_run),
            Err(AppError::Store(
                crate::store::StoreError::RunBoundToMission { .. }
            ))
        ));
    }

    /// Integrating a package whose run still holds its change applies the
    /// run: the checkout gains the change, the run reads applied, and the
    /// package is in on that evidence.
    #[test]
    fn integrating_applies_a_run_whose_change_is_still_in_its_worktree() {
        let fixture = Fixture::new();
        let missions = fixture.missions();
        let runs = fixture.runs();
        let mission = missions
            .create_mission("JEV", "goal", &fixture.repo)
            .unwrap();
        let core = WorkPackageId::new("core").unwrap();
        missions
            .add_package(mission.id, package("core", "Core", &[]))
            .unwrap();
        let (report, _) = missions
            .start_package(
                &runs,
                mission.id,
                &core,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortRequest::ProfileDefault,
            )
            .unwrap();
        let run_id = report.details.id;
        let worktree = SqliteStore::open(&fixture.database)
            .unwrap()
            .load_workspace(run_id)
            .unwrap()
            .unwrap()
            .worktree_path()
            .to_path_buf();
        std::fs::write(worktree.join("README.md"), "changed by the package\n").unwrap();

        let details = missions
            .integrate_package(&runs, mission.id, &core)
            .unwrap();

        assert_eq!(
            details.package(&core).unwrap().status,
            WorkPackageStatus::Integrated
        );
        assert_eq!(
            details.package(&core).unwrap().run_status,
            Some(RunStatus::Applied)
        );
        assert_eq!(
            std::fs::read_to_string(fixture.repo.join("README.md")).unwrap(),
            "changed by the package\n"
        );
    }

    #[test]
    fn dependent_package_requires_committing_integrated_changes_first() {
        let fixture = Fixture::new();
        let missions = fixture.missions();
        let runs = fixture.runs();
        let mission = missions
            .create_mission("JEV", "goal", &fixture.repo)
            .unwrap();
        let persistence = WorkPackageId::new("persistence").unwrap();
        let memory = WorkPackageId::new("memory").unwrap();
        missions
            .add_package(mission.id, package("persistence", "Persistence", &[]))
            .unwrap();
        missions
            .add_package(mission.id, package("memory", "Memory", &["persistence"]))
            .unwrap();

        let (report, _) = missions
            .start_package(
                &runs,
                mission.id,
                &persistence,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortRequest::ProfileDefault,
            )
            .unwrap();
        let worktree = SqliteStore::open(&fixture.database)
            .unwrap()
            .load_workspace(report.details.id)
            .unwrap()
            .unwrap()
            .worktree_path()
            .to_path_buf();
        std::fs::write(worktree.join("README.md"), "persistence is integrated\n").unwrap();
        missions
            .integrate_package(&runs, mission.id, &persistence)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.repo.join("README.md")).unwrap(),
            "persistence is integrated\n"
        );
        // Applying leaves the source change unstaged and uncommitted.
        git(&fixture.repo, &["diff", "--cached", "--quiet"]);

        let store = SqliteStore::open(&fixture.database).unwrap();
        let run_count_before_refusal = store.list_runs().unwrap().len();
        drop(store);
        let error = missions
            .start_package(
                &runs,
                mission.id,
                &memory,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortRequest::ProfileDefault,
            )
            .unwrap_err();
        assert!(
            matches!(
                &error,
                AppError::MissionIntegrationNeedsCommit {
                    mission_id: error_mission,
                    package_id
                } if *error_mission == mission.id && package_id == &memory
            ),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("Commit changes from integrated packages")
        );

        let details = missions.inspect_mission(mission.id).unwrap();
        let package = details.package(&memory).unwrap();
        assert_eq!(package.status, WorkPackageStatus::Ready);
        assert_eq!(package.current_run, None);
        let store = SqliteStore::open(&fixture.database).unwrap();
        assert_eq!(store.list_runs().unwrap().len(), run_count_before_refusal);
        drop(store);

        git(&fixture.repo, &["add", "README.md"]);
        git(
            &fixture.repo,
            &["commit", "-qm", "integrate persistence package"],
        );
        let (dependent_report, _) = missions
            .start_package(
                &runs,
                mission.id,
                &memory,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortRequest::ProfileDefault,
            )
            .unwrap();
        let dependent_workspace = SqliteStore::open(&fixture.database)
            .unwrap()
            .load_workspace(dependent_report.details.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dependent_workspace.worktree_path().join("README.md")).unwrap(),
            "persistence is integrated\n"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one mission, start to finish")]
    fn a_package_runs_on_the_fake_provider_and_integration_readies_its_dependent() {
        let fixture = Fixture::new();
        let missions = fixture.missions();
        let runs = fixture.runs();
        let mission = missions
            .create_mission("JEV", "persistent autonomous lives", &fixture.repo)
            .unwrap();
        assert_eq!(mission.status, MissionStatus::Planning);
        assert_eq!(mission.repository, fixture.repo.canonicalize().unwrap());

        missions
            .add_package(mission.id, package("persistence", "Persistence", &[]))
            .unwrap();
        let details = missions
            .add_package(mission.id, package("memory", "Memory", &["persistence"]))
            .unwrap();
        let persistence = WorkPackageId::new("persistence").unwrap();
        let memory = WorkPackageId::new("memory").unwrap();
        assert_eq!(
            details.package(&persistence).unwrap().status,
            WorkPackageStatus::Ready
        );
        assert_eq!(
            details.package(&memory).unwrap().status,
            WorkPackageStatus::Planned
        );

        let error = missions
            .start_package(
                &runs,
                mission.id,
                &memory,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortRequest::ProfileDefault,
            )
            .unwrap_err();
        assert!(matches!(error, AppError::Mission(_)), "{error}");

        let (report, details) = missions
            .start_package(
                &runs,
                mission.id,
                &persistence,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortRequest::ProfileDefault,
            )
            .unwrap();
        assert_eq!(report.details.status, RunStatus::Completed);
        let task = report.details.task.unwrap();
        assert!(task.starts_with("# Work package: Persistence (persistence)"));
        assert!(task.contains("Part of mission \"JEV\": persistent autonomous lives"));
        let run_id = report.details.id;
        let summary = details.package(&persistence).unwrap();
        assert_eq!(summary.status, WorkPackageStatus::Delivered);
        assert_eq!(summary.current_run, Some(run_id));
        assert_eq!(summary.run_status, Some(RunStatus::Completed));
        // The handoff record hashes exactly what the run holds as its input.
        let handoff = summary
            .handoff
            .as_ref()
            .expect("a started package has a handoff");
        assert_eq!(handoff.task_sha256, sha256_hex(task.as_bytes()));
        assert_eq!(handoff.task_size, task.len() as u64);
        assert!(handoff.contract_current);
        // The result is committed run evidence: the fake changed nothing,
        // and Fast's verify stage really ran.
        let result = summary
            .result
            .as_ref()
            .expect("a delivered package has a result");
        assert_eq!(result.run_id, run_id);
        assert!(result.changed_files.is_empty());
        assert!(result.changes_complete);
        assert_eq!(
            result.verification.as_ref().map(|v| v.status),
            Some(crate::domain::StageStatus::Completed)
        );
        assert!(result.reviews.is_empty());
        assert_eq!(details.status, MissionStatus::Active);
        assert_eq!(
            details.attention.awaiting_integration,
            vec![persistence.clone()]
        );

        // The run knows its package, and cannot be deleted while it does.
        let binding = SqliteStore::open(&fixture.database)
            .unwrap()
            .mission_of_run(run_id)
            .unwrap()
            .unwrap();
        assert_eq!(binding.package_id, persistence);

        // The fake provider changes nothing, so the completed run's empty
        // delta is the integration evidence; apply reports the same.
        let details = missions
            .integrate_package(&runs, mission.id, &persistence)
            .unwrap();
        assert_eq!(
            details.package(&persistence).unwrap().status,
            WorkPackageStatus::Integrated
        );
        assert_eq!(
            details.package(&memory).unwrap().status,
            WorkPackageStatus::Ready
        );
        assert!(details.attention.is_empty());

        // The dependent's handoff names what came before it.
        let (report, _) = missions
            .start_package(
                &runs,
                mission.id,
                &memory,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortRequest::ProfileDefault,
            )
            .unwrap();
        assert!(report.details.task.unwrap().contains(
            "## Already integrated before this package\n\n- Persistence (persistence): deliver Persistence"
        ));
        // Integrating applies the run itself when it still has to.
        missions
            .integrate_package(&runs, mission.id, &memory)
            .unwrap();
        assert_eq!(
            runs.inspect_run(report.details.id).unwrap().status,
            RunStatus::Completed,
            "an empty delta leaves the run completed, as apply does"
        );
        let details = missions.complete_mission(mission.id).unwrap();
        assert_eq!(details.status, MissionStatus::Completed);
        assert_eq!(details.packages.len(), 2);

        let listed = missions.list_missions().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].integrated, 2);
        assert_eq!(listed[0].status, MissionStatus::Completed);

        // The mission's own history explains the state.
        let events = SqliteStore::open(&fixture.database)
            .unwrap()
            .load_mission_events(mission.id)
            .unwrap();
        assert!(matches!(
            events.last().unwrap().event.kind(),
            crate::domain::MissionEventKind::MissionCompleted
        ));
    }

    #[test]
    fn a_standard_package_is_reworked_through_a_fix_cycle_and_delivers_again() {
        let fixture = Fixture::new();
        let missions = fixture.missions();
        let runs = fixture.runs();
        let mission = missions
            .create_mission("JEV", "goal", &fixture.repo)
            .unwrap();
        let a = WorkPackageId::new("a").unwrap();
        missions
            .add_package(
                mission.id,
                NewWorkPackage {
                    id: a.clone(),
                    contract: contract("A", WorkflowKind::Standard),
                    dependencies: vec![],
                },
            )
            .unwrap();
        let (report, details) = missions
            .start_package(
                &runs,
                mission.id,
                &a,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortRequest::ProfileDefault,
            )
            .unwrap();
        assert_eq!(report.details.status, RunStatus::Completed);
        let result = details.package(&a).unwrap().result.clone().unwrap();
        assert_eq!(result.reviews.len(), 2, "standard runs two reviews");
        assert!(result.decision.is_some());
        let captured_at = result.captured_at;

        // A fix cycle reuses the run; the package delivers again with a
        // fresh result, and the mission's history says it was reworked.
        let (report, details) = missions
            .rework_package(&runs, mission.id, &a, Rework::Fix)
            .unwrap();
        assert_eq!(report.details.status, RunStatus::Completed);
        let reworked = details.package(&a).unwrap();
        assert_eq!(reworked.status, WorkPackageStatus::Delivered);
        assert_eq!(reworked.runs.len(), 1);
        assert!(reworked.result.as_ref().unwrap().captured_at >= captured_at);
        let events = SqliteStore::open(&fixture.database)
            .unwrap()
            .load_mission_events(mission.id)
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            event.event.kind(),
            crate::domain::MissionEventKind::PackageReworked { .. }
        )));
        let delivered = events
            .iter()
            .filter(|event| {
                matches!(
                    event.event.kind(),
                    crate::domain::MissionEventKind::PackageDelivered { .. }
                )
            })
            .count();
        assert_eq!(delivered, 2);

        // Nothing to resume on a quiescent mission; the call is a no-op.
        let (reports, _) = missions.resume_mission(&runs, mission.id).unwrap();
        assert!(reports.is_empty());
    }

    #[test]
    fn attaching_a_run_checks_its_repository_and_a_foreign_run_is_refused() {
        let fixture = Fixture::new();
        let missions = fixture.missions();
        let runs = fixture.runs();
        let mission = missions
            .create_mission("JEV", "goal", &fixture.repo)
            .unwrap();
        missions
            .add_package(mission.id, package("a", "A", &[]))
            .unwrap();
        let a = WorkPackageId::new("a").unwrap();
        let report = runs
            .start_run(
                WorkflowKind::Fast,
                "standalone",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortRequest::ProfileDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let details = missions
            .attach_run(mission.id, &a, report.details.id)
            .unwrap();
        // A completed run attached to a ready package is observed at once,
        // with its result but no handoff: nobody rendered its task.
        let attached = details.package(&a).unwrap();
        assert_eq!(attached.status, WorkPackageStatus::Delivered);
        assert!(attached.result.is_some());
        assert!(attached.handoff.is_none());

        let other = fixture.temp.path().join("other");
        fs::create_dir(&other).unwrap();
        git(&other, &["init", "-q"]);
        git(&other, &["config", "user.email", "test@example.com"]);
        git(&other, &["config", "user.name", "Test"]);
        fs::write(other.join("x"), "x\n").unwrap();
        git(&other, &["add", "x"]);
        git(&other, &["commit", "-qm", "initial"]);
        let foreign = runs
            .start_run(
                WorkflowKind::Fast,
                "elsewhere",
                &other,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortRequest::ProfileDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        missions
            .add_package(mission.id, package("b", "B", &[]))
            .unwrap();
        let error = missions
            .attach_run(
                mission.id,
                &WorkPackageId::new("b").unwrap(),
                foreign.details.id,
            )
            .unwrap_err();
        assert!(
            matches!(error, AppError::MissionRepositoryMismatch { .. }),
            "{error}"
        );
    }

    #[test]
    fn the_handoff_quotes_contract_dependencies_and_decisions() {
        let at = Utc.with_ymd_and_hms(2026, 9, 22, 9, 0, 0).unwrap();
        let id = MissionId::from_u128(1);
        let mut mission = Mission::new(id, at);
        let persistence = WorkPackageId::new("persistence").unwrap();
        let memory = WorkPackageId::new("memory").unwrap();
        mission
            .add_package(
                persistence.clone(),
                WorkPackageContract {
                    title: "Persistence".to_owned(),
                    goal: "persist character state".to_owned(),
                    rationale: String::new(),
                    scope: String::new(),
                    acceptance_criteria: vec![],
                    verification: String::new(),
                    workflow: WorkflowKind::Fast,
                },
                vec![],
                at,
            )
            .unwrap();
        mission
            .add_package(
                memory.clone(),
                WorkPackageContract {
                    title: "Memory".to_owned(),
                    goal: "give characters memory".to_owned(),
                    rationale: "memory needs persistence".to_owned(),
                    scope: "src/memory only".to_owned(),
                    acceptance_criteria: vec!["memories survive restart".to_owned()],
                    verification: "cargo test memory".to_owned(),
                    workflow: WorkflowKind::Standard,
                },
                vec![persistence],
                at,
            )
            .unwrap();
        mission
            .record_decision(
                DecisionId::from_u128(3),
                "Persist first",
                "memory needs a substrate",
                DecisionAuthor::Lead,
                at,
            )
            .unwrap();
        let loaded = LoadedMission {
            input: MissionInput::new(id, "JEV", "autonomous lives", "/repo", "abc", at).unwrap(),
            revision: MissionRevision::initial(),
            mission,
        };
        let text = handoff_task(&loaded, loaded.mission.package(&memory).unwrap());
        assert!(text.starts_with("# Work package: Memory (memory)\n\nPart of mission \"JEV\": autonomous lives\n\n## Goal\n\ngive characters memory\n"));
        assert!(text.contains("## Why this exists\n\nmemory needs persistence\n"));
        assert!(text.contains("## Scope\n\nsrc/memory only\n"));
        assert!(text.contains("## Acceptance criteria\n\n- memories survive restart\n"));
        assert!(text.contains("## Verification expectations\n\ncargo test memory\n"));
        assert!(text.contains("## Already integrated before this package\n\n- Persistence (persistence): persist character state\n"));
        assert!(
            text.contains("## Decisions to respect\n\n- Persist first: memory needs a substrate\n")
        );
        assert!(text.ends_with("instead of doing it here.\n"));
    }
}
