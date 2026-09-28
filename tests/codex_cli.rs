#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use senate::domain::{ArtifactKind, Role, RunId, RunStatus};
use senate::process::ManagedProcess;
use senate::store::SqliteStore;

/// A completed run whose decision the operator rejects is sent back in place.
///
/// The run grows a fix and a fresh decision over it, keeps its own workspace
/// and identity, and leaves the operator's checkout alone throughout — the
/// remediation is not a second run, and it is not an early apply.
#[test]
fn a_rejected_run_is_fixed_in_place_and_the_source_is_untouched_until_apply() {
    let fixture = Fixture::new();
    let started = fixture.senate(
        &[
            "standard",
            "Fix cycle task",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--provider",
            "codex",
        ],
        true,
        false,
    );
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    let run_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap()
        .to_owned();

    let before = fixture.status(&run_id);
    assert!(before.contains("Status     completed"), "{before}");
    assert!(before.contains("decision"), "{before}");
    assert!(
        !before.contains("fix_1"),
        "a run grows a fix only when asked: {before}"
    );

    // The operator read the verdict and rejected it. Nothing about the
    // decision's own wording is consulted.
    let fixed = fixture.senate(&["fix", &run_id], true, false);
    assert_success(&fixed);

    let after = fixture.status(&run_id);
    assert!(after.contains("fix_1"), "{after}");
    assert!(after.contains("decision_1"), "{after}");
    assert!(
        after.contains("Status     completed"),
        "the cycle runs to a new verdict: {after}"
    );

    // The fix produced its own artifact, and it is the fix contract's, not a
    // second implementation's.
    let artifact = fixture
        .data
        .join("runs")
        .join(&run_id)
        .join("artifacts")
        .join("fix_1.md");
    assert!(artifact.exists(), "fix artifact missing at {artifact:?}");

    // Everything it changed stayed in the run's own workspace.
    assert!(
        !fixture.repo.join("hello.txt").exists(),
        "a fix is not an apply"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("README.md")).unwrap(),
        "baseline\n"
    );

    // Rejecting again answers the newest verdict rather than the first.
    let again = fixture.senate(&["fix", &run_id], true, false);
    assert_success(&again);
    let after = fixture.status(&run_id);
    assert!(after.contains("fix_2"), "{after}");
    assert!(after.contains("decision_2"), "{after}");

    let applied = fixture.senate(&["apply", &run_id], false, false);
    assert_success(&applied);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("hello.txt")).unwrap(),
        "created by fake Codex\n"
    );

    // A run that has been applied is finished; there is nothing left to send
    // back.
    let refused = fixture.senate(&["fix", &run_id], false, false);
    assert!(
        !refused.status.success(),
        "fix accepted after apply: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
}

#[test]
fn native_codex_fixture_runs_through_tmux_preserves_source_then_applies() {
    let fixture = Fixture::new();
    let marker = "SUPER_SECRET_TASK_MARKER";
    let started = fixture.senate(
        &[
            "fast",
            marker,
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--provider",
            "codex",
        ],
        true,
        false,
    );
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    assert!(stdout.contains("Status     completed"));
    assert!(stdout.contains("implementer  codex"));
    assert!(stdout.contains("native=codex-thread-implementation"));
    // Codex never names its model or its reasoning effort on stdout. Both come
    // from the session record it writes for itself, so a run routed with no
    // pinned model still reports what actually ran instead of "unconfirmed".
    assert!(stdout.contains("actual=codex/gpt-5.6-luna"), "{stdout}");
    assert!(
        stdout.contains("effort=native default requested → xhigh observed"),
        "{stdout}"
    );
    // Codex folds its cached input into `input_tokens`, so the 3 cached
    // units are named inside the total and never listed again as a separate
    // cache-read dimension: that would report 14 units of input for 11.
    assert!(
        stdout.contains(
            "Usage      codex    11 input units (3 of them cached) · 7 output units · 2 reasoning output units"
        ),
        "{stdout}"
    );
    assert!(!stdout.contains("cache read units"), "{stdout}");
    let run_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap();

    assert!(!fixture.repo.join("hello.txt").exists());
    assert_eq!(
        fs::read_to_string(fixture.repo.join("README.md")).unwrap(),
        "baseline\n"
    );
    let argv = fs::read_to_string(fixture.capture.join("implementation.argv")).unwrap();
    let stdin = fs::read_to_string(fixture.capture.join("implementation.stdin")).unwrap();
    assert!(!argv.contains(marker));
    assert!(stdin.contains(marker));
    assert!(argv.contains("--sandbox\nworkspace-write"));
    assert!(argv.contains("--ask-for-approval\nnever"));
    for forbidden in [
        "--yolo",
        "danger-full-access",
        "--ephemeral",
        "--skip-git-repo-check",
        "--ignore-user-config",
        "--ignore-rules",
        "--dangerously-bypass-hook-trust",
    ] {
        assert!(!argv.contains(forbidden));
    }
    let artifact = fixture
        .data
        .join("runs")
        .join(run_id)
        .join("artifacts")
        .join("implementation.md");
    assert!(
        fs::read_to_string(artifact)
            .unwrap()
            .contains("Fake Codex completed")
    );

    let status = fixture.senate(&["status", run_id], false, false);
    assert_success(&status);
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(status.contains("configured=codex/native default"));
    assert!(status.contains("conversation=completed"));
    assert!(!status.contains("fixture-secret"));

    let applied = fixture.senate(&["apply", run_id], false, false);
    assert_success(&applied);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("hello.txt")).unwrap(),
        "created by fake Codex\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("README.md")).unwrap(),
        "fixture changed by fake Codex\n"
    );
}

#[test]
fn doctor_reports_codex_health_without_leaking_auth_output_or_creating_db() {
    let fixture = Fixture::new();
    let healthy = fixture.senate(&["doctor"], false, false);
    assert_success(&healthy);
    let stdout = String::from_utf8(healthy.stdout).unwrap();
    // Operational readiness, not development history.
    assert!(
        !stdout.contains("Milestone"),
        "doctor describes the product, not the milestone it was built in: {stdout}"
    );
    // Git is a runtime prerequisite, so it is diagnosed like the providers are.
    assert!(stdout.contains("Git: available ("), "{stdout}");
    assert!(stdout.contains("Codex CLI: available (codex-cli fixture-1)"));
    assert!(stdout.contains("Codex auth: ready (ChatGPT)"));
    assert!(!stdout.contains("fixture-secret"));
    assert!(!fixture.data.join("senate.db").exists());

    let logged_out = fixture.senate(&["doctor"], false, true);
    assert_success(&logged_out);
    assert!(
        String::from_utf8(logged_out.stdout)
            .unwrap()
            .contains("Codex auth: not authenticated")
    );
    let rejected = fixture.senate(
        &[
            "fast",
            "task",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--provider",
            "codex",
        ],
        false,
        true,
    );
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8(rejected.stderr)
            .unwrap()
            .contains("not authenticated")
    );
    assert!(!fixture.data.join("senate.db").exists());
}

#[test]
fn standard_run_uses_separate_threads_direct_artifacts_and_stage_sandboxes() {
    let fixture = Fixture::new();
    let started = fixture.senate(
        &[
            "standard",
            "Analyze then implement fixture task",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--provider",
            "codex",
        ],
        false,
        false,
    );
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    assert!(stdout.contains("Status     completed"));
    let run_id: RunId = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap()
        .parse()
        .unwrap();

    let mut store = SqliteStore::open(fixture.data.join("senate.db")).unwrap();
    assert_eq!(
        store.load_run(run_id).unwrap().run.status(),
        RunStatus::Completed
    );
    let sessions = store.list_provider_sessions(run_id).unwrap();
    assert_eq!(sessions.len(), 6);
    let native = sessions
        .iter()
        .map(|session| session.native_session_id().unwrap().as_str().to_owned())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(native.len(), 6);
    assert!(native.contains("codex-thread-architecture"));
    assert!(native.contains("codex-thread-implementation"));
    assert!(native.contains("codex-thread-simplification"));
    assert!(native.contains("codex-thread-quality_review"));
    assert!(native.contains("codex-thread-spec_review"));
    assert!(native.contains("codex-thread-decision"));

    let implementation = fs::read_to_string(fixture.capture.join("implementation.stdin")).unwrap();
    assert!(implementation.contains("# architecture result"));
    let quality = fs::read_to_string(fixture.capture.join("quality_review.stdin")).unwrap();
    assert!(quality.contains("# implementation result"));
    assert!(!quality.contains("# architecture result"));
    assert!(quality.contains("Judge HOW the implementation is engineered"));
    assert!(quality.contains("Do not repeat a complete requirement"));

    let spec = fs::read_to_string(fixture.capture.join("spec_review.stdin")).unwrap();
    assert!(spec.contains("# architecture result"));
    assert!(spec.contains("# implementation result"));
    assert!(spec.contains("Judge WHAT behavior was delivered"));
    assert!(spec.contains("Missing, Wrong, or Unrequested"));

    let decision = fs::read_to_string(fixture.capture.join("decision.stdin")).unwrap();
    assert!(decision.contains("# quality_review result"));
    assert!(decision.contains("# spec_review result"));
    assert!(!decision.contains("# architecture result"));
    assert!(!decision.contains("# implementation result"));
    assert!(decision.contains("implementation quality and specification compliance"));
    for stage in ["implementation", "simplification"] {
        assert!(
            fs::read_to_string(fixture.capture.join(format!("{stage}.argv")))
                .unwrap()
                .contains("--sandbox\nworkspace-write")
        );
    }
    for stage in ["architecture", "quality_review", "spec_review", "decision"] {
        assert!(
            fs::read_to_string(fixture.capture.join(format!("{stage}.argv")))
                .unwrap()
                .contains("--sandbox\nread-only")
        );
    }
    let artifact_root = fixture
        .data
        .join("runs")
        .join(run_id.to_string())
        .join("artifacts");
    assert!(artifact_root.join("quality_review.md").is_file());
    assert!(artifact_root.join("spec_review.md").is_file());
    let artifacts = store.list_artifacts(run_id).unwrap();
    let quality_artifact = artifacts
        .iter()
        .find(|artifact| artifact.metadata().stage_id().as_str() == "quality_review")
        .unwrap();
    assert_eq!(
        quality_artifact.metadata().kind(),
        ArtifactKind::CodeQualityReview
    );
    assert_eq!(
        quality_artifact.metadata().role(),
        Role::CodeQualityReviewer
    );
    let spec_artifact = artifacts
        .iter()
        .find(|artifact| artifact.metadata().stage_id().as_str() == "spec_review")
        .unwrap();
    assert_eq!(spec_artifact.metadata().kind(), ArtifactKind::SpecReview);
    assert_eq!(spec_artifact.metadata().role(), Role::SpecReviewer);
    assert_eq!(git_output(&fixture.repo, &["status", "--porcelain"]), "");
}

#[test]
fn retry_creates_new_provider_session_and_new_native_thread() {
    let fixture = Fixture::new();
    let failed = fixture.senate_with_fail_once(&[
        "fast",
        "retry fixture task",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--provider",
        "codex",
    ]);
    assert_success(&failed);
    let failed = String::from_utf8(failed.stdout).unwrap();
    assert!(failed.contains("Status     failed"));
    let run_id = failed
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap();

    let retried = fixture.senate_with_fail_once(&["retry", run_id, "implementation"]);
    assert_success(&retried);
    assert!(
        String::from_utf8(retried.stdout)
            .unwrap()
            .contains("Status     completed")
    );
    let run_id: RunId = run_id.parse().unwrap();
    let store = SqliteStore::open(fixture.data.join("senate.db")).unwrap();
    let sessions = store.list_provider_sessions(run_id).unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].attempt(), 1);
    assert_eq!(sessions[1].attempt(), 2);
    assert_eq!(
        sessions[0].native_session_id().unwrap().as_str(),
        "codex-thread-implementation-attempt-1"
    );
    assert_eq!(
        sessions[1].native_session_id().unwrap().as_str(),
        "codex-thread-implementation-attempt-2"
    );
    let second_process = store
        .load_managed_process(sessions[1].current_process_id().unwrap())
        .unwrap();
    assert!(
        !second_process
            .spec()
            .argv()
            .iter()
            .any(|arg| arg == "resume")
    );
}

#[test]
fn detached_frontend_leaves_tmux_provider_alive_and_resume_consumes_retained_output() {
    let fixture = Fixture::new();
    let mut frontend = fixture.spawn_waiting_codex(&[
        "fast",
        "detach fixture task",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--provider",
        "codex",
    ]);
    // Generous on purpose: this waits for a real process under tmux to come
    // up, which is setup rather than the property under test. A budget tight
    // enough to be tripped by a loaded runner makes the gate report on the
    // machine instead of on the code — the sibling wait below already uses 30s.
    let deadline = Instant::now() + Duration::from_secs(30);
    let (run_id, process_id, session_name) = loop {
        if fixture.data.join("senate.db").exists()
            && let Ok(store) = SqliteStore::open(fixture.data.join("senate.db"))
            && let Ok(runs) = store.list_runs()
            && let Some(run) = runs.first()
            && let Ok(processes) = store.list_managed_processes(run.id)
            && let Some(process) = processes.first()
            && process.status().is_active()
        {
            let session = process.backend_session_id().as_str().to_owned();
            let live = Command::new(fixture.fake_bin.join("tmux"))
                .args(["-L", &session, "has-session", "-t", &session])
                .output()
                .unwrap();
            if live.status.success()
                && fixture.data.join("release-provider.waiting").exists()
                && !fs::read_to_string(process.spec().stdout_path())
                    .unwrap()
                    .contains("turn.completed")
            {
                break (run.id, process.id(), session);
            }
        }
        assert!(Instant::now() < deadline, "managed provider did not start");
        std::thread::sleep(Duration::from_millis(25));
    };

    frontend.kill().unwrap();
    let _ = frontend.wait().unwrap();
    let tmux = Command::new(fixture.fake_bin.join("tmux"))
        .args(["-L", &session_name, "has-session", "-t", &session_name])
        .output()
        .unwrap();
    assert!(
        tmux.status.success(),
        "frontend exit killed managed provider: {}",
        String::from_utf8_lossy(&tmux.stderr)
    );
    fs::write(fixture.data.join("release-provider"), b"continue").unwrap();

    let finished = Instant::now() + Duration::from_secs(30);
    loop {
        let store = SqliteStore::open(fixture.data.join("senate.db")).unwrap();
        let process = store.load_managed_process(process_id).unwrap();
        if fs::read_to_string(process.spec().stdout_path())
            .unwrap()
            .contains("turn.completed")
        {
            break;
        }
        assert!(
            Instant::now() < finished,
            "detached provider output did not finish"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let resumed = fixture.senate(&["resume", &run_id.to_string()], false, false);
    assert_success(&resumed);
    assert!(
        String::from_utf8(resumed.stdout)
            .unwrap()
            .contains("Status     completed")
    );
    let store = SqliteStore::open(fixture.data.join("senate.db")).unwrap();
    assert_eq!(store.list_provider_sessions(run_id).unwrap().len(), 1);
    assert_eq!(store.list_managed_processes(run_id).unwrap().len(), 1);
}

// Stopping a run is normally done while a second Senate process is still
// driving it, and that driver keeps bumping the revision of the very managed
// process rows the stop has to transition. This exercises that contention for
// real: the frontend stays attached and blocked inside the provider turn while
// `senate stop` runs against the same database.
#[test]
fn stop_interrupts_a_live_run_while_its_driver_is_still_attached() {
    let fixture = Fixture::new();
    let mut frontend = fixture.spawn_waiting_codex(&[
        "standard",
        "stop fixture task",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--provider",
        "codex",
    ]);
    // Generous: the whole suite runs in parallel, and this test waits for a
    // real tmux-backed provider to reach its blocking point.
    let deadline = Instant::now() + Duration::from_secs(30);
    let (run_id, worktree) = loop {
        if fixture.data.join("senate.db").exists()
            && fixture.data.join("release-provider.waiting").exists()
            && let Ok(store) = SqliteStore::open(fixture.data.join("senate.db"))
            && let Ok(runs) = store.list_runs()
            && let Some(run) = runs.first()
            && let Ok(processes) = store.list_managed_processes(run.id)
            && let Some(process) = processes.first()
            && process.status().is_active()
            && let Ok(Some(workspace)) = store.load_workspace(run.id)
        {
            break (run.id, workspace.worktree_path().to_path_buf());
        }
        assert!(Instant::now() < deadline, "managed provider did not start");
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(worktree.exists(), "worktree missing before stop");

    // The driver is deliberately still alive here: this is the race.
    let stopped = fixture.senate(&["stop", &run_id.to_string()], false, false);
    let stderr = String::from_utf8(stopped.stderr.clone()).unwrap();
    assert!(
        !stderr.contains("changed since revision"),
        "stop surfaced a revision race instead of retrying: {stderr}"
    );
    assert_success(&stopped);
    assert!(
        String::from_utf8(stopped.stdout)
            .unwrap()
            .contains("Status     interrupted"),
        "stop did not durably interrupt the run"
    );

    let _ = frontend.kill();
    let _ = frontend.wait();

    // Stop keeps the work: the run stays interrupted, its workspace and its
    // managed-process record survive, and no new attempt was created.
    let mut store = SqliteStore::open(fixture.data.join("senate.db")).unwrap();
    assert_eq!(
        store.load_run(run_id).unwrap().run.status(),
        RunStatus::Interrupted
    );
    assert!(worktree.exists(), "stop deleted the worktree");
    assert_eq!(store.list_managed_processes(run_id).unwrap().len(), 1);

    // Idempotent: a second stop neither errors nor changes the state.
    let again = fixture.senate(&["stop", &run_id.to_string()], false, false);
    assert_success(&again);
    assert_eq!(
        store.load_run(run_id).unwrap().run.status(),
        RunStatus::Interrupted
    );

    // One resume is enough. Stop must leave the stage durably interrupted, so
    // that recovery both restores the lifecycle and resumes the provider in
    // the same command instead of stranding the run in Running.
    let resumed = fixture.senate(&["resume", &run_id.to_string()], false, false);
    assert_success(&resumed);
    assert!(
        String::from_utf8(resumed.stdout)
            .unwrap()
            .contains("Status     completed"),
        "a stopped run needed more than one resume to continue"
    );
    // Recovery continued the interrupted attempt rather than starting a new
    // one. Resuming a native thread legitimately costs a second invocation,
    // so the invariant is the attempt number, not the process count.
    let attempts: Vec<u32> = store
        .list_managed_processes(run_id)
        .unwrap()
        .iter()
        .filter(|process| process.stage_id().as_str() == "architecture")
        .map(ManagedProcess::attempt)
        .collect();
    assert!(
        !attempts.is_empty() && attempts.iter().all(|attempt| *attempt == attempts[0]),
        "stop cost the run a retry attempt: {attempts:?}"
    );
}

struct Fixture {
    _temp: TempDir,
    repo: PathBuf,
    data: PathBuf,
    capture: PathBuf,
    fake_bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repository with spaces");
        let data = temp.path().join("data");
        let capture = temp.path().join("capture");
        let fake_bin = temp.path().join("fake-bin");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&fake_bin).unwrap();
        for tool in ["git", "tmux"] {
            let executable = find_on_path(tool).unwrap_or_else(|| panic!("{tool} is required"));
            std::os::unix::fs::symlink(executable, fake_bin.join(tool)).unwrap();
        }
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("README.md"), "baseline\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-qm", "initial"]);
        let wrapper = fake_bin.join("codex");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec '{}' codex \"$@\"\n",
                env!("CARGO_BIN_EXE_senate-test-agent")
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&wrapper, permissions).unwrap();
        Self {
            _temp: temp,
            repo,
            data,
            capture,
            fake_bin,
        }
    }

    fn status(&self, run_id: &str) -> String {
        let output = self.senate(&["status", run_id], false, false);
        assert_success(&output);
        String::from_utf8(output.stdout).unwrap()
    }

    fn senate(&self, args: &[&str], write: bool, logged_out: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_senate"));
        command
            .args(args)
            .env("PATH", &self.fake_bin)
            .env("SENATE_DATA_DIR", &self.data)
            .env("CODEX_HOME", self.data.join("codex-home"))
            .env("SENATE_FAKE_CODEX_CAPTURE_DIR", &self.capture);
        if write {
            command.env("SENATE_FAKE_CODEX_WRITE", "1");
        }
        if logged_out {
            command.env("SENATE_FAKE_CODEX_UNAUTHENTICATED", "1");
        }
        command.output().unwrap()
    }

    fn senate_with_fail_once(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_senate"))
            .args(args)
            .env("PATH", &self.fake_bin)
            .env("SENATE_DATA_DIR", &self.data)
            .env("CODEX_HOME", self.data.join("codex-home"))
            .env("SENATE_FAKE_CODEX_CAPTURE_DIR", &self.capture)
            .env(
                "SENATE_FAKE_CODEX_FAIL_ONCE_DIR",
                self.capture.join("fail-once"),
            )
            .output()
            .unwrap()
    }

    fn spawn_waiting_codex(&self, args: &[&str]) -> Child {
        Command::new(env!("CARGO_BIN_EXE_senate"))
            .args(args)
            .env("PATH", &self.fake_bin)
            .env("SENATE_DATA_DIR", &self.data)
            .env("CODEX_HOME", self.data.join("codex-home"))
            .env("SENATE_FAKE_CODEX_CAPTURE_DIR", &self.capture)
            .env(
                "SENATE_FAKE_CODEX_WAIT_FILE",
                self.data.join("release-provider"),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert_success(&output);
}

fn git_output(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert_success(&output);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
