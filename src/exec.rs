//! Shared subprocess credential filtering, process-group cleanup and launch
//! retries for a test's still-forking thread.
//!
//! Tests that write a stub executable (a fake `gh`, `codex`, or a staged
//! release binary) and immediately exec it can hit `ETXTBSY`
//! (`ExecutableFileBusy`): another test thread's `fork` briefly inherits the
//! write file descriptor the stub was created with, and the kernel refuses
//! to exec a file that is still open for writing. The window is a few
//! milliseconds wide, so a short, bounded retry clears it without masking a
//! real failure — every other error kind is returned on the first attempt.

use std::io;
use std::thread;
use std::time::Duration;

/// Total attempts made before giving up on a busy executable.
const MAX_ATTEMPTS: u32 = 5;
/// Backoff before the first retry; doubles on each attempt after that.
const INITIAL_BACKOFF: Duration = Duration::from_millis(10);

/// Reaps a command started as its own process-group leader, including the
/// descendants that may still hold its output pipes after the leader exits.
pub(crate) fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::Kill);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Jira configuration belongs to the input importer, not child tools.
pub(crate) fn jira_environment_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with("SENATE_JIRA_"))
}

/// Preserve native tool authentication while withholding Jira importer secrets.
pub(crate) fn without_jira_credentials(
    mut command: std::process::Command,
) -> std::process::Command {
    for (key, _) in std::env::vars_os().filter(|(key, _)| jira_environment_name(key)) {
        command.env_remove(key);
    }
    // Record removal of known credentials even when absent from this environment.
    command
        .env_remove("SENATE_JIRA_TOKEN")
        .env_remove("SENATE_JIRA_EMAIL");
    command
}

/// Runs `attempt`, retrying only when it fails with
/// [`io::ErrorKind::ExecutableFileBusy`], up to [`MAX_ATTEMPTS`] times with a
/// short doubling backoff. Any other error is returned immediately.
pub(crate) fn retry_busy<T>(mut attempt: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempts_left = MAX_ATTEMPTS;
    let mut backoff = INITIAL_BACKOFF;
    loop {
        attempts_left -= 1;
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error)
                if error.kind() == io::ErrorKind::ExecutableFileBusy && attempts_left > 0 =>
            {
                thread::sleep(backoff);
                backoff *= 2;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jira_credentials_are_removed_without_clearing_native_auth() {
        let command = without_jira_credentials(std::process::Command::new("native-provider"));
        let removals = command.get_envs().collect::<Vec<_>>();
        assert!(removals.contains(&(std::ffi::OsStr::new("SENATE_JIRA_TOKEN"), None)));
        assert!(removals.contains(&(std::ffi::OsStr::new("SENATE_JIRA_EMAIL"), None)));
        assert!(!removals.iter().any(|(key, _)| *key == "ANTHROPIC_API_KEY"));
        assert!(jira_environment_name(std::ffi::OsStr::new(
            "SENATE_JIRA_TOKEN"
        )));
        assert!(!jira_environment_name(std::ffi::OsStr::new(
            "OPENAI_API_KEY"
        )));
    }

    #[test]
    fn a_non_busy_error_returns_on_the_first_attempt() {
        let mut attempts = 0;
        let result = retry_busy(|| {
            attempts += 1;
            Err::<(), _>(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
        });
        assert_eq!(attempts, 1);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_busy_executable_is_retried_until_it_succeeds() {
        let mut attempts = 0;
        let result = retry_busy(|| {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::from(io::ErrorKind::ExecutableFileBusy))
            } else {
                Ok(42)
            }
        });
        assert_eq!(attempts, 3);
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn a_busy_executable_that_never_clears_is_reported_after_the_bound() {
        let mut attempts = 0;
        let result = retry_busy(|| {
            attempts += 1;
            Err::<(), _>(io::Error::from(io::ErrorKind::ExecutableFileBusy))
        });
        assert_eq!(attempts, MAX_ATTEMPTS);
        assert_eq!(
            result.unwrap_err().kind(),
            io::ErrorKind::ExecutableFileBusy
        );
    }
}
