use std::path::PathBuf;

use thiserror::Error;

use crate::domain::{RunId, RunStatus, RunTransitionError};
use crate::git::GitError;
use crate::store::StoreError;

use super::{WorkspaceMode, WorkspaceStatus};

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("run lifecycle transition failed: {0}")]
    RunTransition(#[from] RunTransitionError),
    #[error("run {run_id} status {status:?} is not valid for {operation}")]
    InvalidRunStatus {
        run_id: RunId,
        status: RunStatus,
        operation: &'static str,
    },
    #[error("run {0} already has a workspace")]
    WorkspaceAlreadyExists(RunId),
    #[error("run {0} has no workspace")]
    WorkspaceMissing(RunId),
    #[error("workspace for run {run_id} is {status:?}; expected {expected}")]
    InvalidWorkspaceStatus {
        run_id: RunId,
        status: WorkspaceStatus,
        expected: &'static str,
    },
    #[error("workspace path already exists: {0}")]
    WorkspacePathConflict(PathBuf),
    #[error("workspace branch already exists: {0}")]
    BranchConflict(String),
    #[error("workspace ownership mismatch for run {run_id}: {reason}")]
    WorkspaceOwnershipMismatch { run_id: RunId, reason: String },
    #[error("workspace for run {run_id} is broken: {reason}")]
    WorkspaceBroken { run_id: RunId, reason: String },
    #[error("source checkout contains local changes: {0}")]
    SourceCheckoutDirty(PathBuf),
    /// The repository's `[setup]` table could not be read. Preparation fails
    /// on it for the reason `[verify]` does: a configuration The Senate cannot
    /// understand is a finding, not an absence.
    #[error("{0}")]
    SetupConfig(String),
    /// A `[setup]` command did not exit zero, so the worktree is not the
    /// working tree the repository says it needs. There is no artifact at
    /// preparation time, so the output travels in the error.
    #[error("workspace setup command `{command}` {reason}\n{output}")]
    SetupFailed {
        command: String,
        reason: String,
        output: String,
    },
    #[error("review/detached workspace cannot be applied")]
    ReviewWorkspaceNotApplicable,
    /// The run's latest verify stage did not pass. A failed verification
    /// still completes the run — the decision only optionally depends on
    /// it — so this gate, not the run status, is what keeps unverified
    /// changes out of the source checkout. Publish is not gated by it: a
    /// branch and a pull request are where unverified work belongs.
    #[error("verification did not pass: stage {stage_id} is {status}")]
    VerificationNotPassed {
        stage_id: crate::domain::StageId,
        status: String,
    },
    #[error("workspace has no changes to apply")]
    EmptyPatch,
    #[error("workspace has no changes to publish")]
    NothingToPublish,
    #[error("repository at {0} has no 'origin' remote to publish to")]
    NoRemote(PathBuf),
    /// `git apply --check` refused the run's delta over the source checkout.
    ///
    /// The reason is almost always the checkout moving on underneath a run
    /// that was still working, so the message carries what is known about
    /// that — how far it moved, and the command that moves the run to meet
    /// it — rather than leaving the operator to diff two trees by hand.
    ///
    /// `rebasable` is set when the checkout moved forward past the run's
    /// base, the one refusal a rebase answers, so an interface can offer it.
    #[error("patch cannot be applied cleanly: {reason}")]
    PatchCheckFailed { reason: String, rebasable: bool },
    /// The run's verification passed over a base the workspace no longer
    /// stands on, because a rebase moved it afterwards.
    #[error(
        "verification predates the rebase onto {base}: stage {stage_id} checked the change on its \
         old base, so it no longer says this change passes"
    )]
    VerificationPrecedesRebase {
        stage_id: crate::domain::StageId,
        base: String,
    },
    #[error("run lifecycle rejected the rebase: {0}")]
    RunRebase(#[from] crate::domain::RunRebaseError),
    #[error("run lifecycle rejected the publish: {0}")]
    RunPublish(#[from] crate::domain::RunPublishError),
    /// The source checkout is not ahead of the run's base, so there is no
    /// newer `HEAD` to move onto.
    #[error("run {run_id} is already based on the source checkout's HEAD {base}")]
    RebaseAlreadyCurrent { run_id: RunId, base: String },
    /// The run's base is not reachable from the source checkout's `HEAD`: the
    /// checkout was rewound, force-updated, or moved to an unrelated branch.
    #[error(
        "source checkout HEAD {head} does not contain this run's base {base}, so the change cannot \
         be moved onto it"
    )]
    RebaseBaseNotAncestor { base: String, head: String },
    /// The delta and the commits the checkout gained touch the same lines.
    #[error(
        "rebasing run {run_id} onto {head} conflicts, so the workspace was left on {base}: \
         {reason}"
    )]
    RebaseConflict {
        run_id: RunId,
        base: String,
        head: String,
        reason: String,
    },
    /// A rebase would move the base an apply intent already committed to.
    #[error("run {0} has an apply operation in progress, so its workspace cannot be rebased")]
    RebaseBlockedByApply(RunId),
    #[error("apply state is ambiguous; manual recovery required")]
    AmbiguousApplyState,
    #[error("run {0} was already applied")]
    ApplyAlreadyPerformed(RunId),
    #[error("run {0} has an apply operation in progress")]
    ApplyInProgress(RunId),
    #[error("persisted apply patch hash differs from regenerated workspace patch")]
    PatchHashMismatch,
    #[error("stored workspace is invalid: {0}")]
    InvalidStoredWorkspace(&'static str),
    #[error("workspace mode {0:?} has no branch")]
    MissingBranch(WorkspaceMode),
    #[cfg(test)]
    #[error("injected crash at {0}")]
    InjectedCrash(&'static str),
}
