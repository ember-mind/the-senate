//! The run-scoped MCP server the native CLI launches: `senate __image-tool
//! --socket <path>`. Stdio JSON-RPC in, one tool out. It knows nothing but
//! the socket path; authorization, bound, credential and placement all sit
//! behind the socket in the Senate process.
//!
//! Implements the subset of the Model Context Protocol both Claude Code and
//! Codex need from a stdio server: `initialize`, `notifications/initialized`,
//! `ping`, `tools/list`, `tools/call`. Everything else is `-32601`.

use std::io::{BufRead, Write as _};
use std::path::Path;

use serde_json::{Value, json};

use super::host::call_host;
use super::service::{ImageToolCall, MAX_PROMPT_BYTES};

/// The single tool name. Under Claude it appears as
/// `mcp__senate_image__image_generate`.
pub const TOOL_NAME: &str = "image_generate";
const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_JSON_RPC_LINE_BYTES: usize = 64 * 1024;

/// Runs the server on this process's stdin/stdout until stdin closes.
///
/// # Errors
/// Returns an I/O failure or an oversized/invalid UTF-8 frame. Malformed JSON
/// within the frame limit receives a protocol error and does not stop the server.
pub fn run_stdio_server(socket: &Path) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut out = stdout.lock();
    let mut line = Vec::with_capacity(1024);
    while read_bounded_line(&mut input, &mut line)?.is_some() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let line = std::str::from_utf8(&line)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if let Some(response) = handle_line(line, socket) {
            serde_json::to_writer(&mut out, &response)?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Reads at most one bounded JSON-RPC frame. On overflow it stops before
/// consuming the rest of that line; the shim exits instead of buffering an
/// arbitrarily large request from a provider.
fn read_bounded_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> std::io::Result<Option<()>> {
    line.clear();
    loop {
        let (consumed, terminated) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok((!line.is_empty()).then_some(()));
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            if line.len().saturating_add(consumed) > MAX_JSON_RPC_LINE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("JSON-RPC line exceeds {MAX_JSON_RPC_LINE_BYTES} byte limit"),
                ));
            }
            let content = if newline.is_some() {
                consumed - 1
            } else {
                consumed
            };
            line.extend_from_slice(&available[..content]);
            (consumed, newline.is_some())
        };
        reader.consume(consumed);
        if terminated {
            return Ok(Some(()));
        }
    }
}

/// One request line to at most one response. Notifications get none.
pub(crate) fn handle_line(line: &str, socket: &Path) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            return Some(error_response(
                &Value::Null,
                -32700,
                &format!("parse error: {error}"),
            ));
        }
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    if method.starts_with("notifications/") {
        return None;
    }
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION),
            "capabilities": {"tools": {}},
            "serverInfo": {
                "name": "senate-image",
                "version": env!("CARGO_PKG_VERSION"),
            },
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": [tool_definition()]}),
        "tools/call" => return Some(tool_call(&id, &params, socket)),
        _ => {
            return Some(error_response(
                &id,
                -32601,
                &format!("method not found: {method}"),
            ));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn tool_call(id: &Value, params: &Value, socket: &Path) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    if name != TOOL_NAME {
        return error_response(id, -32602, &format!("unknown tool: {name}"));
    }
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let call: ImageToolCall = match serde_json::from_value(arguments) {
        Ok(call) => call,
        Err(error) => {
            return tool_result(
                id,
                true,
                &json!({"code": "invalid_argument", "message": error.to_string()}),
            );
        }
    };
    match call_host(socket, &call) {
        Ok(success) => tool_result(
            id,
            false,
            &serde_json::to_value(success).unwrap_or(Value::Null),
        ),
        Err(error) => tool_result(
            id,
            true,
            &serde_json::to_value(error).unwrap_or(Value::Null),
        ),
    }
}

fn tool_result(id: &Value, is_error: bool, payload: &Value) -> Value {
    let text = serde_json::to_string_pretty(payload).unwrap_or_default();
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": text}],
            "isError": is_error,
        }
    })
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The contract the agent sees. Kept in one place so prompts, schema and
/// the service agree.
pub(crate) fn tool_definition() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": format!(
            "Generate one original PNG image with an external image model and write it into \
             the project at output_path (relative to the project root, must end in .png, \
             must not already exist). Use it only when the task genuinely benefits from a \
             new image asset. Generations per run are limited; the result reports how many \
             remain. Prompt at most {MAX_PROMPT_BYTES} bytes. On failure you receive a typed \
             error and should continue the task without the image."
        ),
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["prompt", "output_path"],
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "What the image should show: subject, composition, style, mood, colours."
                },
                "output_path": {
                    "type": "string",
                    "description": "Project-relative destination ending in .png, e.g. assets/hero.png. Parent directories are created; existing files are never overwritten."
                },
                "size": {
                    "type": "string",
                    "enum": ["auto", "1024x1024", "1536x1024", "1024x1536"],
                    "description": "Output resolution; default auto (model chooses)."
                },
                "quality": {
                    "type": "string",
                    "enum": ["low", "medium", "high"],
                    "description": "Rendering quality; default medium."
                },
                "transparent_background": {
                    "type": "boolean",
                    "description": "Request a transparent background (icons, cut-outs); default false."
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reader_preserves_multiple_frames_and_unterminated_final_frame() {
        let input = b"{\"id\":1}\n\n{\"id\":2}";
        let mut reader = std::io::Cursor::new(input.as_slice());
        let mut line = Vec::new();
        let mut frames = Vec::new();
        while read_bounded_line(&mut reader, &mut line).unwrap().is_some() {
            frames.push(line.clone());
        }
        assert_eq!(
            frames,
            [b"{\"id\":1}".to_vec(), Vec::new(), b"{\"id\":2}".to_vec()]
        );
    }

    #[test]
    fn an_oversized_frame_fails_before_the_rest_of_the_line_is_read() {
        let mut input = vec![b'x'; MAX_JSON_RPC_LINE_BYTES * 16];
        input.push(b'\n');
        let input_len = input.len() as u64;
        let mut reader = std::io::BufReader::with_capacity(4096, std::io::Cursor::new(input));
        let mut line = Vec::new();
        let error = read_bounded_line(&mut reader, &mut line).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(line.len() <= MAX_JSON_RPC_LINE_BYTES);
        assert!(reader.get_ref().position() < input_len);
    }

    #[test]
    fn frame_limit_includes_the_newline_and_accepts_exactly_bounded_eof() {
        let mut input = vec![b'x'; MAX_JSON_RPC_LINE_BYTES - 1];
        input.push(b'\n');
        let mut reader = std::io::Cursor::new(input);
        let mut line = Vec::new();
        assert_eq!(read_bounded_line(&mut reader, &mut line).unwrap(), Some(()));
        assert_eq!(line.len(), MAX_JSON_RPC_LINE_BYTES - 1);

        let mut input = vec![b'x'; MAX_JSON_RPC_LINE_BYTES];
        let mut reader = std::io::Cursor::new(input.clone());
        assert_eq!(read_bounded_line(&mut reader, &mut line).unwrap(), Some(()));
        assert_eq!(line.len(), MAX_JSON_RPC_LINE_BYTES);
        input.push(b'\n');
        let error = read_bounded_line(&mut std::io::Cursor::new(input), &mut line).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn initialize_list_and_unknown_methods_follow_json_rpc() {
        let socket = Path::new("/nonexistent/pcimg.sock");
        let init = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
            socket,
        )
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        assert!(
            handle_line(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                socket
            )
            .is_none()
        );
        let list =
            handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, socket).unwrap();
        assert_eq!(list["result"]["tools"][0]["name"], TOOL_NAME);
        assert_eq!(
            list["result"]["tools"][0]["inputSchema"]["required"],
            json!(["prompt", "output_path"])
        );
        let unknown = handle_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#,
            socket,
        )
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        let garbage = handle_line("not json", socket).unwrap();
        assert_eq!(garbage["error"]["code"], -32700);
    }

    #[test]
    fn a_call_without_a_host_is_a_tool_error_not_a_protocol_error() {
        let socket = Path::new("/nonexistent/pcimg.sock");
        let call = handle_line(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"image_generate","arguments":{"prompt":"p","output_path":"a.png"}}}"#,
            socket,
        )
        .unwrap();
        assert_eq!(call["result"]["isError"], true);
        let text = call["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("backend_unreachable"), "{text}");
        let bad_args = handle_line(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"image_generate","arguments":{"prompt":"p","output_path":"a.png","extra":1}}}"#,
            socket,
        )
        .unwrap();
        assert_eq!(bad_args["result"]["isError"], true);
        assert!(
            bad_args["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("invalid_argument")
        );
        let wrong_tool = handle_line(
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"other"}}"#,
            socket,
        )
        .unwrap();
        assert_eq!(wrong_tool["error"]["code"], -32602);
    }
}
