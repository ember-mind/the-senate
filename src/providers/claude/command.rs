use std::collections::BTreeSet;
use std::ffi::OsString;

use crate::domain::{EffortLevel, EffortSetting, ModelId, ProviderSessionId};
use crate::image::{ImageToolServerCommand, MCP_SERVER_NAME, TOOL_NAME};

use super::{ClaudeProviderError, PermissionDenial};

/// The exact permission rule for the image tool under Claude Code's
/// `mcp__<server>__<tool>` naming.
pub(crate) fn image_tool_rule() -> String {
    format!("mcp__{MCP_SERVER_NAME}__{TOOL_NAME}")
}

/// The `--mcp-config` payload: one stdio server, the Senate executable as
/// shim, the run's socket path. No environment block, so nothing here can
/// carry a credential.
pub(crate) fn mcp_config_json(image: &ImageToolServerCommand) -> String {
    serde_json::json!({
        "mcpServers": {
            MCP_SERVER_NAME: {
                "command": image.executable.to_string_lossy(),
                "args": image.args.iter().map(|arg| arg.to_string_lossy()).collect::<Vec<_>>(),
            }
        }
    })
    .to_string()
}

#[derive(Debug)]
pub(crate) struct ClaudeCommand {
    pub argv: Vec<OsString>,
    pub stdin: Vec<u8>,
}

pub(crate) fn initial(
    prompt: &str,
    allow: &BTreeSet<String>,
    model: Option<&ModelId>,
    effort: EffortSetting,
    image: Option<&ImageToolServerCommand>,
) -> ClaudeCommand {
    let mut argv = base(model, effort, image);
    let mut rules = allow.clone();
    if image.is_some() {
        rules.insert(image_tool_rule());
    }
    push_allowed_tools(&mut argv, rules);
    ClaudeCommand {
        argv,
        stdin: prompt.as_bytes().to_vec(),
    }
}

/// Builds the command that continues an existing native session.
///
/// Never refuses except on an unanswered question. Whatever *can* be granted
/// as an exact rule is granted; whatever cannot is simply not granted, and the
/// session is told so in the prompt. [`grant_rules`] is the gate that decides
/// whether an approval is worth committing, and it runs at resolution time,
/// before anything is persisted. Refusing here as well would mean a session
/// already committed to an unreplayable approval — or an interrupted session
/// carrying no denials at all — can never build a resume command: every later
/// drive fails on it, and the stage sits `running` for good with no process
/// behind it, reachable only by a retry that throws the work away.
///
/// # Errors
/// `QuestionResponseRequired` when a pending question has no response: a
/// question resumed without its answer is the one continuation that cannot
/// mean anything.
pub(crate) fn resume(
    session_id: &ProviderSessionId,
    denials: &[PermissionDenial],
    response: Option<&str>,
    allow: &BTreeSet<String>,
    model: Option<&ModelId>,
    effort: EffortSetting,
    image: Option<&ImageToolServerCommand>,
) -> Result<ClaudeCommand, ClaudeProviderError> {
    let response = response.filter(|response| !response.trim().is_empty());
    if response.is_none()
        && denials
            .iter()
            .any(|denial| denial.tool_name == "AskUserQuestion")
    {
        return Err(ClaudeProviderError::QuestionResponseRequired);
    }
    let mut rules = BTreeSet::new();
    let mut ungranted = false;
    // An operator answer is not an approval. Skipping ("do it without that")
    // and answering ("do X instead") both continue the session and grant
    // nothing: turning the denials into rules anyway would hand the agent
    // the exact permission the operator just declined.
    if response.is_none() {
        for denial in denials
            .iter()
            .filter(|denial| denial.tool_name != "AskUserQuestion")
        {
            match denial.exact_rules() {
                Ok(granted) => rules.extend(granted),
                Err(_) => ungranted = true,
            }
        }
    }
    // What this approval itself granted, before the stage's standing rules
    // are folded in: the prompt below has to describe *this* request, not the
    // permissions the stage already held.
    let granted_now = !rules.is_empty();
    rules.extend(allow.iter().cloned());
    // The image grant rides the same flag as approved denials: still one
    // exact rule, still only when the stage holds the grant.
    if image.is_some() {
        rules.insert(image_tool_rule());
    }
    let mut argv = base(model, effort, image);
    argv.push(OsString::from("--resume"));
    argv.push(OsString::from(session_id.as_str()));
    push_allowed_tools(&mut argv, rules);
    let stdin = match response {
        Some(response) => response,
        None if ungranted && granted_now => PARTIAL_RESUME_PROMPT,
        None if ungranted => UNGRANTED_RESUME_PROMPT,
        None if denials.is_empty() => CONTINUE_RESUME_PROMPT,
        None => APPROVED_RESUME_PROMPT,
    };
    Ok(ClaudeCommand {
        argv,
        stdin: stdin.as_bytes().to_vec(),
    })
}

/// The text a resumed session is continued on when nothing was pending: a
/// stopped or interrupted stage picking up where it left off.
pub(crate) const CONTINUE_RESUME_PROMPT: &str =
    "Continue the same task and session where it stopped.";

/// The text a resumed session is continued on when the pending request was
/// granted exactly.
pub(crate) const APPROVED_RESUME_PROMPT: &str =
    "User approved the exact pending permission request. Continue the same task and session.";

/// The text a resumed session is continued on when a pending request was
/// closed but cannot be replayed as any exact rule, so nothing was granted.
pub(crate) const UNGRANTED_RESUME_PROMPT: &str = concat!(
    "A pending permission request was closed without granting anything, and it will not be ",
    "granted: the tool call cannot be expressed as an exact permission rule. Do not retry that ",
    "call. Continue the same task and session with the tools you already have, or stop and ",
    "report what you could not do.",
);

/// The text a resumed session is continued on when the approval covered some
/// of the pending requests but not all of them: the session must not read a
/// partial grant as permission to retry the part that was refused.
pub(crate) const PARTIAL_RESUME_PROMPT: &str = concat!(
    "User approved the pending permission requests, and the ones that can be expressed as exact ",
    "permission rules were granted. At least one could not be, and was not granted. Do not retry ",
    "that call. Continue the same task and session with the tools you now have, and report what ",
    "you could not do.",
);

fn push_allowed_tools(argv: &mut Vec<OsString>, rules: BTreeSet<String>) {
    if rules.is_empty() {
        return;
    }
    argv.push(OsString::from("--allowedTools"));
    argv.extend(rules.into_iter().map(OsString::from));
}

/// Exact `--allowedTools` rules that replay one approved attention, or the
/// reason it cannot be replayed at all.
///
/// This is the single gate for "can this approval be honoured": resolution
/// must run it *before* committing, so an unreplayable denial — shell no
/// parser will read exactly, an unknown tool — is refused to the user instead
/// of committing a resolution every later drive fails on.
///
/// An operator response is also an answer to a permission request: when no
/// denial can be granted exactly, a non-empty response still resumes the
/// session with that text and grants nothing, so the operator can say
/// "skip it, do X instead" rather than only stop the run.
///
/// # Errors
/// `QuestionResponseRequired` when a pending question has no response;
/// `UnsafePermission` when no denial can be granted as an exact rule and
/// there is no response to continue on.
pub(crate) fn grant_rules(
    denials: &[PermissionDenial],
    response: Option<&str>,
) -> Result<BTreeSet<String>, ClaudeProviderError> {
    let has_question = denials
        .iter()
        .any(|denial| denial.tool_name == "AskUserQuestion");
    if has_question && response.is_none_or(|response| response.trim().is_empty()) {
        return Err(ClaudeProviderError::QuestionResponseRequired);
    }
    let mut rules = BTreeSet::new();
    let mut unsafe_permission = None;
    for denial in denials
        .iter()
        .filter(|denial| denial.tool_name != "AskUserQuestion")
    {
        match denial.exact_rules() {
            Ok(granted) => rules.extend(granted),
            // The first ungrantable denial, not the last: the operator reads
            // the pending requests in order, and a refusal has to name the
            // one at the top of that list rather than whichever happened to
            // be scanned last.
            Err(error) => {
                unsafe_permission.get_or_insert(error);
            }
        }
    }
    let has_response = response.is_some_and(|response| !response.trim().is_empty());
    if rules.is_empty() && !has_question && !has_response {
        return Err(unsafe_permission.unwrap_or_else(|| {
            ClaudeProviderError::UnsafePermission("empty denial set".to_owned())
        }));
    }
    // With nothing grantable but an operator answer present, the session
    // resumes on that answer alone: "continue without it". Nothing is granted.
    Ok(rules)
}

fn base(
    model: Option<&ModelId>,
    effort: EffortSetting,
    image: Option<&ImageToolServerCommand>,
) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("-p"),
        OsString::from("--verbose"),
        OsString::from("--output-format"),
        OsString::from("stream-json"),
        OsString::from("--permission-mode"),
        OsString::from("dontAsk"),
    ];
    if let Some(model) = model {
        argv.push(OsString::from("--model"));
        argv.push(OsString::from(model.as_str()));
    }
    // Adapter-owned mapping onto the native supported `--effort` session flag
    // (Claude Code 2.x: low|medium|high|xhigh|max). NativeDefault omits the
    // flag entirely so native configuration keeps deciding.
    if let EffortSetting::Level(level) = effort {
        argv.push(OsString::from("--effort"));
        argv.push(OsString::from(native_effort_value(level)));
    }
    // Run-scoped MCP server: added to whatever the user configured, never
    // replacing it (`--strict-mcp-config` stays off), never written to
    // ~/.claude or the project.
    if let Some(image) = image {
        argv.push(OsString::from("--mcp-config"));
        argv.push(OsString::from(mcp_config_json(image)));
    }
    argv
}

/// Native Claude Code value for one explicit requested level.
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
    use serde_json::json;

    use super::*;

    #[test]
    fn native_default_argv_is_byte_identical_to_pre_effort_invocation() {
        let command = initial(
            "prompt",
            &BTreeSet::new(),
            None,
            EffortSetting::NativeDefault,
            None,
        );
        let args = command
            .argv
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                "-p",
                "--verbose",
                "--output-format",
                "stream-json",
                "--permission-mode",
                "dontAsk",
            ]
        );
    }

    #[test]
    fn effort_changes_only_argv_never_the_injected_prompt() {
        let prompt = "identical stage prompt with change map";
        let baseline = initial(
            prompt,
            &BTreeSet::new(),
            None,
            EffortSetting::NativeDefault,
            None,
        );
        for setting in [
            EffortSetting::LOW,
            EffortSetting::MEDIUM,
            EffortSetting::HIGH,
            EffortSetting::XHIGH,
        ] {
            let command = initial(prompt, &BTreeSet::new(), None, setting, None);
            assert_eq!(command.stdin, baseline.stdin);
            assert_ne!(command.argv, baseline.argv);
        }
    }

    #[test]
    fn explicit_effort_maps_onto_native_effort_flag_never_silently() {
        for (setting, native) in [
            (EffortSetting::LOW, "low"),
            (EffortSetting::MEDIUM, "medium"),
            (EffortSetting::HIGH, "high"),
            (EffortSetting::XHIGH, "xhigh"),
        ] {
            let command = initial("prompt", &BTreeSet::new(), None, setting, None);
            let args = command
                .argv
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert!(
                args.windows(2).any(|pair| pair == ["--effort", native]),
                "{setting:?} must produce --effort {native}"
            );
        }
        let resume_command = resume(
            &ProviderSessionId::new("session-9").unwrap(),
            &[PermissionDenial {
                tool_name: "Write".to_owned(),
                tool_use_id: None,
                tool_input: json!({"file_path":"/tmp/result.txt"}),
            }],
            None,
            &BTreeSet::new(),
            None,
            EffortSetting::HIGH,
            None,
        )
        .unwrap();
        let args = resume_command
            .argv
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["--effort", "high"]));
    }

    #[test]
    fn resume_uses_same_session_and_exact_write_target() {
        let command = resume(
            &ProviderSessionId::new("session-1").unwrap(),
            &[PermissionDenial {
                tool_name: "Write".to_owned(),
                tool_use_id: None,
                tool_input: json!({"file_path":"/tmp/result.txt"}),
            }],
            None,
            &BTreeSet::new(),
            None,
            EffortSetting::NativeDefault,
            None,
        )
        .unwrap();
        let args = command
            .argv
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--resume", "session-1"])
        );
        assert!(args.iter().any(|arg| arg == "Edit(//tmp/result.txt)"));
        assert_eq!(
            args.iter().filter(|arg| *arg == "--allowedTools").count(),
            1
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
    }

    #[test]
    fn unreplayable_permission_resumes_on_operator_response_without_granting() {
        let denials = vec![PermissionDenial {
            tool_name: "Bash".to_owned(),
            tool_input: json!({"command": "yarn install \"$(cat .yarnrc)\""}),
            tool_use_id: None,
        }];
        let session = ProviderSessionId::new("s").unwrap();
        let refused = grant_rules(&denials, None).unwrap_err().to_string();
        assert!(refused.contains("yarn install"), "{refused}");
        let command = resume(
            &session,
            &denials,
            Some("Skip the install; the tests will run in CI."),
            &BTreeSet::new(),
            None,
            EffortSetting::NativeDefault,
            None,
        )
        .unwrap();
        assert!(
            !command.argv.iter().any(|arg| arg == "--allowedTools"),
            "nothing granted: {:?}",
            command.argv
        );
        assert_eq!(
            command.stdin,
            b"Skip the install; the tests will run in CI."
        );
        assert!(
            grant_rules(&denials, Some("  ")).is_err(),
            "blank response is not an answer"
        );
    }

    /// A pending set holding one ungrantable Bash line no longer sinks the
    /// approval: the MCP calls beside it are granted, and the resume says
    /// part of the request was refused.
    #[test]
    fn mcp_denials_are_granted_beside_an_ungrantable_bash_line() {
        let denials = vec![
            PermissionDenial {
                tool_name: "Bash".to_owned(),
                tool_input: json!({"command": "cd src && cat -n b.ts; ls utils/test*"}),
                tool_use_id: None,
            },
            PermissionDenial {
                tool_name: "mcp__linear-server__get_issue".to_owned(),
                tool_input: json!({"id": "DOTSUP-99"}),
                tool_use_id: None,
            },
            PermissionDenial {
                tool_name: "mcp__linear-server__list_comments".to_owned(),
                tool_input: json!({"issueId": "DOTSUP-99"}),
                tool_use_id: None,
            },
        ];
        let rules = grant_rules(&denials, None).unwrap();
        assert_eq!(
            rules.iter().cloned().collect::<Vec<_>>(),
            vec![
                "mcp__linear-server__get_issue".to_owned(),
                "mcp__linear-server__list_comments".to_owned(),
            ],
        );
        let command = resume(
            &ProviderSessionId::new("s").unwrap(),
            &denials,
            None,
            &BTreeSet::new(),
            None,
            EffortSetting::NativeDefault,
            None,
        )
        .unwrap();
        assert_eq!(command.stdin, PARTIAL_RESUME_PROMPT.as_bytes());
    }

    /// When nothing is grantable, the refusal names the first pending
    /// request, not whichever was scanned last.
    #[test]
    fn refusal_names_the_first_ungrantable_request() {
        let denials = vec![
            PermissionDenial {
                tool_name: "Bash".to_owned(),
                tool_input: json!({"command": "echo `whoami`"}),
                tool_use_id: None,
            },
            PermissionDenial {
                tool_name: "Bash".to_owned(),
                tool_input: json!({"command": "rm -rf \"$(cat target)\""}),
                tool_use_id: None,
            },
        ];
        let refused = grant_rules(&denials, None).unwrap_err().to_string();
        assert!(refused.contains("echo `whoami`"), "{refused}");
    }

    #[test]
    fn interrupted_session_without_denials_still_builds_a_resume() {
        let command = resume(
            &ProviderSessionId::new("session-2").unwrap(),
            &[],
            None,
            &BTreeSet::new(),
            None,
            EffortSetting::NativeDefault,
            None,
        )
        .expect("a stopped session carries no denials and must still resume");
        assert_eq!(command.stdin, CONTINUE_RESUME_PROMPT.as_bytes());
        assert!(!command.argv.iter().any(|arg| arg == "--allowedTools"));
    }

    #[test]
    fn ungrantable_denial_resumes_granting_nothing_instead_of_stranding_the_stage() {
        let denials = vec![
            PermissionDenial {
                tool_name: "Bash".to_owned(),
                tool_use_id: None,
                tool_input: json!({"command": "yarn typecheck `date +%s`"}),
            },
            PermissionDenial {
                tool_name: "Write".to_owned(),
                tool_use_id: None,
                tool_input: json!({"file_path": "/tmp/result.txt"}),
            },
        ];
        // The resolution gate still refuses an unreplayable set, so nothing new
        // arrives here without an answer; a resolution committed before that
        // gate existed did, and it has to be able to move again.
        let command = resume(
            &ProviderSessionId::new("session-3").unwrap(),
            &denials,
            None,
            &BTreeSet::new(),
            None,
            EffortSetting::NativeDefault,
            None,
        )
        .unwrap();
        let args = command
            .argv
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|arg| arg == "Edit(//tmp/result.txt)"));
        assert!(
            !args.iter().any(|arg| arg.contains("yarn typecheck")),
            "shell the parser will not read is never granted: {args:?}"
        );
        // Part of the batch was granted, so the session must not be told that
        // nothing was — only that the refused call stays refused.
        assert_eq!(command.stdin, PARTIAL_RESUME_PROMPT.as_bytes());

        assert!(
            grant_rules(&denials[..1], None).is_err(),
            "the resolution gate still refuses what cannot be replayed"
        );
        let nothing_grantable = resume(
            &ProviderSessionId::new("session-4").unwrap(),
            &denials[..1],
            None,
            &BTreeSet::new(),
            None,
            EffortSetting::NativeDefault,
            None,
        )
        .expect("a session committed to an unreplayable approval must still resume");
        assert!(
            !nothing_grantable
                .argv
                .iter()
                .any(|arg| arg == "--allowedTools")
        );
        assert_eq!(nothing_grantable.stdin, UNGRANTED_RESUME_PROMPT.as_bytes());
    }

    #[test]
    fn repository_allowlist_reaches_both_commands_beside_derived_grants() {
        let allow = BTreeSet::from(["Bash(yarn jest:*)".to_owned()]);
        let initial_args = initial("prompt", &allow, None, EffortSetting::NativeDefault, None)
            .argv
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            initial_args
                .windows(2)
                .any(|pair| pair == ["--allowedTools", "Bash(yarn jest:*)"]),
            "{initial_args:?}"
        );

        let resume_args = resume(
            &ProviderSessionId::new("session-5").unwrap(),
            &[PermissionDenial {
                tool_name: "Write".to_owned(),
                tool_use_id: None,
                tool_input: json!({"file_path": "/tmp/result.txt"}),
            }],
            None,
            &allow,
            None,
            EffortSetting::NativeDefault,
            None,
        )
        .unwrap()
        .argv
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
        assert!(resume_args.iter().any(|arg| arg == "Bash(yarn jest:*)"));
        assert!(
            resume_args
                .iter()
                .any(|arg| arg == "Edit(//tmp/result.txt)")
        );
        assert_eq!(
            resume_args
                .iter()
                .filter(|arg| *arg == "--allowedTools")
                .count(),
            1
        );
    }

    #[test]
    fn question_requires_response() {
        let denial = PermissionDenial {
            tool_name: "AskUserQuestion".to_owned(),
            tool_use_id: None,
            tool_input: json!({"questions":[{"question":"Which option?"}]}),
        };
        assert!(matches!(
            resume(
                &ProviderSessionId::new("session-1").unwrap(),
                std::slice::from_ref(&denial),
                None,
                &BTreeSet::new(),
                None,
                EffortSetting::NativeDefault,
                None,
            ),
            Err(ClaudeProviderError::QuestionResponseRequired)
        ));
        assert!(
            resume(
                &ProviderSessionId::new("session-1").unwrap(),
                &[denial],
                Some("Option A"),
                &BTreeSet::new(),
                None,
                EffortSetting::NativeDefault,
                None,
            )
            .is_ok()
        );
    }

    fn image_command() -> ImageToolServerCommand {
        ImageToolServerCommand {
            executable: std::path::PathBuf::from("/opt/senate/bin/senate"),
            args: vec![
                OsString::from("__image-tool"),
                OsString::from("--socket"),
                OsString::from("/tmp/pcimg-run.sock"),
            ],
        }
    }

    #[test]
    fn image_grant_adds_run_scoped_mcp_server_and_exact_allow_rule_only() {
        let baseline = initial(
            "prompt",
            &BTreeSet::new(),
            None,
            EffortSetting::NativeDefault,
            None,
        );
        let granted = initial(
            "prompt",
            &BTreeSet::new(),
            None,
            EffortSetting::NativeDefault,
            Some(&image_command()),
        );
        assert_eq!(
            granted.stdin, baseline.stdin,
            "the grant never edits the prompt bytes here"
        );
        let args = granted
            .argv
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let config_index = args.iter().position(|arg| arg == "--mcp-config").unwrap();
        let config: serde_json::Value = serde_json::from_str(&args[config_index + 1]).unwrap();
        assert_eq!(
            config,
            json!({"mcpServers": {"senate_image": {
                "command": "/opt/senate/bin/senate",
                "args": ["__image-tool", "--socket", "/tmp/pcimg-run.sock"],
            }}})
        );
        assert!(
            config["mcpServers"]["senate_image"].get("env").is_none(),
            "the MCP config must carry no environment block"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--allowedTools", "mcp__senate_image__image_generate"])
        );
        assert!(!args.iter().any(|arg| arg == "--strict-mcp-config"));
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
        // Baseline argv is a strict prefix: nothing else moved.
        assert_eq!(&granted.argv[..baseline.argv.len()], &baseline.argv[..]);

        let resumed = resume(
            &ProviderSessionId::new("session-1").unwrap(),
            &[PermissionDenial {
                tool_name: "Write".to_owned(),
                tool_use_id: None,
                tool_input: json!({"file_path":"/tmp/result.txt"}),
            }],
            None,
            &BTreeSet::new(),
            None,
            EffortSetting::NativeDefault,
            Some(&image_command()),
        )
        .unwrap();
        let args = resumed
            .argv
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args.iter().filter(|arg| *arg == "--allowedTools").count(),
            1,
            "one flag carries both the approved rule and the image rule"
        );
        assert!(
            args.iter()
                .any(|arg| arg == "mcp__senate_image__image_generate")
        );
        assert!(args.iter().any(|arg| arg == "--mcp-config"));
    }

    #[test]
    fn image_grant_carries_no_credential_anywhere_in_the_invocation() {
        // The invocation is built from the grant alone: the builder takes
        // no credential and reads no environment, so the only way a key could
        // appear is through the grant, which carries an executable path and
        // a socket path and nothing else. `env` keys and `OPENAI` anywhere
        // in argv are the two shapes a leak would take.
        let command = initial(
            "prompt",
            &BTreeSet::new(),
            None,
            EffortSetting::HIGH,
            Some(&image_command()),
        );
        let joined = command
            .argv
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains("OPENAI"), "{joined}");
        assert!(!joined.contains("\"env\""), "{joined}");
        assert!(!joined.contains("sk-proj"), "{joined}");
        assert_eq!(command.stdin, b"prompt");
    }
}
