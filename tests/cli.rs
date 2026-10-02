use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

#[cfg(unix)]
#[test]
fn official_registration_requires_a_successful_bounded_version_probe() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    let candidate = fixture.repo.join("candidate");
    for (body, error) in [
        (
            format!(
                "#!/bin/sh\necho senate {}\nexit 42\n",
                env!("CARGO_PKG_VERSION")
            ),
            "Code(42)",
        ),
        (
            format!(
                "#!/bin/sh\nhead -c 70000 /dev/zero\necho senate {}\n",
                env!("CARGO_PKG_VERSION")
            ),
            "64 KiB",
        ),
    ] {
        fs::write(&candidate, body).unwrap();
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_senate"))
            .args(["__register-official-install", "candidate"])
            .current_dir(&fixture.repo)
            .env("SENATE_DATA_DIR", &fixture.data)
            .env("SENATE_DISABLE_UPDATE_CHECK", "1")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(error),
            "{output:?}"
        );
        assert!(!fixture.data.join("install.json").exists());
    }
    fs::write(
        &candidate,
        format!("#!/bin/sh\necho senate {}\n", env!("CARGO_PKG_VERSION")),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_senate"))
        .args(["__register-official-install", "candidate"])
        .current_dir(&fixture.repo)
        .env("SENATE_DATA_DIR", &fixture.data)
        .env("SENATE_DISABLE_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert_success(&output);
    assert!(fixture.data.join("install.json").exists());
}

#[test]
fn deep_command_survives_process_restart_and_status_is_read_only() {
    let fixture = Fixture::new();
    let started = fixture.senate(&[
        "deep",
        "CLI integration task",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--provider",
        "fake",
    ]);
    assert_success(&started);
    let stdout = String::from_utf8(started.stdout).unwrap();
    assert!(stdout.contains("Workflow   deep"));
    assert!(stdout.contains("Status     completed"));
    let run_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .expect("run ID in output");

    let listed = fixture.senate(&["runs"]);
    assert_success(&listed);
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert!(listed.contains(run_id));
    assert!(listed.contains("CLI integration task"));
    assert!(listed.contains(fixture.repo.to_str().unwrap()));

    let status_before = fixture.senate(&["status", run_id]);
    assert_success(&status_before);
    let status_before = String::from_utf8(status_before.stdout).unwrap();
    // Usage is attributed to the runtime that reported it, never merged into
    // one run-wide figure. `fake` declares no input-accounting convention, so
    // its numbers are reported raw with nothing derived from them.
    assert!(
        status_before.contains("Usage      fake     70 input units · 35 output units"),
        "{status_before}"
    );
    assert!(status_before.contains("quality_review"));
    assert!(status_before.contains("spec_review"));

    let resumed = fixture.senate(&["resume", run_id]);
    assert_success(&resumed);
    let resumed = String::from_utf8(resumed.stdout).unwrap();
    assert!(!resumed.contains("provider started"));
    assert!(resumed.contains("Status     completed"));

    let status_after = fixture.senate(&["status", run_id]);
    assert_success(&status_after);
    assert_eq!(
        status_before,
        String::from_utf8(status_after.stdout).unwrap()
    );
    assert!(git_output(&fixture.repo, &["status", "--porcelain"]).is_empty());
}

#[test]
fn the_default_profile_never_falls_back_to_the_development_provider() {
    // Neither native CLI is on this fixture's PATH. Omitting the selection
    // flags therefore has to fail, and fail saying what is missing: the
    // development provider produces artifacts that look like work, so a
    // default that quietly reached for it would be worse than no default.
    let fixture = Fixture::new();
    let output = fixture.senate_without_native_providers(&[
        "fast",
        "task",
        "--repo",
        fixture.repo.to_str().unwrap(),
    ]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("recommended profile requires authenticated Claude Code or Codex CLI"),
        "{stderr}"
    );
    assert!(!stderr.contains("fake"), "{stderr}");
    assert!(!fixture.data.join("senate.db").exists());
}

#[test]
fn current_directory_is_default_repository_and_empty_runs_is_side_effect_free() {
    let fixture = Fixture::new();
    let empty = fixture.senate(&["runs"]);
    assert_success(&empty);
    assert_eq!(String::from_utf8(empty.stdout).unwrap(), "No runs.\n");
    assert!(!fixture.data.join("senate.db").exists());

    let output = Command::new(env!("CARGO_BIN_EXE_senate"))
        .args(["review", "Review from cwd", "--provider", "fake"])
        .current_dir(&fixture.repo)
        .env("SENATE_DATA_DIR", &fixture.data)
        .env("CODEX_HOME", fixture.data.join("codex-home"))
        .output()
        .unwrap();
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Workflow   review"));
    assert!(stdout.contains("Status     completed"));
    assert!(stdout.contains(fixture.repo.to_str().unwrap()));
}

#[test]
fn doctor_reports_runtime_prerequisites_without_creating_database() {
    let fixture = Fixture::new();
    let output = fixture.doctor_without_native_providers();
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    // Operational readiness, not development history.
    assert!(
        !stdout.contains("Milestone"),
        "doctor describes the product, not the milestone it was built in"
    );
    assert!(stdout.contains("version: "));
    assert!(stdout.contains("install source: "));
    assert!(stdout.contains("Claude Code:"));
    assert!(stdout.contains("Codex CLI: not found on PATH"));
    // Git is a runtime prerequisite, so it is diagnosed like tmux is.
    assert!(stdout.contains("Git: available ("), "{stdout}");
    assert!(stdout.contains("tmux: available (tmux "));
    // A missing native provider is diagnostic, never fatal.
    assert!(!fixture.data.join("senate.db").exists());
}

#[test]
fn no_args_non_tty_prints_help_and_explicit_tui_fails_without_control_sequences() {
    let fixture = Fixture::new();
    let no_args = fixture.senate(&[]);
    assert_success(&no_args);
    let stdout = String::from_utf8(no_args.stdout).unwrap();
    assert!(stdout.contains("Usage: senate"));
    assert!(stdout.contains("Legacy command name: `senate`"));
    assert!(!stdout.contains('\u{1b}'));

    let explicit = fixture.senate(&["tui"]);
    assert!(!explicit.status.success());
    let stderr = String::from_utf8(explicit.stderr).unwrap();
    assert!(stderr.contains("requires interactive stdin and stdout"));
    assert!(!stderr.contains('\u{1b}'));
}

#[test]
fn senate_is_primary_binary_and_senate_help_identifies_legacy_alias() {
    let primary = Command::new(env!("CARGO_BIN_EXE_senate"))
        .arg("--help")
        .output()
        .unwrap();
    let legacy = Command::new(env!("CARGO_BIN_EXE_senate"))
        .arg("--help")
        .output()
        .unwrap();
    assert_success(&primary);
    assert_success(&legacy);
    let primary_help = String::from_utf8(primary.stdout).unwrap();
    let legacy_help = String::from_utf8(legacy.stdout).unwrap();
    assert!(primary_help.contains("Usage: senate"));
    assert!(legacy_help.contains("Usage: senate"));
    assert!(primary_help.contains("Legacy command name: `senate`"));
    assert!(legacy_help.contains("Legacy command name: `senate`"));
}

struct Fixture {
    temp: TempDir,
    repo: std::path::PathBuf,
    data: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        let data = temp.path().join("data");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("README.md"), "baseline\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-qm", "initial"]);
        Self { temp, repo, data }
    }

    fn senate(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_senate"))
            .args(args)
            .env("SENATE_DATA_DIR", &self.data)
            .env("CODEX_HOME", self.data.join("codex-home"))
            .output()
            .unwrap()
    }

    /// A PATH carrying only the runtime prerequisites, with both native
    /// provider CLIs deliberately absent.
    ///
    /// Tests that turn on a provider being unavailable must say so themselves
    /// rather than inheriting whatever the machine running them happens to
    /// have installed and authenticated.
    fn bin_without_native_providers(&self, name: &str) -> PathBuf {
        let bin = self.temp.path().join(name);
        fs::create_dir_all(&bin).unwrap();
        for tool in ["tmux", "git"] {
            let found =
                find_on_path(tool).unwrap_or_else(|| panic!("{tool} is required for CLI tests"));
            #[cfg(unix)]
            std::os::unix::fs::symlink(found, bin.join(tool)).unwrap();
        }
        bin
    }

    fn senate_without_native_providers(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_senate"))
            .args(args)
            .env("PATH", self.bin_without_native_providers("bare-bin"))
            .env("SENATE_DATA_DIR", &self.data)
            .env("CODEX_HOME", self.data.join("codex-home"))
            .output()
            .unwrap()
    }

    fn doctor_without_native_providers(&self) -> Output {
        // The native provider CLIs being absent must stay diagnostic rather
        // than fatal.
        Command::new(env!("CARGO_BIN_EXE_senate"))
            .arg("doctor")
            .env("PATH", self.bin_without_native_providers("doctor-bin"))
            .env("SENATE_DATA_DIR", &self.data)
            .env("CODEX_HOME", self.data.join("codex-home"))
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
