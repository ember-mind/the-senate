//! Crash-reconcilable Git workspace lifecycle orchestration.

mod branch_name;
mod error;
mod github;
mod manager;
mod model;
mod pull_request;
mod setup;

pub use error::WorkspaceError;
pub(crate) use github::RemoteRepository;
pub use github::{GhClient, PullRequestReach, PullRequestRef};
pub use manager::{
    BranchDisposition, PublishReceipt, PullRequestStatus, RebaseReceipt, ReconciliationOutcome,
    WorkspaceManager,
};
pub use model::{
    ApplyStatus, RunApplyOperation, RunWorkspace, WorkspaceMode, WorkspaceRevision, WorkspaceStatus,
};
pub use pull_request::{PullRequestDraft, extract as extract_pull_request_draft};
