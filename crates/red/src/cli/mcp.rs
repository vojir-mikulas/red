//! `red mcp <connection>`: a headless **stdio** MCP server (see
//! docs/plans/todo/mcp-stdio.md). Gives any MCP client (Claude Code, other chats)
//! Red's read-only database tools without the GUI open.
//!
//! This is a dumb stdio↔JSON-RPC pump: it reads one JSON-RPC message per line
//! from stdin, answers `initialize`/`ping` locally, and translates `tools/list`/
//! `tools/call` into the `AiToolList`/`AiToolCall` commands the backend already
//! guards. **All safety enforcement (tier filter, write/GUI-tool withholding,
//! tool-call budget) lives in `red-service`**; the CLI never owns the driver. No
//! bearer nonce: stdio is the trust boundary (the client launched this process).

use std::io::{BufRead, Write};

use clap::Args;
use red_service::{Command, Event};
use serde_json::{Value as Json, json};

use super::{EXIT_OK, EventRx, PRIMARY, connect, recv, resolve, shutdown, start};

#[derive(Args)]
pub(crate) struct McpArgs {
    /// A saved connection name, or an inline DSN. One connection per process.
    pub(crate) conn: String,
}

pub(crate) fn cmd_mcp(args: McpArgs) -> u8 {
    let config = match resolve(&args.conn) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return super::EXIT_USAGE;
        }
    };
    let (svc, mut events) = start();
    if let Err(code) = connect(&svc, &mut events, config) {
        shutdown(&svc);
        return code;
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    // Monotonic correlation id so a reply is matched to the request that issued
    // it; stdio is sequential (one blocking round-trip at a time) but the id
    // keeps the pairing explicit and defends against a stray earlier event.
    let mut call_id: u64 = 0;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break }; // stdin closed / unreadable
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Json = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                write_message(&mut stdout, rpc_error(Json::Null, -32700, "parse error"));
                continue;
            }
        };
        let method = msg.get("method").and_then(Json::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Json::Null);
        // No `id` ⇒ a notification (e.g. `notifications/initialized`): no reply.
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };

        let result = match method {
            "initialize" => Ok(initialize_result(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => {
                call_id += 1;
                let want = call_id;
                svc.send_to(PRIMARY, Command::AiToolList { call_id: want });
                match recv_tool_catalog(&mut events, want) {
                    Some(tools_json) => {
                        let tools: Json =
                            serde_json::from_str(&tools_json).unwrap_or_else(|_| json!([]));
                        Ok(json!({ "tools": tools }))
                    }
                    None => Err((-32603, "backend closed".to_string())),
                }
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    Err((-32602, "missing tool name".to_string()))
                } else {
                    let arguments = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    call_id += 1;
                    let want = call_id;
                    svc.send_to(
                        PRIMARY,
                        Command::AiToolCall {
                            call_id: want,
                            name,
                            input: arguments.to_string(),
                        },
                    );
                    match recv_tool_result(&mut events, want) {
                        Some((text, is_error)) => Ok(json!({
                            "content": [ { "type": "text", "text": text } ],
                            "isError": is_error,
                        })),
                        None => Err((-32603, "backend closed".to_string())),
                    }
                }
            }
            other => Err((-32601, format!("method not found: {other}"))),
        };

        let envelope = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => rpc_error(id, code, &message),
        };
        write_message(&mut stdout, envelope);
    }

    shutdown(&svc);
    EXIT_OK
}

/// The `initialize` reply: echo the client's protocol version, advertise only the
/// `tools` capability, and identify as `red-db` (matching the HTTP MCP server so
/// behaviour is identical across transports).
fn initialize_result(params: &Json) -> Json {
    let version = params
        .get("protocolVersion")
        .and_then(Json::as_str)
        .unwrap_or("2025-06-18");
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "red-db", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// A JSON-RPC error envelope.
fn rpc_error(id: Json, code: i64, message: &str) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Drain events until the `AiToolCatalog` for `want` arrives (or the backend
/// closes). Sequential stdio means the matching reply is the next relevant event.
fn recv_tool_catalog(events: &mut EventRx, want: u64) -> Option<String> {
    loop {
        match recv(events)? {
            Event::AiToolCatalog {
                call_id,
                tools_json,
            } if call_id == want => {
                return Some(tools_json);
            }
            _ => continue,
        }
    }
}

/// Drain events until the `AiToolResult` for `want` arrives (or the backend
/// closes), returning `(text, is_error)`.
fn recv_tool_result(events: &mut EventRx, want: u64) -> Option<(String, bool)> {
    loop {
        match recv(events)? {
            Event::AiToolResult {
                call_id,
                text,
                is_error,
            } if call_id == want => return Some((text, is_error)),
            _ => continue,
        }
    }
}

/// Write one JSON-RPC message as a single line and flush, so the client sees it
/// immediately (stdio MCP is newline-delimited).
fn write_message(out: &mut impl Write, value: Json) {
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_echoes_version_and_names_red_db() {
        let params = json!({ "protocolVersion": "2025-03-26" });
        let result = initialize_result(&params);
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["serverInfo"]["name"], "red-db");
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_defaults_version_when_absent() {
        let result = initialize_result(&json!({}));
        assert_eq!(result["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn rpc_error_has_code_and_message() {
        let e = rpc_error(json!(7), -32601, "method not found: foo");
        assert_eq!(e["id"], 7);
        assert_eq!(e["error"]["code"], -32601);
        assert_eq!(e["error"]["message"], "method not found: foo");
    }
}
