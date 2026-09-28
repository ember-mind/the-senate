use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use super::environment::{
    HANDOFF_SOCKET_ENV, MAX_HANDOFF_BYTES, decode_forwarded_environment, internal_environment_name,
    safe_environment_name,
};
use super::model::{LaunchManifestV1, RuntimeEvidence};
use super::tmux::{OWNER_FINGERPRINT_ENV, OWNER_PROCESS_ENV};
use super::{ExitEvidence, ExitResult, ProcessError};

/// Executes one immutable runner manifest. Intended only for hidden CLI use.
///
/// # Errors
/// Rejects malformed/foreign manifests and durable evidence write failures.
pub fn run_managed_process(manifest_path: &Path) -> Result<(), ProcessError> {
    if !cfg!(unix) {
        return Err(ProcessError::UnsupportedPlatform);
    }
    let manifest_json = std::fs::read_to_string(manifest_path)?;
    let manifest = LaunchManifestV1::decode(&manifest_json)?;
    let spec = manifest.validated_spec()?;
    validate_tmux_ownership(&manifest)?;

    let started_at = now();
    let stdout = append_file(spec.stdout_path())?;
    let stderr = append_file(spec.stderr_path())?;
    let forwarded_environment = receive_forwarded_environment()?;
    let executable = std::env::current_exe()?;
    let absolute_manifest = std::fs::canonicalize(manifest_path)?;
    let mut command = Command::new(executable);
    command
        .arg("__exec-process")
        .arg(absolute_manifest)
        .current_dir(spec.working_directory())
        .env_clear()
        .envs(safe_child_environment())
        .envs(forwarded_environment)
        .envs(spec.environment())
        .env(OWNER_PROCESS_ENV, manifest.process_id().to_string())
        .env(OWNER_FINGERPRINT_ENV, manifest.command_fingerprint())
        .stdin(verified_stdin(&spec)?)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        prepare_child_interrupt_disposition()?;
        command.process_group(0);
    }

    let result = match command.spawn() {
        Ok(mut child) => {
            let runtime = RuntimeEvidence::new(
                manifest.process_id(),
                manifest.command_fingerprint().to_owned(),
                std::process::id(),
                child.id(),
            );
            match write_atomic_json(manifest_path, "runtime.json", &runtime) {
                Ok(()) => match child.wait() {
                    Ok(status) => exit_result(status),
                    Err(error) => ExitResult::RunnerError {
                        message: format!("child wait failed: {}", error.kind()),
                    },
                },
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    ExitResult::RunnerError {
                        message: format!("runtime evidence failed: {error}"),
                    }
                }
            }
        }
        Err(error) => ExitResult::RunnerError {
            message: format!("child spawn failed: {}", error.kind()),
        },
    };
    let interrupt_observed = matches!(result, ExitResult::Signal { signal: 2 });
    let evidence = ExitEvidence::new(
        manifest.process_id(),
        manifest.command_fingerprint().to_owned(),
        result,
        interrupt_observed,
        started_at,
        now(),
    );
    write_atomic_json(manifest_path, "exit.json", &evidence)
}

/// Replaces signal-normalized launch helper with exact managed command.
///
/// # Errors
/// Rejects malformed/foreign manifests or failed Unix `exec`.
pub fn exec_managed_process(manifest_path: &Path) -> Result<(), ProcessError> {
    if !cfg!(unix) {
        return Err(ProcessError::UnsupportedPlatform);
    }
    let manifest_json = std::fs::read_to_string(manifest_path)?;
    let manifest = LaunchManifestV1::decode(&manifest_json)?;
    let spec = manifest.validated_spec()?;
    validate_tmux_ownership(&manifest)?;
    let environment = std::env::vars_os()
        .filter(|(key, _)| !internal_environment_name(key))
        .collect::<Vec<_>>();
    let mut command = Command::new(spec.executable());
    command
        .args(spec.argv())
        .current_dir(spec.working_directory())
        .env_clear()
        .envs(environment);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        let error = command.exec();
        return Err(ProcessError::Runner(format!(
            "child exec failed: {}",
            error.kind()
        )));
    }
    #[allow(unreachable_code)]
    Err(ProcessError::UnsupportedPlatform)
}

fn safe_child_environment() -> impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)> {
    std::env::vars_os().filter(|(key, _)| safe_environment_name(key))
}

#[cfg(unix)]
fn receive_forwarded_environment()
-> Result<std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>, ProcessError> {
    use std::os::unix::net::UnixStream;

    let Some(socket_path) = std::env::var_os(HANDOFF_SOCKET_ENV) else {
        return Ok(std::collections::BTreeMap::new());
    };
    let stream = UnixStream::connect(socket_path)?;
    let mut encoded = Vec::new();
    stream
        .take(u64::try_from(MAX_HANDOFF_BYTES + 1).expect("handoff limit fits u64"))
        .read_to_end(&mut encoded)?;
    if encoded.len() > MAX_HANDOFF_BYTES {
        return Err(ProcessError::InvalidSpec(
            "forwarded environment exceeds safe handoff limit",
        ));
    }
    decode_forwarded_environment(&encoded)
}

#[cfg(not(unix))]
fn receive_forwarded_environment()
-> Result<std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>, ProcessError> {
    Err(ProcessError::UnsupportedPlatform)
}

fn verified_stdin(spec: &super::ProcessSpec) -> Result<Stdio, ProcessError> {
    let (Some(path), Some(expected)) = (spec.stdin_path(), spec.stdin_sha256()) else {
        return Ok(Stdio::null());
    };
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    let actual = hex(hash.finalize().as_ref());
    if actual != expected {
        return Err(ProcessError::InvalidSpec("stdin content hash mismatch"));
    }
    file.rewind()?;
    Ok(Stdio::from(file))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(unix)]
fn prepare_child_interrupt_disposition() -> Result<(), ProcessError> {
    use std::sync::{Arc, atomic::AtomicBool};

    // tmux may ignore SIGINT. Ignored dispositions survive exec, while caught
    // dispositions reset to default. Registering a safe handler here ensures
    // the managed child receives normal Ctrl-C semantics after exec.
    signal_hook::flag::register(
        signal_hook::consts::SIGINT,
        Arc::new(AtomicBool::new(false)),
    )?;
    Ok(())
}

fn validate_tmux_ownership(manifest: &LaunchManifestV1) -> Result<(), ProcessError> {
    let process_owner = std::env::var(OWNER_PROCESS_ENV).ok();
    let fingerprint_owner = std::env::var(OWNER_FINGERPRINT_ENV).ok();
    if process_owner.as_deref() != Some(manifest.process_id().to_string().as_str())
        || fingerprint_owner.as_deref() != Some(manifest.command_fingerprint())
    {
        return Err(ProcessError::OwnershipMismatch {
            process_id: manifest.process_id(),
        });
    }
    Ok(())
}

fn append_file(path: &Path) -> Result<File, ProcessError> {
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

#[cfg(unix)]
fn exit_result(status: std::process::ExitStatus) -> ExitResult {
    use std::os::unix::process::ExitStatusExt;
    status.code().map_or_else(
        || ExitResult::Signal {
            signal: status.signal().unwrap_or_default(),
        },
        |code| ExitResult::ExitCode { code },
    )
}

#[cfg(not(unix))]
fn exit_result(status: std::process::ExitStatus) -> ExitResult {
    ExitResult::ExitCode {
        code: status.code().unwrap_or_default(),
    }
}

fn write_atomic_json<T: serde::Serialize>(
    manifest_path: &Path,
    name: &str,
    evidence: &T,
) -> Result<(), ProcessError> {
    let directory = manifest_path
        .parent()
        .ok_or(ProcessError::InvalidSpec("manifest path has no parent"))?;
    let evidence_path = directory.join(name);
    let encoded = serde_json::to_vec(evidence)?;
    if evidence_path.exists() {
        if std::fs::read(&evidence_path)? == encoded {
            return Ok(());
        }
        return Err(ProcessError::InvalidSpec(
            "existing runtime evidence differs",
        ));
    }
    let temporary = temporary_evidence_path(directory, name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    match std::fs::hard_link(&temporary, &evidence_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read(&evidence_path)? != encoded {
                let _ = std::fs::remove_file(&temporary);
                return Err(ProcessError::InvalidSpec(
                    "existing runtime evidence differs",
                ));
            }
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(error.into());
        }
    }
    std::fs::remove_file(&temporary)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn temporary_evidence_path(directory: &Path, name: &str) -> PathBuf {
    directory.join(format!(".{name}-{}.tmp", std::process::id()))
}

fn now() -> DateTime<Utc> {
    std::time::SystemTime::now().into()
}
