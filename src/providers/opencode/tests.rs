use std::collections::HashSet;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::domain::{
    ConfigSnapshotId, DomainEventKind, EventId, EventMetadata, Run, RunStatus, StageDefinition,
    StageId, WorkflowDefinition, WorkflowKind,
};
use crate::engine::{EngineStatus, WorkflowEngine};
use crate::process::{
    BackendAvailability, BackendSessionId, BackendSessionState, ExitEvidence, ExitResult,
    ManagedProcess, ProcessError, TerminationSignal,
};
use crate::store::{ResolvedConfigSnapshot, RunInput};
use crate::workspace::WorkspaceManager;

fn implementation_only() -> WorkflowDefinition {
    WorkflowDefinition::new(
        WorkflowKind::Fast,
        vec![StageDefinition::new(
            StageId::new("implementation").unwrap(),
            StageKind::Implementation,
            Role::Implementer,
            vec![],
        )],
    )
    .unwrap()
}

const SUCCESS_OUTPUT: &str = concat!(
    "{\"type\":\"step_start\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\"}}\n",
    "{\"type\":\"tool_use\",\"sessionID\":\"ses_A\",\"part\":{\"tool\":\"read\",\"state\":{\"status\":\"completed\"}}}\n",
    "{\"type\":\"text\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\",\"text\":\"# opencode result\\nFixture\"}}\n",
    "{\"type\":\"step_finish\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\",\"reason\":\"stop\",\"tokens\":{\"total\":167,\"input\":100,\"output\":47,\"reasoning\":0,\"cache\":{\"write\":0,\"read\":20}}}}\n"
);

const PERMISSION_HALT_OUTPUT: &str = concat!(
    "{\"type\":\"step_start\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\"}}\n",
    "{\"type\":\"tool_use\",\"sessionID\":\"ses_A\",\"part\":{\"tool\":\"bash\",\"callID\":\"call_1\",\"state\":{\"status\":\"error\",\"input\":{\"command\":\"python3 -c \\\"from calc import add; assert add(2,3)==5\\\"\"},\"error\":\"The user rejected permission to use this specific tool call.\"}}}\n",
    "{\"type\":\"step_finish\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\",\"reason\":\"tool-calls\",\"tokens\":{\"total\":50,\"input\":40,\"output\":10}}}\n"
);

const RESUME_AFTER_APPROVAL_OUTPUT: &str = concat!(
    "{\"type\":\"step_start\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_2\"}}\n",
    "{\"type\":\"tool_use\",\"sessionID\":\"ses_A\",\"part\":{\"tool\":\"bash\",\"callID\":\"call_2\",\"state\":{\"status\":\"completed\",\"input\":{\"command\":\"python3 -c \\\"from calc import add; assert add(2,3)==5\\\"\"},\"output\":\"\"}}}\n",
    "{\"type\":\"text\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_2\",\"text\":\"# opencode result\\nVerified.\"}}\n",
    "{\"type\":\"step_finish\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_2\",\"reason\":\"stop\",\"tokens\":{\"total\":30,\"input\":20,\"output\":10}}}\n"
);

const COMPOUND_PERMISSION_HALT_OUTPUT: &str = concat!(
    "{\"type\":\"step_start\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\"}}\n",
    "{\"type\":\"tool_use\",\"sessionID\":\"ses_A\",\"part\":{\"tool\":\"bash\",\"callID\":\"call_1\",\"state\":{\"status\":\"error\",\"input\":{\"command\":\"pwd && ls -la\"},\"error\":\"The user rejected permission to use this specific tool call.\"}}}\n",
    "{\"type\":\"step_finish\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\",\"reason\":\"tool-calls\",\"tokens\":{\"total\":50,\"input\":40,\"output\":10}}}\n"
);

const COMPOUND_RESUME_AFTER_APPROVAL_OUTPUT: &str = concat!(
    "{\"type\":\"step_start\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_2\"}}\n",
    "{\"type\":\"tool_use\",\"sessionID\":\"ses_A\",\"part\":{\"tool\":\"bash\",\"callID\":\"call_1\",\"state\":{\"status\":\"completed\",\"input\":{\"command\":\"pwd && ls -la\"},\"output\":\"\"}}}\n",
    "{\"type\":\"text\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_2\",\"text\":\"# opencode result\\nDone.\"}}\n",
    "{\"type\":\"step_finish\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_2\",\"reason\":\"stop\",\"tokens\":{\"total\":30,\"input\":20,\"output\":10}}}\n"
);

const INSUFFICIENT_BALANCE_OUTPUT: &str = "{\"type\":\"error\",\"sessionID\":\"ses_A\",\"error\":{\"name\":\"APIError\",\"data\":{\"message\":\"Insufficient Balance\",\"statusCode\":402}}}\n";

type RecordedInvocation = (u32, Vec<String>);

/// Shared backend for single-invocation and halt/resume protocol fixtures.
#[derive(Clone)]
struct FixtureBackend {
    started: Arc<Mutex<HashSet<ManagedProcessId>>>,
    completed: Arc<Mutex<HashSet<ManagedProcessId>>>,
    output: Arc<String>,
    resume_output: Option<&'static str>,
    exit: Option<ExitResult>,
    invocations: Arc<Mutex<Vec<RecordedInvocation>>>,
}

impl FixtureBackend {
    fn new(output: &str) -> Self {
        Self {
            started: Arc::new(Mutex::new(HashSet::new())),
            completed: Arc::new(Mutex::new(HashSet::new())),
            output: Arc::new(output.to_owned()),
            resume_output: None,
            exit: Some(ExitResult::ExitCode { code: 0 }),
            invocations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_exit(mut self, exit: Option<ExitResult>) -> Self {
        self.exit = exit;
        self
    }

    fn with_resume(mut self, output: &'static str) -> Self {
        self.resume_output = Some(output);
        self
    }
}

impl ProcessBackend for FixtureBackend {
    fn kind(&self) -> &'static str {
        "fixture"
    }

    fn session_id(&self, process_id: ManagedProcessId) -> BackendSessionId {
        BackendSessionId::for_process(process_id)
    }

    fn availability(&self) -> Result<BackendAvailability, ProcessError> {
        Ok(BackendAvailability {
            kind: self.kind(),
            version: "fixture-1".to_owned(),
        })
    }

    fn start(&self, process: &ManagedProcess, _manifest: &Path) -> Result<(), ProcessError> {
        let output = if process.invocation() > 1 {
            self.resume_output.unwrap_or(self.output.as_str())
        } else {
            self.output.as_str()
        };
        std::fs::write(process.spec().stdout_path(), output)?;
        self.invocations.lock().unwrap().push((
            process.invocation(),
            process
                .spec()
                .argv()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        ));
        self.started.lock().unwrap().insert(process.id());
        Ok(())
    }

    fn inspect_session(&self, process: &ManagedProcess) -> Result<BackendSessionState, ProcessError> {
        if self.started.lock().unwrap().contains(&process.id()) {
            self.completed.lock().unwrap().insert(process.id());
        }
        Ok(BackendSessionState::Absent)
    }

    fn read_output(
        &self,
        process: &ManagedProcess,
        stream: OutputStream,
        offset: u64,
        max_bytes: usize,
    ) -> Result<OutputChunk, ProcessError> {
        let path = match stream {
            OutputStream::Stdout => process.spec().stdout_path(),
            OutputStream::Stderr => process.spec().stderr_path(),
        };
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take(u64::try_from(max_bytes).unwrap())
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
        let path = match stream {
            OutputStream::Stdout => process.spec().stdout_path(),
            OutputStream::Stderr => process.spec().stderr_path(),
        };
        Ok(std::fs::metadata(path)?.len())
    }

    fn read_exit_evidence(
        &self,
        process: &ManagedProcess,
    ) -> Result<Option<ExitEvidence>, ProcessError> {
        let Some(result) = self.exit.clone() else {
            return Ok(None);
        };
        if !self.completed.lock().unwrap().contains(&process.id()) {
            return Ok(None);
        }
        let now = OpencodeProvider::<Self>::now();
        Ok(Some(ExitEvidence::new(
            process.id(),
            process.command_fingerprint().to_owned(),
            result,
            false,
            now,
            now,
        )))
    }

    fn signal(
        &self,
        _process: &ManagedProcess,
        _signal: TerminationSignal,
    ) -> Result<(), ProcessError> {
        Ok(())
    }

    fn cleanup(&self, _process: &ManagedProcess) -> Result<(), ProcessError> {
        Ok(())
    }
}

fn fixture(
    output: &str,
) -> (
    TempDir,
    PathBuf,
    crate::domain::RunId,
    SqliteStore,
    OpencodeProvider<FixtureBackend>,
) {
    fixture_with(FixtureBackend::new(output))
}

fn fixture_with<B: ProcessBackend>(
    backend: B,
) -> (
    TempDir,
    PathBuf,
    crate::domain::RunId,
    SqliteStore,
    OpencodeProvider<B>,
) {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    init_repository(&source);
    let database = temp.path().join("senate.db");
    let process_root = temp.path().join("runs");
    let run_id = crate::domain::RunId::new();
    let created_at = OpencodeProvider::<B>::now();
    let config_id = ConfigSnapshotId::new(format!("opencode-{run_id}")).unwrap();
    let run = Run::new(run_id, implementation_only(), config_id.clone(), created_at);
    let input = RunInput::new(run_id, "fixture task", created_at).unwrap();
    let config = opencode_config(config_id, created_at);
    let created = run.created_event(EventMetadata::new(EventId::new(), created_at));
    let mut store = SqliteStore::open(&database).unwrap();
    store
        .create_run_with_input(&run, &input, &config, &[created])
        .unwrap();
    WorkspaceManager::new(temp.path().join("worktrees"))
        .prepare_run_workspace(&mut store, run_id, &source)
        .unwrap();
    let provider = OpencodeProvider {
        id: ProviderId::new("opencode").unwrap(),
        installation: OpencodeInstallation::fixture(PathBuf::from("/bin/true")),
        model: None,
        effort: EffortSetting::NativeDefault,
        manager: ProcessManager::new(&process_root, backend),
        artifact_root: process_root,
    };
    (temp, database, run_id, store, provider)
}

fn opencode_config(
    config_id: ConfigSnapshotId,
    created_at: DateTime<Utc>,
) -> ResolvedConfigSnapshot {
    ResolvedConfigSnapshot::new(
        config_id,
        1,
        json!({
            "schema_version":1,
            "profile":"native_opencode",
            "provider":"opencode",
            "model":null,
            "provider_options":{
                "execution_protocol":"run_json_v1",
                "permission_policy":"stage_kind_v1"
            }
        }),
        created_at,
    )
    .unwrap()
}

fn init_repository(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    command(path, &["init"]);
    command(path, &["config", "user.email", "senate@example.invalid"]);
    command(path, &["config", "user.name", "The Senate Test"]);
    std::fs::write(path.join("README.md"), "fixture\n").unwrap();
    command(path, &["add", "README.md"]);
    command(path, &["commit", "-m", "fixture"]);
}

fn command(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn drive_to_completion<B: ProcessBackend>(
    engine: &mut WorkflowEngine<OpencodeProvider<B>>,
    store: &mut SqliteStore,
    run_id: crate::domain::RunId,
) -> RunStatus {
    loop {
        match engine.drive(store, run_id).unwrap() {
            EngineStatus::Finished { run_status } => return run_status,
            EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
            status => panic!("unexpected status: {status:?}"),
        }
    }
}

fn await_attention(
    engine: &mut WorkflowEngine<OpencodeProvider<FixtureBackend>>,
    store: &mut SqliteStore,
    run_id: crate::domain::RunId,
) -> AttentionRequestId {
    loop {
        match engine.drive(store, run_id).unwrap() {
            EngineStatus::NeedsUser { requests } => return requests[0],
            EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
            status => panic!("unexpected status: {status:?}"),
        }
    }
}

#[test]
fn successful_turn_persists_artifact_usage_and_completes() {
    let (_temp, database, run_id, mut store, provider) = fixture(SUCCESS_OUTPUT);
    let marker = "SUPER_SECRET_TASK_MARKER";
    let mut engine = WorkflowEngine::new(provider, marker);
    assert_eq!(
        drive_to_completion(&mut engine, &mut store, run_id),
        RunStatus::Completed
    );
    let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
    assert_eq!(session.status(), ProviderSessionStatus::Completed);
    assert_eq!(session.native_session_id().unwrap().as_str(), "ses_A");
    let process = store
        .load_managed_process(session.current_process_id().unwrap())
        .unwrap();
    let argv = process
        .spec()
        .argv()
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>();
    assert!(argv.iter().all(|arg| !arg.contains(marker)));
    let stdin = std::fs::read_to_string(process.spec().stdin_path().unwrap()).unwrap();
    assert!(stdin.contains(marker));
    assert!(process.spec().stdin_sha256().is_some());
    assert_eq!(
        process
            .spec()
            .environment()
            .get(&std::ffi::OsString::from("OPENCODE_DISABLE_PROJECT_CONFIG")),
        Some(&std::ffi::OsString::from("1"))
    );
    assert!(
        process
            .spec()
            .environment()
            .contains_key(&std::ffi::OsString::from("OPENCODE_CONFIG"))
    );
    assert!(
        process
            .spec()
            .environment()
            .contains_key(&std::ffi::OsString::from("OPENCODE_PERMISSION"))
    );
    assert!(argv.iter().any(|arg| arg == "--pure"));
    let events = store.load_events(run_id).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.event.kind(),
                DomainEventKind::ProviderUsageUpdated {
                    input_units: 100,
                    output_units: 47,
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.event.kind(),
                DomainEventKind::ProviderCompleted { .. }
            ))
            .count(),
        1
    );
    let artifacts = store.list_artifacts(run_id).unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].metadata().provider_id().unwrap().as_str(), "opencode");
    let content = std::fs::read_to_string(artifacts[0].path()).unwrap();
    assert!(content.contains("# opencode result"));
    drop(store);
    let mut store = SqliteStore::open(database).unwrap();
    assert_eq!(store.load_run(run_id).unwrap().run.status(), RunStatus::Completed);
}

#[test]
fn large_composed_prompts_are_bound_to_managed_stdin_not_argv() {
    let (_temp, _database, run_id, mut store, provider) = fixture(SUCCESS_OUTPUT);
    let task = "x".repeat(150_000);
    let mut engine = WorkflowEngine::new(provider, &task);
    assert_eq!(
        drive_to_completion(&mut engine, &mut store, run_id),
        RunStatus::Completed
    );
    let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
    let process = store
        .load_managed_process(session.current_process_id().unwrap())
        .unwrap();
    let stdin = std::fs::read_to_string(process.spec().stdin_path().unwrap()).unwrap();
    assert!(stdin.contains(&task));
    assert!(stdin.len() > 128 * 1024);
    assert!(process.spec().argv().iter().all(|arg| arg.len() < 128 * 1024));
}

#[test]
fn a_permission_halt_raises_attention_and_approval_resumes_and_completes() {
    let backend = FixtureBackend::new(PERMISSION_HALT_OUTPUT)
        .with_resume(RESUME_AFTER_APPROVAL_OUTPUT);
    let inspector = backend.clone();
    let (temp, _database, run_id, mut store, provider) = fixture_with(backend);
    let mut engine = WorkflowEngine::new(provider, "fix the bug");
    let request_id = await_attention(&mut engine, &mut store, run_id);
    let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
    assert_eq!(session.status(), ProviderSessionStatus::NeedsUser);
    assert_eq!(session.native_session_id().unwrap().as_str(), "ses_A");
    let loaded = store.load_run(run_id).unwrap();
    let attention = loaded.run.attention_requests().iter().find(|request| request.id() == request_id).unwrap();
    assert_eq!(attention.kind(), AttentionKind::Permission);
    assert!(attention.summary().contains("python3 -c"));
    engine
        .resolve_attention_with_response(&mut store, run_id, request_id, None)
        .unwrap();
    assert_eq!(
        drive_to_completion(&mut engine, &mut store, run_id),
        RunStatus::Completed
    );
    let invocations = inspector.invocations.lock().unwrap().clone();
    assert_eq!(invocations.len(), 2);
    assert_eq!(invocations[0].0, 1);
    assert_eq!(invocations[1].0, 2);
    assert!(invocations[1].1.windows(2).any(|pair| pair[0] == "--session" && pair[1] == "ses_A"));
    let config_path = temp.path().join("runs").join(run_id.to_string())
        .join("provider-output").join("opencode").join(session.id().to_string())
        .join("invocation-2.config.json");
    let config: Value = serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
    assert_eq!(config["permission"]["bash"]["python3 -c \"from calc import add; assert add(2,3)==5\""], json!("allow"));
    let resumed = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
    let process = store.load_managed_process(resumed.current_process_id().unwrap()).unwrap();
    let stdin = std::fs::read_to_string(process.spec().stdin_path().unwrap()).unwrap();
    assert!(stdin.contains("The operator approved"));
    assert!(process.spec().argv().iter().all(|arg| !arg.to_string_lossy().contains("The operator approved")));
}

#[test]
fn approving_a_compound_denial_grants_every_split_part() {
    let backend = FixtureBackend::new(COMPOUND_PERMISSION_HALT_OUTPUT)
        .with_resume(COMPOUND_RESUME_AFTER_APPROVAL_OUTPUT);
    let (temp, _database, run_id, mut store, provider) = fixture_with(backend);
    let mut engine = WorkflowEngine::new(provider, "fix the bug");
    let request_id = await_attention(&mut engine, &mut store, run_id);
    let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
    engine.resolve_attention_with_response(&mut store, run_id, request_id, None).unwrap();
    assert_eq!(drive_to_completion(&mut engine, &mut store, run_id), RunStatus::Completed);
    let config_path = temp.path().join("runs").join(run_id.to_string())
        .join("provider-output").join("opencode").join(session.id().to_string())
        .join("invocation-2.config.json");
    let config: Value = serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
    assert_eq!(config["permission"]["bash"]["pwd"], json!("allow"));
    assert_eq!(config["permission"]["bash"]["ls -la"], json!("allow"));
}

#[test]
fn a_declined_permission_halt_resumes_without_granting_anything() {
    let backend = FixtureBackend::new(PERMISSION_HALT_OUTPUT).with_resume(RESUME_AFTER_APPROVAL_OUTPUT);
    let (temp, _database, run_id, mut store, provider) = fixture_with(backend);
    let mut engine = WorkflowEngine::new(provider, "fix the bug");
    let request_id = await_attention(&mut engine, &mut store, run_id);
    let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
    engine.resolve_attention_with_response(&mut store, run_id, request_id, Some("Continue without running it.")).unwrap();
    assert_eq!(drive_to_completion(&mut engine, &mut store, run_id), RunStatus::Completed);
    let config_path = temp.path().join("runs").join(run_id.to_string())
        .join("provider-output").join("opencode").join(session.id().to_string())
        .join("invocation-2.config.json");
    let config: Value = serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
    assert_eq!(config["permission"]["bash"].get("python3 -c \"from calc import add; assert add(2,3)==5\""), None);
    let resumed = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
    let process = store.load_managed_process(resumed.current_process_id().unwrap()).unwrap();
    let stdin = std::fs::read_to_string(process.spec().stdin_path().unwrap()).unwrap();
    assert!(stdin.contains("The operator declined"));
    assert!(stdin.contains("Continue without running it."));
}

#[test]
fn a_vendor_error_fails_the_stage_with_a_scrubbed_message() {
    let (_temp, _database, run_id, mut store, provider) = fixture(INSUFFICIENT_BALANCE_OUTPUT);
    let mut engine = WorkflowEngine::new(provider, "fixture task");
    assert_eq!(drive_to_completion(&mut engine, &mut store, run_id), RunStatus::Failed);
    let events = store.load_events(run_id).unwrap();
    assert!(events.iter().any(|event| matches!(
        event.event.kind(),
        DomainEventKind::ProviderFailed { reason: Some(reason), .. }
            if reason.contains("APIError") && reason.contains("Insufficient Balance")
    )));
}

#[test]
fn a_stop_step_after_an_unclean_exit_is_a_recoverable_interruption_not_a_completion() {
    let (_temp, _database, run_id, mut store, provider) = fixture_with(
        FixtureBackend::new(SUCCESS_OUTPUT).with_exit(Some(ExitResult::ExitCode { code: 1 })),
    );
    let mut engine = WorkflowEngine::new(provider, "fixture task");
    loop {
        match engine.drive(&mut store, run_id).unwrap() {
            EngineStatus::Interrupted { .. } => break,
            EngineStatus::Finished { run_status } => panic!("unexpected terminal status: {run_status:?}"),
            EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
            other => panic!("unexpected status: {other:?}"),
        }
    }
    assert!(store.list_artifacts(run_id).unwrap().is_empty());
    let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
    assert_eq!(session.status(), ProviderSessionStatus::Interrupted);
}

#[test]
fn a_resumed_invocations_first_record_reports_resumed_not_started() {
    let (_temp, _database, run_id, mut store, mut provider) = fixture(SUCCESS_OUTPUT);
    let workspace = store.load_workspace(run_id).unwrap().unwrap();
    let mut session = ProviderSessionRecord::new(
        ProviderSessionRecordId::new(), run_id, StageId::new("implementation").unwrap(), 1,
        ProviderId::new("opencode").unwrap(), PROTOCOL_VERSION, None,
        OpencodeProvider::<FixtureBackend>::now(),
    );
    session.activate(ProviderSessionId::new("ses_A").unwrap(), None, OpencodeProvider::<FixtureBackend>::now()).unwrap();
    let process = provider.manager.prepare_with_input(
        &mut store, run_id, StageId::new("implementation").unwrap(), 1, 2,
        Path::new("/bin/true"), vec![], BTreeMap::new(), &[],
    ).unwrap();
    session.bind_process(process.id(), 2, OpencodeProvider::<FixtureBackend>::now()).unwrap();
    let request = ProviderRequest::new(
        run_id, StageId::new("implementation").unwrap(), StageKind::Implementation,
        StageStatus::Running, Role::Implementer, "fixture task".to_owned(),
        workspace.worktree_path().to_path_buf(), 1, 1,
        Some(ProviderSessionId::new("ses_A").unwrap()), vec![],
    );
    let event = OpencodeEvent { session_id: "ses_A".to_owned(), kind: OpencodeKind::StepStart };
    let chunk = OutputChunk::new(process.id(), OutputStream::Stdout, 0, 0, Vec::new()).unwrap();
    let poll = provider.map_record(&mut store, &request, session, chunk, 0, event, ManagedProcessStatus::Preparing, false).unwrap();
    match poll {
        ProviderPoll::Emission { signals, .. } => assert_eq!(signals, vec![ProviderSignal::Resumed]),
        other => panic!("expected an Emission carrying Resumed, got {other:?}"),
    }
}
