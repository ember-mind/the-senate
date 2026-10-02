use std::fmt::Write as _;
use std::io::Read as _;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use std::collections::BTreeMap;

use crate::domain::{
    ArtifactKind, ArtifactStatus, AttentionKind, AttentionRequestId, AttentionStatus,
    CompletionBlockerReason, DependencyOutcome, DomainEventKind, EffortSetting, NativeModelUsage,
    Role, RunId, RunStatus, StageDependencyReport, StageId, StageKind, StageStatus, WorkflowKind,
};
use crate::process::{ManagedProcessId, OutputStream, ProcessManager, TmuxBackend};
use crate::providers::{InputAccounting, input_accounting};
use crate::store::{RunRevision, SequencedEvent, SqliteStore, StoreError};
use crate::workspace::{WorkspaceMode, WorkspaceStatus};

use super::AppError;
use super::RoutingPlan;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunListItem {
    pub id: RunId,
    pub workflow: WorkflowKind,
    pub status: RunStatus,
    pub task_summary: String,
    pub repository: Option<PathBuf>,
    pub updated_at: DateTime<Utc>,
    /// Archived runs are left out of the default Runs list.
    pub archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageSummary {
    pub id: StageId,
    pub kind: StageKind,
    pub role: Role,
    pub status: StageStatus,
    pub configured_provider: String,
    /// Persisted requested effort from the immutable resource plan.
    /// `NativeDefault` for every pre-effort-policy snapshot.
    pub requested_effort: EffortSetting,
    /// The runtime's own effort value for this stage, verbatim.
    ///
    /// A `NativeDefault` request asks for nothing, so the level the runtime
    /// then chose is knowable only from what it recorded. `None` means it
    /// recorded nothing; it never means "the same as requested".
    pub observed_effort: Option<String>,
    pub configured_model: Option<String>,
    /// `configured_provider`/`configured_model` come from an operator's
    /// retry-time override rather than the role's snapshot route.
    pub route_overridden: bool,
    pub actual_provider: Option<String>,
    pub actual_model: Option<String>,
    pub provider_session_record: Option<String>,
    pub native_session: Option<String>,
    pub provider_session_status: Option<String>,
    pub process_status: Option<String>,
    /// Semantic wall-clock span of the stage's current attempt, folded from
    /// committed `StageStarted` and terminal stage events. A retry restarts
    /// the span, so a running stage reports the attempt in flight. Distinct
    /// from provider latency, which measures native invocations only. Both
    /// stay `None` when a run's events carry no such evidence.
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Why this stage is not running, when it's Pending or Ready. `None` for
    /// every other status, and for a Pending/Ready stage whose dependencies
    /// are all satisfied (the scheduler just hasn't marked it Ready yet).
    pub waiting: Option<StageWaitingSummary>,
    /// Why the stage's current attempt ended in failure, folded from the last
    /// committed `ProviderFailed` event that carried one. Sanitized to one
    /// line and capped for display; see [`sanitize_failure_reason`]. `None`
    /// when the stage never failed, or failed without the runtime reporting
    /// why.
    pub failure_reason: Option<String>,
    /// Whether this stage's failure is one the workflow graph cannot route
    /// around: a failed leaf stage, or a failed stage with a dependent that
    /// requires it rather than merely tolerating its absence. Reuses
    /// [`crate::domain::Run::completion_blockers`], the same notion the
    /// engine checks before letting a run complete. Always `false` for a
    /// stage that has not failed, and for a failed stage every one of whose
    /// dependents treats it as optional — the workflow already planned for
    /// exactly that, and something downstream carries on in degraded mode.
    pub blocking: bool,
}

/// The Codex model a retry is offered when the runtime refused the model the
/// stage ran on. An account signed in through a subscription cannot use every
/// model the Codex CLI can be configured with, so a `model = ...` in
/// `~/.codex/config.toml` the account is not entitled to fails every Codex
/// stage the same way, and a plain retry fails again. Bump it when the subscription line moves.
pub const CODEX_FALLBACK_MODEL: &str = "gpt-5.6-luna";

impl StageSummary {
    /// The model worth retrying this stage on, when it failed because Codex
    /// refused its model. `None` for any other failure, for a stage that did
    /// not run on Codex, and for one already on the fallback, since offering
    /// the model that just failed would only fail again.
    #[must_use]
    pub fn model_fallback(&self) -> Option<&'static str> {
        let provider = self
            .actual_provider
            .as_deref()
            .unwrap_or(&self.configured_provider);
        let refused = self
            .failure_reason
            .as_deref()
            .is_some_and(is_unsupported_model_reason);
        (self.status == StageStatus::Failed
            && provider == "codex"
            && refused
            && self.configured_model.as_deref() != Some(CODEX_FALLBACK_MODEL))
        .then_some(CODEX_FALLBACK_MODEL)
    }
}

/// Codex relays the API's refusal verbatim, as
/// `The 'gpt-6-sol' model is not supported when using Codex with a ChatGPT
/// account.` inside a JSON error body.
fn is_unsupported_model_reason(reason: &str) -> bool {
    reason
        .to_ascii_lowercase()
        .contains("model is not supported")
}

/// One dependency stage referenced from a [`StageWaitingSummary`] bucket,
/// carrying its kind so a caller can render a human title without a second
/// lookup against the run's stage list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageDependencyRef {
    pub id: StageId,
    pub kind: StageKind,
}

/// A required dependency holding a stage back, with the outcome that put it
/// there. `outcome` distinguishes a dependency that failed outright from one
/// that was itself skipped — rendering must say which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedDependencyRef {
    pub id: StageId,
    pub kind: StageKind,
    pub outcome: DependencyOutcome,
}

/// Dependency readiness for one Pending/Ready stage, recomputed read-only
/// from [`crate::domain::Run::stage_dependency_report`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageWaitingSummary {
    /// Dependencies that have not finished yet.
    pub waiting_on: Vec<StageDependencyRef>,
    /// Required dependencies that failed or were skipped; this stage will be
    /// skipped in turn.
    pub blocked_by: Vec<BlockedDependencyRef>,
    /// Optional dependencies that failed or were skipped; this stage will
    /// still run, without them.
    pub degraded: Vec<StageDependencyRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteSummary {
    pub role: Role,
    pub configured_provider: String,
    pub configured_model: Option<String>,
    pub reason: String,
    /// Requested effort from the immutable resource plan, when the sealed
    /// configuration states one for this role. `None` on a v3 snapshot sealed
    /// before fix-cycle routing, whose plan stops at the roles the workflow
    /// started with — a route without an effort cannot be driven.
    pub requested_effort: Option<EffortSetting>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttentionSummary {
    pub id: AttentionRequestId,
    pub stage_id: StageId,
    pub kind: AttentionKind,
    pub summary: String,
}

/// Provider-native usage folded from committed usage events of ONE runtime.
///
/// Units are provider-native. Summaries from different providers must never
/// be added together or compared: see [`InputAccounting`] for why their input
/// totals are not the same quantity. Optional dimensions stay `None` while no
/// event reported them (`None` = unavailable, `Some(0)` = explicitly reported
/// zero).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageSummary {
    pub input_units: u64,
    pub output_units: u64,
    pub cache_read_units: Option<u64>,
    pub cache_write_units: Option<u64>,
    pub reasoning_output_units: Option<u64>,
}

impl UsageSummary {
    fn absorb(
        &mut self,
        input_units: u64,
        output_units: u64,
        cache_read_units: Option<u64>,
        cache_write_units: Option<u64>,
        reasoning_output_units: Option<u64>,
    ) {
        self.input_units = self.input_units.saturating_add(input_units);
        self.output_units = self.output_units.saturating_add(output_units);
        absorb_dimension(&mut self.cache_read_units, cache_read_units);
        absorb_dimension(&mut self.cache_write_units, cache_write_units);
        absorb_dimension(&mut self.reasoning_output_units, reasoning_output_units);
    }
}

/// One runtime's folded usage together with the convention it counts by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderUsage {
    pub provider: String,
    /// `None` when this runtime never declared a convention.
    pub accounting: Option<InputAccounting>,
    pub usage: UsageSummary,
}

impl ProviderUsage {
    /// Input units this runtime processed that its own cache did not serve.
    ///
    /// This is the only input figure that means the same thing for every
    /// runtime, so it is the only one worth reading across a mixed run.
    /// `None` when the runtime declared no convention, or declared a
    /// cache-inclusive one and then reported no cache read to subtract:
    /// absence stays absence and is never rendered as zero.
    #[must_use]
    pub fn uncached_input_units(&self) -> Option<u64> {
        match self.accounting? {
            InputAccounting::CacheExclusive => Some(self.usage.input_units),
            InputAccounting::CacheInclusive => self
                .usage
                .cache_read_units
                .map(|cached| self.usage.input_units.saturating_sub(cached)),
        }
    }

    /// Whether this runtime's reported input total already contains its
    /// reported cache reads. Presentation must not show both as if they were
    /// separate quantities when it does.
    #[must_use]
    pub const fn input_contains_cache_reads(&self) -> bool {
        matches!(self.accounting, Some(InputAccounting::CacheInclusive))
    }
}

/// Run-level usage, folded per provider and never across providers.
///
/// A run routes different roles to different runtimes, and those runtimes do
/// not report the same quantity under the same name. One total for the run
/// would therefore be arithmetic without a referent, so this type does not
/// offer one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunUsage {
    by_provider: BTreeMap<String, UsageSummary>,
}

impl RunUsage {
    fn absorb(&mut self, provider: &str, usage: &UsageSummary) {
        self.by_provider
            .entry(provider.to_owned())
            .or_default()
            .absorb(
                usage.input_units,
                usage.output_units,
                usage.cache_read_units,
                usage.cache_write_units,
                usage.reasoning_output_units,
            );
    }

    /// Per-runtime usage, ordered by provider name for a stable display.
    pub fn providers(&self) -> impl Iterator<Item = ProviderUsage> + '_ {
        self.by_provider
            .iter()
            .map(|(provider, usage)| ProviderUsage {
                provider: provider.clone(),
                accounting: input_accounting(provider),
                usage: *usage,
            })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_provider.is_empty()
    }

    /// Builds the view from per-provider totals that were folded elsewhere.
    #[must_use]
    pub fn from_totals(totals: impl IntoIterator<Item = (String, UsageSummary)>) -> Self {
        Self {
            by_provider: totals.into_iter().collect(),
        }
    }
}

fn absorb_dimension(total: &mut Option<u64>, delta: Option<u64>) {
    if let Some(delta) = delta {
        *total = Some(total.unwrap_or(0).saturating_add(delta));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunDetails {
    pub id: RunId,
    pub task: Option<String>,
    pub workflow: WorkflowKind,
    pub status: RunStatus,
    pub repository: Option<PathBuf>,
    pub workspace_status: Option<WorkspaceStatus>,
    /// Whether this run's workspace can carry changes back to the operator's
    /// checkout. A review is prepared detached and adopts a branch only if it
    /// is later asked to fix what it found.
    pub workspace_mode: Option<WorkspaceMode>,
    pub base_commit: Option<String>,
    pub profile: String,
    pub profile_version: String,
    pub routes: Vec<RouteSummary>,
    pub revision: RunRevision,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub stages: Vec<StageSummary>,
    pub attention: Vec<AttentionSummary>,
    /// Whether the operator armed this run to approve grantable permission
    /// requests on its own.
    pub auto_approve: bool,
    pub usage: RunUsage,
    /// Semantic wall-clock span of the run, folded from committed
    /// `RunStarted` and terminal run events. Resuming does not restart it.
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// The blocking failed stage's [`StageSummary::failure_reason`], carried
    /// up so a run-level view never has to walk `stages` to find it. `None`
    /// unless `status` is `RunStatus::Failed`, and `None` even then when the
    /// failed stage carries no reason.
    pub failure_reason: Option<String>,
    /// Every image the image-generation tool wrote for this run, in order.
    /// Empty for every run that never held the grant.
    pub image_generations: Vec<ImageGenerationSummary>,
    /// The run's latest publish, if it was ever pushed.
    pub publication: Option<Publication>,
}

/// Where a run's delta was last pushed, and whether that push still carries
/// everything the run has produced since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    pub branch: String,
    pub pull_request_url: Option<String>,
    /// A fix or continue cycle, or a rebase, came after the push, so the
    /// remote no longer has the run's latest work.
    pub outdated: bool,
}

/// Folds the latest `RunPublished` out of the log, marking it outdated when
/// any event that changes the delta — a new cycle or a rebase — follows it.
fn publication(events: &[SequencedEvent]) -> Option<Publication> {
    let (index, branch, pull_request_url) =
        events
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, sequenced)| match sequenced.event.kind() {
                DomainEventKind::RunPublished {
                    branch,
                    pull_request_url,
                    ..
                } => Some((index, branch.clone(), pull_request_url.clone())),
                _ => None,
            })?;
    let outdated = events[index + 1..].iter().any(|sequenced| {
        matches!(
            sequenced.event.kind(),
            DomainEventKind::RunFixRequested { .. }
                | DomainEventKind::RunContinueRequested { .. }
                | DomainEventKind::WorkspaceRebased { .. }
        )
    });
    Some(Publication {
        branch,
        pull_request_url,
        outdated,
    })
}

/// One image-generation evidence row as the operator sees it. Says which
/// stage asked, what backend/model answered, where the bytes went and what
/// they hash to. It does not say what the image shows: nobody has looked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageGenerationSummary {
    pub ordinal: u32,
    pub stage_id: StageId,
    pub attempt: u32,
    pub backend: String,
    pub model: String,
    pub output_path: String,
    pub output_sha256: String,
    pub output_size: u64,
    pub completed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedEvent {
    pub sequence: u64,
    pub stage_id: Option<StageId>,
    pub kind: DomainEventKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactSummary {
    pub stage_id: StageId,
    pub kind: ArtifactKind,
    pub status: ArtifactStatus,
    pub attempt: u32,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub content_size: u64,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactView {
    pub summary: ArtifactSummary,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFileSummary {
    pub path: String,
    pub binary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunDiffPreview {
    pub text: String,
    pub changed_files: Vec<ChangedFileSummary>,
    pub total_bytes: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessLogStream {
    pub text: String,
    pub total_bytes: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessLogView {
    pub process_id: ManagedProcessId,
    pub process_status: String,
    pub stdout: ProcessLogStream,
    pub stderr: ProcessLogStream,
}

/// Durable per-stage execution evidence for observability.
///
/// `latency_ms` is provider execution latency: the span from the stage's
/// first committed `ProviderStarted` event to its last committed
/// `ProviderCompleted`/`ProviderFailed` event, across every attempt. It
/// excludes scheduler/queueing delay and is unavailable while no terminal
/// provider event exists. `invocation_count` counts persisted managed native
/// invocations for the stage across attempts. `injected_prompt_bytes` sums
/// the exact stdin bytes The Senate piped into those invocations (initial
/// prompts plus continuations); it measures only The Senate-injected content,
/// never files, project instructions, MCP context, or anything the native
/// runtime read on its own. `native_model_usage` is the runtime-reported
/// per-model breakdown merged by model; it overlaps `usage` and must not be
/// summed with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageExecutionEvidence {
    pub stage_id: StageId,
    pub configured_provider: String,
    pub configured_model: Option<String>,
    /// See [`StageSummary::route_overridden`].
    pub route_overridden: bool,
    pub actual_provider: Option<String>,
    pub confirmed_model: Option<String>,
    /// The runtime's own reasoning-effort value for this stage, verbatim.
    ///
    /// Distinct from [`StageSummary::requested_effort`], which is what
    /// The Senate asked for. A native-default request asks for nothing, so this
    /// is the only place the resulting level is visible at all. `None` means
    /// the runtime never made it observable.
    pub native_effort: Option<String>,
    pub provider_cli_version: Option<String>,
    pub usage: UsageSummary,
    pub native_model_usage: Option<Vec<NativeModelUsage>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub latency_ms: Option<u64>,
    pub invocation_count: u64,
    pub injected_prompt_bytes: Option<u64>,
}

pub(crate) fn list(store: &SqliteStore) -> Result<Vec<RunListItem>, AppError> {
    Ok(store
        .list_runs()?
        .into_iter()
        .map(|run| RunListItem {
            id: run.id,
            workflow: run.workflow,
            status: run.status,
            task_summary: task_summary(run.task.as_deref()),
            repository: run.repository_path.map(PathBuf::from),
            updated_at: run.updated_at,
            archived: run.archived,
        })
        .collect())
}

#[allow(
    clippy::too_many_lines,
    reason = "one query assembly keeps configured and actual stage projections aligned"
)]
pub(crate) fn inspect(store: &mut SqliteStore, run_id: RunId) -> Result<RunDetails, AppError> {
    let loaded = store.load_run(run_id)?;
    let input = store.load_run_input(run_id)?;
    let workspace = store.load_workspace(run_id)?;
    let events = store.load_events(run_id)?;
    let plan = match RoutingPlan::from_snapshot(&loaded.config_snapshot, loaded.run.workflow()) {
        Ok(plan) => Some(plan),
        Err(_) if loaded.config_snapshot.schema_version() == 1 => None,
        Err(error) => return Err(error.into()),
    };
    let resource_plan =
        super::routing::ResourcePlan::from_snapshot(&loaded.config_snapshot, loaded.run.workflow())
            .ok();
    let usage = run_usage(&events);
    let run_span = run_span(&events);
    let sessions = store.list_provider_sessions(run_id)?;
    let mut routes = plan
        .iter()
        .flat_map(RoutingPlan::routes)
        .map(|(role, route)| RouteSummary {
            role,
            configured_provider: route.target().provider_id().to_string(),
            configured_model: route.target().model_id().map(ToString::to_string),
            reason: route.reason().to_owned(),
            requested_effort: resource_plan.as_ref().and_then(|plan| plan.effort(role)),
        })
        .collect::<Vec<_>>();
    routes.sort_by_key(|route| role_order(route.role));
    // The stages, if any, whose failure the workflow graph cannot route
    // around — reused per stage below so a failed-but-optional dependency
    // (degraded-tolerant, like a review the workflow can complete without)
    // never gets mistaken for the failure that actually stopped the run.
    let blocking_failed_stages: std::collections::HashSet<StageId> = loaded
        .run
        .completion_blockers()
        .into_iter()
        .filter(|blocker| {
            matches!(
                blocker.reason,
                CompletionBlockerReason::FailedLeafStage
                    | CompletionBlockerReason::RequiredStageFailed
            )
        })
        .map(|blocker| blocker.stage_id)
        .collect();
    let mut stages = Vec::new();
    for stage in loaded.run.stages() {
        let (started_at, finished_at) = stage_span(&events, stage.id());
        let route = plan.as_ref().and_then(|plan| plan.route(stage.role()));
        let configured = configured_target(stage, route);
        let session = sessions
            .iter()
            .filter(|session| session.stage_id() == stage.id())
            .max_by_key(|session| session.attempt());
        let started = events.iter().rev().find_map(|event| {
            if event.event.stage_id() != Some(stage.id()) {
                return None;
            }
            match event.event.kind() {
                DomainEventKind::ProviderStarted {
                    provider_id,
                    model_id,
                    ..
                } => Some((
                    provider_id.to_string(),
                    model_id.as_ref().map(ToString::to_string),
                )),
                _ => None,
            }
        });
        let observed = events.iter().rev().find_map(|event| {
            if event.event.stage_id() != Some(stage.id()) {
                return None;
            }
            match event.event.kind() {
                DomainEventKind::ProviderRuntimeObserved {
                    model_id,
                    native_effort,
                    ..
                } => Some((
                    model_id.as_ref().map(ToString::to_string),
                    native_effort.clone(),
                )),
                _ => None,
            }
        });
        let process_status = session
            .and_then(crate::providers::ProviderSessionRecord::current_process_id)
            .map(|process_id| store.load_managed_process(process_id))
            .transpose()?
            .map(|process| process.status().as_str().to_owned());
        let waiting = loaded
            .run
            .stage_dependency_report(stage.id())
            .map(|report| waiting_summary(&loaded.run, report));
        let failure_reason = stage_failure_reason(&events, stage.id());
        let blocking = blocking_failed_stages.contains(stage.id());
        stages.push(StageSummary {
            id: stage.id().clone(),
            kind: stage.kind(),
            role: stage.role(),
            status: stage.status(),
            configured_provider: configured
                .provider
                .unwrap_or_else(|| "unavailable".to_owned()),
            requested_effort: resource_plan
                .as_ref()
                .and_then(|plan| plan.effort(stage.role()))
                .unwrap_or_default(),
            configured_model: configured.model,
            route_overridden: configured.overridden,
            actual_provider: session
                .map(|session| session.provider_id().to_string())
                .or_else(|| started.as_ref().map(|(provider, _)| provider.clone())),
            observed_effort: observed.as_ref().and_then(|(_, effort)| effort.clone()),
            actual_model: observed
                .and_then(|(model, _)| model)
                .or_else(|| {
                    session
                        .and_then(crate::providers::ProviderSessionRecord::model_id)
                        .map(ToString::to_string)
                })
                .or_else(|| started.and_then(|(_, model)| model)),
            provider_session_record: session.map(|session| session.id().to_string()),
            native_session: session
                .and_then(crate::providers::ProviderSessionRecord::native_session_id)
                .map(ToString::to_string),
            provider_session_status: session.map(|session| session.status().as_str().to_owned()),
            process_status,
            started_at,
            finished_at,
            waiting,
            failure_reason,
            blocking,
        });
    }
    // The run's own reason is the *blocking* failed stage's — never merely
    // the first failed stage in workflow order, which can be a degraded,
    // non-blocking optional dependency that failed on the way to a
    // completion-blocking failure somewhere else. A caller showing run-level
    // status never has to walk `stages` looking for the right one.
    let failure_reason = (loaded.run.status() == RunStatus::Failed)
        .then(|| {
            stages
                .iter()
                .find(|stage| stage.status == StageStatus::Failed && stage.blocking)
                .and_then(|stage| stage.failure_reason.clone())
        })
        .flatten();
    let auto_approve = store.run_auto_approve(run_id)?;
    Ok(RunDetails {
        publication: publication(&events),
        id: loaded.run.id(),
        auto_approve,
        task: input.map(|input| input.task().to_owned()),
        workflow: loaded.run.workflow_kind(),
        status: loaded.run.status(),
        repository: workspace
            .as_ref()
            .map(|workspace| workspace.source_repo_path().to_path_buf()),
        workspace_mode: workspace.as_ref().map(crate::workspace::RunWorkspace::mode),
        workspace_status: workspace
            .as_ref()
            .map(crate::workspace::RunWorkspace::status),
        base_commit: workspace
            .as_ref()
            .map(|workspace| workspace.base_commit().to_owned()),
        profile: plan.as_ref().map_or_else(
            || {
                loaded
                    .config_snapshot
                    .payload()
                    .get("profile")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unavailable")
                    .to_owned()
            },
            |plan| plan.profile().to_owned(),
        ),
        profile_version: plan.as_ref().map_or_else(
            || "unavailable".to_owned(),
            |plan| plan.profile_version().to_owned(),
        ),
        routes,
        revision: loaded.revision,
        created_at: *loaded.run.created_at(),
        updated_at: *loaded.run.updated_at(),
        stages,
        attention: loaded
            .run
            .attention_requests()
            .iter()
            .filter(|request| request.status() == &AttentionStatus::Pending)
            .map(|request| AttentionSummary {
                id: request.id(),
                stage_id: request.stage_id().clone(),
                kind: request.kind(),
                summary: request.summary().to_owned(),
            })
            .collect(),
        usage,
        started_at: run_span.0,
        finished_at: run_span.1,
        failure_reason,
        image_generations: store
            .list_image_generations(run_id)?
            .into_iter()
            .map(|record| ImageGenerationSummary {
                ordinal: record.ordinal,
                stage_id: record.stage_id,
                attempt: record.attempt,
                backend: record.backend,
                model: record.model,
                output_path: record.output_path,
                output_sha256: record.output_sha256,
                output_size: record.output_size,
                completed_at: record.completed_at,
            })
            .collect(),
    })
}

/// One display line: whitespace/newlines collapsed to single spaces, every
/// remaining control character replaced with a visible placeholder, and
/// capped at [`FAILURE_REASON_LIMIT`] characters with an ellipsis.
///
/// Persisted reasons are free text from provider stderr or an exit message:
/// untrimmed, possibly multi-line, unbounded in length, and never sanitized
/// upstream. That text flows verbatim into a TUI span and CLI stdout, so
/// beyond whitespace collapse this also strips every C0/C1 control character
/// (ESC, BEL, raw ANSI escape sequences, …) — left alone, a garbled or
/// hostile reason could move the cursor or repaint the operator's terminal.
/// Every surface that shows a reason wants the same short, single-line, safe
/// form, so the fold produces it once here instead of each caller
/// re-deriving it. `None` only for a reason that is empty or all whitespace.
const FAILURE_REASON_LIMIT: usize = 200;

/// Whether a character must never reach a terminal buffer unescaped.
/// `char::is_control()` covers the C0 range (including ESC and BEL) and C1,
/// but misses DEL and the zero-width family some terminals still act on —
/// the same extra guard the TUI's own `viewer_line` sanitizer uses for diff
/// and log content.
fn is_terminal_unsafe(character: char) -> bool {
    character.is_control() || matches!(character, '\u{7f}'..='\u{9f}' | '\u{200b}'..='\u{200f}')
}

/// Single bounded pass, deliberately: a persisted reason is untrusted,
/// unbounded-length free text (a garbled or hostile provider record can run
/// to tens of megabytes), and the naive approach — collapse the whole thing,
/// escape the whole collapsed copy, then take a prefix — allocates multiple
/// full-length copies before ever looking at the 200-character budget. This
/// instead builds the output incrementally and stops consuming the input the
/// moment the budget (plus room for the ellipsis) is spent, so memory use
/// stays a small constant regardless of how long `reason` is.
fn sanitize_failure_reason(reason: &str) -> Option<String> {
    let mut output = String::new();
    let mut len = 0usize;
    let mut pending_space = false;
    let mut truncated = false;
    for character in reason.chars() {
        if character.is_whitespace() {
            // Leading whitespace never becomes a visible leading space.
            if len > 0 {
                pending_space = true;
            }
            continue;
        }
        // The pending collapsed space and the character itself both need to
        // fit inside the budget, or neither is written this iteration.
        let needed = if pending_space { 2 } else { 1 };
        if len + needed > FAILURE_REASON_LIMIT {
            truncated = true;
            break;
        }
        if pending_space {
            output.push(' ');
            len += 1;
            pending_space = false;
        }
        output.push(if is_terminal_unsafe(character) {
            '\u{fffd}'
        } else {
            character
        });
        len += 1;
    }
    if output.is_empty() {
        return None;
    }
    if truncated {
        output.push('…');
    }
    Some(output)
}

/// Folds the run's semantic span from committed lifecycle events. Resuming
/// keeps the original start; the terminal event closes the span.
fn run_span(events: &[SequencedEvent]) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let mut span = (None, None);
    for event in events {
        match event.event.kind() {
            DomainEventKind::RunStarted => {
                span.0.get_or_insert(*event.event.occurred_at());
            }
            DomainEventKind::RunCompleted | DomainEventKind::RunFailed => {
                span.1 = Some(*event.event.occurred_at());
            }
            _ => {}
        }
    }
    span
}

/// Resolves each bucket of a [`StageDependencyReport`] to the dependency
/// stage's kind, so a caller can render a human title without a further
/// lookup against the run's stage list.
fn waiting_summary(run: &crate::domain::Run, report: StageDependencyReport) -> StageWaitingSummary {
    let resolve = |ids: Vec<StageId>| {
        ids.into_iter()
            .filter_map(|id| {
                run.stage(&id).map(|dependency| StageDependencyRef {
                    kind: dependency.kind(),
                    id,
                })
            })
            .collect::<Vec<_>>()
    };
    let resolve_blocked = |blocked: Vec<crate::domain::BlockedDependency>| {
        blocked
            .into_iter()
            .filter_map(|blocked| {
                run.stage(&blocked.stage_id)
                    .map(|dependency| BlockedDependencyRef {
                        kind: dependency.kind(),
                        id: blocked.stage_id,
                        outcome: blocked.outcome,
                    })
            })
            .collect::<Vec<_>>()
    };
    StageWaitingSummary {
        waiting_on: resolve(report.waiting_on),
        blocked_by: resolve_blocked(report.blocked_by),
        degraded: resolve(report.degraded),
    }
}

/// Folds one stage's semantic span from committed lifecycle events. Each
/// `StageStarted` opens a fresh span, so a retried stage reports its current
/// attempt rather than the total across attempts.
fn stage_span(
    events: &[SequencedEvent],
    stage_id: &StageId,
) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let mut span = (None, None);
    for event in events
        .iter()
        .filter(|event| event.event.stage_id() == Some(stage_id))
    {
        match event.event.kind() {
            DomainEventKind::StageStarted => span = (Some(*event.event.occurred_at()), None),
            DomainEventKind::StageCompleted
            | DomainEventKind::StageFailed
            | DomainEventKind::StageSkipped => span.1 = Some(*event.event.occurred_at()),
            // Returned to pending, with or without ever having run.
            DomainEventKind::StageRetryScheduled => span = (None, None),
            _ => {}
        }
    }
    span
}

/// Folds one stage's failure reason from its committed `ProviderFailed`
/// events, keyed by the event envelope's own `stage_id` — the same
/// attribution [`stage_execution_evidence`] uses, so a stage that ran
/// somewhere other than where it was routed is still charged the reason it
/// actually reported.
///
/// Scoped to the current attempt only: `StageStarted` and
/// `StageRetryScheduled` both close out whatever the previous attempt
/// reported, matching the attempt boundary the scheduler's own checkpoint
/// fold uses (`StageRetryScheduled` advances `attempt` in
/// `engine::scheduler::reduce_checkpoint`). Without this, a retried stage
/// that is still pending — or has since succeeded — would go on reporting
/// the failure of the attempt it already left behind. Within one attempt the
/// *last* reason wins, and a terminal `ProviderFailed` with no reason never
/// erases an earlier one that had it.
fn stage_failure_reason(events: &[SequencedEvent], stage_id: &StageId) -> Option<String> {
    let mut reason = None;
    for event in events
        .iter()
        .filter(|event| event.event.stage_id() == Some(stage_id))
    {
        match event.event.kind() {
            DomainEventKind::StageStarted | DomainEventKind::StageRetryScheduled => {
                reason = None;
            }
            DomainEventKind::ProviderFailed {
                reason: Some(new_reason),
                ..
            } => {
                if let Some(sanitized) = sanitize_failure_reason(new_reason) {
                    reason = Some(sanitized);
                }
            }
            _ => {}
        }
    }
    reason
}

pub(crate) fn list_artifacts(
    store: &SqliteStore,
    run_id: RunId,
) -> Result<Vec<ArtifactSummary>, AppError> {
    Ok(store
        .list_artifacts(run_id)?
        .into_iter()
        .map(|artifact| artifact_summary(&artifact))
        .collect())
}

pub(crate) fn read_artifact(
    store: &SqliteStore,
    run_id: RunId,
    stage_id: &StageId,
) -> Result<ArtifactView, AppError> {
    let artifact = store
        .list_artifacts(run_id)?
        .into_iter()
        .filter(|artifact| artifact.metadata().stage_id() == stage_id)
        .max_by_key(crate::providers::ArtifactRecord::attempt)
        .ok_or_else(|| AppError::ArtifactNotFound {
            run_id,
            stage_id: stage_id.clone(),
        })?;
    let file = std::fs::File::open(artifact.path()).map_err(StoreError::Io)?;
    if file.metadata().map_err(StoreError::Io)?.len() != artifact.content_size() {
        return Err(StoreError::ArtifactIntegrity(artifact.path().to_path_buf()).into());
    }
    let mut bytes = Vec::new();
    file.take(artifact.content_size().saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(StoreError::Io)?;
    let size_matches = u64::try_from(bytes.len()) == Ok(artifact.content_size());
    let mut hash = String::with_capacity(64);
    for byte in Sha256::digest(&bytes) {
        write!(hash, "{byte:02x}").expect("writing to String cannot fail");
    }
    if !size_matches || hash != artifact.content_hash() {
        return Err(StoreError::ArtifactIntegrity(artifact.path().to_path_buf()).into());
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| StoreError::ArtifactIntegrity(artifact.path().to_path_buf()))?;
    Ok(ArtifactView {
        summary: artifact_summary(&artifact),
        text,
    })
}

/// The pull request the newest editing stage that wrote one drafted for the
/// run's change. Fix and follow-up stages restate it, so the most recent
/// editing artifact — by creation time, then attempt — describes what the
/// branch carries now; when that one ignored the contract, an older editing
/// artifact that honored it still describes the change far better than the
/// task text does, so the search walks back rather than giving up on the
/// first artifact without a section. Read through the same
/// integrity-verified path as opening the artifact.
///
/// # Errors
/// Returns store errors and artifact integrity failures; a corrupt artifact
/// fails closed here as everywhere, never silently publishes from the task.
pub(crate) fn pull_request_draft(
    store: &SqliteStore,
    run_id: RunId,
) -> Result<Option<crate::workspace::PullRequestDraft>, AppError> {
    let artifacts = store.list_artifacts(run_id)?;
    for artifact in editing_artifacts_newest_first(&artifacts) {
        let view = read_artifact(store, run_id, artifact.metadata().stage_id())?;
        if let Some(draft) = crate::workspace::extract_pull_request_draft(&view.text) {
            return Ok(Some(draft));
        }
    }
    Ok(None)
}

/// The complete artifacts of stages that edit the workspace, newest first by
/// creation time, then attempt.
fn editing_artifacts_newest_first(
    artifacts: &[crate::providers::ArtifactRecord],
) -> Vec<&crate::providers::ArtifactRecord> {
    let mut editing: Vec<_> = artifacts
        .iter()
        .filter(|artifact| {
            artifact.metadata().status() == ArtifactStatus::Complete
                && matches!(
                    artifact.metadata().kind(),
                    ArtifactKind::Implementation
                        | ArtifactKind::Simplification
                        | ArtifactKind::Fix
                        | ArtifactKind::FollowUp
                )
        })
        .collect();
    editing.sort_by_key(|artifact| {
        std::cmp::Reverse((*artifact.metadata().created_at(), artifact.attempt()))
    });
    editing
}

pub(crate) fn process_log_tail(
    store: &SqliteStore,
    manager: &ProcessManager<TmuxBackend>,
    run_id: RunId,
    stage_id: &StageId,
    max_bytes: usize,
) -> Result<ProcessLogView, AppError> {
    let process = store
        .list_managed_processes(run_id)?
        .into_iter()
        .filter(|process| process.stage_id() == stage_id)
        .max_by_key(|process| {
            (
                process.attempt(),
                process.invocation(),
                *process.created_at(),
            )
        })
        .ok_or_else(|| AppError::ProcessLogNotFound {
            run_id,
            stage_id: stage_id.clone(),
        })?;
    let (stdout, stdout_total, stdout_truncated) =
        manager.read_output_tail(store, process.id(), OutputStream::Stdout, max_bytes)?;
    let (stderr, stderr_total, stderr_truncated) =
        manager.read_output_tail(store, process.id(), OutputStream::Stderr, max_bytes)?;
    Ok(ProcessLogView {
        process_id: process.id(),
        process_status: process.status().as_str().to_owned(),
        stdout: ProcessLogStream {
            text: String::from_utf8_lossy(stdout.bytes()).into_owned(),
            total_bytes: stdout_total,
            truncated: stdout_truncated,
        },
        stderr: ProcessLogStream {
            text: String::from_utf8_lossy(stderr.bytes()).into_owned(),
            total_bytes: stderr_total,
            truncated: stderr_truncated,
        },
    })
}

/// Counts persisted native invocations for one stage and sums the exact
/// stdin bytes The Senate piped into them (initial prompt plus continuations).
///
/// The stdin file holds the exact immutable bytes piped into the native CLI
/// (SHA-256 verified at spawn); its length IS the injected prompt size.
/// Missing files or stdin-less invocations leave the sum unavailable rather
/// than pretending zero.
/// Merges runtime-reported per-model breakdown entries by model name.
///
/// The merged view stays separate from the aggregate usage fold: the two
/// views overlap (the breakdown spans subagent models) and must never be
/// summed together.
fn merge_native_models<'entry>(
    merged: &mut BTreeMap<String, NativeModelUsage>,
    entries: impl Iterator<Item = &'entry NativeModelUsage>,
) {
    for entry in entries {
        let model = merged
            .entry(entry.model.clone())
            .or_insert_with(|| NativeModelUsage {
                model: entry.model.clone(),
                input_units: 0,
                output_units: 0,
                cache_read_units: None,
                cache_write_units: None,
            });
        model.input_units = model.input_units.saturating_add(entry.input_units);
        model.output_units = model.output_units.saturating_add(entry.output_units);
        absorb_dimension(&mut model.cache_read_units, entry.cache_read_units);
        absorb_dimension(&mut model.cache_write_units, entry.cache_write_units);
    }
}

/// Clamped span between the stage's first `ProviderStarted` and last terminal
/// provider event. Same definition as eval `duration_ms`; committed event
/// timestamps are monotone per stage, so the clamp is defensive only.
fn provider_latency_ms((start, finish): (DateTime<Utc>, DateTime<Utc>)) -> u64 {
    u64::try_from(
        finish
            .signed_duration_since(start)
            .num_milliseconds()
            .max(0),
    )
    .unwrap_or(u64::MAX)
}

fn invocation_telemetry(
    store: &SqliteStore,
    run_id: RunId,
    stage_id: &StageId,
) -> Result<(u64, Option<u64>), AppError> {
    let mut invocation_count = 0_u64;
    let mut injected_prompt_bytes: Option<u64> = None;
    for process in store.list_managed_processes(run_id)? {
        if process.stage_id() != stage_id {
            continue;
        }
        invocation_count += 1;
        if let Some(path) = process.spec().stdin_path()
            && let Ok(metadata) = std::fs::metadata(path)
        {
            absorb_dimension(&mut injected_prompt_bytes, Some(metadata.len()));
        }
    }
    Ok((invocation_count, injected_prompt_bytes))
}

#[allow(
    clippy::too_many_lines,
    reason = "one fold keeps every per-stage evidence dimension read from the same event pass"
)]
pub(crate) fn stage_execution_evidence(
    store: &mut SqliteStore,
    run_id: RunId,
    stage_id: &StageId,
) -> Result<StageExecutionEvidence, AppError> {
    let loaded = store.load_run(run_id)?;
    let stage = loaded
        .run
        .stage(stage_id)
        .ok_or_else(|| AppError::StageNotFound {
            run_id,
            stage_id: stage_id.clone(),
        })?;
    let plan = RoutingPlan::from_snapshot(&loaded.config_snapshot, loaded.run.workflow())?;
    let route = plan
        .route(stage.role())
        .ok_or(super::RoutingError::MissingRoleRoute(stage.role()))?;
    let configured = configured_target(stage, Some(route));
    let events = store.load_events(run_id)?;
    let mut observed_model: Option<String> = None;
    let mut native_effort: Option<String> = None;
    let mut usage = UsageSummary::default();
    let mut native_models: BTreeMap<String, NativeModelUsage> = BTreeMap::new();
    let mut started_at = None;
    let mut finished_at = None;
    let mut started_target = None;
    for event in events
        .iter()
        .filter(|event| event.event.stage_id() == Some(stage_id))
    {
        match event.event.kind() {
            DomainEventKind::ProviderStarted {
                provider_id,
                model_id,
                ..
            } => {
                started_at.get_or_insert(*event.event.occurred_at());
                started_target = Some((
                    provider_id.to_string(),
                    model_id.as_ref().map(ToString::to_string),
                ));
            }
            DomainEventKind::ProviderUsageUpdated {
                input_units,
                output_units,
                cache_read_units,
                cache_write_units,
                reasoning_output_units,
                native_models: event_models,
                ..
            } => {
                usage.absorb(
                    *input_units,
                    *output_units,
                    *cache_read_units,
                    *cache_write_units,
                    *reasoning_output_units,
                );
                merge_native_models(&mut native_models, event_models.iter().flatten());
            }
            DomainEventKind::ProviderRuntimeObserved {
                model_id,
                native_effort: effort,
                ..
            } => {
                observed_model = model_id
                    .as_ref()
                    .map(ToString::to_string)
                    .or(observed_model);
                native_effort = effort.clone().or(native_effort);
            }
            DomainEventKind::ProviderCompleted { .. } | DomainEventKind::ProviderFailed { .. } => {
                finished_at = Some(*event.event.occurred_at());
            }
            _ => {}
        }
    }
    let latency_ms = started_at.zip(finished_at).map(provider_latency_ms);
    let (invocation_count, injected_prompt_bytes) = invocation_telemetry(store, run_id, stage_id)?;
    let session = store
        .list_provider_sessions(run_id)?
        .into_iter()
        .filter(|session| session.stage_id() == stage_id)
        .max_by_key(crate::providers::ProviderSessionRecord::attempt);
    Ok(StageExecutionEvidence {
        stage_id: stage_id.clone(),
        configured_provider: configured
            .provider
            .expect("a resolved role route always names a provider"),
        configured_model: configured.model,
        route_overridden: configured.overridden,
        actual_provider: session
            .as_ref()
            .map(|session| session.provider_id().to_string())
            .or_else(|| {
                started_target
                    .as_ref()
                    .map(|(provider, _)| provider.clone())
            }),
        // What the runtime's own records said it ran outranks what it
        // announced at launch, and both outrank silence. Nothing here ever
        // falls back to the configured model.
        confirmed_model: observed_model
            .or_else(|| {
                session
                    .as_ref()
                    .and_then(crate::providers::ProviderSessionRecord::model_id)
                    .map(ToString::to_string)
            })
            .or_else(|| started_target.and_then(|(_, model)| model)),
        native_effort,
        provider_cli_version: session
            .as_ref()
            .and_then(crate::providers::ProviderSessionRecord::cli_version)
            .map(ToOwned::to_owned),
        usage,
        native_model_usage: (!native_models.is_empty())
            .then(|| native_models.into_values().collect()),
        started_at,
        finished_at,
        latency_ms,
        invocation_count,
        injected_prompt_bytes,
    })
}

fn artifact_summary(artifact: &crate::providers::ArtifactRecord) -> ArtifactSummary {
    ArtifactSummary {
        stage_id: artifact.metadata().stage_id().clone(),
        kind: artifact.metadata().kind(),
        status: artifact.metadata().status(),
        attempt: artifact.attempt(),
        provider: artifact.metadata().provider_id().map(ToString::to_string),
        model: artifact.metadata().model_id().map(ToString::to_string),
        content_size: artifact.content_size(),
        created_at: *artifact.metadata().created_at(),
    }
}

pub(crate) fn summarize_diff(text: String, total_bytes: u64, truncated: bool) -> RunDiffPreview {
    let mut changed_files = Vec::<ChangedFileSummary>::new();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("diff --git a/") {
            let path = path
                .split_once(" b/")
                .map_or(path, |(_, destination)| destination)
                .to_owned();
            if !changed_files.iter().any(|file| file.path == path) {
                changed_files.push(ChangedFileSummary {
                    path,
                    binary: false,
                });
            }
        } else if (line.starts_with("Binary files ") || line == "GIT binary patch")
            && let Some(file) = changed_files.last_mut()
        {
            file.binary = true;
        }
    }
    RunDiffPreview {
        text,
        changed_files,
        total_bytes,
        truncated,
    }
}

const fn role_order(role: Role) -> u8 {
    match role {
        Role::Researcher => 0,
        Role::Architect => 1,
        Role::Implementer => 2,
        Role::Simplifier => 3,
        Role::CodeQualityReviewer => 4,
        Role::SpecReviewer => 5,
        Role::Reviewer => 6,
        Role::EngineeringLead => 7,
        Role::Verifier => 8,
    }
}

fn task_summary(task: Option<&str>) -> String {
    let Some(task) = task else {
        return "<legacy input unavailable>".to_owned();
    };
    let first = task
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(task);
    let first = compact_github_refs(first.trim());
    let mut chars = first.chars();
    let summary = chars.by_ref().take(72).collect::<String>();
    if chars.next().is_some() {
        format!("{summary}…")
    } else {
        summary
    }
}

/// Rewrites pasted GitHub PR/issue URLs to `<number> <repo>` so a run like
/// "Review <https://github.com/Automattic/wp-calypso/pull/113847>" lists as
/// "Review 113847 wp-calypso" instead of a URL that never fits the column.
fn compact_github_refs(line: &str) -> String {
    line.split_whitespace()
        .map(|word| compact_github_ref(word).unwrap_or_else(|| word.to_owned()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn compact_github_ref(word: &str) -> Option<String> {
    let rest = word
        .strip_prefix("https://")
        .or_else(|| word.strip_prefix("http://"))?;
    let rest = rest
        .strip_prefix("github.com/")
        .or_else(|| rest.strip_prefix("www.github.com/"))?;
    let mut segments = rest.split('/');
    let owner = segments.next()?;
    let repo = segments.next()?;
    let kind = segments.next()?;
    if owner.is_empty() || repo.is_empty() || !matches!(kind, "pull" | "issues") {
        return None;
    }
    let number: String = segments
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if number.is_empty() {
        return None;
    }
    Some(format!("{number} {repo}"))
}

/// Folds committed usage events into one summary per reporting runtime.
///
/// The provider is taken from the event that reported the numbers, not from
/// the configured route, so a stage that ran somewhere other than where it
/// was routed is still counted against the runtime that actually did it.
fn run_usage(events: &[SequencedEvent]) -> RunUsage {
    events.iter().fold(RunUsage::default(), |mut usage, event| {
        if let DomainEventKind::ProviderUsageUpdated {
            provider_id,
            input_units,
            output_units,
            cache_read_units,
            cache_write_units,
            reasoning_output_units,
            ..
        } = event.event.kind()
        {
            usage.absorb(
                provider_id.as_str(),
                &UsageSummary {
                    input_units: *input_units,
                    output_units: *output_units,
                    cache_read_units: *cache_read_units,
                    cache_write_units: *cache_write_units,
                    reasoning_output_units: *reasoning_output_units,
                },
            );
        }
        usage
    })
}

/// What the stage is configured to run on: the operator's override when one
/// was recorded, else the role's snapshot route. The override replaces the
/// whole target, model included.
struct ConfiguredTarget {
    provider: Option<String>,
    model: Option<String>,
    overridden: bool,
}

fn configured_target(
    stage: &crate::domain::Stage,
    route: Option<&super::RoleRoute>,
) -> ConfiguredTarget {
    stage.route_override().map_or_else(
        || ConfiguredTarget {
            provider: route.map(|route| route.target().provider_id().to_string()),
            model: route
                .and_then(|route| route.target().model_id())
                .map(ToString::to_string),
            overridden: false,
        },
        |route| ConfiguredTarget {
            provider: Some(route.provider_id().to_string()),
            model: route.model_id().map(ToString::to_string),
            overridden: true,
        },
    )
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;
    use crate::domain::{DomainEvent, EventId, EventMetadata};

    fn at(hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 21, hour, minute, second)
            .single()
            .unwrap()
    }

    fn artifact(
        stage: &str,
        kind: ArtifactKind,
        status: ArtifactStatus,
        created_at: DateTime<Utc>,
        attempt: u32,
    ) -> crate::providers::ArtifactRecord {
        let metadata = crate::domain::ArtifactMetadata::new(
            crate::domain::ArtifactId::new(),
            RunId::from_u128(1),
            StageId::new(stage).unwrap(),
            kind,
            Role::Implementer,
            status,
            created_at,
        );
        crate::providers::ArtifactRecord::new(
            metadata,
            attempt,
            PathBuf::from("/artifacts").join(stage),
            "0".repeat(64),
            0,
            created_at,
        )
        .unwrap()
    }

    /// A fix restates the pull request for the whole branch, so the newest
    /// editing artifact wins; reviews and decisions never describe a change
    /// they did not make, and a failed attempt never speaks for the branch.
    #[test]
    fn editing_artifacts_are_walked_back_newest_first_for_the_pull_request() {
        let artifacts = vec![
            artifact(
                "implementation",
                ArtifactKind::Implementation,
                ArtifactStatus::Complete,
                at(1, 0, 0),
                1,
            ),
            artifact(
                "decision",
                ArtifactKind::Decision,
                ArtifactStatus::Complete,
                at(2, 0, 0),
                1,
            ),
            artifact(
                "fix-1",
                ArtifactKind::Fix,
                ArtifactStatus::Complete,
                at(3, 0, 0),
                1,
            ),
            artifact(
                "fix-1",
                ArtifactKind::Fix,
                ArtifactStatus::Complete,
                at(3, 0, 0),
                2,
            ),
            artifact(
                "fix-2",
                ArtifactKind::Fix,
                ArtifactStatus::Failed,
                at(4, 0, 0),
                1,
            ),
            artifact(
                "review",
                ArtifactKind::CodeQualityReview,
                ArtifactStatus::Complete,
                at(5, 0, 0),
                1,
            ),
        ];

        let ordered = editing_artifacts_newest_first(&artifacts);

        let first = ordered.first().unwrap();
        assert_eq!(first.metadata().stage_id().to_string(), "fix-1");
        assert_eq!(first.attempt(), 2);
        // The walk-back order: the newest fix's earlier attempt, then the
        // implementation. A decision, a review, and a failed fix are not
        // editing artifacts and never enter it.
        let rest: Vec<_> = ordered[1..]
            .iter()
            .map(|artifact| {
                (
                    artifact.metadata().stage_id().to_string(),
                    artifact.attempt(),
                )
            })
            .collect();
        assert_eq!(
            rest,
            vec![("fix-1".to_owned(), 1), ("implementation".to_owned(), 1)]
        );
        assert!(editing_artifacts_newest_first(&artifacts[1..2]).is_empty());
        assert!(editing_artifacts_newest_first(&[]).is_empty());
    }

    fn event(
        sequence: u64,
        at: DateTime<Utc>,
        stage: Option<&str>,
        kind: DomainEventKind,
    ) -> SequencedEvent {
        SequencedEvent {
            sequence,
            event: DomainEvent::new(
                EventMetadata::new(EventId::from_u128(u128::from(sequence)), at),
                RunId::from_u128(1),
                stage.map(|id| StageId::new(id).unwrap()),
                kind,
            ),
        }
    }

    fn usage_event(
        sequence: u64,
        stage: &str,
        provider: &str,
        input_units: u64,
        cache_read_units: Option<u64>,
    ) -> SequencedEvent {
        event(
            sequence,
            at(12, 0, 0),
            Some(stage),
            DomainEventKind::ProviderUsageUpdated {
                provider_id: crate::domain::ProviderId::new(provider).unwrap(),
                input_units,
                output_units: 10,
                cache_read_units,
                cache_write_units: None,
                reasoning_output_units: None,
                native_models: None,
            },
        )
    }

    /// A deep run splits its roles across both runtimes, and the two do not
    /// report the same quantity under the name "input". Folding them into one
    /// total would produce a figure that measures nothing, so the fold keys by
    /// the runtime that reported the numbers.
    #[test]
    fn usage_folds_per_runtime_and_never_into_one_run_total() {
        let usage = run_usage(&[
            usage_event(1, "research", "claude", 46, Some(1_619_864)),
            usage_event(2, "implementation", "codex", 5_783_474, Some(5_606_656)),
            usage_event(3, "decision", "claude", 32, Some(925_906)),
        ]);
        let by_provider = usage.providers().collect::<Vec<_>>();
        assert_eq!(by_provider.len(), 2, "one entry per reporting runtime");

        let claude = &by_provider[0];
        assert_eq!(claude.provider, "claude");
        assert_eq!(claude.usage.input_units, 78, "same-runtime events do add");
        assert_eq!(claude.usage.cache_read_units, Some(2_545_770));
        assert_eq!(claude.uncached_input_units(), Some(78));

        let codex = &by_provider[1];
        assert_eq!(codex.provider, "codex");
        assert_eq!(codex.usage.input_units, 5_783_474);
        // Codex already counted its cache reads inside that total.
        assert!(codex.input_contains_cache_reads());
        assert_eq!(codex.uncached_input_units(), Some(176_818));

        // Nothing anywhere offers 5_783_552 as the run's input.
        assert!(
            !by_provider
                .iter()
                .any(|entry| entry.usage.input_units == 5_783_552),
            "the two runtimes are never summed"
        );
    }

    /// A cache-inclusive runtime that reports no cache read leaves the
    /// uncached figure unavailable rather than claiming its whole input was
    /// fresh.
    #[test]
    fn a_missing_cache_read_leaves_uncached_input_unavailable() {
        let usage = run_usage(&[usage_event(1, "implementation", "codex", 900, None)]);
        let codex = usage.providers().next().expect("one entry");
        assert_eq!(codex.usage.input_units, 900);
        assert_eq!(codex.uncached_input_units(), None);
    }

    #[test]
    fn stage_span_folds_from_persisted_lifecycle_events() {
        let events = vec![
            event(1, at(12, 0, 0), None, DomainEventKind::RunStarted),
            event(
                2,
                at(12, 0, 5),
                Some("architecture"),
                DomainEventKind::StageStarted,
            ),
            event(
                3,
                at(12, 2, 19),
                Some("architecture"),
                DomainEventKind::StageCompleted,
            ),
            event(
                4,
                at(12, 2, 19),
                Some("implementation"),
                DomainEventKind::StageStarted,
            ),
        ];
        let architecture = stage_span(&events, &StageId::new("architecture").unwrap());
        assert_eq!(architecture, (Some(at(12, 0, 5)), Some(at(12, 2, 19))));

        // A stage in flight has a start and no finish, so the caller measures
        // against the current time rather than a persisted end.
        let implementation = stage_span(&events, &StageId::new("implementation").unwrap());
        assert_eq!(implementation, (Some(at(12, 2, 19)), None));

        // A stage that has not started carries no span at all.
        let pending = stage_span(&events, &StageId::new("decision").unwrap());
        assert_eq!(pending, (None, None));
    }

    #[test]
    fn retry_restarts_the_stage_span_on_the_current_attempt() {
        let events = vec![
            event(
                1,
                at(12, 0, 0),
                Some("implementation"),
                DomainEventKind::StageStarted,
            ),
            event(
                2,
                at(12, 1, 0),
                Some("implementation"),
                DomainEventKind::StageFailed,
            ),
            event(
                3,
                at(12, 9, 0),
                Some("implementation"),
                DomainEventKind::StageStarted,
            ),
        ];
        assert_eq!(
            stage_span(&events, &StageId::new("implementation").unwrap()),
            (Some(at(12, 9, 0)), None),
            "the retried attempt owns the span, not the failed one"
        );
    }

    #[test]
    fn run_span_survives_resume_and_closes_on_the_terminal_event() {
        let events = vec![
            event(1, at(12, 0, 0), None, DomainEventKind::RunStarted),
            event(2, at(12, 3, 0), None, DomainEventKind::RunPaused),
            event(3, at(12, 5, 0), None, DomainEventKind::RunResumed),
            event(4, at(12, 12, 48), None, DomainEventKind::RunCompleted),
        ];
        assert_eq!(
            run_span(&events),
            (Some(at(12, 0, 0)), Some(at(12, 12, 48)))
        );
    }

    #[test]
    fn runs_without_timing_evidence_report_no_span() {
        assert_eq!(run_span(&[]), (None, None));
        assert_eq!(
            stage_span(&[], &StageId::new("implementation").unwrap()),
            (None, None)
        );
    }

    #[test]
    fn task_summary_compacts_github_urls_to_number_and_repo() {
        assert_eq!(
            task_summary(Some(
                "Review https://github.com/Automattic/wp-calypso/pull/113847"
            )),
            "Review 113847 wp-calypso"
        );
        assert_eq!(
            task_summary(Some("Fix https://github.com/rust-lang/rust/issues/42")),
            "Fix 42 rust"
        );
        assert_eq!(
            task_summary(Some(
                "Compare https://github.com/a/b/pull/1/files and https://github.com/a/b/pull/2"
            )),
            "Compare 1 b and 2 b"
        );
    }

    fn provider_failed_event(sequence: u64, stage: &str, reason: Option<&str>) -> SequencedEvent {
        event(
            sequence,
            at(12, 0, 0),
            Some(stage),
            DomainEventKind::ProviderFailed {
                provider_id: crate::domain::ProviderId::new("codex").unwrap(),
                session_id: None,
                reason: reason.map(ToOwned::to_owned),
            },
        )
    }

    #[test]
    fn stage_failure_reason_is_attributed_to_the_event_own_stage() {
        let events = vec![
            provider_failed_event(1, "research", Some("research provider timed out")),
            provider_failed_event(
                2,
                "implementation",
                Some("compile failed: missing semicolon"),
            ),
        ];
        assert_eq!(
            stage_failure_reason(&events, &StageId::new("implementation").unwrap()),
            Some("compile failed: missing semicolon".to_owned())
        );
        assert_eq!(
            stage_failure_reason(&events, &StageId::new("research").unwrap()),
            Some("research provider timed out".to_owned())
        );
        assert_eq!(
            stage_failure_reason(&events, &StageId::new("decision").unwrap()),
            None,
            "a stage with no failure event carries no reason"
        );
    }

    #[test]
    fn stage_failure_reason_prefers_the_last_reason_within_one_attempt() {
        let events = vec![
            provider_failed_event(1, "implementation", Some("first signal: disk full")),
            provider_failed_event(2, "implementation", Some("terminal: compile failed")),
        ];
        assert_eq!(
            stage_failure_reason(&events, &StageId::new("implementation").unwrap()),
            Some("terminal: compile failed".to_owned())
        );
    }

    #[test]
    fn stage_failure_reason_a_reasonless_failure_never_erases_an_earlier_one_in_the_same_attempt() {
        let events = vec![
            provider_failed_event(1, "implementation", Some("first attempt: OOM")),
            provider_failed_event(2, "implementation", None),
        ];
        assert_eq!(
            stage_failure_reason(&events, &StageId::new("implementation").unwrap()),
            Some("first attempt: OOM".to_owned())
        );
    }

    /// A `StageRetryScheduled` closes out whatever the abandoned attempt
    /// reported. A stage waiting to run again after that carries no reason —
    /// showing the previous attempt's failure while the retry has not even
    /// started would blame the wrong run.
    #[test]
    fn stage_failure_reason_clears_at_retry_while_the_new_attempt_is_pending() {
        let events = vec![
            event(
                1,
                at(12, 0, 0),
                Some("implementation"),
                DomainEventKind::StageStarted,
            ),
            provider_failed_event(2, "implementation", Some("first attempt: OOM")),
            event(
                3,
                at(12, 1, 0),
                Some("implementation"),
                DomainEventKind::StageFailed,
            ),
            event(
                4,
                at(12, 2, 0),
                Some("implementation"),
                DomainEventKind::StageRetryScheduled,
            ),
        ];
        assert_eq!(
            stage_failure_reason(&events, &StageId::new("implementation").unwrap()),
            None,
            "a retried stage waiting to run again carries no stale reason"
        );
    }

    /// A retry that goes on to succeed must never leave the abandoned
    /// attempt's failure behind for a completed stage to still report.
    #[test]
    fn stage_failure_reason_clears_after_a_retry_that_succeeds() {
        let events = vec![
            provider_failed_event(1, "implementation", Some("first attempt: OOM")),
            event(
                2,
                at(12, 1, 0),
                Some("implementation"),
                DomainEventKind::StageRetryScheduled,
            ),
            event(
                3,
                at(12, 2, 0),
                Some("implementation"),
                DomainEventKind::StageStarted,
            ),
            event(
                4,
                at(12, 3, 0),
                Some("implementation"),
                DomainEventKind::StageCompleted,
            ),
        ];
        assert_eq!(
            stage_failure_reason(&events, &StageId::new("implementation").unwrap()),
            None,
            "a retry that goes on to succeed leaves no reason behind"
        );
    }

    /// A retry that fails again must report only the new attempt's reason,
    /// never the one the abandoned attempt carried.
    #[test]
    fn stage_failure_reason_after_a_retry_reports_only_the_new_attempts_reason() {
        let events = vec![
            provider_failed_event(1, "implementation", Some("first attempt: OOM")),
            event(
                2,
                at(12, 1, 0),
                Some("implementation"),
                DomainEventKind::StageRetryScheduled,
            ),
            event(
                3,
                at(12, 2, 0),
                Some("implementation"),
                DomainEventKind::StageStarted,
            ),
            provider_failed_event(4, "implementation", Some("second attempt: disk full")),
        ];
        assert_eq!(
            stage_failure_reason(&events, &StageId::new("implementation").unwrap()),
            Some("second attempt: disk full".to_owned()),
            "the abandoned attempt's reason never leaks into the retry's own failure"
        );
    }

    #[test]
    fn sanitize_failure_reason_escapes_control_characters() {
        assert_eq!(
            sanitize_failure_reason("\u{1b}[31mCRASH\u{1b}[0m"),
            Some("\u{fffd}[31mCRASH\u{fffd}[0m".to_owned()),
            "an ANSI escape sequence must never reach the terminal unescaped"
        );
        assert_eq!(
            sanitize_failure_reason("alert\u{7}bell"),
            Some("alert\u{fffd}bell".to_owned())
        );
    }

    #[test]
    fn sanitize_failure_reason_collapses_whitespace_to_one_line() {
        assert_eq!(
            sanitize_failure_reason("exit code 1\n\nstderr:\n  compile failed\n"),
            Some("exit code 1 stderr: compile failed".to_owned())
        );
    }

    #[test]
    fn sanitize_failure_reason_caps_long_text_with_ellipsis() {
        let reason = "x".repeat(250);
        let sanitized = sanitize_failure_reason(&reason).unwrap();
        assert_eq!(
            sanitized.chars().count(),
            201,
            "200 characters plus the ellipsis"
        );
        assert!(sanitized.ends_with('…'));
        assert!(sanitized.starts_with(&"x".repeat(200)));
    }

    #[test]
    fn sanitize_failure_reason_leaves_short_text_untouched() {
        assert_eq!(
            sanitize_failure_reason("compile failed"),
            Some("compile failed".to_owned())
        );
    }

    #[test]
    fn sanitize_failure_reason_treats_blank_text_as_no_reason() {
        assert_eq!(sanitize_failure_reason(""), None);
        assert_eq!(sanitize_failure_reason("   \n\t  "), None);
    }

    /// A garbled or hostile provider record can run to tens of megabytes.
    /// The bounded single-pass implementation must produce exactly the same
    /// capped, single-line output a short input would — proving the early
    /// break on the 200-character budget still yields correct results — and
    /// must do it without the naive collapse-then-escape-then-slice approach
    /// ever materializing a copy proportional to the input's length. The
    /// interesting content sits in the first few characters and everything
    /// after it is discarded padding, so a bounded scan finishes almost
    /// immediately no matter how long the tail is; this is the property
    /// `sanitize_failure_reason`'s doc comment claims.
    #[test]
    fn sanitize_failure_reason_caps_a_multi_megabyte_input() {
        let huge = format!("compile failed{}", "z".repeat(8 * 1024 * 1024));
        let sanitized = sanitize_failure_reason(&huge).unwrap();
        assert_eq!(
            sanitized.chars().count(),
            201,
            "200 characters plus the ellipsis, regardless of the input's real length"
        );
        assert!(sanitized.starts_with("compile failed"));
        assert!(sanitized.ends_with('…'));
    }

    #[test]
    fn task_summary_leaves_non_reference_urls_alone() {
        assert_eq!(
            task_summary(Some("See https://github.com/Automattic/wp-calypso")),
            "See https://github.com/Automattic/wp-calypso"
        );
        assert_eq!(
            task_summary(Some("See https://github.com/a/b/pull/not-a-number")),
            "See https://github.com/a/b/pull/not-a-number"
        );
        assert_eq!(
            task_summary(Some("See https://example.com/a/b/pull/3")),
            "See https://example.com/a/b/pull/3"
        );
    }

    #[test]
    fn publication_is_outdated_once_a_later_cycle_or_rebase_moves_the_delta() {
        use crate::domain::RunId;
        let run_id = RunId::from_u128(1);
        let event = |sequence: u64, kind: DomainEventKind| SequencedEvent {
            sequence,
            event: DomainEvent::new(
                EventMetadata::new(EventId::new(), at(9, 0, 0)),
                run_id,
                None,
                kind,
            ),
        };
        let published = || DomainEventKind::RunPublished {
            branch: "senate/run-1".to_owned(),
            commit: "abc".to_owned(),
            pull_request_url: Some("https://github.com/o/r/pull/9".to_owned()),
        };
        assert_eq!(publication(&[]), None);
        let current = publication(&[event(1, published())]).unwrap();
        assert!(!current.outdated);
        assert_eq!(
            current.pull_request_url.as_deref(),
            Some("https://github.com/o/r/pull/9")
        );
        for later in [
            DomainEventKind::RunFixRequested { stage_ids: vec![] },
            DomainEventKind::RunContinueRequested { stage_ids: vec![] },
            DomainEventKind::WorkspaceRebased {
                from_base: "a".to_owned(),
                to_base: "b".to_owned(),
            },
        ] {
            assert!(
                publication(&[event(1, published()), event(2, later.clone())])
                    .unwrap()
                    .outdated
            );
            // Publishing again after it catches the remote up.
            assert!(
                !publication(&[
                    event(1, published()),
                    event(2, later),
                    event(3, published())
                ])
                .unwrap()
                .outdated
            );
        }
    }
}
