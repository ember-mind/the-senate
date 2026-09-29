//! Command-line, immutable stdin and permission configuration for opencode.
//!
//! Native opencode 1.18.33 reads piped input with `Bun.stdin.text()`. Stage
//! prompts therefore use the same run-private immutable stdin transport as
//! the other providers, never argv (which exposes prompts and has per-string
//! operating-system limits independent of `ARG_MAX`).
//!
//! `OPENCODE_PERMISSION` overrides global permissions, but not per-agent rules.
//! The adapter audits resolved native configuration before launch and refuses
//! agent/mode permission overrides. Both discovery and execution disable
//! project configuration and external plugins. Native global configuration
//! and per-vendor authentication remain available; configuration override
//! environment variables must not introduce a different configuration into
//! the audited and executing processes.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use serde_json::{Value, json};

use crate::domain::{EffortLevel, EffortSetting, ModelId, ProviderSessionId, StageKind};

/// Resource limit for composed context, not an argv or operating-system limit.
pub(crate) const MAX_PROMPT_BYTES: usize = 256 * 1024;

pub(crate) struct OpencodeCommand {
    pub argv: Vec<OsString>,
    pub environment: BTreeMap<OsString, OsString>,
    pub stdin: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpencodeSandbox {
    ReadOnly,
    WorkspaceWrite,
}

impl OpencodeSandbox {
    /// Mirrors Codex's stage-kind split, including simplification.
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

    /// Native permissions for one stage. The separate configuration audit
    /// rejects per-agent overrides, which would otherwise supersede these.
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

/// Converts the shared Claude `Bash(<prefix>:*)` allowlist to native glob rules.
/// Non-bash rules and unrecognized shapes are skipped, never guessed at.
/// This retains the existing Claude baseline, including find/sed/awk and its
/// associated shell-command risks; it is not an operating-system sandbox.
///
/// # Errors
/// Returns whatever reading the repository's permissions table returns.
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

/// An exact permission pattern for an already-atomic command.
///
/// # Errors
/// Refuses empty commands and glob/escape syntax that could widen a grant.
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

/// Splits denied commands at opencode's top-level permission granularity.
/// Refuses ambiguous shell syntax rather than guessing about a permission.
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

/// Exact native patterns for every top-level part of a denied command.
///
/// # Errors
/// Refuses commands whose splitting or glob semantics are uncertain.
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
    OpencodeCommand {
        argv: base(model, effort, workspace),
        environment: environment(config_path, permission),
        stdin: prompt.as_bytes().to_vec(),
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
    OpencodeCommand {
        argv,
        environment: environment(config_path, permission),
        stdin: prompt.as_bytes().to_vec(),
    }
}

fn base(model: Option<&ModelId>, effort: EffortSetting, workspace: &Path) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("run"),
        OsString::from("--format"),
        OsString::from("json"),
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

/// Shared by execution and all discovery probes. Ambient configuration
/// content/directories are cleared so they cannot reintroduce a project file
/// or produce different configurations in the neutral probe directory and
/// managed worktree. Global native config and vendor auth variables remain.
pub(super) fn environment(config_path: &Path, permission: &Value) -> BTreeMap<OsString, OsString> {
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
        (OsString::from("OPENCODE_CONFIG_DIR"), OsString::new()),
        (OsString::from("OPENCODE_CONFIG_CONTENT"), OsString::new()),
    ])
}

/// Native opencode variant for an explicit effort level.
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
    fn initial_and_resume_prompts_use_stdin_never_argv_or_environment() {
        let marker = "SUPER_SECRET_TASK_MARKER\nquotes: \" ' \\ and Unicode: è\n";
        for command in [
            initial(
                marker,
                StageKind::Review,
                None,
                EffortSetting::NativeDefault,
                Path::new("/managed/worktree"),
                Path::new("/private/config.json"),
                &sample_permission(),
            ),
            resume(
                &ProviderSessionId::new("ses_A").unwrap(),
                marker,
                StageKind::Review,
                None,
                EffortSetting::NativeDefault,
                Path::new("/managed/worktree"),
                Path::new("/private/config.json"),
                &sample_permission(),
            ),
        ] {
            assert_eq!(command.stdin, marker.as_bytes());
            assert!(command.argv.iter().all(|arg| arg != marker));
            assert!(command.environment.values().all(|value| value != marker));
        }
    }

    #[cfg(unix)]
    #[test]
    fn prompts_above_linux_single_argument_limit_launch_and_round_trip_through_stdin() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::process::{Command, Stdio};

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("opencode");
        std::fs::write(&executable, "#!/bin/sh\nexec /bin/cat\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let prompt = format!("{}\nquotes: \" ' \\ and Unicode: è\n", "x".repeat(150_000));
        let command = initial(
            &prompt,
            StageKind::Review,
            None,
            EffortSetting::NativeDefault,
            directory.path(),
            &directory.path().join("config.json"),
            &sample_permission(),
        );
        let stdin_path = directory.path().join("stdin");
        std::fs::write(&stdin_path, &command.stdin).unwrap();
        let output = Command::new(executable)
            .args(&command.argv)
            .envs(&command.environment)
            .stdin(Stdio::from(std::fs::File::open(stdin_path).unwrap()))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, prompt.as_bytes());
    }

    #[test]
    fn every_invocation_carries_pure_and_the_config_override_defenses() {
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
            assert!(strings(&command.argv).iter().any(|arg| arg == "--pure"));
            assert_eq!(
                command
                    .environment
                    .get(&OsString::from("OPENCODE_DISABLE_PROJECT_CONFIG")),
                Some(&OsString::from("1"))
            );
            let permission = command
                .environment
                .get(&OsString::from("OPENCODE_PERMISSION"))
                .unwrap();
            let decoded: Value = serde_json::from_str(&permission.to_string_lossy()).unwrap();
            assert_eq!(decoded, sample_permission());
            for name in ["OPENCODE_CONFIG_DIR", "OPENCODE_CONFIG_CONTENT"] {
                assert_eq!(
                    command.environment.get(&OsString::from(name)),
                    Some(&OsString::new())
                );
            }
        }
    }

    #[test]
    fn model_and_workspace_are_explicit_without_auto() {
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
        assert_eq!(command.environment.len(), 5);
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
                "permission": {"edit":"deny", "bash":"deny", "webfetch":"deny", "external_directory":"deny"}
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
        assert_eq!(patterns.get("grep *").map(String::as_str), Some("allow"));
        assert_eq!(
            patterns.get("git status *").map(String::as_str),
            Some("allow")
        );
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
