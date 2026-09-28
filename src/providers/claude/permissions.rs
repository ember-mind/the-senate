//! The repository's own standing tool allowlist.
//!
//! Every Claude invocation runs with `--permission-mode dontAsk`, so anything
//! The Senate has not granted is denied outright. Derived grants only ever come
//! from a denial the user then approved, which means the first attempt at a
//! repository's own build, lint and test commands is always denied, always
//! costs a round trip through attention, and — when the command is compound —
//! can never be granted exactly at all.
//!
//! A repository answers that once, in its own `.senate.toml`, with the rules
//! it is willing to hand every run:
//!
//! ```toml
//! [permissions]
//! allow = ["Bash(yarn jest:*)", "Bash(yarn lint:css:*)", "mcp__linear-server"]
//! ```
//!
//! The strings are native Claude Code `--allowedTools` rules, passed through
//! verbatim: this file is the repository's explicit intent, not a guess
//! The Senate derives from a denial, so it is not re-parsed or widened here. The
//! one thing refused is a rule that grants everything, because a permission
//! model that can be turned off in a config file is not one.
//!
//! On top of whatever the repository says, every run also gets [`BASELINE`] —
//! the tools an agent needs before it knows anything about the repository at
//! all. Without it the first minutes of every run in every repository are the
//! same denial storm: 342 denials across 33 measured runs, of which `grep`
//! (58), `ls` (20), `cat`, `sed`, `find` and the read-only `git` subcommands
//! account for well over a third, and `Edit`/`Write` for another 60. None of
//! those is a decision a repository should have to make.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

use crate::providers::repo_config;

/// The per-repository configuration file, shared with `[verify]`. Read from
/// the run's worktree so a change the run itself makes is what takes effect,
/// falling back to the repository the worktree was cut from; `repo_config`
/// owns that order and the reason for it.
pub(crate) use repo_config::CONFIG_FILE;

/// Rules that would grant every tool, refused however they are spelled.
const BLANKET_RULES: [&str; 2] = ["*", "Bash(*)"];

/// The grants every run starts with, whatever the repository says.
///
/// Two kinds of thing, and nothing else:
///
///   * Reading the tree — searching it, listing it, printing files, and the
///     `git` subcommands that only report. A run cannot begin without these
///     and no repository has a reason to withhold them.
///   * Changing the tree — `Edit`, `MultiEdit`, `Write`. Editing the checkout
///     is the whole job, and the checkout is a throwaway worktree on a
///     throwaway branch, so the blast radius of a bad edit is that worktree.
///
/// Deliberately absent: anything that installs, builds, runs a package
/// script, or reaches the network. `node`, `python3`, `yarn`, `npm`, `npx`
/// and `gh` all showed up in the measured denials, and all of them run
/// arbitrary code chosen by something other than this list — they belong in
/// a repository's own `[permissions]`, where a human decided the repository
/// wanted them.
///
/// `sed` and `cat` can write through a shell redirection, and are listed
/// anyway: `Edit` and `Write` are already granted here, so refusing them
/// would protect nothing and only cost the round trip this list exists to
/// remove.
const BASELINE: &[&str] = &[
    // Changing the tree.
    "Edit",
    "MultiEdit",
    "Write",
    // Searching and reading it.
    "Bash(grep:*)",
    "Bash(rg:*)",
    "Bash(find:*)",
    "Bash(ls:*)",
    "Bash(cat:*)",
    "Bash(head:*)",
    "Bash(tail:*)",
    "Bash(sed:*)",
    "Bash(awk:*)",
    "Bash(cut:*)",
    "Bash(tr:*)",
    "Bash(sort:*)",
    "Bash(uniq:*)",
    "Bash(wc:*)",
    "Bash(diff:*)",
    "Bash(jq:*)",
    "Bash(echo:*)",
    "Bash(printf:*)",
    "Bash(basename:*)",
    "Bash(dirname:*)",
    // Reporting on history. Nothing that fetches, writes a ref, or moves the
    // working tree — a run's own commits go through The Senate, not the agent.
    "Bash(git status:*)",
    "Bash(git log:*)",
    "Bash(git show:*)",
    "Bash(git diff:*)",
    "Bash(git grep:*)",
];

#[derive(Debug, Error)]
pub enum PermissionsConfigError {
    #[error("{CONFIG_FILE}: {0}")]
    Unreadable(String),
    #[error("{CONFIG_FILE}: [permissions] allow rule {index} is empty")]
    EmptyRule { index: usize },
    #[error(
        "{CONFIG_FILE}: [permissions] allow rule {index} grants every tool ({rule}); list the commands the repository actually needs"
    )]
    BlanketRule { index: usize, rule: String },
}

/// The whole file. Other tables are tolerated so a repository that configures
/// `[verify]` and nothing else still reads cleanly here.
#[derive(Deserialize)]
struct ConfigFile {
    permissions: Option<PermissionsTable>,
}

/// Unknown keys are rejected: a misspelt `allow` that silently granted nothing
/// would look exactly like the denial storm this table exists to end.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionsTable {
    allow: Vec<String>,
}

/// [`BASELINE`] as the set every caller compares against.
pub(crate) fn baseline() -> BTreeSet<String> {
    BASELINE.iter().map(|rule| (*rule).to_owned()).collect()
}

/// Reads the standing allowlist for one run: [`BASELINE`] plus whatever the
/// repository adds to it.
///
/// A missing file, or a file without a `[permissions]` table, is the baseline
/// alone rather than an error — most repositories have neither.
///
/// # Errors
/// A `.senate.toml` that exists but cannot be read or parsed, an empty rule,
/// or a rule that grants every tool.
pub(crate) fn allow_rules(
    worktree: &Path,
    source_repo: Option<&Path>,
) -> Result<BTreeSet<String>, PermissionsConfigError> {
    let baseline = baseline();
    let Some(found) = repo_config::locate(worktree, source_repo) else {
        return Ok(baseline);
    };
    let text = std::fs::read_to_string(&found.path)
        .map_err(|error| PermissionsConfigError::Unreadable(error.to_string()))?;
    let file: ConfigFile = toml::from_str(&text)
        .map_err(|error| PermissionsConfigError::Unreadable(error.message().to_owned()))?;
    let Some(table) = file.permissions else {
        return Ok(baseline);
    };
    let mut rules = baseline;
    for (index, rule) in table.allow.iter().enumerate() {
        let rule = rule.trim();
        if rule.is_empty() {
            return Err(PermissionsConfigError::EmptyRule { index: index + 1 });
        }
        if BLANKET_RULES.contains(&rule) {
            return Err(PermissionsConfigError::BlanketRule {
                index: index + 1,
                rule: rule.to_owned(),
            });
        }
        rules.insert(rule.to_owned());
    }
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worktree_with(contents: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temp worktree");
        std::fs::write(directory.path().join(CONFIG_FILE), contents).expect("write config");
        directory
    }

    /// What a repository added, with the baseline taken back out — the only
    /// part of the result its `.senate.toml` is responsible for.
    fn beyond_baseline(rules: &BTreeSet<String>) -> Vec<String> {
        rules.difference(&baseline()).cloned().collect()
    }

    #[test]
    fn missing_file_and_missing_table_both_grant_the_baseline() {
        let empty = tempfile::tempdir().expect("temp worktree");
        assert_eq!(
            allow_rules(empty.path(), None).expect("no file"),
            baseline()
        );

        let other_table = worktree_with("[verify]\ncommands = [\"cargo test\"]\n");
        assert_eq!(
            allow_rules(other_table.path(), None).expect("verify-only config"),
            baseline()
        );
    }

    /// The point of the baseline: a repository that has never heard of
    /// The Senate still gets to search and read its own tree, and to edit it.
    #[test]
    fn the_baseline_covers_searching_reading_and_editing_without_any_config() {
        let empty = tempfile::tempdir().expect("temp worktree");
        let rules = allow_rules(empty.path(), None).expect("no file");

        for granted in [
            "Bash(grep:*)",
            "Bash(ls:*)",
            "Bash(git log:*)",
            "Edit",
            "Write",
        ] {
            assert!(rules.contains(granted), "baseline is missing {granted}");
        }
    }

    /// The baseline is not a place to smuggle in a package manager. Anything
    /// that installs, builds or reaches the network stays a decision the
    /// repository makes in its own `[permissions]`.
    #[test]
    fn the_baseline_runs_nothing_that_installs_builds_or_reaches_the_network() {
        for rule in BASELINE {
            assert!(
                ![
                    "yarn", "npm", "npx", "pnpm", "node", "python", "python3", "cargo", "gh",
                    "curl", "wget", "make", "docker", "ssh", "rm", "git"
                ]
                .iter()
                .any(|program| *rule == format!("Bash({program}:*)")),
                "{rule} grants more than reading or editing the tree"
            );
        }
    }

    #[test]
    fn the_source_repository_grants_for_a_repository_senate_cannot_commit_to() {
        let worktree = tempfile::tempdir().expect("temp worktree");
        let source = worktree_with("[permissions]\nallow = [\"Bash(yarn jest:*)\"]\n");

        let rules = allow_rules(worktree.path(), Some(source.path())).expect("source-repo grants");

        assert_eq!(
            beyond_baseline(&rules),
            vec!["Bash(yarn jest:*)".to_owned()]
        );
    }

    #[test]
    fn the_worktree_file_still_wins_over_the_source_repository() {
        let worktree = worktree_with("[permissions]\nallow = [\"Bash(cargo test:*)\"]\n");
        let source = worktree_with("[permissions]\nallow = [\"Bash(yarn jest:*)\"]\n");

        let rules = allow_rules(worktree.path(), Some(source.path())).expect("worktree grants");

        assert_eq!(
            beyond_baseline(&rules),
            vec!["Bash(cargo test:*)".to_owned()]
        );
    }

    #[test]
    fn rules_reach_the_command_verbatim_and_deduplicated() {
        let directory = worktree_with(
            "[permissions]\nallow = [\"Bash(yarn jest:*)\", \" mcp__linear-server \", \"Bash(yarn jest:*)\", \"Bash(grep:*)\"]\n",
        );
        let rules = allow_rules(directory.path(), None).expect("valid allowlist");
        assert_eq!(
            beyond_baseline(&rules),
            vec![
                "Bash(yarn jest:*)".to_owned(),
                "mcp__linear-server".to_owned()
            ],
            "a repository restating a baseline rule adds nothing"
        );
        assert_eq!(
            rules.iter().filter(|rule| *rule == "Bash(grep:*)").count(),
            1
        );
    }

    #[test]
    fn blanket_and_empty_rules_are_refused_by_position() {
        let blanket = worktree_with("[permissions]\nallow = [\"Bash(yarn jest:*)\", \"*\"]\n");
        let error = allow_rules(blanket.path(), None).expect_err("blanket rule");
        assert!(error.to_string().contains("rule 2"), "{error}");

        let empty = worktree_with("[permissions]\nallow = [\"  \"]\n");
        let error = allow_rules(empty.path(), None).expect_err("empty rule");
        assert!(error.to_string().contains("rule 1"), "{error}");
    }

    #[test]
    fn misspelt_key_fails_instead_of_granting_nothing() {
        let directory = worktree_with("[permissions]\nallowed = [\"Bash(yarn jest:*)\"]\n");
        assert!(matches!(
            allow_rules(directory.path(), None),
            Err(PermissionsConfigError::Unreadable(_))
        ));
    }
}
