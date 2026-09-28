use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::net::Shutdown;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(unix)]
use std::time::{Duration, Instant};

use super::environment::{HANDOFF_SOCKET_ENV, encode_forwarded_environment, safe_environment_name};
use super::model::RuntimeEvidence;
use super::{
    BackendAvailability, BackendSessionId, BackendSessionState, ExitEvidence, ManagedProcess,
    ManagedProcessId, OutputChunk, OutputStream, ProcessBackend, ProcessError, TerminationSignal,
};

pub(crate) const OWNER_PROCESS_ENV: &str = "SENATE_MANAGED_PROCESS_ID";
pub(crate) const OWNER_FINGERPRINT_ENV: &str = "SENATE_COMMAND_FINGERPRINT";
const MAX_OUTPUT_READ: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct TmuxBackend {
    executable: OsString,
    runner_executable: PathBuf,
    socket_name: Option<OsString>,
}

impl TmuxBackend {
    #[must_use]
    pub fn new(runner_executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: OsString::from("tmux"),
            runner_executable: runner_executable.into(),
            socket_name: None,
        }
    }

    #[must_use]
    pub fn with_executables(
        executable: impl Into<OsString>,
        runner_executable: impl Into<PathBuf>,
    ) -> Self {
        Self {
            executable: executable.into(),
            runner_executable: runner_executable.into(),
            socket_name: None,
        }
    }

    /// Uses isolated tmux server socket, primarily for integration tests.
    #[must_use]
    pub fn with_socket_name(mut self, socket_name: impl Into<OsString>) -> Self {
        self.socket_name = Some(socket_name.into());
        self
    }

    fn command_for_session(&self, session: &BackendSessionId) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .env_clear()
            .envs(std::env::vars_os().filter(|(key, _)| safe_environment_name(key)));
        let socket_name = self
            .socket_name
            .clone()
            .unwrap_or_else(|| OsString::from(session.as_str()));
        command.arg("-L").arg(socket_name);
        command
    }

    fn output(
        &self,
        session: &BackendSessionId,
        operation: &'static str,
        args: &[&OsStr],
    ) -> Result<Output, ProcessError> {
        let mut command = self.command_for_session(session);
        command.args(args);
        Self::execute(operation, &mut command)
    }

    fn execute(operation: &'static str, command: &mut Command) -> Result<Output, ProcessError> {
        command.output().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ProcessError::TmuxNotFound
            } else {
                ProcessError::TmuxCommand {
                    operation,
                    message: error.kind().to_string(),
                }
            }
        })
    }

    fn require_success(
        &self,
        session: &BackendSessionId,
        operation: &'static str,
        args: &[&OsStr],
    ) -> Result<Output, ProcessError> {
        let output = self.output(session, operation, args)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(ProcessError::TmuxCommand {
                operation,
                message: sanitized_stderr(&output.stderr),
            })
        }
    }

    fn exact_target(session: &BackendSessionId) -> OsString {
        OsString::from(format!("={}", session.as_str()))
    }

    fn pane_target(session: &BackendSessionId) -> OsString {
        OsString::from(format!("={}:0.0", session.as_str()))
    }

    fn session_environment(
        &self,
        session: &BackendSessionId,
        name: &str,
    ) -> Result<Option<String>, ProcessError> {
        let target = Self::exact_target(session);
        let output = self.output(
            session,
            "inspect session environment",
            &[
                OsStr::new("show-environment"),
                OsStr::new("-t"),
                &target,
                OsStr::new(name),
            ],
        )?;
        if !output.status.success() {
            return Ok(None);
        }
        let line = String::from_utf8_lossy(&output.stdout);
        Ok(line
            .trim_end()
            .strip_prefix(name)
            .and_then(|value| value.strip_prefix('='))
            .map(ToOwned::to_owned))
    }

    fn session_exists(&self, session: &BackendSessionId) -> Result<bool, ProcessError> {
        let target = Self::exact_target(session);
        Ok(self
            .output(
                session,
                "inspect session",
                &[OsStr::new("has-session"), OsStr::new("-t"), &target],
            )?
            .status
            .success())
    }

    fn output_path(process: &ManagedProcess, stream: OutputStream) -> &Path {
        match stream {
            OutputStream::Stdout => process.spec().stdout_path(),
            OutputStream::Stderr => process.spec().stderr_path(),
        }
    }

    fn exit_path(process: &ManagedProcess) -> Result<PathBuf, ProcessError> {
        process
            .spec()
            .stdout_path()
            .parent()
            .map(|directory| directory.join("exit.json"))
            .ok_or(ProcessError::InvalidSpec("stdout path has no parent"))
    }

    fn runtime_path(process: &ManagedProcess) -> Result<PathBuf, ProcessError> {
        process
            .spec()
            .stdout_path()
            .parent()
            .map(|directory| directory.join("runtime.json"))
            .ok_or(ProcessError::InvalidSpec("stdout path has no parent"))
    }

    fn pane_pid(&self, process: &ManagedProcess) -> Result<u32, ProcessError> {
        let target = Self::pane_target(process.backend_session_id());
        let output = self.require_success(
            process.backend_session_id(),
            "inspect runner PID",
            &[
                OsStr::new("display-message"),
                OsStr::new("-p"),
                OsStr::new("-t"),
                &target,
                OsStr::new("#{pane_pid}"),
            ],
        )?;
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|_| ProcessError::MissingRuntimeEvidence(process.id()))
    }

    fn runtime_evidence(&self, process: &ManagedProcess) -> Result<RuntimeEvidence, ProcessError> {
        let path = Self::runtime_path(process)?;
        let bytes =
            std::fs::read(path).map_err(|_| ProcessError::MissingRuntimeEvidence(process.id()))?;
        let evidence: RuntimeEvidence = serde_json::from_slice(&bytes)
            .map_err(|_| ProcessError::MissingRuntimeEvidence(process.id()))?;
        evidence.validate_for(process)?;
        if evidence.runner_pid() != self.pane_pid(process)? {
            return Err(ProcessError::MissingRuntimeEvidence(process.id()));
        }
        Ok(evidence)
    }
}

impl ProcessBackend for TmuxBackend {
    fn kind(&self) -> &'static str {
        "tmux"
    }

    fn session_id(&self, process_id: ManagedProcessId) -> BackendSessionId {
        BackendSessionId::for_process(process_id)
    }

    fn availability(&self) -> Result<BackendAvailability, ProcessError> {
        let mut command = Command::new(&self.executable);
        command
            .env_clear()
            .envs(std::env::vars_os().filter(|(key, _)| safe_environment_name(key)));
        command.arg("-V");
        let output = Self::execute("version check", &mut command)?;
        if !output.status.success() {
            return Err(ProcessError::TmuxCommand {
                operation: "version check",
                message: sanitized_stderr(&output.stderr),
            });
        }
        Ok(BackendAvailability {
            kind: self.kind(),
            version: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        })
    }

    fn start(&self, process: &ManagedProcess, manifest: &Path) -> Result<(), ProcessError> {
        match self.inspect_session(process)? {
            BackendSessionState::Owned => return Ok(()),
            BackendSessionState::Absent => {}
        }
        let environment_handoff = EnvironmentHandoff::prepare()?;
        let mut command = self.command_for_session(process.backend_session_id());
        command
            .arg("new-session")
            .arg("-d")
            .arg("-s")
            .arg(process.backend_session_id().as_str())
            .arg("-c")
            .arg(process.spec().working_directory());
        for (key, value) in std::env::vars_os().filter(|(key, _)| safe_environment_name(key)) {
            command.arg("-e").arg(environment_assignment(key, &value));
        }
        if let Some(handoff) = &environment_handoff {
            command.arg("-e").arg(handoff.environment_assignment());
        }
        command
            .arg("-e")
            .arg(format!("{OWNER_PROCESS_ENV}={}", process.id()))
            .arg("-e")
            .arg(format!(
                "{OWNER_FINGERPRINT_ENV}={}",
                process.command_fingerprint()
            ))
            .arg("--")
            .arg(&self.runner_executable)
            .arg("__run-process")
            .arg(manifest);
        let output = Self::execute("start session", &mut command)?;
        if output.status.success() {
            if let Some(handoff) = environment_handoff {
                handoff.deliver()?;
            }
            return Ok(());
        }
        if self.inspect_session(process)? == BackendSessionState::Owned {
            return Ok(());
        }
        Err(ProcessError::TmuxCommand {
            operation: "start session",
            message: sanitized_stderr(&output.stderr),
        })
    }

    fn inspect_session(
        &self,
        process: &ManagedProcess,
    ) -> Result<BackendSessionState, ProcessError> {
        if !self.session_exists(process.backend_session_id())? {
            return Ok(BackendSessionState::Absent);
        }
        let process_owner =
            self.session_environment(process.backend_session_id(), OWNER_PROCESS_ENV)?;
        let fingerprint_owner =
            self.session_environment(process.backend_session_id(), OWNER_FINGERPRINT_ENV)?;
        if process_owner.as_deref() == Some(process.id().to_string().as_str())
            && fingerprint_owner.as_deref() == Some(process.command_fingerprint())
        {
            Ok(BackendSessionState::Owned)
        } else if !self.session_exists(process.backend_session_id())? {
            Ok(BackendSessionState::Absent)
        } else {
            Err(ProcessError::ForeignSession {
                session_id: process.backend_session_id().clone(),
            })
        }
    }

    fn read_output(
        &self,
        process: &ManagedProcess,
        stream: OutputStream,
        offset: u64,
        max_bytes: usize,
    ) -> Result<OutputChunk, ProcessError> {
        if max_bytes == 0 || max_bytes > MAX_OUTPUT_READ {
            return Err(ProcessError::InvalidReadSize(MAX_OUTPUT_READ));
        }
        let path = Self::output_path(process, stream);
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        if length < offset {
            return Err(ProcessError::OutputTruncated(process.id()));
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
        file.take(
            u64::try_from(max_bytes).map_err(|_| ProcessError::InvalidReadSize(MAX_OUTPUT_READ))?,
        )
        .read_to_end(&mut bytes)?;
        OutputChunk::new(
            process.id(),
            stream,
            process.cursor(stream).revision(),
            offset,
            bytes,
        )
    }

    fn output_length(
        &self,
        process: &ManagedProcess,
        stream: OutputStream,
    ) -> Result<u64, ProcessError> {
        Ok(std::fs::metadata(Self::output_path(process, stream))?.len())
    }

    fn read_exit_evidence(
        &self,
        process: &ManagedProcess,
    ) -> Result<Option<ExitEvidence>, ProcessError> {
        let path = Self::exit_path(process)?;
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let evidence: ExitEvidence =
            serde_json::from_slice(&bytes).map_err(|_| ProcessError::InvalidExitEvidence {
                process_id: process.id(),
                reason: "invalid JSON",
            })?;
        evidence.validate_for(process)?;
        Ok(Some(evidence))
    }

    fn signal(
        &self,
        process: &ManagedProcess,
        signal: TerminationSignal,
    ) -> Result<(), ProcessError> {
        if self.inspect_session(process)? == BackendSessionState::Absent {
            return Ok(());
        }
        let runtime = self.runtime_evidence(process)?;
        // The negative PID addresses the whole group. The agent is only ever
        // the root of a tree — its own children are what actually hold the
        // machine — so signalling the leader alone would leave the expensive
        // half running.
        let process_group = format!("-{}", runtime.process_group_id());
        let output = Command::new("/bin/kill")
            .arg(signal.flag())
            .arg("--")
            .arg(process_group)
            .output()
            .map_err(|error| ProcessError::SignalCommand(error.kind().to_string()))?;
        if !output.status.success() {
            let message = sanitized_stderr(&output.stderr);
            if group_already_gone(&message) {
                return Ok(());
            }
            return Err(ProcessError::SignalCommand(message));
        }
        Ok(())
    }

    fn cleanup(&self, process: &ManagedProcess) -> Result<(), ProcessError> {
        if self.inspect_session(process)? == BackendSessionState::Absent {
            return Ok(());
        }
        let target = Self::exact_target(process.backend_session_id());
        self.require_success(
            process.backend_session_id(),
            "cleanup session",
            &[OsStr::new("kill-session"), OsStr::new("-t"), &target],
        )?;
        Ok(())
    }
}

#[cfg(unix)]
struct EnvironmentHandoff {
    listener: UnixListener,
    path: PathBuf,
    encoded: Vec<u8>,
}

#[cfg(unix)]
impl EnvironmentHandoff {
    fn prepare() -> Result<Option<Self>, ProcessError> {
        let Some(encoded) = encode_forwarded_environment()? else {
            return Ok(None);
        };
        let path = std::env::temp_dir().join(format!("senate-env-{}.sock", ulid::Ulid::new()));
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Some(Self {
            listener,
            path,
            encoded,
        }))
    }

    fn environment_assignment(&self) -> OsString {
        environment_assignment(OsString::from(HANDOFF_SOCKET_ENV), self.path.as_os_str())
    }

    fn deliver(self) -> Result<(), ProcessError> {
        const TIMEOUT: Duration = Duration::from_secs(5);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_write_timeout(Some(TIMEOUT))?;
                    stream.write_all(&self.encoded)?;
                    stream.shutdown(Shutdown::Write)?;
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(ProcessError::Runner(
                            "environment handoff timed out".to_owned(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

#[cfg(unix)]
impl Drop for EnvironmentHandoff {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(not(unix))]
struct EnvironmentHandoff;

#[cfg(not(unix))]
impl EnvironmentHandoff {
    fn prepare() -> Result<Option<Self>, ProcessError> {
        Err(ProcessError::UnsupportedPlatform)
    }

    fn environment_assignment(&self) -> OsString {
        OsString::new()
    }

    fn deliver(self) -> Result<(), ProcessError> {
        Err(ProcessError::UnsupportedPlatform)
    }
}

/// Whether a failed signal only means the group finished first.
///
/// Escalation makes this the common case rather than an edge one: the rung
/// below usually worked, and the next rung is sent to a group that has already
/// gone. Both BSD and GNU `kill` say so in the same words, and neither offers
/// a distinguishable exit status, so the message is what there is to read.
fn group_already_gone(message: &str) -> bool {
    message.to_ascii_lowercase().contains("no such process")
}

fn sanitized_stderr(stderr: &[u8]) -> String {
    const LIMIT: usize = 512;
    let prefix = &stderr[..stderr.len().min(LIMIT)];
    let message = String::from_utf8_lossy(prefix).trim().to_owned();
    if message.is_empty() {
        "unknown tmux failure".to_owned()
    } else {
        message
    }
}

fn environment_assignment(mut key: OsString, value: &OsStr) -> OsString {
    key.push("=");
    key.push(value);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tmux_binary_is_typed() {
        let backend = TmuxBackend::with_executables(
            "/definitely/missing/senate-tmux",
            "/definitely/missing/senate-runner",
        );
        assert!(matches!(
            backend.availability(),
            Err(ProcessError::TmuxNotFound)
        ));
    }
}
