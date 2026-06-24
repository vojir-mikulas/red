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

use red_acp::{
    AcpCommand, AcpConfig, AcpConfigCategory, AcpConfigOption, AcpConversation, AcpDelta,
    AcpPermission, AcpStop, McpGrounding,
};
use red_core::AiPolicy;
use red_driver::DatabaseDriver;
use tokio::sync::{mpsc, oneshot};

use crate::ai::{system_prompt, tool_catalog, user_turn, ReportSink};
use crate::dispatch::{emit, Events};
use crate::mcp::McpServer;
use crate::protocol::{
    AiAuthStatus, AiCommand, AiConfigCategory, AiConfigChoice, AiConfigOption, AiContext, AiDelta,
    AiUsage, ReportTheme,
};
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
    /// Crash-restart bookkeeping per conversation, so a reliably-crashing agent
    /// can't be re-spawned on every turn (see [`AcpManager::allow_restart`]). Kept
    /// after the `Conversation` itself is dropped — that's the point: it has to
    /// outlive the dead agent to bound how often it comes back.
    restarts: HashMap<u64, RestartTracker>,
    /// In-flight interactive sign-ins (paste-code OAuth), keyed by agent id. The
    /// login relay parks the CLI's code sink here; `AiSubmitLoginCode` takes it out
    /// and fires it. Bounded by the number of agents. The `u64` token lets a login
    /// clear only *its own* entry, so a sign-in restarted for the same agent (which
    /// replaces and cancels the old one) can't have its successor cleared out from
    /// under it when the cancelled predecessor finishes.
    logins: HashMap<String, (u64, oneshot::Sender<String>)>,
    next_login_token: u64,
}

/// Cap on outstanding (un-answered) permission prompts. The UI serializes one
/// prompt at a time; a higher number means an agent is spamming requests — deny
/// the overflow rather than let the map grow without bound.
const MAX_PENDING_PERMISSIONS: usize = 32;

/// How many crash-restarts a single conversation may take within
/// [`RESTART_WINDOW`] before further restarts are refused until the window rolls
/// over. Starting an agent spawns a subprocess and binds a fresh MCP port, so a
/// crash-on-every-prompt agent would otherwise thrash both on each turn.
const RESTART_MAX: u32 = 5;

/// The rolling window over which [`RESTART_MAX`] is counted. Long enough to catch a
/// crash loop, short enough that a transient failure clears on its own.
const RESTART_WINDOW: Duration = Duration::from_secs(60);

/// Per-conversation crash-restart counter over a rolling [`RESTART_WINDOW`].
struct RestartTracker {
    count: u32,
    window_start: Instant,
}

impl AcpManager {
    /// Cancel the in-flight turn for a conversation, if it exists.
    pub(crate) fn cancel(&self, conversation_id: u64) {
        if let Some(c) = self.conversations.get(&conversation_id) {
            c.agent.cancel();
        }
    }

    /// The live agent handle for a conversation, if it's been started. Cloned so the
    /// caller can issue a request (e.g. a config change) without holding the manager
    /// lock across the await.
    pub(crate) fn conversation_agent(&self, conversation_id: u64) -> Option<AcpConversation> {
        self.conversations
            .get(&conversation_id)
            .map(|c| c.agent.clone())
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

    /// Record a crash-restart for `conversation_id` and report whether another is
    /// within budget ([`RESTART_MAX`] per [`RESTART_WINDOW`]). Past the budget the
    /// caller refuses the turn instead of re-spawning, so a crash-on-every-prompt
    /// agent can't thrash a subprocess + MCP bind on each send; the window rolls
    /// over on its own, so a later genuine retry isn't blocked forever.
    fn allow_restart(&mut self, conversation_id: u64) -> bool {
        let now = Instant::now();
        let tracker = self
            .restarts
            .entry(conversation_id)
            .or_insert(RestartTracker {
                count: 0,
                window_start: now,
            });
        if now.duration_since(tracker.window_start) > RESTART_WINDOW {
            tracker.count = 0;
            tracker.window_start = now;
        }
        tracker.count += 1;
        tracker.count <= RESTART_MAX
    }

    /// Drop restart trackers whose conversation no longer exists, so the map can't
    /// outgrow the live conversation set. Eviction is a clean teardown (idle sweep,
    /// session close, re-auth), not a crash, so resetting the budget for a
    /// reclaimed conversation is correct — a rapidly-crashing one stays recent and
    /// is retained, keeping its budget intact.
    fn prune_restarts(&mut self) {
        let conversations = &self.conversations;
        self.restarts.retain(|id, _| conversations.contains_key(id));
    }

    /// Answer a parked permission prompt (the panel's Allow/Deny). A stale id (the
    /// prompt was already resolved or the agent went away) is a no-op.
    pub(crate) fn resolve_permission(&mut self, request_id: u64, allow: bool) {
        if let Some(decide) = self.pending.remove(&request_id) {
            let _ = decide.send(allow);
        }
    }

    /// Park the code sink for an interactive sign-in, returning a token that
    /// identifies *this* attempt. Replaces any prior sign-in for the agent (dropping
    /// its sink, which cancels it).
    fn park_login(&mut self, agent_id: String, code: oneshot::Sender<String>) -> u64 {
        let token = self.next_login_token;
        self.next_login_token += 1;
        self.logins.insert(agent_id, (token, code));
        token
    }

    /// Deliver the pasted OAuth code to the in-flight sign-in for `agent_id`. A
    /// stale submit (no sign-in running, or it already finished) is a no-op.
    pub(crate) fn submit_login_code(&mut self, agent_id: &str, code: String) {
        if let Some((_, sink)) = self.logins.remove(agent_id) {
            let _ = sink.send(code);
        }
    }

    /// Cancel an in-flight sign-in for `agent_id` (drops its sink → the CLI is
    /// killed). A no-op if none is running.
    pub(crate) fn cancel_login(&mut self, agent_id: &str) {
        self.logins.remove(agent_id);
    }

    /// Drop the parked sink for a finished sign-in, but only if it's still the one
    /// this attempt parked (matching `token`) — a sign-in restarted for the same
    /// agent installs a fresh token, so the stale predecessor's cleanup is skipped.
    fn clear_login(&mut self, agent_id: &str, token: u64) {
        if self.logins.get(agent_id).is_some_and(|(t, _)| *t == token) {
            self.logins.remove(agent_id);
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
        self.prune_restarts();
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
        self.prune_restarts();
    }

    /// Drop every conversation not mid-turn so its next prompt re-spawns the agent
    /// and re-runs the ACP handshake (M-S4) — used after a Settings re-auth so live
    /// chats pick up the newly signed-in account. A conversation with a turn in
    /// flight is left alone, the same way the idle sweep skips it.
    pub(crate) fn drop_idle(&mut self) {
        let before = self.conversations.len();
        self.conversations.retain(|_, c| c.active);
        let dropped = before - self.conversations.len();
        if dropped > 0 {
            tracing::debug!("dropped {dropped} idle ACP conversation(s) after re-auth");
        }
        self.prune_restarts();
    }

    /// Tear down a conversation the UI has closed or deleted (M-S5), freeing its
    /// agent subprocess and MCP port now rather than waiting for the idle sweep.
    /// Dropping the `Conversation` unwinds the same way as [`Self::evict_session`].
    pub(crate) fn forget(&mut self, conversation_id: u64) {
        self.restarts.remove(&conversation_id);
        if self.conversations.remove(&conversation_id).is_some() {
            tracing::debug!("forgetting closed ACP conversation {conversation_id}");
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
        self.restarts.clear();
        // Dropping the parked code sinks cancels any in-flight sign-in.
        self.logins.clear();
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
        context.theme.as_deref().cloned(),
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
                message: "the agent connection ended".into(),
            },
        ),
    }
}

/// Drive an interactive subscription sign-in for an ACP agent from Settings. The
/// agent's bundled CLI runs a paste-code OAuth flow: it opens the browser to an
/// authorize URL (relayed as `AiLoginPrompt`), then waits for the code the user
/// pastes. We park a code sink the UI fires via `AiSubmitLoginCode`, relay the
/// CLI's lifecycle as `AiLoginPrompt`/`AiLoginFinished`, and on success force idle
/// conversations to re-handshake so they adopt the (possibly switched) account.
/// Red never sees the OAuth tokens — the CLI owns them.
pub(crate) async fn start_login(
    manager: Arc<tokio::sync::Mutex<AcpManager>>,
    command: String,
    agent_id: String,
    events: Events,
) {
    let (code_tx, code_rx) = oneshot::channel::<String>();
    // Park the code sink (replacing/cancelling any prior sign-in for this agent) and
    // remember our token so cleanup only touches this attempt.
    let token = manager.lock().await.park_login(agent_id.clone(), code_tx);

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<red_acp::LoginEvent>();
    tokio::spawn(red_acp::run_login(command, ev_tx, code_rx));

    while let Some(event) = ev_rx.recv().await {
        match event {
            red_acp::LoginEvent::Url(url) => emit(
                &events,
                None,
                Event::AiLoginPrompt {
                    agent_id: agent_id.clone(),
                    url,
                },
            ),
            red_acp::LoginEvent::Done(result) => {
                let (ok, message) = match result {
                    Ok(()) => (true, String::new()),
                    Err(message) => (false, message),
                };
                {
                    let mut guard = manager.lock().await;
                    guard.clear_login(&agent_id, token);
                    if ok {
                        // Live chats re-handshake on their next turn to adopt the
                        // newly signed-in (or switched) account.
                        guard.drop_idle();
                    }
                }
                emit(
                    &events,
                    None,
                    Event::AiLoginFinished {
                        agent_id,
                        ok,
                        message,
                    },
                );
                break;
            }
        }
    }
}

/// Ask the agent's bundled CLI who is signed in and emit it as `AiAgentAuthStatus`.
/// A failure (no CLI, spawn error, non-Claude agent) is logged and reported as
/// "not signed in" rather than surfaced as an error — Settings just shows a sign-in
/// affordance.
pub(crate) async fn check_auth_status(command: String, agent_id: String, events: Events) {
    let status = match red_acp::auth_status(&command).await {
        Ok(s) => AiAuthStatus {
            logged_in: s.logged_in,
            email: s.email,
            subscription: s.subscription_type,
            method: s.auth_method,
        },
        Err(e) => {
            tracing::debug!("acp auth status for {agent_id} unavailable: {e}");
            AiAuthStatus::default()
        }
    };
    emit(&events, None, Event::AiAgentAuthStatus { agent_id, status });
}

/// Sign out of an ACP agent's subscription, then re-emit its status so Settings
/// reflects the change. On success, idle conversations are dropped so they
/// re-handshake (and find no account) on their next turn.
pub(crate) async fn sign_out(
    manager: Arc<tokio::sync::Mutex<AcpManager>>,
    command: String,
    agent_id: String,
    events: Events,
) {
    match red_acp::logout(&command).await {
        Ok(()) => manager.lock().await.drop_idle(),
        Err(e) => tracing::warn!("acp sign-out for {agent_id} failed: {e}"),
    }
    check_auth_status(command, agent_id, events).await;
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
    theme: Option<ReportTheme>,
) -> Result<(AcpConversation, bool), String> {
    let mut guard = manager.lock().await;
    // Restart on crash (M-S3): a conversation whose connection task has ended
    // (agent exited / crashed) can't serve another turn, so drop it and fall
    // through to a fresh start — a new agent session, so grounding re-folds.
    if guard
        .conversations
        .get(&conversation_id)
        .is_some_and(|c| !c.agent.is_alive())
    {
        tracing::debug!("ACP conversation {conversation_id} died — restarting");
        guard.conversations.remove(&conversation_id);
        // Bound crash-restarts: a reliably-crashing agent would otherwise re-spawn a
        // subprocess + MCP bind on every prompt. Past the budget, refuse this turn.
        if !guard.allow_restart(conversation_id) {
            return Err(format!(
                "the assistant agent has crashed repeatedly ({RESTART_MAX} times within \
                 {}s) and won't be restarted again right now. Check the agent command in \
                 Settings, then try again shortly.",
                RESTART_WINDOW.as_secs()
            ));
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
        let report = ReportSink::new(events.clone(), session, conversation_id, theme);
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
        // Slash commands the agent advertises (connection-lifetime): relayed to the
        // UI as `AiCommandsAvailable`. Spawned before `events` moves into the
        // permission relay below.
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Vec<AcpCommand>>();
        tokio::spawn(commands_relay(
            events.clone(),
            session,
            conversation_id,
            cmd_rx,
        ));
        // Session config selectors (model / reasoning): relayed as
        // `AiConfigOptionsAvailable`. Also spawned before `events` moves below.
        let (cfg_tx, cfg_rx) = mpsc::unbounded_channel::<Vec<AcpConfigOption>>();
        tokio::spawn(config_relay(
            events.clone(),
            session,
            conversation_id,
            cfg_rx,
        ));
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
            // Auto-allow only the read-only tools; the write tool (Feature B) is
            // deliberately excluded so every write is routed to the user for an
            // explicit Allow/Deny, never silently run by the agent.
            allow_tools: tool_catalog(&policy)
                .into_iter()
                .map(|t| t.name)
                .filter(|n| !crate::ai::is_write_tool(n))
                .collect(),
            permissions: Some(perm_tx),
            commands: Some(cmd_tx),
            config: Some(cfg_tx),
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

/// Relay the agent's advertised slash commands from one conversation to the UI as
/// `AiCommandsAvailable`. Connection-lifetime: ends when the conversation is torn
/// down (the agent drops its sender, closing `cmd_rx`).
async fn commands_relay(
    events: Events,
    session: Option<SessionId>,
    conversation_id: u64,
    mut cmd_rx: mpsc::UnboundedReceiver<Vec<AcpCommand>>,
) {
    while let Some(commands) = cmd_rx.recv().await {
        let commands = commands
            .into_iter()
            .map(|c| AiCommand {
                name: c.name,
                description: c.description,
            })
            .collect();
        emit(
            &events,
            session,
            Event::AiCommandsAvailable {
                conversation_id,
                commands,
            },
        );
    }
}

/// Relay the agent's session config selectors (model / reasoning) from one
/// conversation to the UI as `AiConfigOptionsAvailable`. Connection-lifetime, like
/// `commands_relay`.
async fn config_relay(
    events: Events,
    session: Option<SessionId>,
    conversation_id: u64,
    mut cfg_rx: mpsc::UnboundedReceiver<Vec<AcpConfigOption>>,
) {
    while let Some(options) = cfg_rx.recv().await {
        emit(
            &events,
            session,
            Event::AiConfigOptionsAvailable {
                conversation_id,
                options: options.iter().map(map_config_option).collect(),
            },
        );
    }
}

/// Apply a model / reasoning change on a conversation (`Command::AiSetConfigOption`):
/// issue the ACP `session/set_config_option` and emit the refreshed selector set so
/// the UI reconciles. A request for an unknown/dead conversation, or a failure, is
/// surfaced as an `AiError` on that conversation. A no-op if the conversation isn't
/// live yet (its agent starts on the first turn).
pub(crate) async fn set_config_option(
    manager: Arc<tokio::sync::Mutex<AcpManager>>,
    events: Events,
    session: Option<SessionId>,
    conversation_id: u64,
    config_id: String,
    value: String,
) {
    let agent = {
        let guard = manager.lock().await;
        guard.conversation_agent(conversation_id)
    };
    let Some(agent) = agent else {
        return;
    };
    match agent.set_config(config_id, value).await {
        Ok(Ok(options)) => emit(
            &events,
            session,
            Event::AiConfigOptionsAvailable {
                conversation_id,
                options: options.iter().map(map_config_option).collect(),
            },
        ),
        Ok(Err(e)) => emit(
            &events,
            session,
            Event::AiError {
                conversation_id,
                message: e.to_string(),
            },
        ),
        // The agent connection ended before answering — leave the dropdown as-is.
        Err(_) => {}
    }
}

/// Map a `red-acp` config option to its UI-facing twin.
fn map_config_option(option: &AcpConfigOption) -> AiConfigOption {
    AiConfigOption {
        id: option.id.clone(),
        name: option.name.clone(),
        category: match option.category {
            AcpConfigCategory::Model => AiConfigCategory::Model,
            AcpConfigCategory::Reasoning => AiConfigCategory::Reasoning,
            AcpConfigCategory::Mode => AiConfigCategory::Mode,
            AcpConfigCategory::Other => AiConfigCategory::Other,
        },
        current_value: option.current_value.clone(),
        choices: option
            .choices
            .iter()
            .map(|c| AiConfigChoice {
                value: c.value.clone(),
                name: c.name.clone(),
                description: c.description.clone(),
            })
            .collect(),
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
