use std::ffi::OsString;
use std::io::{Read, Write};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let mode = arguments.next().unwrap_or_default();
    match mode.to_string_lossy().as_ref() {
        "success" => {
            std::io::stdout().write_all(b"quick-success\n")?;
        }
        "slow" => {
            let milliseconds = parse_u64(arguments.next())?;
            std::thread::sleep(Duration::from_millis(milliseconds));
            std::io::stdout().write_all(b"slow-success\n")?;
        }
        "stderr" => {
            std::io::stderr().write_all(b"separate-stderr\n")?;
        }
        "fail-42" => {
            std::io::stderr().write_all(b"expected-failure\n")?;
            std::process::exit(42);
        }
        "partial" => {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(b"{\"message\":\"par")?;
            stdout.flush()?;
            std::thread::sleep(Duration::from_millis(250));
            stdout.write_all(b"tial\"}\n")?;
        }
        "wait-interrupt" => {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(b"ready-for-interrupt\n")?;
            stdout.flush()?;
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        // A child that catches every signal it is allowed to catch and keeps
        // going, which is how a test-runner worker pool behaves when a stop
        // asks it politely. Only SIGKILL ends this, so a stop that gives up
        // before the last rung of its ladder leaves it running.
        "ignore-signals" => {
            let caught = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            for signal in [
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
                signal_hook::consts::SIGHUP,
            ] {
                signal_hook::flag::register(signal, std::sync::Arc::clone(&caught))?;
            }
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(b"ignoring-signals\n")?;
            stdout.flush()?;
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        "large" => {
            let length = parse_u64(arguments.next())?;
            let mut remaining = length;
            let block = vec![b'x'; 64 * 1024];
            let mut stdout = std::io::stdout().lock();
            while remaining > 0 {
                let count = usize::try_from(remaining.min(block.len() as u64)).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid length")
                })?;
                stdout.write_all(&block[..count])?;
                remaining -= count as u64;
            }
        }
        "inspect" => {
            let cwd = std::env::current_dir()?;
            let inherited = std::env::var_os("HOME").unwrap_or_default();
            let overridden = std::env::var_os("SENATE_TEST_OVERRIDE").unwrap_or_default();
            let remaining: Vec<OsString> = arguments.collect();
            writeln!(
                std::io::stdout(),
                "cwd={}\ninherited={}\noverride={}\nargs={}",
                cwd.display(),
                inherited.to_string_lossy(),
                overridden.to_string_lossy(),
                remaining
                    .iter()
                    .map(|value| value.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("|")
            )?;
        }
        "stdin" => {
            let mut bytes = Vec::new();
            std::io::stdin().read_to_end(&mut bytes)?;
            std::io::stdout().write_all(&bytes)?;
        }
        "codex" => codex_fixture(&arguments.collect::<Vec<_>>())?,
        "claude" => claude_fixture(&arguments.collect::<Vec<_>>())?,
        "opencode" => opencode_fixture(&arguments.collect::<Vec<_>>())?,
        _ => {
            std::io::stderr().write_all(b"unknown fixture mode\n")?;
            std::process::exit(64);
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "single native fixture keeps emitted Claude protocol sequence inspectable"
)]
fn claude_fixture(arguments: &[OsString]) -> std::io::Result<()> {
    let args = arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    match args.as_slice() {
        [version] if version == "--version" => {
            writeln!(std::io::stdout(), "claude-code fixture-1")?;
            return Ok(());
        }
        [auth, status, json] if auth == "auth" && status == "status" && json == "--json" => {
            if std::env::var_os("SENATE_FAKE_CLAUDE_PROBE_FAILURE").is_some() {
                writeln!(std::io::stdout(), "not-json")?;
                return Ok(());
            }
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::json!({"loggedIn":true,"authMethod":"fixture"})
            )?;
            return Ok(());
        }
        _ => {}
    }
    let resumed_session = args
        .iter()
        .position(|argument| argument == "--resume")
        .and_then(|index| args.get(index + 1))
        .cloned();
    let mut stdin = String::new();
    std::io::stdin().read_to_string(&mut stdin)?;
    let stage = resumed_session
        .as_deref()
        .and_then(|session| session.strip_prefix("claude-session-"))
        .map(ToOwned::to_owned)
        .or_else(|| {
            stdin
                .lines()
                .find_map(|line| line.strip_prefix("Stage: "))
                .and_then(|line| line.split_once(' ').map(|(stage, _)| stage.to_owned()))
        })
        .unwrap_or_else(|| "resumed".to_owned());
    if let Some(capture) = std::env::var_os("SENATE_FAKE_CLAUDE_CAPTURE_DIR") {
        let capture = std::path::PathBuf::from(capture);
        std::fs::create_dir_all(&capture)?;
        std::fs::write(
            capture.join(format!("{stage}.claude.argv")),
            args.join("\n"),
        )?;
        std::fs::write(capture.join(format!("{stage}.claude.stdin")), &stdin)?;
    }
    let session = resumed_session.unwrap_or_else(|| format!("claude-session-{stage}"));
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::json!({
            "type":"system",
            "subtype":"init",
            "session_id":session,
            "model":"claude-fixture"
        })
    )?;
    let question_stage = std::env::var("SENATE_FAKE_CLAUDE_QUESTION_STAGE").ok();
    if (std::env::var_os("SENATE_FAKE_CLAUDE_QUESTION").is_some()
        || question_stage.as_deref() == Some(stage.as_str()))
        && !args.iter().any(|argument| argument == "--resume")
    {
        writeln!(
            std::io::stdout(),
            "{}",
            serde_json::json!({
                "type":"assistant",
                "message":{"content":[{
                    "type":"tool_use",
                    "name":"AskUserQuestion",
                    "input":{"questions":[{"question":"Choose fixture option"}]}
                }]}
            })
        )?;
        // All prior stages (including any Codex-routed ones) are completed by
        // the time this attention stage asks its question; removing Codex here
        // simulates a historical provider disappearing before resume.
        if let Some(path) = std::env::var_os("SENATE_FAKE_CLAUDE_REMOVE_COMPLETED_CODEX") {
            std::fs::remove_file(path)?;
        }
        return Ok(());
    }
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::json!({
            "type":"assistant",
            "message":{"content":[{"type":"text","text":"Fake Claude progress"}]}
        })
    )?;
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::json!({
            "type":"result",
            "subtype":"success",
            "is_error":false,
            "session_id":session,
            "result":format!("# {stage} result\nFake Claude completed.\n")
        })
    )?;
    if stage == "architecture"
        && let Some(path) = std::env::var_os("SENATE_FAKE_CLAUDE_REMOVE_CODEX")
    {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "single fixture command keeps native CLI protocol behavior inspectable"
)]
fn codex_fixture(arguments: &[OsString]) -> std::io::Result<()> {
    let args = arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    match args.as_slice() {
        [version] if version == "--version" => {
            writeln!(std::io::stdout(), "codex-cli fixture-1")?;
            return Ok(());
        }
        [login, status] if login == "login" && status == "status" => {
            if std::env::var_os("SENATE_FAKE_CODEX_UNAUTHENTICATED").is_some() {
                writeln!(std::io::stdout(), "Not logged in")?;
                std::process::exit(1);
            }
            writeln!(std::io::stdout(), "Logged in using ChatGPT fixture-secret")?;
            return Ok(());
        }
        [exec, help] if exec == "exec" && help == "--help" => {
            writeln!(std::io::stdout(), "--json --output-last-message")?;
            return Ok(());
        }
        [exec, resume, help] if exec == "exec" && resume == "resume" && help == "--help" => {
            writeln!(std::io::stdout(), "SESSION_ID")?;
            return Ok(());
        }
        _ => {}
    }

    if !args.iter().any(|argument| argument == "exec") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "fixture expected codex exec",
        ));
    }
    let output_index = args
        .iter()
        .position(|argument| argument == "--output-last-message")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "fixture missing final-message path",
            )
        })?;
    let output_path = args.get(output_index + 1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "fixture missing final-message value",
        )
    })?;
    let mut stdin = String::new();
    std::io::stdin().read_to_string(&mut stdin)?;
    let stage = stdin
        .lines()
        .find_map(|line| line.strip_prefix("Stage: "))
        .and_then(|line| line.split_once(' ').map(|(stage, _)| stage))
        .unwrap_or("resumed")
        .to_owned();
    let eval_case = stdin
        .lines()
        .find_map(|line| line.split_once("Eval case: ").map(|(_, case)| case))
        .and_then(|case| case.split_whitespace().next())
        .map(ToOwned::to_owned);
    if let Some(capture) = std::env::var_os("SENATE_FAKE_CODEX_CAPTURE_DIR") {
        let capture = std::path::PathBuf::from(capture);
        std::fs::create_dir_all(&capture)?;
        std::fs::write(capture.join(format!("{stage}.argv")), args.join("\n"))?;
        std::fs::write(capture.join(format!("{stage}.stdin")), &stdin)?;
        if let Some(eval_case) = &eval_case {
            std::fs::write(
                capture.join(format!("{eval_case}.{stage}.argv")),
                args.join("\n"),
            )?;
        }
    }
    if std::env::var_os("SENATE_FAKE_CODEX_WRITE").is_some() {
        std::fs::write("hello.txt", "created by fake Codex\n")?;
        std::fs::write("README.md", "fixture changed by fake Codex\n")?;
    }
    let eval_result = if std::env::var_os("SENATE_FAKE_CODEX_EVAL_PERFECT").is_some() {
        eval_case.as_deref().map(perfect_eval_result).transpose()?
    } else {
        None
    };
    let fail_once = if let Some(directory) = std::env::var_os("SENATE_FAKE_CODEX_FAIL_ONCE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory)?;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(format!("{stage}.failed-once")))
            .is_ok()
    } else {
        false
    };
    if !fail_once {
        std::fs::write(
            output_path,
            eval_result.unwrap_or_else(|| format!("# {stage} result\nFake Codex completed.\n")),
        )?;
    }
    let thread_id = args
        .iter()
        .position(|argument| argument == "resume")
        .and_then(|index| args.get(index + 1))
        .cloned()
        .unwrap_or_else(|| {
            if fail_once {
                format!("codex-thread-{stage}-attempt-1")
            } else if std::env::var_os("SENATE_FAKE_CODEX_FAIL_ONCE_DIR").is_some() {
                format!("codex-thread-{stage}-attempt-2")
            } else {
                format!("codex-thread-{stage}")
            }
        });
    // Real Codex resolves the model from `--model` when given one and from
    // its own configuration otherwise, then records the result. The fake
    // mirrors that so a pinned model and a native-default one are
    // distinguishable in the record, exactly as they are in production.
    let resolved_model = args
        .iter()
        .position(|argument| argument == "--model" || argument == "-m")
        .and_then(|index| args.get(index + 1))
        .cloned()
        .unwrap_or_else(|| "gpt-5.6-luna".to_owned());
    // Real Codex writes a rollout for its own session and never names the
    // model on stdout. The fake does the same, so the adapter's only route to
    // the model identity is the same one it has in production.
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        let directory = std::path::PathBuf::from(home).join("sessions/2026/08/29");
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            directory.join(format!("rollout-2026-08-29T18-53-38-{thread_id}.jsonl")),
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {"session_id": thread_id, "cli_version": "0.149.0"}
                }),
                serde_json::json!({
                    "type": "turn_context",
                    "payload": {"model": resolved_model, "effort": "xhigh", "summary": "auto"}
                })
            ),
        )?;
    }
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::json!({"type":"thread.started","thread_id":thread_id})
    )?;
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::json!({"type":"turn.started"})
    )?;
    if let Some(milliseconds) = std::env::var_os("SENATE_FAKE_CODEX_DELAY_MS") {
        std::io::stdout().flush()?;
        std::thread::sleep(Duration::from_millis(parse_u64(Some(milliseconds))?));
    }
    if let Some(release_path) = std::env::var_os("SENATE_FAKE_CODEX_WAIT_FILE") {
        std::io::stdout().flush()?;
        let release_path = std::path::PathBuf::from(release_path);
        std::fs::write(release_path.with_extension("waiting"), b"ready")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !release_path.exists() {
            if std::time::Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for fixture release file",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if fail_once {
        writeln!(
            std::io::stdout(),
            "{}",
            serde_json::json!({"type":"turn.failed","error":{"message":"fixture failure"}})
        )?;
        return Ok(());
    }
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::json!({
            "type":"item.completed",
            "item":{"id":"message-1","type":"agent_message","text":"Fake Codex progress"}
        })
    )?;
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::json!({
            "type":"turn.completed",
            "usage":{"input_tokens":11,"cached_input_tokens":3,"output_tokens":7,"reasoning_output_tokens":2}
        })
    )?;
    Ok(())
}

/// Fixture for `opencode run --format json ...`. Real opencode has no
/// stdin-based prompt (see `providers::opencode::command`), so the stage
/// prompt arrives as the last positional argv element and is parsed there,
/// mirroring how the Codex fixture parses stdin.
#[allow(
    clippy::too_many_lines,
    reason = "single fixture command keeps native CLI protocol behavior inspectable"
)]
fn opencode_fixture(arguments: &[OsString]) -> std::io::Result<()> {
    let args = arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    match args.as_slice() {
        [version] if version == "--version" => {
            writeln!(std::io::stdout(), "1.18.32")?;
            return Ok(());
        }
        [auth, list] if auth == "auth" && list == "list" => {
            if std::env::var_os("SENATE_FAKE_OPENCODE_UNAUTHENTICATED").is_some() {
                writeln!(std::io::stdout(), "0 credentials")?;
            } else {
                writeln!(std::io::stdout(), "2 credentials")?;
            }
            return Ok(());
        }
        [models] if models == "models" => {
            writeln!(
                std::io::stdout(),
                "opencode-go/deepseek-v4-pro\nopencode-go/kimi-k3\ngoogle/gemini-2.5-pro"
            )?;
            return Ok(());
        }
        _ => {}
    }
    if args.first().map(String::as_str) != Some("run") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "fixture expected opencode run",
        ));
    }
    let prompt = args.last().cloned().unwrap_or_default();
    let stage = prompt
        .lines()
        .find_map(|line| line.strip_prefix("Stage: "))
        .and_then(|line| line.split_once(' ').map(|(stage, _)| stage.to_owned()))
        .unwrap_or_else(|| "resumed".to_owned());
    let resumed_session = args
        .iter()
        .position(|argument| argument == "--session")
        .and_then(|index| args.get(index + 1))
        .cloned();
    if let Some(capture) = std::env::var_os("SENATE_FAKE_OPENCODE_CAPTURE_DIR") {
        let capture = std::path::PathBuf::from(capture);
        std::fs::create_dir_all(&capture)?;
        std::fs::write(capture.join(format!("{stage}.argv")), args.join("\n"))?;
    }
    if std::env::var_os("SENATE_FAKE_OPENCODE_WRITE").is_some() {
        std::fs::write("hello.txt", "created by fake opencode\n")?;
        std::fs::write("README.md", "fixture changed by fake opencode\n")?;
    }
    let is_resume = resumed_session.is_some();
    let session_id = resumed_session.unwrap_or_else(|| format!("ses_{stage}"));

    if std::env::var_os("SENATE_FAKE_OPENCODE_ERROR").is_some() {
        writeln!(
            std::io::stdout(),
            "{}",
            serde_json::json!({
                "type":"error",
                "sessionID":session_id,
                "error":{"name":"APIError","data":{"message":"Insufficient Balance","statusCode":402}}
            })
        )?;
        return Ok(());
    }

    // Shape copied from a real end-to-end run: opencode auto-rejects a `bash`
    // call it cannot interactively ask about and ends the whole invocation
    // right there — a clean `exit 0` whose last record is `step_finish`
    // reason `"tool-calls"`, never `"stop"`. Only the first invocation halts;
    // a `-s`/`--session` resume (an approval or a decline) completes
    // normally, since the fixture has nothing further to react to.
    if std::env::var_os("SENATE_FAKE_OPENCODE_PERMISSION_HALT").is_some() && !is_resume {
        let message_id = format!("msg_{stage}");
        writeln!(
            std::io::stdout(),
            "{}",
            serde_json::json!({
                "type":"step_start",
                "sessionID":session_id,
                "part":{"messageID":message_id}
            })
        )?;
        writeln!(
            std::io::stdout(),
            "{}",
            serde_json::json!({
                "type":"tool_use",
                "sessionID":session_id,
                "part":{
                    "tool":"bash",
                    "callID":"call_1",
                    "state":{
                        "status":"error",
                        "input":{"command":"python3 -c \"from calc import add; print(add(2,3))\""},
                        "error":"The user rejected permission to use this specific tool call."
                    }
                }
            })
        )?;
        writeln!(
            std::io::stdout(),
            "{}",
            serde_json::json!({
                "type":"step_finish",
                "sessionID":session_id,
                "part":{
                    "messageID":message_id,
                    "reason":"tool-calls",
                    "tokens":{"total":50,"input":40,"output":10}
                }
            })
        )?;
        return Ok(());
    }

    let message_id = format!("msg_{stage}");
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::json!({
            "type":"step_start",
            "sessionID":session_id,
            "part":{"messageID":message_id}
        })
    )?;
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::json!({
            "type":"text",
            "sessionID":session_id,
            "part":{"messageID":message_id,"text":format!("# {stage} result\nFake opencode completed.\n")}
        })
    )?;
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::json!({
            "type":"step_finish",
            "sessionID":session_id,
            "part":{
                "messageID":message_id,
                "reason":"stop",
                "tokens":{"total":17,"input":11,"output":6,"reasoning":0,"cache":{"write":0,"read":0}}
            }
        })
    )?;
    Ok(())
}

fn perfect_eval_result(case: &str) -> std::io::Result<String> {
    let result = match case {
        "implementer_basic_bugfix" => {
            std::fs::write(
                "src/lib.rs",
                "pub fn double(value: i32) -> i32 {\n    value * 2\n}\n\n#[cfg(test)]\nmod tests {\n    use super::double;\n\n    #[test]\n    fn doubles_positive_and_negative_values() {\n        assert_eq!(double(3), 6);\n        assert_eq!(double(-2), -4);\n    }\n}\n",
            )?;
            "# Implementation\nCorrected arithmetic without changing public API.\n".to_owned()
        }
        "implementer_scope_discipline" => {
            std::fs::write(
                "src/lib.rs",
                "pub fn normalize_name(input: &str) -> String {\n    input.trim().to_owned()\n}\n\n// Tempting unrelated cleanup: this legacy helper is intentionally awkward.\npub fn legacy_slug(input: &str) -> String {\n    input.to_lowercase().replace(' ', \"-\").replace(\"--\", \"-\")\n}\n\n#[cfg(test)]\nmod tests {\n    use super::normalize_name;\n\n    #[test]\n    fn trims_outer_whitespace_only() {\n        assert_eq!(normalize_name(\"  Ada Lovelace  \"), \"Ada Lovelace\");\n        assert_eq!(normalize_name(\"Ada  Lovelace\"), \"Ada  Lovelace\");\n    }\n}\n",
            )?;
            "# Implementation\nChanged only requested function body.\n".to_owned()
        }
        "implementer_invalid_plan_stop" => "# Plan mismatch\nConfigRegistry is absent; no files changed.\n\n```json\n{\"eval_outcome\":\"plan_mismatch\"}\n```\n".to_owned(),
        "quality_planted" => "# Quality review\n\n```json\n{\"eval_version\":1,\"findings\":[{\"severity\":\"must_fix\",\"file\":\"src/lib.rs\",\"line\":3,\"summary\":\"FlagParser is an unnecessary abstraction with one caller\"},{\"severity\":\"must_fix\",\"file\":\"src/lib.rs\",\"line\":18,\"summary\":\"UserName keeps duplicate representation in raw and normalized fields\"},{\"severity\":\"minor\",\"file\":\"src/lib.rs\",\"line\":28,\"summary\":\"Nested control flow and repeated unwrap obscure classification\"}]}\n```\n".to_owned(),
        "quality_clean" => "# Quality review\nNo actionable defects.\n\n```json\n{\"eval_version\":1,\"findings\":[]}\n```\n".to_owned(),
        "spec_missing_wrong_unrequested" => "# Specification review\n\n```json\n{\"eval_version\":1,\"findings\":[{\"category\":\"missing\",\"file\":\"src/lib.rs\",\"line\":1,\"summary\":\"Negative quantity validation is missing\"},{\"category\":\"wrong\",\"file\":\"src/lib.rs\",\"line\":2,\"summary\":\"Discount wrongly includes shipping instead of subtotal only\"},{\"category\":\"unrequested\",\"file\":\"src/lib.rs\",\"line\":6,\"summary\":\"Coupon EXTRA5 behavior is unrequested\"}]}\n```\n".to_owned(),
        "spec_clean" => "# Specification review\nBehavior matches requested scope.\n\n```json\n{\"eval_version\":1,\"findings\":[]}\n```\n".to_owned(),
        other => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unknown eval fixture {other}"),
            ));
        }
    };
    Ok(result)
}

fn parse_u64(value: Option<OsString>) -> std::io::Result<u64> {
    value
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing integer"))
}
