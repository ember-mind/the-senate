//! The run-scoped MCP server the native CLI launches: `senate __image-tool
//! --socket <path>`. Stdio JSON-RPC in, one tool out. It knows nothing but
//! the socket path; authorization, bound, credential and placement all sit
//! behind the socket in the Senate process.
//!
//! Implements the subset of the Model Context Protocol both Claude Code and
//! Codex need from a stdio server: `initialize`, `notifications/initialized`,
//! `ping`, `tools/list`, `tools/call`. Everything else is `-32601`.

use std::io::{BufRead as _, Write as _};
use std::path::Path;

use serde_json::{Value, json};

use super::host::call_host;
use super::service::{ImageToolCall, MAX_PROMPT_BYTES};

/// The single tool name. Under Claude it appears as
/// `mcp__senate_image__image_generate`.
pub const TOOL_NAME: &str = "image_generate";
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Runs the server on this process's stdin/stdout until stdin closes.
///
/// # Errors
/// Returns only a stdout write failure; malformed requests are answered,
/// not fatal.
pub fn run_stdio_server(socket: &Path) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&line, socket) {
            serde_json::to_writer(&mut out, &response)?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
    }
    Ok(())
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
