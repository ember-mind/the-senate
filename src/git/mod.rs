//! Focused native Git CLI boundary for repository and worktree operations.

mod command;
mod error;
mod patch;
mod remote;
mod repository;
mod worktree;

pub use error::GitError;
pub use repository::GitRepository;

pub(crate) use repository::count_commits_between;

pub(crate) use command::{Git, git_version};
#[cfg(test)]
pub(crate) use patch::ChangeKind;
pub(crate) use patch::{
    ChangedFileRecord, PatchPreview, apply_patch, check_patch, generate_change_evidence,
    generate_patch, generate_patch_preview, source_is_clean, tree_is_clean,
};
pub(crate) use remote::{
    PUBLISH_TARGET_REF, delete_ref, fetch_branch, is_ancestor, push_branch, push_commit_to_branch,
    remote_url,
};
pub(crate) use worktree::{
    WorktreeIdentity, branch_exists, branch_tip, commit_all_in_worktree, create_branch_in_worktree,
    create_worktree, delete_owned_branch, detach_worktree, inspect_worktree, rebase_worktree_onto,
    remove_worktree,
};
