use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::domain::{EffortLevel, EffortSetting, ModelId, ProviderSessionId, StageKind};
use crate::image::{ImageToolServerCommand, MCP_SERVER_NAME, TOOL_NAME};

/// Codex tool timeout for the image server. Generation can take minutes;
/// the native default of 60 seconds would fail a healthy call.
const IMAGE_TOOL_TIMEOUT_SEC: u32 = 300;

/// Root-level `-c` overrides that register the run-scoped MCP server for
/// this invocation only: nothing is written to `~/.codex/config.toml`, and
/// the user's own servers stay configured. Values are TOML basic strings and
/// arrays, encoded through JSON string escaping, which TOML accepts.
pub(crate) fn mcp_overrides(image: &ImageToolServerCommand) -> Vec<OsString> {
    let command =
        serde_json::to_string(&image.executable.to_string_lossy()).expect("string encodes");
    let args = serde_json::to_string(
        &image
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
    )
    .expect("string list encodes");
    vec![
        OsString::from("-c"),
        OsString::from(format!("mcp_servers.{MCP_SERVER_NAME}.command={command}")),
        OsString::from("-c"),
        OsString::from(format!("mcp_servers.{MCP_SERVER_NAME}.args={args}")),
        OsString::from("-c"),
        OsString::from(format!(
            "mcp_servers.{MCP_SERVER_NAME}.tool_timeout_sec={IMAGE_TOOL_TIMEOUT_SEC}"
        )),
        // Under `--ask-for-approval never` an MCP tool call is refused unless
        // the tool is pre-approved; this is the exact-tool equivalent of the
        // Claude `--allowedTools` rule, scoped to this one tool.
        OsString::from("-c"),
        OsString::from(format!(
            "mcp_servers.{MCP_SERVER_NAME}.tools.{TOOL_NAME}.approval_mode=\"approve\""
        )),
    ]
}

pub(crate) struct CodexCommand {
    pub argv: Vec<OsString>,
    pub stdin: Vec<u8>,
    pub final_message_path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodexSandbox {
    ReadOnly,
    WorkspaceWrite,
}

impl CodexSandbox {
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

    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }
}

pub(crate) fn initial(
    prompt: &str,
    stage_kind: StageKind,
    model: Option<&ModelId>,
    effort: EffortSetting,
    final_message_path: &Path,
    image: Option<&ImageToolServerCommand>,
) -> CodexCommand {
    let mut argv = base(stage_kind, model, effort, final_message_path, image);
    argv.push(OsString::from("-"));
    CodexCommand {
        argv,
        stdin: prompt.as_bytes().to_vec(),
        final_message_path: final_message_path.to_path_buf(),
    }
}

pub(crate) fn resume(
    session_id: &ProviderSessionId,
    prompt: &str,
    stage_kind: StageKind,
    model: Option<&ModelId>,
    effort: EffortSetting,
    final_message_path: &Path,
    image: Option<&ImageToolServerCommand>,
) -> CodexCommand {
    let mut argv = base(stage_kind, model, effort, final_message_path, image);
    argv.push(OsString::from("resume"));
    argv.push(OsString::from(session_id.as_str()));
    argv.push(OsString::from("-"));
    CodexCommand {
        argv,
        stdin: prompt.as_bytes().to_vec(),
        final_message_path: final_message_path.to_path_buf(),
    }
}

fn base(
    stage_kind: StageKind,
    model: Option<&ModelId>,
    effort: EffortSetting,
    final_message_path: &Path,
    image: Option<&ImageToolServerCommand>,
) -> Vec<OsString> {
    let mut argv = Vec::new();
    if let Some(image) = image {
        argv.extend(mcp_overrides(image));
    }
    if let Some(model) = model {
        argv.push(OsString::from("--model"));
        argv.push(OsString::from(model.as_str()));
    }
    // Adapter-owned mapping onto the native supported `model_reasoning_effort`
    // configuration override. NativeDefault omits the override entirely so
    // ~/.codex/config.toml keeps deciding.
    if let EffortSetting::Level(level) = effort {
        argv.push(OsString::from("-c"));
        argv.push(OsString::from(format!(
            "model_reasoning_effort=\"{}\"",
            native_effort_value(level)
        )));
    }
    argv.extend([
        OsString::from("--sandbox"),
        OsString::from(CodexSandbox::for_stage(stage_kind).as_str()),
        OsString::from("--ask-for-approval"),
        OsString::from("never"),
        OsString::from("exec"),
        OsString::from("--json"),
        OsString::from("--color"),
        OsString::from("never"),
        OsString::from("--output-last-message"),
        final_message_path.as_os_str().to_owned(),
    ]);
    argv
}

/// Native Codex value for one explicit requested level.
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
    fn native_default_omits_reasoning_override_byte_identical() {
        let command = initial(
            "prompt",
            StageKind::Review,
            None,
            EffortSetting::NativeDefault,
            Path::new("/private/final.md"),
            None,
        );
        assert!(
            !command
                .argv
                .iter()
                .any(|arg| arg.to_string_lossy().contains("model_reasoning_effort"))
        );
        assert!(!command.argv.iter().any(|arg| arg == "-c"));
    }

    #[test]
    fn explicit_effort_maps_onto_model_reasoning_effort_override() {
        for (setting, expected) in [
            (EffortSetting::LOW, "model_reasoning_effort=\"low\""),
            (EffortSetting::MEDIUM, "model_reasoning_effort=\"medium\""),
            (EffortSetting::HIGH, "model_reasoning_effort=\"high\""),
            (EffortSetting::XHIGH, "model_reasoning_effort=\"xhigh\""),
        ] {
            for command in [
                initial(
                    "prompt",
                    StageKind::Review,
                    None,
                    setting,
                    Path::new("/private/final.md"),
                    None,
                ),
                resume(
                    &ProviderSessionId::new("thread-1").unwrap(),
                    "continue",
                    StageKind::Review,
                    None,
                    setting,
                    Path::new("/private/final.md"),
                    None,
                ),
            ] {
                let args = strings(&command.argv);
                assert!(
                    args.windows(2)
                        .any(|pair| pair[0] == "-c" && pair[1] == expected),
                    "{setting:?} must produce -c {expected}"
                );
                // Root-level override must precede the exec subcommand.
                let c_index = args.iter().position(|arg| arg == "-c").unwrap();
                let exec_index = args.iter().position(|arg| arg == "exec").unwrap();
                assert!(c_index < exec_index);
            }
        }
    }

    #[test]
    fn initial_prompt_is_stdin_and_policy_is_explicit() {
        let marker = "SUPER_SECRET_TASK_MARKER";
        let command = initial(
            marker,
            StageKind::Review,
            None,
            EffortSetting::NativeDefault,
            Path::new("/private/final.md"),
            None,
        );
        let args = strings(&command.argv);
        assert_eq!(args.last().map(String::as_str), Some("-"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "read-only"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ask-for-approval", "never"])
        );
        assert!(args.iter().any(|arg| arg == "--json"));
        assert!(!args.iter().any(|arg| arg.contains(marker)));
        assert_eq!(command.stdin, marker.as_bytes());
        assert_safe(&args);
    }

    #[test]
    fn implementation_uses_workspace_write_and_explicit_model() {
        let command = initial(
            "task",
            StageKind::Implementation,
            Some(&ModelId::new("configured-model").unwrap()),
            EffortSetting::NativeDefault,
            Path::new("/private/final.md"),
            None,
        );
        let args = strings(&command.argv);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "workspace-write"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "configured-model"])
        );
        assert_safe(&args);
    }

    #[test]
    fn specialized_review_stages_are_read_only() {
        for kind in [StageKind::CodeQualityReview, StageKind::SpecReview] {
            let command = initial(
                "review",
                kind,
                None,
                EffortSetting::NativeDefault,
                Path::new("/private/final.md"),
                None,
            );
            let args = strings(&command.argv);
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--sandbox", "read-only"])
            );
            assert_safe(&args);
        }
    }

    #[test]
    fn resume_targets_exact_native_session_without_last() {
        let command = resume(
            &ProviderSessionId::new("thread-A").unwrap(),
            "continue",
            StageKind::Fix,
            None,
            EffortSetting::NativeDefault,
            Path::new("/private/final.md"),
            None,
        );
        let args = strings(&command.argv);
        assert!(
            args.windows(3)
                .any(|part| part == ["resume", "thread-A", "-"])
        );
        assert!(!args.iter().any(|arg| arg == "--last"));
        assert!(!args.iter().any(|arg| arg == "--model"));
        assert_safe(&args);
    }

    fn strings(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn assert_safe(args: &[String]) {
        for forbidden in [
            "--yolo",
            "--dangerously-bypass-approvals-and-sandbox",
            "danger-full-access",
            "--ephemeral",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
            "--dangerously-bypass-hook-trust",
        ] {
            assert!(!args.iter().any(|arg| arg == forbidden));
        }
    }

    fn image_command() -> ImageToolServerCommand {
        ImageToolServerCommand {
            executable: PathBuf::from("/opt/senate/bin/senate"),
            args: vec![
                OsString::from("__image-tool"),
                OsString::from("--socket"),
                OsString::from("/tmp/pcimg-run.sock"),
            ],
        }
    }

    #[test]
    fn image_grant_registers_the_server_through_root_overrides_only() {
        let baseline = initial(
            "prompt",
            StageKind::Implementation,
            None,
            EffortSetting::NativeDefault,
            Path::new("/private/final.md"),
            None,
        );
        let granted = initial(
            "prompt",
            StageKind::Implementation,
            None,
            EffortSetting::NativeDefault,
            Path::new("/private/final.md"),
            Some(&image_command()),
        );
        assert_eq!(granted.stdin, baseline.stdin);
        let args = strings(&granted.argv);
        let overrides = args
            .windows(2)
            .filter(|pair| pair[0] == "-c")
            .map(|pair| pair[1].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            overrides,
            vec![
                "mcp_servers.senate_image.command=\"/opt/senate/bin/senate\"",
                "mcp_servers.senate_image.args=[\"__image-tool\",\"--socket\",\"/tmp/pcimg-run.sock\"]",
                "mcp_servers.senate_image.tool_timeout_sec=300",
                "mcp_servers.senate_image.tools.image_generate.approval_mode=\"approve\"",
            ]
        );
        assert!(
            !overrides.iter().any(|value| value.contains(".env")),
            "no environment is handed to the server"
        );
        // Root-level overrides precede the exec subcommand, like effort does.
        let last_c = args.iter().rposition(|arg| arg == "-c").unwrap();
        let exec_index = args.iter().position(|arg| arg == "exec").unwrap();
        assert!(last_c < exec_index);
        // Sandbox and approval policy are untouched by the grant.
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "workspace-write"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ask-for-approval", "never"])
        );
        assert_safe(&args);
        // Without a grant the argv is byte-identical to before the tool existed.
        assert!(
            !strings(&baseline.argv)
                .iter()
                .any(|arg| arg.contains("mcp_servers"))
        );
    }

    #[test]
    fn image_grant_carries_no_credential_anywhere_in_the_invocation() {
        // Same reasoning as the Claude builder: no credential enters this
        // function, so none can come out of it.
        let command = resume(
            &ProviderSessionId::new("thread-1").unwrap(),
            "continue",
            StageKind::Implementation,
            None,
            EffortSetting::HIGH,
            Path::new("/private/final.md"),
            Some(&image_command()),
        );
        let joined = strings(&command.argv).join("\n");
        assert!(!joined.contains("OPENAI"), "{joined}");
        assert!(!joined.contains(".env"), "{joined}");
        assert!(!joined.contains("sk-proj"), "{joined}");
        assert_eq!(command.stdin, b"continue");
    }
}
