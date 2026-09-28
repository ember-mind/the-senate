use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// Opt-in smoke test against installed/authenticated native Claude Code.
///
/// Run explicitly with:
/// `SENATE_REAL_CLAUDE=1 cargo test --test claude_real -- --ignored --nocapture`
#[test]
#[ignore = "requires installed/authenticated Claude Code and consumes native provider usage"]
fn native_claude_completes_disposable_fast_run() {
    if std::env::var("SENATE_REAL_CLAUDE").as_deref() != Ok("1") {
        eprintln!("SENATE_REAL_CLAUDE=1 not set; skipping opt-in native test");
        return;
    }
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repository");
    let data = temp.path().join("data");
    init_repository(&repository);
    let initial = senate(
        &data,
        &[
            "fast",
            "Append one line containing `M7 native smoke test` to README.md. Make no other change.",
            "--repo",
            repository.to_str().unwrap(),
            "--provider",
            "claude",
        ],
    );
    assert_success(&initial);
    let mut output = String::from_utf8(initial.stdout).unwrap();
    let run_id = output
        .lines()
        .find_map(|line| line.strip_prefix("Run        "))
        .unwrap()
        .to_owned();
    for _ in 0..8 {
        if output.contains("Status     completed") {
            assert_eq!(git_output(&repository, &["status", "--porcelain"]), "");
            return;
        }
        let attention = output
            .lines()
            .find_map(|line| line.split_once(" · ").map(|(id, _)| id))
            .expect("native run should expose attention or complete");
        let resolved = senate(&data, &["resolve", &run_id, attention]);
        assert_success(&resolved);
        output = String::from_utf8(resolved.stdout).unwrap();
    }
    panic!("native Claude run did not complete after bounded attention resolutions\n{output}");
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
    String::from_utf8(output.stdout).unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
