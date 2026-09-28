//! Command-line and permission-config construction for `opencode run`.
//!
//! `opencode run --help` (v1.18.32, verified by hand) offers no way to feed
//! the stage prompt through stdin or a file: the message is positional argv
//! text, and `-f`/`--file` attaches a file to the message rather than
//! replacing it. Every other adapter in this codebase puts its prompt on
//! immutable stdin; here that is not available, so the prompt travels as a
//! plain positional argument instead — still one `std::process::Command`
//! argument, never shell text, so no interpolation or quoting risk exists.
//! What argv-as-transport does cost is the platform's combined argv+environ
//! size ceiling (`ARG_MAX`), which stdin never faced. [`MAX_PROMPT_BYTES`]
//! keeps this adapter's composed prompt (already the smallest of any adapter,
//! since it also carries no stdin-only content) comfortably inside every
//! documented `ARG_MAX` this process is likely to run under, and prompt
//! composition sheds the same optional navigation evidence Codex sheds before
//! failing closed.
//!
//! Permissions travel through a per-invocation config file passed as
//! `OPENCODE_CONFIG`, verified by hand to load beside (not instead of) the
//! user's own `~/.config/opencode` configuration. `OPENCODE_CONFIG` is a path
//! to a file this adapter wrote, not a credential, so it travels as an
//! ordinary process-spec environment entry like any other adapter's argv —
//! never through the credential-handoff socket that exists for the user's own
//! native authentication.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use serde_json::{Value, json};

use crate::domain::{EffortLevel, EffortSetting, ModelId, ProviderSessionId, StageKind};

/// Conservative ceiling on the composed prompt, well inside every documented
/// POSIX `ARG_MAX` (commonly 128 KiB–2 MiB, combined with the environment
/// block) even though opencode's prompt already carries no other adapter's
/// stdin-only content. Not verified against opencode itself: no automated
/// probe here would be safe to run against real vendor credentials, so this
/// is a documented margin rather than a measured limit.
pub(crate) const MAX_PROMPT_BYTES: usize = 256 * 1024;

pub(crate) struct OpencodeCommand {
    pub argv: Vec<OsString>,
    pub environment: BTreeMap<OsString, OsString>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpencodeSandbox {
    ReadOnly,
    WorkspaceWrite,
}

impl OpencodeSandbox {
    /// Mirrors Codex's own stage-kind split exactly: `Implementation`, `Fix`,
    /// and `FollowUp` write to the workspace; every other stage kind,
    /// including both specialized reviewers, is read-only.
    pub(crate) const fn for_stage(kind: StageKind) -> Self {
        match kind {
            StageKind::Implementation
            | StageKind::Simplification
            | StageKind::Fix
            | StageKind::FollowUp => Self::WorkspaceWrite,
            StageKind::Research
            | StageKind::Architecture
            | StageKind::CodeQualityReview
            | StageKind::SpecReview
            | StageKind::Review
            | StageKind::IndependentReview
            | StageKind::DeepAnalysis
            | StageKind::Synthesis
            | StageKind::Decision
            | StageKind::Lead
            | StageKind::Verify => Self::ReadOnly,
        }
    }

    /// The permission config this sandbox writes. Read-only denies edit,
    /// bash, webfetch, and external-directory access outright (the exact
    /// shape verified by hand against a real read-only run). Workspace-write
    /// allows edit, still denies webfetch and external-directory access, and
    /// gates bash behind an explicit allowlist derived from the repository's
    /// own Claude allowlist (see [`bash_allow_patterns`]), asking for
    /// anything else rather than allowing or silently denying it — headless
    /// opencode auto-rejects an `ask` it cannot answer, so this never hangs.
    pub(crate) fn permission_config(self, bash_allow: &BTreeMap<String, String>) -> Value {
        match self {
            Self::ReadOnly => json!({
                "$schema": "https://opencode.ai/config.json",
                "permission": {
                    "edit": "deny",
                    "bash": "deny",
                    "webfetch": "deny",
                    "external_directory": "deny",
                }
            }),
            Self::WorkspaceWrite => {
                let mut bash = serde_json::Map::new();
                bash.insert("*".to_owned(), Value::String("ask".to_owned()));
                for (pattern, action) in bash_allow {
                    bash.insert(pattern.clone(), Value::String(action.clone()));
                }
                json!({
                    "$schema": "https://opencode.ai/config.json",
                    "permission": {
                        "edit": "allow",
                        "webfetch": "deny",
                        "external_directory": "deny",
                        "bash": Value::Object(bash),
                    }
                })
            }
        }
    }
}

/// Converts the repository's own Claude `--allowedTools` bash allowlist (see
/// `crate::providers::claude::permissions`) into opencode bash-permission
/// glob patterns, so a repository states its safe commands once rather than
/// once per provider. Only rules of the exact-prefix shape Claude itself uses
/// for a plain command (`Bash(<prefix>:*)`) translate; every other rule
/// (`Edit`, `mcp__*`, or a shape this function does not recognize) is simply
/// not a bash pattern and is skipped rather than guessed at.
///
/// # Errors
/// Returns whatever reading the repository's `[permissions]` table returns.
pub(crate) fn bash_allow_patterns(
    worktree: &Path,
    source_repo: Option<&Path>,
) -> Result<BTreeMap<String, String>, crate::providers::claude::permissions::PermissionsConfigError>
{
    let rules = crate::providers::claude::permissions::allow_rules(worktree, source_repo)?;
    let mut patterns = BTreeMap::new();
    for rule in rules {
        if let Some(inner) = rule
            .strip_prefix("Bash(")
            .and_then(|rest| rest.strip_suffix(')'))
            && let Some(prefix) = inner.strip_suffix(":*")
            && !prefix.is_empty()
        {
            patterns.insert(format!("{prefix} *"), "allow".to_owned());
        }
    }
    Ok(patterns)
}

pub(crate) fn initial(
    prompt: &str,
    _stage_kind: StageKind,
    model: Option<&ModelId>,
    effort: EffortSetting,
    workspace: &Path,
    config_path: &Path,
) -> OpencodeCommand {
    let mut argv = base(model, effort, workspace);
    argv.push(OsString::from(prompt));
    OpencodeCommand {
        argv,
        environment: environment(config_path),
    }
}

pub(crate) fn resume(
    session_id: &ProviderSessionId,
    prompt: &str,
    _stage_kind: StageKind,
    model: Option<&ModelId>,
    effort: EffortSetting,
    workspace: &Path,
    config_path: &Path,
) -> OpencodeCommand {
    let mut argv = base(model, effort, workspace);
    argv.push(OsString::from("--session"));
    argv.push(OsString::from(session_id.as_str()));
    argv.push(OsString::from(prompt));
    OpencodeCommand {
        argv,
        environment: environment(config_path),
    }
}

fn base(model: Option<&ModelId>, effort: EffortSetting, workspace: &Path) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("run"),
        OsString::from("--format"),
        OsString::from("json"),
    ];
    if let Some(model) = model {
        argv.push(OsString::from("-m"));
        argv.push(OsString::from(model.as_str()));
    }
    if let EffortSetting::Level(level) = effort {
        argv.push(OsString::from("--variant"));
        argv.push(OsString::from(native_effort_value(level)));
    }
    argv.push(OsString::from("--dir"));
    argv.push(workspace.as_os_str().to_owned());
    argv
}

fn environment(config_path: &Path) -> BTreeMap<OsString, OsString> {
    BTreeMap::from([(
        OsString::from("OPENCODE_CONFIG"),
        config_path.as_os_str().to_owned(),
    )])
}

/// Native opencode `--variant` value for one explicit requested level.
/// opencode's variants are provider-specific (`high`, `max`, `minimal`, ...);
/// The Senate's four ordered levels map onto the vendor-neutral words every
/// provider in practice accepts, the same low/medium/high/xhigh spelling
/// Codex's own `model_reasoning_effort` uses.
pub(crate) const fn native_effort_value(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::Low => "low",
        EffortLevel::Medium => "medium",
        EffortLevel::High => "high",
        EffortLevel::XHigh => "xhigh",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_default_omits_variant_byte_identical() {
        let command = initial(
            "prompt",
            StageKind::Review,
            None,
            EffortSetting::NativeDefault,
            Path::new("/managed/worktree"),
            Path::new("/private/config.json"),
        );
        let args = strings(&command.argv);
        assert!(!args.iter().any(|arg| arg == "--variant"));
    }

    #[test]
    fn explicit_effort_maps_onto_variant() {
        for (setting, expected) in [
            (EffortSetting::LOW, "low"),
            (EffortSetting::MEDIUM, "medium"),
            (EffortSetting::HIGH, "high"),
            (EffortSetting::XHIGH, "xhigh"),
        ] {
            let command = initial(
                "prompt",
                StageKind::Review,
                None,
                setting,
                Path::new("/managed/worktree"),
                Path::new("/private/config.json"),
            );
            let args = strings(&command.argv);
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == "--variant" && pair[1] == expected),
                "{setting:?} must produce --variant {expected}"
            );
        }
    }

    #[test]
    fn prompt_is_the_final_positional_argument_and_never_touches_the_environment() {
        let marker = "SUPER_SECRET_TASK_MARKER";
        let command = initial(
            marker,
            StageKind::Review,
            None,
            EffortSetting::NativeDefault,
            Path::new("/managed/worktree"),
            Path::new("/private/config.json"),
        );
        assert_eq!(command.argv.last().map(|arg| arg == marker), Some(true));
        assert!(command.environment.values().all(|value| value != marker));
    }

    #[test]
    fn model_and_workdir_reach_argv() {
        let command = initial(
            "task",
            StageKind::Implementation,
            Some(&ModelId::new("opencode-go/deepseek-v4-pro").unwrap()),
            EffortSetting::NativeDefault,
            Path::new("/managed/worktree"),
            Path::new("/private/config.json"),
        );
        let args = strings(&command.argv);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-m", "opencode-go/deepseek-v4-pro"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--dir", "/managed/worktree"])
        );
        assert!(!args.iter().any(|arg| arg == "--auto"));
    }

    #[test]
    fn resume_targets_exact_session_without_auto() {
        let command = resume(
            &ProviderSessionId::new("ses_A").unwrap(),
            "continue",
            StageKind::Fix,
            None,
            EffortSetting::NativeDefault,
            Path::new("/managed/worktree"),
            Path::new("/private/config.json"),
        );
        let args = strings(&command.argv);
        assert!(args.windows(2).any(|pair| pair == ["--session", "ses_A"]));
        assert!(!args.iter().any(|arg| arg == "--auto"));
    }

    #[test]
    fn the_config_path_reaches_the_environment_and_nothing_else_does() {
        let command = initial(
            "prompt",
            StageKind::Implementation,
            None,
            EffortSetting::NativeDefault,
            Path::new("/managed/worktree"),
            Path::new("/private/run/config.json"),
        );
        assert_eq!(command.environment.len(), 1);
        assert_eq!(
            command.environment.get(&OsString::from("OPENCODE_CONFIG")),
            Some(&OsString::from("/private/run/config.json"))
        );
    }

    #[test]
    fn implementation_and_fix_and_followup_are_workspace_write_everything_else_is_read_only() {
        for kind in [
            StageKind::Implementation,
            StageKind::Simplification,
            StageKind::Fix,
            StageKind::FollowUp,
        ] {
            assert_eq!(
                OpencodeSandbox::for_stage(kind),
                OpencodeSandbox::WorkspaceWrite,
                "{kind:?}"
            );
        }
        for kind in [
            StageKind::Research,
            StageKind::Architecture,
            StageKind::CodeQualityReview,
            StageKind::SpecReview,
            StageKind::Review,
            StageKind::Decision,
            StageKind::Verify,
        ] {
            assert_eq!(
                OpencodeSandbox::for_stage(kind),
                OpencodeSandbox::ReadOnly,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn read_only_config_matches_the_verified_real_shape() {
        let config = OpencodeSandbox::ReadOnly.permission_config(&BTreeMap::new());
        assert_eq!(
            config,
            json!({
                "$schema": "https://opencode.ai/config.json",
                "permission": {
                    "edit": "deny",
                    "bash": "deny",
                    "webfetch": "deny",
                    "external_directory": "deny",
                }
            })
        );
    }

    #[test]
    fn workspace_write_config_asks_for_bash_by_default_and_allows_named_patterns() {
        let mut allow = BTreeMap::new();
        allow.insert("git status *".to_owned(), "allow".to_owned());
        let config = OpencodeSandbox::WorkspaceWrite.permission_config(&allow);
        assert_eq!(config["permission"]["edit"], json!("allow"));
        assert_eq!(config["permission"]["webfetch"], json!("deny"));
        assert_eq!(config["permission"]["external_directory"], json!("deny"));
        assert_eq!(config["permission"]["bash"]["*"], json!("ask"));
        assert_eq!(config["permission"]["bash"]["git status *"], json!("allow"));
    }

    #[test]
    fn bash_allow_patterns_translate_the_shared_claude_allowlist() {
        let worktree = tempfile::tempdir().unwrap();
        let patterns = bash_allow_patterns(worktree.path(), None).unwrap();
        // The baseline every Claude run starts with also seeds opencode's
        // bash allowlist: the repository states its safe commands once.
        assert_eq!(patterns.get("grep *").map(String::as_str), Some("allow"));
        assert_eq!(
            patterns.get("git status *").map(String::as_str),
            Some("allow")
        );
        // Non-bash rules (Edit, MultiEdit, Write) are not bash patterns.
        assert!(!patterns.contains_key("Edit *"));
    }

    #[test]
    fn bash_allow_patterns_pick_up_repository_customization() {
        let worktree = tempfile::tempdir().unwrap();
        std::fs::write(
            worktree.path().join(".senate.toml"),
            "[permissions]\nallow = [\"Bash(cargo test:*)\"]\n",
        )
        .unwrap();
        let patterns = bash_allow_patterns(worktree.path(), None).unwrap();
        assert_eq!(
            patterns.get("cargo test *").map(String::as_str),
            Some("allow")
        );
    }

    fn strings(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }
}
