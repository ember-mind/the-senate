use serde_json::Value;

use crate::engine::UsageDelta;

use super::CodexProviderError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CodexRecord {
    ThreadStarted { thread_id: String },
    Progress(String),
    TurnCompleted(UsageDelta),
    Failed(String),
    Ignored,
}

pub(crate) fn first_record(
    bytes: &[u8],
) -> Result<Option<(CodexRecord, usize)>, CodexProviderError> {
    let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
        return Ok(None);
    };
    let line = &bytes[..newline];
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(Some((CodexRecord::Ignored, newline + 1)));
    }
    let value: Value = serde_json::from_slice(line)
        .map_err(|error| CodexProviderError::Protocol(error.to_string()))?;
    Ok(Some((decode(&value)?, newline + 1)))
}

/// The verbatim text of one completed agent message, if this line is one.
///
/// Deliberately separate from [`decode`], which turns every item into an
/// opaque progress string: corroborating a final-message file needs the exact
/// bytes Codex said it produced, not a description of them. A line that is not
/// valid JSON, not an `item.completed`, or not an agent message is simply not
/// one of these, so it is `None` rather than an error.
pub(crate) fn agent_message_text(line: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("item.completed") {
        return None;
    }
    let item = value.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("agent_message") {
        return None;
    }
    item.get("text")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn decode(value: &Value) -> Result<CodexRecord, CodexProviderError> {
    match value.get("type").and_then(Value::as_str) {
        Some("thread.started") => Ok(CodexRecord::ThreadStarted {
            thread_id: required_string(value, "thread_id")?,
        }),
        Some("item.completed") => Ok(decode_item(value.get("item").unwrap_or(&Value::Null))),
        Some("turn.completed") => Ok(CodexRecord::TurnCompleted(decode_usage(value))),
        Some("turn.failed") => Ok(CodexRecord::Failed(error_message(
            value,
            "Codex turn failed",
        ))),
        Some("error") => Ok(CodexRecord::Failed(error_message(
            value,
            "Codex execution failed",
        ))),
        Some(_) | None => Ok(CodexRecord::Ignored),
    }
}

fn decode_item(item: &Value) -> CodexRecord {
    match item.get("type").and_then(Value::as_str) {
        Some("agent_message") => item
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map_or(CodexRecord::Ignored, |text| {
                CodexRecord::Progress(text.to_owned())
            }),
        Some("command_execution") => CodexRecord::Progress("Codex completed a command".to_owned()),
        Some("file_change") => CodexRecord::Progress("Codex modified files".to_owned()),
        Some("mcp_tool_call") => {
            CodexRecord::Progress("Codex completed an MCP tool call".to_owned())
        }
        Some("web_search") => CodexRecord::Progress("Codex completed a web search".to_owned()),
        Some("plan" | "todo_list") => CodexRecord::Progress("Codex updated its plan".to_owned()),
        Some(_) | None => CodexRecord::Ignored,
    }
}

fn decode_usage(value: &Value) -> UsageDelta {
    let usage = value.get("usage").unwrap_or(&Value::Null);
    // Optional dimensions stay `None` when the native record omits them;
    // absence is "not reported", never zero. All values are Codex-native
    // units: `cached_input_tokens` is Codex's own cached-input accounting and
    // `reasoning_output_tokens` its private reasoning output, reported
    // separately from `output_tokens` exactly as the runtime emits them.
    UsageDelta {
        input_units: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_units: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_units: usage.get("cached_input_tokens").and_then(Value::as_u64),
        cache_write_units: usage
            .get("cache_write_input_tokens")
            .and_then(Value::as_u64),
        reasoning_output_units: usage.get("reasoning_output_tokens").and_then(Value::as_u64),
        native_models: None,
    }
}

fn required_string(value: &Value, key: &'static str) -> Result<String, CodexProviderError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| CodexProviderError::Protocol(format!("missing {key}")))
}

fn error_message(value: &Value, fallback: &str) -> String {
    value
        .get("error")
        .and_then(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .or_else(|| value.get("message").and_then(Value::as_str))
        .filter(|message| !message.trim().is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_record_waits_without_consumption() {
        assert_eq!(first_record(br#"{"type":"thread.star"#).unwrap(), None);
    }

    #[test]
    fn thread_started_extracts_identity() {
        let raw = b"{\"type\":\"thread.started\",\"thread_id\":\"thread-A\",\"future\":1}\n";
        assert_eq!(
            first_record(raw).unwrap(),
            Some((
                CodexRecord::ThreadStarted {
                    thread_id: "thread-A".to_owned()
                },
                raw.len()
            ))
        );
    }

    #[test]
    fn agent_message_is_progress_but_reasoning_is_not_exposed() {
        let message = b"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"safe result\"}}\n";
        assert!(matches!(
            first_record(message).unwrap(),
            Some((CodexRecord::Progress(text), _)) if text == "safe result"
        ));
        let reasoning = b"{\"type\":\"item.completed\",\"item\":{\"type\":\"reasoning\",\"text\":\"private chain\"}}\n";
        assert!(matches!(
            first_record(reasoning).unwrap(),
            Some((CodexRecord::Ignored, _))
        ));
    }

    #[test]
    fn turn_completed_captures_native_cache_and_reasoning_dimensions() {
        // Shape copied structurally from a real role_core_v3 Codex
        // turn.completed record (implementer_invalid_plan_stop rep-003).
        let raw = b"{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":48610,\"cached_input_tokens\":40192,\"cache_write_input_tokens\":0,\"output_tokens\":720,\"reasoning_output_tokens\":219}}\n";
        assert_eq!(
            first_record(raw).unwrap(),
            Some((
                CodexRecord::TurnCompleted(UsageDelta {
                    input_units: 48610,
                    output_units: 720,
                    cache_read_units: Some(40192),
                    // Explicit native zero stays Some(0), not unavailable.
                    cache_write_units: Some(0),
                    reasoning_output_units: Some(219),
                    native_models: None,
                }),
                raw.len()
            ))
        );
    }

    #[test]
    fn turn_completed_missing_optional_dimensions_stay_unavailable() {
        let raw =
            b"{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":20}}\n";
        assert_eq!(
            first_record(raw).unwrap(),
            Some((
                CodexRecord::TurnCompleted(UsageDelta {
                    input_units: 100,
                    output_units: 20,
                    cache_read_units: None,
                    cache_write_units: None,
                    reasoning_output_units: None,
                    native_models: None,
                }),
                raw.len()
            ))
        );
    }

    #[test]
    fn unknown_is_checkpoint_and_invalid_complete_line_fails() {
        let unknown = b"{\"type\":\"future.codex.event\",\"something\":123}\n";
        assert!(matches!(
            first_record(unknown).unwrap(),
            Some((CodexRecord::Ignored, _))
        ));
        assert!(matches!(
            first_record(b"{\"type\":\"broken\"\n"),
            Err(CodexProviderError::Protocol(_))
        ));
    }

    /// The final-message file is corroborated against this text, so it must
    /// come back byte for byte — multi-line content, escapes and all — and
    /// nothing that merely looks like a message may masquerade as one.
    #[test]
    fn an_agent_message_yields_its_exact_text_and_nothing_else_does() {
        // Shape copied structurally from a real Codex `item.completed`
        // agent_message record, whose text is what `--output-last-message`
        // holds byte for byte.
        let raw = br##"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"# Result\n\n- Fixed `double` to multiply.\n"}}"##;
        assert_eq!(
            agent_message_text(raw).as_deref(),
            Some("# Result\n\n- Fixed `double` to multiply.\n")
        );

        for other in [
            &br#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"cargo test"}}"#[..],
            &br#"{"type":"item.completed","item":{"id":"r1","type":"reasoning","text":"private chain"}}"#[..],
            &br#"{"type":"item.started","item":{"id":"m1","type":"agent_message","text":"not finished"}}"#[..],
            &br#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#[..],
            &b"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_mess"[..],
        ] {
            assert_eq!(agent_message_text(other), None);
        }
    }

    #[test]
    fn failures_are_typed_without_strict_extra_fields() {
        let raw =
            b"{\"type\":\"turn.failed\",\"error\":{\"message\":\"sandbox denied\",\"code\":42}}\n";
        assert!(matches!(
            first_record(raw).unwrap(),
            Some((CodexRecord::Failed(message), _)) if message == "sandbox denied"
        ));
    }
}
