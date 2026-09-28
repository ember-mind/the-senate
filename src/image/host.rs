//! The Senate-side end of the tool: a unix socket the run-scoped MCP shim
//! connects to, one JSON line per call. It lives in the Senate process,
//! which is the only process that ever holds the credential.
//!
//! The socket path is a pure function of the run id, so a Senate process
//! that restarts and resumes the run rebinds the same path and the coding
//! agent's already-running MCP shim keeps working. While no The Senate process
//! is listening (The Senate exited, the agent kept going in tmux), a call fails
//! as `broker unavailable` and the agent continues without an image; nothing
//! is queued or retried on its behalf.

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::RunId;

use super::service::{ImageToolError, ImageToolErrorCode, ImageToolScope, ImageToolService};
use super::{ImageToolCall, ImageToolSuccess};

/// Name of the MCP server as both CLIs will show it; the Claude permission
/// rule is `mcp__senate_image__image_generate`.
pub const MCP_SERVER_NAME: &str = "senate_image";
/// Hidden subcommand that runs the stdio shim.
pub const SHIM_SUBCOMMAND: &str = "__image-tool";
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// What a native CLI must launch to reach this host: the Senate executable
/// itself, as the shim. Carries no secret; safe on argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageToolServerCommand {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
}

/// One request line from the shim.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireRequest {
    pub call: ImageToolCall,
}

/// One response line to the shim.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "ok")]
pub(crate) enum WireResponse {
    #[serde(rename = "true")]
    Success { result: ImageToolSuccess },
    #[serde(rename = "false")]
    Failure { error: ImageToolError },
}

pub struct ImageToolHost {
    service: Arc<ImageToolService>,
    socket_path: PathBuf,
    active: Mutex<Option<ImageToolScope>>,
    stop: AtomicBool,
    acceptor: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for ImageToolHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageToolHost")
            .field("socket_path", &self.socket_path)
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl ImageToolHost {
    /// Binds the run's socket (replacing a stale file from a previous
    /// The Senate process) and starts serving. Nothing is authorized until
    /// [`Self::activate`] names a stage.
    ///
    /// # Errors
    /// Returns the bind failure.
    ///
    /// # Panics
    /// If the acceptor slot lock is poisoned, which cannot happen before the
    /// thread is spawned.
    pub fn start(service: ImageToolService, run_id: RunId) -> std::io::Result<Arc<Self>> {
        let socket_path = socket_path_for(run_id);
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let host = Arc::new(Self {
            service: Arc::new(service),
            socket_path,
            active: Mutex::new(None),
            stop: AtomicBool::new(false),
            acceptor: Mutex::new(None),
        });
        // The thread holds a weak handle: a strong one would keep the host
        // alive for as long as the thread runs, and the thread runs until
        // the host drops.
        let worker = Arc::downgrade(&host);
        let handle = std::thread::Builder::new()
            .name("senate-image-tool".to_owned())
            .spawn(move || Self::accept_loop(&worker, &listener))?;
        *host.acceptor.lock().expect("acceptor lock") = Some(handle);
        Ok(host)
    }

    /// The stage now allowed to call the tool. Idempotent; adapters call it
    /// on every poll of a granted stage so a resumed process re-arms it.
    ///
    /// # Panics
    /// If the scope lock is poisoned.
    pub fn activate(&self, scope: ImageToolScope) {
        let mut active = self.active.lock().expect("active scope lock");
        if active.as_ref() != Some(&scope) {
            *active = Some(scope);
        }
    }

    /// No stage may call the tool until the next `activate`.
    ///
    /// # Panics
    /// If the scope lock is poisoned.
    pub fn deactivate(&self) {
        *self.active.lock().expect("active scope lock") = None;
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[must_use]
    pub fn service(&self) -> &ImageToolService {
        &self.service
    }

    /// The exact command a native CLI launches as the MCP server.
    ///
    /// # Errors
    /// Returns the failure to locate this executable.
    pub fn server_command(&self) -> std::io::Result<ImageToolServerCommand> {
        Ok(ImageToolServerCommand {
            executable: std::env::current_exe()?,
            args: vec![
                OsString::from(SHIM_SUBCOMMAND),
                OsString::from("--socket"),
                self.socket_path.as_os_str().to_owned(),
            ],
        })
    }

    /// Serves one call against the active scope. Public so a test can drive
    /// the host without a socket.
    ///
    /// # Errors
    /// The typed tool error, `NotAuthorized` when no stage is armed.
    ///
    /// # Panics
    /// If the scope lock is poisoned.
    pub fn handle(&self, call: &ImageToolCall) -> Result<ImageToolSuccess, ImageToolError> {
        let scope = self
            .active
            .lock()
            .expect("active scope lock")
            .clone()
            .ok_or_else(|| {
                ImageToolError::new(
                    ImageToolErrorCode::NotAuthorized,
                    "no stage is currently authorized to generate images",
                )
            })?;
        self.service.generate(&scope, call)
    }

    fn accept_loop(host: &Weak<Self>, listener: &UnixListener) {
        loop {
            // Never hold a strong handle across the idle sleep: the host's
            // Drop is what ends this loop, and it cannot run while the loop
            // keeps the host alive.
            match listener.accept() {
                Ok((stream, _)) => {
                    let Some(host) = host.upgrade() else {
                        return;
                    };
                    if host.stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
                    host.serve(&stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if host.strong_count() == 0 {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => {
                    if host.strong_count() == 0 {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    fn serve(&self, stream: &UnixStream) {
        let mut reader = BufReader::new(stream.take(MAX_REQUEST_BYTES as u64));
        let mut line = String::new();
        let response = match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => match serde_json::from_str::<WireRequest>(line.trim_end()) {
                Ok(request) => match self.handle(&request.call) {
                    Ok(result) => WireResponse::Success { result },
                    Err(error) => WireResponse::Failure { error },
                },
                Err(error) => WireResponse::Failure {
                    error: ImageToolError::new(
                        ImageToolErrorCode::InvalidArgument,
                        format!("malformed tool arguments: {error}"),
                    ),
                },
            },
            Err(error) => WireResponse::Failure {
                error: ImageToolError::new(ImageToolErrorCode::Internal, error.to_string()),
            },
        };
        let mut writer = stream;
        if let Ok(mut encoded) = serde_json::to_vec(&response) {
            encoded.push(b'\n');
            let _ = writer.write_all(&encoded);
            let _ = writer.flush();
        }
    }
}

impl Drop for ImageToolHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.acceptor.lock().ok().and_then(|mut slot| slot.take()) {
            // Whose thread this runs on is not a given. The acceptor holds a
            // strong handle for as long as it is serving a call, so if the
            // owner lets go first, the acceptor's own drop is the last one and
            // this runs on the acceptor thread. Joining there is a thread
            // joining itself: `EDEADLK`, a panic inside a `Drop`, and — because
            // the panic happens before the line below — a socket file left in
            // the temp directory for every run that ended that way.
            //
            // The join is only ever a courtesy. The loop stops on its own once
            // the weak handle stops upgrading, which the drop finishing is
            // exactly what causes.
            if handle.thread().id() != std::thread::current().id() {
                let _ = handle.join();
            }
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Deterministic per-run socket path. Short on purpose: `sun_path` is 104
/// bytes on macOS and `TMPDIR` already spends half of them.
#[must_use]
pub fn socket_path_for(run_id: RunId) -> PathBuf {
    let path = std::env::temp_dir().join(format!("pcimg-{run_id}.sock"));
    if path.as_os_str().len() < 100 {
        path
    } else {
        PathBuf::from(format!("/tmp/pcimg-{run_id}.sock"))
    }
}

/// Client side used by the shim: one connection, one line each way.
///
/// # Errors
/// Returns a `BackendUnreachable`-class tool error when no host is
/// listening, so the agent sees a normal typed failure.
pub(crate) fn call_host(
    socket: &Path,
    call: &ImageToolCall,
) -> Result<ImageToolSuccess, ImageToolError> {
    let mut stream = UnixStream::connect(socket).map_err(|error| {
        ImageToolError::new(
            ImageToolErrorCode::BackendUnreachable,
            format!(
                "The Senate image broker is not running ({error}); continue without a new image"
            ),
        )
    })?;
    // Generation can take minutes; the response wait is bounded by the
    // backend's own timeout inside the host.
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut encoded = serde_json::to_vec(&WireRequest { call: call.clone() })
        .map_err(|error| ImageToolError::new(ImageToolErrorCode::Internal, error.to_string()))?;
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .and_then(|()| stream.flush())
        .map_err(|error| {
            ImageToolError::new(ImageToolErrorCode::BackendUnreachable, error.to_string())
        })?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|error| {
            ImageToolError::new(ImageToolErrorCode::BackendUnreachable, error.to_string())
        })?;
    match serde_json::from_str::<WireResponse>(line.trim_end()) {
        Ok(WireResponse::Success { result }) => Ok(result),
        Ok(WireResponse::Failure { error }) => Err(error),
        Err(error) => Err(ImageToolError::new(
            ImageToolErrorCode::Internal,
            format!("malformed broker response: {error}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_response_tags_success_and_failure_distinctly() {
        let failure = WireResponse::Failure {
            error: ImageToolError::new(ImageToolErrorCode::LimitReached, "no more"),
        };
        let json = serde_json::to_string(&failure).unwrap();
        assert!(json.contains("\"ok\":\"false\""));
        assert!(json.contains("limit_reached"));
        let decoded: WireResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, WireResponse::Failure { .. }));
    }

    #[test]
    fn socket_path_is_deterministic_per_run_and_short() {
        let run = RunId::from_u128(7);
        assert_eq!(socket_path_for(run), socket_path_for(run));
        assert!(socket_path_for(run).as_os_str().len() < 104);
    }

    #[test]
    fn calling_an_absent_host_is_a_typed_unreachable_error() {
        let error = call_host(
            Path::new("/nonexistent/pcimg-test.sock"),
            &ImageToolCall {
                prompt: "p".to_owned(),
                output_path: "a.png".to_owned(),
                size: None,
                quality: None,
                transparent_background: None,
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ImageToolErrorCode::BackendUnreachable);
    }

    #[test]
    fn the_shim_reaches_the_host_over_the_socket_and_the_png_lands_in_the_worktree() {
        use crate::domain::{
            ConfigSnapshotId, EventId, EventMetadata, Role, Run, StageDefinition, StageId,
            StageKind, WorkflowDefinition, WorkflowKind,
        };
        use crate::image::{FakeImageGenerator, mcp};
        use crate::store::{ResolvedConfigSnapshot, SqliteStore};
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let database = temp.path().join("senate.db");
        let worktree = temp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let run_id = RunId::new();
        {
            let mut store = SqliteStore::open(&database).unwrap();
            let at: chrono::DateTime<chrono::Utc> = std::time::SystemTime::now().into();
            let config_id = ConfigSnapshotId::new("c").unwrap();
            let workflow = WorkflowDefinition::new(
                WorkflowKind::Fast,
                vec![StageDefinition::new(
                    StageId::new("implementation").unwrap(),
                    StageKind::Implementation,
                    Role::Implementer,
                    vec![],
                )],
            )
            .unwrap();
            let run = Run::new(run_id, workflow, config_id.clone(), at);
            let config =
                ResolvedConfigSnapshot::new(config_id, 1, serde_json::json!({}), at).unwrap();
            let event = run.created_event(EventMetadata::new(EventId::new(), at));
            store.create_run(&run, &config, &[event]).unwrap();
        }
        let service = ImageToolService::new(
            temp.path().join("runs"),
            Some(Arc::new(FakeImageGenerator::new())),
            vec![Role::Implementer],
            2,
        );
        let host = ImageToolHost::start(service, run_id).unwrap();
        let socket = host.socket_path().to_path_buf();
        assert!(socket.exists());
        let command = host.server_command().unwrap();
        assert_eq!(command.args[0], SHIM_SUBCOMMAND);
        assert_eq!(command.args[2], socket.as_os_str());

        let request = |path: &str| {
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"image_generate","arguments":{{"prompt":"a hero","output_path":"{path}"}}}}}}"#
            )
        };
        // Nothing is armed yet: a typed refusal, not a hang and not a file.
        let refused = mcp::handle_line(&request("assets/hero.png"), &socket).unwrap();
        assert_eq!(refused["result"]["isError"], true);
        assert!(
            refused["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("not_authorized")
        );
        host.activate(ImageToolScope {
            run_id,
            stage_id: StageId::new("implementation").unwrap(),
            attempt: 1,
            role: Role::Implementer,
            worktree: worktree.clone(),
            database: database.clone(),
        });
        let ok = mcp::handle_line(&request("assets/hero.png"), &socket).unwrap();
        assert_eq!(ok["result"]["isError"], false, "{ok}");
        let text = ok["result"]["content"][0]["text"].as_str().unwrap();
        let success: ImageToolSuccess = serde_json::from_str(text).unwrap();
        assert_eq!(success.output_path, "assets/hero.png");
        assert_eq!(
            std::fs::read(worktree.join("assets/hero.png")).unwrap(),
            FakeImageGenerator::png_for("a hero")
        );
        // A reviewer scope over the same host is refused by the service.
        host.activate(ImageToolScope {
            run_id,
            stage_id: StageId::new("review").unwrap(),
            attempt: 1,
            role: Role::CodeQualityReviewer,
            worktree: worktree.clone(),
            database: database.clone(),
        });
        let reviewer = mcp::handle_line(&request("assets/other.png"), &socket).unwrap();
        assert_eq!(reviewer["result"]["isError"], true);
        assert!(!worktree.join("assets/other.png").exists());
        drop(host);
        // Releasing the last handle a *test* holds is not necessarily the last
        // handle: the acceptor thread upgrades its weak reference for as long
        // as it is serving a call, and the request just above is one. So `Drop`
        // — and the removal it does — can land after this line rather than
        // during it. Waiting for the socket to go is the contract; waiting no
        // time at all was a race the test lost under CI load.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while socket.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!socket.exists(), "dropping the host removes its socket");
    }
}
