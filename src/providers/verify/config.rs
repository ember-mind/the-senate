//! Where a workspace's verification commands come from.
//!
//! The repository owns its own definition of "verified": a `[verify]` table
//! in its `.senate.toml` when it has one, otherwise a guess from the build
//! files present. The Senate never invents commands beyond that guess, and it
//! says which of the three it used in the artifact — naming the checkout the
//! file came from — so a green stage can always be read back to what was
//! actually checked.
//!
//! Which checkout answers is `repo_config`'s question, not this module's.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use super::VerifyError;
use crate::providers::repo_config::{self, ConfigOrigin};

pub use repo_config::CONFIG_FILE;

/// How long one command may run before it is killed and counted as failed.
/// Generous because a full test suite is the common case; the artifact
/// names the limit when it bites.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1800);

/// Build files The Senate recognises when no `[verify]` table exists, with the
/// command each implies. Order matters: the first present wins, so a
/// polyglot repository verifies with one toolchain rather than all of them.
const DETECTION_RULES: [(&str, &str); 5] = [
    ("Cargo.toml", "cargo test"),
    ("package.json", "npm test"),
    ("pyproject.toml", "pytest"),
    ("pytest.ini", "pytest"),
    ("go.mod", "go test ./..."),
];

/// Which of the three sources answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommandSource {
    /// The `[verify]` table of `.senate.toml`; carries the checkout the
    /// file was read from, because the two are configured differently and a
    /// reader chasing a command needs to know which file to open.
    ConfigFile(ConfigOrigin),
    /// A build file The Senate recognises; carries the file that matched.
    Detected(&'static str),
    /// A build file matched, but the command it implies was refused because
    /// it would not have said anything about the change; carries the file
    /// and why, so the artifact can tell the reader what to configure.
    Declined {
        marker: &'static str,
        reason: DeclineReason,
    },
    /// Neither a table nor a recognised build file.
    Nothing,
}

/// Why a build file's implied command was not worth running.
///
/// Both cases are the same judgement: a guess that cannot come back green on
/// an unchanged tree is worse than checking nothing, because the stage then
/// reports a failure no change caused and a fix cycle gets spent on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeclineReason {
    /// A workspaces root. `npm test` there runs the whole monorepo — every
    /// package, for any change — which is slow and, in a repository of any
    /// size, usually already failing for reasons of its own.
    Workspaces,
    /// No `test` script, so `npm test` would exit non-zero on the missing
    /// script alone and fail the stage without running anything.
    NoTestScript,
}

/// The commands one verification pass will run, in order, and its limits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifyPlan {
    pub commands: Vec<String>,
    pub timeout: Duration,
    pub source: CommandSource,
}

/// The file as a whole. Other tables are tolerated, not rejected: the file
/// is documented as the future home of more per-repository settings, and a
/// stage must not fail because a table it does not read appeared.
#[derive(Deserialize)]
struct ConfigFile {
    verify: Option<VerifyTable>,
}

/// The one table this stage reads. Unknown keys inside it are rejected,
/// because a misspelt `commands` silently verifying nothing is exactly the
/// failure a verification stage exists to prevent.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyTable {
    commands: Vec<String>,
    timeout_seconds: Option<u64>,
}

/// Resolves the plan for one worktree.
///
/// # Errors
/// A `.senate.toml` that exists but cannot be read or parsed, or a
/// `[verify]` table with an empty command or a zero timeout. The stage
/// fails on these rather than falling back to detection: a broken
/// configuration is a finding, not an absence.
pub(crate) fn plan_for(
    worktree: &Path,
    source_repo: Option<&Path>,
) -> Result<VerifyPlan, VerifyError> {
    if let Some(found) = repo_config::locate(worktree, source_repo) {
        let text = std::fs::read_to_string(&found.path)
            .map_err(|error| VerifyError::Config(format!("{CONFIG_FILE}: {error}")))?;
        let file: ConfigFile = toml::from_str(&text)
            .map_err(|error| VerifyError::Config(format!("{CONFIG_FILE}: {}", error.message())))?;
        if let Some(table) = file.verify {
            return plan_from_table(table, found.origin);
        }
    }
    Ok(DETECTION_RULES
        .iter()
        .find(|(marker, _)| worktree.join(marker).is_file())
        .map_or(
            VerifyPlan {
                commands: Vec::new(),
                timeout: DEFAULT_TIMEOUT,
                source: CommandSource::Nothing,
            },
            |(marker, command)| match decline_reason(worktree, marker) {
                Some(reason) => VerifyPlan {
                    commands: Vec::new(),
                    timeout: DEFAULT_TIMEOUT,
                    source: CommandSource::Declined { marker, reason },
                },
                None => VerifyPlan {
                    commands: vec![(*command).to_owned()],
                    timeout: DEFAULT_TIMEOUT,
                    source: CommandSource::Detected(marker),
                },
            },
        ))
}

/// The one thing a configured command may ask The Senate to fill in.
///
/// Commands are argv, not shell, so there is no `$VAR` to expand and no way
/// for a repository to name the commit its worktree was cut from — which is
/// exactly what a test runner needs to check the change rather than the whole
/// tree (`yarn test-client --changedSince={base_commit}`, `cargo test
/// --since {base_commit}`). One substitution keeps that reachable without
/// reintroducing a shell.
pub(crate) const BASE_COMMIT_PLACEHOLDER: &str = "{base_commit}";

/// Fills [`BASE_COMMIT_PLACEHOLDER`] in with the run's base commit.
///
/// Substituting before the runner splits on whitespace is safe: a commit ID
/// is hexadecimal, so it can never introduce a word boundary and turn one
/// argument into two.
///
/// # Errors
/// A command asks for the base commit on a run that has none recorded.
/// Running it with the placeholder left in would send the literal text
/// `{base_commit}` to the test runner, which either errors confusingly or —
/// worse — is read as a revision name and silently checks the wrong thing.
pub(crate) fn resolve_placeholders(
    commands: &[String],
    base_commit: Option<&str>,
) -> Result<Vec<String>, VerifyError> {
    commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            if !command.contains(BASE_COMMIT_PLACEHOLDER) {
                return Ok(command.clone());
            }
            let base_commit = base_commit.ok_or_else(|| {
                VerifyError::Config(format!(
                    "{CONFIG_FILE}: [verify] command {} uses {BASE_COMMIT_PLACEHOLDER}, \
                     but this run has no recorded base commit",
                    index + 1
                ))
            })?;
            Ok(command.replace(BASE_COMMIT_PLACEHOLDER, base_commit))
        })
        .collect()
}

/// Whether the command a matched build file implies is worth running.
///
/// Only `package.json` is inspected. The other markers imply a command that
/// is right for a workspace root as well as a single crate or module —
/// `cargo test` and `go test ./...` mean "this workspace" and are meant to be
/// run there — whereas `npm test` means "whatever the root `test` script
/// says", which in a monorepo is every package at once and in many
/// repositories is nothing at all.
///
/// A `package.json` that cannot be read or parsed falls through to the guess.
/// It is someone else's file, not The Senate's configuration, so a surprise in
/// it must not decide the stage; `.senate.toml` is the file whose being
/// broken is a finding.
fn decline_reason(worktree: &Path, marker: &str) -> Option<DeclineReason> {
    if marker != "package.json" {
        return None;
    }
    let text = std::fs::read_to_string(worktree.join(marker)).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;
    // Both array form (`["packages/*"]`) and the object form npm and Bun
    // accept (`{ "packages": [...] }`) mark a workspaces root.
    let workspaces = match manifest.get("workspaces") {
        Some(serde_json::Value::Array(globs)) => !globs.is_empty(),
        Some(serde_json::Value::Object(table)) => table
            .get("packages")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|globs| !globs.is_empty()),
        _ => false,
    };
    if workspaces {
        return Some(DeclineReason::Workspaces);
    }
    let has_test_script = manifest
        .get("scripts")
        .and_then(|scripts| scripts.get("test"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|script| !script.trim().is_empty());
    (!has_test_script).then_some(DeclineReason::NoTestScript)
}

fn plan_from_table(table: VerifyTable, origin: ConfigOrigin) -> Result<VerifyPlan, VerifyError> {
    for (index, command) in table.commands.iter().enumerate() {
        if command.split_whitespace().next().is_none() {
            return Err(VerifyError::Config(format!(
                "{CONFIG_FILE}: [verify] command {} is empty",
                index + 1
            )));
        }
    }
    let timeout = match table.timeout_seconds {
        None => DEFAULT_TIMEOUT,
        Some(0) => {
            return Err(VerifyError::Config(format!(
                "{CONFIG_FILE}: [verify] timeout_seconds must be positive"
            )));
        }
        Some(seconds) => Duration::from_secs(seconds),
    };
    Ok(VerifyPlan {
        commands: table.commands,
        timeout,
        source: CommandSource::ConfigFile(origin),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worktree() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_verify_table_wins_over_every_build_file() {
        let dir = worktree();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[verify]\ncommands = [\"cargo fmt --check\", \"cargo test\"]\ntimeout_seconds = 60\n",
        )
        .unwrap();

        let plan = plan_for(dir.path(), None).unwrap();

        assert_eq!(plan.commands, ["cargo fmt --check", "cargo test"]);
        assert_eq!(plan.timeout, Duration::from_secs(60));
        assert_eq!(
            plan.source,
            CommandSource::ConfigFile(ConfigOrigin::Worktree)
        );
    }

    #[test]
    fn the_source_repository_configures_a_repository_senate_cannot_commit_to() {
        let dir = worktree();
        let source = worktree();
        std::fs::write(
            dir.path().join("package.json"),
            "{\"scripts\":{\"test\":\"jest\"}}\n",
        )
        .unwrap();
        std::fs::write(
            source.path().join("package.json"),
            "{\"scripts\":{\"test\":\"jest\"}}\n",
        )
        .unwrap();
        std::fs::write(
            source.path().join(CONFIG_FILE),
            "[verify]\ncommands = [\"yarn test-client client/dashboard\"]\n",
        )
        .unwrap();

        let plan = plan_for(dir.path(), Some(source.path())).unwrap();

        // Detection would have said `npm test` here; the source repository's
        // file is what stops that from being the answer forever.
        assert_eq!(plan.commands, ["yarn test-client client/dashboard"]);
        assert_eq!(
            plan.source,
            CommandSource::ConfigFile(ConfigOrigin::SourceRepo)
        );
    }

    #[test]
    fn the_worktree_file_still_wins_over_the_source_repository() {
        let dir = worktree();
        let source = worktree();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[verify]\ncommands = [\"from the worktree\"]\n",
        )
        .unwrap();
        std::fs::write(
            source.path().join(CONFIG_FILE),
            "[verify]\ncommands = [\"from the source repo\"]\n",
        )
        .unwrap();

        let plan = plan_for(dir.path(), Some(source.path())).unwrap();

        assert_eq!(plan.commands, ["from the worktree"]);
        assert_eq!(
            plan.source,
            CommandSource::ConfigFile(ConfigOrigin::Worktree)
        );
    }

    #[test]
    fn a_worktree_file_without_a_verify_table_does_not_reach_past_itself() {
        let dir = worktree();
        let source = worktree();
        std::fs::write(
            dir.path().join("package.json"),
            "{\"scripts\":{\"test\":\"jest\"}}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "[permissions]\nallow = []\n").unwrap();
        std::fs::write(
            source.path().join(CONFIG_FILE),
            "[verify]\ncommands = [\"from the source repo\"]\n",
        )
        .unwrap();

        let plan = plan_for(dir.path(), Some(source.path())).unwrap();

        // One file answers, not two merged: the worktree's file is the
        // repository's current word on every table, including the ones it
        // leaves out.
        assert_eq!(plan.commands, ["npm test"]);
        assert_eq!(plan.source, CommandSource::Detected("package.json"));
    }

    #[test]
    fn a_config_file_without_a_verify_table_falls_back_to_detection() {
        let dir = worktree();
        std::fs::write(
            dir.path().join("package.json"),
            "{\"scripts\":{\"test\":\"jest\"}}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "[other]\nkey = 1\n").unwrap();

        let plan = plan_for(dir.path(), None).unwrap();

        assert_eq!(plan.commands, ["npm test"]);
        assert_eq!(plan.source, CommandSource::Detected("package.json"));
        assert_eq!(plan.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn the_base_commit_placeholder_is_filled_in_before_the_command_runs() {
        let commands = vec![
            "yarn test-client --changedSince={base_commit}".to_owned(),
            "cargo test".to_owned(),
        ];

        let resolved = resolve_placeholders(&commands, Some("0".repeat(40).as_str())).unwrap();

        assert_eq!(
            resolved,
            [
                format!("yarn test-client --changedSince={}", "0".repeat(40)),
                "cargo test".to_owned(),
            ]
        );
    }

    #[test]
    fn a_placeholder_with_no_base_commit_is_a_configuration_error() {
        // Left in, the literal `{base_commit}` reaches the test runner, which
        // either errors confusingly or reads it as a revision and checks the
        // wrong thing. Neither is something to discover from a green stage.
        let commands = vec!["yarn test-client --changedSince={base_commit}".to_owned()];

        let error = resolve_placeholders(&commands, None).unwrap_err();

        assert!(
            matches!(&error, VerifyError::Config(message)
                if message.contains("command 1") && message.contains(BASE_COMMIT_PLACEHOLDER)),
            "{error:?}"
        );
    }

    #[test]
    fn commands_without_the_placeholder_never_need_a_base_commit() {
        let commands = vec!["cargo test".to_owned()];

        assert_eq!(
            resolve_placeholders(&commands, None).unwrap(),
            ["cargo test"]
        );
    }

    #[test]
    fn a_workspaces_root_is_not_guessed_at() {
        for manifest in [
            r#"{"workspaces":["packages/*"]}"#,
            r#"{"workspaces":{"packages":["packages/*"]}}"#,
            // A test script does not rescue it: the root script is what runs
            // the whole monorepo in the first place.
            r#"{"workspaces":["packages/*"],"scripts":{"test":"jest"}}"#,
        ] {
            let dir = worktree();
            std::fs::write(dir.path().join("package.json"), manifest).unwrap();

            let plan = plan_for(dir.path(), None).unwrap();

            assert!(plan.commands.is_empty(), "{manifest}");
            assert_eq!(
                plan.source,
                CommandSource::Declined {
                    marker: "package.json",
                    reason: DeclineReason::Workspaces,
                },
                "{manifest}"
            );
        }
    }

    #[test]
    fn a_package_without_a_test_script_is_not_guessed_at() {
        for manifest in [
            r#"{"name":"x"}"#,
            r#"{"scripts":{"build":"tsc"}}"#,
            r#"{"scripts":{"test":"   "}}"#,
        ] {
            let dir = worktree();
            std::fs::write(dir.path().join("package.json"), manifest).unwrap();

            let plan = plan_for(dir.path(), None).unwrap();

            assert!(plan.commands.is_empty(), "{manifest}");
            assert_eq!(
                plan.source,
                CommandSource::Declined {
                    marker: "package.json",
                    reason: DeclineReason::NoTestScript,
                },
                "{manifest}"
            );
        }
    }

    #[test]
    fn an_ordinary_package_with_a_test_script_still_runs_npm_test() {
        let dir = worktree();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"jest"}}"#,
        )
        .unwrap();

        let plan = plan_for(dir.path(), None).unwrap();

        assert_eq!(plan.commands, ["npm test"]);
        assert_eq!(plan.source, CommandSource::Detected("package.json"));
    }

    #[test]
    fn an_unreadable_package_json_falls_through_to_the_guess() {
        // Someone else's manifest is not The Senate's configuration: a surprise
        // in it must not decide the stage the way a broken `.senate.toml`
        // does.
        let dir = worktree();
        std::fs::write(dir.path().join("package.json"), "{not json").unwrap();

        let plan = plan_for(dir.path(), None).unwrap();

        assert_eq!(plan.commands, ["npm test"]);
        assert_eq!(plan.source, CommandSource::Detected("package.json"));
    }

    #[test]
    fn a_verify_table_still_wins_over_a_declined_build_file() {
        let dir = worktree();
        std::fs::write(dir.path().join("package.json"), r#"{"workspaces":["p/*"]}"#).unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[verify]\ncommands = [\"yarn build-packages\"]\n",
        )
        .unwrap();

        let plan = plan_for(dir.path(), None).unwrap();

        assert_eq!(plan.commands, ["yarn build-packages"]);
        assert_eq!(
            plan.source,
            CommandSource::ConfigFile(ConfigOrigin::Worktree)
        );
    }

    #[test]
    fn only_package_json_is_second_guessed() {
        // `cargo test` and `go test ./...` mean "this workspace" and are the
        // right command at a workspace root, so a Cargo workspace keeps its
        // guess.
        let dir = worktree();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\"]\n",
        )
        .unwrap();

        let plan = plan_for(dir.path(), None).unwrap();

        assert_eq!(plan.commands, ["cargo test"]);
        assert_eq!(plan.source, CommandSource::Detected("Cargo.toml"));
    }

    #[test]
    fn the_first_recognised_build_file_wins() {
        let dir = worktree();
        std::fs::write(dir.path().join("go.mod"), "module x\n").unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "[tool]\n").unwrap();

        let plan = plan_for(dir.path(), None).unwrap();

        assert_eq!(plan.commands, ["pytest"]);
        assert_eq!(plan.source, CommandSource::Detected("pyproject.toml"));
    }

    #[test]
    fn nothing_recognised_means_an_empty_plan_not_an_error() {
        let dir = worktree();

        let plan = plan_for(dir.path(), None).unwrap();

        assert!(plan.commands.is_empty());
        assert_eq!(plan.source, CommandSource::Nothing);
    }

    #[test]
    fn malformed_toml_empty_commands_and_zero_timeouts_are_configuration_errors() {
        let dir = worktree();
        for text in [
            "[verify\ncommands = 1\n",
            "[verify]\ncommands = [\"cargo test\", \"   \"]\n",
            "[verify]\ncommands = [\"cargo test\"]\ntimeout_seconds = 0\n",
            "[verify]\ncommand = [\"cargo test\"]\n",
        ] {
            std::fs::write(dir.path().join(CONFIG_FILE), text).unwrap();
            let error = plan_for(dir.path(), None).unwrap_err();
            assert!(
                matches!(&error, VerifyError::Config(message) if message.starts_with(CONFIG_FILE)),
                "{text:?} produced {error:?}"
            );
        }
    }
}
