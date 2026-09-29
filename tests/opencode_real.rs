use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// Opt-in smoke test against an installed/authenticated native opencode CLI.
///
/// Consumes real provider usage against whatever vendor the chosen model
/// belongs to. Run explicitly with:
/// `SENATE_REAL_OPENCODE=1 cargo test --test opencode_real -- --ignored --nocapture`
#[test]
#[ignore = "requires installed/authenticated opencode CLI and consumes native provider usage"]
fn native_opencode_completes_disposable_fast_run() {
    if std::env::var("SENATE_REAL_OPENCODE").as_deref() != Ok("1") {
        eprintln!("SENATE_REAL_OPENCODE=1 not set; skipping opt-in native test");
        return;
    }
    let auth = Command::new("opencode").args(["auth", "list"]).output();
    match auth {
        Ok(output) if output.status.success() => {}
        Ok(output) => panic!(
            "native opencode auth unavailable: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => panic!("native opencode executable unavailable: {error}"),
    }

    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repository");
    let data = temp.path().join("data");
    init_repository(&repository);
    // A model the operator's own installation is expected to have credentials
    // for; there is no cheap universal default across opencode's dozens of
    // vendors, so this is deliberately the one piece of this test a real run
    // must be prepared to adjust.
    let model = std::env::var("SENATE_REAL_OPENCODE_MODEL")
        .unwrap_or_else(|_| "opencode-go/deepseek-v4-flash".to_owned());
    let output = senate(
        &data,
        &[
            "fast",
            "Create hello.txt containing exactly `M-opencode native smoke test` and a newline. Make no other change.",
            "--repo",
            repository.to_str().unwrap(),
            "--provider",
            "opencode",
            "--model",
            &model,
        ],
    );
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Status     completed"), "{stdout}");
    assert!(stdout.contains("implementer  opencode"), "{stdout}");
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
