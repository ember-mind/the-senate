use thiserror::Error;

use crate::domain::{
    AttentionError, Role, RunAttentionError, RunFixError, RunId, RunProviderEventError,
    RunStageError, RunStatus, RunTransitionError, StageId,
};
use crate::process::ProcessError;
use crate::store::StoreError;
use crate::workspace::{ApplyStatus, WorkspaceStatus};

use super::ProviderError;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error(transparent)]
    RunTransition(#[from] RunTransitionError),
    #[error(transparent)]
    StageTransition(#[from] RunStageError),
    #[error(transparent)]
    AttentionTransition(#[from] RunAttentionError),
    #[error(transparent)]
    Attention(#[from] AttentionError),
    #[error(transparent)]
    ProviderEvent(#[from] RunProviderEventError),
    #[error(transparent)]
    Fix(#[from] RunFixError),
    #[error("run {0} has no persisted workspace")]
    MissingWorkspace(RunId),
    #[error("run {run_id} requires Ready workspace, found {status:?}")]
    WorkspaceNotReady {
        run_id: RunId,
        status: WorkspaceStatus,
    },
    #[error("run {run_id} execution is frozen by apply intent {status:?}")]
    ApplyInProgress { run_id: RunId, status: ApplyStatus },
    #[error("run execution cannot start from {0:?}")]
    RunNotPrepared(RunStatus),
    #[error("provider does not support role {0:?}")]
    UnsupportedRole(Role),
    #[error("provider changed during stage {stage_id}: {previous} -> {current}")]
    ProviderChanged {
        stage_id: StageId,
        previous: String,
        current: String,
    },
    #[error("provider protocol error for stage {stage_id}: {message}")]
    ProviderProtocol { stage_id: StageId, message: String },
    #[error("scheduler made no legal progress for run {0}")]
    NoProgress(RunId),
    #[error("scheduler exceeded {0} deterministic transitions")]
    DriveLimit(usize),
    #[error("provider checkpoint counter overflow")]
    CheckpointOverflow,
}

impl EngineError {
    /// Whether this is an optimistic-concurrency loss beneath the engine.
    /// Delegated downwards so a new nesting needs no change here.
    #[must_use]
    pub const fn is_lost_revision(&self) -> bool {
        match self {
            Self::Store(error) => error.is_lost_revision(),
            Self::Process(error) => error.is_lost_revision(),
            _ => false,
        }
    }
}
