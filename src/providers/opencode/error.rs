use std::path::PathBuf;

use thiserror::Error;

use crate::domain::{ModelId, StageId};

#[derive(Debug, Error)]
pub enum OpencodeProviderError {
    #[error("opencode CLI executable was not found on PATH")]
    NotFound,
    #[error("opencode CLI version probe failed: {0}")]
    VersionProbeFailed(String),
    #[error("opencode CLI authentication probe failed: {0}")]
    AuthStatusFailed(String),
    #[error(
        "opencode CLI is installed but has no configured credentials; authenticate with native `opencode auth login`, then retry"
    )]
    NotAuthenticated,
    #[error("opencode CLI model listing failed: {0}")]
    ModelProbeFailed(String),
    #[error(
        "model {0:?} is not one `opencode models` lists; run `opencode models` to see every provider/model this installation supports"
    )]
    UnknownModel(ModelId),
    #[error("opencode CLI emitted invalid run JSON: {0}")]
    Protocol(String),
    #[error("{0}")]
    PermissionsConfig(String),
    #[error("opencode native session mismatch: expected {expected}, received {actual}")]
    SessionMismatch { expected: String, actual: String },
    #[error("opencode process ended before emitting any session ID: {0}")]
    MissingSessionId(String),
    #[error("opencode artifact exceeds {0} bytes")]
    ArtifactTooLarge(usize),
    #[error("opencode artifact path conflict: {0}")]
    ArtifactConflict(PathBuf),
    #[error(
        "opencode follow-up stage {0} has no room left in the composed prompt for the operator's \
         instruction; refusing to run the stage unscoped"
    )]
    ContinueInstructionOmitted(StageId),
    #[error(
        "opencode stage prompt exceeds the {0}-byte argv safety ceiling even after shedding \
         navigation evidence; opencode has no verified stdin or file alternative for a stage prompt"
    )]
    PromptTooLarge(usize),
    #[error("{0}")]
    UnsafePermission(String),
    #[error("opencode attention response cannot be empty")]
    EmptyAttentionResponse,
    #[error(transparent)]
    ChangeHandoff(#[from] crate::providers::change_handoff::ChangeHandoffError),
    #[error(transparent)]
    ContinueInstruction(#[from] crate::providers::continue_instruction::ContinueInstructionError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Process(#[from] crate::process::ProcessError),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
}
