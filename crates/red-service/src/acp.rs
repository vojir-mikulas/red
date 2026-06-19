//! The subscription assistant's backend half: drives a turn through `red-acp`
//! (Claude Code over ACP) instead of the Messages API. The agent runs its own
//! model → tool → model loop and reaches Red's database through the localhost MCP
//! server we host (`crate::mcp`); this module just keeps one live ACP
//! conversation per `conversation_id`, feeds it grounded prompts, and relays the
//! streamed deltas as the **same** `Event::AiDelta`/`AiTurnFinished`/`AiError`
//! the API-key path emits — so the panel is reused unchanged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use red_acp::{AcpConfig, AcpConversation, AcpDelta, AcpStop, McpGrounding};
use red_driver::DatabaseDriver;
use tokio::sync::mpsc;

use crate::ai::{system_prompt, user_turn};
use crate::dispatch::{emit, Events};
use crate::mcp::McpServer;
use crate::protocol::{AiContext, AiDelta, AiUsage};
use crate::{Event, SessionId};

/// The MCP server name the agent sees for Red's DB tools.
const MCP_SERVER_NAME: &str = "red-db";

/// One live conversation: the agent handle (cheap to clone), the MCP server kept
/// alive for the agent's lifetime, and whether the next turn is the first (so we
/// fold the full grounding instruction in once).
struct Conversation {
    agent: AcpConversation,
    /// Held only to keep the loopback MCP server (and its port) alive.
    _mcp: McpServer,
    first_turn: bool,
}

/// Registry of live ACP conversations, keyed by `conversation_id`. Held behind a
/// `tokio::sync::Mutex` in the dispatch loop so a (slow) agent start awaits off
/// the command pump without wedging it.
#[derive(Default)]
pub(crate) struct AcpManager {
    conversations: HashMap<u64, Conversation>,
}

impl AcpManager {
    /// Cancel the in-flight turn for a conversation, if it exists.
    pub(crate) fn cancel(&self, conversation_id: u64) {
        if let Some(c) = self.conversations.get(&conversation_id) {
            c.agent.cancel();
        }
    }
}

/// Run one subscription turn to completion: ensure the conversation's agent is up
/// (starting its MCP server + session lazily), send the grounded prompt, relay
/// streamed deltas, and finish with usage or an error.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_turn(
    manager: Arc<tokio::sync::Mutex<AcpManager>>,
    driver: Arc<dyn DatabaseDriver>,
    command: String,
    cwd: PathBuf,
    events: Events,
    session: Option<SessionId>,
    conversation_id: u64,
    user_message: String,
    context: AiContext,
) {
    // Ensure the conversation exists, and learn whether this is its first turn.
    let (agent, first_turn) =
        match ensure_conversation(&manager, driver, command, cwd, conversation_id).await {
            Ok(pair) => pair,
            Err(message) => {
                emit(
                    &events,
                    session,
                    Event::AiError {
                        conversation_id,
                        message,
                    },
                );
                return;
            }
        };

    // Fold grounding into the prompt text: the full instruction once (ACP has no
    // system role), then just the volatile per-turn context on follow-ups.
    let text = if first_turn {
        format!(
            "{}\n\n{}",
            system_prompt(&context),
            user_turn(&user_message, &context)
        )
    } else {
        user_turn(&user_message, &context)
    };

    // Relay streamed deltas onto the existing event vocabulary as they arrive.
    let (sink_tx, mut sink_rx) = mpsc::unbounded_channel::<AcpDelta>();
    let relay = {
        let events = events.clone();
        tokio::spawn(async move {
            while let Some(delta) = sink_rx.recv().await {
                let delta = match delta {
                    AcpDelta::Text(t) => AiDelta::Text(t),
                    AcpDelta::Thinking(t) => AiDelta::Thinking(t),
                    AcpDelta::ToolStarted { name } => AiDelta::ToolStarted { name },
                    AcpDelta::ToolFinished { name, ok } => AiDelta::ToolFinished { name, ok },
                };
                emit(
                    &events,
                    session,
                    Event::AiDelta {
                        conversation_id,
                        delta,
                    },
                );
            }
        })
    };

    let outcome = agent.prompt(text, sink_tx).await;
    // The sink closes when the turn ends (the agent drops it), so the relay drains.
    let _ = relay.await;

    match outcome {
        Ok(Ok(result)) => {
            // A refusal/cancel is still a "finished" turn from the panel's view.
            if result.stop == AcpStop::Refusal {
                emit(
                    &events,
                    session,
                    Event::AiError {
                        conversation_id,
                        message: "the model declined to respond".into(),
                    },
                );
            } else {
                emit(
                    &events,
                    session,
                    Event::AiTurnFinished {
                        conversation_id,
                        usage: map_usage(&result.usage),
                    },
                );
            }
        }
        Ok(Err(e)) => emit(
            &events,
            session,
            Event::AiError {
                conversation_id,
                message: e.to_string(),
            },
        ),
        Err(_) => emit(
            &events,
            session,
            Event::AiError {
                conversation_id,
                message: "the assistant connection ended".into(),
            },
        ),
    }
}

/// Look up (or lazily start) the conversation's agent, returning a handle and
/// whether this is its first turn. The first turn flips the stored flag.
// `map_entry` would have us use the `entry` API, but starting a conversation
// awaits (MCP server + agent handshake) and the borrow can't be held across it.
#[allow(clippy::map_entry)]
async fn ensure_conversation(
    manager: &Arc<tokio::sync::Mutex<AcpManager>>,
    driver: Arc<dyn DatabaseDriver>,
    command: String,
    cwd: PathBuf,
    conversation_id: u64,
) -> Result<(AcpConversation, bool), String> {
    let mut guard = manager.lock().await;
    if !guard.conversations.contains_key(&conversation_id) {
        // Host the DB grounding server bound to this session's driver, then bring
        // the agent up with it attached.
        let mcp = McpServer::start(driver)
            .await
            .map_err(|e| format!("could not start the DB tool server: {e}"))?;
        let grounding = McpGrounding {
            name: MCP_SERVER_NAME.into(),
            url: mcp.url().to_string(),
            token: mcp.token().to_string(),
        };
        let config = AcpConfig {
            command,
            cwd,
            mcp: Some(grounding),
        };
        let agent = AcpConversation::start(config)
            .await
            .map_err(|e| e.to_string())?;
        guard.conversations.insert(
            conversation_id,
            Conversation {
                agent,
                _mcp: mcp,
                first_turn: true,
            },
        );
    }

    let entry = guard
        .conversations
        .get_mut(&conversation_id)
        .expect("just inserted");
    let first_turn = entry.first_turn;
    entry.first_turn = false;
    Ok((entry.agent.clone(), first_turn))
}

/// ACP reports cumulative context/cost, not per-turn input/output. Surface the
/// context tokens in the footer's input slot; per-turn split + cost land in M-S4.
fn map_usage(usage: &red_acp::AcpUsage) -> AiUsage {
    AiUsage {
        input_tokens: usage.used_tokens,
        output_tokens: 0,
        cache_read_input_tokens: 0,
    }
}
