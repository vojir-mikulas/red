//! A canned ACP **agent** used only by `red-acp`'s integration test; it lets the
//! real client (`AcpConversation`) be driven end-to-end over stdio without a
//! subscription or network. It speaks the agent side of ACP with the same SDK:
//!
//! - `initialize` → succeeds, advertises no auth methods (already "logged in").
//! - `session/new` → returns a fixed session id, advertising a model selector and
//!   a boolean fast-mode switch so the config round trip is testable.
//! - `session/set_config_option` → applies the change (rejecting a boolean sent as
//!   a string, which is the encoding bug worth catching) and answers with the full
//!   refreshed set.
//! - `session/prompt` → streams a thought + two text chunks, then ends the turn.
//!   If the prompt text contains `CONFIG`, it instead streams the session config as
//!   it stands *at prompt time*, so a client can prove a setting applied before the
//!   turn rather than after it.
//!   If the prompt text contains `HANG`, it instead waits for `session/cancel`
//!   and reports `Cancelled`, so the cancel path is testable. If it contains
//!   `PERMIT`, it first asks the client for tool permission (a `run_select`-named
//!   tool unless the text also contains `UNKNOWN`) and streams `GRANTED`/`DENIED`
//!   reflecting the client's decision, so permission path is testable.
//!   If it contains `EXIT`, it exits non-zero mid-turn (simulating a crash) so the
//!   client's death detection (restart-on-crash) is testable.
//!
//! (The MCP tool round-trip is covered separately by `red-service`'s MCP server
//! tests, so this fixture stays dependency-free: just the ACP SDK.)

use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, SessionConfigOption, SessionConfigOptionValue,
    SessionConfigSelectOption, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, TextContent,
    ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Error, Result, Stdio};
use tokio::sync::Notify;

const SESSION: &str = "fake-session";

/// The session config this fake agent holds, as the client changes it.
#[derive(Clone)]
struct Config {
    model: String,
    fast: bool,
}

impl Config {
    /// The advertised control set, mirroring whatever the client last set.
    fn options(&self) -> Vec<SessionConfigOption> {
        vec![
            SessionConfigOption::select(
                "model",
                "Model",
                self.model.clone(),
                vec![
                    SessionConfigSelectOption::new("sonnet", "Sonnet"),
                    SessionConfigSelectOption::new("opus", "Opus"),
                ],
            ),
            SessionConfigOption::boolean("fast-mode", "Fast", self.fast),
        ]
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Bridges `session/cancel` (a notification) to the in-flight prompt handler.
    let cancelled = Arc::new(Notify::new());
    let cancelled_notif = cancelled.clone();
    let config = Arc::new(Mutex::new(Config {
        model: "sonnet".to_string(),
        fast: false,
    }));
    let (config_new, config_set, config_prompt) = (config.clone(), config.clone(), config.clone());

    Agent
        .builder()
        .name("red-acp-fake-agent")
        .on_receive_request(
            async move |init: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(init.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: NewSessionRequest, responder, _cx| {
                let options = lock(&config_new).options();
                responder.respond(
                    NewSessionResponse::new(SessionId::new(SESSION)).config_options(options),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: SetSessionConfigOptionRequest, responder, _cx| {
                {
                    let mut config = lock(&config_set);
                    match (&*req.config_id.0, &req.value) {
                        // A switch must arrive as a JSON boolean. Refusing a string
                        // here is the point: that's the encoding a client gets wrong.
                        ("fast-mode", SessionConfigOptionValue::Boolean { value }) => {
                            config.fast = *value;
                        }
                        ("model", SessionConfigOptionValue::ValueId { value }) => {
                            config.model = value.0.to_string();
                        }
                        (id, value) => {
                            return responder.respond_with_error(Error::invalid_params().data(
                                format!("unknown option {id} or wrong value shape: {value:?}"),
                            ));
                        }
                    }
                }
                responder.respond(SetSessionConfigOptionResponse::new(
                    lock(&config_set).options(),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let cancelled = cancelled.clone();
                async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let sid = SessionId::new(SESSION);
                    let text = prompt_text(&req);
                    if text.contains("EXIT") {
                        // Simulate a crash: bail with a non-zero status mid-turn so
                        // the client's child monitor reports the connection dead
                        // (exercises restart-on-crash detection).
                        std::process::exit(1);
                    }
                    if text.contains("CONFIG") {
                        // What the session is set to *as the turn starts*: a client
                        // that applied config after the prompt would see the defaults.
                        let config = lock(&config_prompt);
                        chunk(
                            &cx,
                            &sid,
                            SessionUpdate::AgentMessageChunk(text_chunk(&format!(
                                "model={} fast={}",
                                config.model, config.fast
                            ))),
                        );
                    } else if text.contains("PERMIT") {
                        // The permission path streams its own GRANTED/DENIED chunk
                        // below; skip the default greeting so the test reads cleanly.
                    } else {
                        chunk(
                            &cx,
                            &sid,
                            SessionUpdate::AgentThoughtChunk(text_chunk("pondering")),
                        );
                        chunk(
                            &cx,
                            &sid,
                            SessionUpdate::AgentMessageChunk(text_chunk("Hello ")),
                        );
                        chunk(
                            &cx,
                            &sid,
                            SessionUpdate::AgentMessageChunk(text_chunk("world")),
                        );
                    }
                    if text.contains("HANG") {
                        // Wait for `session/cancel` to respond, but do it OFF the
                        // message loop (via `cx.spawn`) so the loop stays free to
                        // dispatch that very notification. Awaiting here would
                        // deadlock the connection.
                        let cancelled = cancelled.clone();
                        cx.spawn(async move {
                            cancelled.notified().await;
                            responder.respond(PromptResponse::new(StopReason::Cancelled))
                        })?;
                        Ok(())
                    } else if text.contains("PERMIT") {
                        // Ask the client to approve a tool call, then report its
                        // decision. Done OFF the message loop so the loop stays free
                        // to dispatch the client's response (awaiting here deadlocks).
                        let tool = if text.contains("UNKNOWN") {
                            "transmogrify"
                        } else {
                            "run_select"
                        };
                        let req = RequestPermissionRequest::new(
                            sid.clone(),
                            ToolCallUpdate::new(
                                ToolCallId::new("call-1"),
                                ToolCallUpdateFields::new().title(tool.to_string()),
                            ),
                            vec![
                                PermissionOption::new(
                                    "allow",
                                    "Allow",
                                    PermissionOptionKind::AllowOnce,
                                ),
                                PermissionOption::new(
                                    "deny",
                                    "Deny",
                                    PermissionOptionKind::RejectOnce,
                                ),
                            ],
                        );
                        // Send the request and respond to the prompt once the
                        // client answers: the idiomatic way for an agent to call
                        // back mid-turn (keeps the message loop free).
                        let reply_cx = cx.clone();
                        let reply_sid = sid.clone();
                        cx.send_request(req)
                            .on_receiving_result(async move |result| {
                                let granted = matches!(
                                    result,
                                    Ok(resp) if matches!(
                                        &resp.outcome,
                                        RequestPermissionOutcome::Selected(s)
                                            if &*s.option_id.0 == "allow"
                                    )
                                );
                                chunk(
                                    &reply_cx,
                                    &reply_sid,
                                    SessionUpdate::AgentMessageChunk(text_chunk(if granted {
                                        "GRANTED"
                                    } else {
                                        "DENIED"
                                    })),
                                );
                                responder.respond(PromptResponse::new(StopReason::EndTurn))
                            })?;
                        Ok(())
                    } else {
                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let cancelled = cancelled_notif;
                async move |_c: CancelNotification, _cx| {
                    cancelled.notify_waiters();
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

/// The config behind its lock. A poisoned lock can't happen here (nothing panics
/// while holding it) and this is a test fixture, so recover rather than ceremony.
fn lock(config: &Arc<Mutex<Config>>) -> std::sync::MutexGuard<'_, Config> {
    config.lock().unwrap_or_else(|e| e.into_inner())
}

fn prompt_text(req: &PromptRequest) -> String {
    req.prompt
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

fn text_chunk(text: &str) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
}

fn chunk(cx: &ConnectionTo<Client>, session_id: &SessionId, update: SessionUpdate) {
    let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update));
}
