use thiserror::Error;

use crate::domain::{IdError, RunId, StageId};
use crate::engine::{EngineError, FakeScenarioError};
use crate::git::GitError;
use crate::process::ProcessError;
use crate::providers::claude::ClaudeProviderError;
use crate::providers::codex::CodexProviderError;
use crate::providers::opencode::OpencodeProviderError;
use crate::store::{RunInputError, StoreError};
use crate::workspace::WorkspaceError;

use super::RoutingError;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("mission {0} is already being driven by another process")]
    MissionDriveBusy(crate::domain::MissionId),
    #[error("mission drive concurrency must be between 1 and 16, got {0}")]
    InvalidMissionDriveLimit(usize),
    #[error("mission drive is unavailable: {0}")]
    MissionDriveUnavailable(String),
    #[error("the Consul proposed no plan changes for mission {0}")]
    NoLeadProposals(crate::domain::MissionId),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    RunInput(#[from] RunInputError),
    #[error(transparent)]
    Identifier(#[from] IdError),
    #[error(transparent)]
    RunTransition(#[from] crate::domain::RunTransitionError),
    #[error(transparent)]
    Mission(#[from] crate::domain::MissionError),
    #[error(transparent)]
    MissionInput(#[from] crate::store::MissionInputError),
    #[error("mission {0} has no package {1}")]
    PackageNotFound(crate::domain::MissionId, crate::domain::WorkPackageId),
    #[error("run {run_id} works in {actual:?}, not in the mission's repository {expected}")]
    MissionRepositoryMismatch {
        run_id: RunId,
        expected: std::path::PathBuf,
        actual: Option<std::path::PathBuf>,
    },
    #[error("run {run_id} is {status:?}; only a completed or applied run can be integrated")]
    PackageNotIntegrated {
        run_id: RunId,
        status: crate::domain::RunStatus,
    },
    #[error(
        "run {run_id} completed but its worktree is {workspace:?}, so its delta cannot be read; \
`senate resume {run_id}` reconciles the workspace, or discard the run and retry the package"
    )]
    PackageWorkspaceUnavailable {
        run_id: RunId,
        workspace: Option<crate::workspace::WorkspaceStatus>,
    },
    #[error(
        "the lead of mission {mission_id} (run {run_id}) is {status:?}; wait for it, resume it, or discard it before asking again"
    )]
    LeadBusy {
        mission_id: crate::domain::MissionId,
        run_id: RunId,
        status: crate::domain::RunStatus,
    },
    #[error(
        "run {run_id} started as the lead of mission {mission_id} but could not be bound to it: {reason}"
    )]
    LeadRunUnbound {
        mission_id: crate::domain::MissionId,
        run_id: RunId,
        reason: String,
    },
    #[error("mission {0} has no lead answer to apply; ask the lead first (`senate mission ask`)")]
    NoLeadAnswer(crate::domain::MissionId),
    #[error(
        "the lead's answer (run {run_id}, stage {stage_id}) proposes nothing that can be applied: {source}"
    )]
    LeadProposalUnreadable {
        run_id: RunId,
        stage_id: StageId,
        #[source]
        source: crate::domain::PlanChangeParseError,
    },
    #[error(
        "run {run_id} started but could not be bound to package {package_id} of mission \
{mission_id}: {reason}. Attach it with `senate mission attach {mission_id} {package_id} {run_id}`."
    )]
    PackageRunUnbound {
        mission_id: crate::domain::MissionId,
        package_id: crate::domain::WorkPackageId,
        run_id: RunId,
        reason: String,
    },
    #[error(transparent)]
    FakeScenario(#[from] FakeScenarioError),
    #[error(transparent)]
    Claude(#[from] ClaudeProviderError),
    #[error(transparent)]
    Codex(#[from] CodexProviderError),
    #[error(transparent)]
    Opencode(#[from] OpencodeProviderError),
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error(transparent)]
    Routing(#[from] RoutingError),
    #[error(
        "execution selection is required; use --provider claude|codex|opencode|fake or --profile recommended"
    )]
    NoProductionProvider,
    #[error("unsupported provider {0:?}; supported providers: claude, codex, opencode, fake")]
    UnsupportedProvider(String),
    #[error(
        "--provider opencode requires an explicit model, because opencode's whole point is picking a vendor by model id and it has no single native default across them; use --provider opencode --model <provider/model> (run `opencode models` to see every model this installation supports)"
    )]
    OpencodeModelRequired,
    #[error(
        "image generation cannot be enabled: {0}. Install and authenticate the Codex CLI (`codex login`) and retry, or start without --allow-image-generation."
    )]
    ImageGenerationUnavailable(String),
    #[error("image generation is not supported by this provider factory: {0}")]
    ImageGenerationUnsupported(String),
    #[error(
        "Repository has uncommitted changes.\n  Commit or stash them before starting a Senate run."
    )]
    DirtySourceRepository,
    #[error(
        "Mission {mission_id} package {package_id} cannot start while the source checkout has uncommitted changes.\n  Commit changes from integrated packages before starting the next package; stash unrelated changes if needed.\n  The Senate does not stage or commit for you. Retry your original `senate mission start` command with the same options."
    )]
    MissionIntegrationNeedsCommit {
        mission_id: crate::domain::MissionId,
        package_id: crate::domain::WorkPackageId,
    },
    #[error(
        "Pull request {url} cannot be read.\n  {detail}\n  The run would give every stage the same nothing, so it is not started.\n  Fix: make {host} reachable for `gh` — VPN, or HTTPS_PROXY when the host sits behind a proxy — check with `gh auth status --hostname {host}`, then start the run again, or paste the pull request title, description and diff into the task instead."
    )]
    PullRequestUnreachable {
        url: String,
        host: String,
        detail: String,
    },
    #[error("run {0} cannot be stopped from status {1:?}")]
    RunNotStoppable(RunId, crate::domain::RunStatus),
    #[error("run {0} cannot be resumed because its input predates the executable schema")]
    LegacyRunInput(RunId),
    #[error(
        "run {0} cannot be resumed because its execution configuration predates the executable schema"
    )]
    LegacyExecutionConfig(RunId),
    #[error(
        "run {run_id} cannot be fixed: its execution configuration was sealed before \
fix-cycle routing existed and has no route for {role:?}. Start a new run to act on this one's findings."
    )]
    UnroutableFixCycle {
        run_id: RunId,
        role: crate::domain::Role,
    },
    #[error(
        "run {run_id} cannot continue: its execution configuration was sealed before \
continue-cycle routing existed and has no route for {role:?}. Start a new run instead."
    )]
    UnroutableContinueCycle {
        run_id: RunId,
        role: crate::domain::Role,
    },
    #[error("run {0} was asked to continue with an empty instruction")]
    EmptyContinueInstruction(RunId),
    #[error("run {0} was discarded and cannot continue")]
    DiscardedRun(RunId),
    #[error("run {run_id} stage {stage_id} has no verified artifact")]
    ArtifactNotFound { run_id: RunId, stage_id: StageId },
    #[error("run {run_id} stage {stage_id} has no managed process log")]
    ProcessLogNotFound { run_id: RunId, stage_id: StageId },
    #[error("run {run_id} has no stage {stage_id}")]
    StageNotFound { run_id: RunId, stage_id: StageId },
    #[error(
        "run {0} is not archived. Deleting a run for good is only offered for a run already \
set aside, so archive it first."
    )]
    RunNotArchived(RunId),
    #[error("run {0} is still {1:?} and cannot be deleted. Stop it first.")]
    RunNotDeletable(RunId, crate::domain::RunStatus),
    #[error("run files at {0} could not be removed: {1}. The run is still listed; try again.")]
    RunFilesNotRemoved(std::path::PathBuf, #[source] std::io::Error),
    #[error(
        "run {run_id} was NOT stopped: managed process {process_id} never recorded the runtime \
evidence a signal needs within {waited_ms}ms. The process may still be running; try stopping again."
    )]
    StopRuntimeEvidenceUnavailable {
        run_id: RunId,
        process_id: crate::process::ManagedProcessId,
        waited_ms: u128,
    },
    #[error(
        "Pull request {url} belongs to {repository}, and no local checkout of it was found.\n  \
Looked at every checkout The Senate has run from before, and at the folders next to {searched_parent}.\n  \
Fix: clone {repository} locally and start the run from that folder."
    )]
    NoLocalCheckoutForPullRequest {
        url: String,
        /// `host/owner/repo`, folded into one string at the point this is
        /// built rather than carried as three fields just to be joined again
        /// for the message.
        repository: String,
        searched_parent: std::path::PathBuf,
    },
    #[error(
        "Pull request {url} belongs to a repository this checkout's origin does not match, and \
every local checkout of it that was found has uncommitted changes: {checkouts:?}\n  \
Commit or stash the changes there before starting a Senate run from it."
    )]
    PullRequestCheckoutsDirty {
        url: String,
        checkouts: Vec<std::path::PathBuf>,
    },
}

impl AppError {
    /// Whether this error is an optimistic-concurrency loss rather than a real
    /// failure. Callers that are safe to repeat — every step of a stop is
    /// idempotent — retry instead of surfacing a revision number to the user.
    /// Each wrapper asks the layer beneath it, instead of this one predicate
    /// enumerating every shape a lost race can arrive in. The same store error
    /// reaches here through several nestings — bare, through the engine, and
    /// through a process error that itself wraps a store error — and listing
    /// them by hand has now missed one twice, each time turning a retryable
    /// race into a stop that aborts and shows the user a revision number. With
    /// the question delegated downwards, a new nesting is covered by the layer
    /// that introduced it. (The source chain cannot be walked instead: these
    /// errors are transparent, so `source` skips past the very error that
    /// carries the answer.)
    #[must_use]
    pub const fn is_concurrent_modification(&self) -> bool {
        match self {
            Self::Store(error) => error.is_lost_revision(),
            Self::Process(error) => error.is_lost_revision(),
            Self::Engine(error) => error.is_lost_revision(),
            _ => false,
        }
    }

    /// Whether this stop failed only because a managed process had not yet
    /// recorded its runtime evidence. Repeating the stop is safe — every step
    /// of it is idempotent — and the evidence is imminent whenever the runner
    /// is alive, so the stop path waits rather than telling a user who pressed
    /// stop moments after starting a run that it cannot be stopped.
    #[must_use]
    pub const fn is_pending_runtime_evidence(&self) -> bool {
        match self {
            Self::Process(error) => error.is_missing_runtime_evidence(),
            _ => false,
        }
    }

    /// The managed process a pending-runtime-evidence failure refers to.
    #[must_use]
    pub const fn pending_runtime_evidence_process(
        &self,
    ) -> Option<crate::process::ManagedProcessId> {
        match self {
            Self::Process(ProcessError::MissingRuntimeEvidence(process_id)) => Some(*process_id),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RunId;
    use crate::process::ManagedProcessId;

    // The stop path retries only what this predicate classifies as a lost
    // revision race. If an arm is dropped, stop silently goes back to
    // aborting mid-way and reporting a revision number to the user, so the
    // classification is pinned here rather than only through the racy
    // end-to-end fixture.
    // Missing runtime evidence is not a lost race and must never be treated
    // as one: the two have separate retry budgets in the stop path, and
    // collapsing them would let a burst of one exhaust the other.
    #[test]
    fn pending_runtime_evidence_is_its_own_retryable_condition() {
        let process_id = ManagedProcessId::new();
        let error = AppError::Process(ProcessError::MissingRuntimeEvidence(process_id));
        assert!(error.is_pending_runtime_evidence());
        assert!(!error.is_concurrent_modification());
        assert_eq!(error.pending_runtime_evidence_process(), Some(process_id));

        let race = AppError::Process(ProcessError::ConcurrentModification {
            process_id,
            expected: 3,
        });
        assert!(!race.is_pending_runtime_evidence());
        assert_eq!(race.pending_runtime_evidence_process(), None);

        // A process that failed to start for a permanent reason is not
        // something a stop should sit and wait on.
        assert!(
            !AppError::Process(ProcessError::InterruptTimeout(process_id))
                .is_pending_runtime_evidence()
        );
        assert!(!AppError::DirtySourceRepository.is_pending_runtime_evidence());
    }

    #[test]
    fn only_lost_revision_races_are_retryable() {
        assert!(
            AppError::Store(StoreError::ConcurrentModification {
                run_id: RunId::new(),
                expected: 3,
            })
            .is_concurrent_modification()
        );
        assert!(
            AppError::Process(ProcessError::ConcurrentModification {
                process_id: ManagedProcessId::new(),
                expected: 3,
            })
            .is_concurrent_modification()
        );

        // The same race arrives wrapped when it is lost inside the engine,
        // which is exactly the path a stop reconciles through.
        assert!(
            AppError::Engine(EngineError::Store(StoreError::ConcurrentModification {
                run_id: RunId::new(),
                expected: 5,
            }))
            .is_concurrent_modification()
        );

        // Reconciling a stop also writes provider sessions, which carry their
        // own revision and therefore their own lost race.
        assert!(
            AppError::Store(StoreError::ProviderSessionConcurrentModification {
                id: crate::providers::ProviderSessionRecordId::new(),
                expected: 2,
            })
            .is_concurrent_modification()
        );

        // The nesting that actually escaped in the field: a provider-session
        // race, lost inside the process manager, reached through the engine.
        // Three layers, none of which the shape-matching form enumerated.
        assert!(
            AppError::Engine(EngineError::Process(ProcessError::Store(
                StoreError::ProviderSessionConcurrentModification {
                    id: crate::providers::ProviderSessionRecordId::new(),
                    expected: 2,
                }
            )))
            .is_concurrent_modification(),
            "a lost revision is retryable however deeply it is wrapped"
        );
        assert!(
            AppError::Process(ProcessError::Store(StoreError::ConcurrentModification {
                run_id: RunId::new(),
                expected: 1,
            }))
            .is_concurrent_modification()
        );

        // A real refusal must never be retried into success.
        assert!(
            !AppError::RunNotStoppable(RunId::new(), crate::domain::RunStatus::Completed)
                .is_concurrent_modification()
        );
        assert!(!AppError::DirtySourceRepository.is_concurrent_modification());
    }
}
