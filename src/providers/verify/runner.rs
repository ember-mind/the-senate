//! Runs one verification command in a worktree and reports what happened.
//!
//! Direct argv only. The configured string is split on whitespace and the
//! first word is the program; there is no shell, so no pipes, globs,
//! redirections or environment expansion. That is deliberate: what the
//! artifact shows under `$` is exactly what ran, and a repository cannot
//! smuggle a shell into The Senate through its own configuration file.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::VerifyError;

/// How often the runner checks whether the child has exited. Short enough
/// that a timeout is honoured promptly, long enough to cost nothing.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Bytes kept per stream while a command runs. A suite that logs gigabytes
/// must not take the driver down with it; the tail is what a reader wants
/// anyway, and the artifact cuts it further to a readable length.
pub(crate) const CAPTURE_TAIL_BYTES: usize = 64 * 1024;

/// How one command ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommandExit {
    /// The process exited on its own with this code.
    Code(i32),
    /// The process was ended by a signal before it could exit.
    Signal(i32),
    /// The Senate killed it — and everything it had spawned — after the
    /// configured limit.
    TimedOut(Duration),
    /// The program could not be started at all, typically because it is
    /// not installed on this machine.
    CouldNotStart(String),
    /// The child started but its status could not be read back; it was
    /// killed rather than left running unobserved.
    StatusUnavailable(String),
}

impl CommandExit {
    pub(crate) const fn succeeded(&self) -> bool {
        matches!(self, Self::Code(0))
    }
}

/// The tail of one stream and how much of it was let go while reading.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Captured {
    pub bytes: Vec<u8>,
    /// Bytes that arrived before the kept tail and were dropped on the way;
    /// the artifact says so rather than pretending the tail is the whole.
    pub dropped: u64,
}

/// Everything the artifact needs to say about one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandReport {
    pub command: String,
    pub exit: CommandExit,
    pub stdout: Captured,
    pub stderr: Captured,
}

/// Runs `command` in `cwd`, capturing both streams, and kills it once
/// `timeout` elapses.
///
/// Output is drained on two threads while the parent waits: a child that
/// fills a pipe would otherwise block on write and never exit, turning every
/// verbose test suite into a timeout. On Unix the child leads its own
/// process group, so a timeout kills the whole tree: test runners fork
/// workers that inherit the pipes, and killing only the parent would leave
/// them holding the write ends open, with the drain never seeing EOF.
///
/// # Errors
/// Only when the caller's own arguments are wrong (an empty command). A
/// program that does not exist, exits non-zero or is killed is reported in
/// the returned [`CommandExit`], because those are verification findings
/// rather than infrastructure failures.
pub(crate) fn run(
    command: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<CommandReport, VerifyError> {
    let mut words = command.split_whitespace();
    let program = words
        .next()
        .ok_or_else(|| VerifyError::Config("verify command is empty".to_owned()))?;
    let mut spawn = Command::new(program);
    spawn
        .args(words)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        spawn.process_group(0);
    }
    // Tests exec a freshly written stub script immediately, which can race
    // another test thread's fork still holding its write fd open
    // (`ETXTBSY`); retry_busy clears that window before this falls back to
    // reporting the program as unable to start.
    let mut child = match crate::exec::retry_busy(|| spawn.spawn()) {
        Ok(child) => child,
        Err(error) => {
            return Ok(CommandReport {
                command: command.to_owned(),
                exit: CommandExit::CouldNotStart(error.to_string()),
                stdout: Captured::default(),
                stderr: Captured::default(),
            });
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (exit, stdout, stderr) = std::thread::scope(|scope| {
        let stdout = scope.spawn(move || drain(stdout));
        let stderr = scope.spawn(move || drain(stderr));
        let exit = wait_with_timeout(&mut child, timeout);
        (
            exit,
            stdout.join().unwrap_or_default(),
            stderr.join().unwrap_or_default(),
        )
    });
    Ok(CommandReport {
        command: command.to_owned(),
        exit,
        stdout,
        stderr,
    })
}

/// Reads a stream to its end keeping only the last [`CAPTURE_TAIL_BYTES`],
/// so memory stays bounded however much the command says.
fn drain(stream: Option<impl Read>) -> Captured {
    let mut captured = Captured::default();
    let Some(mut stream) = stream else {
        return captured;
    };
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        // A read error mid-stream keeps what arrived; the exit status still
        // says what the command did.
        let read = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        captured.bytes.extend_from_slice(&chunk[..read]);
        // Trim only once the buffer has doubled, so the cost stays
        // amortised rather than a memmove per chunk.
        if captured.bytes.len() > 2 * CAPTURE_TAIL_BYTES {
            let cut = captured.bytes.len() - CAPTURE_TAIL_BYTES;
            captured.bytes.drain(..cut);
            captured.dropped += cut as u64;
        }
    }
    if captured.bytes.len() > CAPTURE_TAIL_BYTES {
        let cut = captured.bytes.len() - CAPTURE_TAIL_BYTES;
        captured.bytes.drain(..cut);
        captured.dropped += cut as u64;
    }
    captured
}

/// Polls the child until it exits or the limit passes, then kills its whole
/// tree. Every kill is followed by a wait so the leader is reaped; its
/// descendants die with the group, which closes the pipes and lets the
/// draining threads finish.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> CommandExit {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return exit_of(status),
            Ok(None) => {}
            Err(error) => {
                kill_tree(child);
                return CommandExit::StatusUnavailable(error.to_string());
            }
        }
        if started.elapsed() >= timeout {
            kill_tree(child);
            return CommandExit::TimedOut(timeout);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Kills the child and, on Unix, every process in the group it leads.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        // The child was spawned as a group leader, so its pid is the group
        // id. A failure here (the group is already gone) changes nothing.
        if let Some(pid) = i32::try_from(child.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::Kill);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn exit_of(status: std::process::ExitStatus) -> CommandExit {
    if let Some(code) = status.code() {
        return CommandExit::Code(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return CommandExit::Signal(signal);
        }
    }
    CommandExit::Signal(-1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_passing_command_reports_exit_zero_and_its_output() {
        let dir = cwd();
        let report = run("echo hello world", dir.path(), Duration::from_secs(5)).unwrap();

        assert_eq!(report.command, "echo hello world");
        assert_eq!(report.exit, CommandExit::Code(0));
        assert!(report.exit.succeeded());
        assert_eq!(report.stdout.bytes, b"hello world\n");
        assert_eq!(report.stdout.dropped, 0);
        assert!(report.stderr.bytes.is_empty());
    }

    #[test]
    fn a_failing_command_reports_its_exit_code() {
        let dir = cwd();
        let report = run("false", dir.path(), Duration::from_secs(5)).unwrap();

        assert_eq!(report.exit, CommandExit::Code(1));
        assert!(!report.exit.succeeded());
    }

    #[test]
    fn a_command_past_its_limit_is_killed_and_reported_as_timed_out() {
        let dir = cwd();
        let started = Instant::now();
        let report = run("sleep 5", dir.path(), Duration::from_secs(1)).unwrap();

        assert_eq!(report.exit, CommandExit::TimedOut(Duration::from_secs(1)));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the runner must not wait for the child's own exit"
        );
    }

    /// Test runners fork workers that inherit the pipes. Killing only the
    /// leader would leave the worker holding the write end, the drain
    /// waiting for an EOF that never comes, and the timeout never reported.
    #[test]
    fn a_timeout_kills_the_grandchildren_that_hold_the_pipes() {
        let dir = cwd();
        let script = dir.path().join("slow.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 6\necho done\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let started = Instant::now();

        let report = run(
            &script.display().to_string(),
            dir.path(),
            Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(report.exit, CommandExit::TimedOut(Duration::from_secs(1)));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the grandchild `sleep` kept the pipe open for {:?}",
            started.elapsed()
        );
        assert!(report.stdout.bytes.is_empty(), "`echo` never ran");
    }

    /// Memory stays bounded however much a command prints; only the tail
    /// is kept and the amount let go is counted.
    #[test]
    fn capture_keeps_only_the_tail_and_counts_what_it_dropped() {
        let dir = cwd();
        let script = dir.path().join("noisy.sh");
        // 300 KiB of numbered lines, well past the 64 KiB tail.
        std::fs::write(
            &script,
            "#!/bin/sh\ni=0\nwhile [ $i -lt 30000 ]; do echo \"line $i\"; i=$((i+1)); done\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let report = run(
            &script.display().to_string(),
            dir.path(),
            Duration::from_secs(30),
        )
        .unwrap();

        assert_eq!(report.exit, CommandExit::Code(0));
        assert!(report.stdout.bytes.len() <= CAPTURE_TAIL_BYTES);
        assert!(report.stdout.dropped > 0);
        assert!(String::from_utf8_lossy(&report.stdout.bytes).ends_with("line 29999\n"));
        let total = (0..30000)
            .map(|i| format!("line {i}\n").len())
            .sum::<usize>();
        assert_eq!(
            report.stdout.dropped + u64::try_from(report.stdout.bytes.len()).unwrap(),
            u64::try_from(total).unwrap()
        );
    }

    #[test]
    fn a_program_that_does_not_exist_is_a_finding_not_an_infrastructure_error() {
        let dir = cwd();
        let report = run(
            "senate-no-such-program-0xdeadbeef --flag",
            dir.path(),
            Duration::from_secs(5),
        )
        .unwrap();

        assert!(matches!(report.exit, CommandExit::CouldNotStart(_)));
    }

    #[test]
    fn the_command_runs_in_the_given_directory_without_a_shell() {
        let dir = cwd();
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
        // With a shell, `*` would expand; with direct argv it is passed as-is
        // and `ls` complains about a file literally named `*`.
        let report = run("ls *", dir.path(), Duration::from_secs(5)).unwrap();

        assert!(!report.exit.succeeded());
        assert!(String::from_utf8_lossy(&report.stderr.bytes).contains('*'));

        let report = run("ls marker.txt", dir.path(), Duration::from_secs(5)).unwrap();
        assert!(report.exit.succeeded());
    }

    #[test]
    fn an_empty_command_is_the_callers_error() {
        let dir = cwd();
        assert!(matches!(
            run("   ", dir.path(), Duration::from_secs(1)),
            Err(VerifyError::Config(_))
        ));
    }
}
