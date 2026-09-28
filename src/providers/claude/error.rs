use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClaudeProviderError {
    #[error("Claude Code executable was not found on PATH")]
    NotFound,
    #[error("Claude Code command failed during {operation}: {message}")]
    Command {
        operation: &'static str,
        message: String,
    },
    #[error("Claude Code is not authenticated; run `claude auth login`")]
    NotAuthenticated,
    #[error("Claude Code emitted invalid stream JSON: {0}")]
    Protocol(String),
    #[error("{0}")]
    PermissionsConfig(String),
    #[error("Claude Code permission cannot be resumed safely: {0}")]
    UnsafePermission(String),
    #[error("Claude Code question requires a non-empty `--response`")]
    QuestionResponseRequired,
    #[error("Claude Code attention response exceeds {0} bytes")]
    ResponseTooLarge(usize),
    #[error("Claude artifact exceeds {0} bytes")]
    ArtifactTooLarge(usize),
    #[error("Claude artifact path conflict: {0}")]
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
