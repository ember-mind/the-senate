use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::domain::RunId;

use super::WorkspaceError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceMode {
    Branch,
    Detached,
}

impl WorkspaceMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Detached => "detached",
        }
    }

    pub(crate) fn from_str(value: &str) -> Result<Self, WorkspaceError> {
        match value {
            "branch" => Ok(Self::Branch),
            "detached" => Ok(Self::Detached),
            _ => Err(WorkspaceError::InvalidStoredWorkspace(
                "unknown workspace mode",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceStatus {
    Preparing,
    Ready,
    Removing,
    Removed,
    Broken,
}

impl WorkspaceStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Ready => "ready",
            Self::Removing => "removing",
            Self::Removed => "removed",
            Self::Broken => "broken",
        }
    }

    pub(crate) fn from_str(value: &str) -> Result<Self, WorkspaceError> {
        match value {
            "preparing" => Ok(Self::Preparing),
            "ready" => Ok(Self::Ready),
            "removing" => Ok(Self::Removing),
            "removed" => Ok(Self::Removed),
            "broken" => Ok(Self::Broken),
            _ => Err(WorkspaceError::InvalidStoredWorkspace(
                "unknown workspace status",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceRevision(u64);

impl WorkspaceRevision {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunWorkspace {
    run_id: RunId,
    source_repo_path: PathBuf,
    git_common_dir: PathBuf,
    base_commit: String,
    worktree_path: PathBuf,
    branch_name: Option<String>,
    mode: WorkspaceMode,
    status: WorkspaceStatus,
    branch_owned: bool,
    removal_head: Option<String>,
    last_error: Option<String>,
    revision: WorkspaceRevision,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl RunWorkspace {
    #[allow(
        clippy::too_many_arguments,
        reason = "constructor captures complete immutable workspace identity"
    )]
    pub(crate) fn preparing(
        run_id: RunId,
        source_repo_path: PathBuf,
        git_common_dir: PathBuf,
        base_commit: String,
        worktree_path: PathBuf,
        branch_name: Option<String>,
        mode: WorkspaceMode,
        now: DateTime<Utc>,
    ) -> Result<Self, WorkspaceError> {
        let workspace = Self {
            run_id,
            source_repo_path,
            git_common_dir,
            base_commit,
            worktree_path,
            branch_name,
            mode,
            status: WorkspaceStatus::Preparing,
            branch_owned: false,
            removal_head: None,
            last_error: None,
            revision: WorkspaceRevision::default(),
            created_at: now,
            updated_at: now,
        };
        workspace.validate()?;
        Ok(workspace)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_stored(
        run_id: RunId,
        source_repo_path: PathBuf,
        git_common_dir: PathBuf,
        base_commit: String,
        worktree_path: PathBuf,
        branch_name: Option<String>,
        mode: WorkspaceMode,
        status: WorkspaceStatus,
        branch_owned: bool,
        removal_head: Option<String>,
        last_error: Option<String>,
        revision: WorkspaceRevision,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<Self, WorkspaceError> {
        let workspace = Self {
            run_id,
            source_repo_path,
            git_common_dir,
            base_commit,
            worktree_path,
            branch_name,
            mode,
            status,
            branch_owned,
            removal_head,
            last_error,
            revision,
            created_at,
            updated_at,
        };
        workspace.validate()?;
        Ok(workspace)
    }

    fn validate(&self) -> Result<(), WorkspaceError> {
        if !self.source_repo_path.is_absolute()
            || !self.git_common_dir.is_absolute()
            || !self.worktree_path.is_absolute()
        {
            return Err(WorkspaceError::InvalidStoredWorkspace(
                "workspace paths must be absolute",
            ));
        }
        if !valid_commit(&self.base_commit)
            || self
                .removal_head
                .as_deref()
                .is_some_and(|head| !valid_commit(head))
        {
            return Err(WorkspaceError::InvalidStoredWorkspace(
                "invalid Git commit ID",
            ));
        }
        match (self.mode, self.branch_name.as_deref()) {
            (WorkspaceMode::Branch, Some(branch)) if !branch.is_empty() => {}
            (WorkspaceMode::Detached, None) => {}
            _ => {
                return Err(WorkspaceError::InvalidStoredWorkspace(
                    "workspace mode and branch disagree",
                ));
            }
        }
        if self.mode == WorkspaceMode::Detached && self.branch_owned {
            return Err(WorkspaceError::InvalidStoredWorkspace(
                "detached workspace cannot own a branch",
            ));
        }
        if self.status == WorkspaceStatus::Preparing && self.branch_owned {
            return Err(WorkspaceError::InvalidStoredWorkspace(
                "preparing workspace cannot claim branch ownership",
            ));
        }
        if self.status == WorkspaceStatus::Ready
            && self.mode == WorkspaceMode::Branch
            && !self.branch_owned
        {
            return Err(WorkspaceError::InvalidStoredWorkspace(
                "ready branch workspace must own its branch",
            ));
        }
        if self.status == WorkspaceStatus::Removing && self.removal_head.is_none() {
            return Err(WorkspaceError::InvalidStoredWorkspace(
                "removing workspace must record expected removal HEAD",
            ));
        }
        if self.updated_at < self.created_at {
            return Err(WorkspaceError::InvalidStoredWorkspace(
                "workspace timestamp regression",
            ));
        }
        Ok(())
    }

    pub(crate) fn mark_ready(&mut self, now: DateTime<Utc>) {
        self.status = WorkspaceStatus::Ready;
        self.branch_owned = self.mode == WorkspaceMode::Branch;
        self.removal_head = None;
        self.last_error = None;
        self.updated_at = now.max(self.updated_at);
    }

    pub(crate) fn mark_removing(&mut self, head: String, now: DateTime<Utc>) {
        self.status = WorkspaceStatus::Removing;
        self.removal_head = Some(head);
        self.last_error = None;
        self.updated_at = now.max(self.updated_at);
    }

    /// Gives a read-only workspace a branch of its own to write on.
    ///
    /// A review is prepared detached because it is not meant to produce
    /// changes. Asking it to fix what it found changes that, and apply will
    /// only transfer a branch The Senate owns, so the mode and the branch move
    /// together — the store rejects one without the other.
    pub(crate) fn adopt_branch(&mut self, branch: String, now: DateTime<Utc>) {
        self.branch_name = Some(branch);
        self.mode = WorkspaceMode::Branch;
        self.branch_owned = true;
        self.updated_at = now.max(self.updated_at);
    }

    /// Records that the worktree's delta now stands on a different commit.
    ///
    /// The base is otherwise creation-time identity: every patch this run
    /// produces is read against it, so moving it is moving what the run's
    /// change means. Rebase is the single caller allowed to, and only after
    /// Git has already moved the worktree there.
    pub(crate) fn rebase_onto(&mut self, base: String, now: DateTime<Utc>) {
        self.base_commit = base;
        self.updated_at = now.max(self.updated_at);
    }

    /// Gives up the claim on the branch, so removal takes the worktree and
    /// leaves the branch standing.
    ///
    /// The disposition has to be persisted rather than passed along, because
    /// removal is resumable: a crash between the intent and the deletion leaves
    /// only the stored workspace to say what was meant. Ownership is what
    /// removal reads, so releasing it here is what a later resume obeys.
    pub(crate) fn release_branch_ownership(&mut self) {
        self.branch_owned = false;
    }

    pub(crate) fn confirm_branch_ownership(&mut self) {
        if self.mode == WorkspaceMode::Branch {
            self.branch_owned = true;
        }
    }

    pub(crate) fn mark_removed(&mut self, now: DateTime<Utc>) {
        self.status = WorkspaceStatus::Removed;
        self.last_error = None;
        self.updated_at = now.max(self.updated_at);
    }

    pub(crate) fn mark_broken(&mut self, error: impl Into<String>, now: DateTime<Utc>) {
        self.status = WorkspaceStatus::Broken;
        self.last_error = Some(error.into());
        self.updated_at = now.max(self.updated_at);
    }

    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }
    #[must_use]
    pub fn source_repo_path(&self) -> &Path {
        &self.source_repo_path
    }
    #[must_use]
    pub fn git_common_dir(&self) -> &Path {
        &self.git_common_dir
    }
    #[must_use]
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }
    #[must_use]
    pub fn worktree_path(&self) -> &Path {
        &self.worktree_path
    }
    #[must_use]
    pub fn branch_name(&self) -> Option<&str> {
        self.branch_name.as_deref()
    }
    #[must_use]
    pub const fn mode(&self) -> WorkspaceMode {
        self.mode
    }
    #[must_use]
    pub const fn status(&self) -> WorkspaceStatus {
        self.status
    }
    #[must_use]
    pub const fn branch_owned(&self) -> bool {
        self.branch_owned
    }
    #[must_use]
    pub fn removal_head(&self) -> Option<&str> {
        self.removal_head.as_deref()
    }
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
    #[must_use]
    pub const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }
    #[must_use]
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }
    #[must_use]
    pub const fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyStatus {
    Prepared,
    AppliedToSource,
    Recorded,
    Failed,
}

impl ApplyStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::AppliedToSource => "applied_to_source",
            Self::Recorded => "recorded",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn from_str(value: &str) -> Result<Self, WorkspaceError> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "applied_to_source" => Ok(Self::AppliedToSource),
            "recorded" => Ok(Self::Recorded),
            "failed" => Ok(Self::Failed),
            _ => Err(WorkspaceError::InvalidStoredWorkspace(
                "unknown apply status",
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunApplyOperation {
    run_id: RunId,
    status: ApplyStatus,
    patch_hash: String,
    run_revision: u64,
    last_error: Option<String>,
    revision: u64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl RunApplyOperation {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_stored(
        run_id: RunId,
        status: ApplyStatus,
        patch_hash: String,
        run_revision: u64,
        last_error: Option<String>,
        revision: u64,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<Self, WorkspaceError> {
        if patch_hash.len() != 64 || !patch_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(WorkspaceError::InvalidStoredWorkspace("invalid patch hash"));
        }
        if updated_at < created_at {
            return Err(WorkspaceError::InvalidStoredWorkspace(
                "apply timestamp regression",
            ));
        }
        Ok(Self {
            run_id,
            status,
            patch_hash,
            run_revision,
            last_error,
            revision,
            created_at,
            updated_at,
        })
    }

    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }
    #[must_use]
    pub const fn status(&self) -> ApplyStatus {
        self.status
    }
    #[must_use]
    pub fn patch_hash(&self) -> &str {
        &self.patch_hash
    }
    #[must_use]
    pub const fn run_revision(&self) -> u64 {
        self.run_revision
    }
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
    #[must_use]
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }
    #[must_use]
    pub const fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }
}

fn valid_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64) && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
}
