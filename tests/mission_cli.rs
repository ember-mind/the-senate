//! `senate mission ...` end to end: every command is a fresh process, so
//! whatever the next one sees came back from the database.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

#[test]
#[allow(clippy::too_many_lines, reason = "one mission, start to finish")]
fn a_mission_is_planned_started_integrated_and_survives_every_restart() {
    let fixture = Fixture::new();

    let created = fixture.senate(&[
        "mission",
        "new",
        "JEV little world",
        "--goal",
        "characters keep living while nobody watches",
        "--repo",
        fixture.repo.to_str().unwrap(),
    ]);
    assert_success(&created);
    let stdout = String::from_utf8_lossy(&created.stdout);
    let mission_id = stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Mission "))
        .and_then(|rest| rest.split(':').next())
        .expect("first line names the mission")
        .to_owned();
    assert!(stdout.contains("Status: planning"), "{stdout}");
    assert!(stdout.contains("Nothing needs you."), "{stdout}");

    assert_success(&fixture.senate(&[
        "mission",
        "add",
        &mission_id,
        "persistence",
        "--title",
        "Persistence",
        "--goal",
        "persist every character between sessions",
        "--accept",
        "a character survives a restart",
    ]));
    let added = fixture.senate(&[
        "mission",
        "add",
        &mission_id,
        "memory",
        "--title",
        "Memory",
        "--goal",
        "characters remember what happened",
        "--depends-on",
        "persistence",
        "--workflow",
        "fast",
    ]);
    assert_success(&added);
    let stdout = String::from_utf8_lossy(&added.stdout);
    assert!(stdout.contains("ready      persistence"), "{stdout}");
    assert!(stdout.contains("planned    memory"), "{stdout}");
    assert!(stdout.contains("depends on: persistence"), "{stdout}");

    // A package whose dependency is not integrated cannot start.
    let refused = fixture.senate(&[
        "mission",
        "start",
        &mission_id,
        "memory",
        "--provider",
        "fake",
    ]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("needs a ready package"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let started = fixture.senate(&[
        "mission",
        "start",
        &mission_id,
        "persistence",
        "--provider",
        "fake",
    ]);
    assert_success(&started);
    let stdout = String::from_utf8_lossy(&started.stdout);
    assert!(stdout.contains("delivered  persistence"), "{stdout}");
    assert!(stdout.contains("Status: active"), "{stdout}");
    assert!(stdout.contains("brings it in"), "{stdout}");
    let run_id = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("run: "))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the delivered package names its run")
        .to_owned();

    // The run's immutable input is the handoff, rendered from the mission.
    let status = fixture.senate(&["status", &run_id]);
    assert_success(&status);
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("Work package: Persistence (persistence)"),
        "{stdout}"
    );

    // A run a mission remembers cannot be deleted.
    assert_success(&fixture.senate(&["archive", &run_id]));
    let refused = fixture.senate(&["delete", &run_id, "--yes"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("belongs to mission"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    // Delivery carries evidence, not a claim: the fake changed nothing and
    // Fast's verify stage really ran.
    let shown = fixture.senate(&["mission", "show", &mission_id]);
    assert_success(&shown);
    let stdout = String::from_utf8_lossy(&shown.stdout);
    assert!(
        stdout.contains("delivered: 0 file(s) changed; verify completed; no reviews"),
        "{stdout}"
    );

    // Nothing is waiting to be driven; resume is a no-op that reports so.
    let resumed = fixture.senate(&["mission", "resume", &mission_id]);
    assert_success(&resumed);
    assert!(
        String::from_utf8_lossy(&resumed.stdout).contains("Resumed 0 run(s)."),
        "{}",
        String::from_utf8_lossy(&resumed.stdout)
    );

    // A fast run has no decision stage, so a fix cycle is refused by the
    // run itself and the package stays delivered.
    let refused = fixture.senate(&["mission", "fix", &mission_id, "persistence"]);
    assert_eq!(refused.status.code(), Some(1));
    let shown = fixture.senate(&["mission", "show", &mission_id]);
    assert!(
        String::from_utf8_lossy(&shown.stdout).contains("delivered  persistence"),
        "{}",
        String::from_utf8_lossy(&shown.stdout)
    );

    let integrated = fixture.senate(&["mission", "integrate", &mission_id, "persistence"]);
    assert_success(&integrated);
    let stdout = String::from_utf8_lossy(&integrated.stdout);
    assert!(stdout.contains("integrated persistence"), "{stdout}");
    assert!(stdout.contains("ready      memory"), "{stdout}");
    assert!(stdout.contains("Nothing needs you."), "{stdout}");

    let decided = fixture.senate(&[
        "mission",
        "decide",
        &mission_id,
        "Persist before memory",
        "--why",
        "memory needs a durable substrate",
        "--by",
        "lead",
    ]);
    assert_success(&decided);
    let stdout = String::from_utf8_lossy(&decided.stdout);
    assert!(
        stdout.contains("- Persist before memory (lead): memory needs a durable substrate"),
        "{stdout}"
    );

    let listed = fixture.senate(&["mission", "list"]);
    assert_success(&listed);
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        stdout.contains(&format!(
            "{mission_id}  active  1/2 integrated  0 active  0 need you  JEV little world"
        )),
        "{stdout}"
    );

    let shown = fixture.senate(&["mission", "show", &mission_id]);
    assert_success(&shown);
    let stdout = String::from_utf8_lossy(&shown.stdout);
    assert!(
        stdout.contains("Packages (1/2 integrated, 0 active)"),
        "{stdout}"
    );
    assert!(stdout.contains("Decisions"), "{stdout}");
}

struct Fixture {
    _temp: TempDir,
    repo: PathBuf,
    data: PathBuf,
}

#[test]
fn mission_drive_reuses_policy_and_waits_for_explicit_integration_across_processes() {
    let fixture = Fixture::new();
    let created = fixture.senate(&[
        "mission",
        "new",
        "Driven",
        "--goal",
        "Two packages",
        "--repo",
        fixture.repo.to_str().unwrap(),
    ]);
    assert_success(&created);
    let mission_id = String::from_utf8_lossy(&created.stdout)
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Mission ")
        .unwrap()
        .split(':')
        .next()
        .unwrap()
        .to_owned();
    for id in ["first", "second"] {
        assert_success(&fixture.senate(&[
            "mission",
            "add",
            &mission_id,
            id,
            "--title",
            id,
            "--goal",
            "Deliver package",
            "--workflow",
            "fast",
        ]));
    }
    let invalid = fixture.senate(&[
        "mission",
        "drive",
        &mission_id,
        "--max-parallel",
        "0",
        "--provider",
        "fake",
        "--once",
    ]);
    assert_eq!(invalid.status.code(), Some(1));
    let driven = fixture.senate(&[
        "mission",
        "drive",
        &mission_id,
        "--max-parallel",
        "1",
        "--provider",
        "fake",
        "--once",
    ]);
    assert_success(&driven);
    let text = String::from_utf8_lossy(&driven.stdout);
    assert!(text.contains("delivered  first"), "{text}");
    assert!(text.contains("ready      second"), "{text}");
    assert!(text.contains("awaiting explicit integration"), "{text}");
    let paused = fixture.senate(&["mission", "drive", &mission_id]);
    assert_success(&paused);
    assert!(String::from_utf8_lossy(&paused.stdout).contains("ready      second"));
    assert_success(&fixture.senate(&["mission", "integrate", &mission_id, "first"]));
    let resumed = fixture.senate(&["mission", "drive", &mission_id]);
    assert_success(&resumed);
    let text = String::from_utf8_lossy(&resumed.stdout);
    assert!(text.contains("maximum 1 concurrent runs"), "{text}");
    assert!(text.contains("delivered  second"), "{text}");
    assert!(text.contains("integrated first"), "{text}");
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
        Self {
            _temp: temp,
            repo,
            data,
        }
    }

    fn senate(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_senate"))
            .args(args)
            .current_dir(&self.repo)
            .env("SENATE_DATA_DIR", &self.data)
            .env("CODEX_HOME", self.data.join("codex-home"))
            .output()
            .unwrap()
    }
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
    let status = Command::new("git")
        .args(args)
        .current_dir(path)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}
