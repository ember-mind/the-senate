//! Retries a subprocess launch that raced another test's still-forking
//! thread.
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
