use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodexProviderError {
    #[error("Codex CLI executable was not found on PATH")]
    NotFound,
    #[error("Codex CLI version probe failed: {0}")]
    VersionProbeFailed(String),
    #[error("Codex CLI authentication probe failed: {0}")]
    AuthStatusFailed(String),
    #[error("Codex CLI lacks required `exec --json` and exact-resume capabilities: {0}")]
    UnsupportedCli(String),
    #[error(
        "Codex CLI is installed but not authenticated; authenticate with native `codex login`, then retry"
    )]
    NotAuthenticated,
    #[error("Codex CLI emitted invalid exec JSON: {0}")]
    Protocol(String),
    #[error("Codex native session mismatch: expected {expected}, received {actual}")]
    SessionMismatch { expected: String, actual: String },
    #[error("Codex process ended before emitting a native thread ID: {0}")]
    MissingThreadId(String),
    #[error("Codex successful turn did not produce final message file: {0}")]
    MissingFinalMessage(PathBuf),
    #[error("Codex artifact exceeds {0} bytes")]
    ArtifactTooLarge(usize),
    #[error(
        "Codex follow-up stage {0} has no room left in the composed prompt for the operator's \
         instruction; refusing to run the stage unscoped"
    )]
    ContinueInstructionOmitted(crate::domain::StageId),
    #[error("Codex artifact path conflict: {0}")]
    ArtifactConflict(PathBuf),
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
