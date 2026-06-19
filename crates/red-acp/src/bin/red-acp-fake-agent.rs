//! A canned ACP **agent** used only by `red-acp`'s integration test — it lets the
//! real client (`AcpConversation`) be driven end-to-end over stdio without a
//! subscription or network. It speaks the agent side of ACP with the same SDK:
//!
//! - `initialize` → succeeds, advertises no auth methods (already "logged in").
//! - `session/new` → returns a fixed session id.
//! - `session/prompt` → streams a thought + two text chunks, then ends the turn.
//!   If the prompt text contains `HANG`, it instead waits for `session/cancel`
//!   and reports `Cancelled` — so the cancel path is testable.
//!
//! (The MCP tool round-trip is covered separately by `red-service`'s MCP server
//! tests, so this fixture stays dependency-free — just the ACP SDK.)

use std::sync::Arc;

use agent_client_protocol::schema::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Result, Stdio};
use tokio::sync::Notify;

const SESSION: &str = "fake-session";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Bridges `session/cancel` (a notification) to the in-flight prompt handler.
    let cancelled = Arc::new(Notify::new());
    let cancelled_notif = cancelled.clone();

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
                responder.respond(NewSessionResponse::new(SessionId::new(SESSION)))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let cancelled = cancelled.clone();
                async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let sid = SessionId::new(SESSION);
                    let text = prompt_text(&req);
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
                    if text.contains("HANG") {
                        // Wait for `session/cancel` to respond — but do it OFF the
                        // message loop (via `cx.spawn`) so the loop stays free to
                        // dispatch that very notification. Awaiting here would
                        // deadlock the connection.
                        let cancelled = cancelled.clone();
                        cx.spawn(async move {
                            cancelled.notified().await;
                            responder.respond(PromptResponse::new(StopReason::Cancelled))
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
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
                message.respond_with_error(
                    agent_client_protocol::util::internal_error("unhandled message"),
                    cx,
                )
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new())
        .await
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
