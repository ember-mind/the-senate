use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// Opt-in smoke test against installed/authenticated native Codex CLI.
///
/// Run explicitly with:
/// `SENATE_REAL_CODEX=1 cargo test --test codex_real -- --ignored --nocapture`
#[test]
#[ignore = "requires installed/authenticated Codex CLI and consumes native provider usage"]
fn native_codex_completes_disposable_fast_run() {
    if std::env::var("SENATE_REAL_CODEX").as_deref() != Ok("1") {
        eprintln!("SENATE_REAL_CODEX=1 not set; skipping opt-in native test");
        return;
    }
    let status = Command::new("codex").args(["login", "status"]).output();
    match status {
        Ok(output) if output.status.success() => {}
        Ok(output) => panic!(
            "native Codex auth unavailable: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => panic!("native Codex executable unavailable: {error}"),
    }

    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repository");
    let data = temp.path().join("data");
    init_repository(&repository);
    let output = senate(
        &data,
        &[
            "fast",
            "Create hello.txt containing exactly `M9 native Codex smoke test` and a newline. Make no other change.",
            "--repo",
            repository.to_str().unwrap(),
            "--provider",
            "codex",
        ],
    );
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Status     completed"));
    assert!(stdout.contains("implementer  codex"));
    assert!(!repository.join("hello.txt").exists());
    assert_eq!(git_output(&repository, &["status", "--porcelain"]), "");
}

fn init_repository(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q"]);
    git(path, &["config", "user.email", "senate@example.invalid"]);
    git(path, &["config", "user.name", "The Senate Test"]);
    std::fs::write(path.join("README.md"), "# Fixture\n").unwrap();
    git(path, &["add", "README.md"]);
    git(path, &["commit", "-qm", "fixture"]);
}

fn senate(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_senate"))
        .args(args)
        .env("SENATE_DATA_DIR", data)
        .env("CODEX_HOME", data.join("codex-home"))
        .output()
        .unwrap()
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
