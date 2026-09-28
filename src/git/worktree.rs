use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::command::{Git, os, text_output};
use super::repository::validate_commit;
use super::{GitError, GitRepository};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorktreeIdentity {
    pub path: PathBuf,
    pub git_common_dir: PathBuf,
    pub head_commit: String,
    pub branch: Option<String>,
}

pub(crate) fn create_worktree(
    git: &Git,
    repository: &GitRepository,
    path: &Path,
    base_commit: &str,
    branch: Option<&str>,
) -> Result<(), GitError> {
    let mut args = vec![os("worktree"), os("add")];
    if let Some(branch) = branch {
        git.checked(
            repository.source_path(),
            &[os("check-ref-format"), os("--branch"), os(branch)],
        )?;
        args.extend([os("-b"), os(branch)]);
    } else {
        args.push(os("--detach"));
    }
    args.extend([path.as_os_str().to_os_string(), os(base_commit)]);
    git.checked(repository.source_path(), &args)?;
    Ok(())
}

pub(crate) fn inspect_worktree(git: &Git, path: &Path) -> Result<WorktreeIdentity, GitError> {
    let repository = GitRepository::discover_with(git, path)?;
    let symbolic = git.output(
        repository.source_path(),
        &[os("symbolic-ref"), os("--quiet"), os("--short"), os("HEAD")],
        &[],
    )?;
    let branch = if symbolic.status.success() {
        Some(text_output(symbolic)?)
    } else if symbolic.status.code() == Some(1) {
        None
    } else {
        return Err(symbolic.into_failure());
    };
    Ok(WorktreeIdentity {
        path: repository.source_path().to_path_buf(),
        git_common_dir: repository.git_common_dir().to_path_buf(),
        head_commit: repository.head_commit().to_owned(),
        branch,
    })
}

/// Returns one worktree to a detached HEAD at `commit`.
///
/// Refuses nothing on its own: the caller decides whether losing the current
/// HEAD is safe, because only the caller knows whether the tree is clean.
pub(crate) fn detach_worktree(git: &Git, path: &Path, commit: &str) -> Result<(), GitError> {
    validate_commit(commit)?;
    git.checked(path, &[os("checkout"), os("--detach"), os(commit)])?;
    Ok(())
}

/// Creates `branch` at one worktree's current HEAD and checks it out there.
///
/// The branch starts where the worktree already stands rather than at any
/// named commit: the caller is adopting the tree as it is, not moving it.
pub(crate) fn create_branch_in_worktree(
    git: &Git,
    path: &Path,
    branch: &str,
) -> Result<(), GitError> {
    git.checked(path, &[os("check-ref-format"), os("--branch"), os(branch)])?;
    git.checked(path, &[os("checkout"), os("-b"), os(branch)])?;
    Ok(())
}

/// Stages and commits every change in one worktree, returning the new HEAD.
///
/// The one place The Senate commits: turning a finished run's delta into a
/// commit on the branch the run owns, so the branch can travel to a remote.
/// Uses the worktree's real index deliberately — the tree belongs to the run,
/// and the commit is meant to be its permanent record.
///
/// A machine-owned commit under a raw-mode interface, so nothing interactive
/// or repository-local may run: hooks are skipped and signing is disabled,
/// because a pinentry prompt beneath the alternate screen would hang the
/// publish, and the operator's pre-commit hooks were written for their
/// checkout, not for a worktree an agent already finished with. Staging that
/// normalizes to nothing (a clean/autocrlf round trip) is answered with the
/// unchanged HEAD rather than a "nothing to commit" failure.
pub(crate) fn commit_all_in_worktree(
    git: &Git,
    path: &Path,
    message: &str,
) -> Result<String, GitError> {
    git.checked(path, &[os("add"), os("-A"), os("--"), os(".")])?;
    let staged = git.output(path, &[os("diff"), os("--cached"), os("--quiet")], &[])?;
    if staged.status.code() == Some(1) {
        git.checked(
            path,
            &[
                os("-c"),
                os("commit.gpgsign=false"),
                os("commit"),
                os("--no-verify"),
                os("-m"),
                os(message),
            ],
        )?;
    } else {
        staged.ensure_success()?;
    }
    let head = text_output(git.checked(path, &[os("rev-parse"), os("HEAD")])?)?;
    validate_commit(&head)?;
    Ok(head)
}

pub(crate) fn remove_worktree(
    git: &Git,
    repository: &GitRepository,
    path: &Path,
) -> Result<(), GitError> {
    git.checked(
        repository.source_path(),
        &[
            os("worktree"),
            os("remove"),
            os("--force"),
            path.as_os_str().to_os_string(),
        ],
    )?;
    Ok(())
}

pub(crate) fn branch_exists(
    git: &Git,
    repository: &GitRepository,
    branch: &str,
) -> Result<bool, GitError> {
    Ok(branch_tip(git, repository, branch)?.is_some())
}

pub(crate) fn branch_tip(
    git: &Git,
    repository: &GitRepository,
    branch: &str,
) -> Result<Option<String>, GitError> {
    let reference = format!("refs/heads/{branch}");
    let revision = format!("{reference}^{{commit}}");
    let output = git.output(
        repository.source_path(),
        &[
            os("rev-parse"),
            os("--verify"),
            os("--quiet"),
            os(&revision),
        ],
        &[],
    )?;
    if output.status.success() {
        let commit = text_output(output)?;
        validate_commit(&commit)?;
        Ok(Some(commit))
    } else if output.status.code() == Some(1) {
        Ok(None)
    } else {
        Err(output.into_failure())
    }
}

pub(crate) fn delete_owned_branch(
    git: &Git,
    repository: &GitRepository,
    branch: &str,
    expected_tip: &str,
) -> Result<bool, GitError> {
    let Some(actual_tip) = branch_tip(git, repository, branch)? else {
        return Ok(false);
    };
    if actual_tip != expected_tip {
        return Err(GitError::InvalidOutput(format!(
            "branch {branch} moved from expected owned tip {expected_tip} to {actual_tip}"
        )));
    }
    if branch_is_checked_out(git, repository, branch)? {
        return Err(GitError::InvalidOutput(format!(
            "branch {branch} remains checked out in a worktree"
        )));
    }
    let reference = format!("refs/heads/{branch}");
    git.checked(
        repository.source_path(),
        &[
            os("update-ref"),
            os("-d"),
            OsString::from(reference),
            os(expected_tip),
        ],
    )?;
    Ok(true)
}

/// Moves the commits one worktree's branch carries since `base` onto `onto`,
/// returning the branch's new tip.
///
/// The second machine-owned history rewrite, and the only one that can fail
/// on the operator's content rather than on Git: a conflicting rebase leaves
/// a sequencer mid-flight, so a failure aborts it and restores the branch
/// before returning. The caller therefore sees either a moved branch or the
/// branch it had, never a worktree stopped inside a rebase.
///
/// Hooks and signing are off for the same reason [`commit_all_in_worktree`]
/// turns them off: the operator's hooks were written for their checkout, and
/// a signing prompt beneath the alternate screen would hang.
pub(crate) fn rebase_worktree_onto(
    git: &Git,
    path: &Path,
    base: &str,
    onto: &str,
) -> Result<String, GitError> {
    validate_commit(base)?;
    validate_commit(onto)?;
    let output = git.output(
        path,
        &[
            os("-c"),
            os("core.hooksPath=/dev/null"),
            os("rebase"),
            os("--no-gpg-sign"),
            os("--onto"),
            os(onto),
            os(base),
        ],
        &[],
    )?;
    if !output.status.success() {
        let failure = output.into_failure();
        abort_rebase(git, path)?;
        return Err(failure);
    }
    let head = text_output(git.checked(path, &[os("rev-parse"), os("HEAD")])?)?;
    validate_commit(&head)?;
    Ok(head)
}

/// Returns a worktree to the state it had before a failed rebase.
///
/// `git rebase --abort` fails when there is nothing to abort, which is what a
/// rebase that refused before it started leaves behind. That is not a
/// recovery failure, so it is only reported as one when a sequencer is in
/// fact still there.
fn abort_rebase(git: &Git, path: &Path) -> Result<(), GitError> {
    let output = git.output(path, &[os("rebase"), os("--abort")], &[])?;
    if output.status.success() || !rebase_in_progress(git, path)? {
        Ok(())
    } else {
        Err(output.into_failure())
    }
}

fn rebase_in_progress(git: &Git, path: &Path) -> Result<bool, GitError> {
    let git_dir = text_output(git.checked(path, &[os("rev-parse"), os("--absolute-git-dir")])?)?;
    let git_dir = PathBuf::from(git_dir);
    Ok(git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists())
}

fn branch_is_checked_out(
    git: &Git,
    repository: &GitRepository,
    branch: &str,
) -> Result<bool, GitError> {
    let output = git.checked(
        repository.source_path(),
        &[os("worktree"), os("list"), os("--porcelain"), os("-z")],
    )?;
    let expected = format!("branch refs/heads/{branch}").into_bytes();
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .any(|field| field == expected.as_slice()))
}
