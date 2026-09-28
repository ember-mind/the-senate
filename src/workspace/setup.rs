//! The commands a repository needs run once before anyone works in a new
//! worktree.
//!
//! A worktree is a fresh checkout of tracked files, which for many
//! repositories is not a working tree at all: build output is gitignored, so
//! `packages/*/dist`, `target/`, `vendor/` and their kin start empty. Every
//! agent then reads a tree where imports do not resolve and a type-check
//! cannot run, and the verification at the end fails on the missing artifacts
//! rather than on the change.
//!
//! `[verify]` could carry the build as its first command, and that is better
//! than nothing, but it runs after all the thinking is done. This table runs
//! before the first agent sees the worktree:
//!
//! ```toml
//! [setup]
//! commands = ["yarn build-packages"]
//! timeout_seconds = 3600
//! ```
//!
//! Same argv rule as `[verify]` — no shell, no pipes — and the same file,
//! found the same way, so a repository The Senate cannot commit to configures
//! this from the user's own checkout.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use super::WorkspaceError;
use crate::providers::repo_config::{self, CONFIG_FILE};
use crate::providers::verify::runner::{self, CommandExit};

/// Longer than verification's default: this is a cold build of a repository
/// that has never been built in this directory, not an incremental check.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3600);

/// The file as a whole; other tables belong to other readers.
#[derive(Deserialize)]
struct ConfigFile {
    setup: Option<SetupTable>,
}

/// Unknown keys are rejected for the same reason `[verify]` rejects them: a
/// misspelt `commands` that silently prepared nothing would show up as a
/// baffling failure three stages later.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupTable {
    commands: Vec<String>,
    timeout_seconds: Option<u64>,
}

/// Runs the repository's setup commands in a freshly created worktree.
///
/// A repository with no `[setup]` table does nothing, which is every
/// repository that was working before this existed.
///
/// # Errors
/// A `.senate.toml` whose `[setup]` table cannot be read, an empty command,
/// a zero timeout, or a command that does not exit zero. Preparation fails
/// rather than continuing: a worktree whose setup failed is exactly the
/// half-built tree this table exists to prevent, and letting agents loose in
/// it wastes a whole run to reach the same conclusion.
pub(crate) fn run_for(worktree: &Path, source_repo: &Path) -> Result<(), WorkspaceError> {
    let Some(table) = table_for(worktree, source_repo)? else {
        return Ok(());
    };
    let timeout = match table.timeout_seconds {
        None => DEFAULT_TIMEOUT,
        Some(0) => {
            return Err(setup_config_error(
                "[setup] timeout_seconds must be positive",
            ));
        }
        Some(seconds) => Duration::from_secs(seconds),
    };
    for (index, command) in table.commands.iter().enumerate() {
        if command.split_whitespace().next().is_none() {
            return Err(setup_config_error(&format!(
                "[setup] command {} is empty",
                index + 1
            )));
        }
    }
    for command in &table.commands {
        let report = runner::run(command, worktree, timeout)
            .map_err(|error| setup_config_error(&error.to_string()))?;
        if !report.exit.succeeded() {
            return Err(WorkspaceError::SetupFailed {
                command: command.clone(),
                reason: exit_sentence(&report.exit),
                output: tail_of(&report),
            });
        }
    }
    Ok(())
}

fn table_for(worktree: &Path, source_repo: &Path) -> Result<Option<SetupTable>, WorkspaceError> {
    let Some(found) = repo_config::locate(worktree, Some(source_repo)) else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&found.path)
        .map_err(|error| setup_config_error(&error.to_string()))?;
    let file: ConfigFile =
        toml::from_str(&text).map_err(|error| setup_config_error(error.message()))?;
    Ok(file.setup)
}

fn setup_config_error(reason: &str) -> WorkspaceError {
    WorkspaceError::SetupConfig(format!("{CONFIG_FILE}: {reason}"))
}

fn exit_sentence(exit: &CommandExit) -> String {
    match exit {
        CommandExit::Code(code) => format!("exited {code}"),
        CommandExit::Signal(signal) => format!("was killed by signal {signal}"),
        CommandExit::TimedOut(limit) => format!("timed out after {} s", limit.as_secs()),
        CommandExit::CouldNotStart(error) => format!("could not start: {error}"),
        CommandExit::StatusUnavailable(error) => format!("status could not be read: {error}"),
    }
}

/// The last few lines of what the command printed, so the failure a user sees
/// names the actual cause rather than only the exit code. Preparation has no
/// artifact to write, so this is the only place the output survives.
fn tail_of(report: &runner::CommandReport) -> String {
    const KEPT_LINES: usize = 20;
    let mut text = String::new();
    for captured in [&report.stdout, &report.stderr] {
        let stream = String::from_utf8_lossy(&captured.bytes);
        let lines: Vec<&str> = stream.lines().collect();
        let start = lines.len().saturating_sub(KEPT_LINES);
        for line in &lines[start..] {
            text.push_str(line);
            text.push('\n');
        }
    }
    text.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worktree_with(config: Option<&str>) -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temp worktree");
        if let Some(text) = config {
            std::fs::write(directory.path().join(CONFIG_FILE), text).expect("write config");
        }
        directory
    }

    #[test]
    fn no_file_and_no_setup_table_both_do_nothing() {
        let empty = worktree_with(None);
        assert!(run_for(empty.path(), empty.path()).is_ok());

        let other = worktree_with(Some("[verify]\ncommands = [\"cargo test\"]\n"));
        assert!(run_for(other.path(), other.path()).is_ok());
    }

    #[test]
    fn commands_run_in_the_worktree_and_in_order() {
        let directory = worktree_with(Some(
            "[setup]\ncommands = [\"touch first\", \"mv first second\"]\n",
        ));

        run_for(directory.path(), directory.path()).expect("setup runs");

        // The second command could only succeed from inside the worktree and
        // only after the first one had.
        assert!(directory.path().join("second").is_file());
    }

    #[test]
    fn a_failing_command_stops_preparation_and_names_it() {
        let directory = worktree_with(Some(
            "[setup]\ncommands = [\"ls /no-such-senate-setup-path\", \"touch never\"]\n",
        ));

        let error = run_for(directory.path(), directory.path()).expect_err("setup fails");

        assert!(
            matches!(&error, WorkspaceError::SetupFailed { command, .. }
                if command == "ls /no-such-senate-setup-path"),
            "{error}"
        );
        assert!(
            !directory.path().join("never").exists(),
            "later commands must not run"
        );
    }

    #[test]
    fn the_failure_carries_what_the_command_printed() {
        // Commands are argv, so the failing one has to say something useful
        // without a shell: `ls` of a missing path prints the path it could
        // not find.
        let directory = worktree_with(Some(
            "[setup]\ncommands = [\"ls /no-such-senate-setup-path\"]\n",
        ));

        let error = run_for(directory.path(), directory.path()).expect_err("setup fails");

        let message = error.to_string();
        assert!(message.contains("/no-such-senate-setup-path"), "{message}");
        assert!(message.contains("exited"), "{message}");
    }

    #[test]
    fn the_source_repository_can_configure_a_repository_senate_cannot_commit_to() {
        let worktree = worktree_with(None);
        let source = worktree_with(Some("[setup]\ncommands = [\"touch from-source\"]\n"));

        run_for(worktree.path(), source.path()).expect("setup runs");

        assert!(worktree.path().join("from-source").is_file());
    }

    #[test]
    fn a_broken_setup_table_fails_preparation_rather_than_being_ignored() {
        for text in [
            "[setup]\ncommands = 1\n",
            "[setup]\ncommands = [\"true\", \"  \"]\n",
            "[setup]\ncommands = [\"true\"]\ntimeout_seconds = 0\n",
            "[setup]\ncommand = [\"true\"]\n",
        ] {
            let directory = worktree_with(Some(text));

            let error = run_for(directory.path(), directory.path()).expect_err(text);

            assert!(
                matches!(&error, WorkspaceError::SetupConfig(message)
                    if message.starts_with(CONFIG_FILE)),
                "{text:?} produced {error:?}"
            );
        }
    }
}
