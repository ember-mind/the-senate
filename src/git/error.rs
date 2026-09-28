use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git executable was not found")]
    GitNotFound,
    #[error("cannot run git command {command}: {source}")]
    CommandIo {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("git command failed ({status:?}): {command}: {stderr}")]
    CommandFailed {
        command: String,
        status: Option<i32>,
        stderr: String,
    },
    #[error("git command returned non-UTF-8 text: {0}")]
    NonUtf8Output(String),
    #[error("git command returned invalid output: {0}")]
    InvalidOutput(String),
    #[error("repository path is unavailable: {0}")]
    RepositoryUnavailable(PathBuf),
    #[error("path is not inside a Git repository: {0}")]
    NotGitRepository(PathBuf),
    #[error("cannot canonicalize Git path {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Git path cannot be stored as UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error("invalid Git commit ID: {0}")]
    InvalidCommit(String),
}
