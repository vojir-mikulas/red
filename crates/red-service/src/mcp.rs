//! A localhost HTTP MCP server that grounds the **subscription (ACP) assistant**
//! in the live database. Claude Code (the ACP agent) connects to MCP servers we
//! name in `session/new`; the live run showed it advertises `mcp_capabilities`
//! `http` (not `acp`), so Red *hosts* the server and the agent connects in.
//!
//! It serves the exact same four read-only tools the API-key path uses
//! (`crate::ai::tool_catalog` / `run_tool`), bound to one session's
//! `DatabaseDriver` — so the model browses the database through the same guards a
//! human does and (in M1) cannot mutate anything.
//!
//! Hardening: bound to loopback on a random port, gated by a per-session bearer
//! nonce (handed to the agent via the MCP server's `Authorization` header), and
//! torn down with the conversation (the accept loop is aborted on `Drop`). Only
//! the agent we spawned, holding the nonce, can reach the tools.

use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use red_ai::CancelToken;
use red_core::AiPolicy;
use red_driver::DatabaseDriver;
use serde_json::{json, Value as Json};
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::ai::{run_tool, tool_catalog, ReportSink};

/// A running MCP server, one per ACP conversation. Holds the URL + bearer nonce
/// to put in `session/new.mcp_servers`; aborts its accept loop on `Drop`, so the
/// loopback port closes when the conversation ends.
pub(crate) struct McpServer {
    url: String,
    token: String,
    task: tokio::task::JoinHandle<()>,
}

impl McpServer {
    /// Bind a fresh loopback server backed by `driver`, gated by `policy`, and
    /// start accepting. The policy (access tier + resource guards, M-S7) is
    /// captured here and enforced on every `tools/list`/`tools/call` — so the
    /// subscription agent sees exactly the catalog the tier allows and can't
    /// exceed the limits, the same gate the API-key path runs under.
    pub(crate) async fn start(
        driver: Arc<dyn DatabaseDriver>,
        policy: AiPolicy,
        report: ReportSink,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        // Two v4 UUIDs of entropy — a loopback-only secret, not a long-term key.
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let url = format!("http://127.0.0.1:{port}/mcp");

        // One cumulative tool-call tally for the agent's whole lifetime, bounding a
        // runaway loop (the subscription-path analogue of the API path's
        // per-conversation budget).
        let calls = Arc::new(AtomicUsize::new(0));
        let token_task = token.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let io = TokioIo::new(stream);
                let driver = driver.clone();
                let token = token_task.clone();
                let calls = calls.clone();
                let report = report.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        handle_request(
                            req,
                            driver.clone(),
                            token.clone(),
                            policy,
                            calls.clone(),
                            report.clone(),
                        )
                    });
                    // Errors here are per-connection (client hung up) — drop quietly.
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        });

        Ok(Self { url, token, task })
    }

    /// The `http://127.0.0.1:<port>/mcp` URL for `session/new`.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// The bearer nonce the agent must send (`Authorization: Bearer <token>`).
    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One HTTP request → one JSON-RPC reply. Never errors (`Infallible`): every
/// failure is encoded as an HTTP status or a JSON-RPC error envelope.
async fn handle_request(
    req: Request<Incoming>,
    driver: Arc<dyn DatabaseDriver>,
    token: String,
    policy: AiPolicy,
    calls: Arc<AtomicUsize>,
    report: ReportSink,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // JSON-RPC rides POST. We don't push, so a GET SSE stream is unsupported.
    if req.method() != Method::POST {
        return Ok(text(StatusCode::METHOD_NOT_ALLOWED, "POST only"));
    }
    // Bearer-nonce gate: only the agent we handed the token to may reach the tools.
    if !authorized(req.headers(), &token) {
        return Ok(text(StatusCode::UNAUTHORIZED, "unauthorized"));
    }

    let body = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Ok(text(StatusCode::BAD_REQUEST, "unreadable body")),
    };
    let msg: Json = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return Ok(json_body(rpc_error(Json::Null, -32700, "parse error"))),
    };

    let method = msg.get("method").and_then(Json::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Json::Null);
    // No `id` ⇒ a notification (e.g. `notifications/initialized`): ack, no body.
    let Some(id) = msg.get("id").cloned() else {
        return Ok(empty(StatusCode::ACCEPTED));
    };

    let envelope = match dispatch(method, &params, &driver, policy, &calls, &report).await {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => rpc_error(id, code, &message),
    };
    Ok(json_body(envelope))
}

/// Dispatch one MCP method under the access policy. Returns the JSON-RPC `result`
/// payload, or a `(code, message)` JSON-RPC error.
async fn dispatch(
    method: &str,
    params: &Json,
    driver: &Arc<dyn DatabaseDriver>,
    policy: AiPolicy,
    calls: &AtomicUsize,
    report: &ReportSink,
) -> Result<Json, (i64, String)> {
    match method {
        // Echo the client's protocol version; we only need the `tools` capability.
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Json::as_str)
                .unwrap_or("2025-06-18");
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "red-db", "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => {
            // The tier filters the catalog (M-S7) — the agent never even sees a
            // tool above its access tier. The subscription/MCP path additionally
            // withholds *write* tools: a write executes only on the API-key path,
            // where per-statement approval is enforced in-process *before* the tool
            // runs. The MCP server can't verify the external agent actually prompted
            // the user, so it never offers (or runs) a mutating tool — reads only.
            let tools: Vec<Json> = tool_catalog(&policy)
                .into_iter()
                .filter(|t| !crate::ai::is_write_tool(&t.name))
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })
                })
                .collect();
            Ok(json!({ "tools": tools }))
        }
        "tools/call" => {
            let name = params.get("name").and_then(Json::as_str).unwrap_or("");
            if name.is_empty() {
                return Err((-32602, "missing tool name".into()));
            }
            // Writes never run over the subscription/MCP path (see tools/list): only
            // the in-process-gated API-key path may mutate. Refused in-band so the
            // model can recover (and before charging the budget).
            if crate::ai::is_write_tool(name) {
                return Ok(json!({
                    "content": [ { "type": "text", "text":
                        "error: this agent cannot modify data. Hand the user the SQL with \
                        open_query so they can run it themselves, or tell them to use the \
                        API-key agent, which gates each write behind explicit approval." } ],
                    "isError": true,
                }));
            }
            // Charge the agent's cumulative tool-call budget before running anything
            // (M-S7). Over budget → a tool error the model can recover from, not a
            // transport failure.
            let max = policy.limits.max_tool_calls;
            if max != 0 && calls.fetch_add(1, Ordering::Relaxed) >= max {
                return Ok(json!({
                    "content": [ { "type": "text", "text":
                        "error: this conversation's tool-call budget is exhausted; answer with \
                        what you have or ask the user to start a new chat" } ],
                    "isError": true,
                }));
            }
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            // A fresh cancel token: the agent owns turn cancellation over ACP; a
            // single tool call is short and runs to completion here.
            let (content, ok) =
                run_tool(driver, name, &args, &policy, &CancelToken::new(), report).await;
            Ok(json!({
                "content": [ { "type": "text", "text": content } ],
                "isError": !ok,
            }))
        }
        other => Err((-32601, format!("method not found: {other}"))),
    }
}

/// Whether the request carries the exact bearer nonce. The comparison is
/// constant-time so response timing can't leak the token byte by byte to a local
/// process probing the port (loopback + a 256-bit nonce already make that
/// impractical; this closes the gap regardless).
fn authorized(headers: &hyper::HeaderMap, token: &str) -> bool {
    let Some(value) = headers
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    ct_eq(value.as_bytes(), format!("Bearer {token}").as_bytes())
}

/// Constant-time byte-slice equality: always compares every byte, so timing
/// doesn't reveal how long a common prefix matched. The length check
/// short-circuits, but the nonce's length is fixed and not itself a secret.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn rpc_error(id: Json, code: i64, message: &str) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn json_body(value: Json) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(&value).unwrap_or_default();
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(bytes)))
        .expect("static response builds")
}

fn text(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(message.to_owned())))
        .expect("static response builds")
}

fn empty(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::new()))
        .expect("static response builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_driver::SqliteDriver;

    /// Spin up the server over a tiny fixture DB at `policy` and return
    /// `(server, client)`.
    async fn fixture_with(policy: AiPolicy) -> (McpServer, reqwest::Client) {
        let path = std::env::temp_dir().join(format!("red-mcp-{}.db", Uuid::new_v4()));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);
                 INSERT INTO widgets (id, name) VALUES (1, 'alpha'), (2, 'beta');",
            )
            .unwrap();
        }
        let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::new(path, true));
        let server = McpServer::start(driver, policy, ReportSink::disabled())
            .await
            .unwrap();
        (server, reqwest::Client::new())
    }

    /// The common case: the full `read` tier with default limits.
    async fn fixture() -> (McpServer, reqwest::Client) {
        fixture_with(AiPolicy::default()).await
    }

    /// POST a JSON-RPC message with the bearer nonce and return the parsed reply.
    async fn call(server: &McpServer, client: &reqwest::Client, body: Json) -> Json {
        client
            .post(server.url())
            .header("Authorization", format!("Bearer {}", server.token()))
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn tools_list_exposes_the_readonly_tools() {
        let (server, client) = fixture().await;
        let reply = call(
            &server,
            &client,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        )
        .await;
        let names: Vec<&str> = reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "list_schema",
                "describe_table",
                "run_select",
                "explain",
                "generate_report",
                "open_query"
            ]
        );
    }

    #[tokio::test]
    async fn tools_call_runs_a_guarded_select_against_the_driver() {
        let (server, client) = fixture().await;
        let reply = call(
            &server,
            &client,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "run_select",
                    "arguments": { "sql": "SELECT name FROM widgets ORDER BY id" },
                },
            }),
        )
        .await;
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("alpha") && text.contains("beta"),
            "got: {text}"
        );
        assert_eq!(reply["result"]["isError"], json!(false));
    }

    #[tokio::test]
    async fn tools_call_rejects_a_non_select_via_the_shared_gate() {
        let (server, client) = fixture().await;
        let reply = call(
            &server,
            &client,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "run_select",
                    "arguments": { "sql": "DELETE FROM widgets" },
                },
            }),
        )
        .await;
        // The read-only gate (shared with the API-key path) reports a tool error,
        // not a transport error — the model can recover from it.
        assert_eq!(reply["result"]["isError"], json!(true));
    }

    #[tokio::test]
    async fn schema_tier_withholds_the_data_tools() {
        let (server, client) = fixture_with(AiPolicy {
            tier: red_core::AiTier::Schema,
            ..AiPolicy::default()
        })
        .await;
        // tools/list shows only the structure tools.
        let list = call(
            &server,
            &client,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        )
        .await;
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["list_schema", "describe_table"]);
        // And a run_select call is refused by the server-side tier check (defense in
        // depth), as an in-band tool error rather than a transport failure.
        let reply = call(
            &server,
            &client,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": "run_select", "arguments": { "sql": "SELECT 1" } },
            }),
        )
        .await;
        assert_eq!(reply["result"]["isError"], json!(true));
    }

    #[tokio::test]
    async fn tool_call_budget_is_enforced_server_side() {
        let (server, client) = fixture_with(AiPolicy {
            limits: red_core::AiLimits {
                max_tool_calls: 1,
                ..red_core::AiLimits::default()
            },
            ..AiPolicy::default()
        })
        .await;
        let select = || {
            json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "tools/call",
                "params": { "name": "run_select", "arguments": { "sql": "SELECT 1" } },
            })
        };
        // First call runs; the second exceeds the budget and is refused in-band.
        let first = call(&server, &client, select()).await;
        assert_eq!(first["result"]["isError"], json!(false));
        let second = call(&server, &client, select()).await;
        assert_eq!(second["result"]["isError"], json!(true));
        assert!(second["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("budget"));
    }

    #[tokio::test]
    async fn write_tier_is_read_only_over_mcp() {
        // Even at the Write tier on a writable connection, the subscription/MCP path
        // never exposes or runs a write tool — writes are the API-key path's alone.
        let path = std::env::temp_dir().join(format!("red-mcp-w-{}.db", Uuid::new_v4()));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, n TEXT);
                 INSERT INTO t (id, n) VALUES (1, 'before');",
            )
            .unwrap();
        }
        let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::new(path, false));
        let server = McpServer::start(
            driver,
            AiPolicy {
                tier: red_core::AiTier::Write,
                ..AiPolicy::default()
            },
            ReportSink::disabled(),
        )
        .await
        .unwrap();
        let client = reqwest::Client::new();

        // tools/list withholds the write tool.
        let list = call(
            &server,
            &client,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        )
        .await;
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"propose_write"), "got: {names:?}");

        // And calling it anyway is refused in-band (the row stays untouched).
        let reply = call(
            &server,
            &client,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "propose_write",
                    "arguments": { "sql": "UPDATE t SET n = 'after' WHERE id = 1" },
                },
            }),
        )
        .await;
        assert_eq!(reply["result"]["isError"], json!(true));
        assert!(reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cannot modify data"));
    }

    #[tokio::test]
    async fn missing_bearer_nonce_is_unauthorized() {
        let (server, client) = fixture().await;
        let status = client
            .post(server.url())
            .json(&json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/list" }))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    }
}
