use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::domain::{
    AttentionKind, AttentionRequestId, ConfigSnapshotId, DomainEvent, EventId, EventMetadata, Run,
    RunId, RunStatus, RunTransition, StageId, StageStatus, WorkflowDefinition, WorkflowKind,
};
use crate::engine::{EngineStatus, WorkflowEngine};
use crate::git::GitRepository;
use crate::process::{ManagedProcessStatus, ProcessManager};
use crate::store::{
    ResolvedConfigSnapshot, RunInput, SqliteStore, StoreError, database_file, process_root,
    worktree_root,
};
use crate::workspace::{
    GhClient, PullRequestReach, PullRequestRef, RebaseReceipt, ReconciliationOutcome,
    RemoteRepository, WorkspaceError, WorkspaceManager, WorkspaceStatus,
};

use super::provider_factory::{ProviderFactory, ProviderResolver};
use super::{
    AppError, ArtifactSummary, ArtifactView, CommittedEvent, EffortRequest, ExecutionSelection,
    ImageGenerationPlan, OPERATOR_OVERRIDE_REASON, ProcessLogView, RetryRoute, RunDetails,
    RunDiffPreview, RunListItem, StageExecutionEvidence, query,
};

const DIFF_PREVIEW_LIMIT: usize = 2 * 1024 * 1024;
const PROCESS_LOG_TAIL_LIMIT: usize = 256 * 1024;
/// How many times a stop retries after losing a revision race with the process
/// that is still driving the run. A stop reconciles through the engine, which
/// touches the run, its managed processes, and its provider sessions, so a busy
/// driver offers several rows to collide on. Bounded well under a second in
/// total: the driver is being interrupted, so contention ends quickly.
const STOP_CONCURRENCY_ATTEMPTS: u32 = 12;
const STOP_CONCURRENCY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(40);
/// How many times a stop retries while a managed process has started but has
/// not yet recorded the runtime evidence a signal needs. The runner spawns its
/// child before writing `runtime.json`, so a user who presses stop in the first
/// moments of a run can arrive inside that window. The evidence is imminent
/// whenever the runner is alive, so waiting it out is right; the bound exists
/// because the same error also carries permanent conditions — corrupt or
/// foreign evidence — which must surface as a failure rather than as an
/// unbounded wait. Roughly three seconds in total, and exhausting it returns
/// `StopRuntimeEvidenceUnavailable`: never a stop that reports success while
/// the process keeps running.
const STOP_EVIDENCE_ATTEMPTS: u32 = 75;
const STOP_EVIDENCE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(40);
/// How long a run must sit untouched before a read is allowed to settle it.
///
/// Reading a run must never race the process driving it. A driver commits on
/// every provider signal it observes, so a run still under execution keeps a
/// fresh `updated_at`; only a run nothing has touched for this long is treated
/// as abandoned. Long enough to cover a stage handover, short enough that a
/// dead run tells the truth on the next refresh instead of the next stop.
const ABANDONED_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    NoChanges,
}

/// What a purge destroyed. The rows always go; the run's directory of
/// artifacts and logs may already have been gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PurgeReceipt {
    pub run_id: RunId,
    pub files_removed: bool,
}

/// What a run's start is doing, in the order it does it.
///
/// Everything before [`StartProgress::PreparingWorkspace`] happens before the
/// run exists anywhere, so a start refused there leaves nothing behind for a
/// list or a detail screen to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartProgress {
    /// The chosen checkout is not the pull request's repository; looking for
    /// a local one that is.
    FindingCheckout { repository: String },
    /// Asking `gh` whether the pull request can be read at all.
    CheckingPullRequest { url: String, host: String },
    /// The run is persisted; its worktree is being created.
    PreparingWorkspace(RunId),
    /// The workspace is ready and the first stage is on its way.
    Running(RunId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionReport {
    pub details: RunDetails,
    pub committed_events: Vec<CommittedEvent>,
    pub outcome: QuiescentState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuiescentState {
    Completed,
    NeedsUser,
    Paused,
    Interrupted,
    Failed,
    Applied,
    Discarded,
    WaitingForProvider { stage_id: StageId },
    Active { run_status: RunStatus },
}

/// What the provider hears when the operator skips a permission request:
/// the call stays denied and the task continues on what is already allowed.
pub const SKIP_ATTENTION_RESPONSE: &str = "The operator declined this permission request. Do not retry it or work around it. Continue the task without it, and state plainly in your result what you could not do or verify because of it.";

/// How many permission requests one settling may approve on the operator's
/// behalf before handing the run back.
///
/// Not every approved call is a call the provider will then allow: some are
/// refused again whatever rule is granted, and the stage asks once more. A
/// ceiling turns that into a run that stops and shows the operator the
/// request, instead of a loop approving forever while nothing progresses.
const MAX_AUTO_APPROVALS: usize = 12;

pub struct RunService<F> {
    database: PathBuf,
    worktrees: PathBuf,
    provider_factory: F,
    abandoned_after: std::time::Duration,
    gh: GhClient,
}

/// An application can join Run creation to its own durable binding before
/// workspace preparation or provider work begins.
type RunCreation<'a> = dyn Fn(
        &mut SqliteStore,
        &Run,
        &RunInput,
        &ResolvedConfigSnapshot,
        &[DomainEvent],
    ) -> Result<(), AppError>
    + 'a;

impl<F> RunService<F>
where
    F: ProviderResolver,
{
    #[must_use]
    pub fn new(database: PathBuf, worktrees: PathBuf, provider_factory: F) -> Self {
        Self {
            database,
            worktrees,
            provider_factory,
            abandoned_after: ABANDONED_AFTER,
            gh: GhClient::default(),
        }
    }

    /// Replaces the `gh` client used by the pull-request start precondition.
    ///
    /// The default reads `gh` from `PATH`. Only a caller that knows which
    /// executable should answer that probe has any business replacing it.
    #[must_use]
    pub fn probing_pull_requests_with(mut self, gh: GhClient) -> Self {
        self.gh = gh;
        self
    }

    /// Overrides how long a run must sit untouched before a read settles it.
    ///
    /// The default, [`ABANDONED_AFTER`], is what keeps a read off a run some
    /// other process is still driving. Only a caller that owns both sides of
    /// that race has any business shortening it.
    #[must_use]
    pub const fn abandoning_after(mut self, idle: std::time::Duration) -> Self {
        self.abandoned_after = idle;
        self
    }

    /// Uses The Senate data paths from environment.
    ///
    /// # Errors
    /// Returns path resolution errors.
    pub fn from_environment(provider_factory: F) -> Result<Self, AppError> {
        Ok(Self::new(
            database_file()?,
            worktree_root()?,
            provider_factory,
        ))
    }

    /// Creates, prepares, and executes one built-in workflow to quiescence.
    ///
    /// # Errors
    /// Returns validation, repository, persistence, workspace, or engine errors.
    pub fn start_run(
        &self,
        workflow_kind: WorkflowKind,
        task: impl Into<String>,
        repository_path: impl AsRef<Path>,
        selection: Option<ExecutionSelection>,
        effort: impl Into<EffortRequest>,
        image: &ImageGenerationPlan,
    ) -> Result<ExecutionReport, AppError>
    where
        F: ProviderFactory,
    {
        self.start_run_observed(
            workflow_kind,
            task,
            repository_path,
            selection,
            effort,
            image,
            &|_| {},
        )
    }

    /// [`Self::start_run`], telling `observe` what the start is doing while
    /// it does it.
    ///
    /// The checks before a run exists can take as long as a network timeout,
    /// and until the run is persisted nothing else can show that anything is
    /// happening at all.
    ///
    /// # Errors
    /// Returns validation, repository, persistence, workspace, or engine errors.
    #[allow(clippy::too_many_arguments)]
    pub fn start_run_observed(
        &self,
        workflow_kind: WorkflowKind,
        task: impl Into<String>,
        repository_path: impl AsRef<Path>,
        selection: Option<ExecutionSelection>,
        effort: impl Into<EffortRequest>,
        image: &ImageGenerationPlan,
        observe: &dyn Fn(StartProgress),
    ) -> Result<ExecutionReport, AppError>
    where
        F: ProviderFactory,
    {
        self.start_run_bound_observed(
            workflow_kind,
            task.into(),
            repository_path.as_ref(),
            selection,
            effort.into(),
            image,
            observe,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_run_bound_observed(
        &self,
        workflow_kind: WorkflowKind,
        task: String,
        repository_path: &Path,
        selection: Option<ExecutionSelection>,
        effort: EffortRequest,
        image: &ImageGenerationPlan,
        observe: &dyn Fn(StartProgress),
        persist: Option<&RunCreation<'_>>,
    ) -> Result<ExecutionReport, AppError>
    where
        F: ProviderFactory,
    {
        let created_at = now();
        let workflow = WorkflowDefinition::built_in(workflow_kind);
        let run_id = RunId::new();
        let config_id = ConfigSnapshotId::new(format!("config-{run_id}"))?;
        let selection = selection.ok_or(AppError::NoProductionProvider)?;
        let config = self.provider_factory.config_for_new_run_with_image(
            selection,
            effort,
            image,
            &workflow,
            config_id.clone(),
            created_at,
        )?;
        self.start_run_with_config_at(
            workflow,
            task,
            repository_path,
            &config,
            run_id,
            created_at,
            observe,
            persist,
        )
    }

    pub(crate) fn start_run_with_config(
        &self,
        workflow: WorkflowDefinition,
        task: impl Into<String>,
        repository_path: impl AsRef<Path>,
        config: &ResolvedConfigSnapshot,
    ) -> Result<ExecutionReport, AppError> {
        self.start_run_with_config_at(
            workflow,
            task.into(),
            repository_path.as_ref(),
            config,
            RunId::new(),
            now(),
            &|_| {},
            None,
        )
    }

    /// Every distinct source-checkout path a run has ever been started from,
    /// most recently used first — or nothing at all when no run has ever
    /// started, since [`SqliteStore::open`] creates the database on first
    /// touch and a read performed only to look for a pull request's checkout
    /// must not itself become the reason a fresh install grows one. Mirrors
    /// [`Self::list_runs`]'s own guard for the same reason.
    fn previously_used_source_repositories(&self) -> Result<Vec<PathBuf>, AppError> {
        if !self.database.exists() {
            return Ok(Vec::new());
        }
        Ok(SqliteStore::open(&self.database)?.distinct_source_repository_paths()?)
    }

    #[allow(clippy::too_many_arguments)]
    fn start_run_with_config_at(
        &self,
        workflow: WorkflowDefinition,
        task: String,
        repository_path: &Path,
        config: &ResolvedConfigSnapshot,
        run_id: RunId,
        created_at: DateTime<Utc>,
        observe: &dyn Fn(StartProgress),
        persist: Option<&RunCreation<'_>>,
    ) -> Result<ExecutionReport, AppError> {
        let git = crate::git::Git::default();
        let mut repository = GitRepository::discover(repository_path)?;
        // A task naming a pull request is a task whose evidence — and, for a
        // workflow that edits, whose destination — belongs to a specific
        // repository. The chosen checkout need not be it: run 01M2JTBB08W8VXP3RM1V7Z9YQ2
        // was started in a Calypso checkout on a Jetpack pull request, and the
        // Fix stage that later ran there changed nothing because the worktree
        // held the wrong repository's history from the start. When the
        // chosen checkout's own `origin` does not name the pull request's
        // repository, look for a local checkout that does and start there
        // instead — before anything about the run becomes durable, in the
        // same window as the dirty-source and reachability checks below, so a
        // refusal here leaves nothing behind either.
        let pull_request = PullRequestRef::parse(&task);
        if let Some(pull_request) = pull_request.as_ref()
            && !remote_names_pull_request(&git, &repository, pull_request)?
        {
            observe(StartProgress::FindingCheckout {
                repository: format!("{}/{}", pull_request.owner, pull_request.repository),
            });
            let previously_used = self.previously_used_source_repositories()?;
            repository = resolve_pull_request_checkout(
                previously_used,
                &git,
                repository,
                pull_request,
                &workflow,
            )?;
        }
        // A managed worktree is created from the source repository's committed
        // HEAD, so uncommitted work would be invisible to the agent while
        // remaining visible to the user. Apply already refuses a dirty source;
        // refusing at the start means the disagreement never happens, and it
        // runs before any run ID, config, input, event, or workspace intent is
        // persisted, so a rejected start leaves nothing behind. Applies to the
        // repository the run actually starts from, which the search above may
        // have switched away from what the operator chose.
        if !crate::git::source_is_clean(&git, &repository)? {
            return Err(AppError::DirtySourceRepository);
        }
        // A task naming a pull request is a task whose evidence lives outside
        // the workspace. The pull request need not belong to this repository —
        // reviewing a remote one is a legitimate run — but if nothing can read
        // it, every stage produces the same "I was blocked" artifact and the
        // run reaches its end looking successful. Refuse here, in the same
        // window as the dirty-source check, so nothing is persisted.
        if let Some(pull_request) = pull_request {
            observe(StartProgress::CheckingPullRequest {
                url: pull_request.url(),
                host: pull_request.host.clone(),
            });
            if let PullRequestReach::Unreachable(detail) = self.gh.pull_request_reach(&pull_request)
            {
                return Err(AppError::PullRequestUnreachable {
                    url: pull_request.url(),
                    host: pull_request.host,
                    detail,
                });
            }
        }
        let input = RunInput::new(run_id, task, created_at)?;
        let config_id = config.id().clone();
        let run = Run::new(run_id, workflow, config_id, created_at);
        let created = run.created_event(EventMetadata::new(EventId::new(), created_at));
        let mut store = SqliteStore::open(&self.database)?;
        if let Some(persist) = persist {
            persist(&mut store, &run, &input, config, &[created])?;
        } else {
            store.create_run_with_input(&run, &input, config, &[created])?;
        }
        observe(StartProgress::PreparingWorkspace(run_id));

        let manager = WorkspaceManager::new(&self.worktrees);
        manager.prepare_run_workspace_with(
            &mut store,
            run_id,
            repository.source_path(),
            Some(&self.gh),
        )?;
        observe(StartProgress::Running(run_id));
        let status = self.drive(&mut store, run_id, ResumeAction::Continue)?;
        self.settle(&mut store, run_id, 0, status.as_ref())
    }

    /// Continues one persisted run according to safe lifecycle policy.
    ///
    /// # Errors
    /// Returns legacy-input/config, reconciliation, guard, or execution errors.
    pub fn resume_run(&self, run_id: RunId) -> Result<ExecutionReport, AppError> {
        // Resuming a run that something else is already driving is the common
        // case, not an exceptional one: the driver commits on every provider
        // signal it observes, so a resume issued while output is arriving loses
        // a revision race on rows the driver touched first. Every step here is
        // idempotent, so the answer is to redo the attempt against newer rows —
        // never to tell the user their run could not be resumed because of a
        // revision number.
        retry_concurrent(|| self.resume_run_once(run_id), std::thread::sleep)
    }

    fn resume_run_once(&self, run_id: RunId) -> Result<ExecutionReport, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let before = last_sequence(&store, run_id)?;
        // Mission Run creation and binding are atomic. A crash immediately
        // afterwards can leave a Created Run without a workspace intent;
        // the binding identifies its source so preparation can be recovered.
        if store.load_run(run_id)?.run.status() == RunStatus::Created
            && store.load_workspace(run_id)?.is_none()
        {
            let mission_id = store
                .mission_of_run(run_id)?
                .map(|binding| binding.mission_id)
                .or(store.mission_of_lead_run(run_id)?);
            if let Some(mission_id) = mission_id {
                let mission = store.load_mission(mission_id)?;
                let repository = GitRepository::discover(mission.input.source_repo_path())?;
                if !crate::git::source_is_clean(&crate::git::Git::default(), &repository)? {
                    return Err(AppError::DirtySourceRepository);
                }
                WorkspaceManager::new(&self.worktrees).prepare_run_workspace_with(
                    &mut store,
                    run_id,
                    repository.source_path(),
                    Some(&self.gh),
                )?;
            }
        }
        self.reconcile(&mut store, run_id)?;
        let status = self.drive(&mut store, run_id, ResumeAction::Resume)?;
        self.settle(&mut store, run_id, before, status.as_ref())
    }

    /// Resolves one attention request, then executes to quiescence.
    ///
    /// # Errors
    /// Returns identity, guard, lifecycle, persistence, or provider errors.
    pub fn resolve_attention(
        &self,
        run_id: RunId,
        request_id: AttentionRequestId,
    ) -> Result<ExecutionReport, AppError> {
        self.resolve_attention_with_response(run_id, request_id, None)
    }

    /// Resolves a permission request by declining it and telling the provider
    /// to carry on without it. One keypress in the TUI, `--skip` on the CLI.
    ///
    /// # Errors
    /// Returns identity, guard, lifecycle, persistence, or provider errors.
    pub fn skip_attention(
        &self,
        run_id: RunId,
        request_id: AttentionRequestId,
    ) -> Result<ExecutionReport, AppError> {
        self.resolve_attention_with_response(run_id, request_id, Some(SKIP_ATTENTION_RESPONSE))
    }

    /// Resolves one attention request with optional provider-native response text.
    ///
    /// # Errors
    /// Returns identity, guard, lifecycle, persistence, or provider errors.
    pub fn resolve_attention_with_response(
        &self,
        run_id: RunId,
        request_id: AttentionRequestId,
        response: Option<&str>,
    ) -> Result<ExecutionReport, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let before = last_sequence(&store, run_id)?;
        self.reconcile(&mut store, run_id)?;
        let mut engine = self.engine(&mut store, run_id)?;
        engine.resolve_attention_with_response(&mut store, run_id, request_id, response)?;
        let status = drive_attached(&mut engine, &mut store, run_id)?;
        self.settle(&mut store, run_id, before, Some(&status))
    }

    /// Resolves attention only when provider policy explicitly proves safe
    /// disposable-eval continuation. Returns `None` for human-required input.
    pub(crate) fn auto_resolve_attention(
        &self,
        run_id: RunId,
        request_id: AttentionRequestId,
    ) -> Result<Option<ExecutionReport>, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let before = last_sequence(&store, run_id)?;
        self.reconcile(&mut store, run_id)?;
        let mut engine = self.engine(&mut store, run_id)?;
        if !engine.can_auto_resolve_attention(&mut store, run_id, request_id)? {
            return Ok(None);
        }
        engine.resolve_attention(&mut store, run_id, request_id)?;
        let status = drive_attached(&mut engine, &mut store, run_id)?;
        Ok(Some(Self::report(
            &mut store,
            run_id,
            before,
            Some(&status),
        )?))
    }

    /// Retries one failed stage, then executes to quiescence.
    ///
    /// # Errors
    /// Returns identity, retry-safety, guard, persistence, or provider errors.
    ///
    /// With `route`, the stage is sent to that provider first. The provider is
    /// checked for readiness before anything is committed, so a retry aimed at
    /// a CLI that is not installed or logged in fails here with the stage
    /// untouched, not halfway through with the stage rerouted and pending.
    pub fn retry_stage(
        &self,
        run_id: RunId,
        stage_id: &StageId,
        route: Option<RetryRoute>,
    ) -> Result<ExecutionReport, AppError> {
        if let Some(route) = route.as_ref() {
            self.provider_factory.require_provider(route.provider())?;
        }
        let mut store = SqliteStore::open(&self.database)?;
        let before = last_sequence(&store, run_id)?;
        self.reconcile(&mut store, run_id)?;
        let mut engine = self.engine(&mut store, run_id)?;
        engine.retry_stage(
            &mut store,
            run_id,
            stage_id,
            route.map(|route| (route.into_override(), OPERATOR_OVERRIDE_REASON)),
        )?;
        let status = drive_attached(&mut engine, &mut store, run_id)?;
        self.settle(&mut store, run_id, before, Some(&status))
    }

    /// Sends a completed run back for one remediation cycle, then executes to
    /// quiescence.
    ///
    /// The run grows a fix stage and a fresh decision over it, keeping its
    /// workspace, artifacts and identity. The Senate never reads the previous
    /// verdict to decide whether this is warranted: the decision artifact is
    /// prose written for a person, and the operator asking is the whole
    /// signal. The fix stage reads the verdict it is answering as a dependency
    /// artifact, together with the run's immutable task.
    ///
    /// # Errors
    /// Returns identity, lifecycle, guard, persistence, or provider errors.
    /// A run that is not completed, whose workspace is gone, or whose apply is
    /// under way is refused without being modified.
    pub fn request_fix(&self, run_id: RunId) -> Result<ExecutionReport, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let before = last_sequence(&store, run_id)?;
        self.reconcile(&mut store, run_id)?;
        // Ask before committing to anything. Requesting a fix appends stages
        // and gives the workspace a branch, and a configuration sealed before
        // fix-cycle routing existed cannot execute those stages — nor can the
        // run be read afterwards, because reading it resolves its routes.
        let loaded = store.load_run(run_id)?;
        if loaded.run.status() != RunStatus::Completed {
            return Err(crate::engine::EngineError::Fix(
                crate::domain::RunFixError::RunNotCompleted(loaded.run.status()),
            )
            .into());
        }
        if let Some(role) =
            crate::app::unroutable_fix_role(&loaded.config_snapshot, loaded.run.workflow())?
        {
            return Err(AppError::UnroutableFixCycle { run_id, role });
        }
        // Provider/engine construction can itself refuse (configured provider
        // gone, resolution failure), so it happens before the workspace gains
        // a branch too — everything that can refuse this request must refuse
        // before anything about it becomes durable.
        let mut engine = self.engine(&mut store, run_id)?;
        // A review's workspace is detached, because a review is not meant to
        // produce changes. This is the request that changes that, so the
        // workspace earns its branch here — now that run status, routability,
        // and provider construction have all already had their chance to
        // refuse — rather than leaving the fix to write into a tree apply
        // would later refuse to transfer.
        WorkspaceManager::new(&self.worktrees).adopt_branch_for_fix(&mut store, run_id)?;
        engine.request_fix(&mut store, run_id)?;
        let status = drive_attached(&mut engine, &mut store, run_id)?;
        self.settle(&mut store, run_id, before, Some(&status))
    }

    /// Sends a completed run back with the operator's own next instruction,
    /// then executes to quiescence.
    ///
    /// The sibling of [`Self::request_fix`]: the run grows a follow-up stage
    /// and a fresh decision over it, keeping its workspace, artifacts and
    /// identity, and shares that method's guards exactly — the roles a
    /// continue cycle adds are the same roles a fix cycle adds, so the same
    /// routability check answers for both. It differs only in what the new
    /// implementer stage answers to: an operator instruction instead of the
    /// verdict's own blocking findings. That instruction is immutable
    /// run-private content, never a domain event field; the engine stages it
    /// with the provider before the stage that will carry it is created, the
    /// same ordering an attention response's text follows relative to the
    /// resolution it answers.
    ///
    /// # Errors
    /// Returns identity, lifecycle, guard, persistence, or provider errors.
    /// A blank instruction, a run that is not completed, whose workspace is
    /// gone, or whose apply is under way is refused without being modified.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "owned operator input at the public service boundary, mirroring how callers already own the text they read from a TUI field or CLI argument"
    )]
    pub fn request_continue(
        &self,
        run_id: RunId,
        instruction: String,
    ) -> Result<ExecutionReport, AppError> {
        let instruction = instruction.trim().to_owned();
        if instruction.is_empty() {
            return Err(AppError::EmptyContinueInstruction(run_id));
        }
        let mut store = SqliteStore::open(&self.database)?;
        let before = last_sequence(&store, run_id)?;
        self.reconcile(&mut store, run_id)?;
        let loaded = store.load_run(run_id)?;
        if loaded.run.status() != RunStatus::Completed {
            return Err(crate::engine::EngineError::Fix(
                crate::domain::RunFixError::RunNotCompleted(loaded.run.status()),
            )
            .into());
        }
        if let Some(role) =
            crate::app::unroutable_fix_role(&loaded.config_snapshot, loaded.run.workflow())?
        {
            return Err(AppError::UnroutableContinueCycle { run_id, role });
        }
        // See request_fix: provider/engine construction can itself refuse, so
        // it happens before the workspace gains a branch too.
        let mut engine = self.engine(&mut store, run_id)?;
        // See request_fix: this is the request that starts producing changes
        // over a review's detached workspace, so it earns its branch here —
        // now that run status, routability, and provider construction have
        // all already had their chance to refuse.
        WorkspaceManager::new(&self.worktrees).adopt_branch_for_fix(&mut store, run_id)?;
        engine.request_continue(&mut store, run_id, &instruction)?;
        let status = drive_attached(&mut engine, &mut store, run_id)?;
        self.settle(&mut store, run_id, before, Some(&status))
    }

    /// One more exchange with a mission's lead session: appends a lead
    /// stage carrying `message` and drives the run to quiescence.
    ///
    /// # Errors
    /// Returns an error when the run is not a lead session, is not
    /// completed, or when the engine, provider, or store refuse.
    pub fn request_lead_turn(
        &self,
        run_id: RunId,
        message: &str,
    ) -> Result<ExecutionReport, AppError> {
        let message = message.trim().to_owned();
        if message.is_empty() {
            return Err(AppError::EmptyContinueInstruction(run_id));
        }
        let mut store = SqliteStore::open(&self.database)?;
        let before = last_sequence(&store, run_id)?;
        self.reconcile(&mut store, run_id)?;
        let loaded = store.load_run(run_id)?;
        if loaded.run.workflow().kind() != WorkflowKind::Lead {
            return Err(crate::engine::EngineError::Fix(
                crate::domain::RunFixError::NotALeadSession,
            )
            .into());
        }
        if loaded.run.status() != RunStatus::Completed {
            return Err(crate::engine::EngineError::Fix(
                crate::domain::RunFixError::RunNotCompleted(loaded.run.status()),
            )
            .into());
        }
        let mut engine = self.engine(&mut store, run_id)?;
        engine.request_lead_turn(&mut store, run_id, &message)?;
        let status = drive_attached(&mut engine, &mut store, run_id)?;
        self.settle(&mut store, run_id, before, Some(&status))
    }

    /// Applies completed workspace changes to source checkout.
    ///
    /// # Errors
    /// Returns workspace ownership, lifecycle, patch, Git, or store errors.
    pub fn apply_run(&self, run_id: RunId) -> Result<(ApplyOutcome, ExecutionReport), AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let before = last_sequence(&store, run_id)?;
        let manager = WorkspaceManager::new(&self.worktrees);
        let outcome = match manager.apply(&mut store, run_id) {
            Ok(()) => ApplyOutcome::Applied,
            Err(WorkspaceError::EmptyPatch) => ApplyOutcome::NoChanges,
            Err(error) => return Err(error.into()),
        };
        // The changes are in the source checkout now, and an applied run can
        // neither be applied again nor fixed, so its worktree has stopped being
        // work in progress and become a copy of a repository nobody will open.
        // For a large checkout that is gigabytes per run, kept forever.
        self.reclaim_workspace(&mut store, run_id);
        let report = Self::report(&mut store, run_id, before, None)?;
        Ok((outcome, report))
    }

    /// Moves a completed run's changes onto the source checkout's current
    /// `HEAD`, so a checkout that moved on can still take them.
    ///
    /// Unlike apply, this leaves the workspace in place and the run
    /// `Completed`: the point is to make a later apply possible, not to be
    /// one. That later apply still has to wait for a verification that ran
    /// after the move.
    ///
    /// # Errors
    /// Returns workspace ownership, lifecycle, conflict, Git, or store errors.
    pub fn rebase_run(&self, run_id: RunId) -> Result<(RebaseReceipt, ExecutionReport), AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let before = last_sequence(&store, run_id)?;
        let receipt = WorkspaceManager::new(&self.worktrees).rebase(&mut store, run_id)?;
        let report = Self::report(&mut store, run_id, before, None)?;
        Ok((receipt, report))
    }

    /// Returns the worktree of a run that is finished with it, keeping the
    /// branch so nothing the run produced is lost.
    ///
    /// Best effort by construction. Reclaiming disk is never worth failing an
    /// operation that has already succeeded, so a workspace that refuses to go
    /// — a dirty tree, a source repository that has moved, a concurrent apply —
    /// is left exactly as it was for the next sweep to try again.
    fn reclaim_workspace(&self, store: &mut SqliteStore, run_id: RunId) -> bool {
        WorkspaceManager::new(&self.worktrees)
            .release_worktree(store, run_id)
            .is_ok()
    }

    /// Returns the worktrees of every run that has closed its lifecycle and
    /// still holds one, and reports which runs were reclaimed.
    ///
    /// Reclaiming at the moment a run closes only helps runs that close from
    /// now on. This is the sweep for the rest: runs applied or discarded
    /// before reclamation existed, and runs whose reclamation was interrupted
    /// part-way. Only `Applied` and `Discarded` qualify — a `Completed` run
    /// can still be applied, fixed, or continued, and all three read the
    /// worktree, so its checkout is live data rather than a leak.
    ///
    /// # Errors
    /// Returns store failures. A single workspace that refuses to go is not an
    /// error: it is skipped and the sweep continues.
    pub fn reclaim_closed_workspaces(&self) -> Result<Vec<RunId>, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let closed: Vec<RunId> = store
            .list_runs()?
            .into_iter()
            .filter(|run| run.status.is_lifecycle_closed())
            .map(|run| run.id)
            .collect();
        let mut reclaimed = Vec::new();
        for run_id in closed {
            let holds_worktree = store
                .load_workspace(run_id)
                .ok()
                .flatten()
                .is_some_and(|workspace| workspace.status() != WorkspaceStatus::Removed);
            if holds_worktree && self.reclaim_workspace(&mut store, run_id) {
                reclaimed.push(run_id);
            }
        }
        Ok(reclaimed)
    }

    /// Publishes completed workspace changes as a remote branch and pull
    /// request, leaving the source checkout untouched.
    ///
    /// The parallel-friendly alternative to `apply_run`: nothing serializes on
    /// the operator's checkout, so any number of completed runs can publish
    /// concurrently. The run stays `Completed` — apply, fix, and discard all
    /// remain available afterwards. Unlike apply, a failed verification does
    /// not refuse: the remote is where a change gets graded.
    ///
    /// # Errors
    /// Returns workspace ownership, lifecycle, Git, remote, or store errors.
    pub fn publish_run(
        &self,
        run_id: RunId,
    ) -> Result<(crate::workspace::PublishReceipt, ExecutionReport), AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let before = last_sequence(&store, run_id)?;
        // The pull request is quoted from the latest editing stage's own
        // artifact when it wrote one; the task text stands in otherwise.
        let draft = query::pull_request_draft(&store, run_id)?;
        let receipt =
            WorkspaceManager::new(&self.worktrees).publish(&mut store, run_id, draft.as_ref())?;
        let report = Self::report(&mut store, run_id, before, None)?;
        Ok((receipt, report))
    }

    /// Stops execution while preserving everything the run produced.
    ///
    /// Stop is not disposition: the workspace, its changes, artifacts, logs,
    /// and provider session identity all survive, and `resume_run` recovers
    /// through the existing `Interrupted -> Recover` path rather than starting
    /// a new logical run or a new stage attempt.
    ///
    /// Ordering matters for crash safety. Managed processes are interrupted
    /// first — `ProcessManager::interrupt` persists its intent before
    /// signalling — and the run-level interruption is committed afterwards. A
    /// crash in between leaves interrupted processes under a still-running
    /// run, which the existing reconciliation and `ProviderSignal::Interrupted`
    /// path already resolves. The reverse order could leave a run marked
    /// stopped while its provider kept working.
    ///
    /// # Errors
    /// Returns reconciliation, process, lifecycle, or store errors. Runs that
    /// have reached a terminal disposition cannot be stopped.
    pub fn stop_run(&self, run_id: RunId) -> Result<ExecutionReport, AppError> {
        // A run is normally stopped while another The Senate process is still
        // driving it, and that driver keeps reconciling the same managed
        // process rows. Losing an optimistic-concurrency race here used to
        // abort the stop after the provider had already been signalled,
        // reporting failure while leaving the run marked Running with an
        // interrupted stage. Retry the whole stop instead: every step is
        // idempotent, so a later attempt observes the newer revisions and
        // commits the run-level interruption the user asked for.
        retry_stop(run_id, || self.stop_run_once(run_id), std::thread::sleep)
    }

    fn stop_run_once(&self, run_id: RunId) -> Result<ExecutionReport, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let before = last_sequence(&store, run_id)?;
        self.reconcile(&mut store, run_id)?;
        let loaded = store.load_run(run_id)?;
        match loaded.run.status() {
            // Already stopped: report the existing state rather than
            // committing a second interruption.
            RunStatus::Interrupted => return Self::report(&mut store, run_id, before, None),
            RunStatus::Running | RunStatus::NeedsUser => {}
            status => return Err(AppError::RunNotStoppable(run_id, status)),
        }
        let process_manager = ProcessManager::from_environment()?;
        for process in store.list_managed_processes(run_id)? {
            let inspection = process_manager.inspect(&mut store, process.id())?;
            if inspection.process.status().is_active() {
                // Interrupt only. Cleanup would remove the durable process
                // record this run needs in order to recover.
                process_manager.interrupt(&mut store, process.id())?;
            }
        }
        // Signalling the provider is not the same as recording that the stage
        // stopped. Let the engine observe the now-interrupted processes and
        // commit the stage-level interruption before the run-level one.
        // Without it the stage still reads Running under an Interrupted run,
        // and the first resume recovers only the run, leaving the user to
        // issue resume a second time.
        //
        // Observe, never Continue: the adapters decide on their own whether to
        // resume, from the persisted session status against the stage status.
        // A run carrying a stale NeedsUser session with a still-Running stage
        // would otherwise have its provider resumed by the very command asking
        // to halt it — and a pending permission that cannot be replayed safely
        // would surface as a stop failure.
        self.drive(&mut store, run_id, ResumeAction::Observe)?;
        let loaded = store.load_run(run_id)?;
        match loaded.run.status() {
            RunStatus::Running | RunStatus::NeedsUser => {}
            // Observing can settle the run on its own. A process that had
            // already exited resolves a stale Running into the run's true
            // resting state — interrupted, or failed for a process that died
            // without a result. The run is stopped either way, so report what
            // it actually is instead of forcing an Interrupt the domain would
            // rightly reject.
            _ => return Self::report(&mut store, run_id, before, None),
        }
        let mut run = loaded.run;
        let at = now();
        let event = run.transition(
            RunTransition::Interrupt,
            EventMetadata::new(EventId::new(), at),
        )?;
        store.commit_run_update(&run, loaded.revision, &[event])?;
        Self::report(&mut store, run_id, before, None)
    }

    /// Discards one run and cleans its owned workspace resources.
    ///
    /// # Errors
    /// Returns workspace ownership, lifecycle, Git, or store errors.
    pub fn discard_run(&self, run_id: RunId) -> Result<ExecutionReport, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let before = last_sequence(&store, run_id)?;
        self.release_owned_resources(&mut store, run_id)?;
        Self::report(&mut store, run_id, before, None)
    }

    /// Interrupts and cleans up the run's managed processes, then discards
    /// its workspace: everything the run owns outside its own rows and
    /// files. Shared by discard and by the purge that follows it.
    fn release_owned_resources(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<(), AppError> {
        let process_manager = ProcessManager::from_environment()?;
        for process in store.list_managed_processes(run_id)? {
            let inspection = process_manager.inspect(store, process.id())?;
            let inspection = if inspection.process.status().is_active() {
                process_manager.interrupt(store, process.id())?
            } else {
                inspection
            };
            if matches!(
                inspection.process.status(),
                ManagedProcessStatus::Exited
                    | ManagedProcessStatus::Interrupted
                    | ManagedProcessStatus::Missing
                    | ManagedProcessStatus::Broken
            ) {
                process_manager.cleanup(store, process.id())?;
            }
        }
        WorkspaceManager::new(&self.worktrees).discard(store, run_id)?;
        Ok(())
    }

    /// Sets whether a run is archived out of the default Runs list. Pure
    /// list metadata: no lifecycle transition, no revision bump, no process
    /// or workspace side effects.
    ///
    /// # Errors
    /// Returns store or path errors; unknown runs surface as `RunNotFound`.
    pub fn set_run_archived(&self, run_id: RunId, archived: bool) -> Result<(), AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        store.set_run_archived(run_id, archived)?;
        Ok(())
    }

    /// Arms or disarms automatic approval of this run's permission requests.
    ///
    /// Arming records an instruction for the future; it does not reach into a
    /// request that is already waiting. A run sitting on one keeps sitting on
    /// it until something drives the run again — resuming it is what makes
    /// the standing approval apply.
    ///
    /// # Errors
    /// Returns store or path errors; unknown runs surface as `RunNotFound`.
    pub fn set_run_auto_approve(&self, run_id: RunId, auto_approve: bool) -> Result<(), AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        store.set_run_auto_approve(run_id, auto_approve)?;
        Ok(())
    }

    /// Deletes an archived run for good: its worktree, its files, its rows.
    ///
    /// The one irreversible operation The Senate offers, so it is deliberately
    /// hard to reach by accident — only an already archived run qualifies,
    /// and only one that is no longer running. Everything the run owns goes
    /// in the order that survives an interruption: processes and worktree
    /// first (each already idempotent and resumable), then the run's own
    /// directory of artifacts and process logs, and only then the rows that
    /// said where all of it lived. A failure part-way leaves a run that is
    /// still listed and can be deleted again.
    ///
    /// # Errors
    /// Returns [`AppError::RunNotArchived`] for a run not set aside first,
    /// [`AppError::RunNotDeletable`] while it is still running, and
    /// workspace, process, filesystem, or store errors.
    pub fn purge_run(&self, run_id: RunId) -> Result<PurgeReceipt, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let summary = store
            .list_runs()?
            .into_iter()
            .find(|run| run.id == run_id)
            .ok_or(StoreError::RunNotFound(run_id))?;
        if !summary.archived {
            return Err(AppError::RunNotArchived(run_id));
        }
        if matches!(summary.status, RunStatus::Running | RunStatus::NeedsUser) {
            return Err(AppError::RunNotDeletable(run_id, summary.status));
        }
        // Checked before anything is released: the store refuses the purge
        // too, but by then the run would already be discarded and its files
        // gone, which is exactly the loss a bound run is protected from.
        if let Some(binding) = store.mission_of_run(run_id)? {
            return Err(StoreError::RunBoundToMission {
                run_id,
                mission_id: binding.mission_id,
            }
            .into());
        }
        if let Some(mission_id) = store.mission_of_lead_run(run_id)? {
            return Err(StoreError::RunBoundToMission { run_id, mission_id }.into());
        }
        self.release_owned_resources(&mut store, run_id)?;
        let run_directory = process_root()?.join(run_id.to_string());
        let files_removed = match std::fs::remove_dir_all(&run_directory) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(AppError::RunFilesNotRemoved(run_directory, error)),
        };
        store.purge_run(run_id)?;
        Ok(PurgeReceipt {
            run_id,
            files_removed,
        })
    }

    /// Returns indexed summaries without creating a missing database.
    ///
    /// # Errors
    /// Returns query, migration, or path errors.
    pub fn list_runs(&self) -> Result<Vec<RunListItem>, AppError> {
        if !self.database.exists() {
            return Ok(Vec::new());
        }
        let mut store = SqliteStore::open(&self.database)?;
        let listed = query::list(&store)?;
        let running = listed
            .iter()
            .filter(|item| item.status == RunStatus::Running)
            .map(|item| item.id)
            .collect::<Vec<_>>();
        let mut settled = false;
        for run_id in running {
            settled |= self.settle_if_abandoned(&mut store, run_id);
        }
        if settled {
            return query::list(&store);
        }
        Ok(listed)
    }

    /// Returns detailed persisted state, settling the run first if it was
    /// abandoned.
    ///
    /// # Errors
    /// Returns not-found, corrupt state, or query errors.
    pub fn inspect_run(&self, run_id: RunId) -> Result<RunDetails, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        self.settle_if_abandoned(&mut store, run_id);
        query::inspect(&mut store, run_id)
    }

    /// Lists integrity-verified artifact metadata for one run.
    ///
    /// # Errors
    /// Returns persistence or artifact integrity failures.
    pub fn list_artifacts(&self, run_id: RunId) -> Result<Vec<ArtifactSummary>, AppError> {
        query::list_artifacts(&SqliteStore::open(&self.database)?, run_id)
    }

    /// Reads latest integrity-verified artifact for one stage.
    ///
    /// # Errors
    /// Returns not-found, non-UTF-8, persistence, or integrity failures.
    pub fn read_artifact(
        &self,
        run_id: RunId,
        stage_id: &StageId,
    ) -> Result<ArtifactView, AppError> {
        query::read_artifact(&SqliteStore::open(&self.database)?, run_id, stage_id)
    }

    /// Generates bounded read-only workspace diff from same delta semantics as apply.
    ///
    /// # Errors
    /// Returns workspace ownership/readiness or Git failures.
    pub fn preview_run_diff(&self, run_id: RunId) -> Result<RunDiffPreview, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let preview = WorkspaceManager::new(&self.worktrees).preview_patch(
            &mut store,
            run_id,
            DIFF_PREVIEW_LIMIT,
        )?;
        Ok(query::summarize_diff(
            String::from_utf8_lossy(&preview.bytes).into_owned(),
            preview.total_bytes,
            preview.truncated,
        ))
    }

    /// Reads bounded stdout/stderr tails without acknowledging provider output.
    ///
    /// # Errors
    /// Returns process lookup, path, or retained-output failures.
    pub fn read_process_log_tail(
        &self,
        run_id: RunId,
        stage_id: &StageId,
    ) -> Result<ProcessLogView, AppError> {
        let store = SqliteStore::open(&self.database)?;
        let manager = ProcessManager::from_environment()?;
        query::process_log_tail(&store, &manager, run_id, stage_id, PROCESS_LOG_TAIL_LIMIT)
    }

    /// Returns provider-neutral evidence for exactly one stage.
    ///
    /// Usage excludes every other routed stage and reading does not mutate cursors or run state.
    ///
    /// # Errors
    /// Returns missing/corrupt run, stage, routing, event, or provider-session data.
    pub fn stage_execution_evidence(
        &self,
        run_id: RunId,
        stage_id: &StageId,
    ) -> Result<StageExecutionEvidence, AppError> {
        query::stage_execution_evidence(&mut SqliteStore::open(&self.database)?, run_id, stage_id)
    }

    /// Commits what already happened to a run nothing is driving any more.
    ///
    /// Nothing observes a run unless a command drives the engine, so a
    /// provider process that died leaves its run reading Running for as long
    /// as the user only ever reads it. Reads are how a user notices a run at
    /// all, so they settle it: a run whose processes have all ended and that
    /// nothing has touched for [`ABANDONED_AFTER`] is observed exactly the way
    /// a stop observes it, and the truth is committed once.
    ///
    /// Three conditions keep this from becoming a write on a live run. The run
    /// must be Running, no managed process may still be active — an active
    /// process means a driver is attached, and racing its commits would fail
    /// the run this call only meant to display — and the run must have been
    /// idle long enough that no driver can be mid-handover between processes.
    ///
    /// Best effort by construction: reading a run the engine cannot observe is
    /// still reading a run the user is entitled to see, so every failure here
    /// leaves the persisted state exactly as it was and the caller reports it
    /// unchanged. Returns whether observation ran.
    fn settle_if_abandoned(&self, store: &mut SqliteStore, run_id: RunId) -> bool {
        let Ok(loaded) = store.load_run(run_id) else {
            return false;
        };
        if loaded.run.status() != RunStatus::Running {
            return false;
        }
        let Ok(idle) = now()
            .signed_duration_since(*loaded.run.updated_at())
            .to_std()
        else {
            // Updated in the future: a clock this read cannot reason about.
            return false;
        };
        if idle < self.abandoned_after {
            return false;
        }
        let Ok(processes) = store.list_managed_processes(run_id) else {
            return false;
        };
        if processes.iter().any(|process| process.status().is_active()) {
            return false;
        }
        if self.reconcile(store, run_id).is_err() {
            return false;
        }
        // Observe, never Continue or Resume: a read must never become the
        // reason a provider starts working again.
        self.drive(store, run_id, ResumeAction::Observe).is_ok()
    }

    fn reconcile(&self, store: &mut SqliteStore, run_id: RunId) -> Result<(), AppError> {
        let outcome = WorkspaceManager::new(&self.worktrees).reconcile(store, run_id)?;
        match outcome {
            ReconciliationOutcome::Ready(_)
            | ReconciliationOutcome::Unchanged(_)
            | ReconciliationOutcome::Removed(_)
            | ReconciliationOutcome::Broken(_) => Ok(()),
        }
    }

    fn engine(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<WorkflowEngine<F::Provider>, AppError> {
        let loaded = store.load_run(run_id)?;
        let input = store
            .load_run_input(run_id)?
            .ok_or(AppError::LegacyRunInput(run_id))?;
        let events = store.load_events(run_id)?;
        let provider = self.provider_factory.resolve_for_run(
            run_id,
            &loaded.config_snapshot,
            loaded.run.workflow(),
            &events,
        )?;
        Ok(WorkflowEngine::new(provider, input.task().to_owned()))
    }

    fn drive(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        action: ResumeAction,
    ) -> Result<Option<EngineStatus>, AppError> {
        let loaded = store.load_run(run_id)?;
        match loaded.run.status() {
            RunStatus::NeedsUser
            | RunStatus::Failed
            | RunStatus::Completed
            | RunStatus::Applied => {
                return Ok(None);
            }
            RunStatus::Discarded => return Err(AppError::DiscardedRun(run_id)),
            _ => {}
        }
        let mut engine = self.engine(store, run_id)?;
        if action == ResumeAction::Resume {
            match loaded.run.status() {
                RunStatus::Paused => {
                    engine.resume_run(store, run_id)?;
                }
                RunStatus::Interrupted => {
                    engine.recover_run(store, run_id)?;
                }
                _ => {}
            }
            // Recovering the run cascades stage *status* in the domain, but a
            // provider-backed stage also has to be resumed through the engine.
            // Reload so the cascade is visible, then bring every suspended
            // stage back in this same call — otherwise a stopped run reports
            // Running and needs a second resume to actually continue.
            let reloaded = store.load_run(run_id)?;
            resume_suspended_stages(&mut engine, store, &reloaded.run)?;
        }
        if action == ResumeAction::Observe {
            return Ok(Some(engine.drive_observing(store, run_id)?));
        }
        Ok(Some(drive_attached(&mut engine, store, run_id)?))
    }

    /// Reports where the run came to rest, approving on the way when the
    /// operator armed this run for it.
    ///
    /// Every driving entry point ends here, so arming is honoured wherever
    /// the run was driven from — the TUI, the CLI, a resume that happened to
    /// pass through a permission request — and never has to be remembered at
    /// each call site.
    ///
    /// Only permission requests are approved. A question needs an answer no
    /// approval can stand in for, and a request that cannot be replayed as an
    /// exact rule cannot be approved at all: both stop the run and wait for
    /// the operator, exactly as they would unarmed. An approval that fails
    /// leaves its request pending and ends the settling rather than
    /// propagating: the work already reported here really did happen, and the
    /// operator meets the untouched request on the next screen.
    fn settle(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        after_sequence: u64,
        engine_status: Option<&EngineStatus>,
    ) -> Result<ExecutionReport, AppError> {
        let mut report = Self::report(store, run_id, after_sequence, engine_status)?;
        if !store.run_auto_approve(run_id)? {
            return Ok(report);
        }
        for _ in 0..MAX_AUTO_APPROVALS {
            if report.outcome != QuiescentState::NeedsUser {
                break;
            }
            let Some(request) = report
                .details
                .attention
                .iter()
                .find(|attention| attention.kind == AttentionKind::Permission)
                .map(|attention| attention.id)
            else {
                break;
            };
            let approved = (|| -> Result<EngineStatus, AppError> {
                let mut engine = self.engine(store, run_id)?;
                engine.resolve_attention(store, run_id, request)?;
                drive_attached(&mut engine, store, run_id)
            })();
            let Ok(status) = approved else {
                break;
            };
            report = Self::report(store, run_id, after_sequence, Some(&status))?;
        }
        Ok(report)
    }

    fn report(
        store: &mut SqliteStore,
        run_id: RunId,
        after_sequence: u64,
        engine_status: Option<&EngineStatus>,
    ) -> Result<ExecutionReport, AppError> {
        let committed_events = store
            .load_events(run_id)?
            .into_iter()
            .filter(|event| event.sequence > after_sequence)
            .map(|event| CommittedEvent {
                sequence: event.sequence,
                stage_id: event.event.stage_id().cloned(),
                kind: event.event.kind().clone(),
            })
            .collect();
        let details = query::inspect(store, run_id)?;
        let outcome = quiescent_state(details.status, engine_status);
        Ok(ExecutionReport {
            details,
            committed_events,
            outcome,
        })
    }
}

fn drive_attached<P: crate::engine::Provider>(
    engine: &mut WorkflowEngine<P>,
    store: &mut SqliteStore,
    run_id: RunId,
) -> Result<EngineStatus, AppError> {
    loop {
        let status = engine.drive(store, run_id)?;
        if matches!(
            status,
            EngineStatus::WaitingForProvider {
                keep_attached: true,
                ..
            }
        ) {
            std::thread::sleep(std::time::Duration::from_millis(150));
            continue;
        }
        return Ok(status);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResumeAction {
    Continue,
    Resume,
    /// Record what already happened without ever starting or resuming
    /// provider work. The control-plane stop path, and nothing else.
    Observe,
}

fn resume_suspended_stages<P: crate::engine::Provider>(
    engine: &mut WorkflowEngine<P>,
    store: &mut SqliteStore,
    run: &Run,
) -> Result<(), AppError> {
    for stage in run.stages() {
        match stage.status() {
            StageStatus::Paused => {
                engine.resume_stage(store, run.id(), stage.id())?;
            }
            StageStatus::Interrupted => {
                engine.recover_stage(store, run.id(), stage.id())?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Runs one stop attempt repeatedly while it fails for a reason repeating can
/// resolve, with a separate budget per reason.
///
/// Two transient conditions reach a stop, and they are not the same shape. A
/// lost revision race means another driver committed first and the work must
/// simply be redone against newer rows. Missing runtime evidence means the
/// target process exists but cannot yet be named, and the wait is on a file the
/// runner is about to write. Counting them together would let a burst of one
/// consume the tolerance the other needs.
///
/// `sleep` is injected so the policy can be exercised without wall-clock time.
/// Exhausting either budget returns an error: a stop that gives up says so, and
/// never reports success over a process that is still running.
/// Repeats one idempotent attempt while it loses an optimistic-concurrency
/// race, on the same bound and backoff a stop uses.
///
/// `sleep` is injected so the policy can be exercised without wall-clock time.
fn retry_concurrent<T, F, S>(mut attempt: F, mut sleep: S) -> Result<T, AppError>
where
    F: FnMut() -> Result<T, AppError>,
    S: FnMut(std::time::Duration),
{
    let mut attempts = 0;
    loop {
        match attempt() {
            Err(error) if error.is_concurrent_modification() => {
                attempts += 1;
                if attempts >= STOP_CONCURRENCY_ATTEMPTS {
                    return Err(error);
                }
                sleep(STOP_CONCURRENCY_BACKOFF);
            }
            outcome => return outcome,
        }
    }
}

fn retry_stop<T, F, S>(run_id: RunId, mut attempt: F, mut sleep: S) -> Result<T, AppError>
where
    F: FnMut() -> Result<T, AppError>,
    S: FnMut(std::time::Duration),
{
    let started = std::time::Instant::now();
    let mut concurrency_attempts = 0;
    let mut evidence_attempts = 0;
    loop {
        match attempt() {
            Err(error) if error.is_concurrent_modification() => {
                concurrency_attempts += 1;
                if concurrency_attempts >= STOP_CONCURRENCY_ATTEMPTS {
                    return Err(error);
                }
                sleep(STOP_CONCURRENCY_BACKOFF);
            }
            Err(error) if error.is_pending_runtime_evidence() => {
                evidence_attempts += 1;
                if evidence_attempts >= STOP_EVIDENCE_ATTEMPTS {
                    let Some(process_id) = error.pending_runtime_evidence_process() else {
                        return Err(error);
                    };
                    return Err(AppError::StopRuntimeEvidenceUnavailable {
                        run_id,
                        process_id,
                        waited_ms: started.elapsed().as_millis(),
                    });
                }
                sleep(STOP_EVIDENCE_BACKOFF);
            }
            outcome => return outcome,
        }
    }
}

/// Whether `repository`'s `origin` remote names the same repository as
/// `pull_request`. A checkout with no `origin` configured at all does not
/// name it either.
fn remote_names_pull_request(
    git: &crate::git::Git,
    repository: &GitRepository,
    pull_request: &PullRequestRef,
) -> Result<bool, AppError> {
    let remote = crate::git::remote_url(git, repository.source_path(), "origin")?;
    Ok(remote
        .as_deref()
        .and_then(RemoteRepository::parse)
        .is_some_and(|remote| remote.names(pull_request)))
}

/// Chooses which local checkout a run whose task names a pull request should
/// actually start from, given that `chosen`'s own `origin` does not name that
/// pull request's repository.
///
/// Searches, in order and de-duplicated, `previously_used` — every distinct
/// source-checkout path a run has ever been started from, most recently used
/// first, see [`RunService::previously_used_source_repositories`] — then the
/// immediate subdirectories of `chosen`'s own parent directory. Neither tier
/// recurses or scans the rest of disk: a checkout The Senate has never heard of
/// and that is not sitting right next to the one the operator chose is not
/// one this can be expected to find.
///
/// A matching checkout that is dirty is not a usable answer, for the same
/// reason [`AppError::DirtySourceRepository`] refuses `chosen` itself when
/// nothing gets switched — so a dirty match is skipped in favour of a clean
/// one. A candidate Git cannot answer about is skipped too: it is somebody
/// else's folder, and its trouble must not stop this run from starting.
///
/// When no clean checkout matches, a workflow that edits is refused: it needs
/// to write into the pull request's own repository, which no checkout on
/// hand can provide — and if dirty matches exist, the refusal names them
/// rather than suggesting a clone that already exists. A read-only workflow
/// keeps today's behaviour and starts in `chosen` regardless — reviewing a
/// remote pull request over the network is a legitimate run.
fn resolve_pull_request_checkout(
    previously_used: Vec<PathBuf>,
    git: &crate::git::Git,
    chosen: GitRepository,
    pull_request: &PullRequestRef,
    workflow: &WorkflowDefinition,
) -> Result<GitRepository, AppError> {
    let mut dirty = Vec::new();
    for candidate in candidate_checkout_paths(previously_used, &chosen) {
        let Ok(repository) = GitRepository::discover_with(git, &candidate) else {
            // No longer exists, or is not a Git repository: not a checkout.
            continue;
        };
        if !remote_names_pull_request(git, &repository, pull_request).unwrap_or(false) {
            continue;
        }
        match crate::git::source_is_clean(git, &repository) {
            Ok(true) => return Ok(repository),
            Ok(false) => dirty.push(repository.source_path().to_path_buf()),
            Err(_) => {}
        }
    }
    if !workflow.requires_writable_workspace() {
        return Ok(chosen);
    }
    if !dirty.is_empty() {
        return Err(AppError::PullRequestCheckoutsDirty {
            url: pull_request.url(),
            checkouts: dirty,
        });
    }
    Err(AppError::NoLocalCheckoutForPullRequest {
        url: pull_request.url(),
        repository: format!(
            "{}/{}/{}",
            pull_request.host, pull_request.owner, pull_request.repository
        ),
        searched_parent: chosen
            .source_path()
            .parent()
            .map_or_else(|| chosen.source_path().to_path_buf(), Path::to_path_buf),
    })
}

/// Candidate checkout paths worth discovering as Git repositories while
/// looking for one that carries a pull request's repository, de-duplicated in
/// the order they are tried. See [`resolve_pull_request_checkout`].
fn candidate_checkout_paths(previously_used: Vec<PathBuf>, chosen: &GitRepository) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut candidates = Vec::new();
    for path in previously_used {
        if seen.insert(path.clone()) {
            candidates.push(path);
        }
    }
    if let Some(parent) = chosen.source_path().parent()
        && let Ok(entries) = std::fs::read_dir(parent)
    {
        let mut siblings: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir() && path.as_path() != chosen.source_path())
            .collect();
        siblings.sort();
        for path in siblings {
            if seen.insert(path.clone()) {
                candidates.push(path);
            }
        }
    }
    candidates
}

fn last_sequence(store: &SqliteStore, run_id: RunId) -> Result<u64, AppError> {
    Ok(store
        .load_events(run_id)?
        .last()
        .map_or(0, |event| event.sequence))
}

fn now() -> DateTime<Utc> {
    std::time::SystemTime::now().into()
}

fn quiescent_state(status: RunStatus, engine_status: Option<&EngineStatus>) -> QuiescentState {
    if let Some(EngineStatus::WaitingForProvider { stage_id, .. }) = engine_status {
        return QuiescentState::WaitingForProvider {
            stage_id: stage_id.clone(),
        };
    }
    match status {
        RunStatus::Completed => QuiescentState::Completed,
        RunStatus::NeedsUser => QuiescentState::NeedsUser,
        RunStatus::Paused => QuiescentState::Paused,
        RunStatus::Interrupted => QuiescentState::Interrupted,
        RunStatus::Failed => QuiescentState::Failed,
        RunStatus::Applied => QuiescentState::Applied,
        RunStatus::Discarded => QuiescentState::Discarded,
        RunStatus::Running if matches!(engine_status, Some(EngineStatus::Paused { .. })) => {
            QuiescentState::Paused
        }
        RunStatus::Running if matches!(engine_status, Some(EngineStatus::Interrupted { .. })) => {
            QuiescentState::Interrupted
        }
        run_status => QuiescentState::Active { run_status },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use tempfile::TempDir;

    use super::*;
    use crate::app::{
        DevelopmentFakeProviderFactory, ProviderFactory, RoutedProvider, UniformProvider,
    };
    use crate::domain::{
        ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactStatus, AttentionKind,
        ConfigSnapshotId, Dependency, DomainEventKind, EffortSetting, ProviderId, Role,
        StageDefinition, StageKind,
    };
    use crate::engine::{
        FakeEvent, FakeProvider, FakeScenario, Provider, ProviderError, ProviderPoll,
        ProviderRequest,
    };
    use crate::process::{OutputStream, ProcessManager, TmuxBackend};
    use crate::providers::ArtifactRecord;
    use crate::store::{ResolvedConfigSnapshot, SequencedEvent, StoreError};

    use crate::process::{ManagedProcessId, ProcessError};

    fn missing_evidence(process_id: ManagedProcessId) -> AppError {
        AppError::Process(ProcessError::MissingRuntimeEvidence(process_id))
    }

    // A resume issued while the run is still being driven loses a revision
    // race on rows the driver touched first. That is contention, not failure:
    // resuming is idempotent, so it is redone against newer rows instead of
    // reported to the user as a revision number.
    #[test]
    fn resume_redoes_the_attempt_after_losing_a_revision_race() {
        let cursor_race = || {
            AppError::Process(ProcessError::CursorConcurrentModification {
                process_id: ManagedProcessId::new(),
                stream: OutputStream::Stdout,
                expected: 33,
            })
        };
        let mut remaining = STOP_CONCURRENCY_ATTEMPTS - 1;
        let mut slept = 0_u32;
        let outcome = retry_concurrent(
            || {
                if remaining > 0 {
                    remaining -= 1;
                    Err(cursor_race())
                } else {
                    Ok(7_u32)
                }
            },
            |_| slept += 1,
        );
        assert_eq!(outcome.unwrap(), 7);
        assert_eq!(slept, STOP_CONCURRENCY_ATTEMPTS - 1);

        let exhausted = retry_concurrent::<u32, _, _>(|| Err(cursor_race()), |_| {});
        assert!(
            exhausted
                .expect_err("unbounded contention still has to surface")
                .is_concurrent_modification()
        );

        let real_failure =
            retry_concurrent::<u32, _, _>(|| Err(AppError::DirtySourceRepository), |_| {});
        assert!(matches!(real_failure, Err(AppError::DirtySourceRepository)));
    }

    // The window this covers: the runner spawns its child before writing
    // `runtime.json`, so a stop issued in the first moments of a run finds a
    // live process it cannot yet name. Waiting is the whole point — the file
    // is imminent — so the stop must survive the window rather than telling
    // the user the run cannot be stopped while it keeps running.
    #[test]
    fn stop_waits_out_the_runtime_evidence_window() {
        let process_id = ManagedProcessId::new();
        let mut remaining = STOP_EVIDENCE_ATTEMPTS - 1;
        let mut slept = 0_u32;
        let outcome = retry_stop(
            RunId::new(),
            || {
                if remaining > 0 {
                    remaining -= 1;
                    Err(missing_evidence(process_id))
                } else {
                    Ok(7_u32)
                }
            },
            |_| slept += 1,
        );
        assert_eq!(outcome.unwrap(), 7);
        assert_eq!(slept, STOP_EVIDENCE_ATTEMPTS - 1);
    }

    // The failure mode the fix must not produce is a stop that reports
    // success over a process that is still running. Evidence that never
    // arrives is a real condition — corrupt or foreign evidence looks
    // identical — so the bounded wait ends in an error that says the run was
    // not stopped, not in an Ok.
    #[test]
    fn stop_gives_up_loudly_when_runtime_evidence_never_arrives() {
        let process_id = ManagedProcessId::new();
        let run_id = RunId::new();
        let mut attempts = 0_u32;
        let error = retry_stop::<u32, _, _>(
            run_id,
            || {
                attempts += 1;
                Err(missing_evidence(process_id))
            },
            |_| {},
        )
        .expect_err("a stop that never finds its target must fail");
        assert_eq!(attempts, STOP_EVIDENCE_ATTEMPTS);
        assert!(matches!(
            error,
            AppError::StopRuntimeEvidenceUnavailable {
                run_id: reported_run,
                process_id: reported_process,
                ..
            } if reported_run == run_id && reported_process == process_id
        ));
        let message = error.to_string();
        assert!(message.contains("NOT stopped"), "{message}");
        assert!(message.contains("may still be running"), "{message}");
    }

    // The two transient conditions have separate budgets on purpose: a burst
    // of lost revision races must not consume the tolerance the startup
    // window needs, and vice versa. Interleaving them here pins that, since a
    // shared counter would exhaust well before this attempt count.
    #[test]
    fn concurrency_and_evidence_budgets_are_independent() {
        let process_id = ManagedProcessId::new();
        let mut races = STOP_CONCURRENCY_ATTEMPTS - 1;
        let mut windows = STOP_EVIDENCE_ATTEMPTS - 1;
        let outcome = retry_stop(
            RunId::new(),
            || {
                // Alternate while both budgets still have room, so neither is
                // spent in one uninterrupted burst.
                if races > 0 && (windows == 0 || races % 2 == 1) {
                    races -= 1;
                    return Err(AppError::Store(StoreError::ConcurrentModification {
                        run_id: RunId::new(),
                        expected: 3,
                    }));
                }
                if windows > 0 {
                    windows -= 1;
                    return Err(missing_evidence(process_id));
                }
                Ok(1_u32)
            },
            |_| {},
        );
        assert_eq!(outcome.unwrap(), 1);
    }

    // A permanent failure must not be delayed by either budget.
    #[test]
    fn non_retryable_failures_are_returned_at_once() {
        let mut attempts = 0_u32;
        let error = retry_stop::<u32, _, _>(
            RunId::new(),
            || {
                attempts += 1;
                Err(AppError::DirtySourceRepository)
            },
            |_| panic!("a permanent failure must not sleep"),
        )
        .expect_err("permanent failures surface");
        assert_eq!(attempts, 1);
        assert!(matches!(error, AppError::DirtySourceRepository));
    }

    #[derive(Clone)]
    struct ScriptedFactory {
        scenario: FakeScenario,
        inner: DevelopmentFakeProviderFactory,
    }

    #[derive(Clone)]
    struct RecordingFactory {
        tasks: Arc<Mutex<Vec<String>>>,
        inner: DevelopmentFakeProviderFactory,
    }

    struct RecordingProvider {
        inner: RoutedProvider,
        tasks: Arc<Mutex<Vec<String>>>,
    }

    #[derive(Clone)]
    struct GatedFactory {
        released: Arc<AtomicBool>,
        inner: DevelopmentFakeProviderFactory,
    }

    impl Provider for RecordingProvider {
        fn provider_id_for(&self, request: &ProviderRequest) -> Result<ProviderId, ProviderError> {
            self.inner.provider_id_for(request)
        }

        fn supports_request(&self, request: &ProviderRequest) -> Result<bool, ProviderError> {
            self.inner.supports_request(request)
        }

        fn supports_role(&self, role: Role) -> bool {
            self.inner.supports_role(role)
        }

        fn poll(
            &mut self,
            store: &mut SqliteStore,
            request: &ProviderRequest,
        ) -> Result<ProviderPoll, ProviderError> {
            self.tasks.lock().unwrap().push(request.task().to_owned());
            self.inner.poll(store, request)
        }
    }

    impl ProviderFactory for RecordingFactory {
        type Provider = RecordingProvider;

        fn require_provider(&self, provider: UniformProvider) -> Result<(), AppError> {
            (provider == UniformProvider::Fake)
                .then_some(())
                .ok_or_else(|| AppError::UnsupportedProvider(provider.as_str().to_owned()))
        }

        fn config_for_new_run(
            &self,
            selection: ExecutionSelection,
            effort: EffortRequest,
            workflow: &WorkflowDefinition,
            id: ConfigSnapshotId,
            created_at: DateTime<Utc>,
        ) -> Result<ResolvedConfigSnapshot, AppError> {
            self.inner
                .config_for_new_run(selection, effort, workflow, id, created_at)
        }

        fn for_run(
            &self,
            run_id: RunId,
            config: &ResolvedConfigSnapshot,
            workflow: &WorkflowDefinition,
            events: &[SequencedEvent],
        ) -> Result<Self::Provider, AppError> {
            Ok(RecordingProvider {
                inner: self.inner.for_run(run_id, config, workflow, events)?,
                tasks: Arc::clone(&self.tasks),
            })
        }
    }

    impl ProviderFactory for GatedFactory {
        type Provider = FakeProvider;

        fn require_provider(&self, provider: UniformProvider) -> Result<(), AppError> {
            (provider == UniformProvider::Fake)
                .then_some(())
                .ok_or_else(|| AppError::UnsupportedProvider(provider.as_str().to_owned()))
        }

        fn config_for_new_run(
            &self,
            selection: ExecutionSelection,
            effort: EffortRequest,
            workflow: &WorkflowDefinition,
            id: ConfigSnapshotId,
            created_at: DateTime<Utc>,
        ) -> Result<ResolvedConfigSnapshot, AppError> {
            self.inner
                .config_for_new_run(selection, effort, workflow, id, created_at)
        }

        fn for_run(
            &self,
            run_id: RunId,
            config: &ResolvedConfigSnapshot,
            workflow: &WorkflowDefinition,
            events: &[SequencedEvent],
        ) -> Result<Self::Provider, AppError> {
            let _ = self.inner.for_run(run_id, config, workflow, events)?;
            let scenario = FakeScenario::new()
                .stage("implementation")
                .events([
                    FakeEvent::Started,
                    FakeEvent::progress("checkpoint before process exit"),
                    FakeEvent::delay("process-restarted"),
                    FakeEvent::Completed,
                ])
                .stage("verify")
                .events([FakeEvent::Started, FakeEvent::Completed]);
            let mut provider = FakeProvider::new(scenario)?;
            if self.released.load(Ordering::SeqCst) {
                provider.release("implementation", "process-restarted")?;
            }
            Ok(provider)
        }
    }

    impl ProviderFactory for ScriptedFactory {
        type Provider = FakeProvider;

        fn require_provider(&self, provider: UniformProvider) -> Result<(), AppError> {
            (provider == UniformProvider::Fake)
                .then_some(())
                .ok_or_else(|| AppError::UnsupportedProvider(provider.as_str().to_owned()))
        }

        /// Seals the grant like the development factory does; the image
        /// tests then play the broker's part by hand against the worktree.
        fn config_for_new_run_with_image(
            &self,
            selection: ExecutionSelection,
            effort: EffortRequest,
            image: &ImageGenerationPlan,
            workflow: &WorkflowDefinition,
            id: ConfigSnapshotId,
            created_at: DateTime<Utc>,
        ) -> Result<ResolvedConfigSnapshot, AppError> {
            self.inner
                .config_for_new_run_with_image(selection, effort, image, workflow, id, created_at)
        }

        fn config_for_new_run(
            &self,
            selection: ExecutionSelection,
            effort: EffortRequest,
            workflow: &WorkflowDefinition,
            id: ConfigSnapshotId,
            created_at: DateTime<Utc>,
        ) -> Result<ResolvedConfigSnapshot, AppError> {
            self.inner
                .config_for_new_run(selection, effort, workflow, id, created_at)
        }

        fn for_run(
            &self,
            _run_id: RunId,
            _config: &ResolvedConfigSnapshot,
            _workflow: &WorkflowDefinition,
            _events: &[SequencedEvent],
        ) -> Result<Self::Provider, AppError> {
            Ok(FakeProvider::new(self.scenario.clone())?)
        }
    }

    struct Fixture {
        temp: TempDir,
        repo: PathBuf,
        database: PathBuf,
        worktrees: PathBuf,
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
            let worktrees = temp.path().join("data/worktrees");
            Self {
                temp,
                repo,
                database,
                worktrees,
            }
        }

        /// Commits a `.senate.toml` whose `[verify]` table runs exactly
        /// these commands, so the run's worktree — cut from `HEAD` — carries
        /// it.
        fn verify_with(&self, commands: &[&str]) {
            let list = commands
                .iter()
                .map(|command| format!("{command:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            fs::write(
                self.repo.join(".senate.toml"),
                format!("[verify]\ncommands = [{list}]\n"),
            )
            .unwrap();
            git(&self.repo, &["add", ".senate.toml"]);
            git(&self.repo, &["commit", "-qm", "verify config"]);
        }

        /// Every fake factory writes verification artifacts under this
        /// fixture, never under the developer's own data directory.
        fn fake_factory(&self) -> DevelopmentFakeProviderFactory {
            DevelopmentFakeProviderFactory::new(self.temp.path().join("runs"))
        }

        fn default_service(&self) -> RunService<DevelopmentFakeProviderFactory> {
            RunService::new(
                self.database.clone(),
                self.worktrees.clone(),
                self.fake_factory(),
            )
        }

        fn scripted_service(&self, scenario: FakeScenario) -> RunService<ScriptedFactory> {
            RunService::new(
                self.database.clone(),
                self.worktrees.clone(),
                ScriptedFactory {
                    scenario,
                    inner: self.fake_factory(),
                },
            )
        }
    }

    #[test]
    fn every_cli_workflow_completes_and_multiple_runs_are_queryable() {
        let fixture = Fixture::new();
        let service = fixture.default_service();
        let head = git_output(&fixture.repo, &["rev-parse", "HEAD"]);
        for kind in [
            WorkflowKind::Fast,
            WorkflowKind::Standard,
            WorkflowKind::Deep,
            WorkflowKind::Review,
        ] {
            let report = service
                .start_run(
                    kind,
                    format!("task for {kind:?}"),
                    &fixture.repo,
                    Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                    EffortSetting::NativeDefault,
                    &ImageGenerationPlan::disabled(),
                )
                .unwrap();
            assert_eq!(report.details.status, RunStatus::Completed);
            assert!(
                report
                    .details
                    .stages
                    .iter()
                    .all(|stage| stage.status == StageStatus::Completed)
            );
        }
        assert_eq!(service.list_runs().unwrap().len(), 4);
        assert_eq!(git_output(&fixture.repo, &["rev-parse", "HEAD"]), head);
        assert!(git_output(&fixture.repo, &["status", "--porcelain"]).is_empty());
    }

    /// The verify stage is never faked: every agent role goes to the Fake
    /// provider, and the repository's own `[verify]` commands really run in
    /// the worktree. Passing checks let the run complete and be applied.
    #[test]
    fn a_standard_run_runs_the_repositorys_verify_commands_and_can_be_applied() {
        let fixture = Fixture::new();
        fixture.verify_with(&["true"]);
        let service = fixture.default_service();

        let report = service
            .start_run(
                WorkflowKind::Standard,
                "task with passing checks",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();

        assert_eq!(report.details.status, RunStatus::Completed);
        let verify = report
            .details
            .stages
            .iter()
            .find(|stage| stage.kind == StageKind::Verify)
            .expect("standard runs verify");
        assert_eq!(verify.id.to_string(), "verify");
        assert_eq!(verify.status, StageStatus::Completed);
        assert_eq!(verify.configured_provider, "verify");
        assert_eq!(verify.actual_provider.as_deref(), Some("verify"));
        assert!(
            report
                .details
                .routes
                .iter()
                .all(|route| route.role != Role::Verifier),
            "the verifier is not a persisted route"
        );
        let artifact = service
            .read_artifact(report.details.id, &verify.id)
            .unwrap();
        assert_eq!(artifact.summary.kind, ArtifactKind::Verify);
        assert_eq!(artifact.summary.provider.as_deref(), Some("verify"));
        assert!(
            artifact
                .text
                .contains("## Bottom line\npassed — 1 command\n")
        );
        assert!(artifact.text.contains("### $ true\nexit: 0\n"));
        // The artifact lives under the fixture, never under the developer's
        // own data directory: a fake factory must not leave `verify.md`
        // files in `~/.senate/runs` on every test run.
        let store = SqliteStore::open(&fixture.database).unwrap();
        let records = store.list_artifacts(report.details.id).unwrap();
        assert!(
            records
                .iter()
                .all(|record| record.path().starts_with(fixture.temp.path())),
            "{records:?}"
        );
        if let Ok(real_root) = crate::store::process_root() {
            assert!(
                !real_root.join(report.details.id.to_string()).exists(),
                "test artifacts leaked into {}",
                real_root.display()
            );
        }

        let (outcome, after) = service.apply_run(report.details.id).unwrap();
        assert_eq!(outcome, ApplyOutcome::NoChanges);
        assert_eq!(after.details.status, RunStatus::Completed);
    }

    /// A failing check does not dead-end the run: the decision still runs
    /// (verification is an optional edge into it), the run completes, apply
    /// refuses by naming verification, publish is free to carry the change to
    /// a branch anyway — and a fix cycle stays available to answer it.
    #[test]
    fn a_failing_verify_command_completes_the_run_apply_names_verification_and_a_fix_is_available()
    {
        let fixture = Fixture::new();
        fixture.verify_with(&["true", "false"]);
        let service = fixture.default_service();

        let report = service
            .start_run(
                WorkflowKind::Standard,
                "task with failing checks",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();

        assert_eq!(report.details.status, RunStatus::Completed);
        let stage = |kind: StageKind| {
            report
                .details
                .stages
                .iter()
                .find(|stage| stage.kind == kind)
                .unwrap()
        };
        assert_eq!(stage(StageKind::Verify).status, StageStatus::Failed);
        assert_eq!(stage(StageKind::Decision).status, StageStatus::Completed);
        let artifact = service
            .read_artifact(report.details.id, &stage(StageKind::Verify).id)
            .unwrap();
        assert_eq!(artifact.summary.status, ArtifactStatus::Failed);
        assert!(
            artifact
                .text
                .contains("## Bottom line\nfailed — false exited 1\n")
        );

        let error = service.apply_run(report.details.id).unwrap_err();
        assert!(
            matches!(
                &error,
                AppError::Workspace(WorkspaceError::VerificationNotPassed { stage_id, status })
                    if stage_id.as_str() == "verify" && status == "failed"
            ),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            "verification did not pass: stage verify is failed"
        );

        // Publish is deliberately not gated on verification: a branch and a
        // pull request are where an unverified change belongs. This fixture
        // has no `origin`, so publish gets as far as the remote check — which
        // is proof enough that verification stopped refusing it.
        let error = service.publish_run(report.details.id).unwrap_err();
        assert!(
            matches!(&error, AppError::Workspace(WorkspaceError::NoRemote(_))),
            "{error}"
        );

        let fixed = service.request_fix(report.details.id).unwrap();
        assert!(
            fixed
                .details
                .stages
                .iter()
                .any(|stage| stage.id.as_str() == "verify_1"),
            "a fix cycle, with its own verification, was appended"
        );
    }

    /// The loop the gate exists for: checks fail, a fix cycle runs, its own
    /// verification passes, and only then does apply go through. The
    /// commands read a marker file so the same `.senate.toml` fails first
    /// and passes after the "fix".
    #[test]
    fn apply_succeeds_once_a_fix_cycles_own_verification_passes() {
        let fixture = Fixture::new();
        let marker = fixture.temp.path().join("checks-pass");
        fixture.verify_with(&[&format!("test -e {}", marker.display())]);
        let service = fixture.default_service();

        let first = service
            .start_run(
                WorkflowKind::Standard,
                "task fixed on the second attempt",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(first.details.status, RunStatus::Completed);
        assert!(matches!(
            service.apply_run(first.details.id),
            Err(AppError::Workspace(
                WorkspaceError::VerificationNotPassed { .. }
            ))
        ));

        fs::write(&marker, "").unwrap();
        let fixed = service.request_fix(first.details.id).unwrap();
        assert_eq!(fixed.details.status, RunStatus::Completed);
        let status = |id: &str| {
            fixed
                .details
                .stages
                .iter()
                .find(|stage| stage.id.as_str() == id)
                .unwrap()
                .status
        };
        assert_eq!(
            status("verify"),
            StageStatus::Failed,
            "the first verdict stands"
        );
        assert_eq!(status("verify_1"), StageStatus::Completed);
        assert_eq!(status("decision_1"), StageStatus::Completed);

        let (outcome, applied) = service.apply_run(first.details.id).unwrap();
        assert_eq!(outcome, ApplyOutcome::NoChanges);
        assert_eq!(applied.details.status, RunStatus::Completed);
    }

    /// A completed run can be sent back with the operator's own instruction:
    /// the run reopens, grows exactly one follow-up stage, its verification
    /// and a fresh decision, and reaches `Completed` again.
    ///
    /// `ScriptedFactory` also pins the follow-up cycle's stages to explicit,
    /// deliberately narrated scripts — belt and braces alongside
    /// `request_continue_completes_on_a_fake_run_without_prescripting_appended_stages`,
    /// which proves the same cycle completes with no scripting at all.
    #[test]
    fn a_completed_run_grows_a_continue_cycle_and_completes_again() {
        let fixture = Fixture::new();
        let mut scenario =
            FakeScenario::successful(&WorkflowDefinition::built_in(WorkflowKind::Standard));
        for stage_id in ["followup_1", "followup_verify_1", "followup_decision_1"] {
            scenario = scenario
                .stage(stage_id)
                .events([FakeEvent::Started, FakeEvent::Completed]);
        }
        let service = fixture.scripted_service(scenario);
        let started = service
            .start_run(
                WorkflowKind::Standard,
                "task for continue",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(started.details.status, RunStatus::Completed);
        let before = started.details.stages.len();

        let report = service
            .request_continue(
                started.details.id,
                "  add tests for the edge case  ".to_owned(),
            )
            .unwrap();

        assert_eq!(report.details.status, RunStatus::Completed);
        assert_eq!(report.details.stages.len(), before + 3);
        let follow_up = report
            .details
            .stages
            .iter()
            .find(|stage| stage.kind == StageKind::FollowUp)
            .expect("a follow-up stage was appended");
        assert_eq!(follow_up.status, StageStatus::Completed);
        assert_eq!(follow_up.id.to_string(), "followup_1");
    }

    /// Regression: `FakeScenario::successful` used to be a snapshot of the
    /// workflow at provider-construction time, so a stage a remediation
    /// cycle appended mid-drive had no script and polling it failed with
    /// "no fake script for stage …" — breaking `[c]`/`[w]` (and, latently,
    /// `[f]`) for every fake-provider run, `--provider fake` included since
    /// `RoutedProvider` builds its own fake runtime the same way. Driven
    /// through the plain `default_service()` — no `ScriptedFactory`, no
    /// stage prescripted by hand — so this only stays green if the fake
    /// runtime itself heals the gap.
    #[test]
    fn request_continue_completes_on_a_fake_run_without_prescripting_appended_stages() {
        let fixture = Fixture::new();
        let service = fixture.default_service();
        let started = service
            .start_run(
                WorkflowKind::Standard,
                "task for continue",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(started.details.status, RunStatus::Completed);

        let report = service
            .request_continue(started.details.id, "add more tests".to_owned())
            .unwrap();

        assert_eq!(report.details.status, RunStatus::Completed);
        assert!(report.details.stages.iter().any(
            |stage| stage.kind == StageKind::FollowUp && stage.status == StageStatus::Completed
        ));
    }

    /// Same regression as
    /// `request_continue_completes_on_a_fake_run_without_prescripting_appended_stages`,
    /// for the sibling cycle: `[f]` Fix had the identical latent bug on a
    /// fake run before the runtime fix, just never a test to catch it.
    #[test]
    fn request_fix_completes_on_a_fake_run_without_prescripting_appended_stages() {
        let fixture = Fixture::new();
        let service = fixture.default_service();
        let started = service
            .start_run(
                WorkflowKind::Standard,
                "task for fix",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(started.details.status, RunStatus::Completed);

        let report = service.request_fix(started.details.id).unwrap();

        assert_eq!(report.details.status, RunStatus::Completed);
        assert!(
            report
                .details
                .stages
                .iter()
                .any(|stage| stage.kind == StageKind::Fix
                    && stage.status == StageStatus::Completed)
        );
    }

    #[test]
    fn request_continue_refuses_a_blank_instruction_without_touching_the_run() {
        let fixture = Fixture::new();
        let service = fixture.default_service();
        let started = service
            .start_run(
                WorkflowKind::Standard,
                "task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();

        for blank in ["", "   ", "\n\t"] {
            let error = service
                .request_continue(started.details.id, blank.to_owned())
                .unwrap_err();
            assert!(
                matches!(error, AppError::EmptyContinueInstruction(id) if id == started.details.id)
            );
        }
        // A refused request never reopened the run.
        let after = service.inspect_run(started.details.id).unwrap();
        assert_eq!(after.stages.len(), started.details.stages.len());
    }

    /// Same guard as `request_fix`, reused rather than duplicated: a
    /// workflow with no decision has no verdict to continue past.
    #[test]
    fn request_continue_refuses_a_run_that_never_reached_a_decision() {
        let fixture = Fixture::new();
        let service = fixture.default_service();
        let started = service
            .start_run(
                WorkflowKind::Fast,
                "task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();

        let error = service
            .request_continue(started.details.id, "keep going".to_owned())
            .unwrap_err();
        assert!(matches!(
            error,
            AppError::Engine(crate::engine::EngineError::Fix(
                crate::domain::RunFixError::NoDecisionToAnswer
            ))
        ));
    }

    /// `adopt_branch_for_fix` used to run before the run-status check, so a
    /// refused continue against a Review run's detached workspace (Review
    /// never appends `Implementation`/`Fix`/`FollowUp`, so it never earns a
    /// branch on its own) would still convert that workspace to a Senate
    /// branch — making an untouched completed review look applyable even
    /// though the continue itself never went through. Blocking on
    /// `NeedsUser` rather than driving to `Completed` exercises the explicit
    /// run-status check that now runs, and branch adoption with it, before
    /// anything durable happens.
    #[test]
    fn a_refused_continue_on_a_review_run_leaves_the_workspace_detached() {
        let fixture = Fixture::new();
        let scenario = FakeScenario::new().stage("research").events([
            FakeEvent::Started,
            FakeEvent::needs_user(AttentionKind::Decision, "which angle first?"),
            FakeEvent::progress("resumed after resolution"),
            FakeEvent::Completed,
        ]);
        let started = fixture
            .scripted_service(scenario)
            .start_run(
                WorkflowKind::Review,
                "task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(started.details.status, RunStatus::NeedsUser);
        let run_id = started.details.id;
        let workspace_before = SqliteStore::open(&fixture.database)
            .unwrap()
            .load_workspace(run_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            workspace_before.mode(),
            crate::workspace::WorkspaceMode::Detached
        );

        let error = fixture
            .default_service()
            .request_continue(run_id, "keep going".to_owned())
            .unwrap_err();
        assert!(matches!(
            error,
            AppError::Engine(crate::engine::EngineError::Fix(
                crate::domain::RunFixError::RunNotCompleted(RunStatus::NeedsUser)
            ))
        ));

        let workspace_after = SqliteStore::open(&fixture.database)
            .unwrap()
            .load_workspace(run_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            workspace_after.mode(),
            crate::workspace::WorkspaceMode::Detached,
            "a refused continue must never adopt a branch"
        );
    }

    /// Same pre-existing flaw as `request_continue`, in the same file, fixed
    /// identically: `request_fix` adopted a Review run's branch before its
    /// own run-status check too.
    #[test]
    fn a_refused_fix_on_a_review_run_leaves_the_workspace_detached() {
        let fixture = Fixture::new();
        let scenario = FakeScenario::new().stage("research").events([
            FakeEvent::Started,
            FakeEvent::needs_user(AttentionKind::Decision, "which angle first?"),
            FakeEvent::progress("resumed after resolution"),
            FakeEvent::Completed,
        ]);
        let started = fixture
            .scripted_service(scenario)
            .start_run(
                WorkflowKind::Review,
                "task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(started.details.status, RunStatus::NeedsUser);
        let run_id = started.details.id;

        let error = fixture.default_service().request_fix(run_id).unwrap_err();
        assert!(matches!(
            error,
            AppError::Engine(crate::engine::EngineError::Fix(
                crate::domain::RunFixError::RunNotCompleted(RunStatus::NeedsUser)
            ))
        ));

        let workspace_after = SqliteStore::open(&fixture.database)
            .unwrap()
            .load_workspace(run_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            workspace_after.mode(),
            crate::workspace::WorkspaceMode::Detached,
            "a refused fix must never adopt a branch"
        );
    }

    /// The managed worktree is built from committed HEAD, so a dirty source
    /// would silently hide the user's work from the agent. Every start is
    /// refused before anything durable exists.
    #[test]
    fn a_dirty_source_repository_is_refused_before_any_run_is_persisted() {
        for (label, dirty) in [("tracked modification", true), ("untracked file", false)] {
            let fixture = Fixture::new();
            if dirty {
                fs::write(fixture.repo.join("README.md"), "uncommitted edit\n").unwrap();
            } else {
                fs::write(fixture.repo.join("scratch.txt"), "untracked\n").unwrap();
            }
            let error = fixture
                .default_service()
                .start_run(
                    WorkflowKind::Fast,
                    "task",
                    &fixture.repo,
                    Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                    EffortSetting::NativeDefault,
                    &ImageGenerationPlan::disabled(),
                )
                .unwrap_err();
            assert!(
                matches!(error, AppError::DirtySourceRepository),
                "{label}: {error:?}"
            );
            let message = error.to_string();
            assert!(message.contains("uncommitted changes"), "{label}");
            assert!(message.contains("Commit or stash"), "{label}");

            // Nothing durable may survive a refused start.
            assert!(
                !fixture.database.exists(),
                "{label}: a refused start created a database"
            );
            assert!(
                !fixture.worktrees.exists(),
                "{label}: a refused start created workspace resources"
            );
            assert_eq!(
                fixture.default_service().list_runs().unwrap().len(),
                0,
                "{label}: a refused start left a run behind"
            );
        }
    }

    /// A `gh` stand-in that answers every probe with `body`. No test reaches a
    /// network.
    fn stub_gh(directory: &std::path::Path, body: &str) -> GhClient {
        use std::os::unix::fs::PermissionsExt;
        let script = directory.join("gh");
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        GhClient::with_executable(script)
    }

    /// Run `01M1K8KAJ1HMS47H7WR8YMN2PW` was started on an enterprise pull
    /// request URL while its host was reachable only over VPN. Every stage
    /// then wrote an artifact saying it was blocked, and the run finished
    /// looking successful. The pull request need not live in this repository —
    /// reviewing a remote one is allowed — but an unreadable one is refused
    /// before anything durable exists.
    #[test]
    fn an_unreadable_pull_request_is_refused_before_any_run_is_persisted() {
        let fixture = Fixture::new();
        let service = fixture
            .default_service()
            .probing_pull_requests_with(stub_gh(
                fixture.temp.path(),
                "echo 'dial tcp 192.0.80.209:443: i/o timeout' >&2\nexit 1",
            ));
        let error = service
            .start_run(
                WorkflowKind::Review,
                "https://github.a8c.com/Automattic/missioncontrol/pull/164318",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap_err();
        let AppError::PullRequestUnreachable {
            ref url,
            ref host,
            ref detail,
        } = error
        else {
            panic!("{error:?}");
        };
        assert_eq!(
            url,
            "https://github.a8c.com/Automattic/missioncontrol/pull/164318"
        );
        assert_eq!(host, "github.a8c.com");
        assert!(detail.contains("i/o timeout"), "{detail}");
        let message = error.to_string();
        assert!(message.contains("not started"), "{message}");
        assert!(message.contains("gh auth status"), "{message}");

        assert!(
            !fixture.database.exists(),
            "a refused start created a database"
        );
        assert!(
            !fixture.worktrees.exists(),
            "a refused start created workspace resources"
        );
    }

    /// The pull request check is what a start waits on while its host is
    /// silent, so it is reported before it runs — and a refusal reports
    /// nothing after it, since no run ever came to exist.
    #[test]
    fn a_start_reports_its_pull_request_check_and_stops_reporting_when_refused() {
        let fixture = Fixture::new();
        let service = fixture
            .default_service()
            .probing_pull_requests_with(stub_gh(
                fixture.temp.path(),
                "echo 'dial tcp 192.0.80.209:443: i/o timeout' >&2\nexit 1",
            ));
        let seen = std::cell::RefCell::new(Vec::new());
        service
            .start_run_observed(
                WorkflowKind::Review,
                "https://github.a8c.com/Automattic/wpcom/pull/241491",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
                &|progress| seen.borrow_mut().push(progress),
            )
            .unwrap_err();
        assert_eq!(
            seen.into_inner(),
            vec![
                StartProgress::FindingCheckout {
                    repository: "Automattic/wpcom".to_owned(),
                },
                StartProgress::CheckingPullRequest {
                    url: "https://github.a8c.com/Automattic/wpcom/pull/241491".to_owned(),
                    host: "github.a8c.com".to_owned(),
                },
            ]
        );
    }

    /// A start that gets through its checks names its run as soon as the run
    /// exists, and again once the run is actually running.
    #[test]
    fn a_start_names_its_run_once_it_exists_and_once_it_runs() {
        let fixture = Fixture::new();
        let seen = std::cell::RefCell::new(Vec::new());
        let report = fixture
            .default_service()
            .start_run_observed(
                WorkflowKind::Standard,
                "no pull request here",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
                &|progress| seen.borrow_mut().push(progress),
            )
            .unwrap();
        let run_id = report.details.id;
        assert_eq!(
            seen.into_inner(),
            vec![
                StartProgress::PreparingWorkspace(run_id),
                StartProgress::Running(run_id),
            ]
        );
    }

    /// A pull request in another repository is a legitimate run: the run is
    /// refused for unreadability alone, never for belonging elsewhere.
    #[test]
    fn a_readable_pull_request_from_another_repository_still_starts() {
        let fixture = Fixture::new();
        let started = fixture
            .default_service()
            .probing_pull_requests_with(stub_gh(fixture.temp.path(), "echo 164318"))
            .start_run(
                WorkflowKind::Review,
                "https://github.a8c.com/Automattic/missioncontrol/pull/164318",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_ne!(started.details.status, RunStatus::Failed);
    }

    /// Initialises a fresh Git repository at `path` with one commit, so it is
    /// a discoverable checkout of its own — a stand-in for the sibling
    /// checkouts and previously-used repositories the pull-request-checkout
    /// search looks for.
    fn init_checkout(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "-q"]);
        git(path, &["config", "user.email", "test@example.com"]);
        git(path, &["config", "user.name", "Test"]);
        fs::write(path.join("README.md"), "baseline\n").unwrap();
        git(path, &["add", "README.md"]);
        git(path, &["commit", "-qm", "initial"]);
    }

    const JETPACK_PULL_REQUEST: &str = "https://github.com/Automattic/jetpack/pull/52322";

    /// The shape of run `01M2JTBB08W8VXP3RM1V7Z9YQ2`: the operator opened
    /// The Senate in a checkout of one repository (Calypso) and started a run
    /// whose task named a pull request belonging to another (Jetpack), with a
    /// checkout of that other repository sitting right next to the one
    /// chosen. The Fix stage that later ran there changed nothing, because
    /// the worktree held the wrong repository's history from the start. The
    /// run must switch to the sibling checkout before anything is persisted.
    #[test]
    fn a_pull_request_from_elsewhere_switches_to_a_sibling_checkout_that_matches() {
        let fixture = Fixture::new();
        git(
            &fixture.repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/other/repo.git",
            ],
        );
        let sibling = fixture.temp.path().join("jetpack");
        init_checkout(&sibling);
        git(
            &sibling,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/Automattic/jetpack.git",
            ],
        );

        let report = fixture
            .default_service()
            .probing_pull_requests_with(stub_gh(fixture.temp.path(), "echo 164318"))
            .start_run(
                WorkflowKind::Fast,
                JETPACK_PULL_REQUEST,
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();

        assert_ne!(report.details.status, RunStatus::Failed);
        assert_eq!(
            report.details.repository.as_deref(),
            Some(sibling.canonicalize().unwrap().as_path())
        );
    }

    /// A run started from history rather than a sibling: no directory next to
    /// the chosen checkout matches, but a checkout The Senate has previously
    /// started a run from does. The store, not the filesystem next door, is
    /// what finds it.
    #[test]
    fn a_pull_request_from_elsewhere_switches_to_a_previously_used_checkout() {
        let fixture = Fixture::new();
        let elsewhere = fixture.temp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let previous = elsewhere.join("jetpack");
        init_checkout(&previous);
        git(
            &previous,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/Automattic/jetpack.git",
            ],
        );
        let service = fixture
            .default_service()
            .probing_pull_requests_with(stub_gh(fixture.temp.path(), "echo 164318"));
        // Records `previous` in `run_workspaces` as a source checkout, with no
        // pull request involved.
        service
            .start_run(
                WorkflowKind::Fast,
                "an ordinary task",
                &previous,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();

        git(
            &fixture.repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/other/repo.git",
            ],
        );
        let report = service
            .start_run(
                WorkflowKind::Fast,
                JETPACK_PULL_REQUEST,
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();

        assert_ne!(report.details.status, RunStatus::Failed);
        assert_eq!(
            report.details.repository.as_deref(),
            Some(previous.canonicalize().unwrap().as_path())
        );
    }

    /// No checkout anywhere matches the pull request's repository, and the
    /// workflow edits — it needs to write into that repository's own history,
    /// which nothing on hand can provide. Refused before anything durable
    /// exists, naming the repository and where The Senate looked.
    #[test]
    fn a_pull_request_from_elsewhere_with_no_local_checkout_refuses_an_editing_workflow() {
        let fixture = Fixture::new();
        git(
            &fixture.repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/other/repo.git",
            ],
        );
        let error = fixture
            .default_service()
            .probing_pull_requests_with(stub_gh(fixture.temp.path(), "echo 164318"))
            .start_run(
                WorkflowKind::Fast,
                JETPACK_PULL_REQUEST,
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap_err();
        let AppError::NoLocalCheckoutForPullRequest {
            ref url,
            ref repository,
            ..
        } = error
        else {
            panic!("{error:?}");
        };
        assert_eq!(url, JETPACK_PULL_REQUEST);
        assert_eq!(repository, "github.com/Automattic/jetpack");
        let message = error.to_string();
        assert!(message.contains("clone"), "{message}");

        assert!(
            !fixture.database.exists(),
            "a refused start created a database"
        );
        assert!(
            !fixture.worktrees.exists(),
            "a refused start created workspace resources"
        );
    }

    /// No checkout anywhere matches, but the workflow is read-only: reviewing
    /// a remote pull request over the network is legitimate, so the run keeps
    /// today's behaviour and starts in the checkout the operator chose.
    #[test]
    fn a_pull_request_from_elsewhere_with_no_local_checkout_still_starts_a_review() {
        let fixture = Fixture::new();
        git(
            &fixture.repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/other/repo.git",
            ],
        );
        let report = fixture
            .default_service()
            .probing_pull_requests_with(stub_gh(fixture.temp.path(), "echo 164318"))
            .start_run(
                WorkflowKind::Review,
                JETPACK_PULL_REQUEST,
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_ne!(report.details.status, RunStatus::Failed);
        assert_eq!(
            report.details.repository.as_deref(),
            Some(fixture.repo.canonicalize().unwrap().as_path())
        );
    }

    /// The one local checkout that matches is dirty: not a usable answer, for
    /// the same reason a dirty chosen checkout is refused. Named, not
    /// silently skipped.
    #[test]
    fn a_pull_request_matching_only_a_dirty_checkout_is_refused_by_name() {
        let fixture = Fixture::new();
        git(
            &fixture.repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/other/repo.git",
            ],
        );
        let sibling = fixture.temp.path().join("jetpack");
        init_checkout(&sibling);
        git(
            &sibling,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/Automattic/jetpack.git",
            ],
        );
        fs::write(sibling.join("README.md"), "uncommitted\n").unwrap();

        let error = fixture
            .default_service()
            .probing_pull_requests_with(stub_gh(fixture.temp.path(), "echo 164318"))
            .start_run(
                WorkflowKind::Fast,
                JETPACK_PULL_REQUEST,
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap_err();
        let AppError::PullRequestCheckoutsDirty {
            ref url,
            ref checkouts,
        } = error
        else {
            panic!("{error:?}");
        };
        assert_eq!(url, JETPACK_PULL_REQUEST);
        assert_eq!(*checkouts, vec![sibling.canonicalize().unwrap()]);

        assert!(
            !fixture.database.exists(),
            "a refused start created a database"
        );
    }

    /// A review only reads, so a dirty matching checkout is no reason to
    /// refuse it: it starts in the chosen checkout, exactly as it would have
    /// before The Senate looked for the pull request's own.
    #[test]
    fn a_review_whose_only_matching_checkout_is_dirty_starts_where_it_was_asked() {
        let fixture = Fixture::new();
        git(
            &fixture.repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/other/repo.git",
            ],
        );
        let sibling = fixture.temp.path().join("jetpack");
        init_checkout(&sibling);
        git(
            &sibling,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/Automattic/jetpack.git",
            ],
        );
        fs::write(sibling.join("README.md"), "uncommitted\n").unwrap();

        let report = fixture
            .default_service()
            .probing_pull_requests_with(stub_gh(fixture.temp.path(), "echo 164318"))
            .start_run(
                WorkflowKind::Review,
                JETPACK_PULL_REQUEST,
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(
            report.details.repository.as_deref(),
            Some(fixture.repo.canonicalize().unwrap().as_path())
        );
    }

    /// The chosen checkout's own `origin` already names the pull request's
    /// repository: no search is needed or performed, and the run starts
    /// exactly where the operator pointed it, unchanged.
    #[test]
    fn a_pull_request_matching_the_chosen_repository_needs_no_search() {
        let fixture = Fixture::new();
        git(
            &fixture.repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/Automattic/jetpack.git",
            ],
        );
        let report = fixture
            .default_service()
            .probing_pull_requests_with(stub_gh(fixture.temp.path(), "echo 164318"))
            .start_run(
                WorkflowKind::Fast,
                JETPACK_PULL_REQUEST,
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_ne!(report.details.status, RunStatus::Failed);
        assert_eq!(
            report.details.repository.as_deref(),
            Some(fixture.repo.canonicalize().unwrap().as_path())
        );
    }

    /// Review workflows read the source too, so they take the same preflight.
    #[test]
    fn every_workflow_takes_the_dirty_source_preflight() {
        for kind in [
            WorkflowKind::Fast,
            WorkflowKind::Standard,
            WorkflowKind::Deep,
            WorkflowKind::Review,
        ] {
            let fixture = Fixture::new();
            fs::write(fixture.repo.join("README.md"), "uncommitted\n").unwrap();
            let error = fixture
                .default_service()
                .start_run(
                    kind,
                    "task",
                    &fixture.repo,
                    Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                    EffortSetting::NativeDefault,
                    &ImageGenerationPlan::disabled(),
                )
                .unwrap_err();
            assert!(matches!(error, AppError::DirtySourceRepository), "{kind:?}");
        }
    }

    /// Committing the same content makes the identical start succeed, so the
    /// preflight rejects dirtiness rather than the repository itself.
    #[test]
    fn a_clean_source_repository_starts_normally() {
        let fixture = Fixture::new();
        fs::write(fixture.repo.join("feature.txt"), "work\n").unwrap();
        git(&fixture.repo, &["add", "feature.txt"]);
        git(&fixture.repo, &["commit", "-qm", "feature"]);
        let report = fixture
            .default_service()
            .start_run(
                WorkflowKind::Fast,
                "task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(report.details.status, RunStatus::Completed);
    }

    /// The preflight applies to new runs only: an existing run resumes even if
    /// the source has since been modified.
    #[test]
    fn resuming_an_existing_run_is_not_subject_to_the_preflight() {
        let fixture = Fixture::new();
        let report = fixture
            .default_service()
            .start_run(
                WorkflowKind::Fast,
                "task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = report.details.id;
        fs::write(fixture.repo.join("README.md"), "dirty after the fact\n").unwrap();
        let resumed = fixture.default_service().resume_run(run_id).unwrap();
        assert_eq!(resumed.details.id, run_id);
    }

    #[test]
    fn restart_preserves_completed_run_input_and_does_not_replay_signals() {
        let fixture = Fixture::new();
        let report = fixture
            .default_service()
            .start_run(
                WorkflowKind::Deep,
                "  Unicode α\nsecond line  ",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = report.details.id;
        drop(report);

        let restarted = fixture.default_service();
        let inspected = restarted.inspect_run(run_id).unwrap();
        assert_eq!(inspected.task.as_deref(), Some("Unicode α\nsecond line"));
        assert_eq!(inspected.status, RunStatus::Completed);
        let resumed = restarted.resume_run(run_id).unwrap();
        assert!(resumed.committed_events.is_empty());
        assert_eq!(resumed.details, inspected);
    }

    #[test]
    fn legacy_reviewer_snapshot_resumes_its_stored_graph_without_conversion() {
        let fixture = Fixture::new();
        let created_at = now();
        let run_id = RunId::from_u128(9_100_001);
        let config_id = ConfigSnapshotId::new("legacy-reviewer-config").unwrap();
        let id = |value: &str| StageId::new(value).unwrap();
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Standard,
            vec![
                StageDefinition::new(
                    id("architecture"),
                    StageKind::Architecture,
                    Role::Architect,
                    vec![],
                ),
                StageDefinition::new(
                    id("implementation"),
                    StageKind::Implementation,
                    Role::Implementer,
                    vec![Dependency::required(id("architecture"))],
                ),
                StageDefinition::new(
                    id("review"),
                    StageKind::Review,
                    Role::Reviewer,
                    vec![Dependency::required(id("implementation"))],
                ),
                StageDefinition::new(
                    id("decision"),
                    StageKind::Decision,
                    Role::EngineeringLead,
                    vec![Dependency::required(id("review"))],
                ),
            ],
        )
        .unwrap();
        let run = Run::new(run_id, workflow.clone(), config_id.clone(), created_at);
        let input = RunInput::new(run_id, "legacy standard task", created_at).unwrap();
        let config = fixture
            .fake_factory()
            .config_for_new_run(
                ExecutionSelection::Uniform(UniformProvider::Fake),
                EffortSetting::NativeDefault.into(),
                &workflow,
                config_id,
                created_at,
            )
            .unwrap();
        let event = run.created_event(EventMetadata::new(EventId::new(), created_at));
        let mut store = SqliteStore::open(&fixture.database).unwrap();
        store
            .create_run_with_input(&run, &input, &config, &[event])
            .unwrap();
        WorkspaceManager::new(&fixture.worktrees)
            .prepare_run_workspace(&mut store, run_id, &fixture.repo)
            .unwrap();
        drop(store);

        let reloaded = fixture.default_service().inspect_run(run_id).unwrap();
        assert_eq!(reloaded.stages.len(), 4);
        let completed = fixture.default_service().resume_run(run_id).unwrap();
        assert_eq!(completed.details.status, RunStatus::Completed);
        let loaded = SqliteStore::open(&fixture.database)
            .unwrap()
            .load_run(run_id)
            .unwrap()
            .run;
        assert_eq!(loaded.workflow(), &workflow);
        assert_eq!(
            loaded.workflow().stage(&id("review")).unwrap().role(),
            Role::Reviewer
        );
        assert!(loaded.workflow().stage(&id("quality_review")).is_none());
        assert!(loaded.workflow().stage(&id("spec_review")).is_none());
    }

    #[test]
    fn restart_after_one_specialized_review_does_not_replay_completed_branch() {
        let fixture = Fixture::new();
        let scenario = || {
            FakeScenario::new()
                .stage("architecture")
                .events([FakeEvent::Started, FakeEvent::Completed])
                .stage("implementation")
                .events([FakeEvent::Started, FakeEvent::Completed])
                .stage("simplification")
                .events([FakeEvent::Started, FakeEvent::Completed])
                .stage("verify")
                .events([FakeEvent::Started, FakeEvent::Completed])
                .stage("quality_review")
                .events([FakeEvent::Started, FakeEvent::Completed])
                .stage("spec_review")
                .events([
                    FakeEvent::Started,
                    FakeEvent::Interrupted,
                    FakeEvent::Completed,
                ])
                .stage("decision")
                .events([FakeEvent::Started, FakeEvent::Completed])
        };
        let interrupted = fixture
            .scripted_service(scenario())
            .start_run(
                WorkflowKind::Standard,
                "restart specialized reviews",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = interrupted.details.id;
        let quality = StageId::new("quality_review").unwrap();
        let spec = StageId::new("spec_review").unwrap();
        let decision = StageId::new("decision").unwrap();
        assert_eq!(
            interrupted
                .details
                .stages
                .iter()
                .find(|stage| stage.id == quality)
                .unwrap()
                .status,
            StageStatus::Completed
        );
        assert_eq!(
            interrupted
                .details
                .stages
                .iter()
                .find(|stage| stage.id == spec)
                .unwrap()
                .status,
            StageStatus::Interrupted
        );
        assert_eq!(
            interrupted
                .details
                .stages
                .iter()
                .find(|stage| stage.id == decision)
                .unwrap()
                .status,
            StageStatus::Pending
        );

        let completed = fixture
            .scripted_service(scenario())
            .resume_run(run_id)
            .unwrap();
        assert_eq!(completed.details.status, RunStatus::Completed);
        let events = SqliteStore::open(&fixture.database)
            .unwrap()
            .load_events(run_id)
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.event.stage_id() == Some(&quality)
                        && matches!(event.event.kind(), DomainEventKind::ProviderStarted { .. })
                })
                .count(),
            1
        );
    }

    /// The dependency information the scheduler computes and used to discard
    /// is now queryable: a `decision` stage stalled behind an interrupted
    /// `spec_review` reports exactly that stage as `waiting_on`, and reports
    /// nothing for the sibling `quality_review` dependency it already has.
    #[test]
    fn inspect_reports_waiting_on_for_a_stage_stalled_on_an_unfinished_dependency() {
        let fixture = Fixture::new();
        let scenario = FakeScenario::new()
            .stage("architecture")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("implementation")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("simplification")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("verify")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("quality_review")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("spec_review")
            .events([
                FakeEvent::Started,
                FakeEvent::Interrupted,
                FakeEvent::Completed,
            ])
            .stage("decision")
            .events([FakeEvent::Started, FakeEvent::Completed]);
        let interrupted = fixture
            .scripted_service(scenario)
            .start_run(
                WorkflowKind::Standard,
                "waiting reason for a stalled decision",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();

        let decision = interrupted
            .details
            .stages
            .iter()
            .find(|stage| stage.id == StageId::new("decision").unwrap())
            .expect("decision stage exists");
        assert_eq!(decision.status, StageStatus::Pending);
        let waiting = decision
            .waiting
            .as_ref()
            .expect("a pending stage reports why it is not running");
        assert_eq!(
            waiting
                .waiting_on
                .iter()
                .map(|dependency| dependency.id.to_string())
                .collect::<Vec<_>>(),
            vec!["spec_review".to_owned()],
            "quality_review already completed, so only spec_review is outstanding"
        );
        assert!(waiting.blocked_by.is_empty());
        assert!(waiting.degraded.is_empty());

        // Every other stage has either finished or has nothing left to wait
        // on, so none of them carries a waiting summary.
        for stage in &interrupted.details.stages {
            if stage.id != StageId::new("decision").unwrap() {
                assert!(
                    stage.waiting.is_none(),
                    "stage {} unexpectedly carries a waiting summary",
                    stage.id
                );
            }
        }
    }

    /// `waiting: Some(..)` must mean "this stage is actually waiting on
    /// something" — never "this stage is merely Pending or Ready". A stage
    /// requesting attention halts the *whole run* (unlike an interrupted
    /// stage, which only blocks its own branch — the engine keeps advancing
    /// every other stage it can), so parking `quality_review` on
    /// `NeedsUser` catches `spec_review` at exactly the moment its own
    /// required dependencies (architecture, implementation, simplification)
    /// are all satisfied but the scheduler has only gotten around to marking
    /// it `Ready`, never started: `inspect_run` must report `waiting: None`.
    #[test]
    fn inspect_reports_no_waiting_summary_for_a_ready_stage_with_satisfied_dependencies() {
        let fixture = Fixture::new();
        let scenario = FakeScenario::new()
            .stage("architecture")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("implementation")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("simplification")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("quality_review")
            .events([
                FakeEvent::Started,
                FakeEvent::needs_user(AttentionKind::Decision, "Approve the quality pass"),
                FakeEvent::Completed,
            ])
            .stage("spec_review")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("decision")
            .events([FakeEvent::Started, FakeEvent::Completed]);
        let blocked = fixture
            .scripted_service(scenario)
            .start_run(
                WorkflowKind::Standard,
                "ready stage needs no waiting summary",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(blocked.details.status, RunStatus::NeedsUser);

        let quality_review = blocked
            .details
            .stages
            .iter()
            .find(|stage| stage.id == StageId::new("quality_review").unwrap())
            .expect("quality_review stage exists");
        assert_eq!(quality_review.status, StageStatus::NeedsUser);

        let spec_review = blocked
            .details
            .stages
            .iter()
            .find(|stage| stage.id == StageId::new("spec_review").unwrap())
            .expect("spec_review stage exists");
        assert_eq!(spec_review.status, StageStatus::Ready);
        assert!(
            spec_review.waiting.is_none(),
            "a ready stage whose dependencies are all satisfied has nothing left to report"
        );
    }

    #[test]
    fn normalized_immutable_task_reaches_every_provider_poll_exactly() {
        let fixture = Fixture::new();
        let factory = RecordingFactory {
            tasks: Arc::default(),
            inner: fixture.fake_factory(),
        };
        let observed = Arc::clone(&factory.tasks);
        let service = RunService::new(fixture.database.clone(), fixture.worktrees.clone(), factory);

        service
            .start_run(
                WorkflowKind::Standard,
                "  α first line\nsecond line  ",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();

        let tasks = observed.lock().unwrap();
        assert!(!tasks.is_empty());
        assert!(tasks.iter().all(|task| task == "α first line\nsecond line"));
    }

    #[test]
    fn needs_user_survives_restart_and_requires_explicit_resolution() {
        let fixture = Fixture::new();
        let scenario = || {
            FakeScenario::new()
                .stage("implementation")
                .events([
                    FakeEvent::Started,
                    FakeEvent::needs_user(AttentionKind::Decision, "Choose API shape"),
                    FakeEvent::progress("Apply choice"),
                    FakeEvent::Completed,
                ])
                .stage("verify")
                .events([FakeEvent::Started, FakeEvent::Completed])
        };
        let blocked = fixture
            .scripted_service(scenario())
            .start_run(
                WorkflowKind::Fast,
                "attention task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(blocked.details.status, RunStatus::NeedsUser);
        let request = blocked.details.attention[0].id;
        let run_id = blocked.details.id;

        let restarted = fixture.scripted_service(scenario());
        let still_blocked = restarted.resume_run(run_id).unwrap();
        assert_eq!(still_blocked.details.status, RunStatus::NeedsUser);
        assert!(still_blocked.committed_events.is_empty());
        let completed = restarted.resolve_attention(run_id, request).unwrap();
        assert_eq!(completed.details.status, RunStatus::Completed);
        assert!(
            completed
                .committed_events
                .iter()
                .any(|event| matches!(&event.kind, DomainEventKind::AttentionResolved { .. }))
        );
    }

    #[test]
    fn resume_recovers_paused_and_interrupted_stages() {
        for suspended in [FakeEvent::Paused, FakeEvent::Interrupted] {
            let fixture = Fixture::new();
            let scenario = || {
                FakeScenario::new()
                    .stage("implementation")
                    .events([FakeEvent::Started, suspended.clone(), FakeEvent::Completed])
                    .stage("verify")
                    .events([FakeEvent::Started, FakeEvent::Completed])
            };
            let blocked = fixture
                .scripted_service(scenario())
                .start_run(
                    WorkflowKind::Fast,
                    "recover task",
                    &fixture.repo,
                    Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                    EffortSetting::NativeDefault,
                    &ImageGenerationPlan::disabled(),
                )
                .unwrap();
            assert_eq!(blocked.details.status, RunStatus::Running);
            let completed = fixture
                .scripted_service(scenario())
                .resume_run(blocked.details.id)
                .unwrap();
            assert_eq!(completed.details.status, RunStatus::Completed);
        }
    }

    /// A run held open by the provider, so Stop has something to interrupt.
    fn delayed_scenario() -> FakeScenario {
        FakeScenario::new().stage("implementation").events([
            FakeEvent::Started,
            FakeEvent::delay("external-ready"),
            FakeEvent::Completed,
        ])
    }

    #[test]
    fn stop_interrupts_execution_and_keeps_everything_the_run_produced() {
        let fixture = Fixture::new();
        let running = fixture
            .scripted_service(delayed_scenario())
            .start_run(
                WorkflowKind::Fast,
                "stoppable task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = running.details.id;
        assert_eq!(running.details.status, RunStatus::Running);
        let workspace_before = {
            let store = SqliteStore::open(&fixture.database).unwrap();
            store.load_workspace(run_id).unwrap().unwrap()
        };

        let stopped = fixture
            .scripted_service(delayed_scenario())
            .stop_run(run_id)
            .unwrap();
        assert_eq!(stopped.details.status, RunStatus::Interrupted);
        assert!(
            stopped
                .details
                .stages
                .iter()
                .all(|stage| stage.status != StageStatus::Running),
            "no stage is left executing"
        );

        // Stop is not disposition: the workspace and its worktree survive.
        let store = SqliteStore::open(&fixture.database).unwrap();
        let workspace_after = store.load_workspace(run_id).unwrap().unwrap();
        assert_eq!(workspace_after.status(), workspace_before.status());
        assert_eq!(
            workspace_after.worktree_path(),
            workspace_before.worktree_path()
        );
        assert!(workspace_after.worktree_path().exists());
    }

    #[test]
    fn a_stopped_run_survives_restart_and_resumes_without_a_new_attempt() {
        let fixture = Fixture::new();
        let started = fixture
            .scripted_service(delayed_scenario())
            .start_run(
                WorkflowKind::Fast,
                "restartable task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = started.details.id;
        fixture
            .scripted_service(delayed_scenario())
            .stop_run(run_id)
            .unwrap();

        // A brand-new service instance stands in for a process restart.
        let after_restart = fixture
            .scripted_service(delayed_scenario())
            .inspect_run(run_id)
            .unwrap();
        assert_eq!(after_restart.status, RunStatus::Interrupted);

        let attempts_before = attempt_count(&fixture, run_id);
        let resumed = fixture
            .scripted_service(delayed_scenario())
            .resume_run(run_id)
            .unwrap();
        assert_eq!(resumed.details.id, run_id, "the same logical run continues");
        // One resume must recover the run AND its stages. Asserting merely
        // "no longer Interrupted" at the run level hid a real defect: the
        // run-level recovery landed while the stage stayed suspended, so the
        // run sat in Running and the user had to issue resume a second time.
        // This scenario blocks on its delay gate, so Running is the correct
        // resting state here — what matters is that no stage is left behind.
        assert_ne!(
            resumed.details.status,
            RunStatus::Interrupted,
            "recovery leaves the interrupted state"
        );
        assert!(
            resumed
                .details
                .stages
                .iter()
                .all(|stage| stage.status != StageStatus::Interrupted),
            "one resume left a stage suspended, forcing a second resume"
        );
        assert_eq!(
            attempt_count(&fixture, run_id),
            attempts_before,
            "stopping never creates a retry attempt"
        );
    }

    /// Stop reconciles through the engine, and the provider adapters decide
    /// for themselves whether a poll should resume, from the persisted session
    /// status against the stage status. Only an observing pass tells them not
    /// to. This is a fence, not a behaviour test: the behaviour is covered by
    /// the Claude adapter's stale-NeedsUser test, but nothing there notices if
    /// this call site quietly goes back to a resuming action.
    #[test]
    fn stop_reconciles_without_ever_authorising_provider_work() {
        // Only the non-test half of this file is the call graph under test.
        let source = include_str!("run_service.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        let stop = code
            .split("fn stop_run_once")
            .nth(1)
            .expect("stop_run_once must exist")
            .split("\n    /// ")
            .next()
            .expect("stop_run_once body");
        assert!(
            stop.contains("ResumeAction::Observe"),
            "stop must drive the engine in observing mode"
        );
        assert!(
            !stop.contains("ResumeAction::Continue") && !stop.contains("ResumeAction::Resume"),
            "stop must never drive with an action that lets an adapter resume"
        );
    }

    /// A provider process that died is a process nothing will ever drive
    /// again, so the run it belonged to has already ended — only the record
    /// still says otherwise. Reading the run is how a user notices it at all,
    /// so the read has to commit that truth rather than report work that
    /// stopped hours ago as still running.
    #[test]
    fn reading_settles_a_run_whose_provider_already_ended() {
        let fixture = Fixture::new();
        let started = fixture
            .scripted_service(delayed_scenario())
            .start_run(
                WorkflowKind::Fast,
                "abandoned task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = started.details.id;
        assert_eq!(started.details.status, RunStatus::Running);

        let details = fixture
            .scripted_service(ended_scenario())
            .abandoning_after(std::time::Duration::ZERO)
            .inspect_run(run_id)
            .unwrap();
        assert_eq!(
            details.status,
            RunStatus::Failed,
            "reading an abandoned run reports what actually happened to it"
        );
        assert!(
            details
                .stages
                .iter()
                .all(|stage| stage.status != StageStatus::Running),
            "no stage is left advertising execution"
        );
    }

    /// The run list is the first thing a user sees, and it lies in exactly the
    /// same way the detail view does.
    #[test]
    fn listing_settles_a_run_whose_provider_already_ended() {
        let fixture = Fixture::new();
        let started = fixture
            .scripted_service(delayed_scenario())
            .start_run(
                WorkflowKind::Fast,
                "abandoned task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = started.details.id;

        let listed = fixture
            .scripted_service(ended_scenario())
            .abandoning_after(std::time::Duration::ZERO)
            .list_runs()
            .unwrap();
        let item = listed
            .iter()
            .find(|item| item.id == run_id)
            .expect("the run must still be listed");
        assert_eq!(item.status, RunStatus::Failed);
    }

    /// A run is normally read while another process is still driving it, and
    /// that driver owns every row a settling read would touch. The idle grace
    /// is what keeps the two apart: a run touched a moment ago is a run
    /// someone else is working on, and reading it must change nothing.
    #[test]
    fn reading_leaves_a_run_another_process_is_still_driving_alone() {
        let fixture = Fixture::new();
        let started = fixture
            .scripted_service(delayed_scenario())
            .start_run(
                WorkflowKind::Fast,
                "driven task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = started.details.id;
        let revision_before = SqliteStore::open(&fixture.database)
            .unwrap()
            .load_run(run_id)
            .unwrap()
            .revision;

        let details = fixture
            .scripted_service(ended_scenario())
            .inspect_run(run_id)
            .unwrap();
        assert_eq!(
            details.status,
            RunStatus::Running,
            "a run inside its idle grace is left to its driver"
        );
        assert_eq!(
            SqliteStore::open(&fixture.database)
                .unwrap()
                .load_run(run_id)
                .unwrap()
                .revision,
            revision_before,
            "reading commits nothing while the grace holds"
        );
    }

    /// Settling on a read is the one place a read touches the engine, and it
    /// has two fences. It must observe, never resume — a read is not a reason
    /// for a provider to start working — and it must refuse to settle a run
    /// whose process is still alive, because that process is a driver whose
    /// commits this read would lose a revision race with. Both are call-site
    /// properties nothing downstream would notice going missing.
    #[test]
    fn reading_settles_without_ever_authorising_provider_work() {
        // Only the non-test half of this file is the call graph under test.
        let source = include_str!("run_service.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        let settle = code
            .split("fn settle_if_abandoned(")
            .nth(1)
            .expect("settle_if_abandoned must exist")
            .split("\n    fn ")
            .next()
            .expect("settle_if_abandoned body");
        assert!(
            settle.contains("ResumeAction::Observe"),
            "a read must drive the engine in observing mode"
        );
        assert!(
            !settle.contains("ResumeAction::Continue") && !settle.contains("ResumeAction::Resume"),
            "a read must never drive with an action that lets an adapter resume"
        );
        assert!(
            settle.contains("is_active"),
            "a read must never settle a run whose process is still alive"
        );
    }

    /// A provider that has already ended: the stage reports its failure the
    /// moment anything polls it again.
    fn ended_scenario() -> FakeScenario {
        FakeScenario::new().stage("implementation").events([
            FakeEvent::Started,
            FakeEvent::failed("provider process ended without a result"),
        ])
    }

    /// Total stage attempts recorded for the run, so a Stop can be shown not
    /// to have created one.
    fn attempt_count(fixture: &Fixture, run_id: RunId) -> usize {
        let store = SqliteStore::open(&fixture.database).unwrap();
        store
            .load_events(run_id)
            .unwrap()
            .iter()
            .filter(|event| {
                matches!(
                    event.event.kind(),
                    crate::domain::DomainEventKind::StageStarted
                )
            })
            .count()
    }

    #[test]
    fn stopping_an_already_stopped_run_is_safe() {
        let fixture = Fixture::new();
        let started = fixture
            .scripted_service(delayed_scenario())
            .start_run(
                WorkflowKind::Fast,
                "idempotent stop",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = started.details.id;
        fixture
            .scripted_service(delayed_scenario())
            .stop_run(run_id)
            .unwrap();
        let again = fixture
            .scripted_service(delayed_scenario())
            .stop_run(run_id)
            .unwrap();
        assert_eq!(again.details.status, RunStatus::Interrupted);
        assert!(
            again.committed_events.is_empty(),
            "a second stop commits nothing"
        );
    }

    #[test]
    fn a_finished_run_cannot_be_stopped_and_stop_never_discards() {
        let fixture = Fixture::new();
        let completed = fixture
            .default_service()
            .start_run(
                WorkflowKind::Fast,
                "finished task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = completed.details.id;
        let error = fixture.default_service().stop_run(run_id).unwrap_err();
        assert!(
            matches!(error, AppError::RunNotStoppable(_, RunStatus::Completed)),
            "{error:?}"
        );
        // The refusal changed nothing, least of all into a disposition.
        let details = fixture.default_service().inspect_run(run_id).unwrap();
        assert_eq!(details.status, RunStatus::Completed);

        fixture.default_service().discard_run(run_id).unwrap();
        let discarded = fixture.default_service().inspect_run(run_id).unwrap();
        assert_eq!(
            discarded.status,
            RunStatus::Discarded,
            "discard semantics are unchanged"
        );
    }

    #[test]
    fn provider_delay_is_reported_without_busy_spin_or_speculative_events() {
        let fixture = Fixture::new();
        let scenario = || {
            FakeScenario::new().stage("implementation").events([
                FakeEvent::Started,
                FakeEvent::delay("external-ready"),
                FakeEvent::Completed,
            ])
        };
        let blocked = fixture
            .scripted_service(scenario())
            .start_run(
                WorkflowKind::Fast,
                "delayed task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert!(matches!(
            blocked.outcome,
            QuiescentState::WaitingForProvider { ref stage_id }
                if stage_id.as_str() == "implementation"
        ));
        let resumed = fixture
            .scripted_service(scenario())
            .resume_run(blocked.details.id)
            .unwrap();
        assert!(matches!(
            resumed.outcome,
            QuiescentState::WaitingForProvider { .. }
        ));
        assert!(resumed.committed_events.is_empty());
    }

    #[test]
    fn restart_continues_after_last_checkpoint_without_replay() {
        let fixture = Fixture::new();
        let factory = GatedFactory {
            released: Arc::default(),
            inner: fixture.fake_factory(),
        };
        let released = Arc::clone(&factory.released);
        let service = RunService::new(
            fixture.database.clone(),
            fixture.worktrees.clone(),
            factory.clone(),
        );
        let blocked = service
            .start_run(
                WorkflowKind::Fast,
                "checkpoint task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert!(matches!(
            blocked.outcome,
            QuiescentState::WaitingForProvider { .. }
        ));
        assert!(blocked.committed_events.iter().any(|event| matches!(
            &event.kind,
            DomainEventKind::ProviderProgress { message, .. }
                if message == "checkpoint before process exit"
        )));
        let run_id = blocked.details.id;
        drop(service);

        released.store(true, Ordering::SeqCst);
        let restarted =
            RunService::new(fixture.database.clone(), fixture.worktrees.clone(), factory);
        let completed = restarted.resume_run(run_id).unwrap();
        assert_eq!(completed.details.status, RunStatus::Completed);
        // The verify stage that follows legitimately starts here; only the
        // resumed implementation must not replay.
        assert!(!completed.committed_events.iter().any(|event| {
            event
                .stage_id
                .as_ref()
                .is_some_and(|id| id.as_str() == "implementation")
                && matches!(
                    &event.kind,
                    DomainEventKind::ProviderStarted { .. }
                        | DomainEventKind::ProviderProgress { .. }
                )
        }));
    }

    /// A failed run's reason, persisted in the committing `ProviderFailed`
    /// event, must survive a fresh read: `inspect_run` on a brand-new service
    /// instance is the same read `senate status` and the TUI both use, so
    /// it is the only path this test exercises.
    #[test]
    fn inspect_surfaces_the_failure_reason_on_the_right_stage_and_run() {
        let fixture = Fixture::new();
        let scenario = || {
            FakeScenario::new().stage("implementation").events([
                FakeEvent::Started,
                FakeEvent::failed("compile failed: missing semicolon"),
            ])
        };
        let failed = fixture
            .scripted_service(scenario())
            .start_run(
                WorkflowKind::Fast,
                "failing task with a reason",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(failed.details.status, RunStatus::Failed);

        // A read on a fresh service instance never touches the provider, so
        // the assertion is against exactly what `senate status` would see.
        let details = fixture
            .default_service()
            .inspect_run(failed.details.id)
            .unwrap();
        assert_eq!(
            details.failure_reason.as_deref(),
            Some("compile failed: missing semicolon"),
            "the run-level reason is the blocking failed stage's"
        );
        let implementation = details
            .stages
            .iter()
            .find(|stage| stage.id.as_str() == "implementation")
            .expect("implementation stage");
        assert_eq!(
            implementation.failure_reason.as_deref(),
            Some("compile failed: missing semicolon")
        );
        assert_eq!(implementation.status, StageStatus::Failed);
        assert!(
            implementation.blocking,
            "a failed leaf stage with no dependents blocks completion on its own"
        );

        // No other stage inherits a reason that was never theirs.
        assert!(
            details
                .stages
                .iter()
                .filter(|stage| stage.id.as_str() != "implementation")
                .all(|stage| stage.failure_reason.is_none()),
            "the reason is attributed to the failed stage only"
        );
    }

    /// The Review workflow's `synthesis` treats `quality_review` and
    /// `spec_review` as optional dependencies: either can fail and synthesis
    /// still runs, degraded. If `quality_review` fails and synthesis then
    /// fails too on its own merits, both stages carry `StageStatus::Failed`
    /// — but only synthesis actually stops the run from completing, because
    /// `decision` requires synthesis and nothing requires `quality_review`.
    /// The run-level reason (and, by extension, the TUI's status sentence
    /// built from it) must name synthesis, never the optional review that
    /// merely happened to fail earlier in workflow order.
    #[test]
    fn inspect_attributes_the_run_level_reason_to_the_blocking_stage_not_the_first_failed_one() {
        let fixture = Fixture::new();
        let scenario = || {
            FakeScenario::new()
                .stage("research")
                .events([FakeEvent::Started, FakeEvent::Completed])
                .stage("quality_review")
                .events([
                    FakeEvent::Started,
                    FakeEvent::failed("quality review: lint runner crashed"),
                ])
                .stage("spec_review")
                .events([FakeEvent::Started, FakeEvent::Completed])
                .stage("synthesis")
                .events([
                    FakeEvent::Started,
                    FakeEvent::failed("synthesis: engineering lead ran out of context"),
                ])
        };
        let failed = fixture
            .scripted_service(scenario())
            .start_run(
                WorkflowKind::Review,
                "optional review fails, then the real blocker does too",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(failed.details.status, RunStatus::Failed);

        let details = fixture
            .default_service()
            .inspect_run(failed.details.id)
            .unwrap();
        let stage = |id: &str| {
            details
                .stages
                .iter()
                .find(|stage| stage.id.as_str() == id)
                .unwrap_or_else(|| panic!("{id} stage"))
        };
        assert_eq!(stage("quality_review").status, StageStatus::Failed);
        assert!(
            !stage("quality_review").blocking,
            "synthesis only tolerates quality_review's failure, so it is not a blocker"
        );
        assert_eq!(stage("synthesis").status, StageStatus::Failed);
        assert!(
            stage("synthesis").blocking,
            "decision requires synthesis, so its failure is what actually stops the run"
        );
        assert_eq!(
            details.failure_reason.as_deref(),
            Some("synthesis: engineering lead ran out of context"),
            "the run-level reason names the blocking stage, not the first failed one"
        );
    }

    /// The failed implementation skipped the verify stage after it; the
    /// retry returns both to pending in one commit, so the implementation
    /// cannot succeed into a run whose verification stays skipped.
    #[test]
    fn failed_run_requires_retry_and_empty_apply_is_successful_no_op() {
        let fixture = Fixture::new();
        let scenario = || {
            FakeScenario::new()
                .stage("implementation")
                .events([FakeEvent::Started, FakeEvent::failed("compile failed")])
                .stage("verify")
                .events([FakeEvent::Started, FakeEvent::Completed])
        };
        let failed = fixture
            .scripted_service(scenario())
            .start_run(
                WorkflowKind::Fast,
                "failing task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(failed.details.status, RunStatus::Failed);
        let resumed = fixture
            .scripted_service(scenario())
            .resume_run(failed.details.id)
            .unwrap();
        assert!(resumed.committed_events.is_empty());
        let retried = fixture
            .scripted_service(scenario())
            .retry_stage(
                failed.details.id,
                &crate::domain::StageId::new("implementation").unwrap(),
                None,
            )
            .unwrap();
        assert_eq!(retried.details.status, RunStatus::Failed);
        for stage in ["implementation", "verify"] {
            assert!(
                retried.committed_events.iter().any(|event| {
                    event
                        .stage_id
                        .as_ref()
                        .is_some_and(|id| id.as_str() == stage)
                        && matches!(&event.kind, DomainEventKind::StageRetryScheduled)
                }),
                "{stage} was returned to pending"
            );
        }
        let verify = retried
            .details
            .stages
            .iter()
            .find(|stage| stage.kind == StageKind::Verify)
            .unwrap();
        assert_eq!(
            verify.status,
            StageStatus::Skipped,
            "skipped again by the second failure, not left over from the first"
        );

        let complete = fixture
            .default_service()
            .start_run(
                WorkflowKind::Fast,
                "empty apply",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let (outcome, applied) = fixture
            .default_service()
            .apply_run(complete.details.id)
            .unwrap();
        assert_eq!(outcome, ApplyOutcome::NoChanges);
        assert_eq!(applied.details.status, RunStatus::Completed);
        let discarded = fixture
            .default_service()
            .discard_run(complete.details.id)
            .unwrap();
        assert_eq!(discarded.details.status, RunStatus::Discarded);
        assert_eq!(
            discarded.details.workspace_status,
            Some(crate::workspace::WorkspaceStatus::Removed)
        );
    }

    /// Deleting a run for good is gated twice — the run must be archived,
    /// and it must not still be running — and once it goes through, nothing
    /// of the run is left to list, load, or delete again.
    #[test]
    fn only_an_archived_finished_run_can_be_deleted_and_then_nothing_remains() {
        let fixture = Fixture::new();
        let service = fixture.default_service();
        let started = service
            .start_run(
                WorkflowKind::Fast,
                "delete me for good",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = started.details.id;

        // Not archived: refused by name, and the run is untouched.
        assert!(matches!(
            service.purge_run(run_id),
            Err(AppError::RunNotArchived(refused)) if refused == run_id
        ));
        assert!(service.inspect_run(run_id).is_ok());

        service.set_run_archived(run_id, true).unwrap();
        let receipt = service.purge_run(run_id).unwrap();

        assert_eq!(receipt.run_id, run_id);
        assert!(service.list_runs().unwrap().is_empty());
        assert!(matches!(
            service.inspect_run(run_id),
            Err(AppError::Store(StoreError::RunNotFound(_)))
        ));
        // Nothing left to delete twice.
        assert!(service.purge_run(run_id).is_err());
    }

    #[test]
    fn apply_moves_only_worktree_delta_after_explicit_command() {
        let fixture = Fixture::new();
        let complete = fixture
            .default_service()
            .start_run(
                WorkflowKind::Fast,
                "apply integration",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = complete.details.id;
        let store = SqliteStore::open(&fixture.database).unwrap();
        let workspace = store.load_workspace(run_id).unwrap().unwrap();
        fs::write(
            workspace.worktree_path().join("README.md"),
            "changed by run\n",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(fixture.repo.join("README.md")).unwrap(),
            "baseline\n"
        );
        drop(store);

        let (outcome, report) = fixture.default_service().apply_run(run_id).unwrap();
        assert_eq!(outcome, ApplyOutcome::Applied);
        assert_eq!(report.details.status, RunStatus::Applied);
        assert_eq!(
            fs::read_to_string(fixture.repo.join("README.md")).unwrap(),
            "changed by run\n"
        );
    }

    /// Applying is the moment a worktree stops being work and starts being a
    /// duplicate checkout nobody will open. Keeping it was costing gigabytes
    /// per applied run; the branch is what carries the work forward, and it
    /// survives.
    #[test]
    fn applying_a_run_returns_its_worktree_and_keeps_its_branch() {
        let fixture = Fixture::new();
        let complete = fixture
            .default_service()
            .start_run(
                WorkflowKind::Fast,
                "reclaim after apply",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = complete.details.id;
        let store = SqliteStore::open(&fixture.database).unwrap();
        let workspace = store.load_workspace(run_id).unwrap().unwrap();
        let worktree_path = workspace.worktree_path().to_path_buf();
        let branch = workspace.branch_name().unwrap().to_owned();
        fs::write(worktree_path.join("README.md"), "changed by run\n").unwrap();
        drop(store);

        let (outcome, report) = fixture.default_service().apply_run(run_id).unwrap();

        assert_eq!(outcome, ApplyOutcome::Applied);
        assert_eq!(report.details.status, RunStatus::Applied);
        assert!(
            !worktree_path.exists(),
            "an applied run's worktree is reclaimed"
        );
        assert!(
            git_output(&fixture.repo, &["branch", "--list", &branch]).contains(&branch),
            "the branch carrying the work is kept"
        );
    }

    /// What the sweep decides, which is the only thing it adds over reclaiming
    /// at the moment a run closes: which runs are finished with their
    /// worktrees. A completed run is not one of them — apply, fix, and continue
    /// all read its worktree, so that checkout is live data rather than a leak.
    #[test]
    fn the_sweep_leaves_a_completed_runs_worktree_alone_and_reclaims_nothing_twice() {
        let fixture = Fixture::new();
        let service = fixture.default_service();
        let start = |task: &str| {
            service
                .start_run(
                    WorkflowKind::Fast,
                    task,
                    &fixture.repo,
                    Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                    EffortSetting::NativeDefault,
                    &ImageGenerationPlan::disabled(),
                )
                .unwrap()
                .details
                .id
        };
        let applied = start("applied run");
        let completed = start("completed run");
        let store = SqliteStore::open(&fixture.database).unwrap();
        let applied_path = store
            .load_workspace(applied)
            .unwrap()
            .unwrap()
            .worktree_path()
            .to_path_buf();
        let completed_path = store
            .load_workspace(completed)
            .unwrap()
            .unwrap()
            .worktree_path()
            .to_path_buf();
        fs::write(applied_path.join("README.md"), "landed\n").unwrap();
        drop(store);
        service.apply_run(applied).unwrap();

        let reclaimed = service.reclaim_closed_workspaces().unwrap();

        assert!(
            completed_path.exists(),
            "a completed run can still be applied, fixed, or continued from its worktree"
        );
        assert!(
            !reclaimed.contains(&completed),
            "the sweep does not touch a run that can still be dispositioned"
        );
        assert!(
            !applied_path.exists(),
            "no closed run is left holding a worktree once the sweep has run"
        );
        assert!(
            reclaimed.is_empty(),
            "a worktree already reclaimed at apply time is not reclaimed again: {reclaimed:?}"
        );
    }

    #[test]
    fn diff_preview_is_bounded_read_only_and_apply_uses_same_delta() {
        let fixture = Fixture::new();
        fs::write(fixture.repo.join("delete-me.txt"), "remove me\n").unwrap();
        git(&fixture.repo, &["add", "delete-me.txt"]);
        git(&fixture.repo, &["commit", "-qm", "add deletion fixture"]);
        let service = fixture.default_service();
        let complete = service
            .start_run(
                WorkflowKind::Fast,
                "preview integration",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = complete.details.id;
        let mut store = SqliteStore::open(&fixture.database).unwrap();
        let workspace = store.load_workspace(run_id).unwrap().unwrap();
        fs::write(
            workspace.worktree_path().join("README.md"),
            "preview change\n",
        )
        .unwrap();
        fs::remove_file(workspace.worktree_path().join("delete-me.txt")).unwrap();
        fs::write(workspace.worktree_path().join("new file.txt"), "new\n").unwrap();
        let source_before = git_output(&fixture.repo, &["status", "--porcelain"]);
        let index_before = git_output(workspace.worktree_path(), &["diff", "--cached"]);
        let worktree_before = git_output(workspace.worktree_path(), &["status", "--porcelain"]);
        let loaded_before = store.load_run(run_id).unwrap();
        let events_before = store.load_events(run_id).unwrap().len();
        let workspace_before = store.load_workspace(run_id).unwrap().unwrap();
        assert!(store.load_apply_operation(run_id).unwrap().is_none());
        drop(store);

        let preview = service.preview_run_diff(run_id).unwrap();
        assert!(!preview.truncated);
        assert!(preview.text.contains("preview change"));
        assert!(
            preview
                .changed_files
                .iter()
                .any(|file| file.path == "README.md")
        );
        assert!(
            preview
                .changed_files
                .iter()
                .any(|file| file.path == "delete-me.txt")
        );
        assert!(
            preview
                .changed_files
                .iter()
                .any(|file| file.path == "new file.txt")
        );

        let mut store = SqliteStore::open(&fixture.database).unwrap();
        let loaded_after = store.load_run(run_id).unwrap();
        let workspace_after = store.load_workspace(run_id).unwrap().unwrap();
        assert_eq!(loaded_after.revision, loaded_before.revision);
        assert_eq!(store.load_events(run_id).unwrap().len(), events_before);
        assert_eq!(workspace_after.status(), workspace_before.status());
        assert_eq!(workspace_after.revision(), workspace_before.revision());
        assert!(store.load_apply_operation(run_id).unwrap().is_none());
        assert_eq!(
            git_output(&fixture.repo, &["status", "--porcelain"]),
            source_before
        );
        assert_eq!(
            git_output(workspace.worktree_path(), &["diff", "--cached"]),
            index_before
        );
        assert_eq!(
            git_output(workspace.worktree_path(), &["status", "--porcelain"]),
            worktree_before
        );
        drop(store);

        let (outcome, report) = service.apply_run(run_id).unwrap();
        assert_eq!(outcome, ApplyOutcome::Applied);
        assert_eq!(report.details.status, RunStatus::Applied);
        assert_eq!(
            fs::read_to_string(fixture.repo.join("README.md")).unwrap(),
            "preview change\n"
        );
        assert!(!fixture.repo.join("delete-me.txt").exists());
        assert_eq!(
            fs::read_to_string(fixture.repo.join("new file.txt")).unwrap(),
            "new\n"
        );
        assert!(git_output(&fixture.repo, &["diff", "--cached"]).is_empty());
    }

    #[test]
    fn diff_preview_truncates_large_workspace_output_before_returning_it() {
        let fixture = Fixture::new();
        let service = fixture.default_service();
        let complete = service
            .start_run(
                WorkflowKind::Fast,
                "large preview",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let store = SqliteStore::open(&fixture.database).unwrap();
        let workspace = store.load_workspace(complete.details.id).unwrap().unwrap();
        fs::write(
            workspace.worktree_path().join("generated.txt"),
            "unique preview line 0123456789\n".repeat(100_000),
        )
        .unwrap();
        drop(store);

        let preview = service.preview_run_diff(complete.details.id).unwrap();
        assert!(preview.truncated);
        assert_eq!(preview.text.len(), DIFF_PREVIEW_LIMIT);
        assert!(preview.total_bytes > u64::try_from(DIFF_PREVIEW_LIMIT).unwrap());
    }

    #[test]
    fn artifact_read_revalidates_hash_and_does_not_mutate_run() {
        let fixture = Fixture::new();
        let service = fixture.default_service();
        let complete = service
            .start_run(
                WorkflowKind::Fast,
                "artifact integration",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = complete.details.id;
        let stage_id = StageId::new("implementation").unwrap();
        let artifact_path = fixture.temp.path().join("implementation.md");
        let bytes = b"# Verified output\n";
        fs::write(&artifact_path, bytes).unwrap();
        let created_at = now();
        let metadata = ArtifactMetadata::new(
            ArtifactId::new(),
            run_id,
            stage_id.clone(),
            ArtifactKind::Implementation,
            Role::Implementer,
            ArtifactStatus::Complete,
            created_at,
        )
        .with_provider(ProviderId::new("fake").unwrap(), None);
        let artifact = ArtifactRecord::new(
            metadata,
            1,
            artifact_path.clone(),
            sha256(bytes),
            u64::try_from(bytes.len()).unwrap(),
            created_at,
        )
        .unwrap();
        let mut store = SqliteStore::open(&fixture.database).unwrap();
        store.insert_artifact(&artifact).unwrap();
        let revision = store.load_run(run_id).unwrap().revision;
        let event_count = store.load_events(run_id).unwrap().len();
        drop(store);

        // The run's own verify stage wrote an artifact too; this one is the
        // implementation's.
        assert_eq!(
            service
                .list_artifacts(run_id)
                .unwrap()
                .iter()
                .filter(|artifact| artifact.kind == ArtifactKind::Implementation)
                .count(),
            1
        );
        let view = service.read_artifact(run_id, &stage_id).unwrap();
        assert_eq!(view.text, "# Verified output\n");
        let mut store = SqliteStore::open(&fixture.database).unwrap();
        assert_eq!(store.load_run(run_id).unwrap().revision, revision);
        assert_eq!(store.load_events(run_id).unwrap().len(), event_count);
        drop(store);

        fs::write(&artifact_path, "tampered\n").unwrap();
        assert!(matches!(
            service.read_artifact(run_id, &stage_id),
            Err(AppError::Store(StoreError::ArtifactIntegrity(path))) if path == artifact_path
        ));
    }

    #[test]
    fn raw_log_tail_does_not_advance_provider_cursor() {
        let fixture = Fixture::new();
        let service = fixture.default_service();
        let complete = service
            .start_run(
                WorkflowKind::Fast,
                "log integration",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = complete.details.id;
        let stage_id = StageId::new("implementation").unwrap();
        let mut store = SqliteStore::open(&fixture.database).unwrap();
        let manager = ProcessManager::new(
            fixture.temp.path().join("processes"),
            TmuxBackend::new("/bin/true"),
        );
        let process = manager
            .prepare(
                &mut store,
                run_id,
                stage_id.clone(),
                1,
                "/bin/true",
                Vec::new(),
                BTreeMap::new(),
            )
            .unwrap();
        let stdout = vec![b'x'; PROCESS_LOG_TAIL_LIMIT + 1_024];
        fs::write(process.spec().stdout_path(), &stdout).unwrap();
        fs::write(process.spec().stderr_path(), b"diagnostic\n").unwrap();
        let stdout_cursor = process.cursor(OutputStream::Stdout);
        let stderr_cursor = process.cursor(OutputStream::Stderr);
        let run_revision = store.load_run(run_id).unwrap().revision;
        let event_count = store.load_events(run_id).unwrap().len();
        drop(store);

        let logs = service.read_process_log_tail(run_id, &stage_id).unwrap();
        assert!(logs.stdout.truncated);
        assert_eq!(logs.stdout.text.len(), PROCESS_LOG_TAIL_LIMIT);
        assert_eq!(logs.stderr.text, "diagnostic\n");

        let mut store = SqliteStore::open(&fixture.database).unwrap();
        let after = store.load_managed_process(process.id()).unwrap();
        assert_eq!(after.cursor(OutputStream::Stdout), stdout_cursor);
        assert_eq!(after.cursor(OutputStream::Stderr), stderr_cursor);
        assert_eq!(store.load_run(run_id).unwrap().revision, run_revision);
        assert_eq!(store.load_events(run_id).unwrap().len(), event_count);
    }

    #[test]
    fn explicit_effort_persists_immutably_and_survives_service_restart() {
        let fixture = Fixture::new();
        let report = fixture
            .default_service()
            .start_run(
                WorkflowKind::Standard,
                "effort persistence",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::HIGH,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let run_id = report.details.id;
        // The verifier runs commands, which have no effort dial; every agent
        // role carries the explicit level.
        let expected = |stage: &crate::app::StageSummary| {
            if stage.role == Role::Verifier {
                EffortSetting::NativeDefault
            } else {
                EffortSetting::HIGH
            }
        };
        for stage in &report.details.stages {
            assert_eq!(stage.requested_effort, expected(stage), "{}", stage.id);
        }
        // Fresh service instance: requested effort reconstructs exactly from
        // the persisted immutable snapshot, not from in-memory state.
        let reloaded = fixture.default_service().inspect_run(run_id).unwrap();
        for stage in &reloaded.stages {
            assert_eq!(stage.requested_effort, expected(stage), "{}", stage.id);
        }
    }

    #[test]
    fn omitted_effort_reports_native_default_for_every_stage() {
        let fixture = Fixture::new();
        let report = fixture
            .default_service()
            .start_run(
                WorkflowKind::Fast,
                "default effort",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        for stage in &report.details.stages {
            assert_eq!(stage.requested_effort, EffortSetting::NativeDefault);
            assert_ne!(stage.requested_effort, EffortSetting::MEDIUM);
        }
    }

    #[test]
    fn missing_provider_is_rejected_before_database_creation() {
        let fixture = Fixture::new();
        let error = fixture
            .default_service()
            .start_run(
                WorkflowKind::Fast,
                "task",
                &fixture.repo,
                None,
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap_err();
        assert!(matches!(error, AppError::NoProductionProvider));
        assert!(!fixture.database.exists());
    }

    #[test]
    fn legacy_runs_remain_inspectable_but_resume_fails_with_precise_boundary() {
        let fixture = Fixture::new();
        let created_at = now();
        let run_id = RunId::from_u128(9_000_001);
        let config_id = ConfigSnapshotId::new("legacy-config").unwrap();
        let run = Run::new(
            run_id,
            WorkflowDefinition::built_in(WorkflowKind::Fast),
            config_id.clone(),
            created_at,
        );
        let config = ResolvedConfigSnapshot::new(
            config_id,
            1,
            serde_json::json!({"provider": "fake"}),
            created_at,
        )
        .unwrap();
        let event = run.created_event(EventMetadata::new(EventId::new(), created_at));
        let mut store = SqliteStore::open(&fixture.database).unwrap();
        store.create_run(&run, &config, &[event]).unwrap();
        WorkspaceManager::new(&fixture.worktrees)
            .prepare_run_workspace(&mut store, run_id, &fixture.repo)
            .unwrap();
        drop(store);

        let service = fixture.default_service();
        assert_eq!(service.inspect_run(run_id).unwrap().task, None);
        assert!(matches!(
            service.resume_run(run_id),
            Err(AppError::LegacyRunInput(id)) if id == run_id
        ));
    }

    #[test]
    fn unsupported_legacy_execution_config_is_not_guessed() {
        let fixture = Fixture::new();
        let created_at = now();
        let run_id = RunId::from_u128(9_000_002);
        let config_id = ConfigSnapshotId::new("legacy-execution-config").unwrap();
        let run = Run::new(
            run_id,
            WorkflowDefinition::built_in(WorkflowKind::Fast),
            config_id.clone(),
            created_at,
        );
        let input = RunInput::new(run_id, "legacy task", created_at).unwrap();
        let config = ResolvedConfigSnapshot::new(
            config_id,
            1,
            serde_json::json!({"provider": "fake"}),
            created_at,
        )
        .unwrap();
        let event = run.created_event(EventMetadata::new(EventId::new(), created_at));
        let mut store = SqliteStore::open(&fixture.database).unwrap();
        store
            .create_run_with_input(&run, &input, &config, &[event])
            .unwrap();
        WorkspaceManager::new(&fixture.worktrees)
            .prepare_run_workspace(&mut store, run_id, &fixture.repo)
            .unwrap();
        drop(store);

        assert!(matches!(
            fixture.default_service().resume_run(run_id),
            Err(AppError::LegacyExecutionConfig(id)) if id == run_id
        ));
    }

    /// Builds a service whose implementation stage stops for `count`
    /// permission requests before completing.
    fn service_asking_for_permission(
        fixture: &Fixture,
        count: usize,
        kind: AttentionKind,
    ) -> RunService<ScriptedFactory> {
        let mut events = vec![FakeEvent::Started];
        for index in 0..count {
            events.push(FakeEvent::needs_user(kind, format!("request {index}")));
        }
        events.push(FakeEvent::Completed);
        let scenario = FakeScenario::new()
            .stage("implementation")
            .events(events)
            .stage("verify")
            .events([FakeEvent::Started, FakeEvent::Completed]);
        RunService::new(
            fixture.database.clone(),
            fixture.worktrees.clone(),
            ScriptedFactory {
                scenario,
                inner: fixture.fake_factory(),
            },
        )
    }

    fn start_fake_run<F: ProviderFactory>(
        service: &RunService<F>,
        fixture: &Fixture,
    ) -> ExecutionReport {
        service
            .start_run(
                WorkflowKind::Fast,
                "keep going without asking",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap()
    }

    /// The point of arming a run: every later permission request is approved
    /// and driven on, so a run that would have stopped three times finishes
    /// from a single resume.
    #[test]
    fn an_armed_run_approves_its_own_permission_requests() {
        let fixture = Fixture::new();
        let service = service_asking_for_permission(&fixture, 3, AttentionKind::Permission);
        let started = start_fake_run(&service, &fixture);
        let run_id = started.details.id;
        assert_eq!(started.details.status, RunStatus::NeedsUser);

        service.set_run_auto_approve(run_id, true).unwrap();
        let resumed = service.resume_run(run_id).unwrap();

        assert_eq!(resumed.outcome, QuiescentState::Completed);
        assert!(
            resumed.details.attention.is_empty(),
            "nothing left waiting: {:?}",
            resumed.details.attention
        );
        assert!(resumed.details.auto_approve, "the run stays armed");
    }

    /// Arming is not a licence to answer for the operator. A question has no
    /// approval that means anything, so an armed run stops on one exactly as
    /// an unarmed one does.
    #[test]
    fn an_armed_run_still_stops_for_a_question() {
        let fixture = Fixture::new();
        let service = service_asking_for_permission(&fixture, 1, AttentionKind::Question);
        let started = start_fake_run(&service, &fixture);
        let run_id = started.details.id;

        service.set_run_auto_approve(run_id, true).unwrap();
        let resumed = service.resume_run(run_id).unwrap();

        assert_eq!(resumed.details.status, RunStatus::NeedsUser);
        assert_eq!(resumed.details.attention.len(), 1);
        assert_eq!(resumed.details.attention[0].kind, AttentionKind::Question);
    }

    /// A run that keeps asking is a run something is wrong with. Approval
    /// stops after the ceiling and gives the operator back a run they can
    /// see, rather than approving in a loop forever.
    #[test]
    fn automatic_approval_stops_at_its_ceiling_instead_of_looping() {
        let fixture = Fixture::new();
        let service = service_asking_for_permission(
            &fixture,
            MAX_AUTO_APPROVALS + 4,
            AttentionKind::Permission,
        );
        let started = start_fake_run(&service, &fixture);
        let run_id = started.details.id;

        service.set_run_auto_approve(run_id, true).unwrap();
        let resumed = service.resume_run(run_id).unwrap();

        assert_eq!(
            resumed.details.status,
            RunStatus::NeedsUser,
            "the operator gets the run back"
        );
        assert!(!resumed.details.attention.is_empty());
    }

    /// An unarmed run is untouched by any of this: it stops on the first
    /// request and waits, which is still the default for every run.
    #[test]
    fn an_unarmed_run_still_waits_on_the_first_request() {
        let fixture = Fixture::new();
        let service = service_asking_for_permission(&fixture, 2, AttentionKind::Permission);
        let started = start_fake_run(&service, &fixture);

        assert_eq!(started.details.status, RunStatus::NeedsUser);
        assert!(!started.details.auto_approve);
        let resumed = service.resume_run(started.details.id).unwrap();
        assert_eq!(resumed.details.status, RunStatus::NeedsUser);
    }

    /// The image tool's whole lifecycle claim in one place: a PNG the tool
    /// writes during an Implementer stage is an ordinary worktree change.
    /// The source checkout stays byte-identical until apply, the preview
    /// lists it as a binary file, apply installs the exact bytes, a restart
    /// in between regenerates nothing, and discard leaves the source alone.
    #[test]
    #[allow(clippy::too_many_lines, reason = "one lifecycle, asserted end to end")]
    fn a_generated_image_is_an_ordinary_worktree_change_through_apply_and_restart() {
        use crate::image::{FakeImageGenerator, ImageToolCall, ImageToolScope, ImageToolService};

        let fixture = Fixture::new();
        let scenario = FakeScenario::new()
            .stage("implementation")
            .events([
                FakeEvent::Started,
                FakeEvent::needs_user(AttentionKind::Decision, "image placed; continue?"),
                FakeEvent::Completed,
            ])
            .stage("verify")
            .events([FakeEvent::Started, FakeEvent::Completed]);
        let inner = fixture.fake_factory();
        let service = RunService::new(
            fixture.database.clone(),
            fixture.worktrees.clone(),
            ScriptedFactory {
                scenario,
                inner: inner.clone(),
            },
        );
        let head_before = git_output(&fixture.repo, &["rev-parse", "HEAD"]);
        let started = service
            .start_run(
                WorkflowKind::Fast,
                "landing page with hero image",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::implementer_only(),
            )
            .unwrap();
        assert_eq!(started.details.status, RunStatus::NeedsUser);
        let run_id = started.details.id;
        let request = started.details.attention[0].id;
        let mut store = SqliteStore::open(&fixture.database).unwrap();
        let snapshot = store.load_run(run_id).unwrap().config_snapshot;
        assert_eq!(
            snapshot.schema_version(),
            4,
            "authorization is sealed in the snapshot"
        );
        let worktree = store
            .load_workspace(run_id)
            .unwrap()
            .unwrap()
            .worktree_path()
            .to_path_buf();

        // What the broker does when the agent calls the tool mid-stage.
        let generator = Arc::new(FakeImageGenerator::new());
        let tool = ImageToolService::new(
            fixture.temp.path().join("runs"),
            Some(generator.clone()),
            vec![Role::Implementer],
            4,
        );
        let scope = ImageToolScope {
            run_id,
            stage_id: StageId::new("implementation").unwrap(),
            attempt: 1,
            role: Role::Implementer,
            worktree: worktree.clone(),
            database: fixture.database.clone(),
        };
        let success = tool
            .generate(
                &scope,
                &ImageToolCall {
                    prompt: "hero".to_owned(),
                    output_path: "assets/hero.png".to_owned(),
                    size: None,
                    quality: Some("high".to_owned()),
                    transparent_background: None,
                },
            )
            .unwrap();
        let expected = FakeImageGenerator::png_for("hero");
        assert_eq!(
            fs::read(worktree.join("assets/hero.png")).unwrap(),
            expected
        );
        assert!(
            !fixture.repo.join("assets").exists(),
            "the source checkout must not see the image before apply"
        );
        assert_eq!(git_output(&fixture.repo, &["status", "--porcelain"]), "");
        assert_eq!(
            git_output(&fixture.repo, &["rev-parse", "HEAD"]),
            head_before
        );

        // The ordinary preview sees an untracked binary file.
        let preview = service.preview_run_diff(run_id).unwrap();
        assert_eq!(
            preview.changed_files,
            vec![crate::app::ChangedFileSummary {
                path: "assets/hero.png".to_owned(),
                binary: true,
            }]
        );

        // A restarted service resumes without touching the image: same
        // bytes, same single evidence row, no second backend call.
        let restarted = RunService::new(
            fixture.database.clone(),
            fixture.worktrees.clone(),
            ScriptedFactory {
                scenario: FakeScenario::new()
                    .stage("implementation")
                    .events([
                        FakeEvent::Started,
                        FakeEvent::needs_user(AttentionKind::Decision, "image placed; continue?"),
                        FakeEvent::Completed,
                    ])
                    .stage("verify")
                    .events([FakeEvent::Started, FakeEvent::Completed]),
                inner,
            },
        );
        let still_waiting = restarted.resume_run(run_id).unwrap();
        assert_eq!(still_waiting.details.status, RunStatus::NeedsUser);
        let completed = restarted.resolve_attention(run_id, request).unwrap();
        assert_eq!(completed.details.status, RunStatus::Completed);
        assert_eq!(
            fs::read(worktree.join("assets/hero.png")).unwrap(),
            expected
        );
        assert_eq!(generator.requests().len(), 1);
        let records = store.list_image_generations(run_id).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].output_sha256, success.sha256);
        assert_eq!(completed.details.image_generations.len(), 1);
        assert_eq!(
            completed.details.image_generations[0].output_path,
            "assets/hero.png"
        );
        assert_eq!(
            completed.details.image_generations[0].output_sha256,
            success.sha256
        );
        let loaded = store.load_run(run_id).unwrap();
        assert_eq!(
            ImageGenerationPlan::from_snapshot(&loaded.config_snapshot, loaded.run.workflow())
                .unwrap(),
            ImageGenerationPlan::implementer_only(),
            "resume reads authorization from the snapshot, not from flags"
        );

        // Apply installs the exact bytes, unstaged, like any other change.
        let (outcome, applied) = restarted.apply_run(run_id).unwrap();
        assert_eq!(outcome, ApplyOutcome::Applied);
        assert_eq!(applied.details.status, RunStatus::Applied);
        assert_eq!(
            fs::read(fixture.repo.join("assets/hero.png")).unwrap(),
            expected
        );
        assert_eq!(
            git_output(&fixture.repo, &["status", "--porcelain"]).trim(),
            "?? assets/"
        );
        assert_eq!(
            git_output(&fixture.repo, &["rev-parse", "HEAD"]),
            head_before
        );
    }

    /// The discard half of the same claim: the image dies with the worktree
    /// and the source checkout never had it.
    #[test]
    fn discarding_a_run_removes_its_generated_image_with_the_worktree() {
        use crate::image::{FakeImageGenerator, ImageToolCall, ImageToolScope, ImageToolService};

        let fixture = Fixture::new();
        let scenario = FakeScenario::new()
            .stage("implementation")
            .events([
                FakeEvent::Started,
                FakeEvent::needs_user(AttentionKind::Decision, "image placed; continue?"),
                FakeEvent::Completed,
            ])
            .stage("verify")
            .events([FakeEvent::Started, FakeEvent::Completed]);
        let service = fixture.scripted_service(scenario);
        let started = service
            .start_run(
                WorkflowKind::Fast,
                "landing page with hero image",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::implementer_only(),
            )
            .unwrap();
        let run_id = started.details.id;
        let request = started.details.attention[0].id;
        let store = SqliteStore::open(&fixture.database).unwrap();
        let worktree = store
            .load_workspace(run_id)
            .unwrap()
            .unwrap()
            .worktree_path()
            .to_path_buf();
        let tool = ImageToolService::new(
            fixture.temp.path().join("runs"),
            Some(Arc::new(FakeImageGenerator::new())),
            vec![Role::Implementer],
            4,
        );
        tool.generate(
            &ImageToolScope {
                run_id,
                stage_id: StageId::new("implementation").unwrap(),
                attempt: 1,
                role: Role::Implementer,
                worktree: worktree.clone(),
                database: fixture.database.clone(),
            },
            &ImageToolCall {
                prompt: "hero".to_owned(),
                output_path: "assets/hero.png".to_owned(),
                size: None,
                quality: None,
                transparent_background: None,
            },
        )
        .unwrap();
        assert!(worktree.join("assets/hero.png").exists());
        let completed = service.resolve_attention(run_id, request).unwrap();
        assert_eq!(completed.details.status, RunStatus::Completed);

        let discarded = service.discard_run(run_id).unwrap();
        assert_eq!(discarded.details.status, RunStatus::Discarded);
        assert!(
            !worktree.exists(),
            "discard removes the worktree and the image with it"
        );
        assert!(!fixture.repo.join("assets").exists());
        assert_eq!(git_output(&fixture.repo, &["status", "--porcelain"]), "");
        // Evidence outlives the file: the row still says what was generated.
        assert_eq!(store.list_image_generations(run_id).unwrap().len(), 1);
    }

    /// A factory that cannot host the tool refuses the grant before anything
    /// is persisted, and a run without the grant is untouched by its existence.
    #[test]
    fn an_image_grant_is_refused_by_factories_that_cannot_host_it_and_absent_otherwise() {
        let fixture = Fixture::new();
        let recording = RunService::new(
            fixture.database.clone(),
            fixture.worktrees.clone(),
            RecordingFactory {
                tasks: Arc::new(Mutex::new(Vec::new())),
                inner: fixture.fake_factory(),
            },
        );
        let error = recording
            .start_run(
                WorkflowKind::Fast,
                "task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::implementer_only(),
            )
            .unwrap_err();
        assert!(
            matches!(error, AppError::ImageGenerationUnsupported(_)),
            "{error}"
        );
        assert!(
            recording.list_runs().unwrap().is_empty(),
            "nothing persisted"
        );

        let plain = fixture.default_service();
        let report = plain
            .start_run(
                WorkflowKind::Fast,
                "task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(report.details.status, RunStatus::Completed);
        assert!(report.details.image_generations.is_empty());
        let mut store = SqliteStore::open(&fixture.database).unwrap();
        assert_eq!(
            store
                .load_run(report.details.id)
                .unwrap()
                .config_snapshot
                .schema_version(),
            2
        );
    }

    fn git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn sha256(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        use std::fmt::Write as _;

        let mut hash = String::with_capacity(64);
        for byte in Sha256::digest(bytes) {
            write!(hash, "{byte:02x}").expect("writing to String cannot fail");
        }
        hash
    }

    /// Sending a failed stage elsewhere is recorded on the stage and shown
    /// as its configured target from then on; the snapshot's own routing
    /// table never changes.
    #[test]
    fn retry_with_a_route_records_the_override_and_reports_it_as_configured() {
        let fixture = Fixture::new();
        let scenario = || {
            FakeScenario::new()
                .stage("implementation")
                .events([FakeEvent::Started, FakeEvent::failed("compile failed")])
                .stage("verify")
                .events([FakeEvent::Started, FakeEvent::Completed])
        };
        let failed = fixture
            .scripted_service(scenario())
            .start_run(
                WorkflowKind::Fast,
                "failing task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        assert_eq!(failed.details.status, RunStatus::Failed);
        let implementation = crate::domain::StageId::new("implementation").unwrap();
        let retried = fixture
            .scripted_service(scenario())
            .retry_stage(
                failed.details.id,
                &implementation,
                Some(RetryRoute::new(
                    UniformProvider::Fake,
                    Some(crate::domain::ModelId::new("fake-large").unwrap()),
                )),
            )
            .unwrap();
        let overridden = retried
            .committed_events
            .iter()
            .find(|event| matches!(event.kind, DomainEventKind::StageRouteOverridden { .. }))
            .expect("the override is committed with the retry");
        assert_eq!(overridden.stage_id.as_ref(), Some(&implementation));
        assert_eq!(
            overridden.kind,
            DomainEventKind::StageRouteOverridden {
                provider_id: crate::domain::ProviderId::new("fake").unwrap(),
                model_id: Some(crate::domain::ModelId::new("fake-large").unwrap()),
                reason: OPERATOR_OVERRIDE_REASON.to_owned(),
            }
        );
        let stage = retried
            .details
            .stages
            .iter()
            .find(|stage| stage.id == implementation)
            .unwrap();
        assert!(stage.route_overridden);
        assert_eq!(stage.configured_provider, "fake");
        assert_eq!(stage.configured_model.as_deref(), Some("fake-large"));
        let verify = retried
            .details
            .stages
            .iter()
            .find(|stage| stage.kind == StageKind::Verify)
            .unwrap();
        assert!(!verify.route_overridden, "only the retried stage moves");
        assert!(
            retried
                .details
                .routes
                .iter()
                .all(|route| route.configured_model.is_none()),
            "the snapshot's routing table is untouched"
        );
    }

    /// A provider that cannot take the stage is refused before anything is
    /// committed: the stage stays failed on its original route, retryable.
    #[test]
    fn retry_to_an_unavailable_provider_commits_nothing() {
        let fixture = Fixture::new();
        let scenario = || {
            FakeScenario::new()
                .stage("implementation")
                .events([FakeEvent::Started, FakeEvent::failed("compile failed")])
        };
        let failed = fixture
            .scripted_service(scenario())
            .start_run(
                WorkflowKind::Fast,
                "failing task",
                &fixture.repo,
                Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
                EffortSetting::NativeDefault,
                &ImageGenerationPlan::disabled(),
            )
            .unwrap();
        let implementation = crate::domain::StageId::new("implementation").unwrap();
        let error = fixture
            .scripted_service(scenario())
            .retry_stage(
                failed.details.id,
                &implementation,
                Some(RetryRoute::new(UniformProvider::Claude, None)),
            )
            .unwrap_err();
        assert!(matches!(error, AppError::UnsupportedProvider(_)), "{error}");
        let details = fixture
            .scripted_service(scenario())
            .inspect_run(failed.details.id)
            .unwrap();
        assert_eq!(details.status, RunStatus::Failed);
        let stage = details
            .stages
            .iter()
            .find(|stage| stage.id == implementation)
            .unwrap();
        assert_eq!(stage.status, StageStatus::Failed);
        assert!(!stage.route_overridden);
    }
}
