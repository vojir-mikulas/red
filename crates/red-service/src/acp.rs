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
use std::time::{Duration, Instant};

use red_acp::{AcpConfig, AcpConversation, AcpDelta, AcpPermission, AcpStop, McpGrounding};
use red_core::AiPolicy;
use red_driver::DatabaseDriver;
use tokio::sync::{mpsc, oneshot};

use crate::ai::{system_prompt, tool_catalog, user_turn, ReportSink};
use crate::dispatch::{emit, Events};
use crate::mcp::McpServer;
use crate::protocol::{AiContext, AiDelta, AiUsage};
use crate::{Event, SessionId};

/// The MCP server name the agent sees for Red's DB tools.
const MCP_SERVER_NAME: &str = "red-db";

/// How long a conversation may sit untouched before the idle sweep tears its
/// agent down (M-S3). An idle agent is a parked subprocess plus an open MCP port;
/// a fresh prompt after teardown just restarts it (a new agent session, so the
/// grounding instruction is re-folded). Mirrors `dispatch::IDLE_EVICT` for
/// sessions — a conversation outlives its DB session's idle window so a brief
/// reconnect doesn't kill the chat, but a long-abandoned one is reclaimed.
const IDLE_TEARDOWN: Duration = Duration::from_secs(900);

/// One live conversation: the agent handle (cheap to clone), the MCP server kept
/// alive for the agent's lifetime, the DB session it grounds in (so a disconnect
/// tears down the right agents), whether the next turn is the first (so we fold
/// the full grounding instruction in once), and idle/active bookkeeping for the
/// lifecycle sweep.
struct Conversation {
    agent: AcpConversation,
    /// Held only to keep the loopback MCP server (and its port) alive.
    _mcp: McpServer,
    /// The DB session this conversation grounds in. Dropping that session (a
    /// disconnect, close, or reconnect) evicts this conversation, since its MCP
    /// server holds a now-dead driver clone.
    session: Option<SessionId>,
    first_turn: bool,
    /// When this conversation last started or finished a turn — the idle sweep
    /// reclaims it once this is older than [`IDLE_TEARDOWN`].
    last_used: Instant,
    /// True while a turn is in flight, so the idle sweep never evicts an agent
    /// mid-stream (the on-screen equivalent of a foreground session).
    active: bool,
}

/// Registry of live ACP conversations, keyed by `conversation_id`. Held behind a
/// `tokio::sync::Mutex` in the dispatch loop so a (slow) agent start awaits off
/// the command pump without wedging it.
#[derive(Default)]
pub(crate) struct AcpManager {
    conversations: HashMap<u64, Conversation>,
    /// Permission prompts (M-S2) awaiting the user's answer, keyed by request id.
    /// The relay task parks the agent's decision sink here; `AiPermission` takes
    /// it back out and fires it. Capped so a runaway agent can't grow it forever.
    pending: HashMap<u64, oneshot::Sender<bool>>,
    /// Monotonic id handed to each surfaced permission prompt.
    next_request_id: u64,
}

/// Cap on outstanding (un-answered) permission prompts. The UI serializes one
/// prompt at a time; a higher number means an agent is spamming requests — deny
/// the overflow rather than let the map grow without bound.
const MAX_PENDING_PERMISSIONS: usize = 32;

impl AcpManager {
    /// Cancel the in-flight turn for a conversation, if it exists.
    pub(crate) fn cancel(&self, conversation_id: u64) {
        if let Some(c) = self.conversations.get(&conversation_id) {
            c.agent.cancel();
        }
    }

    /// Park a permission decision sink and return the request id to surface, or
    /// `None` (deny by dropping `decide`) when too many are already outstanding.
    fn park_permission(&mut self, decide: oneshot::Sender<bool>) -> Option<u64> {
        if self.pending.len() >= MAX_PENDING_PERMISSIONS {
            return None;
        }
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.pending.insert(id, decide);
        Some(id)
    }

    /// Answer a parked permission prompt (the panel's Allow/Deny). A stale id (the
    /// prompt was already resolved or the agent went away) is a no-op.
    pub(crate) fn resolve_permission(&mut self, request_id: u64, allow: bool) {
        if let Some(decide) = self.pending.remove(&request_id) {
            let _ = decide.send(allow);
        }
    }

    /// Mark a turn finished: clear the in-flight guard and reset the idle clock so
    /// the conversation stays warm for [`IDLE_TEARDOWN`] after each reply.
    fn finish_turn(&mut self, conversation_id: u64) {
        if let Some(c) = self.conversations.get_mut(&conversation_id) {
            c.active = false;
            c.last_used = Instant::now();
        }
    }

    /// Tear down every conversation grounded in a DB session that's going away (a
    /// disconnect, close, or reconnect on the same id). Their MCP servers hold a
    /// driver clone for the dropped session, so they must not outlive it. Dropping
    /// the `Conversation` drops the agent handle (ending its connection task and
    /// the subprocess) and the MCP server (freeing its port).
    pub(crate) fn evict_session(&mut self, session: Option<SessionId>) {
        let before = self.conversations.len();
        self.conversations.retain(|_, c| c.session != session);
        let dropped = before - self.conversations.len();
        if dropped > 0 {
            tracing::debug!("tore down {dropped} ACP conversation(s) for a closed session");
        }
    }

    /// Reclaim conversations idle past [`IDLE_TEARDOWN`] (M-S3), skipping any with
    /// a turn in flight. Called from the dispatch idle sweep, mirroring the
    /// session-eviction pass — a parked agent is a subprocess plus an MCP port we
    /// can release and lazily rebuild on the next prompt.
    pub(crate) fn evict_idle(&mut self) {
        let now = Instant::now();
        let before = self.conversations.len();
        self.conversations
            .retain(|_, c| c.active || now.duration_since(c.last_used) < IDLE_TEARDOWN);
        let dropped = before - self.conversations.len();
        if dropped > 0 {
            tracing::debug!("evicted {dropped} idle ACP conversation(s)");
        }
    }

    /// Re-authenticate / switch account (M-S4): drop this conversation's agent so
    /// the next turn re-spawns it and re-runs the ACP handshake. When the agent
    /// isn't logged in (e.g. the user signed out of Claude Code elsewhere, or the
    /// subscription token expired) that handshake advertises an auth method and the
    /// agent pops its own browser login — Red never sees the tokens. A conversation
    /// that was never started is a no-op (the first turn starts it fresh anyway).
    pub(crate) fn reauthenticate(&mut self, conversation_id: u64) {
        if self.conversations.remove(&conversation_id).is_some() {
            tracing::debug!("re-authenticating ACP conversation {conversation_id} — agent restart");
        }
    }

    /// Tear down every conversation (window close / service shutdown). Done
    /// explicitly because the permission-relay tasks hold `Arc` clones of the
    /// manager, so letting the outer `Arc` drop would leave a reference cycle
    /// (relay → manager → conversation → agent's command channel → connection task
    /// → permission sender → relay) and never reap the agent subprocesses. Clearing
    /// the map here drops the command channels, which unwinds the cycle.
    pub(crate) fn clear(&mut self) {
        if !self.conversations.is_empty() {
            tracing::debug!(
                "tearing down {} ACP conversation(s) on shutdown",
                self.conversations.len()
            );
            self.conversations.clear();
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
    policy: AiPolicy,
    user_message: String,
    context: AiContext,
) {
    // Ensure the conversation exists, and learn whether this is its first turn.
    let (agent, first_turn) = match ensure_conversation(
        &manager,
        driver,
        command,
        cwd,
        events.clone(),
        session,
        conversation_id,
        policy,
    )
    .await
    {
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
            system_prompt(&context, &policy),
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
    // Clear the in-flight guard and reset the idle clock now the turn is done, so
    // the idle sweep can reclaim the agent after it sits unused. A crash mid-turn
    // still lands here (the prompt resolves with an error), so the guard never
    // sticks. The conversation may already be gone (a disconnect raced us); that's
    // a harmless no-op.
    manager.lock().await.finish_turn(conversation_id);

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
#[allow(clippy::map_entry, clippy::too_many_arguments)]
async fn ensure_conversation(
    manager: &Arc<tokio::sync::Mutex<AcpManager>>,
    driver: Arc<dyn DatabaseDriver>,
    command: String,
    cwd: PathBuf,
    events: Events,
    session: Option<SessionId>,
    conversation_id: u64,
    policy: AiPolicy,
) -> Result<(AcpConversation, bool), String> {
    let mut guard = manager.lock().await;
    // Restart on crash (M-S3): a conversation whose connection task has ended
    // (agent exited / crashed) can't serve another turn, so drop it and fall
    // through to a fresh start — a new agent session, so grounding re-folds.
    if let Some(c) = guard.conversations.get(&conversation_id) {
        if !c.agent.is_alive() {
            tracing::debug!("ACP conversation {conversation_id} died — restarting");
            guard.conversations.remove(&conversation_id);
        }
    }
    if !guard.conversations.contains_key(&conversation_id) {
        // Host the DB grounding server bound to this session's driver and gated by
        // the access policy (M-S7), then bring the agent up with it attached. The
        // policy is captured for the agent's lifetime; a settings change takes
        // effect on the next agent restart (reconnect / idle teardown / re-auth).
        // The report sink (Feature C): `generate_report` runs inside the MCP server
        // here, so it carries the events channel to announce a finished report to the
        // UI (the subscription path never routes individual tool calls back through
        // `run_turn`). Built before `events` is moved into the permission relay.
        let report = ReportSink::new(events.clone(), session, conversation_id);
        let mcp = McpServer::start(driver, policy, report)
            .await
            .map_err(|e| format!("could not start the DB tool server: {e}"))?;
        let grounding = McpGrounding {
            name: MCP_SERVER_NAME.into(),
            url: mcp.url().to_string(),
            token: mcp.token().to_string(),
        };
        // Permission policy (M-S2): auto-allow the DB tools the tier actually
        // exposes; route anything else (including a tool above the tier) to the
        // user via the relay task below. The agent is also capability-restricted
        // (no fs/terminal) in `red-acp`.
        let (perm_tx, perm_rx) = mpsc::unbounded_channel::<AcpPermission>();
        tokio::spawn(permission_relay(
            manager.clone(),
            events,
            session,
            conversation_id,
            perm_rx,
        ));
        let config = AcpConfig {
            command,
            cwd,
            mcp: Some(grounding),
            allow_tools: tool_catalog(&policy).into_iter().map(|t| t.name).collect(),
            permissions: Some(perm_tx),
        };
        let agent = AcpConversation::start(config)
            .await
            .map_err(|e| e.to_string())?;
        guard.conversations.insert(
            conversation_id,
            Conversation {
                agent,
                _mcp: mcp,
                session,
                first_turn: true,
                last_used: Instant::now(),
                active: false,
            },
        );
    }

    let entry = guard
        .conversations
        .get_mut(&conversation_id)
        .expect("just inserted");
    let first_turn = entry.first_turn;
    entry.first_turn = false;
    // Guard against the idle sweep reclaiming this agent mid-turn, and reset its
    // idle clock; `finish_turn` clears the guard when the turn ends.
    entry.active = true;
    entry.last_used = Instant::now();
    Ok((entry.agent.clone(), first_turn))
}

/// Relay non-auto-allowed permission requests (M-S2) from one conversation to the
/// UI: park the agent's decision sink, surface an `AiPermissionRequest`, and let
/// `Command::AiPermission` answer it. Ends when the conversation is torn down (the
/// agent drops its sender, closing `perm_rx`).
async fn permission_relay(
    manager: Arc<tokio::sync::Mutex<AcpManager>>,
    events: Events,
    session: Option<SessionId>,
    conversation_id: u64,
    mut perm_rx: mpsc::UnboundedReceiver<AcpPermission>,
) {
    while let Some(perm) = perm_rx.recv().await {
        let AcpPermission {
            title,
            detail,
            decide,
        } = perm;
        // Park the decision sink; dropping it on overflow denies the call.
        let Some(request_id) = manager.lock().await.park_permission(decide) else {
            tracing::warn!("too many pending AI permission prompts — denying");
            continue;
        };
        emit(
            &events,
            session,
            Event::AiPermissionRequest {
                conversation_id,
                request_id,
                title,
                detail,
            },
        );
    }
}

/// ACP reports cumulative session figures, not per-turn input/output: the tokens
/// currently in context and a running cost. Surface the context tokens in the
/// footer's input slot and pass the cost through (the panel labels it as the
/// session total). A per-turn input/output split isn't something the agent breaks
/// out, so the other slots stay zero.
fn map_usage(usage: &red_acp::AcpUsage) -> AiUsage {
    AiUsage {
        input_tokens: usage.used_tokens,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cost_usd: usage.cost_usd,
    }
}
