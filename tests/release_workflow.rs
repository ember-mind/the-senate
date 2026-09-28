//! Proves a mismatched Cargo/tag version cannot reach release publication.
//!
//! Two halves, both deterministic and offline: the gate binary itself refuses
//! a bad tag with a non-zero exit, and the release workflow is structured so
//! that nothing builds or publishes until that gate has passed.

use std::process::Command;

const WORKFLOW: &str = include_str!("../.github/workflows/release.yml");

/// The hidden gate command is what the workflow runs; these are the exact
/// cases the release process must refuse.
#[test]
fn the_gate_binary_refuses_every_tag_that_is_not_this_build() {
    let package = env!("CARGO_PKG_VERSION");
    let matching = format!("v{package}");
    let accepted = gate(&matching);
    assert!(
        accepted.status.success(),
        "a canonical tag naming this build must be accepted: {}",
        String::from_utf8_lossy(&accepted.stderr)
    );

    let mut ahead: Vec<u32> = package
        .split('.')
        .map(|part| part.parse().expect("package version is numeric semver"))
        .collect();
    ahead[2] += 1;
    let ahead = format!("v{}.{}.{}", ahead[0], ahead[1], ahead[2]);

    for tag in [
        ahead.as_str(),
        package,                        // missing the canonical `v`
        &format!("v{package}-rc.1"),    // prerelease
        &format!("v{package}+build.7"), // build metadata
        "nightly",
    ] {
        let rejected = gate(tag);
        assert!(
            !rejected.status.success(),
            "tag {tag:?} must fail the release gate"
        );
        assert!(
            !String::from_utf8_lossy(&rejected.stderr).is_empty(),
            "tag {tag:?} must explain why it was refused"
        );
    }
}

/// Structural proof: publication is unreachable without the gate. Even a
/// correct gate is worthless if a job can run around it.
#[test]
fn no_job_can_publish_without_passing_the_gate() {
    let jobs = job_dependencies();
    assert!(
        jobs.contains_key("guard") && jobs.contains_key("build") && jobs.contains_key("publish"),
        "expected guard, build and publish jobs, found {:?}",
        jobs.keys().collect::<Vec<_>>()
    );
    assert!(jobs.contains_key("quality"), "the quality gate exists");
    assert_eq!(jobs["guard"], None, "the gate depends on nothing");
    assert_eq!(jobs["quality"].as_deref(), Some("guard"));
    assert_eq!(jobs["build"].as_deref(), Some("quality"));
    assert_eq!(jobs["publish"].as_deref(), Some("build"));

    // Every job other than the gate must be transitively gated by it.
    for name in jobs.keys().filter(|name| *name != "guard") {
        assert!(
            reaches_guard(&jobs, name),
            "job {name} can run without the release gate"
        );
    }
    // And nothing may reach publication without passing quality first.
    for name in ["build", "publish"] {
        assert!(
            depends_on(&jobs, name, "quality"),
            "job {name} can run without the quality gate"
        );
    }
}

/// The release cannot publish assets built from source that fails the checks
/// normal CI enforces. Each command is named so removing one is a visible edit.
#[test]
fn the_quality_gate_runs_the_full_check_suite() {
    let quality = WORKFLOW
        .split("  quality:")
        .nth(1)
        .and_then(|rest| rest.split("\n  build:").next())
        .expect("the workflow has a quality job");
    for command in [
        "cargo fmt --check",
        "cargo clippy --all-targets --all-features -- -D warnings",
        "cargo test --test process_tmux --no-fail-fast",
        "cargo test",
    ] {
        assert!(
            quality.contains(command),
            "the quality gate must run `{command}`"
        );
    }
    assert!(
        quality.contains("install --yes tmux"),
        "the tmux integration prerequisite is installed"
    );
    assert!(
        quality.contains("inputs.tag || github.ref"),
        "the quality gate checks out the tagged source, for pushes and dispatches alike"
    );
}

/// The gate step must actually invoke the canonical check, on both triggers.
#[test]
fn the_gate_runs_the_canonical_check_for_pushes_and_dispatches() {
    assert!(
        WORKFLOW.contains("__verify-release-tag"),
        "the gate must call the one canonical tag rule"
    );
    // Both entry points resolve the same way, so a dispatch cannot bypass the
    // shape and version rules a tag push is held to.
    assert!(
        WORKFLOW.matches("inputs.tag || github.ref_name").count() >= 2,
        "workflow_dispatch and tag pushes must resolve the same candidate tag"
    );
    assert!(
        WORKFLOW.contains("tags: ['v[0-9]+.[0-9]+.[0-9]+']"),
        "the push trigger stays restricted to canonical release tags"
    );
    // The published release must be the stable kind the updater considers.
    assert!(WORKFLOW.contains("--latest"));
    assert!(
        !WORKFLOW.contains("--draft") && !WORKFLOW.contains("--prerelease"),
        "the updater ignores drafts and prereleases, so publishing one would be dead weight"
    );
}

fn gate(tag: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_senate"))
        .args(["__verify-release-tag", tag])
        .output()
        .expect("gate command runs")
}

/// Minimal structural read of `jobs:` and each job's single `needs:` value.
/// Deliberately narrow: it understands exactly the shape this workflow uses
/// and fails loudly if that shape changes.
fn job_dependencies() -> std::collections::BTreeMap<String, Option<String>> {
    let mut jobs = std::collections::BTreeMap::new();
    let mut in_jobs = false;
    let mut current: Option<String> = None;
    for line in WORKFLOW.lines() {
        if line.starts_with("jobs:") {
            in_jobs = true;
            continue;
        }
        if !in_jobs {
            continue;
        }
        // A new top-level key ends the jobs block.
        if !line.starts_with(' ') && !line.trim().is_empty() && !line.trim_start().starts_with('#')
        {
            break;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        if indent == 2 && trimmed.ends_with(':') && !trimmed.starts_with('#') {
            current = Some(trimmed.trim_end_matches(':').to_owned());
            jobs.insert(current.clone().unwrap(), None);
        } else if indent == 4
            && let (Some(name), Some(needs)) = (current.as_ref(), trimmed.strip_prefix("needs:"))
        {
            jobs.insert(name.clone(), Some(needs.trim().to_owned()));
        }
    }
    jobs
}

/// Whether `start` transitively depends on `target`.
fn depends_on(
    jobs: &std::collections::BTreeMap<String, Option<String>>,
    start: &str,
    target: &str,
) -> bool {
    let mut cursor = Some(start.to_owned());
    for _ in 0..jobs.len() {
        let Some(name) = cursor else { return false };
        if name == target {
            return true;
        }
        cursor = jobs.get(&name).and_then(Clone::clone);
    }
    false
}

fn reaches_guard(jobs: &std::collections::BTreeMap<String, Option<String>>, start: &str) -> bool {
    let mut cursor = Some(start.to_owned());
    // The chain is short; a bounded walk avoids looping on a malformed graph.
    for _ in 0..jobs.len() {
        let Some(name) = cursor else { return false };
        if name == "guard" {
            return true;
        }
        cursor = jobs.get(&name).and_then(Clone::clone);
    }
    false
}
