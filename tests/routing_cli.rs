#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use senate::domain::{RunId, RunStatus};
use senate::store::SqliteStore;
use tempfile::TempDir;

#[test]
fn recommended_standard_routes_native_fixtures_per_role_and_preserves_artifact_boundary() {
    let fixture = Fixture::new();
    let started = fixture.senate_write(&[
        "standard",
        "Build mixed routing fixture",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--profile",
        "recommended",
    ]);
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    assert!(stdout.contains("Status     completed"));
    assert!(stdout.contains("Profile    recommended (recommended_v3)"));
    assert!(stdout.contains("architect  claude"));
    assert!(stdout.contains("implementer  codex"));
    let run_id: RunId = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap()
        .parse()
        .unwrap();

    let mut store = SqliteStore::open(fixture.data.join("senate.db")).unwrap();
    let loaded = store.load_run(run_id).unwrap();
    assert_eq!(loaded.run.status(), RunStatus::Completed);
    // Recommended states effort per role, so the snapshot carries a resource
    // plan under schema v3; the column itself has its own test below.
    assert_eq!(loaded.config_snapshot.schema_version(), 3);
    let sessions = store.list_provider_sessions(run_id).unwrap();
    assert_eq!(sessions.len(), 6);
    let by_stage = sessions
        .iter()
        .map(|session| (session.stage_id().as_str(), session.provider_id().as_str()))
        .collect::<HashMap<_, _>>();
    assert_eq!(by_stage["architecture"], "claude");
    assert_eq!(by_stage["implementation"], "codex");
    assert_eq!(by_stage["simplification"], "claude");
    assert_eq!(by_stage["quality_review"], "claude");
    assert_eq!(by_stage["spec_review"], "codex");
    assert_eq!(by_stage["decision"], "claude");

    let implementation = fs::read_to_string(fixture.capture.join("implementation.stdin")).unwrap();
    assert!(implementation.contains("# architecture result"));
    let quality = fs::read_to_string(fixture.capture.join("quality_review.claude.stdin")).unwrap();
    assert!(quality.contains("# implementation result"));
    let spec = fs::read_to_string(fixture.capture.join("spec_review.stdin")).unwrap();
    assert!(spec.contains("# architecture result"));
    assert!(spec.contains("# implementation result"));
    let decision = fs::read_to_string(fixture.capture.join("decision.claude.stdin")).unwrap();
    assert!(decision.contains("# quality_review result"));
    assert!(decision.contains("# spec_review result"));

    // Six native sessions plus the verify stage's own artifact: verification
    // is command execution, so it opens no provider session and is routed
    // implicitly rather than by the recommended profile.
    let artifacts = store.list_artifacts(run_id).unwrap();
    assert_eq!(artifacts.len(), 7);
    for artifact in artifacts {
        let stage = artifact.metadata().stage_id().as_str();
        let expected = if stage == "verify" {
            "verify"
        } else {
            by_stage[stage]
        };
        assert_eq!(
            artifact.metadata().provider_id().unwrap().as_str(),
            expected
        );
    }
    let events = store.load_events(run_id).unwrap();
    assert!(!events.iter().any(|event| {
        format!("{:?}", event.event.kind()).contains("router")
            || format!("{:?}", event.event.kind()).contains("recommended")
    }));
    assert_eq!(git_output(&fixture.repo, &["status", "--porcelain"]), "");

    drop(store);
    let status = fixture.senate(&["status", &run_id.to_string()]);
    assert_success(&status);
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(status.contains("architecture (completed) · role=architect · configured=claude"));
    assert!(status.contains("implementation (completed) · role=implementer · configured=codex"));
    assert!(status.contains("claude-session-architecture"));
    assert!(status.contains("codex-thread-implementation"));

    let applied = fixture.senate(&["apply", &run_id.to_string()]);
    assert_success(&applied);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("hello.txt")).unwrap(),
        "created by fake Codex\n"
    );
}

/// The Recommended profile states a requested effort per role — reasoning
/// roles high, the implementer medium, the simplifier low — and a default run
/// seals exactly that column, reports it in the Routing table, and hands each
/// native runtime its level.
#[test]
fn recommended_seals_the_profiles_effort_per_role_and_reports_it() {
    let fixture = Fixture::new();
    let started = fixture.senate_write(&[
        "standard",
        "Build effort column fixture",
        "--repo",
        fixture.repo.to_str().unwrap(),
    ]);
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    for line in [
        "architect  claude  native default  route_inherited_from_recommended_v2  effort=high",
        "implementer  codex  native default  route_inherited_from_recommended_v2_measured_at_native_default_effort  effort=medium",
        "simplifier  claude  native default  route_inherited_from_recommended_v2  effort=low",
        "code_quality_reviewer  claude  native default  route_inherited_from_recommended_v2_measured_at_native_default_effort  effort=high",
        "engineering_lead  claude  native default  route_inherited_from_recommended_v2  effort=high",
    ] {
        assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    }
    assert!(stdout.contains("· effort=medium requested"), "{stdout}");
    assert!(stdout.contains("· effort=low requested"), "{stdout}");

    let run_id: RunId = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap()
        .parse()
        .unwrap();
    let mut store = SqliteStore::open(fixture.data.join("senate.db")).unwrap();
    let loaded = store.load_run(run_id).unwrap();
    assert_eq!(loaded.config_snapshot.schema_version(), 3);
    let plan = &loaded.config_snapshot.payload()["resource_plan"];
    assert_eq!(plan["architect"], serde_json::json!("high"));
    assert_eq!(plan["implementer"], serde_json::json!("medium"));
    assert_eq!(plan["simplifier"], serde_json::json!("low"));
    assert_eq!(plan["spec_reviewer"], serde_json::json!("high"));

    // `--effort native` is the opt-out: every role native, schema v2, and
    // the profile unchanged. A fresh fixture, because the fake native agents
    // issue one fixed session identity per provider.
    let fixture = Fixture::new();
    let native = fixture.senate_write(&[
        "standard",
        "Build native effort fixture",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--effort",
        "native",
    ]);
    assert_success(&native);
    let stdout = String::from_utf8(native.stdout).unwrap();
    assert!(
        stdout.contains("Profile    recommended (recommended_v3)"),
        "{stdout}"
    );
    assert!(stdout.contains("effort=native default"), "{stdout}");
    assert!(!stdout.contains("effort=medium"), "{stdout}");
}

/// Omitting both selection flags is the same request as `--profile
/// recommended`, and says so in the report rather than routing silently.
#[test]
fn omitting_the_selection_flags_starts_the_recommended_profile() {
    let fixture = Fixture::new();
    let started = fixture.senate_write(&[
        "standard",
        "Build default routing fixture",
        "--repo",
        fixture.repo.to_str().unwrap(),
    ]);
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    assert!(stdout.contains("Status     completed"), "{stdout}");
    assert!(
        stdout.contains("Profile    recommended (recommended_v3)"),
        "the resolved profile is named, not assumed: {stdout}"
    );

    // The same per-role split the explicit flag produces, not one uniform
    // provider standing in for a profile.
    let run_id: RunId = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap()
        .parse()
        .unwrap();
    let store = SqliteStore::open(fixture.data.join("senate.db")).unwrap();
    let by_stage = store
        .list_provider_sessions(run_id)
        .unwrap()
        .iter()
        .map(|session| {
            (
                session.stage_id().as_str().to_owned(),
                session.provider_id().as_str().to_owned(),
            )
        })
        .collect::<HashMap<_, _>>();
    assert_eq!(by_stage["architecture"], "claude");
    assert_eq!(by_stage["implementation"], "codex");
    assert_eq!(by_stage["spec_review"], "codex");
}

#[test]
fn provider_and_profile_flags_conflict_before_state_creation() {
    let fixture = Fixture::new();
    let output = fixture.senate(&[
        "fast",
        "task",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--provider",
        "codex",
        "--profile",
        "recommended",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(!fixture.data.join("senate.db").exists());
}

#[test]
fn recommended_does_not_hide_unexpected_probe_failure_with_codex_fallback() {
    let fixture = Fixture::new();
    let output = fixture.senate_probe_failure(&[
        "fast",
        "task",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--profile",
        "recommended",
    ]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("expected ident")
    );
    assert!(!fixture.data.join("senate.db").exists());
}

#[test]
fn recommended_attention_restart_routes_response_to_same_claude_session() {
    let fixture = Fixture::new();
    let started = fixture.senate_with_env(
        &[
            "standard",
            "Need one answer",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--profile",
            "recommended",
        ],
        true,
    );
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    assert!(stdout.contains("Status     needs_user"));
    let run_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap();
    let attention_id = stdout
        .lines()
        .skip_while(|line| *line != "Attention")
        .find_map(|line| line.split_once(" · ").map(|(id, _)| id))
        .unwrap();

    let resolved = fixture.senate(&[
        "resolve",
        run_id,
        attention_id,
        "--response",
        "Fixture option A",
    ]);
    assert_success(&resolved);
    assert!(
        String::from_utf8(resolved.stdout)
            .unwrap()
            .contains("Status     completed")
    );
    let store = SqliteStore::open(fixture.data.join("senate.db")).unwrap();
    let run_id: RunId = run_id.parse().unwrap();
    let architecture = store
        .list_provider_sessions(run_id)
        .unwrap()
        .into_iter()
        .find(|session| session.stage_id().as_str() == "architecture")
        .unwrap();
    assert_eq!(architecture.provider_id().as_str(), "claude");
    assert_eq!(
        architecture.native_session_id().unwrap().as_str(),
        "claude-session-architecture"
    );
    assert_eq!(architecture.invocation(), 2);
    assert_eq!(
        fs::read_to_string(fixture.capture.join("architecture.claude.stdin")).unwrap(),
        "Fixture option A"
    );
}

#[test]
fn persisted_recommended_route_never_falls_back_when_codex_disappears() {
    let fixture = Fixture::new();
    let failed = fixture.senate_remove_codex_during_architecture(&[
        "standard",
        "Do not reroute implementation",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--profile",
        "recommended",
    ]);
    assert!(
        !failed.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(
        String::from_utf8(failed.stderr)
            .unwrap()
            .contains("configured provider unavailable for codex target")
    );
    let mut store = SqliteStore::open(fixture.data.join("senate.db")).unwrap();
    let run_id = store.list_runs().unwrap()[0].id;
    let loaded = store.load_run(run_id).unwrap();
    assert_eq!(
        loaded.config_snapshot.payload()["routes"]["implementer"]["provider"],
        "codex"
    );
    let sessions = store.list_provider_sessions(run_id).unwrap();
    assert!(sessions.iter().any(|session| {
        session.stage_id().as_str() == "architecture" && session.provider_id().as_str() == "claude"
    }));
    assert!(
        !sessions
            .iter()
            .any(|session| { session.stage_id().as_str() == "implementation" })
    );
    drop(store);

    let resumed = fixture.senate(&["resume", &run_id.to_string()]);
    assert!(!resumed.status.success());
    assert!(
        String::from_utf8(resumed.stderr)
            .unwrap()
            .contains("configured provider unavailable for codex target")
    );
}

#[test]
fn completed_codex_can_disappear_before_restarted_claude_decision() {
    let fixture = Fixture::new();
    let blocked = fixture.senate_remove_completed_codex(&[
        "standard",
        "Finish decision without historical provider",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--profile",
        "recommended",
    ]);
    assert_success(&blocked);
    let stdout = String::from_utf8(blocked.stdout).unwrap();
    assert!(stdout.contains("Status     needs_user"));
    assert!(!fixture.fake_bin.join("codex").exists());
    let run_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap();
    let attention_id = stdout
        .lines()
        .skip_while(|line| *line != "Attention")
        .find_map(|line| line.split_once(" · ").map(|(id, _)| id))
        .unwrap();

    let resolved = fixture.senate(&[
        "resolve",
        run_id,
        attention_id,
        "--response",
        "Approve decision",
    ]);
    assert_success(&resolved);
    assert!(
        String::from_utf8(resolved.stdout)
            .unwrap()
            .contains("Status     completed")
    );
    let store = SqliteStore::open(fixture.data.join("senate.db")).unwrap();
    let sessions = store
        .list_provider_sessions(run_id.parse().unwrap())
        .unwrap();
    assert!(sessions.iter().any(|session| {
        session.stage_id().as_str() == "implementation" && session.provider_id().as_str() == "codex"
    }));
    assert!(sessions.iter().any(|session| {
        session.stage_id().as_str() == "decision"
            && session.provider_id().as_str() == "claude"
            && session.invocation() == 2
    }));
}

/// A stage whose provider ran out of quota is retried somewhere else with
/// one flag, and only that stage moves: the run's own routing table, the
/// snapshot it came from and every other stage keep the provider they had.
#[test]
fn retry_with_provider_sends_only_the_failed_stage_to_the_other_provider() {
    let fixture = Fixture::new();
    let failed = fixture.senate_fail_once(&[
        "fast",
        "Reroute the failed stage",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--provider",
        "codex",
    ]);
    assert_success(&failed);
    let failed = String::from_utf8(failed.stdout).unwrap();
    assert!(failed.contains("Status     failed"), "{failed}");
    let run_id = failed
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap();

    let retried = fixture.senate(&["retry", run_id, "implementation", "--provider", "claude"]);
    assert_success(&retried);
    let retried = String::from_utf8(retried.stdout).unwrap();
    assert!(retried.contains("Status     completed"), "{retried}");
    assert!(
        retried.contains("implementation: route overridden"),
        "{retried}"
    );
    assert!(
        retried.contains("implementation (completed) · role=implementer · configured=claude/native default (operator override)"),
        "{retried}"
    );
    assert!(
        retried.contains("implementer  codex"),
        "the routing table still says what the snapshot decided: {retried}"
    );
    assert!(
        !retried.contains("verify (completed) · role=verifier · configured=verify/native default (operator override)"),
        "the un-skipped verify stage keeps its own route: {retried}"
    );

    let run_id: RunId = run_id.parse().unwrap();
    let mut store = SqliteStore::open(fixture.data.join("senate.db")).unwrap();
    let sessions = store.list_provider_sessions(run_id).unwrap();
    let implementation = sessions
        .iter()
        .filter(|session| session.stage_id().as_str() == "implementation")
        .map(|session| (session.attempt(), session.provider_id().as_str()))
        .collect::<Vec<_>>();
    assert_eq!(implementation, vec![(1, "codex"), (2, "claude")]);
    let loaded = store.load_run(run_id).unwrap();
    let stage = loaded
        .run
        .stage(&"implementation".parse().unwrap())
        .unwrap();
    assert_eq!(
        stage
            .route_override()
            .map(|route| route.provider_id().as_str()),
        Some("claude"),
        "the override is stage state and survives reload"
    );
    assert_eq!(loaded.config_snapshot.schema_version(), 2);

    // A provider the machine cannot run is refused before anything changes.
    let mut second = Fixture::new();
    second.remove_claude();
    let failed = second.senate_fail_once(&[
        "fast",
        "Refuse the missing provider",
        "--repo",
        second.repo.to_str().unwrap(),
        "--provider",
        "codex",
    ]);
    assert_success(&failed);
    let failed = String::from_utf8(failed.stdout).unwrap();
    let run_id = failed
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap();
    let refused = second.senate(&["retry", run_id, "implementation", "--provider", "claude"]);
    assert!(!refused.status.success());
    let status = second.senate(&["status", run_id]);
    assert_success(&status);
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(status.contains("Status     failed"), "{status}");
    assert!(!status.contains("operator override"), "{status}");
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
        let repo = temp.path().join("repo");
        let data = temp.path().join("data");
        let capture = temp.path().join("capture");
        let fake_bin = temp.path().join("bin");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&fake_bin).unwrap();
        for tool in ["git", "tmux"] {
            std::os::unix::fs::symlink(find_on_path(tool).unwrap(), fake_bin.join(tool)).unwrap();
        }
        for (name, mode) in [("claude", "claude"), ("codex", "codex")] {
            let wrapper = fake_bin.join(name);
            fs::write(
                &wrapper,
                if name == "codex" {
                    format!(
                        "#!/bin/sh\nmarker=\"$SENATE_FAKE_CODEX_CAPTURE_DIR/remove-after-probe\"\nif [ \"$SENATE_FAKE_CODEX_REMOVE_AFTER_PROBE\" = 1 ] && [ -f \"$marker\" ] && [ \"$*\" = \"--version\" ]; then\n  echo 'configured Codex disappeared' >&2\n  exit 42\nfi\nif [ \"$SENATE_FAKE_CODEX_REMOVE_AFTER_PROBE\" = 1 ] && [ \"$*\" = \"exec resume --help\" ]; then\n  '{}' codex \"$@\"\n  code=$?\n  mkdir -p \"$SENATE_FAKE_CODEX_CAPTURE_DIR\"\n  touch \"$marker\"\n  exit $code\nfi\nexec '{}' codex \"$@\"\n",
                        env!("CARGO_BIN_EXE_senate-test-agent"),
                        env!("CARGO_BIN_EXE_senate-test-agent")
                    )
                } else {
                    format!(
                        "#!/bin/sh\nexec '{}' {mode} \"$@\"\n",
                        env!("CARGO_BIN_EXE_senate-test-agent")
                    )
                },
            )
            .unwrap();
            let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&wrapper, permissions).unwrap();
        }
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("README.md"), "baseline\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-qm", "initial"]);
        Self {
            _temp: temp,
            repo,
            data,
            capture,
            fake_bin,
        }
    }

    fn senate(&self, args: &[&str]) -> Output {
        self.senate_with_env(args, false)
    }

    fn remove_claude(&mut self) {
        fs::remove_file(self.fake_bin.join("claude")).unwrap();
    }

    fn senate_write(&self, args: &[&str]) -> Output {
        let mut command = self.command(args);
        command.env("SENATE_FAKE_CODEX_WRITE", "1");
        command.output().unwrap()
    }

    /// The fake Codex fails each stage's first attempt and succeeds after.
    fn senate_fail_once(&self, args: &[&str]) -> Output {
        let mut command = self.command(args);
        command.env(
            "SENATE_FAKE_CODEX_FAIL_ONCE_DIR",
            self.capture.join("fail-once"),
        );
        command.output().unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_senate"));
        command
            .args(args)
            .env("PATH", &self.fake_bin)
            .env("SENATE_DATA_DIR", &self.data)
            .env("CODEX_HOME", self.data.join("codex-home"))
            .env("SENATE_FAKE_CODEX_CAPTURE_DIR", &self.capture)
            .env("SENATE_FAKE_CLAUDE_CAPTURE_DIR", &self.capture);
        command
    }

    fn senate_with_env(&self, args: &[&str], question: bool) -> Output {
        let mut command = self.command(args);
        if question {
            command.env("SENATE_FAKE_CLAUDE_QUESTION", "1");
        }
        command.output().unwrap()
    }

    fn senate_remove_codex_during_architecture(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_senate"));
        command
            .args(args)
            .env("PATH", &self.fake_bin)
            .env("SENATE_DATA_DIR", &self.data)
            .env("CODEX_HOME", self.data.join("codex-home"))
            .env("SENATE_FAKE_CODEX_CAPTURE_DIR", &self.capture)
            .env("SENATE_FAKE_CLAUDE_CAPTURE_DIR", &self.capture)
            .env(
                "SENATE_FAKE_CLAUDE_REMOVE_CODEX",
                self.fake_bin.join("codex"),
            )
            .output()
            .unwrap()
    }

    fn senate_probe_failure(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_senate"))
            .args(args)
            .env("PATH", &self.fake_bin)
            .env("SENATE_DATA_DIR", &self.data)
            .env("CODEX_HOME", self.data.join("codex-home"))
            .env("SENATE_FAKE_CLAUDE_PROBE_FAILURE", "1")
            .output()
            .unwrap()
    }

    fn senate_remove_completed_codex(&self, args: &[&str]) -> Output {
        let mut command = self.command(args);
        command
            .env("SENATE_FAKE_CLAUDE_QUESTION_STAGE", "decision")
            .env(
                "SENATE_FAKE_CLAUDE_REMOVE_COMPLETED_CODEX",
                self.fake_bin.join("codex"),
            )
            .output()
            .unwrap()
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
