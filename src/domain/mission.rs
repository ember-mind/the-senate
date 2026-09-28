//! Mission: one project goal above the run, made of work packages.
//!
//! A run is a bounded engineering operation. A mission is the durable
//! plan that several runs serve: its work packages are engineering
//! contracts with dependencies between them, each delivered by a child
//! run, and its decisions are the design choices the plan rests on. The
//! aggregate owns every package so the invariants between them (unique
//! identity, acyclic dependencies, readiness only after integration) are
//! checked in one place, exactly as `Run` owns its stages.
//!
//! Nothing here reads a run: the application layer observes committed run
//! state and reports it through [`Mission::observe_run`], so package state
//! follows deterministic run evidence and never an agent's claim.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    DecisionId, EventId, EventMetadata, MissionId, Role, RunId, RunStatus, StageId, StageStatus,
    WorkPackageId, WorkflowKind,
};

/// Lifecycle of one mission.
///
/// `Planning` lasts until the first package starts. `Completed` and
/// `Cancelled` close the lifecycle: no package moves afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionStatus {
    Planning,
    Active,
    Completed,
    Cancelled,
}

impl MissionStatus {
    #[must_use]
    pub const fn is_closed(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

/// Lifecycle of one work package.
///
/// ```text
/// Planned -> Ready -> Running -> Delivered -> Integrated
///    ^                 |  ^
///    |                 |  `-- Blocked (the run waits for the user, or stopped)
///    |                 `----- Failed
///    `-- retry ---------------'
/// Cancelled from Planned, Ready, or Failed; a package with a live run is
/// cancelled by discarding the run first
/// ```
///
/// `Ready` means every dependency is `Integrated`, so a run started for the
/// package sees their changes in its base. `Delivered` means the child run
/// completed; `Integrated` means its change reached the source checkout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPackageStatus {
    Planned,
    Ready,
    Running,
    Blocked,
    Delivered,
    Integrated,
    Failed,
    Cancelled,
}

impl WorkPackageStatus {
    /// Closed packages never move again.
    #[must_use]
    pub const fn is_closed(self) -> bool {
        matches!(self, Self::Integrated | Self::Cancelled)
    }

    /// A package a child run is currently serving.
    #[must_use]
    pub const fn has_active_run(self) -> bool {
        matches!(self, Self::Running | Self::Blocked)
    }

    /// Packages whose contract may still change: nothing has run for them
    /// since the contract was written.
    #[must_use]
    pub const fn accepts_revision(self) -> bool {
        matches!(self, Self::Planned | Self::Ready | Self::Failed)
    }
}

/// The engineering contract one package delegates.
///
/// Free text is deliberate: the contract is what a lead would write for a
/// colleague, and every field is quoted into the child run's immutable
/// input, so it is durable structured state rather than a prompt copied
/// between agents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkPackageContract {
    pub title: String,
    pub goal: String,
    /// Why the package exists in the plan.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rationale: String,
    /// Expected or permitted scope; empty means the goal bounds it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_criteria: Vec<String>,
    /// What verification the lead expects beyond the repository's own checks.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verification: String,
    /// Which built-in workflow delivers the package.
    pub workflow: WorkflowKind,
}

impl WorkPackageContract {
    /// Validates the contract's required fields, trimming outer whitespace.
    ///
    /// # Errors
    /// Returns [`MissionError::EmptyContractField`] for a blank title or goal.
    pub fn normalized(mut self) -> Result<Self, MissionError> {
        self.title = self.title.trim().to_owned();
        self.goal = self.goal.trim().to_owned();
        self.rationale = self.rationale.trim().to_owned();
        self.scope = self.scope.trim().to_owned();
        self.verification = self.verification.trim().to_owned();
        self.acceptance_criteria = self
            .acceptance_criteria
            .into_iter()
            .map(|criterion| criterion.trim().to_owned())
            .filter(|criterion| !criterion.is_empty())
            .collect();
        if self.title.is_empty() {
            return Err(MissionError::EmptyContractField("title"));
        }
        if self.goal.is_empty() {
            return Err(MissionError::EmptyContractField("goal"));
        }
        Ok(self)
    }
}

/// Deterministic proof that a delivered package's change reached the source
/// checkout, read from the run store and never from an agent's report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationEvidence {
    /// The run is `Applied`: its patch was transferred to the checkout.
    Applied,
    /// The run completed with an empty delta against its base, so there was
    /// nothing to transfer; a read-only package integrates this way.
    NoChanges,
}

/// One file the delivering run changed against its base commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub binary: bool,
}

/// One stage's outcome as evidence: its committed status and, when the
/// stage wrote an artifact with the contracted section, that section
/// quoted verbatim. Never a summary anyone composed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageOutcome {
    pub stage_id: StageId,
    pub role: Role,
    pub status: StageStatus,
    /// The artifact's own `## Bottom line`, verbatim, when it wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom_line: Option<String>,
}

/// What a delivered package's run actually produced, captured from the run
/// store the moment the package is delivered and kept with the package, so
/// it survives the run's worktree being released.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkPackageResult {
    pub run_id: RunId,
    pub captured_at: DateTime<Utc>,
    /// How many stages the run had when this was captured. Fix and continue
    /// cycles append stages, so a run with more stages than this has been
    /// reworked since, and the result no longer describes it.
    pub stage_count: usize,
    /// Files changed against the run's base commit, bounded; `changes_complete`
    /// is false when the bound cut the list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<ChangedFile>,
    pub changes_complete: bool,
    /// The newest editing stage's own bottom line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom_line: Option<String>,
    /// The run's latest verification stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<StageOutcome>,
    /// Every review stage, in workflow order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviews: Vec<StageOutcome>,
    /// The run's latest decision stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<StageOutcome>,
    /// The decision's `## Follow-ups` section, verbatim: what the lead left
    /// open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_questions: Option<String>,
}

/// Who made a recorded decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionAuthor {
    User,
    Lead,
}

/// One design decision the plan rests on. Insert-only: a reversed decision
/// is a new decision that says so.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionDecision {
    pub id: DecisionId,
    pub title: String,
    pub rationale: String,
    pub author: DecisionAuthor,
    pub recorded_at: DateTime<Utc>,
}

/// One work package inside a mission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPackage {
    id: WorkPackageId,
    contract: WorkPackageContract,
    dependencies: Vec<WorkPackageId>,
    status: WorkPackageStatus,
    /// Every child run in start order; the last one is current.
    runs: Vec<RunId>,
    /// Why the package is `Blocked` or `Failed`; cleared when it moves on.
    reason: Option<String>,
    /// Evidence captured when the current run last delivered.
    result: Option<WorkPackageResult>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl WorkPackage {
    #[must_use]
    pub const fn result(&self) -> Option<&WorkPackageResult> {
        self.result.as_ref()
    }

    #[must_use]
    pub const fn id(&self) -> &WorkPackageId {
        &self.id
    }

    #[must_use]
    pub const fn contract(&self) -> &WorkPackageContract {
        &self.contract
    }

    #[must_use]
    pub fn dependencies(&self) -> &[WorkPackageId] {
        &self.dependencies
    }

    #[must_use]
    pub const fn status(&self) -> WorkPackageStatus {
        self.status
    }

    #[must_use]
    pub fn runs(&self) -> &[RunId] {
        &self.runs
    }

    #[must_use]
    pub fn current_run(&self) -> Option<RunId> {
        self.runs.last().copied()
    }

    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    #[must_use]
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    #[must_use]
    pub const fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }
}

/// Persistence-neutral reconstruction state for one package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPackageRehydrationData {
    pub id: WorkPackageId,
    pub contract: WorkPackageContract,
    pub dependencies: Vec<WorkPackageId>,
    pub status: WorkPackageStatus,
    pub runs: Vec<RunId>,
    pub reason: Option<String>,
    pub result: Option<WorkPackageResult>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Persistence-neutral reconstruction state for one mission.
///
/// Values remain untrusted until passed through [`Mission::rehydrate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissionRehydrationData {
    pub id: MissionId,
    pub status: MissionStatus,
    pub packages: Vec<WorkPackageRehydrationData>,
    pub decisions: Vec<MissionDecision>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One mission: the durable plan several runs serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mission {
    id: MissionId,
    status: MissionStatus,
    packages: Vec<WorkPackage>,
    decisions: Vec<MissionDecision>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// Semantic mission history, persisted beside mission state exactly as
/// [`super::DomainEvent`] is beside run state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionEvent {
    id: EventId,
    occurred_at: DateTime<Utc>,
    mission_id: MissionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    package_id: Option<WorkPackageId>,
    #[serde(flatten)]
    kind: MissionEventKind,
}

impl MissionEvent {
    #[must_use]
    pub const fn new(
        metadata: EventMetadata,
        mission_id: MissionId,
        package_id: Option<WorkPackageId>,
        kind: MissionEventKind,
    ) -> Self {
        Self {
            id: metadata.id(),
            occurred_at: metadata.occurred_at(),
            mission_id,
            package_id,
            kind,
        }
    }

    #[must_use]
    pub const fn id(&self) -> EventId {
        self.id
    }

    #[must_use]
    pub const fn occurred_at(&self) -> &DateTime<Utc> {
        &self.occurred_at
    }

    #[must_use]
    pub const fn mission_id(&self) -> MissionId {
        self.mission_id
    }

    #[must_use]
    pub const fn package_id(&self) -> Option<&WorkPackageId> {
        self.package_id.as_ref()
    }

    #[must_use]
    pub const fn kind(&self) -> &MissionEventKind {
        &self.kind
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MissionEventKind {
    MissionCreated,
    MissionActivated,
    MissionCompleted,
    MissionCancelled {
        reason: String,
    },
    PackageAdded,
    PackageContractRevised,
    PackageDependenciesChanged {
        dependencies: Vec<WorkPackageId>,
    },
    /// Every dependency is integrated; a run may start.
    PackageReady,
    /// A dependency change took readiness away before anything ran.
    PackageWaiting,
    PackageStarted {
        run_id: RunId,
    },
    PackageBlocked {
        run_id: RunId,
        reason: String,
    },
    PackageUnblocked {
        run_id: RunId,
    },
    PackageDelivered {
        run_id: RunId,
    },
    /// A delivered package's run went back to work (a fix or continue
    /// cycle); the package is in progress again on the same run.
    PackageReworked {
        run_id: RunId,
    },
    PackageIntegrated {
        run_id: RunId,
        evidence: IntegrationEvidence,
    },
    PackageFailed {
        run_id: RunId,
        reason: String,
    },
    PackageRetryScheduled,
    PackageCancelled {
        reason: String,
    },
    DecisionRecorded {
        decision_id: DecisionId,
    },
}

/// What the user must look at, derived from package state on every read;
/// nothing stores it, so it can never disagree with the packages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MissionAttention {
    /// Packages whose run is waiting for the user.
    pub blocked: Vec<(WorkPackageId, String)>,
    /// Packages whose run failed and await retry, revision, or cancellation.
    pub failed: Vec<(WorkPackageId, String)>,
    /// Packages delivered and waiting for an explicit integration.
    pub awaiting_integration: Vec<WorkPackageId>,
}

impl MissionAttention {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocked.is_empty() && self.failed.is_empty() && self.awaiting_integration.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.blocked.len() + self.failed.len() + self.awaiting_integration.len()
    }
}

/// One mission mutation: the aggregate after the change plus the events
/// that explain it. Callers persist both atomically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissionChange {
    pub events: Vec<MissionEvent>,
}

impl MissionChange {
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.events.is_empty()
    }
}

fn join_ids(ids: &[WorkPackageId]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MissionError {
    #[error("mission {mission_id} is {status:?} and accepts no further changes")]
    MissionClosed {
        mission_id: MissionId,
        status: MissionStatus,
    },
    #[error("mission {0} has no package {1}")]
    PackageNotFound(MissionId, WorkPackageId),
    #[error("mission {0} already has a package {1}")]
    DuplicatePackage(MissionId, WorkPackageId),
    #[error("package {package_id} cannot depend on itself")]
    SelfDependency { package_id: WorkPackageId },
    #[error("package {package_id} depends on unknown package {dependency}")]
    UnknownDependency {
        package_id: WorkPackageId,
        dependency: WorkPackageId,
    },
    #[error("package {package_id} lists dependency {dependency} twice")]
    DuplicateDependency {
        package_id: WorkPackageId,
        dependency: WorkPackageId,
    },
    #[error("dependencies of package {package_id} would form a cycle")]
    DependencyCycle { package_id: WorkPackageId },
    #[error("package {package_id} is {status:?}; {action} needs {expected}")]
    InvalidPackageTransition {
        package_id: WorkPackageId,
        status: WorkPackageStatus,
        action: &'static str,
        expected: &'static str,
    },
    #[error(
        "package {package_id} is still depended on by {}",
        join_ids(dependents)
    )]
    PackageHasDependents {
        package_id: WorkPackageId,
        dependents: Vec<WorkPackageId>,
    },
    #[error("run {run_id} is not the current run of package {package_id}")]
    StaleRun {
        package_id: WorkPackageId,
        run_id: RunId,
    },
    #[error("run {run_id} completed but package {package_id} was given no result to deliver")]
    MissingResult {
        package_id: WorkPackageId,
        run_id: RunId,
    },
    #[error("run {run_id} is already bound to package {package_id}")]
    RunAlreadyBound {
        package_id: WorkPackageId,
        run_id: RunId,
    },
    #[error("mission cannot complete: {0}")]
    CannotComplete(String),
    #[error("mission cannot be cancelled while package {0} has a live run; discard that run first")]
    PackageStillRunning(WorkPackageId),
    #[error("contract {0} must not be empty")]
    EmptyContractField(&'static str),
    #[error("decision {0} must not be empty")]
    EmptyDecisionField(&'static str),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MissionInvariantError {
    #[error("package {0} appears more than once")]
    DuplicatePackage(WorkPackageId),
    #[error("package {package_id} depends on unknown package {dependency}")]
    UnknownDependency {
        package_id: WorkPackageId,
        dependency: WorkPackageId,
    },
    #[error("package {0} depends on itself")]
    SelfDependency(WorkPackageId),
    #[error("package {package_id} lists dependency {dependency} twice")]
    DuplicateDependency {
        package_id: WorkPackageId,
        dependency: WorkPackageId,
    },
    #[error("package dependencies form a cycle through {0}")]
    DependencyCycle(WorkPackageId),
    #[error("package {0} is {1:?} but has no run")]
    MissingRun(WorkPackageId, WorkPackageStatus),
    #[error("run {0} is bound to more than one package")]
    RunBoundTwice(RunId),
    #[error("package {0} is ready but dependency {1} is not integrated")]
    ReadyWithUnintegratedDependency(WorkPackageId, WorkPackageId),
    #[error("package {0} is {1:?} without a reason")]
    MissingReason(WorkPackageId, WorkPackageStatus),
    #[error("package {0} carries a result from a run other than its current one")]
    ResultMismatch(WorkPackageId),
    #[error("mission is {0:?} but package {1} is {2:?}")]
    PackageStatusConflictsWithMission(MissionStatus, WorkPackageId, WorkPackageStatus),
    #[error("completed mission integrated nothing")]
    CompletedWithoutIntegration,
    #[error("decision {0} appears more than once")]
    DuplicateDecision(DecisionId),
    #[error("timestamps regress: {0}")]
    TimestampRegression(&'static str),
    #[error("contract of package {0} is invalid: {1}")]
    InvalidContract(WorkPackageId, MissionError),
}

impl Mission {
    /// Creates one empty mission in `Planning`.
    #[must_use]
    pub fn new(id: MissionId, created_at: DateTime<Utc>) -> Self {
        Self {
            id,
            status: MissionStatus::Planning,
            packages: Vec::new(),
            decisions: Vec::new(),
            created_at,
            updated_at: created_at,
        }
    }

    /// Reconstructs a mission from persisted data, enforcing every
    /// current-state invariant before an aggregate exists.
    ///
    /// # Errors
    /// Returns the first violated invariant.
    pub fn rehydrate(data: MissionRehydrationData) -> Result<Self, MissionInvariantError> {
        let mission = Self {
            id: data.id,
            status: data.status,
            packages: data
                .packages
                .into_iter()
                .map(|package| WorkPackage {
                    id: package.id,
                    contract: package.contract,
                    dependencies: package.dependencies,
                    status: package.status,
                    runs: package.runs,
                    reason: package.reason,
                    result: package.result,
                    created_at: package.created_at,
                    updated_at: package.updated_at,
                })
                .collect(),
            decisions: data.decisions,
            created_at: data.created_at,
            updated_at: data.updated_at,
        };
        mission.validate_invariants()?;
        Ok(mission)
    }

    #[must_use]
    pub const fn id(&self) -> MissionId {
        self.id
    }

    #[must_use]
    pub const fn status(&self) -> MissionStatus {
        self.status
    }

    #[must_use]
    pub fn packages(&self) -> &[WorkPackage] {
        &self.packages
    }

    #[must_use]
    pub fn package(&self, id: &WorkPackageId) -> Option<&WorkPackage> {
        self.packages.iter().find(|package| &package.id == id)
    }

    #[must_use]
    pub fn decisions(&self) -> &[MissionDecision] {
        &self.decisions
    }

    #[must_use]
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    #[must_use]
    pub const fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }

    /// The package one run serves, if any.
    #[must_use]
    pub fn package_of_run(&self, run_id: RunId) -> Option<&WorkPackage> {
        self.packages
            .iter()
            .find(|package| package.runs.contains(&run_id))
    }

    /// Derived attention: what needs the user right now.
    #[must_use]
    pub fn attention(&self) -> MissionAttention {
        let mut attention = MissionAttention::default();
        for package in &self.packages {
            match package.status {
                WorkPackageStatus::Blocked => attention.blocked.push((
                    package.id.clone(),
                    package.reason.clone().unwrap_or_default(),
                )),
                WorkPackageStatus::Failed => attention.failed.push((
                    package.id.clone(),
                    package.reason.clone().unwrap_or_default(),
                )),
                WorkPackageStatus::Delivered => {
                    attention.awaiting_integration.push(package.id.clone());
                }
                _ => {}
            }
        }
        attention
    }

    /// Full current-state invariant validation.
    ///
    /// # Errors
    /// Returns the first violated invariant.
    pub fn validate_invariants(&self) -> Result<(), MissionInvariantError> {
        if self.updated_at < self.created_at {
            return Err(MissionInvariantError::TimestampRegression("mission"));
        }
        let ids = self.validate_package_identities()?;
        self.validate_package_states(&ids)?;
        if let Some(package_id) = self.find_cycle() {
            return Err(MissionInvariantError::DependencyCycle(package_id));
        }
        if self.status == MissionStatus::Completed
            && !self
                .packages
                .iter()
                .any(|package| package.status == WorkPackageStatus::Integrated)
        {
            return Err(MissionInvariantError::CompletedWithoutIntegration);
        }
        let mut decision_ids = HashSet::new();
        for decision in &self.decisions {
            if !decision_ids.insert(decision.id) {
                return Err(MissionInvariantError::DuplicateDecision(decision.id));
            }
        }
        Ok(())
    }

    fn validate_package_identities(&self) -> Result<HashSet<WorkPackageId>, MissionInvariantError> {
        let mut ids = HashSet::new();
        for package in &self.packages {
            if !ids.insert(package.id.clone()) {
                return Err(MissionInvariantError::DuplicatePackage(package.id.clone()));
            }
            package.contract.clone().normalized().map_err(|error| {
                MissionInvariantError::InvalidContract(package.id.clone(), error)
            })?;
            if package.updated_at < package.created_at || package.created_at < self.created_at {
                return Err(MissionInvariantError::TimestampRegression("package"));
            }
        }
        Ok(ids)
    }

    fn validate_package_states(
        &self,
        ids: &HashSet<WorkPackageId>,
    ) -> Result<(), MissionInvariantError> {
        let mut seen_runs = HashSet::new();
        for package in &self.packages {
            let mut deps = HashSet::new();
            for dependency in &package.dependencies {
                if dependency == &package.id {
                    return Err(MissionInvariantError::SelfDependency(package.id.clone()));
                }
                if !ids.contains(dependency) {
                    return Err(MissionInvariantError::UnknownDependency {
                        package_id: package.id.clone(),
                        dependency: dependency.clone(),
                    });
                }
                if !deps.insert(dependency.clone()) {
                    return Err(MissionInvariantError::DuplicateDependency {
                        package_id: package.id.clone(),
                        dependency: dependency.clone(),
                    });
                }
            }
            for run in &package.runs {
                if !seen_runs.insert(*run) {
                    return Err(MissionInvariantError::RunBoundTwice(*run));
                }
            }
            match package.status {
                WorkPackageStatus::Running
                | WorkPackageStatus::Blocked
                | WorkPackageStatus::Delivered
                | WorkPackageStatus::Integrated
                | WorkPackageStatus::Failed
                    if package.runs.is_empty() =>
                {
                    return Err(MissionInvariantError::MissingRun(
                        package.id.clone(),
                        package.status,
                    ));
                }
                // A result, when there is one, is the current run's. There
                // may be none: packages delivered before results were
                // captured (snapshot v1) carry nothing, and the observe pass
                // fills a delivered one in on its next look.
                WorkPackageStatus::Delivered | WorkPackageStatus::Integrated
                    if package
                        .result
                        .as_ref()
                        .is_some_and(|result| Some(result.run_id) != package.current_run()) =>
                {
                    return Err(MissionInvariantError::ResultMismatch(package.id.clone()));
                }
                WorkPackageStatus::Blocked | WorkPackageStatus::Failed
                    if package.reason.as_deref().is_none_or(str::is_empty) =>
                {
                    return Err(MissionInvariantError::MissingReason(
                        package.id.clone(),
                        package.status,
                    ));
                }
                WorkPackageStatus::Ready => {
                    for dependency in &package.dependencies {
                        if self.package(dependency).map(WorkPackage::status)
                            != Some(WorkPackageStatus::Integrated)
                        {
                            return Err(MissionInvariantError::ReadyWithUnintegratedDependency(
                                package.id.clone(),
                                dependency.clone(),
                            ));
                        }
                    }
                }
                _ => {}
            }
            let conflicts = match self.status {
                MissionStatus::Planning => !matches!(
                    package.status,
                    WorkPackageStatus::Planned
                        | WorkPackageStatus::Ready
                        | WorkPackageStatus::Cancelled
                ),
                MissionStatus::Active => false,
                MissionStatus::Completed | MissionStatus::Cancelled => !package.status.is_closed(),
            };
            if conflicts {
                return Err(MissionInvariantError::PackageStatusConflictsWithMission(
                    self.status,
                    package.id.clone(),
                    package.status,
                ));
            }
        }
        Ok(())
    }

    fn find_cycle(&self) -> Option<WorkPackageId> {
        let graph: HashMap<&WorkPackageId, &[WorkPackageId]> = self
            .packages
            .iter()
            .map(|package| (&package.id, package.dependencies.as_slice()))
            .collect();
        let mut done = HashSet::new();
        let mut visiting = HashSet::new();
        for package in &self.packages {
            if let Some(cycle) = visit(&graph, &package.id, &mut visiting, &mut done) {
                return Some(cycle);
            }
        }
        None
    }

    fn open(&self) -> Result<(), MissionError> {
        if self.status.is_closed() {
            return Err(MissionError::MissionClosed {
                mission_id: self.id,
                status: self.status,
            });
        }
        Ok(())
    }

    fn package_mut(&mut self, id: &WorkPackageId) -> Result<&mut WorkPackage, MissionError> {
        let mission_id = self.id;
        self.packages
            .iter_mut()
            .find(|package| &package.id == id)
            .ok_or_else(|| MissionError::PackageNotFound(mission_id, id.clone()))
    }

    fn event(
        &self,
        package_id: Option<&WorkPackageId>,
        kind: MissionEventKind,
        now: DateTime<Utc>,
    ) -> MissionEvent {
        MissionEvent::new(
            EventMetadata::new(EventId::new(), now),
            self.id,
            package_id.cloned(),
            kind,
        )
    }

    fn touch(&mut self, now: DateTime<Utc>) {
        self.updated_at = now;
    }

    fn validate_dependencies(
        &self,
        package_id: &WorkPackageId,
        dependencies: &[WorkPackageId],
    ) -> Result<(), MissionError> {
        let mut seen = HashSet::new();
        for dependency in dependencies {
            if dependency == package_id {
                return Err(MissionError::SelfDependency {
                    package_id: package_id.clone(),
                });
            }
            if self.package(dependency).is_none() {
                return Err(MissionError::UnknownDependency {
                    package_id: package_id.clone(),
                    dependency: dependency.clone(),
                });
            }
            if !seen.insert(dependency) {
                return Err(MissionError::DuplicateDependency {
                    package_id: package_id.clone(),
                    dependency: dependency.clone(),
                });
            }
        }
        Ok(())
    }

    fn dependencies_integrated(&self, package: &WorkPackage) -> bool {
        package.dependencies.iter().all(|dependency| {
            self.package(dependency).map(WorkPackage::status) == Some(WorkPackageStatus::Integrated)
        })
    }

    /// Moves every `Planned` package whose dependencies are all integrated
    /// to `Ready`, and every `Ready` package that lost that to `Planned`.
    fn settle_readiness(&mut self, now: DateTime<Utc>, events: &mut Vec<MissionEvent>) {
        let ids: Vec<WorkPackageId> = self.packages.iter().map(|p| p.id.clone()).collect();
        for id in ids {
            let Some(package) = self.package(&id) else {
                continue;
            };
            let integrated = self.dependencies_integrated(package);
            let next = match (package.status, integrated) {
                (WorkPackageStatus::Planned, true) => {
                    Some((WorkPackageStatus::Ready, MissionEventKind::PackageReady))
                }
                (WorkPackageStatus::Ready, false) => {
                    Some((WorkPackageStatus::Planned, MissionEventKind::PackageWaiting))
                }
                _ => None,
            };
            if let Some((status, kind)) = next {
                events.push(self.event(Some(&id), kind, now));
                if let Ok(package) = self.package_mut(&id) {
                    package.status = status;
                    package.updated_at = now;
                }
            }
        }
    }

    /// Creates the initial event batch for a new mission.
    #[must_use]
    pub fn created_events(&self) -> Vec<MissionEvent> {
        vec![self.event(None, MissionEventKind::MissionCreated, self.created_at)]
    }

    /// Adds one package. It becomes `Ready` at once when it has no unmet
    /// dependency.
    ///
    /// # Errors
    /// Rejects closed missions, duplicate ids, invalid contracts, and unknown,
    /// duplicate, self, or cyclic dependencies.
    pub fn add_package(
        &mut self,
        id: WorkPackageId,
        contract: WorkPackageContract,
        dependencies: Vec<WorkPackageId>,
        now: DateTime<Utc>,
    ) -> Result<MissionChange, MissionError> {
        self.open()?;
        if self.package(&id).is_some() {
            return Err(MissionError::DuplicatePackage(self.id, id));
        }
        let contract = contract.normalized()?;
        self.validate_dependencies(&id, &dependencies)?;
        // A new package cannot close a cycle: nothing depends on it yet.
        self.packages.push(WorkPackage {
            id: id.clone(),
            contract,
            dependencies,
            status: WorkPackageStatus::Planned,
            runs: Vec::new(),
            reason: None,
            result: None,
            created_at: now,
            updated_at: now,
        });
        let mut events = vec![self.event(Some(&id), MissionEventKind::PackageAdded, now)];
        self.settle_readiness(now, &mut events);
        self.touch(now);
        Ok(MissionChange { events })
    }

    /// Replaces the contract of a package nothing has run for.
    ///
    /// # Errors
    /// Rejects closed missions, unknown packages, packages already served by
    /// a run, and invalid contracts.
    pub fn revise_contract(
        &mut self,
        id: &WorkPackageId,
        contract: WorkPackageContract,
        now: DateTime<Utc>,
    ) -> Result<MissionChange, MissionError> {
        self.open()?;
        let contract = contract.normalized()?;
        let package = self.package_mut(id)?;
        if !package.status.accepts_revision() {
            return Err(MissionError::InvalidPackageTransition {
                package_id: id.clone(),
                status: package.status,
                action: "revising the contract",
                expected: "a package nothing has run for",
            });
        }
        if package.contract == contract {
            return Ok(MissionChange { events: Vec::new() });
        }
        package.contract = contract;
        package.updated_at = now;
        let events = vec![self.event(Some(id), MissionEventKind::PackageContractRevised, now)];
        self.touch(now);
        Ok(MissionChange { events })
    }

    /// Replaces the dependency list of a package nothing has run for, then
    /// re-settles readiness.
    ///
    /// # Errors
    /// Rejects closed missions, unknown packages, packages already served by
    /// a run, and invalid or cyclic dependencies.
    pub fn set_dependencies(
        &mut self,
        id: &WorkPackageId,
        dependencies: Vec<WorkPackageId>,
        now: DateTime<Utc>,
    ) -> Result<MissionChange, MissionError> {
        self.open()?;
        self.validate_dependencies(id, &dependencies)?;
        let package = self.package_mut(id)?;
        if !package.status.accepts_revision() {
            return Err(MissionError::InvalidPackageTransition {
                package_id: id.clone(),
                status: package.status,
                action: "changing dependencies",
                expected: "a package nothing has run for",
            });
        }
        if package.dependencies == dependencies {
            return Ok(MissionChange { events: Vec::new() });
        }
        let previous = std::mem::replace(&mut package.dependencies, dependencies.clone());
        if self.find_cycle().is_some() {
            self.package_mut(id)?.dependencies = previous;
            return Err(MissionError::DependencyCycle {
                package_id: id.clone(),
            });
        }
        self.package_mut(id)?.updated_at = now;
        let mut events = vec![self.event(
            Some(id),
            MissionEventKind::PackageDependenciesChanged { dependencies },
            now,
        )];
        self.settle_readiness(now, &mut events);
        self.touch(now);
        Ok(MissionChange { events })
    }

    /// Binds a freshly created child run to a `Ready` package, which becomes
    /// `Running`; a `Planning` mission becomes `Active`.
    ///
    /// # Errors
    /// Rejects closed missions, unknown or non-ready packages, and runs
    /// already bound to any package.
    pub fn start_package(
        &mut self,
        id: &WorkPackageId,
        run_id: RunId,
        now: DateTime<Utc>,
    ) -> Result<MissionChange, MissionError> {
        self.open()?;
        if let Some(owner) = self.package_of_run(run_id) {
            return Err(MissionError::RunAlreadyBound {
                package_id: owner.id.clone(),
                run_id,
            });
        }
        let package = self.package_mut(id)?;
        if package.status != WorkPackageStatus::Ready {
            return Err(MissionError::InvalidPackageTransition {
                package_id: id.clone(),
                status: package.status,
                action: "starting a run",
                expected: "a ready package",
            });
        }
        package.status = WorkPackageStatus::Running;
        package.runs.push(run_id);
        package.reason = None;
        package.result = None;
        package.updated_at = now;
        let mut events =
            vec![self.event(Some(id), MissionEventKind::PackageStarted { run_id }, now)];
        if self.status == MissionStatus::Planning {
            self.status = MissionStatus::Active;
            events.push(self.event(None, MissionEventKind::MissionActivated, now));
        }
        self.touch(now);
        Ok(MissionChange { events })
    }

    /// Reports the committed status of a package's current run. Package
    /// state follows the run: waiting for the user blocks it, completion
    /// delivers it with the `result` the caller captured from the run store,
    /// failure or discard fails it, and a delivered package whose run is at
    /// work again is reworked. Anything else is progress and changes
    /// nothing. Idempotent: repeating the same observation yields no event.
    ///
    /// # Errors
    /// Rejects unknown packages, runs that are not the package's current run,
    /// a result from another run, and a completion reported without a
    /// result. A closed mission observes nothing and returns no change.
    pub fn observe_run(
        &mut self,
        id: &WorkPackageId,
        run_id: RunId,
        run_status: RunStatus,
        reason: Option<&str>,
        result: Option<WorkPackageResult>,
        now: DateTime<Utc>,
    ) -> Result<MissionChange, MissionError> {
        if self.status.is_closed() {
            return Ok(MissionChange { events: Vec::new() });
        }
        let package = self.package_mut(id)?;
        if package.current_run() != Some(run_id) {
            return Err(MissionError::StaleRun {
                package_id: id.clone(),
                run_id,
            });
        }
        if !package.status.has_active_run() && package.status != WorkPackageStatus::Delivered {
            return Ok(MissionChange { events: Vec::new() });
        }
        if let Some(result) = &result
            && result.run_id != run_id
        {
            return Err(MissionError::StaleRun {
                package_id: id.clone(),
                run_id: result.run_id,
            });
        }
        if package.status == WorkPackageStatus::Delivered
            && !matches!(run_status, RunStatus::Failed | RunStatus::Discarded)
        {
            return self.observe_delivered(id, run_id, run_status, reason, result, now);
        }
        let next = active_transition(
            package.status,
            package.reason.as_deref(),
            run_id,
            run_status,
            reason,
            result.is_some(),
        )
        .map_err(|()| MissionError::MissingResult {
            package_id: id.clone(),
            run_id,
        })?;
        let Some((status, reason, kind)) = next else {
            return Ok(MissionChange { events: Vec::new() });
        };
        package.status = status;
        package.reason = reason;
        // A result describes a delivery. A failed rework leaves no delivery
        // to describe, so the old result goes with it rather than standing
        // beside the failure as if it still held.
        package.result = if status == WorkPackageStatus::Delivered {
            result
        } else {
            None
        };
        package.updated_at = now;
        let events = vec![self.event(Some(id), kind, now)];
        self.touch(now);
        Ok(MissionChange { events })
    }

    /// A delivered package's run moved on. At work again, the package is
    /// reworked on the same run and in progress once more. Finished again
    /// with more stages than the result covers (a cycle completed before
    /// anyone looked), the caller's fresh capture replaces the result.
    fn observe_delivered(
        &mut self,
        id: &WorkPackageId,
        run_id: RunId,
        run_status: RunStatus,
        reason: Option<&str>,
        result: Option<WorkPackageResult>,
        now: DateTime<Utc>,
    ) -> Result<MissionChange, MissionError> {
        let package = self.package_mut(id)?;
        if matches!(run_status, RunStatus::Completed | RunStatus::Applied) {
            let Some(fresh) = result else {
                return Ok(MissionChange { events: Vec::new() });
            };
            if package.result.as_ref().map(|r| r.stage_count) == Some(fresh.stage_count) {
                return Ok(MissionChange { events: Vec::new() });
            }
            // A delivery that never had its result captured is not a
            // rework: the evidence is simply late. The delivery is recorded
            // again, now with it, and nothing says the package went back.
            let first_capture = package.result.is_none();
            package.result = Some(fresh);
            package.updated_at = now;
            if first_capture {
                let events =
                    vec![self.event(Some(id), MissionEventKind::PackageDelivered { run_id }, now)];
                self.touch(now);
                return Ok(MissionChange { events });
            }
            let events = vec![
                self.event(Some(id), MissionEventKind::PackageReworked { run_id }, now),
                self.event(Some(id), MissionEventKind::PackageDelivered { run_id }, now),
            ];
            self.touch(now);
            return Ok(MissionChange { events });
        }
        package.status = WorkPackageStatus::Running;
        package.updated_at = now;
        let reworked = self.event(Some(id), MissionEventKind::PackageReworked { run_id }, now);
        let mut change = self.observe_run(id, run_id, run_status, reason, None, now)?;
        change.events.insert(0, reworked);
        self.touch(now);
        Ok(change)
    }

    /// Records that a delivered package's change reached the source checkout,
    /// on the evidence the caller read from the run store. Dependents whose
    /// every dependency is now integrated become `Ready`.
    ///
    /// # Errors
    /// Rejects closed missions and unknown or undelivered packages.
    pub fn integrate_package(
        &mut self,
        id: &WorkPackageId,
        evidence: IntegrationEvidence,
        now: DateTime<Utc>,
    ) -> Result<MissionChange, MissionError> {
        self.open()?;
        let package = self.package_mut(id)?;
        if package.status != WorkPackageStatus::Delivered {
            return Err(MissionError::InvalidPackageTransition {
                package_id: id.clone(),
                status: package.status,
                action: "integrating",
                expected: "a delivered package",
            });
        }
        let Some(run_id) = package.current_run() else {
            return Err(MissionError::InvalidPackageTransition {
                package_id: id.clone(),
                status: package.status,
                action: "integrating",
                expected: "a delivered package with a run",
            });
        };
        package.status = WorkPackageStatus::Integrated;
        package.reason = None;
        package.updated_at = now;
        let mut events = vec![self.event(
            Some(id),
            MissionEventKind::PackageIntegrated { run_id, evidence },
            now,
        )];
        self.settle_readiness(now, &mut events);
        self.touch(now);
        Ok(MissionChange { events })
    }

    /// Returns a failed package to `Planned` so a new run can serve it; it
    /// becomes `Ready` at once when its dependencies are still integrated.
    ///
    /// # Errors
    /// Rejects closed missions and packages that are not failed.
    pub fn retry_package(
        &mut self,
        id: &WorkPackageId,
        now: DateTime<Utc>,
    ) -> Result<MissionChange, MissionError> {
        self.open()?;
        let package = self.package_mut(id)?;
        if package.status != WorkPackageStatus::Failed {
            return Err(MissionError::InvalidPackageTransition {
                package_id: id.clone(),
                status: package.status,
                action: "retrying",
                expected: "a failed package",
            });
        }
        package.status = WorkPackageStatus::Planned;
        package.reason = None;
        package.result = None;
        package.updated_at = now;
        let mut events = vec![self.event(Some(id), MissionEventKind::PackageRetryScheduled, now)];
        self.settle_readiness(now, &mut events);
        self.touch(now);
        Ok(MissionChange { events })
    }

    /// Cancels a package no run is serving and nothing depends on. A package
    /// with a live run — running, waiting, or stopped — is not cancelled
    /// around its run: discard the run and the package fails, then cancel.
    ///
    /// # Errors
    /// Rejects closed missions, packages with a run in flight, delivered or
    /// closed packages, and packages other open packages still depend on.
    pub fn cancel_package(
        &mut self,
        id: &WorkPackageId,
        reason: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<MissionChange, MissionError> {
        self.open()?;
        let dependents: Vec<WorkPackageId> = self
            .packages
            .iter()
            .filter(|package| {
                package.status != WorkPackageStatus::Cancelled && package.dependencies.contains(id)
            })
            .map(|package| package.id.clone())
            .collect();
        let package = self.package_mut(id)?;
        if !matches!(
            package.status,
            WorkPackageStatus::Planned | WorkPackageStatus::Ready | WorkPackageStatus::Failed
        ) {
            return Err(MissionError::InvalidPackageTransition {
                package_id: id.clone(),
                status: package.status,
                action: "cancelling",
                expected: "a planned, ready, or failed package (discard a live run first)",
            });
        }
        if !dependents.is_empty() {
            return Err(MissionError::PackageHasDependents {
                package_id: id.clone(),
                dependents,
            });
        }
        let reason = reason.into();
        package.status = WorkPackageStatus::Cancelled;
        package.reason = Some(reason.clone());
        package.updated_at = now;
        let events = vec![self.event(Some(id), MissionEventKind::PackageCancelled { reason }, now)];
        self.touch(now);
        Ok(MissionChange { events })
    }

    /// Appends one decision.
    ///
    /// # Errors
    /// Rejects closed missions and blank titles or rationales.
    pub fn record_decision(
        &mut self,
        id: DecisionId,
        title: impl Into<String>,
        rationale: impl Into<String>,
        author: DecisionAuthor,
        now: DateTime<Utc>,
    ) -> Result<MissionChange, MissionError> {
        self.open()?;
        let title = title.into().trim().to_owned();
        let rationale = rationale.into().trim().to_owned();
        if title.is_empty() {
            return Err(MissionError::EmptyDecisionField("title"));
        }
        if rationale.is_empty() {
            return Err(MissionError::EmptyDecisionField("rationale"));
        }
        self.decisions.push(MissionDecision {
            id,
            title,
            rationale,
            author,
            recorded_at: now,
        });
        let events = vec![self.event(
            None,
            MissionEventKind::DecisionRecorded { decision_id: id },
            now,
        )];
        self.touch(now);
        Ok(MissionChange { events })
    }

    /// Completes the mission once every package is integrated or cancelled
    /// and at least one is integrated.
    ///
    /// # Errors
    /// Rejects closed missions and unfinished plans, naming what is open.
    pub fn complete(&mut self, now: DateTime<Utc>) -> Result<MissionChange, MissionError> {
        self.open()?;
        let open: Vec<String> = self
            .packages
            .iter()
            .filter(|package| !package.status.is_closed())
            .map(|package| format!("{} is {:?}", package.id, package.status))
            .collect();
        if !open.is_empty() {
            return Err(MissionError::CannotComplete(open.join(", ")));
        }
        if !self
            .packages
            .iter()
            .any(|package| package.status == WorkPackageStatus::Integrated)
        {
            return Err(MissionError::CannotComplete(
                "no package was integrated".to_owned(),
            ));
        }
        self.status = MissionStatus::Completed;
        let events = vec![self.event(None, MissionEventKind::MissionCompleted, now)];
        self.touch(now);
        Ok(MissionChange { events })
    }

    /// Cancels the mission and every open package. A package with a run in
    /// progress must be stopped first: cancellation never reaches into a run.
    ///
    /// # Errors
    /// Rejects closed missions and missions with a running package.
    pub fn cancel(
        &mut self,
        reason: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<MissionChange, MissionError> {
        self.open()?;
        if let Some(package) = self
            .packages
            .iter()
            .find(|package| package.status.has_active_run())
        {
            return Err(MissionError::PackageStillRunning(package.id.clone()));
        }
        let reason = reason.into();
        let mut events = Vec::new();
        let ids: Vec<WorkPackageId> = self
            .packages
            .iter()
            .filter(|package| !package.status.is_closed())
            .map(|package| package.id.clone())
            .collect();
        for id in ids {
            events.push(self.event(
                Some(&id),
                MissionEventKind::PackageCancelled {
                    reason: reason.clone(),
                },
                now,
            ));
            let package = self.package_mut(&id)?;
            package.status = WorkPackageStatus::Cancelled;
            package.reason = Some(reason.clone());
            package.updated_at = now;
        }
        self.status = MissionStatus::Cancelled;
        events.push(self.event(None, MissionEventKind::MissionCancelled { reason }, now));
        self.touch(now);
        Ok(MissionChange { events })
    }

    /// Package ids in dependency order: every package after all it depends
    /// on, ties broken by creation order.
    #[must_use]
    pub fn topological_order(&self) -> Vec<WorkPackageId> {
        let mut order = Vec::with_capacity(self.packages.len());
        let mut placed: HashSet<WorkPackageId> = HashSet::new();
        while order.len() < self.packages.len() {
            let before = order.len();
            for package in &self.packages {
                if placed.contains(&package.id) {
                    continue;
                }
                if package
                    .dependencies
                    .iter()
                    .all(|dependency| placed.contains(dependency))
                {
                    placed.insert(package.id.clone());
                    order.push(package.id.clone());
                }
            }
            if order.len() == before {
                // Only reachable on a cyclic graph, which invariants reject;
                // append the rest in creation order rather than spin.
                for package in &self.packages {
                    if placed.insert(package.id.clone()) {
                        order.push(package.id.clone());
                    }
                }
            }
        }
        order
    }
}

/// The next state of a package with a live run, given the run's committed
/// status. `Err(())` is a completion reported without a result.
#[allow(clippy::type_complexity, reason = "one transition triple, used once")]
fn active_transition(
    status: WorkPackageStatus,
    current_reason: Option<&str>,
    run_id: RunId,
    run_status: RunStatus,
    reason: Option<&str>,
    has_result: bool,
) -> Result<Option<(WorkPackageStatus, Option<String>, MissionEventKind)>, ()> {
    let given = reason.filter(|text| !text.trim().is_empty());
    let blocked_reason =
        || given.map_or_else(|| "the run is waiting for you".to_owned(), str::to_owned);
    let failed_reason = |fallback: &str| given.map_or_else(|| fallback.to_owned(), str::to_owned);
    let stopped_reason =
        |what: &str| format!("the run is {what}; resume it (`senate resume`) or discard it");
    // A blocked package whose run moved between waiting, paused and
    // interrupted is blocked for a new reason; the same reason again is the
    // same observation and yields nothing.
    let block_unless_same = |reason: String| {
        if status == WorkPackageStatus::Blocked && current_reason == Some(reason.as_str()) {
            None
        } else {
            Some((
                WorkPackageStatus::Blocked,
                Some(reason.clone()),
                MissionEventKind::PackageBlocked { run_id, reason },
            ))
        }
    };
    let fail = |reason: String| {
        Some((
            WorkPackageStatus::Failed,
            Some(reason.clone()),
            MissionEventKind::PackageFailed { run_id, reason },
        ))
    };
    Ok(match (status, run_status) {
        (_, RunStatus::NeedsUser) => block_unless_same(blocked_reason()),
        (_, RunStatus::Paused) => block_unless_same(stopped_reason("paused")),
        (_, RunStatus::Interrupted) => block_unless_same(stopped_reason("interrupted")),
        (
            WorkPackageStatus::Blocked,
            RunStatus::Running | RunStatus::Ready | RunStatus::Preparing | RunStatus::Created,
        ) => Some((
            WorkPackageStatus::Running,
            None,
            MissionEventKind::PackageUnblocked { run_id },
        )),
        (_, RunStatus::Completed | RunStatus::Applied) => {
            if !has_result {
                return Err(());
            }
            Some((
                WorkPackageStatus::Delivered,
                None,
                MissionEventKind::PackageDelivered { run_id },
            ))
        }
        (_, RunStatus::Failed) => fail(failed_reason("the run failed")),
        (_, RunStatus::Discarded) => fail(failed_reason("the run was discarded")),
        _ => None,
    })
}

fn visit(
    graph: &HashMap<&WorkPackageId, &[WorkPackageId]>,
    node: &WorkPackageId,
    visiting: &mut HashSet<WorkPackageId>,
    done: &mut HashSet<WorkPackageId>,
) -> Option<WorkPackageId> {
    if done.contains(node) {
        return None;
    }
    if !visiting.insert(node.clone()) {
        return Some(node.clone());
    }
    if let Some(dependencies) = graph.get(node) {
        for dependency in *dependencies {
            if let Some(cycle) = visit(graph, dependency, visiting, done) {
                return Some(cycle);
            }
        }
    }
    visiting.remove(node);
    done.insert(node.clone());
    None
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 22, 10, 0, second).unwrap()
    }

    fn contract(title: &str) -> WorkPackageContract {
        WorkPackageContract {
            title: title.to_owned(),
            goal: format!("deliver {title}"),
            rationale: String::new(),
            scope: String::new(),
            acceptance_criteria: vec!["it works".to_owned()],
            verification: String::new(),
            workflow: WorkflowKind::Fast,
        }
    }

    fn id(value: &str) -> WorkPackageId {
        WorkPackageId::new(value).unwrap()
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "matches the observe_run parameter"
    )]
    fn result_of(run: RunId) -> Option<WorkPackageResult> {
        Some(WorkPackageResult {
            run_id: run,
            captured_at: at(0),
            stage_count: 2,
            changed_files: vec![],
            changes_complete: true,
            bottom_line: None,
            verification: None,
            reviews: vec![],
            decision: None,
            open_questions: None,
        })
    }

    /// The rehydration data of `mission`, after `mutate` has had its way.
    fn rehydration_data(
        mission: &Mission,
        mutate: &dyn Fn(&mut MissionRehydrationData),
    ) -> MissionRehydrationData {
        let mut data = MissionRehydrationData {
            id: mission.id(),
            status: mission.status(),
            packages: mission
                .packages()
                .iter()
                .map(|package| WorkPackageRehydrationData {
                    id: package.id().clone(),
                    contract: package.contract().clone(),
                    dependencies: package.dependencies().to_vec(),
                    status: package.status(),
                    runs: package.runs().to_vec(),
                    reason: package.reason().map(str::to_owned),
                    result: package.result().cloned(),
                    created_at: *package.created_at(),
                    updated_at: *package.updated_at(),
                })
                .collect(),
            decisions: mission.decisions().to_vec(),
            created_at: *mission.created_at(),
            updated_at: *mission.updated_at(),
        };
        mutate(&mut data);
        data
    }

    fn kinds(change: &MissionChange) -> Vec<&MissionEventKind> {
        change.events.iter().map(MissionEvent::kind).collect()
    }

    fn mission_with_chain() -> Mission {
        let mut mission = Mission::new(MissionId::from_u128(1), at(0));
        mission
            .add_package(id("persistence"), contract("Persistence"), vec![], at(1))
            .unwrap();
        mission
            .add_package(
                id("memory"),
                contract("Memory"),
                vec![id("persistence")],
                at(2),
            )
            .unwrap();
        mission
    }

    #[test]
    fn a_package_without_dependencies_is_ready_at_once_and_a_dependent_waits() {
        let mission = mission_with_chain();
        assert_eq!(
            mission.package(&id("persistence")).unwrap().status(),
            WorkPackageStatus::Ready
        );
        assert_eq!(
            mission.package(&id("memory")).unwrap().status(),
            WorkPackageStatus::Planned
        );
        assert_eq!(mission.status(), MissionStatus::Planning);
        mission.validate_invariants().unwrap();
    }

    #[test]
    fn integration_of_a_dependency_readies_its_dependents_and_needs_an_applied_run() {
        let mut mission = mission_with_chain();
        let run = RunId::from_u128(10);
        let change = mission
            .start_package(&id("persistence"), run, at(3))
            .unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [
                MissionEventKind::PackageStarted { .. },
                MissionEventKind::MissionActivated
            ]
        ));
        assert_eq!(mission.status(), MissionStatus::Active);

        let change = mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Completed,
                None,
                result_of(run),
                at(4),
            )
            .unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [MissionEventKind::PackageDelivered { .. }]
        ));
        assert_eq!(
            mission.attention().awaiting_integration,
            vec![id("persistence")]
        );

        let error = mission
            .integrate_package(&id("memory"), IntegrationEvidence::Applied, at(5))
            .unwrap_err();
        assert!(matches!(
            error,
            MissionError::InvalidPackageTransition { .. }
        ));

        let change = mission
            .integrate_package(&id("persistence"), IntegrationEvidence::Applied, at(5))
            .unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [
                MissionEventKind::PackageIntegrated { .. },
                MissionEventKind::PackageReady
            ]
        ));
        assert_eq!(
            mission.package(&id("memory")).unwrap().status(),
            WorkPackageStatus::Ready
        );
        assert!(mission.attention().is_empty());
        mission.validate_invariants().unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one scenario, start to finish")]
    fn observing_a_run_is_idempotent_and_blocks_and_unblocks_with_the_run() {
        let mut mission = mission_with_chain();
        let run = RunId::from_u128(10);
        mission
            .start_package(&id("persistence"), run, at(3))
            .unwrap();

        let change = mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Running,
                None,
                None,
                at(4),
            )
            .unwrap();
        assert!(!change.changed());

        let change = mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::NeedsUser,
                Some("permission: Bash"),
                None,
                at(5),
            )
            .unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [MissionEventKind::PackageBlocked { reason, .. }] if reason == "permission: Bash"
        ));
        assert_eq!(
            mission.attention().blocked,
            vec![(id("persistence"), "permission: Bash".to_owned())]
        );
        let again = mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::NeedsUser,
                Some("permission: Bash"),
                None,
                at(6),
            )
            .unwrap();
        assert!(!again.changed());
        let stopped = mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Interrupted,
                None,
                None,
                at(6),
            )
            .unwrap();
        assert!(matches!(
            kinds(&stopped).as_slice(),
            [MissionEventKind::PackageBlocked { reason, .. }] if reason.contains("interrupted")
        ));
        assert!(
            !mission
                .observe_run(
                    &id("persistence"),
                    run,
                    RunStatus::Interrupted,
                    None,
                    None,
                    at(6)
                )
                .unwrap()
                .changed()
        );

        let change = mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Running,
                None,
                None,
                at(7),
            )
            .unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [MissionEventKind::PackageUnblocked { .. }]
        ));
        assert!(
            mission
                .package(&id("persistence"))
                .unwrap()
                .reason()
                .is_none()
        );

        let change = mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Interrupted,
                None,
                None,
                at(8),
            )
            .unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [MissionEventKind::PackageBlocked { reason, .. }] if reason.contains("interrupted")
        ));
        assert!(matches!(
            mission.cancel_package(&id("persistence"), "drop", at(8)),
            Err(MissionError::InvalidPackageTransition { .. })
        ));
        mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Running,
                None,
                None,
                at(9),
            )
            .unwrap();

        let stale = mission
            .observe_run(
                &id("persistence"),
                RunId::from_u128(99),
                RunStatus::Completed,
                None,
                result_of(RunId::from_u128(99)),
                at(8),
            )
            .unwrap_err();
        assert!(matches!(stale, MissionError::StaleRun { .. }));
        mission.validate_invariants().unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one scenario, start to finish")]
    fn delivery_needs_a_result_from_the_same_run_and_rework_replaces_it() {
        let mut mission = mission_with_chain();
        let run = RunId::from_u128(10);
        mission
            .start_package(&id("persistence"), run, at(3))
            .unwrap();
        assert!(matches!(
            mission.observe_run(
                &id("persistence"),
                run,
                RunStatus::Completed,
                None,
                None,
                at(4)
            ),
            Err(MissionError::MissingResult { .. })
        ));
        assert!(matches!(
            mission.observe_run(
                &id("persistence"),
                run,
                RunStatus::Completed,
                None,
                result_of(RunId::from_u128(11)),
                at(4)
            ),
            Err(MissionError::StaleRun { .. })
        ));
        let mut first = result_of(run).unwrap();
        first.bottom_line = Some("first delivery".to_owned());
        mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Completed,
                None,
                Some(first),
                at(4),
            )
            .unwrap();
        assert_eq!(
            mission
                .package(&id("persistence"))
                .unwrap()
                .result()
                .unwrap()
                .bottom_line
                .as_deref(),
            Some("first delivery")
        );

        // A fix cycle puts the same run back to work: reworked, then blocked
        // by the run in one observation, and the old result stays until the
        // next delivery replaces it.
        let change = mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::NeedsUser,
                None,
                None,
                at(5),
            )
            .unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [
                MissionEventKind::PackageReworked { .. },
                MissionEventKind::PackageBlocked { .. }
            ]
        ));
        assert_eq!(
            mission.package(&id("persistence")).unwrap().status(),
            WorkPackageStatus::Blocked
        );
        let mut second = result_of(run).unwrap();
        second.bottom_line = Some("second delivery".to_owned());
        mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Completed,
                None,
                Some(second),
                at(6),
            )
            .unwrap();
        assert_eq!(
            mission
                .package(&id("persistence"))
                .unwrap()
                .result()
                .unwrap()
                .bottom_line
                .as_deref(),
            Some("second delivery")
        );
        // Applied after delivery is not rework.
        assert!(
            !mission
                .observe_run(
                    &id("persistence"),
                    run,
                    RunStatus::Applied,
                    None,
                    None,
                    at(7)
                )
                .unwrap()
                .changed()
        );
        // A rework that fails takes the stale result with it.
        let mut failing = mission.clone();
        failing
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Failed,
                None,
                None,
                at(8),
            )
            .unwrap();
        let failed = failing.package(&id("persistence")).unwrap();
        assert_eq!(failed.status(), WorkPackageStatus::Failed);
        assert!(failed.result().is_none());
        failing.validate_invariants().unwrap();

        // A run that grew and completed before anyone looked is re-delivered
        // from the fresh capture; the same capture again is silent.
        let mut grown = result_of(run).unwrap();
        grown.stage_count = 5;
        let change = mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Completed,
                None,
                Some(grown.clone()),
                at(8),
            )
            .unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [
                MissionEventKind::PackageReworked { .. },
                MissionEventKind::PackageDelivered { .. }
            ]
        ));
        assert!(
            !mission
                .observe_run(
                    &id("persistence"),
                    run,
                    RunStatus::Completed,
                    None,
                    Some(grown),
                    at(9)
                )
                .unwrap()
                .changed()
        );
        mission.validate_invariants().unwrap();
    }

    /// A package delivered before results were captured (snapshot v1)
    /// rehydrates without one, and the first observe that brings a result
    /// records the delivery again with it rather than as a rework.
    #[test]
    fn a_delivery_without_a_result_is_legacy_not_corruption_and_fills_in_once() {
        let chain = mission_with_chain();
        let mut mission = Mission::rehydrate(rehydration_data(&chain, &|d| {
            d.status = MissionStatus::Active;
            d.packages[0].status = WorkPackageStatus::Delivered;
            d.packages[0].runs = vec![RunId::from_u128(1)];
            d.packages[0].result = None;
        }))
        .unwrap();
        assert!(
            mission
                .package(&id("persistence"))
                .unwrap()
                .result()
                .is_none()
        );

        let change = mission
            .observe_run(
                &id("persistence"),
                RunId::from_u128(1),
                RunStatus::Completed,
                None,
                result_of(RunId::from_u128(1)),
                at(9),
            )
            .unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [MissionEventKind::PackageDelivered { .. }]
        ));
        assert!(
            mission
                .package(&id("persistence"))
                .unwrap()
                .result()
                .is_some()
        );
        let change = mission
            .observe_run(
                &id("persistence"),
                RunId::from_u128(1),
                RunStatus::Completed,
                None,
                result_of(RunId::from_u128(1)),
                at(10),
            )
            .unwrap();
        assert!(change.events.is_empty(), "a repeat capture changes nothing");
        mission.validate_invariants().unwrap();
    }

    #[test]
    fn a_failed_package_is_retried_into_a_fresh_ready_state_with_history_kept() {
        let mut mission = mission_with_chain();
        let run = RunId::from_u128(10);
        mission
            .start_package(&id("persistence"), run, at(3))
            .unwrap();
        mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Failed,
                Some("provider error"),
                None,
                at(4),
            )
            .unwrap();
        assert_eq!(
            mission.attention().failed,
            vec![(id("persistence"), "provider error".to_owned())]
        );
        let error = mission
            .start_package(&id("persistence"), RunId::from_u128(11), at(5))
            .unwrap_err();
        assert!(matches!(
            error,
            MissionError::InvalidPackageTransition { .. }
        ));

        let change = mission.retry_package(&id("persistence"), at(5)).unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [
                MissionEventKind::PackageRetryScheduled,
                MissionEventKind::PackageReady
            ]
        ));
        mission
            .start_package(&id("persistence"), RunId::from_u128(11), at(6))
            .unwrap();
        assert_eq!(
            mission.package(&id("persistence")).unwrap().runs(),
            &[run, RunId::from_u128(11)]
        );
        let rebound = mission
            .start_package(&id("memory"), run, at(7))
            .unwrap_err();
        assert!(matches!(rebound, MissionError::RunAlreadyBound { .. }));
        mission.validate_invariants().unwrap();
    }

    #[test]
    fn dependencies_reject_self_unknown_duplicate_and_cyclic_edges() {
        let mut mission = mission_with_chain();
        assert!(matches!(
            mission.set_dependencies(&id("memory"), vec![id("memory")], at(3)),
            Err(MissionError::SelfDependency { .. })
        ));
        assert!(matches!(
            mission.set_dependencies(&id("memory"), vec![id("ghost")], at(3)),
            Err(MissionError::UnknownDependency { .. })
        ));
        assert!(matches!(
            mission.set_dependencies(
                &id("memory"),
                vec![id("persistence"), id("persistence")],
                at(3)
            ),
            Err(MissionError::DuplicateDependency { .. })
        ));
        assert!(matches!(
            mission.set_dependencies(&id("persistence"), vec![id("memory")], at(3)),
            Err(MissionError::DependencyCycle { .. })
        ));
        // The failed attempt left the graph untouched.
        assert!(
            mission
                .package(&id("persistence"))
                .unwrap()
                .dependencies()
                .is_empty()
        );

        let change = mission
            .set_dependencies(&id("memory"), vec![], at(4))
            .unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [
                MissionEventKind::PackageDependenciesChanged { .. },
                MissionEventKind::PackageReady
            ]
        ));
        let change = mission
            .set_dependencies(&id("memory"), vec![id("persistence")], at(5))
            .unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [
                MissionEventKind::PackageDependenciesChanged { .. },
                MissionEventKind::PackageWaiting
            ]
        ));
        mission.validate_invariants().unwrap();
    }

    #[test]
    fn contracts_freeze_once_a_run_serves_the_package() {
        let mut mission = mission_with_chain();
        let mut revised = contract("Persistence");
        revised.goal = "persist every character".to_owned();
        let change = mission
            .revise_contract(&id("persistence"), revised.clone(), at(3))
            .unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [MissionEventKind::PackageContractRevised]
        ));
        assert!(
            !mission
                .revise_contract(&id("persistence"), revised, at(4))
                .unwrap()
                .changed()
        );
        mission
            .start_package(&id("persistence"), RunId::from_u128(1), at(5))
            .unwrap();
        assert!(matches!(
            mission.revise_contract(&id("persistence"), contract("Other"), at(6)),
            Err(MissionError::InvalidPackageTransition { .. })
        ));
        assert!(matches!(
            mission.set_dependencies(&id("persistence"), vec![], at(6)),
            Err(MissionError::InvalidPackageTransition { .. })
        ));
        let blank = WorkPackageContract {
            goal: " ".to_owned(),
            ..contract("Blank")
        };
        assert!(matches!(
            mission.add_package(id("blank"), blank, vec![], at(7)),
            Err(MissionError::EmptyContractField("goal"))
        ));
    }

    #[test]
    fn cancelling_respects_dependents_and_running_work() {
        let mut mission = mission_with_chain();
        assert!(matches!(
            mission.cancel_package(&id("persistence"), "drop", at(3)),
            Err(MissionError::PackageHasDependents { .. })
        ));
        mission
            .cancel_package(&id("memory"), "later", at(3))
            .unwrap();
        mission
            .cancel_package(&id("persistence"), "drop", at(4))
            .unwrap();
        assert!(matches!(
            mission.complete(at(5)),
            Err(MissionError::CannotComplete(reason)) if reason.contains("no package was integrated")
        ));

        let mut mission = mission_with_chain();
        let run = RunId::from_u128(10);
        mission
            .start_package(&id("persistence"), run, at(3))
            .unwrap();
        assert!(matches!(
            mission.cancel("stop", at(4)),
            Err(MissionError::PackageStillRunning(_))
        ));
        assert!(matches!(
            mission.cancel_package(&id("persistence"), "drop", at(4)),
            Err(MissionError::InvalidPackageTransition { .. })
        ));
        mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Failed,
                None,
                None,
                at(5),
            )
            .unwrap();
        let change = mission.cancel("stop", at(6)).unwrap();
        assert_eq!(change.events.len(), 3);
        assert_eq!(mission.status(), MissionStatus::Cancelled);
        assert!(
            mission
                .packages()
                .iter()
                .all(|package| package.status() == WorkPackageStatus::Cancelled)
        );
        assert!(matches!(
            mission.add_package(id("late"), contract("Late"), vec![], at(7)),
            Err(MissionError::MissionClosed { .. })
        ));
        assert!(
            !mission
                .observe_run(
                    &id("persistence"),
                    run,
                    RunStatus::Completed,
                    None,
                    result_of(run),
                    at(8)
                )
                .unwrap()
                .changed()
        );
        mission.validate_invariants().unwrap();
    }

    #[test]
    fn completion_needs_every_package_closed_and_one_integrated() {
        let mut mission = mission_with_chain();
        let run = RunId::from_u128(10);
        mission
            .start_package(&id("persistence"), run, at(3))
            .unwrap();
        mission
            .observe_run(
                &id("persistence"),
                run,
                RunStatus::Applied,
                None,
                result_of(run),
                at(4),
            )
            .unwrap();
        mission
            .integrate_package(&id("persistence"), IntegrationEvidence::NoChanges, at(5))
            .unwrap();
        assert!(matches!(
            mission.complete(at(6)),
            Err(MissionError::CannotComplete(reason)) if reason.contains("memory is Ready")
        ));
        mission
            .cancel_package(&id("memory"), "not now", at(6))
            .unwrap();
        let change = mission.complete(at(7)).unwrap();
        assert!(matches!(
            kinds(&change).as_slice(),
            [MissionEventKind::MissionCompleted]
        ));
        mission.validate_invariants().unwrap();
    }

    #[test]
    fn decisions_are_recorded_in_order_with_their_author() {
        let mut mission = mission_with_chain();
        let decision = DecisionId::from_u128(7);
        mission
            .record_decision(
                decision,
                "Persist before memory",
                "memory needs a durable substrate",
                DecisionAuthor::Lead,
                at(3),
            )
            .unwrap();
        assert!(matches!(
            mission.record_decision(
                DecisionId::from_u128(8),
                " ",
                "x",
                DecisionAuthor::User,
                at(4)
            ),
            Err(MissionError::EmptyDecisionField("title"))
        ));
        assert_eq!(mission.decisions().len(), 1);
        assert_eq!(mission.decisions()[0].id, decision);
        assert_eq!(mission.decisions()[0].author, DecisionAuthor::Lead);
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one scenario, start to finish")]
    fn rehydration_rejects_states_the_aggregate_could_not_reach() {
        let mission = mission_with_chain();
        let data =
            |mutate: &dyn Fn(&mut MissionRehydrationData)| rehydration_data(&mission, mutate);

        assert_eq!(Mission::rehydrate(data(&|_| {})).unwrap(), mission);
        assert!(matches!(
            Mission::rehydrate(data(&|d| d.packages[1].status = WorkPackageStatus::Ready)),
            Err(MissionInvariantError::ReadyWithUnintegratedDependency(..))
        ));
        assert!(matches!(
            Mission::rehydrate(data(&|d| d.packages[0].status = WorkPackageStatus::Running)),
            Err(MissionInvariantError::MissingRun(..))
        ));
        assert!(matches!(
            Mission::rehydrate(data(&|d| {
                d.packages[0].status = WorkPackageStatus::Failed;
                d.packages[0].runs = vec![RunId::from_u128(1)];
            })),
            Err(MissionInvariantError::MissingReason(..))
        ));
        assert!(matches!(
            Mission::rehydrate(data(&|d| {
                d.packages[0].dependencies = vec![id("memory")];
                d.packages[0].status = WorkPackageStatus::Planned;
            })),
            Err(MissionInvariantError::DependencyCycle(_))
        ));
        assert!(matches!(
            Mission::rehydrate(data(&|d| {
                d.packages[0].runs = vec![RunId::from_u128(1)];
                d.packages[1].runs = vec![RunId::from_u128(1)];
                d.packages[0].status = WorkPackageStatus::Running;
                d.packages[1].status = WorkPackageStatus::Running;
                d.status = MissionStatus::Active;
            })),
            Err(MissionInvariantError::RunBoundTwice(_))
        ));
        assert!(matches!(
            Mission::rehydrate(data(&|d| d.status = MissionStatus::Completed)),
            Err(MissionInvariantError::PackageStatusConflictsWithMission(..))
        ));
        assert!(matches!(
            Mission::rehydrate(data(&|d| {
                d.packages[0].status = WorkPackageStatus::Running;
                d.packages[0].runs = vec![RunId::from_u128(1)];
            })),
            Err(MissionInvariantError::PackageStatusConflictsWithMission(..))
        ));
        assert!(matches!(
            Mission::rehydrate(data(&|d| d.packages[1].id = id("persistence"))),
            Err(MissionInvariantError::DuplicatePackage(_))
        ));
        assert!(matches!(
            Mission::rehydrate(data(&|d| {
                d.status = MissionStatus::Active;
                d.packages[0].status = WorkPackageStatus::Delivered;
                d.packages[0].runs = vec![RunId::from_u128(1)];
                d.packages[0].result = result_of(RunId::from_u128(2));
            })),
            Err(MissionInvariantError::ResultMismatch(_))
        ));
    }

    #[test]
    fn topological_order_places_dependencies_first() {
        let mut mission = mission_with_chain();
        mission
            .add_package(id("journal"), contract("Journal"), vec![], at(3))
            .unwrap();
        mission
            .add_package(
                id("policy"),
                contract("Policy"),
                vec![id("memory"), id("journal")],
                at(4),
            )
            .unwrap();
        assert_eq!(
            mission.topological_order(),
            vec![id("persistence"), id("memory"), id("journal"), id("policy")]
        );
    }
}
