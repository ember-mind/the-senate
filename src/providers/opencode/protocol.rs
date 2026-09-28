//! Decoder for `opencode run --format json` event lines.
//!
//! Every event — `step_start`, `tool_use`, `text`, `step_finish`, `error` —
//! carries its own `sessionID`, unlike Codex, which announces the session
//! once in a dedicated `thread.started` record. There is no distinguished
//! "session begins here" event, so the adapter binds native session identity
//! from whichever record happens to arrive first for an invocation, of
//! whatever type; this module reports that identity on every decoded event
//! so the adapter can do so uniformly.
//!
//! Tokens are reported per `step_finish`, not once per turn as Codex's
//! `turn.completed` does; the adapter emits one usage delta per step and lets
//! the existing read-time summation add them up, exactly as multi-event
//! Claude/Codex usage already does. `input` excludes `cache.read`: verified
//! against a real record where `input + output + cache.read == total`.

use serde_json::Value;

use crate::engine::UsageDelta;

use super::OpencodeProviderError;

/// One decoded event line plus the exact byte count it consumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpencodeEvent {
    pub(crate) session_id: String,
    pub(crate) kind: OpencodeKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpencodeKind {
    /// A step began. Carries no content of its own; relevant only as
    /// whichever record happens to be first, for session binding.
    StepStart,
    /// A tool call finished, successfully or not. `progress` is always a
    /// short, bounded, provider-composed description — never the tool's raw
    /// input, output, or error text, which may be arbitrarily large or (for a
    /// denied external-directory write, observed in the wild) enumerate every
    /// permission rule the user has configured.
    ToolUse {
        progress: String,
    },
    /// Assistant text within a step. Bounded and surfaced as progress; the
    /// authoritative final answer is reconstructed separately by scanning the
    /// retained stream for the text under the winning step's `message_id`.
    Text {
        message_id: String,
        text: String,
    },
    /// One step's accounting and outcome. `stop` is this step's own
    /// `reason == "stop"`, the protocol's signal that the turn is done.
    StepFinish {
        message_id: String,
        stop: bool,
        usage: UsageDelta,
    },
    /// A top-level `error` record: vendor failure (insufficient balance),
    /// unresolvable model, or any other terminal opencode failure.
    Error {
        message: String,
    },
    Ignored,
}

/// Ceiling on one progress description built from tool or text content.
/// Generous for a one-line summary, far under anything that would make the
/// event log itself a place raw agent output accumulates.
const MAX_PROGRESS_CHARS: usize = 2000;

pub(crate) fn first_record(
    bytes: &[u8],
) -> Result<Option<(OpencodeEvent, usize)>, OpencodeProviderError> {
    let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
        return Ok(None);
    };
    let line = &bytes[..newline];
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(Some((
            OpencodeEvent {
                session_id: String::new(),
                kind: OpencodeKind::Ignored,
            },
            newline + 1,
        )));
    }
    let value: Value = serde_json::from_slice(line)
        .map_err(|error| OpencodeProviderError::Protocol(error.to_string()))?;
    Ok(Some((decode(&value)?, newline + 1)))
}

/// The exact, untruncated `(message_id, text)` of one `text` event line, or
/// `None` when this line is not one.
///
/// Deliberately separate from [`decode`], which bounds text for the progress
/// log: reconstructing the winning step's final answer needs the verbatim
/// bytes opencode reported, not a shortened description of them. A line that
/// is not valid JSON or not a `text` event is simply not one of these, so it
/// is `None` rather than an error.
pub(crate) fn text_part(line: &[u8]) -> Option<(String, String)> {
    let value: Value = serde_json::from_slice(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    let part = value.get("part")?;
    let message_id = part.get("messageID").and_then(Value::as_str)?.to_owned();
    let text = part.get("text").and_then(Value::as_str)?.to_owned();
    Some((message_id, text))
}

fn decode(value: &Value) -> Result<OpencodeEvent, OpencodeProviderError> {
    let session_id = value
        .get("sessionID")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| OpencodeProviderError::Protocol("missing sessionID".to_owned()))?;
    let kind = match value.get("type").and_then(Value::as_str) {
        Some("step_start") => OpencodeKind::StepStart,
        Some("tool_use") => decode_tool_use(value.get("part").unwrap_or(&Value::Null)),
        Some("text") => decode_text(value.get("part").unwrap_or(&Value::Null)),
        Some("step_finish") => decode_step_finish(value.get("part").unwrap_or(&Value::Null))?,
        Some("error") => OpencodeKind::Error {
            message: error_message(value.get("error").unwrap_or(&Value::Null)),
        },
        Some(_) | None => OpencodeKind::Ignored,
    };
    Ok(OpencodeEvent { session_id, kind })
}

fn decode_tool_use(part: &Value) -> OpencodeKind {
    let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
    let state = part.get("state").unwrap_or(&Value::Null);
    let status = state.get("status").and_then(Value::as_str).unwrap_or("");
    let progress = if status == "error" {
        let denied = state
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|text| text.to_ascii_lowercase().contains("rejected permission"));
        if denied {
            format!(
                "opencode was denied permission to use `{tool}`; the agent continues without it"
            )
        } else {
            format!("opencode tool `{tool}` failed")
        }
    } else {
        format!("opencode used tool `{tool}`")
    };
    OpencodeKind::ToolUse {
        progress: bounded(&progress),
    }
}

fn decode_text(part: &Value) -> OpencodeKind {
    let message_id = part
        .get("messageID")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let text = part.get("text").and_then(Value::as_str).unwrap_or("");
    OpencodeKind::Text {
        message_id,
        text: bounded(text),
    }
}

fn decode_step_finish(part: &Value) -> Result<OpencodeKind, OpencodeProviderError> {
    let message_id = part
        .get("messageID")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| OpencodeProviderError::Protocol("step_finish missing messageID".to_owned()))?
        .to_owned();
    let stop = part.get("reason").and_then(Value::as_str) == Some("stop");
    Ok(OpencodeKind::StepFinish {
        message_id,
        stop,
        usage: decode_usage(part.get("tokens").unwrap_or(&Value::Null)),
    })
}

/// `input` is reported net of `cache.read` (verified against a real record:
/// `input + output + cache.read == total`), the same convention Claude uses
/// and the opposite of Codex's cache-inclusive `input_tokens`.
fn decode_usage(tokens: &Value) -> UsageDelta {
    UsageDelta {
        input_units: tokens.get("input").and_then(Value::as_u64).unwrap_or(0),
        output_units: tokens.get("output").and_then(Value::as_u64).unwrap_or(0),
        cache_read_units: tokens
            .get("cache")
            .and_then(|cache| cache.get("read"))
            .and_then(Value::as_u64),
        cache_write_units: tokens
            .get("cache")
            .and_then(|cache| cache.get("write"))
            .and_then(Value::as_u64),
        reasoning_output_units: tokens.get("reasoning").and_then(Value::as_u64),
        native_models: None,
    }
}

/// Keeps only name, message, and status code: real `error` records may carry
/// `responseHeaders`/`responseBody` noise and, for a vendor 401, an echoed
/// masked API key. Nothing beyond these three fields ever leaves this
/// function.
fn error_message(error: &Value) -> String {
    let name = error.get("name").and_then(Value::as_str).unwrap_or("");
    let message = error
        .get("data")
        .and_then(|data| data.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("opencode execution failed");
    let status_code = error
        .get("data")
        .and_then(|data| data.get("statusCode"))
        .and_then(Value::as_u64);
    match (name.is_empty(), status_code) {
        (false, Some(code)) => format!("{name} ({code}): {message}"),
        (false, None) => format!("{name}: {message}"),
        (true, Some(code)) => format!("opencode error ({code}): {message}"),
        (true, None) => message.to_owned(),
    }
}

fn bounded(text: &str) -> String {
    if text.chars().count() <= MAX_PROGRESS_CHARS {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(MAX_PROGRESS_CHARS).collect();
    truncated.push_str(" …");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_record_waits_without_consumption() {
        assert_eq!(first_record(br#"{"type":"step_star"#).unwrap(), None);
    }

    #[test]
    fn every_event_reports_its_session_id() {
        let raw = br#"{"type":"step_start","sessionID":"ses_A","part":{}}
"#;
        let (event, _) = first_record(raw).unwrap().unwrap();
        assert_eq!(event.session_id, "ses_A");
        assert_eq!(event.kind, OpencodeKind::StepStart);
    }

    /// Shape copied from the real `ro.jsonl` fixture: a denied permission
    /// becomes bounded progress, never the raw command or the full rule dump.
    #[test]
    fn a_denied_tool_is_progress_without_leaking_the_denial_text() {
        let raw = br#"{"type":"tool_use","sessionID":"ses_A","part":{"tool":"bash","state":{"status":"error","error":"The user rejected permission to use this specific tool call."}}}
"#;
        let (event, _) = first_record(raw).unwrap().unwrap();
        let OpencodeKind::ToolUse { progress } = event.kind else {
            panic!("expected tool use");
        };
        assert!(progress.contains("denied permission"));
        assert!(progress.contains("bash"));
        assert!(!progress.contains("rejected permission to use this specific tool call"));
    }

    #[test]
    fn a_completed_tool_is_progress_without_its_output() {
        let raw = br#"{"type":"tool_use","sessionID":"ses_A","part":{"tool":"read","state":{"status":"completed","output":"SECRET FILE CONTENTS"}}}
"#;
        let (event, _) = first_record(raw).unwrap().unwrap();
        let OpencodeKind::ToolUse { progress } = event.kind else {
            panic!("expected tool use");
        };
        assert!(progress.contains("read"));
        assert!(!progress.contains("SECRET"));
    }

    /// Shape copied from the real `ro.jsonl` fixture: `input + output +
    /// cache.read == total`, so `input` is reported net of cache reads.
    #[test]
    fn step_finish_usage_excludes_cache_read_from_input() {
        let raw = br#"{"type":"step_finish","sessionID":"ses_A","part":{"messageID":"msg_1","reason":"tool-calls","tokens":{"total":12667,"input":10828,"output":47,"reasoning":0,"cache":{"write":0,"read":1792}}}}
"#;
        let (event, _) = first_record(raw).unwrap().unwrap();
        let OpencodeKind::StepFinish {
            message_id,
            stop,
            usage,
        } = event.kind
        else {
            panic!("expected step_finish");
        };
        assert_eq!(message_id, "msg_1");
        assert!(!stop);
        assert_eq!(usage.input_units, 10828);
        assert_eq!(usage.output_units, 47);
        assert_eq!(usage.cache_read_units, Some(1792));
        assert_eq!(usage.cache_write_units, Some(0));
        assert_eq!(usage.reasoning_output_units, Some(0));
        assert_eq!(
            usage.input_units + usage.output_units + usage.cache_read_units.unwrap_or(0),
            12667
        );
    }

    #[test]
    fn step_finish_reason_stop_is_the_only_terminal_reason() {
        let raw = br#"{"type":"step_finish","sessionID":"ses_A","part":{"messageID":"msg_2","reason":"stop","tokens":{"total":1,"input":1,"output":0}}}
"#;
        let (event, _) = first_record(raw).unwrap().unwrap();
        assert!(matches!(
            event.kind,
            OpencodeKind::StepFinish { stop: true, .. }
        ));
    }

    /// Shape copied from the real `err-402.jsonl` fixture: only name, message,
    /// and status code survive; `responseHeaders`/`responseBody` (which can
    /// echo a masked key) never reach the decoded event.
    #[test]
    fn vendor_error_is_scrubbed_to_name_message_and_status_code() {
        let raw = br#"{"type":"error","sessionID":"ses_A","error":{"name":"APIError","data":{"message":"Insufficient Balance (request_id: abc)","statusCode":402,"isRetryable":false,"responseHeaders":{"secret":"leak"},"responseBody":"{\"leak\":true}"}}}
"#;
        let (event, _) = first_record(raw).unwrap().unwrap();
        let OpencodeKind::Error { message } = event.kind else {
            panic!("expected error");
        };
        assert!(message.contains("APIError"));
        assert!(message.contains("402"));
        assert!(message.contains("Insufficient Balance"));
        assert!(!message.contains("leak"));
    }

    /// Shape copied from the real `nomodel.jsonl` fixture.
    #[test]
    fn unknown_model_error_is_typed_progress_not_a_panic() {
        let raw = br#"{"type":"error","sessionID":"ses_A","error":{"name":"UnknownError","data":{"message":"Unexpected server error. Check server logs for details.","ref":"err_0a77e1f7"}}}
"#;
        let (event, _) = first_record(raw).unwrap().unwrap();
        assert!(matches!(event.kind, OpencodeKind::Error { .. }));
    }

    #[test]
    fn unknown_type_is_ignored_and_invalid_complete_line_fails() {
        let raw = br#"{"type":"future.opencode.event","sessionID":"ses_A"}
"#;
        assert!(matches!(
            first_record(raw).unwrap(),
            Some((
                OpencodeEvent {
                    kind: OpencodeKind::Ignored,
                    ..
                },
                _
            ))
        ));
        assert!(matches!(
            first_record(b"{\"type\":\"broken\"\n"),
            Err(OpencodeProviderError::Protocol(_))
        ));
    }

    #[test]
    fn missing_session_id_fails_instead_of_silently_defaulting() {
        let raw = br#"{"type":"step_start","part":{}}
"#;
        assert!(matches!(
            first_record(raw),
            Err(OpencodeProviderError::Protocol(_))
        ));
    }

    #[test]
    fn text_part_extracts_exact_untruncated_text() {
        let huge = "y".repeat(MAX_PROGRESS_CHARS + 500);
        let raw = format!(
            r#"{{"type":"text","sessionID":"ses_A","part":{{"messageID":"msg_1","text":"{huge}"}}}}"#
        );
        let (message_id, text) = text_part(raw.as_bytes()).expect("a text event");
        assert_eq!(message_id, "msg_1");
        assert_eq!(text, huge, "reconstruction must not truncate");

        assert_eq!(
            text_part(b"{\"type\":\"step_start\",\"sessionID\":\"ses_A\"}"),
            None
        );
        assert_eq!(text_part(b"not json"), None);
    }

    #[test]
    fn overlong_progress_text_is_bounded() {
        let huge = "x".repeat(MAX_PROGRESS_CHARS + 500);
        let bounded_text = bounded(&huge);
        assert!(bounded_text.chars().count() <= MAX_PROGRESS_CHARS + 2);
    }
}
