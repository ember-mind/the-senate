use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};

use super::GitError;

#[derive(Clone, Debug)]
pub(crate) struct Git {
    executable: OsString,
}

#[derive(Debug)]
pub(crate) struct GitOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub command: String,
}

impl GitOutput {
    pub fn ensure_success(self) -> Result<Self, GitError> {
        if self.status.success() {
            Ok(self)
        } else {
            Err(GitError::CommandFailed {
                command: self.command,
                status: self.status.code(),
                stderr: String::from_utf8_lossy(&self.stderr).trim().to_owned(),
            })
        }
    }

    pub fn into_failure(self) -> GitError {
        GitError::CommandFailed {
            command: self.command,
            status: self.status.code(),
            stderr: String::from_utf8_lossy(&self.stderr).trim().to_owned(),
        }
    }
}

/// Reports the available Git version, for diagnostics only.
///
/// Runs `git --version` in a directory that is guaranteed not to be a
/// repository under test, so the probe cannot create or read repository or
/// data state.
pub(crate) fn git_version(git: &Git) -> Option<String> {
    let output = git
        .output(Path::new("/"), &[os("--version")], &[])
        .ok()?
        .ensure_success()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.split_whitespace().nth(2)?;
    Some(version.to_owned())
}

impl Default for Git {
    fn default() -> Self {
        Self {
            executable: OsString::from("git"),
        }
    }
}

impl Git {
    pub fn output(
        &self,
        cwd: &Path,
        args: &[OsString],
        environment: &[(OsString, OsString)],
    ) -> Result<GitOutput, GitError> {
        let description = describe_command(&self.executable, cwd, args);
        let mut command = Command::new(&self.executable);
        command
            .current_dir(cwd)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
            "GIT_CEILING_DIRECTORIES",
        ] {
            command.env_remove(key);
        }
        for (key, value) in environment {
            command.env(key, value);
        }
        command.stdin(Stdio::null());

        let child = command.spawn().map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => GitError::GitNotFound,
            _ => GitError::CommandIo {
                command: description.clone(),
                source,
            },
        })?;
        let output = child
            .wait_with_output()
            .map_err(|source| GitError::CommandIo {
                command: description.clone(),
                source,
            })?;
        Ok(GitOutput {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
            command: description,
        })
    }

    pub fn checked(&self, cwd: &Path, args: &[OsString]) -> Result<GitOutput, GitError> {
        self.output(cwd, args, &[])?.ensure_success()
    }

    pub fn checked_with(
        &self,
        cwd: &Path,
        args: &[OsString],
        environment: &[(OsString, OsString)],
    ) -> Result<GitOutput, GitError> {
        self.output(cwd, args, environment)?.ensure_success()
    }

    pub fn checked_to_file(
        &self,
        cwd: &Path,
        args: &[OsString],
        environment: &[(OsString, OsString)],
        stdout: File,
    ) -> Result<(), GitError> {
        let description = describe_command(&self.executable, cwd, args);
        let mut command = Command::new(&self.executable);
        command
            .current_dir(cwd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::piped());
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
            "GIT_CEILING_DIRECTORIES",
        ] {
            command.env_remove(key);
        }
        for (key, value) in environment {
            command.env(key, value);
        }
        let output = command.output().map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => GitError::GitNotFound,
            _ => GitError::CommandIo {
                command: description.clone(),
                source,
            },
        })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(GitError::CommandFailed {
                command: description,
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        }
    }
}

pub(crate) fn text_output(output: GitOutput) -> Result<String, GitError> {
    let text = String::from_utf8(output.stdout)
        .map_err(|_| GitError::NonUtf8Output(output.command.clone()))?;
    Ok(text.trim_end_matches(['\r', '\n']).to_owned())
}

pub(crate) fn os(value: impl AsRef<OsStr>) -> OsString {
    value.as_ref().to_os_string()
}

// Debug on purpose: quoting each argument keeps its boundaries visible when
// one contains a space.
#[allow(clippy::unnecessary_debug_formatting)]
fn describe_command(executable: &OsStr, cwd: &Path, args: &[OsString]) -> String {
    let arguments = args
        .iter()
        .map(|argument| format!("{argument:?}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{:?} {arguments} (working directory: {})",
        executable.to_string_lossy(),
        cwd.display()
    )
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn missing_executable_is_typed_without_panicking() {
        let directory = TempDir::new().unwrap();
        let git = Git {
            executable: directory.path().join("missing-git").into_os_string(),
        };

        let error = git
            .output(directory.path(), &[os("--version")], &[])
            .unwrap_err();

        assert!(matches!(error, GitError::GitNotFound));
    }
}
