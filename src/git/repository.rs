use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

use super::GitError;
use super::command::{Git, os, text_output};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitRepository {
    source_path: PathBuf,
    git_common_dir: PathBuf,
    head_commit: String,
}

impl GitRepository {
    /// Discovers canonical repository/worktree identity through native Git.
    ///
    /// # Errors
    /// Returns typed errors for missing paths, non-repositories, invalid paths,
    /// unavailable Git, or malformed command output.
    pub fn discover(path: impl AsRef<Path>) -> Result<Self, GitError> {
        Self::discover_with(&Git::default(), path.as_ref())
    }

    pub(crate) fn discover_with(git: &Git, path: &Path) -> Result<Self, GitError> {
        if !path.exists() {
            return Err(GitError::RepositoryUnavailable(path.to_path_buf()));
        }
        let top = git.output(path, &[os("rev-parse"), os("--show-toplevel")], &[])?;
        if !top.status.success() {
            return Err(GitError::NotGitRepository(path.to_path_buf()));
        }
        let source_path = canonicalize(&path_output(top.stdout, "rev-parse --show-toplevel")?)?;

        let common = git
            .checked(&source_path, &[os("rev-parse"), os("--git-common-dir")])?
            .stdout;
        let common = path_output(common, "rev-parse --git-common-dir")?;
        let common = if common.is_absolute() {
            common
        } else {
            source_path.join(common)
        };
        let git_common_dir = canonicalize(&common)?;

        let head_commit = text_output(git.checked(
            &source_path,
            &[os("rev-parse"), os("--verify"), os("HEAD^{commit}")],
        )?)?;
        validate_commit(&head_commit)?;

        Ok(Self {
            source_path,
            git_common_dir,
            head_commit,
        })
    }

    #[must_use]
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    #[must_use]
    pub fn git_common_dir(&self) -> &Path {
        &self.git_common_dir
    }

    #[must_use]
    pub fn head_commit(&self) -> &str {
        &self.head_commit
    }
}

/// How many commits `descendant` carries that `ancestor` does not.
///
/// Diagnostic only: the number a refused apply reports so the operator reads
/// "the checkout moved 3 commits" instead of two hashes to compare by eye.
pub(crate) fn count_commits_between(
    git: &Git,
    path: &Path,
    ancestor: &str,
    descendant: &str,
) -> Result<u64, GitError> {
    validate_commit(ancestor)?;
    validate_commit(descendant)?;
    let range = format!("{ancestor}..{descendant}");
    let output = text_output(git.checked(
        path,
        &[os("rev-list"), os("--count"), OsString::from(range)],
    )?)?;
    output
        .parse()
        .map_err(|_| GitError::InvalidOutput(format!("rev-list --count returned {output:?}")))
}

pub(crate) fn validate_commit(commit: &str) -> Result<(), GitError> {
    if matches!(commit.len(), 40 | 64) && commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(GitError::InvalidCommit(commit.to_owned()))
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf, GitError> {
    path.canonicalize()
        .map_err(|source| GitError::Canonicalize {
            path: path.to_path_buf(),
            source,
        })
}

fn path_output(mut bytes: Vec<u8>, command: &str) -> Result<PathBuf, GitError> {
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    if bytes.is_empty() {
        return Err(GitError::InvalidOutput(format!(
            "{command} returned an empty path"
        )));
    }
    #[cfg(unix)]
    {
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        let value =
            String::from_utf8(bytes).map_err(|_| GitError::NonUtf8Output(command.to_owned()))?;
        Ok(PathBuf::from(value))
    }
}
