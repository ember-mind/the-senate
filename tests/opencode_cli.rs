#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// End-to-end: a PATH-injected fake `opencode` drives a real run through
/// tmux, the permission config travels as `OPENCODE_CONFIG`, the prompt
/// travels as argv (opencode has no stdin-based prompt), and the completed
/// run applies cleanly.
#[test]
fn native_opencode_fixture_runs_through_tmux_preserves_source_then_applies() {
    let fixture = Fixture::new();
    let marker = "SUPER_SECRET_TASK_MARKER";
    let started = fixture.senate(
        &[
            "fast",
            marker,
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--provider",
            "opencode",
            "--model",
            "opencode-go/deepseek-v4-pro",
        ],
        true,
        false,
    );
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    assert!(stdout.contains("Status     completed"), "{stdout}");
    assert!(stdout.contains("implementer  opencode"), "{stdout}");
    assert!(stdout.contains("native=ses_implementation"), "{stdout}");
    // opencode reports input net of cache reads, the same convention Claude
    // uses and the opposite of Codex's cache-inclusive input_tokens.
    assert!(
        stdout.contains("Usage      opencode 11 input units"),
        "{stdout}"
    );
    let run_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap();

    assert!(!fixture.repo.join("hello.txt").exists());
    // Unlike Claude/Codex, opencode has no stdin-based prompt (verified by
    // hand against the real CLI's `run --help`): the task text necessarily
    // travels as argv, visible to `ps` on this machine for the invocation's
    // lifetime. This is a documented, deliberate difference from every other
    // adapter, not an oversight.
    let argv = fs::read_to_string(fixture.capture.join("implementation.argv")).unwrap();
    assert!(argv.contains(marker), "{argv}");

    let applied = fixture.senate(&["apply", run_id], false, false);
    assert_success(&applied);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("hello.txt")).unwrap(),
        "created by fake opencode\n"
    );
}

/// opencode's model carries the vendor, so `--provider opencode` without
/// `--model` sends only `--dir`/`--format json` — no `-m` flag at all — and
/// the run still completes on whatever opencode's own CLI would default to.
#[test]
fn opencode_without_an_explicit_model_omits_the_m_flag() {
    let fixture = Fixture::new();
    let started = fixture.senate(
        &[
            "fast",
            "task",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--provider",
            "opencode",
        ],
        false,
        false,
    );
    assert_success(&started);
    let argv = fs::read_to_string(fixture.capture.join("implementation.argv")).unwrap();
    assert!(!argv.lines().any(|line| line == "-m"), "{argv}");
}

/// An unauthenticated opencode installation refuses the run before any state
/// is created, the same bar `--provider claude|codex` already holds explicit
/// selection to.
#[test]
fn unauthenticated_opencode_refuses_to_start() {
    let fixture = Fixture::new();
    let started = fixture.senate(
        &[
            "fast",
            "task",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--provider",
            "opencode",
        ],
        false,
        true,
    );
    assert!(
        !started.status.success(),
        "unauthenticated opencode must refuse: {}",
        String::from_utf8_lossy(&started.stdout)
    );
    assert!(
        String::from_utf8_lossy(&started.stderr).contains("not authenticated")
            || String::from_utf8_lossy(&started.stderr).contains("credentials"),
        "stderr: {}",
        String::from_utf8_lossy(&started.stderr)
    );
}

/// A model `opencode models` does not list fails fast and clearly, before
/// any process launches — instead of reaching the vendor's own vague
/// `UnknownError` ("Unexpected server error").
#[test]
fn an_unlisted_model_refuses_to_start_before_launching() {
    let fixture = Fixture::new();
    let started = fixture.senate(
        &[
            "fast",
            "task",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--provider",
            "opencode",
            "--model",
            "nobody/nothing",
        ],
        false,
        false,
    );
    assert!(
        !started.status.success(),
        "an unlisted model must be refused before launch: {}",
        String::from_utf8_lossy(&started.stdout)
    );
    assert!(
        String::from_utf8_lossy(&started.stderr).contains("nobody/nothing"),
        "stderr: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert!(
        !fixture.capture.join("implementation.argv").exists(),
        "an unlisted model must never reach a launched process"
    );
}

/// Headless opencode auto-rejects a permission it cannot ask about. The
/// denial becomes visible progress in `status`, and the run still completes:
/// no typed continuable permission request is fabricated for a protocol that
/// offers no safe way to resume with broadened scope.
#[test]
fn a_denied_bash_permission_is_visible_and_the_run_still_completes() {
    let fixture = Fixture::new();
    let started = Command::new(env!("CARGO_BIN_EXE_senate"))
        .args([
            "fast",
            "task",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--provider",
            "opencode",
        ])
        .env("PATH", &fixture.fake_bin)
        .env("SENATE_DATA_DIR", &fixture.data)
        .env("SENATE_FAKE_OPENCODE_CAPTURE_DIR", &fixture.capture)
        .env("SENATE_FAKE_OPENCODE_DENY_BASH", "1")
        .output()
        .unwrap();
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    assert!(stdout.contains("Status     completed"), "{stdout}");
    // The denial is committed semantic history (`ProviderProgress`), printed
    // as one of the run's own committed events, not a status-line field.
    assert!(
        stdout.to_lowercase().contains("denied permission"),
        "the denial must be visible: {stdout}"
    );
}

/// A vendor failure (insufficient balance, an unresolvable model server-side,
/// or any other terminal `error` record) fails the stage with a scrubbed
/// message, never the raw vendor response.
#[test]
fn a_vendor_error_fails_the_stage() {
    let fixture = Fixture::new();
    let started = Command::new(env!("CARGO_BIN_EXE_senate"))
        .args([
            "fast",
            "task",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--provider",
            "opencode",
        ])
        .env("PATH", &fixture.fake_bin)
        .env("SENATE_DATA_DIR", &fixture.data)
        .env("SENATE_FAKE_OPENCODE_CAPTURE_DIR", &fixture.capture)
        .env("SENATE_FAKE_OPENCODE_ERROR", "1")
        .output()
        .unwrap();
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    assert!(stdout.contains("Status     failed"), "{stdout}");
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

    fn senate(&self, args: &[&str], write: bool, unauthenticated: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_senate"));
        command
            .args(args)
            .env("PATH", &self.fake_bin)
            .env("SENATE_DATA_DIR", &self.data)
            .env("SENATE_FAKE_OPENCODE_CAPTURE_DIR", &self.capture);
        if write {
            command.env("SENATE_FAKE_OPENCODE_WRITE", "1");
        }
        if unauthenticated {
            command.env("SENATE_FAKE_OPENCODE_UNAUTHENTICATED", "1");
        }
        command.output().unwrap()
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
