use std::path::Path;

use super::GitError;
use super::command::{Git, os, text_output};
use super::repository::validate_commit;

/// Returns the fetch URL of `remote` as configured for the repository owning
/// `path`, or `None` when no such remote exists.
///
/// Asked of a worktree path rather than a [`super::GitRepository`] because
/// remotes are shared repository configuration: a managed worktree sees
/// exactly the remotes of the checkout it was created from.
pub(crate) fn remote_url(git: &Git, path: &Path, remote: &str) -> Result<Option<String>, GitError> {
    let output = git.output(path, &[os("remote"), os("get-url"), os(remote)], &[])?;
    if output.status.success() {
        return Ok(Some(text_output(output)?));
    }
    // Git reports a missing remote as exit 2 today and exit 128 historically —
    // but 128 is also its generic fatal code, so only the message decides.
    let missing = String::from_utf8_lossy(&output.stderr)
        .to_ascii_lowercase()
        .contains("no such remote");
    if missing {
        Ok(None)
    } else {
        Err(output.into_failure())
    }
}

/// Pushes `branch` to `remote`, creating or updating the remote branch and
/// recording it as upstream.
///
/// Never force-pushes: a remote branch that diverged from what The Senate owns
/// is somebody else's work, and the resulting Git error is the right outcome.
///
/// Credential prompts read the terminal directly, past the nulled stdin, so a
/// push with no usable credentials would hang a publish thread forever under
/// the raw-mode interface. `GIT_TERMINAL_PROMPT=0` turns that hang into an
/// error the operator can read.
///
/// `--no-verify` skips the repository's `pre-push` hook. A managed worktree
/// never installs the project's dependencies, so a hook runner that lives in
/// them — husky's `.husky/_/husky.sh`, for one — is simply absent, and the
/// hook then fails every publish for reasons unrelated to the branch.
pub(crate) fn push_branch(
    git: &Git,
    path: &Path,
    remote: &str,
    branch: &str,
) -> Result<(), GitError> {
    let reference = format!("refs/heads/{branch}");
    git.checked_with(
        path,
        &[
            os("push"),
            os("--no-verify"),
            os("--set-upstream"),
            os(remote),
            os(format!("{reference}:{reference}")),
        ],
        &[(os("GIT_TERMINAL_PROMPT"), os("0"))],
    )?;
    Ok(())
}

/// The ref a fetched publish target is parked on, inside The Senate's own
/// namespace so nothing under `refs/heads` or `refs/remotes` is disturbed.
pub(crate) const PUBLISH_TARGET_REF: &str = "refs/senate/publish-target";

/// Fetches `branch` from `remote` onto `target_ref` and returns the commit it
/// points at, or `None` when the remote has no such branch.
///
/// The fetched tip is parked on a Senate-owned ref rather than read back
/// from `FETCH_HEAD`, whose per-worktree ownership is not something a caller
/// should have to reason about. That ref is force-updated because it is a
/// scratch pointer, never history anyone owns.
pub(crate) fn fetch_branch(
    git: &Git,
    path: &Path,
    remote: &str,
    branch: &str,
    target_ref: &str,
) -> Result<Option<String>, GitError> {
    git.checked(path, &[os("check-ref-format"), os("--branch"), os(branch)])?;
    let refspec = format!("+refs/heads/{branch}:{target_ref}");
    let output = git.output(
        path,
        &[os("fetch"), os("--no-tags"), os(remote), os(refspec)],
        &[(os("GIT_TERMINAL_PROMPT"), os("0"))],
    )?;
    if !output.status.success() {
        // A branch the remote does not have is an answer, not a failure: the
        // caller asked whether there was one to build on.
        let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if stderr.contains("couldn't find remote ref")
            || stderr.contains("could not find remote ref")
        {
            return Ok(None);
        }
        return Err(output.into_failure());
    }
    let commit = text_output(git.checked(path, &[os("rev-parse"), os(target_ref)])?)?;
    validate_commit(&commit)?;
    Ok(Some(commit))
}

/// Deletes a scratch ref The Senate parked a fetched commit on.
pub(crate) fn delete_ref(git: &Git, path: &Path, reference: &str) -> Result<(), GitError> {
    git.checked(path, &[os("update-ref"), os("-d"), os(reference)])?;
    Ok(())
}

/// Whether `ancestor` is reachable from `descendant`, a commit counting as its
/// own ancestor.
pub(crate) fn is_ancestor(
    git: &Git,
    path: &Path,
    ancestor: &str,
    descendant: &str,
) -> Result<bool, GitError> {
    validate_commit(ancestor)?;
    validate_commit(descendant)?;
    let output = git.output(
        path,
        &[
            os("merge-base"),
            os("--is-ancestor"),
            os(ancestor),
            os(descendant),
        ],
        &[],
    )?;
    if output.status.success() {
        Ok(true)
    } else if output.status.code() == Some(1) {
        Ok(false)
    } else {
        Err(output.into_failure())
    }
}

/// Pushes one commit onto `branch` at `remote`, a branch The Senate does not own.
///
/// The counterpart to [`push_branch`] for a pull request the run was asked to
/// fix: those commits belong on the branch that pull request already reads
/// from, not on a branch of their own. Never force-pushes and never sets
/// upstream — the local branch keeps its own identity, and a remote branch
/// that moved ahead is somebody else's work, which the caller establishes is
/// not the case before asking.
pub(crate) fn push_commit_to_branch(
    git: &Git,
    path: &Path,
    remote: &str,
    commit: &str,
    branch: &str,
) -> Result<(), GitError> {
    validate_commit(commit)?;
    git.checked(path, &[os("check-ref-format"), os("--branch"), os(branch)])?;
    git.checked_with(
        path,
        &[
            os("push"),
            os("--no-verify"),
            os(remote),
            os(format!("{commit}:refs/heads/{branch}")),
        ],
        &[(os("GIT_TERMINAL_PROMPT"), os("0"))],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn git_cmd(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn fixture() -> (TempDir, PathBuf, PathBuf) {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git_cmd(&repo, &["init", "-b", "main"]);
        git_cmd(&repo, &["config", "user.name", "The Senate Test"]);
        git_cmd(&repo, &["config", "user.email", "senate@example.invalid"]);
        git_cmd(&repo, &["config", "commit.gpgsign", "false"]);
        fs::write(repo.join("README.md"), "base\n").unwrap();
        git_cmd(&repo, &["add", "-A"]);
        git_cmd(&repo, &["commit", "-m", "base"]);
        let origin = temp.path().join("origin.git");
        git_cmd(temp.path(), &["init", "--bare", "origin.git"]);
        (temp, repo, origin)
    }

    #[test]
    fn a_missing_remote_is_an_answer_not_an_error() {
        let (_temp, repo, origin) = fixture();
        let git = Git::default();
        assert_eq!(remote_url(&git, &repo, "origin").unwrap(), None);
        git_cmd(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        assert_eq!(
            remote_url(&git, &repo, "origin").unwrap().as_deref(),
            origin.to_str()
        );
    }

    #[test]
    fn push_creates_and_updates_the_remote_branch_with_upstream() {
        let (_temp, repo, origin) = fixture();
        git_cmd(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git_cmd(&repo, &["checkout", "-b", "senate/run-1"]);
        fs::write(repo.join("work.txt"), "one\n").unwrap();
        git_cmd(&repo, &["add", "-A"]);
        git_cmd(&repo, &["commit", "-m", "one"]);
        let git = Git::default();

        push_branch(&git, &repo, "origin", "senate/run-1").unwrap();
        let pushed = git_cmd(&origin, &["rev-parse", "refs/heads/senate/run-1"]);
        assert_eq!(pushed, git_cmd(&repo, &["rev-parse", "HEAD"]));
        assert_eq!(
            git_cmd(
                &repo,
                &["rev-parse", "--abbrev-ref", "senate/run-1@{upstream}"]
            )
            .trim(),
            "origin/senate/run-1"
        );

        fs::write(repo.join("work.txt"), "two\n").unwrap();
        git_cmd(&repo, &["add", "-A"]);
        git_cmd(&repo, &["commit", "-m", "two"]);
        push_branch(&git, &repo, "origin", "senate/run-1").unwrap();
        assert_eq!(
            git_cmd(&origin, &["rev-parse", "refs/heads/senate/run-1"]),
            git_cmd(&repo, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn a_failing_pre_push_hook_does_not_block_publishing() {
        let (_temp, repo, origin) = fixture();
        git_cmd(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git_cmd(&repo, &["checkout", "-b", "senate/run-1"]);
        // What a managed worktree of a husky repository looks like: the hook is
        // checked in, the runner it sources lives in uninstalled dependencies.
        let hooks = repo.join(".husky");
        fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-push");
        fs::write(&hook, "#!/bin/sh\n. .husky/_/husky.sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
        git_cmd(&repo, &["config", "core.hooksPath", ".husky"]);
        fs::write(repo.join("work.txt"), "one\n").unwrap();
        git_cmd(&repo, &["add", "-A"]);
        git_cmd(&repo, &["commit", "-m", "one"]);
        let git = Git::default();

        push_branch(&git, &repo, "origin", "senate/run-1").unwrap();
        assert_eq!(
            git_cmd(&origin, &["rev-parse", "refs/heads/senate/run-1"]),
            git_cmd(&repo, &["rev-parse", "HEAD"])
        );
    }
}
