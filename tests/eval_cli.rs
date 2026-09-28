#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use senate::app::RoutingPlan;
use senate::domain::{DomainEventKind, Role, StageId};
use senate::eval::{EvalProvider, EvalStatus, EvalTarget, load_results, render_report};
use senate::store::SqliteStore;
use tempfile::{TempDir, tempdir};

#[test]
fn eval_list_exposes_immutable_v1_v2_and_calibrated_v3() {
    let fixture = Fixture::new();
    let listed = fixture.senate(&["eval", "list"]);
    assert_success(&listed);
    let output = String::from_utf8(listed.stdout).unwrap();
    assert!(output.contains(
        "role_core_v1 · 40d035a14aa5c5e8adaa41bcc3dbe7cb927fd0d47e122808f5a1a4b9ff6f843d"
    ));
    assert!(output.contains("role_core_v2 · "));
    assert!(output.contains(
        "role_core_v3 · cb9856d2c8edbc4cb0a59520aa140ef4567dce3b650b14f0436d42c4b11c375b"
    ));
}

#[test]
fn native_usage_requires_per_invocation_opt_in_before_output_creation() {
    let fixture = Fixture::new();
    let output = fixture.root().join("must-not-exist");
    let attempted = fixture.senate(&[
        "eval",
        "run",
        "--provider",
        "codex",
        "--out",
        output.to_str().unwrap(),
    ]);
    assert!(!attempted.status.success());
    assert!(
        String::from_utf8(attempted.stderr)
            .unwrap()
            .contains("may consume provider usage")
    );
    assert!(!output.exists());
}

#[test]
fn fake_repetitions_write_synthetic_results_without_polluting_normal_runs() {
    let fixture = Fixture::new();
    let output = fixture.root().join("fake-results");
    let run = fixture.senate(&[
        "eval",
        "run",
        "--provider",
        "fake",
        "--repeat",
        "2",
        "--out",
        output.to_str().unwrap(),
    ]);
    assert_success(&run);
    let results = load_results(&[output]).unwrap();
    assert_eq!(results.len(), 14);
    assert!(results.iter().all(|result| result.synthetic));
    assert!(
        results
            .iter()
            .all(|result| result.target.provider == EvalProvider::Fake)
    );
    assert!(
        results
            .iter()
            .all(|result| result.suite_version == "role_core_v1")
    );
    assert!(!fixture.normal_data.join("senate.db").exists());

    let listed = fixture.senate(&["runs"]);
    assert_success(&listed);
    assert_eq!(String::from_utf8(listed.stdout).unwrap().trim(), "No runs.");
}

/// Evidence at one effort level must never be mistaken for evidence at
/// another: the requested level is on every result, and the default is the
/// runtime's own, exactly as before the flag existed.
#[test]
fn eval_records_the_requested_effort_on_every_result() {
    let fixture = Fixture::new();
    let output = fixture.root().join("effort-results");
    let run = fixture.senate(&[
        "eval",
        "run",
        "--provider",
        "fake",
        "--effort",
        "xhigh",
        "--out",
        output.to_str().unwrap(),
    ]);
    assert_success(&run);
    let results = load_results(&[output]).unwrap();
    assert!(!results.is_empty());
    assert!(
        results
            .iter()
            .all(|result| result.requested_effort == Some(senate::domain::EffortSetting::XHIGH))
    );

    let refused = fixture.senate(&["eval", "run", "--provider", "fake", "--effort", "max"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("unsupported effort"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn fake_codex_v2_smoke_scores_calibrated_reviewers_without_native_usage() {
    let fixture = Fixture::new();
    let output = fixture.root().join("fake-v2-results");
    let run = fixture.senate_with_perfect_eval(&[
        "eval",
        "run",
        "--suite",
        "role_core_v2",
        "--provider",
        "codex",
        "--model",
        "fixture-model",
        "--allow-native-usage",
        "--out",
        output.to_str().unwrap(),
    ]);
    assert_success(&run);
    let results = load_results(&[output]).unwrap();
    assert_eq!(results.len(), 7);
    assert!(results.iter().all(|result| {
        result.suite_version == "role_core_v2" && result.status == EvalStatus::Passed
    }));
    let report = render_report(&results).unwrap();
    assert!(report.contains("SUITE role_core_v2"));
    assert!(report.contains("severity agreement           3/3 detected"));
    assert!(report.contains("duplicate findings           0"));
}

#[test]
fn controlled_codex_runs_real_boundaries_routes_only_target_role_and_scores_all_cases() {
    let fixture = Fixture::new();
    let output = fixture.root().join("native-results");
    let run = fixture.senate_with_perfect_eval(&[
        "eval",
        "run",
        "--provider",
        "codex",
        "--model",
        "fixture-model",
        "--allow-native-usage",
        "--out",
        output.to_str().unwrap(),
    ]);
    assert_success(&run);
    let results = load_results(std::slice::from_ref(&output)).unwrap();
    assert_eq!(results.len(), 7);
    assert!(
        results.iter().all(|result| {
            result.status == EvalStatus::Passed
                && !result.synthetic
                && result.target.configured_model.as_deref() == Some("fixture-model")
                // Codex never names its model on stdout, so this is read
                // from the session record it writes for itself; a pinned
                // model must come back confirmed, not merely configured.
                && result.confirmed_model.as_deref() == Some("fixture-model")
                && result.provider_cli_version.as_deref() == Some("codex-cli fixture-1")
                && result.usage.input_units == 11
                && result.usage.output_units == 7
        }),
        "{results:#?}"
    );
    assert!(!fixture.normal_data.join("senate.db").exists());

    let spec_database = output
        .join("spec_missing_wrong_unrequested")
        .join("rep-001")
        .join("runtime")
        .join("senate.db");
    let mut store = SqliteStore::open(spec_database).unwrap();
    let run_id = store.list_runs().unwrap()[0].id;
    let loaded = store.load_run(run_id).unwrap();
    let plan = RoutingPlan::from_snapshot(&loaded.config_snapshot, loaded.run.workflow()).unwrap();
    assert_eq!(
        plan.route(Role::SpecReviewer)
            .unwrap()
            .target()
            .provider_id()
            .as_str(),
        "codex"
    );
    for role in [
        Role::Researcher,
        Role::CodeQualityReviewer,
        Role::EngineeringLead,
    ] {
        assert_eq!(
            plan.route(role).unwrap().target().provider_id().as_str(),
            "fake"
        );
    }
    let target_stage = StageId::new("spec_review").unwrap();
    let (target_input, target_output, support_usage) = store
        .load_events(run_id)
        .unwrap()
        .iter()
        .fold((0_u64, 0_u64, 0_u64), |mut totals, event| {
            if let DomainEventKind::ProviderUsageUpdated {
                input_units,
                output_units,
                ..
            } = event.event.kind()
            {
                if event.event.stage_id() == Some(&target_stage) {
                    totals.0 += input_units;
                    totals.1 += output_units;
                } else {
                    totals.2 += input_units + output_units;
                }
            }
            totals
        });
    assert_eq!((target_input, target_output), (11, 7));
    assert!(support_usage > 0);

    let argv = fs::read_to_string(
        fixture
            .capture
            .join("spec_missing_wrong_unrequested.spec_review.argv"),
    )
    .unwrap();
    assert!(
        argv.lines()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|pair| { pair == ["--model", "fixture-model"] })
    );

    let mut synthetic = results[0].clone();
    synthetic.target = EvalTarget::new(EvalProvider::Fake, None).unwrap();
    synthetic.synthetic = true;
    let comparison = render_report(&[results[0].clone(), synthetic]).unwrap();
    assert!(comparison.contains("COMPARISON"));
    assert!(comparison.contains("codex / fixture-model"));
    assert!(comparison.contains("fake / native_default"));
}

struct Fixture {
    temp: TempDir,
    normal_data: PathBuf,
    /// Codex's own session records. Kept out of `normal_data`, which this
    /// suite asserts stays untouched by an isolated eval.
    codex_home: PathBuf,
    capture: PathBuf,
    fake_bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempdir().unwrap();
        let normal_data = temp.path().join("normal-data");
        let codex_home = temp.path().join("codex-home");
        let capture = temp.path().join("capture");
        let fake_bin = temp.path().join("fake-bin");
        fs::create_dir_all(&fake_bin).unwrap();
        for tool in ["git", "tmux", "cargo"] {
            let executable = find_on_path(tool).unwrap_or_else(|| panic!("{tool} is required"));
            std::os::unix::fs::symlink(executable, fake_bin.join(tool)).unwrap();
        }
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
            temp,
            normal_data,
            codex_home,
            capture,
            fake_bin,
        }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn senate(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn senate_with_perfect_eval(&self, args: &[&str]) -> Output {
        self.command(args)
            .env("SENATE_FAKE_CODEX_EVAL_PERFECT", "1")
            .output()
            .unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut paths = vec![self.fake_bin.clone()];
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_senate"));
        command
            .args(args)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("SENATE_DATA_DIR", &self.normal_data)
            .env("CODEX_HOME", &self.codex_home)
            .env("SENATE_FAKE_CODEX_CAPTURE_DIR", &self.capture);
        command
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
