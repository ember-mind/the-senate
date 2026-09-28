use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use chrono::{DateTime, Utc};
use senate::domain::{
    ConfigSnapshotId, EventId, EventMetadata, Run, RunId, StageId, WorkflowDefinition, WorkflowKind,
};
use senate::process::{
    ExitResult, ManagedProcess, ManagedProcessId, ManagedProcessStatus, OutputStream,
    ProcessBackend, ProcessError, ProcessInspection, ProcessManager, TmuxBackend,
};
use senate::store::{ResolvedConfigSnapshot, RunInput, SqliteStore};
use senate::workspace::WorkspaceManager;
use serde_json::json;
use tempfile::TempDir;

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const POLL_LIMIT: usize = 240;

struct Fixture {
    _temp: TempDir,
    source: PathBuf,
    process_root: PathBuf,
    database: PathBuf,
    run_id: RunId,
    stage_id: StageId,
    socket: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source repo with spaces");
        init_repository(&source);
        let database = temp.path().join("state").join("senate.db");
        let worktree_root = temp.path().join("managed worktrees");
        let process_root = temp.path().join("managed process data").join("runs");
        let run_id = RunId::new();
        let stage_id = StageId::new("implementation").unwrap();
        let created_at = now();
        let config_id = ConfigSnapshotId::new(format!("m6-{run_id}")).unwrap();
        let run = Run::new(
            run_id,
            WorkflowDefinition::built_in(WorkflowKind::Fast),
            config_id.clone(),
            created_at,
        );
        let input = RunInput::new(run_id, "test managed process", created_at).unwrap();
        let config =
            ResolvedConfigSnapshot::new(config_id, 1, json!({"provider": "fixture"}), created_at)
                .unwrap();
        let event = run.created_event(EventMetadata::new(EventId::new(), created_at));
        let mut store = SqliteStore::open(&database).unwrap();
        store
            .create_run_with_input(&run, &input, &config, &[event])
            .unwrap();
        WorkspaceManager::new(&worktree_root)
            .prepare_run_workspace(&mut store, run_id, &source)
            .unwrap();
        drop(store);

        let socket = format!("senate-m6-test-{}", ManagedProcessId::new()).to_ascii_lowercase();
        let fixture = Self {
            _temp: temp,
            source,
            process_root,
            database,
            run_id,
            stage_id,
            socket,
        };
        fixture
            .backend()
            .availability()
            .expect("real tmux is required for M6 integration tests");
        fixture
    }

    fn backend(&self) -> TmuxBackend {
        TmuxBackend::with_executables("tmux", env!("CARGO_BIN_EXE_senate"))
            .with_socket_name(&self.socket)
    }

    fn manager(&self) -> ProcessManager<TmuxBackend> {
        ProcessManager::new(&self.process_root, self.backend())
    }

    fn store(&self) -> SqliteStore {
        SqliteStore::open(&self.database).unwrap()
    }

    fn prepare(&self, arguments: &[&str]) -> ManagedProcess {
        self.prepare_with_env(arguments, BTreeMap::new())
    }

    fn prepare_with_env(
        &self,
        arguments: &[&str],
        environment: BTreeMap<OsString, OsString>,
    ) -> ManagedProcess {
        let mut store = self.store();
        self.manager()
            .prepare(
                &mut store,
                self.run_id,
                self.stage_id.clone(),
                0,
                fixture_agent(),
                arguments.iter().map(OsString::from).collect(),
                environment,
            )
            .unwrap()
    }

    fn start_and_wait(&self, process_id: ManagedProcessId) -> ProcessInspection {
        let mut store = self.store();
        let manager = self.manager();
        let _ = manager.start(&mut store, process_id).unwrap();
        wait_terminal(&manager, &mut store, process_id)
    }

    fn tmux(&self, arguments: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .args(arguments)
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}

#[test]
fn exact_argv_cwd_and_environment_are_shell_safe() {
    let fixture = Fixture::new();
    let stale_server = Command::new("tmux")
        .arg("-L")
        .arg(&fixture.socket)
        .args(["new-session", "-d", "-s", "environment-keeper", "--"])
        .arg("/bin/sleep")
        .arg("10")
        .env("HOME", "/definitely/stale-tmux-home")
        .output()
        .unwrap();
    assert!(stale_server.status.success());
    let marker = fixture.source.join("must-not-exist");
    let hostile = format!("; touch {} ; $(uname)", marker.display());
    let process = fixture.prepare_with_env(
        &["inspect", "plain value", &hostile, "quote'\"value"],
        BTreeMap::from([(
            OsString::from("SENATE_TEST_OVERRIDE"),
            OsString::from("explicit value"),
        )]),
    );

    let completed = fixture.start_and_wait(process.id());
    assert_eq!(completed.process.status(), ManagedProcessStatus::Exited);
    assert_eq!(
        completed.process.exit_result(),
        Some(&ExitResult::ExitCode { code: 0 })
    );
    let output = std::fs::read_to_string(process.spec().stdout_path()).unwrap();
    assert!(output.contains(&format!("inherited={}", std::env::var("HOME").unwrap())));
    assert!(output.contains("override=explicit value"));
    assert!(output.contains("plain value"));
    assert!(output.contains(&hostile));
    assert!(output.contains("quote'\"value"));
    assert!(output.contains("source repo with spaces"));
    assert!(!marker.exists(), "hostile argv must never reach a shell");
    let manifest = std::fs::read_to_string(
        process
            .spec()
            .stdout_path()
            .parent()
            .unwrap()
            .join("spec.json"),
    )
    .unwrap();
    assert!(!manifest.contains(r#""value":"HOME""#));
}

#[test]
fn corrupt_preexisting_exit_evidence_blocks_launch_and_marks_broken() {
    let fixture = Fixture::new();
    let process = fixture.prepare(&["success"]);
    let exit = process
        .spec()
        .stdout_path()
        .parent()
        .unwrap()
        .join("exit.json");
    std::fs::write(exit, b"{}\n").unwrap();
    let mut store = fixture.store();

    assert!(matches!(
        fixture.manager().start(&mut store, process.id()),
        Err(ProcessError::InvalidExitEvidence { .. })
    ));
    assert_eq!(
        store.load_managed_process(process.id()).unwrap().status(),
        ManagedProcessStatus::Broken
    );
    assert!(
        std::fs::read(process.spec().stdout_path())
            .unwrap()
            .is_empty()
    );
    assert!(
        !fixture
            .tmux(&[
                "has-session",
                "-t",
                &format!("={}", process.backend_session_id())
            ])
            .status
            .success()
    );
}

#[test]
fn stdout_stderr_nonzero_exit_and_unread_final_output_survive_exit() {
    let fixture = Fixture::new();
    let process = fixture.prepare(&["fail-42"]);
    let completed = fixture.start_and_wait(process.id());
    assert_eq!(completed.process.status(), ManagedProcessStatus::Exited);
    assert_eq!(
        completed.process.exit_result(),
        Some(&ExitResult::ExitCode { code: 42 })
    );

    let store = fixture.store();
    let manager = fixture.manager();
    let stdout = manager
        .read_output(&store, process.id(), OutputStream::Stdout, 4096)
        .unwrap();
    let stderr = manager
        .read_output(&store, process.id(), OutputStream::Stderr, 4096)
        .unwrap();
    assert!(stdout.bytes().is_empty());
    assert_eq!(stderr.bytes(), b"expected-failure\n");
}

#[test]
fn read_ack_restart_has_no_loss_and_no_ack_has_replay() {
    let fixture = Fixture::new();
    let process = fixture.prepare(&["success"]);
    fixture.start_and_wait(process.id());

    let manager = fixture.manager();
    let mut store = fixture.store();
    let first = manager
        .read_output(&store, process.id(), OutputStream::Stdout, 5)
        .unwrap();
    let replay = manager
        .read_output(&store, process.id(), OutputStream::Stdout, 5)
        .unwrap();
    assert_eq!(first, replay);
    manager
        .acknowledge_output(&mut store, &first, first.end_offset())
        .unwrap();
    assert!(matches!(
        manager.acknowledge_output(&mut store, &replay, replay.end_offset()),
        Err(ProcessError::CursorConcurrentModification { .. })
    ));
    drop(store);

    let mut reopened = fixture.store();
    let second = fixture
        .manager()
        .read_output(&reopened, process.id(), OutputStream::Stdout, 4096)
        .unwrap();
    let mut joined = first.bytes().to_vec();
    joined.extend_from_slice(second.bytes());
    assert_eq!(joined, b"quick-success\n");
    fixture
        .manager()
        .acknowledge_output(&mut reopened, &second, second.end_offset())
        .unwrap();
    let empty = fixture
        .manager()
        .read_output(&reopened, process.id(), OutputStream::Stdout, 4096)
        .unwrap();
    assert!(empty.bytes().is_empty());
}

#[test]
fn new_attempt_gets_new_identity_directory_and_retains_first_logs() {
    let fixture = Fixture::new();
    let first = fixture.prepare(&["success"]);
    fixture.start_and_wait(first.id());
    let first_output = first.spec().stdout_path().to_path_buf();

    let mut store = fixture.store();
    let second = fixture
        .manager()
        .prepare(
            &mut store,
            fixture.run_id,
            fixture.stage_id.clone(),
            1,
            fixture_agent(),
            vec![OsString::from("stderr")],
            BTreeMap::new(),
        )
        .unwrap();
    assert_ne!(first.id(), second.id());
    assert_ne!(first.spec().stdout_path(), second.spec().stdout_path());
    fixture.start_and_wait(second.id());
    assert_eq!(std::fs::read(first_output).unwrap(), b"quick-success\n");
    assert_eq!(
        std::fs::read(second.spec().stderr_path()).unwrap(),
        b"separate-stderr\n"
    );
}

#[test]
fn partial_records_remain_raw_and_large_output_is_incremental() {
    let partial_fixture = Fixture::new();
    let partial = partial_fixture.prepare(&["partial"]);
    let mut partial_store = partial_fixture.store();
    let partial_manager = partial_fixture.manager();
    partial_manager
        .start(&mut partial_store, partial.id())
        .unwrap();
    let first = wait_for_output(
        &partial_manager,
        &partial_store,
        partial.id(),
        OutputStream::Stdout,
    );
    assert!(first.bytes().starts_with(b"{\"message\":"));
    assert!(!first.bytes().ends_with(b"}\n"));
    wait_terminal(&partial_manager, &mut partial_store, partial.id());
    let complete = partial_manager
        .read_output(&partial_store, partial.id(), OutputStream::Stdout, 4096)
        .unwrap();
    assert_eq!(complete.bytes(), b"{\"message\":\"partial\"}\n");

    let large_fixture = Fixture::new();
    let length = 3_u64 * 1024 * 1024;
    let large = large_fixture.prepare(&["large", &length.to_string()]);
    let mut large_store = large_fixture.store();
    let large_manager = large_fixture.manager();
    large_manager.start(&mut large_store, large.id()).unwrap();
    wait_terminal(&large_manager, &mut large_store, large.id());
    let mut consumed = 0_u64;
    loop {
        let chunk = large_manager
            .read_output(&large_store, large.id(), OutputStream::Stdout, 64 * 1024)
            .unwrap();
        if chunk.bytes().is_empty() {
            break;
        }
        assert!(chunk.bytes().iter().all(|byte| *byte == b'x'));
        consumed += u64::try_from(chunk.bytes().len()).unwrap();
        large_manager
            .acknowledge_output(&mut large_store, &chunk, chunk.end_offset())
            .unwrap();
    }
    assert_eq!(consumed, length);
}

#[test]
fn detached_tmux_process_survives_original_manager_and_store() {
    let fixture = Fixture::new();
    let process = fixture.prepare(&["slow", "750"]);
    {
        let mut original_store = fixture.store();
        let original_manager = fixture.manager();
        let started = original_manager
            .start(&mut original_store, process.id())
            .unwrap();
        assert!(matches!(
            started.process.status(),
            ManagedProcessStatus::Running | ManagedProcessStatus::Exited
        ));
    }

    let mut restarted_store = fixture.store();
    let restarted_manager = fixture.manager();
    let observed = restarted_manager
        .reconcile(&mut restarted_store, process.id())
        .unwrap();
    assert!(matches!(
        observed.process.status(),
        ManagedProcessStatus::Running | ManagedProcessStatus::Exited
    ));
    let completed = wait_terminal(&restarted_manager, &mut restarted_store, process.id());
    assert_eq!(completed.process.status(), ManagedProcessStatus::Exited);
    assert_eq!(
        std::fs::read(process.spec().stdout_path()).unwrap(),
        b"slow-success\n"
    );
}

#[test]
fn reconciliation_covers_preparing_owned_and_starting_absent_crash_windows() {
    let owned_fixture = Fixture::new();
    let owned = owned_fixture.prepare(&["slow", "500"]);
    let manifest = owned
        .spec()
        .stdout_path()
        .parent()
        .unwrap()
        .join("spec.json");
    owned_fixture.backend().start(&owned, &manifest).unwrap();
    let mut owned_store = owned_fixture.store();
    let reconciled = owned_fixture
        .manager()
        .reconcile(&mut owned_store, owned.id())
        .unwrap();
    assert_eq!(reconciled.process.status(), ManagedProcessStatus::Running);
    wait_terminal(&owned_fixture.manager(), &mut owned_store, owned.id());

    let absent_fixture = Fixture::new();
    let absent = absent_fixture.prepare(&["success"]);
    let connection = rusqlite::Connection::open(&absent_fixture.database).unwrap();
    connection
        .execute(
            "UPDATE managed_processes
             SET status = 'starting', revision = revision + 1
             WHERE id = ?1",
            [absent.id().to_string()],
        )
        .unwrap();
    drop(connection);
    let missing = absent_fixture
        .manager()
        .reconcile(&mut absent_fixture.store(), absent.id())
        .unwrap();
    assert_eq!(missing.process.status(), ManagedProcessStatus::Missing);
    assert!(missing.exit_evidence.is_none());
}

#[test]
fn concurrent_start_runs_one_external_attempt() {
    let fixture = Fixture::new();
    let process = fixture.prepare(&["slow", "300"]);
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let barrier = Arc::clone(&barrier);
        let database = fixture.database.clone();
        let root = fixture.process_root.clone();
        let backend = fixture.backend();
        let process_id = process.id();
        threads.push(std::thread::spawn(move || {
            let mut store = SqliteStore::open(database).unwrap();
            let manager = ProcessManager::new(root, backend);
            barrier.wait();
            manager.start(&mut store, process_id)
        }));
    }
    barrier.wait();
    for thread in threads {
        let result = thread.join().unwrap();
        assert!(
            result.is_ok() || matches!(result, Err(ProcessError::ConcurrentModification { .. }))
        );
    }
    let mut store = fixture.store();
    wait_terminal(&fixture.manager(), &mut store, process.id());
    assert_eq!(
        std::fs::read(process.spec().stdout_path()).unwrap(),
        b"slow-success\n"
    );
}

#[test]
fn interrupt_records_evidence_leaves_no_session_and_cleanup_is_idempotent() {
    let fixture = Fixture::new();
    let process = fixture.prepare(&["wait-interrupt"]);
    let manager = fixture.manager();
    let mut store = fixture.store();
    manager.start(&mut store, process.id()).unwrap();
    // Output proves the child is alive. Interrupt needs something else: the
    // runtime evidence the runner writes *after* spawning it, which carries
    // the PIDs the signal is aimed at. Waiting on the first and then acting on
    // the second is a race, and under CI load it is a lost one.
    wait_for_output(&manager, &store, process.id(), OutputStream::Stdout);
    wait_for_runtime_evidence(&process);

    let interrupted = manager.interrupt(&mut store, process.id()).unwrap();
    assert_eq!(
        interrupted.process.status(),
        ManagedProcessStatus::Interrupted
    );
    assert_eq!(
        interrupted.backend_session,
        senate::process::BackendSessionState::Absent
    );
    assert!(matches!(
        interrupted.process.exit_result(),
        Some(ExitResult::Signal { signal: 2 })
    ));
    let cleaned = manager.cleanup(&mut store, process.id()).unwrap();
    assert_eq!(cleaned.process.status(), ManagedProcessStatus::Cleaned);
    let cleaned_again = manager.cleanup(&mut store, process.id()).unwrap();
    assert_eq!(
        cleaned_again.process.status(),
        ManagedProcessStatus::Cleaned
    );
    assert!(process.spec().stdout_path().exists());
    assert!(process.spec().stderr_path().exists());
}

/// The end-to-end version of the escalation the unit tests cover with a
/// fixture: a real process, in a real tmux session, that catches every signal
/// it is allowed to catch.
///
/// This is the shape that leaked. An agent forwards Ctrl-C to the test runner
/// it launched, the runner's worker pool ignores it, and the old stop reported
/// `InterruptTimeout` and walked away — leaving the pool holding memory for as
/// long as the machine stayed up. Exit signal 9 is the proof that the ladder
/// was climbed to the end rather than abandoned at the first rung.
///
/// Slow by nature: the real grace periods are what make the result meaningful,
/// so this waits out SIGINT and SIGTERM before SIGKILL lands.
#[test]
fn a_process_that_catches_every_catchable_signal_is_still_stopped() {
    let fixture = Fixture::new();
    let process = fixture.prepare(&["ignore-signals"]);
    let manager = fixture.manager();
    let mut store = fixture.store();
    manager.start(&mut store, process.id()).unwrap();
    wait_for_output(&manager, &store, process.id(), OutputStream::Stdout);
    wait_for_runtime_evidence(&process);

    let stopped = manager.interrupt(&mut store, process.id()).unwrap();

    assert_eq!(
        stopped.backend_session,
        senate::process::BackendSessionState::Absent,
        "the session is gone, so nothing of the process group is left"
    );
    assert!(
        matches!(
            stopped.process.exit_result(),
            Some(ExitResult::Signal { signal: 9 })
        ),
        "only SIGKILL can end this child, so that is what must have ended it: {:?}",
        stopped.process.exit_result()
    );
}

/// Reproduces the startup window deterministically instead of racing it.
///
/// The runner spawns its child before writing `runtime.json`, so between those
/// two points a managed process is running and cannot be signalled. That window
/// is timing-dependent and does not reproduce on demand, but its observable
/// state does: a live runner whose runtime evidence is not on disk. Removing
/// the file after the fact puts the backend in exactly that state.
///
/// The primitive must keep refusing. Signalling without knowing which PIDs to
/// aim at would be worse than failing, and swallowing the wait inside
/// `interrupt` would hide a genuine failure to start behind a delay. The
/// tolerance belongs in the stop path, which knows a user is waiting — see
/// `retry_stop` in the app layer.
#[test]
fn interrupt_refuses_while_runtime_evidence_is_absent() {
    let fixture = Fixture::new();
    let process = fixture.prepare(&["wait-interrupt"]);
    let manager = fixture.manager();
    let mut store = fixture.store();
    manager.start(&mut store, process.id()).unwrap();
    wait_for_output(&manager, &store, process.id(), OutputStream::Stdout);
    wait_for_runtime_evidence(&process);

    let evidence = process
        .spec()
        .stdout_path()
        .parent()
        .expect("stdout path has a parent")
        .join("runtime.json");
    let recorded = std::fs::read(&evidence).unwrap();
    std::fs::remove_file(&evidence).unwrap();

    let error = manager
        .interrupt(&mut store, process.id())
        .expect_err("a process that cannot be named must not be reported as signalled");
    assert!(
        matches!(error, ProcessError::MissingRuntimeEvidence(id) if id == process.id()),
        "unexpected error: {error}"
    );

    // The refusal is not a stop: the child is still running, which is exactly
    // why the failure must never be reported as success.
    let inspection = manager.inspect(&mut store, process.id()).unwrap();
    assert!(inspection.process.status().is_active());

    // Restoring the evidence is enough for the same call to succeed, which is
    // what makes waiting the right answer in the layer above — and it leaves
    // no detached tmux session behind.
    std::fs::write(&evidence, recorded).unwrap();
    let interrupted = manager.interrupt(&mut store, process.id()).unwrap();
    assert_eq!(
        interrupted.process.status(),
        ManagedProcessStatus::Interrupted
    );
}

#[test]
fn foreign_session_collision_is_not_reused_or_killed() {
    let fixture = Fixture::new();
    let process = fixture.prepare(&["success"]);
    let output = fixture.tmux(&[
        "new-session",
        "-d",
        "-s",
        process.backend_session_id().as_str(),
        "--",
        "/bin/sleep",
        "10",
    ]);
    assert!(output.status.success());

    let mut store = fixture.store();
    assert!(matches!(
        fixture.manager().start(&mut store, process.id()),
        Err(ProcessError::ForeignSession { .. })
    ));
    assert_eq!(
        store.load_managed_process(process.id()).unwrap().status(),
        ManagedProcessStatus::Broken
    );
    assert!(
        fixture
            .tmux(&[
                "has-session",
                "-t",
                &format!("={}", process.backend_session_id())
            ])
            .status
            .success()
    );
}

#[test]
fn missing_owned_session_without_exit_evidence_is_never_success() {
    let fixture = Fixture::new();
    let process = fixture.prepare(&["slow", "5000"]);
    let manager = fixture.manager();
    let mut store = fixture.store();
    let started = manager.start(&mut store, process.id()).unwrap();
    assert_eq!(started.process.status(), ManagedProcessStatus::Running);
    assert!(
        fixture
            .tmux(&[
                "kill-session",
                "-t",
                &format!("={}", process.backend_session_id())
            ])
            .status
            .success()
    );

    let missing = manager.reconcile(&mut store, process.id()).unwrap();
    assert_eq!(missing.process.status(), ManagedProcessStatus::Missing);
    assert!(missing.exit_evidence.is_none());
}

fn wait_terminal<B: ProcessBackend>(
    manager: &ProcessManager<B>,
    store: &mut SqliteStore,
    process_id: ManagedProcessId,
) -> ProcessInspection {
    for _ in 0..POLL_LIMIT {
        let inspection = manager.reconcile(store, process_id).unwrap();
        if matches!(
            inspection.process.status(),
            ManagedProcessStatus::Exited
                | ManagedProcessStatus::Interrupted
                | ManagedProcessStatus::Broken
        ) {
            return inspection;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    panic!("process did not reach terminal infrastructure state");
}

/// Waits for the runner to record the runtime evidence that signalling needs.
///
/// The runner spawns the child first and writes `runtime.json` afterwards, so
/// a process can be producing output before the file that names its PIDs
/// exists. Anything that signals a managed process has to wait for this, not
/// for the child's own liveness.
fn wait_for_runtime_evidence(process: &ManagedProcess) {
    let path = process
        .spec()
        .stdout_path()
        .parent()
        .expect("stdout path has a parent")
        .join("runtime.json");
    for _ in 0..POLL_LIMIT {
        if path.is_file() {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    panic!(
        "runner never recorded runtime evidence at {}",
        path.display()
    );
}

fn wait_for_output<B: ProcessBackend>(
    manager: &ProcessManager<B>,
    store: &SqliteStore,
    process_id: ManagedProcessId,
    stream: OutputStream,
) -> senate::process::OutputChunk {
    for _ in 0..POLL_LIMIT {
        let chunk = manager
            .read_output(store, process_id, stream, 4096)
            .unwrap();
        if !chunk.bytes().is_empty() {
            return chunk;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    panic!("process produced no output");
}

fn init_repository(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    run_git(path, &["init", "-b", "main"]);
    run_git(path, &["config", "user.email", "senate@example.invalid"]);
    run_git(path, &["config", "user.name", "The Senate Test"]);
    std::fs::write(path.join("README.md"), "base\n").unwrap();
    run_git(path, &["add", "README.md"]);
    run_git(path, &["commit", "-m", "base"]);
}

fn run_git(path: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture_agent() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_senate-test-agent"))
}

fn now() -> DateTime<Utc> {
    std::time::SystemTime::now().into()
}
