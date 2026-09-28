use std::path::PathBuf;

use thiserror::Error;

use crate::domain::{RunId, StageId};
use crate::store::StoreError;

use super::{BackendSessionId, ManagedProcessId, ManagedProcessStatus, OutputStream};

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("SQLite process operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("process filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("process manifest JSON operation failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("managed processes require macOS or Linux")]
    UnsupportedPlatform,
    #[error("invalid {0}")]
    InvalidIdentifier(&'static str),
    #[error("invalid process specification: {0}")]
    InvalidSpec(&'static str),
    #[error("stored managed-process record is invalid: {0}")]
    InvalidStoredProcess(&'static str),
    #[error("managed process {0} does not exist")]
    ProcessNotFound(ManagedProcessId),
    #[error("managed process {0} already exists with different immutable identity")]
    ProcessConflict(ManagedProcessId),
    #[error(
        "run {run_id} stage {stage_id} attempt {attempt} invocation {invocation} already has another process"
    )]
    AttemptConflict {
        run_id: RunId,
        stage_id: StageId,
        attempt: u32,
        invocation: u32,
    },
    #[error("managed process {process_id} changed since revision {expected}")]
    ConcurrentModification {
        process_id: ManagedProcessId,
        expected: u64,
    },
    #[error("managed process {process_id} {stream:?} cursor changed since revision {expected}")]
    CursorConcurrentModification {
        process_id: ManagedProcessId,
        stream: OutputStream,
        expected: u64,
    },
    #[error("invalid managed-process transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: ManagedProcessStatus,
        to: ManagedProcessStatus,
    },
    #[error("run {run_id} has no stage {stage_id}")]
    UnknownStage { run_id: RunId, stage_id: StageId },
    #[error("managed process working directory must equal ready run workspace: {0}")]
    WorkspaceMismatch(PathBuf),
    #[error("tmux executable was not found")]
    TmuxNotFound,
    #[error("tmux command failed during {operation}: {message}")]
    TmuxCommand {
        operation: &'static str,
        message: String,
    },
    #[error("tmux session {session_id} is owned by another process")]
    ForeignSession { session_id: BackendSessionId },
    #[error("managed process {process_id} has invalid ownership evidence")]
    OwnershipMismatch { process_id: ManagedProcessId },
    #[error("managed process {process_id} has corrupt exit evidence: {reason}")]
    InvalidExitEvidence {
        process_id: ManagedProcessId,
        reason: &'static str,
    },
    #[error("managed process {0} output was truncated below acknowledged offset")]
    OutputTruncated(ManagedProcessId),
    #[error("output read size must be between 1 and {0} bytes")]
    InvalidReadSize(usize),
    #[error("output acknowledgement is outside delivered chunk")]
    InvalidAcknowledgement,
    #[error("managed process {0} did not stop before interrupt timeout")]
    InterruptTimeout(ManagedProcessId),
    #[error("managed process {0} has no valid live runtime evidence")]
    MissingRuntimeEvidence(ManagedProcessId),
    #[error("managed process signal command failed: {0}")]
    SignalCommand(String),
    #[error("managed process runner failed: {0}")]
    Runner(String),
}

impl ProcessError {
    /// Whether this is an optimistic-concurrency loss, including one that
    /// arrived through the store beneath it.
    #[must_use]
    pub const fn is_lost_revision(&self) -> bool {
        match self {
            Self::ConcurrentModification { .. } | Self::CursorConcurrentModification { .. } => true,
            Self::Store(error) => error.is_lost_revision(),
            _ => false,
        }
    }

    /// Whether this is the startup window in which a managed process is
    /// running but has not yet recorded the PIDs a signal needs. The runner
    /// spawns its child before writing `runtime.json`, so between those two
    /// points signalling has nothing honest to aim at. Refusing is right —
    /// guessing a PID would be worse — but the condition is transient, and
    /// the layer that knows a user is waiting on a stop can wait it out.
    ///
    /// The same error also carries permanent conditions: corrupt or foreign
    /// evidence, a runner whose pane PID no longer matches. Nothing here can
    /// tell those apart, which is why the caller's tolerance is bounded and
    /// ends in a failure rather than in silence.
    #[must_use]
    pub const fn is_missing_runtime_evidence(&self) -> bool {
        matches!(self, Self::MissingRuntimeEvidence(_))
    }
}
