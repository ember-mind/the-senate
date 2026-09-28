//! Command-line and permission-config construction for `opencode run`.
//!
//! `opencode run --help` (v1.18.32–1.18.33, verified by hand) offers no way to feed
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
//! `OPENCODE_CONFIG`, kept for the record, but it is not the authority:
//! verified by hand, a repository-controlled `opencode.json` (or
//! `.opencode/opencode.json`) in the target worktree *overrides* it — a
//! read-only `OPENCODE_CONFIG` next to a repo file granting
//! `{"bash":"allow","edit":"allow"}` let the agent write a file. Every
//! invocation therefore also sets `OPENCODE_DISABLE_PROJECT_CONFIG=1`
//! (verified: blocks project `opencode.json` and `.opencode/` entirely — it
//! also stops opencode's own walk up parent directories for additional
//! `AGENTS.md` files, an accepted trade-off, since the project-root
//! `AGENTS.md` itself is still read unconditionally) and `OPENCODE_PERMISSION`
//! carrying this invocation's own permission object as JSON (verified:
//! merged on top of whatever config loaded, so it wins even if something
//! else still resolves). `OPENCODE_PERMISSION`, not `OPENCODE_CONFIG`, is the
//! actual authority; `OPENCODE_CONFIG` stays for a human inspecting the run's
//! own artifacts. `--pure` (verified via the installed binary's own strings:
//! gates loading of repository/user-supplied plugin code, distinct from
//! opencode's own built-in plugins) is passed on every invocation too, since
//! a plugin is arbitrary executable code and the class of risk is the same
//! as the config-override one. `OPENCODE_DISABLE_EXTERNAL_SKILLS` and
//! `OPENCODE_DISABLE_CLAUDE_CODE(_SKILLS)` were considered and declined:
//! skills are capability/prompt content, not a bypass of the permission
//! enforcement above, so disabling them would cost legitimate capability
//! without closing a hole `OPENCODE_PERMISSION`/`--pure` do not already close.
//! None of this is a credential, so it travels as ordinary process-spec argv
//! and environment entries, never through the credential-handoff socket that
//! exists for the user's own native authentication.

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
/// This inherits Claude's own baseline verbatim, `find *` and `sed *`/`awk *`
/// included: `find -exec`/`-delete` and `awk`'s `system()` can in principle
/// escape a command allowlist, but Claude's own baseline already accepts that
/// same risk for the same reason (`Edit`/`Write` are already granted, so
/// refusing the read-only-looking half of that risk protects nothing) — kept
/// for parity rather than re-litigated here.
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

/// The one exact bash-permission pattern that replays one already-atomic
/// command and nothing wider, or why it cannot be granted that way.
///
/// opencode's own bash-permission matcher reads every pattern as a glob, so a
/// pattern equal to the command verbatim is only an exact replay when the
/// command itself carries none of that glob's own special characters — `*`,
/// `?`, or `[` would silently widen "grant this exact call" into "grant
/// everything this also matches"; `{`/`}` (brace/group syntax) and `\`
/// (escaping, which would change what the pattern itself means) are refused
/// for the same reason. This is the same fail-closed reasoning behind
/// Claude's own `--allowedTools` rule-syntax refusal.
///
/// # Errors
/// Returns the reason a command cannot be granted as one exact pattern: it is
/// empty, or it carries a character opencode's matcher treats specially.
pub(crate) fn exact_bash_pattern(command: &str) -> Result<String, String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err("the denied command is empty".to_owned());
    }
    if let Some(character) = trimmed
        .chars()
        .find(|character| "*?[{}\\".contains(*character))
    {
        return Err(format!(
            "'{character}' is glob or escape syntax and would grant more than this exact command: {trimmed}"
        ));
    }
    Ok(trimmed.to_owned())
}

/// Splits one denied bash command into opencode's own top-level parts — the
/// exact granularity its permission check evaluates each sub-command at.
/// Verified against a real run: `pwd && ls -la` was auto-rejected because
/// opencode checked `pwd` and `ls -la` as two separate requests, so granting
/// the whole joined string as one pattern (matching neither part) never took
/// effect and the same denial repeated on every resume.
///
/// Splits on top-level `&&`, `||`, `;`, `|`, and newline — never inside
/// single or double quotes, and never on an escaped character. Returns
/// `None`, never a guess, the moment the command carries anything that makes
/// the split uncertain: an unterminated quote, command substitution (`` ` ``
/// or `$(`), brace/group syntax (`{`/`}`), or a bare redirection (`<`/`>`)
/// outside quotes. A caller that gets `None` must fail closed rather than
/// approve a pattern that might not mean what it looks like.
pub(crate) fn split_top_level_commands(command: &str) -> Option<Vec<String>> {
    #[derive(PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let bytes = command.as_bytes();
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut parts = Vec::new();
    let mut start = 0_usize;
    let mut i = 0_usize;
    while i < bytes.len() {
        let byte = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        match quote {
            Quote::Single => {
                if byte == b'\'' {
                    quote = Quote::None;
                }
                i += 1;
                continue;
            }
            Quote::Double => {
                if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quote = Quote::None;
                } else if byte == b'`' || (byte == b'$' && bytes.get(i + 1) == Some(&b'(')) {
                    return None;
                }
                i += 1;
                continue;
            }
            Quote::None => {}
        }
        match byte {
            b'\\' => {
                escaped = true;
                i += 1;
            }
            b'\'' => {
                quote = Quote::Single;
                i += 1;
            }
            b'"' => {
                quote = Quote::Double;
                i += 1;
            }
            b'`' | b'{' | b'}' | b'<' | b'>' => return None,
            b'$' if bytes.get(i + 1) == Some(&b'(') => return None,
            b'&' if bytes.get(i + 1) == Some(&b'&') => {
                parts.push(command.get(start..i)?.trim().to_owned());
                i += 2;
                start = i;
            }
            b'|' if bytes.get(i + 1) == Some(&b'|') => {
                parts.push(command.get(start..i)?.trim().to_owned());
                i += 2;
                start = i;
            }
            b'|' | b';' | b'\n' => {
                parts.push(command.get(start..i)?.trim().to_owned());
                i += 1;
                start = i;
            }
            _ => {
                i += 1;
            }
        }
    }
    if quote != Quote::None {
        return None;
    }
    parts.push(command.get(start..)?.trim().to_owned());
    if parts.iter().any(String::is_empty) {
        return None;
    }
    Some(parts)
}

/// Every exact bash-permission pattern that replays one denied command,
/// split at opencode's own top-level granularity — or the reason it cannot
/// be granted that way, naming the exact command so the operator can decline
/// it instead (`--skip` or `--response "<text>"`) when this refuses.
///
/// # Errors
/// Returns [`split_top_level_commands`]'s refusal reason, or the first
/// [`exact_bash_pattern`] refusal among the split parts.
pub(crate) fn exact_bash_patterns(command: &str) -> Result<Vec<String>, String> {
    let parts = split_top_level_commands(command).ok_or_else(|| {
        format!(
            "the denied command cannot be split into opencode's own sub-commands with confidence \
             (quoting, command substitution, brace syntax, or a redirection makes it ambiguous): \
             {command}"
        )
    })?;
    parts.iter().map(|part| exact_bash_pattern(part)).collect()
}

pub(crate) fn initial(
    prompt: &str,
    _stage_kind: StageKind,
    model: Option<&ModelId>,
    effort: EffortSetting,
    workspace: &Path,
    config_path: &Path,
    permission: &Value,
) -> OpencodeCommand {
    let mut argv = base(model, effort, workspace);
    argv.push(OsString::from(prompt));
    OpencodeCommand {
        argv,
        environment: environment(config_path, permission),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one native command builder, one parameter per argv/env piece it composes"
)]
pub(crate) fn resume(
    session_id: &ProviderSessionId,
    prompt: &str,
    _stage_kind: StageKind,
    model: Option<&ModelId>,
    effort: EffortSetting,
    workspace: &Path,
    config_path: &Path,
    permission: &Value,
) -> OpencodeCommand {
    let mut argv = base(model, effort, workspace);
    argv.push(OsString::from("--session"));
    argv.push(OsString::from(session_id.as_str()));
    argv.push(OsString::from(prompt));
    OpencodeCommand {
        argv,
        environment: environment(config_path, permission),
    }
}

fn base(model: Option<&ModelId>, effort: EffortSetting, workspace: &Path) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("run"),
        OsString::from("--format"),
        OsString::from("json"),
        // Repository/user plugin code is arbitrary executable code, the same
        // class of risk as the config-override vulnerability below; opencode's
        // own built-in plugins are unaffected.
        OsString::from("--pure"),
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

/// `OPENCODE_PERMISSION` is the actual authority (verified to win a merge
/// over whatever `OPENCODE_CONFIG`/project config resolved to);
/// `OPENCODE_DISABLE_PROJECT_CONFIG` additionally keeps a repository's own
/// `opencode.json`/`.opencode/opencode.json` from ever loading at all.
/// `OPENCODE_CONFIG` stays too, kept for a human reading the run's own
/// artifacts, not relied on for enforcement.
fn environment(config_path: &Path, permission: &Value) -> BTreeMap<OsString, OsString> {
    BTreeMap::from([
        (
            OsString::from("OPENCODE_CONFIG"),
            config_path.as_os_str().to_owned(),
        ),
        (
            OsString::from("OPENCODE_DISABLE_PROJECT_CONFIG"),
            OsString::from("1"),
        ),
        (
            OsString::from("OPENCODE_PERMISSION"),
            OsString::from(permission.to_string()),
        ),
    ])
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

    fn sample_permission() -> Value {
        json!({"edit": "deny", "bash": "deny", "webfetch": "deny", "external_directory": "deny"})
    }

    #[test]
    fn native_default_omits_variant_byte_identical() {
        let command = initial(
            "prompt",
            StageKind::Review,
            None,
            EffortSetting::NativeDefault,
            Path::new("/managed/worktree"),
            Path::new("/private/config.json"),
            &sample_permission(),
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
                &sample_permission(),
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
            &sample_permission(),
        );
        assert_eq!(command.argv.last().map(|arg| arg == marker), Some(true));
        assert!(command.environment.values().all(|value| value != marker));
    }

    #[test]
    fn every_invocation_carries_pure_and_the_two_config_override_defenses() {
        for command in [
            initial(
                "prompt",
                StageKind::Implementation,
                None,
                EffortSetting::NativeDefault,
                Path::new("/managed/worktree"),
                Path::new("/private/config.json"),
                &sample_permission(),
            ),
            resume(
                &ProviderSessionId::new("ses_A").unwrap(),
                "continue",
                StageKind::Implementation,
                None,
                EffortSetting::NativeDefault,
                Path::new("/managed/worktree"),
                Path::new("/private/config.json"),
                &sample_permission(),
            ),
        ] {
            assert!(
                strings(&command.argv).iter().any(|arg| arg == "--pure"),
                "{:?}",
                command.argv
            );
            assert_eq!(
                command
                    .environment
                    .get(&OsString::from("OPENCODE_DISABLE_PROJECT_CONFIG")),
                Some(&OsString::from("1"))
            );
            let permission = command
                .environment
                .get(&OsString::from("OPENCODE_PERMISSION"))
                .expect("OPENCODE_PERMISSION must be set");
            let decoded: Value =
                serde_json::from_str(&permission.to_string_lossy()).expect("valid JSON");
            assert_eq!(decoded, sample_permission());
        }
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
            &sample_permission(),
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
            &sample_permission(),
        );
        let args = strings(&command.argv);
        assert!(args.windows(2).any(|pair| pair == ["--session", "ses_A"]));
        assert!(!args.iter().any(|arg| arg == "--auto"));
    }

    #[test]
    fn the_config_path_reaches_the_environment_beside_the_permission_override() {
        let command = initial(
            "prompt",
            StageKind::Implementation,
            None,
            EffortSetting::NativeDefault,
            Path::new("/managed/worktree"),
            Path::new("/private/run/config.json"),
            &sample_permission(),
        );
        assert_eq!(command.environment.len(), 3);
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

    /// Real end-to-end shape: the denied command from a run's own evidence.
    #[test]
    fn exact_bash_pattern_accepts_a_plain_command_verbatim() {
        let command = "python3 -c \"from calc import add; assert add(2,3)==5; print('ok')\"";
        assert_eq!(exact_bash_pattern(command).as_deref(), Ok(command));
    }

    #[test]
    fn exact_bash_pattern_refuses_glob_syntax_that_would_widen_the_grant() {
        for command in [
            "rm *.txt",
            "cat file?.log",
            "ls [ab]*",
            "echo {a,b}",
            "echo \\x",
        ] {
            assert!(
                exact_bash_pattern(command).is_err(),
                "{command} must be refused"
            );
        }
        assert!(exact_bash_pattern("").is_err());
        assert!(exact_bash_pattern("   ").is_err());
    }

    /// Real end-to-end shape: `pwd && ls -la` was auto-rejected because
    /// opencode checked `pwd` and `ls -la` separately (its own stderr named
    /// both: `permission requested: bash (pwd, ls -la); auto-rejecting`).
    /// Granting the whole joined string never took effect; splitting it the
    /// way opencode does does.
    #[test]
    fn split_top_level_commands_matches_opencodes_own_split_of_a_real_denied_command() {
        assert_eq!(
            split_top_level_commands("pwd && ls -la"),
            Some(vec!["pwd".to_owned(), "ls -la".to_owned()])
        );
    }

    #[test]
    fn split_top_level_commands_handles_every_top_level_separator() {
        assert_eq!(
            split_top_level_commands("a || b; c | d\ne"),
            Some(vec![
                "a".to_owned(),
                "b".to_owned(),
                "c".to_owned(),
                "d".to_owned(),
                "e".to_owned()
            ])
        );
    }

    #[test]
    fn split_top_level_commands_never_splits_inside_quotes() {
        assert_eq!(
            split_top_level_commands("echo 'a && b' && echo \"c ; d\""),
            Some(vec![
                "echo 'a && b'".to_owned(),
                "echo \"c ; d\"".to_owned()
            ])
        );
    }

    #[test]
    fn split_top_level_commands_fails_closed_on_ambiguous_shell() {
        for command in [
            "echo `whoami`",
            "echo $(whoami)",
            "echo 'unterminated",
            "echo \"unterminated",
            "{ echo a; }",
            "echo a > out.txt",
            "cat < in.txt",
            "echo a &&",
            "&& echo a",
        ] {
            assert_eq!(
                split_top_level_commands(command),
                None,
                "{command} must be refused as ambiguous"
            );
        }
    }

    /// Command substitution and a bare redirection stay refused even inside
    /// double quotes, where the shell still expands/honours them.
    #[test]
    fn split_top_level_commands_refuses_substitution_inside_double_quotes() {
        assert_eq!(split_top_level_commands("echo \"$(whoami)\""), None);
        assert_eq!(split_top_level_commands("echo \"`whoami`\""), None);
    }

    #[test]
    fn exact_bash_patterns_grants_each_split_part_of_a_compound_command() {
        assert_eq!(
            exact_bash_patterns("pwd && ls -la").unwrap(),
            vec!["pwd".to_owned(), "ls -la".to_owned()]
        );
    }

    #[test]
    fn exact_bash_patterns_refuses_with_a_clear_message_when_uncertain() {
        let error = exact_bash_patterns("echo $(whoami)").unwrap_err();
        assert!(error.contains("cannot be split"), "{error}");
        assert!(error.contains("echo $(whoami)"), "{error}");
    }

    fn strings(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }
}
