#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::json;
use tempfile::TempDir;

/// The fixture exercises real tmux/process supervision and immutable stdin,
/// without invoking a vendor. Source files change only after explicit apply.
#[test]
fn native_opencode_fixture_runs_through_tmux_preserves_source_then_applies() {
    let fixture = Fixture::new();
    let marker = "SUPER_SECRET_TASK_MARKER";
    let started = fixture
        .start(marker)
        .env("SENATE_FAKE_OPENCODE_WRITE", "1")
        .output()
        .unwrap();
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    assert!(stdout.contains("Status     completed"), "{stdout}");
    assert!(stdout.contains("implementer  opencode"), "{stdout}");
    assert!(stdout.contains("native=ses_implementation"), "{stdout}");
    assert!(
        stdout.contains("Usage      opencode 11 input units"),
        "{stdout}"
    );
    let run_id = run_id(&stdout);
    assert!(!fixture.repo.join("hello.txt").exists());
    let argv = fs::read_to_string(fixture.capture.join("implementation.argv")).unwrap();
    let stdin = fs::read_to_string(fixture.capture.join("implementation.stdin")).unwrap();
    assert!(
        !argv.contains(marker),
        "the prompt must not be exposed in argv"
    );
    assert!(stdin.contains(marker), "{stdin}");

    let applied = fixture.command(&["apply", run_id]).output().unwrap();
    assert_success(&applied);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("hello.txt")).unwrap(),
        "created by fake opencode\n"
    );
}

#[test]
fn opencode_without_an_explicit_model_fails_fast_before_any_state_exists() {
    let fixture = Fixture::new();
    let started = fixture
        .command(&["fast", "task", "--provider", "opencode"])
        .output()
        .unwrap();
    assert!(!started.status.success());
    assert!(
        String::from_utf8_lossy(&started.stderr).contains("requires an explicit model"),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert!(!fixture.capture.join("implementation.argv").exists());
    assert!(!fixture.data.join("runs").exists());
}

#[test]
fn unavailable_native_models_refuse_to_start() {
    let fixture = Fixture::new();
    let started = fixture
        .start("task")
        .env("SENATE_FAKE_OPENCODE_UNAUTHENTICATED", "1")
        .output()
        .unwrap();
    assert!(!started.status.success());
    assert!(
        String::from_utf8_lossy(&started.stderr).contains("no available models"),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert!(!fixture.capture.join("implementation.argv").exists());
}

/// Native environment/local providers can have zero stored credentials.
/// Ten and twenty stored credentials must not be mistaken for zero either.
#[test]
fn native_model_availability_is_not_inferred_from_credential_counts() {
    for count in ["0", "1", "10", "20"] {
        let fixture = Fixture::new();
        let started = fixture
            .start("task")
            .env("SENATE_FAKE_OPENCODE_CREDENTIAL_COUNT", count)
            .output()
            .unwrap();
        assert_success(&started);
        let stdout = String::from_utf8_lossy(&started.stdout);
        assert!(stdout.contains("Status     completed"), "{count}: {stdout}");
    }
}

#[test]
fn an_unlisted_model_refuses_to_start_before_launching() {
    let fixture = Fixture::new();
    let started = fixture
        .command(&[
            "fast",
            "task",
            "--provider",
            "opencode",
            "--model",
            "nobody/nothing",
        ])
        .output()
        .unwrap();
    assert!(!started.status.success());
    assert!(String::from_utf8_lossy(&started.stderr).contains("nobody/nothing"));
    assert!(!fixture.capture.join("implementation.argv").exists());
}

/// The CLI double refuses every probe unless it is pure, read-only, receives
/// empty stdin, and executes outside the source checkout. Ambient override
/// directories/content must not reintroduce a different configuration.
#[test]
fn discovery_and_execution_share_the_same_configuration_defenses() {
    let fixture = Fixture::new();
    fs::write(
        fixture.repo.join("opencode.json"),
        r#"{"permission":{"edit":"allow","bash":"allow"}}"#,
    )
    .unwrap();
    let started = fixture
        .start("task")
        .env("OPENCODE_CONFIG_DIR", &fixture.repo)
        .env("OPENCODE_CONFIG_CONTENT", r#"{"permission":"allow"}"#)
        .output()
        .unwrap();
    assert_success(&started);
    assert!(String::from_utf8_lossy(&started.stdout).contains("Status     completed"));
}

#[test]
fn primary_subagent_and_legacy_mode_overrides_are_refused_before_launch() {
    for config in [
        json!({"agent":{"build":{"permission":{"edit":"allow","bash":"allow"}}}}),
        json!({"agent":{"general":{"permission":{"bash":"allow"}}}}),
        json!({"agent":{"custom":{"permission":"allow"}}}),
        json!({"mode":{"custom":{"tools":{"write":true}}}}),
    ] {
        let fixture = Fixture::new();
        let started = fixture
            .start("task")
            .env("SENATE_FAKE_OPENCODE_CONFIG", config.to_string())
            .output()
            .unwrap();
        assert!(
            !started.status.success(),
            "unsafe configuration was accepted"
        );
        assert!(
            String::from_utf8_lossy(&started.stderr).contains("permission preflight"),
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
        assert!(!fixture.capture.join("implementation.argv").exists());
        assert!(!fixture.data.join("runs").exists());
    }
}

#[test]
fn a_permission_halt_resolves_and_resumes_with_the_exact_command_allowed() {
    let fixture = Fixture::new();
    let stdout = fixture.permission_halt();
    let run_id = run_id(&stdout);
    let attention_id = attention_id(&stdout);
    let resolved = fixture
        .command(&["resolve", run_id, &attention_id])
        .output()
        .unwrap();
    assert_success(&resolved);
    let resolved_stdout = String::from_utf8(resolved.stdout).unwrap();
    assert!(
        resolved_stdout.contains("Status     completed"),
        "{resolved_stdout}"
    );
    let resumed_argv = fs::read_to_string(fixture.capture.join("resumed.argv")).unwrap();
    assert!(resumed_argv.contains("--session"));
    assert!(resumed_argv.contains("ses_implementation"));
    assert!(!resumed_argv.contains("The operator approved"));
    let resumed_stdin = fs::read_to_string(fixture.capture.join("resumed.stdin")).unwrap();
    assert!(resumed_stdin.contains("The operator approved"));
    let provider_output = fixture
        .data
        .join("runs")
        .join(run_id)
        .join("provider-output")
        .join("opencode");
    let session_dir = fs::read_dir(provider_output)
        .unwrap()
        .find_map(Result::ok)
        .unwrap()
        .path();
    let config: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(session_dir.join("invocation-2.config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        config["permission"]["bash"]["python3 -c \"from calc import add; print(add(2,3))\""],
        json!("allow")
    );
}

#[test]
fn permission_configuration_is_checked_again_before_resume() {
    let fixture = Fixture::new();
    let stdout = fixture.permission_halt();
    let attention = attention_id(&stdout);
    let resolved = fixture
        .command(&["resolve", run_id(&stdout), &attention])
        .env(
            "SENATE_FAKE_OPENCODE_CONFIG",
            r#"{"agent":{"general":{"permission":{"edit":"allow"}}}}"#,
        )
        .output()
        .unwrap();
    assert!(!resolved.status.success());
    assert!(String::from_utf8_lossy(&resolved.stderr).contains("permission preflight"));
    assert!(!fixture.capture.join("resumed.argv").exists());
}

#[test]
fn a_vendor_error_fails_the_stage() {
    let fixture = Fixture::new();
    let started = fixture
        .start("task")
        .env("SENATE_FAKE_OPENCODE_ERROR", "1")
        .output()
        .unwrap();
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    assert!(stdout.contains("Status     failed"), "{stdout}");
}

fn run_id(stdout: &str) -> &str {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap()
}

fn attention_id(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.split_once(" · ")?;
            rest.contains("permission").then(|| id.to_owned())
        })
        .expect("a pending permission attention line")
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
        let wrapper = fake_bin.join("opencode");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec '{}' opencode \"$@\"\n",
                env!("CARGO_BIN_EXE_senate-test-agent")
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _temp: temp,
            repo,
            data,
            capture,
            fake_bin,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_senate"));
        command
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", &self.fake_bin)
            .env("SENATE_DATA_DIR", &self.data)
            .env("SENATE_FAKE_OPENCODE_CAPTURE_DIR", &self.capture)
            .env("SENATE_FAKE_OPENCODE_SOURCE", &self.repo);
        command
    }

    fn start(&self, task: &str) -> Command {
        self.command(&[
            "fast",
            task,
            "--provider",
            "opencode",
            "--model",
            "opencode-go/deepseek-v4-pro",
        ])
    }

    fn permission_halt(&self) -> String {
        let started = self
            .start("task")
            .env("SENATE_FAKE_OPENCODE_PERMISSION_HALT", "1")
            .output()
            .unwrap();
        assert_success(&started);
        let stdout = String::from_utf8(started.stdout).unwrap();
        assert!(stdout.contains("Status     needs_user"), "{stdout}");
        assert!(stdout.contains("python3 -c"), "{stdout}");
        stdout
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| candidate.canonicalize().ok())
}

fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert_success(&output);
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
