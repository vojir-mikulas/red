//! A long-lived ACP conversation: one spawned agent + one session, kept alive
//! across turns. The connection's own message loop drives streaming
//! (`session/update` → [`AcpDelta`] over the active turn's sink); a command
//! channel feeds prompts in and a cancel maps to `session/cancel`.
//!
//! The whole connection lives inside one `connect_with` future (spawned as a
//! task). Startup (`initialize` → auth → `session/new` with the MCP grounding
//! server) is awaited before [`AcpConversation::start`] returns, so the caller
//! learns immediately whether the agent came up. Dropping the conversation drops
//! the command sender, which ends the turn loop and tears the agent down.

use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use agent_client_protocol::schema::{
    AuthenticateRequest, CancelNotification, ClientCapabilities, ContentBlock,
    FileSystemCapabilities, HttpHeader, Implementation, InitializeRequest, McpServer,
    McpServerHttp, NewSessionRequest, PermissionOption, PermissionOptionId, PermissionOptionKind,
    PlanEntry, PlanEntryStatus, PromptRequest, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOptions, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, StopReason, TextContent, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolKind,
};
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo};
use red_core::{ActivityKind, ActivityStatus, PlanStep, PlanStepStatus};
use tokio::sync::{mpsc, oneshot};

use crate::types::{
    AcpCommand, AcpConfig, AcpConfigCategory, AcpConfigChoice, AcpConfigOption, AcpDelta, AcpError,
    AcpPermission, AcpStop, AcpTurnResult, AcpUsage, McpGrounding,
};

/// The active turn's delta sink, swapped in before each prompt and cleared after.
/// Shared between the connection's notification handler and the turn loop.
type SinkCell = Arc<Mutex<Option<mpsc::UnboundedSender<AcpDelta>>>>;

/// Lock a cell, recovering from poisoning instead of panicking. These cells hold
/// a turn's sink and running usage; the streaming notification handler and the
/// turn loop share them, so a panic on one side must not cascade into a poisoned
/// `unwrap()` that takes down the whole turn task; the inner value is always a
/// valid (if stale) snapshot to keep working from.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
/// The latest usage seen this turn, returned in the turn result.
type UsageCell = Arc<Mutex<AcpUsage>>;

/// A reply channel for one completed (or failed) turn.
type TurnReply = oneshot::Sender<Result<AcpTurnResult, AcpError>>;
/// A take-once readiness signal fired when the session is up (or fails to start).
type ReadyCell = Arc<Mutex<Option<oneshot::Sender<Result<(), AcpError>>>>>;

/// A reply channel for a `session/set_config_option` request: the refreshed option
/// set on success.
type ConfigReply = oneshot::Sender<Result<Vec<AcpConfigOption>, AcpError>>;

/// A command sent into the live connection.
enum Cmd {
    Prompt {
        text: String,
        sink: mpsc::UnboundedSender<AcpDelta>,
        done: TurnReply,
    },
    Cancel,
    /// Change a session config selector (model / reasoning) between turns.
    SetConfig {
        config_id: String,
        value: String,
        done: ConfigReply,
    },
}

/// A handle to a live ACP conversation. Cheap to clone; the agent stays up while
/// any clone lives *and* the service keeps the conversation's MCP server alive.
#[derive(Clone)]
pub struct AcpConversation {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
}

impl AcpConversation {
    /// Spawn the agent, complete the handshake (restricted capabilities), run auth
    /// if the agent asks, and open a session with the MCP grounding server
    /// attached. Returns once the session is ready (or fails to come up).
    pub async fn start(config: AcpConfig) -> Result<Self, AcpError> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        tokio::spawn(run_connection(config, cmd_rx, ready_tx));
        match ready_rx.await {
            Ok(result) => result.map(|()| Self { cmd_tx }),
            // The task ended before signalling readiness; treat as a spawn failure.
            Err(_) => Err(AcpError::Closed),
        }
    }

    /// Send one prompt. Streamed deltas arrive on `sink`; the returned receiver
    /// resolves with the turn's result (usage + stop reason) or an error.
    pub fn prompt(
        &self,
        text: String,
        sink: mpsc::UnboundedSender<AcpDelta>,
    ) -> oneshot::Receiver<Result<AcpTurnResult, AcpError>> {
        let (done, done_rx) = oneshot::channel();
        if let Err(mpsc::error::SendError(Cmd::Prompt { done, .. })) =
            self.cmd_tx.send(Cmd::Prompt { text, sink, done })
        {
            let _ = done.send(Err(AcpError::Closed));
        }
        done_rx
    }

    /// Cancel the in-flight turn, if any (maps to `session/cancel`).
    pub fn cancel(&self) {
        let _ = self.cmd_tx.send(Cmd::Cancel);
    }

    /// Change a session config selector (model / reasoning) by `config_id`/`value`
    /// (maps to `session/set_config_option`). The returned receiver resolves with the
    /// refreshed option set, or an error.
    pub fn set_config(
        &self,
        config_id: String,
        value: String,
    ) -> oneshot::Receiver<Result<Vec<AcpConfigOption>, AcpError>> {
        let (done, done_rx) = oneshot::channel();
        if let Err(mpsc::error::SendError(Cmd::SetConfig { done, .. })) =
            self.cmd_tx.send(Cmd::SetConfig {
                config_id,
                value,
                done,
            })
        {
            let _ = done.send(Err(AcpError::Closed));
        }
        done_rx
    }

    /// Whether the connection task is still running. A closed command channel
    /// means the connection ended (the agent exited or crashed), so the next
    /// prompt would only ever return [`AcpError::Closed`]. The service checks
    /// this to drop a dead conversation and start a fresh one instead of
    /// prompting a corpse.
    pub fn is_alive(&self) -> bool {
        !self.cmd_tx.is_closed()
    }
}

/// The whole connection lifecycle, run as a spawned task.
async fn run_connection(
    config: AcpConfig,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    ready_tx: oneshot::Sender<Result<(), AcpError>>,
) {
    let agent = match AcpAgent::from_str(&config.command) {
        Ok(agent) => agent,
        Err(e) => {
            let _ = ready_tx.send(Err(AcpError::Spawn(e.to_string())));
            return;
        }
    };

    let active_sink: SinkCell = Arc::new(Mutex::new(None));
    let usage_cell: UsageCell = Arc::new(Mutex::new(AcpUsage::default()));
    // `ready_tx` may fire from inside the closure (startup) or after it (a
    // connection that died before startup). A shared take-once cell handles both.
    let ready = Arc::new(Mutex::new(Some(ready_tx)));

    let sink_handler = active_sink.clone();
    let usage_handler = usage_cell.clone();
    // The agent's advertised slash commands go out of band (connection-lifetime),
    // not through the active turn's sink; they arrive right after the session opens.
    let commands_handler = config.commands.clone();
    // Session config selectors (model / reasoning): one clone updates the notification
    // handler on `ConfigOptionUpdate`, the other ships the initial set from `session/new`.
    let config_handler = config.config.clone();
    let config_initial = config.config.clone();
    let ready_closure = ready.clone();
    let cwd = config.cwd.clone();
    let mcp = config.mcp.clone();
    // Permission policy (M-S2): the auto-allow catalog and the user-decision sink,
    // shared into the request handler (which the connection may invoke per call).
    let allow_tools = Arc::new(config.allow_tools.clone());
    let permissions = config.permissions.clone();

    let result = agent_client_protocol::Client
        .builder()
        .name("red")
        // Stream updates onto the active turn's sink.
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                handle_update(
                    &notification.update,
                    &sink_handler,
                    &usage_handler,
                    &commands_handler,
                    &config_handler,
                );
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        // Permission requests (M-S2): auto-allow Red's read-only DB tools (the
        // agent is already capability-restricted to no filesystem/terminal); route
        // anything else to the user for a decision, defaulting to deny.
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let allow = decide_permission(&request, &allow_tools, &permissions).await;
                responder.respond(RequestPermissionResponse::new(resolve(
                    &request.options,
                    allow,
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, move |conn: ConnectionTo<Agent>| async move {
            let session_id = match start_session(&conn, &cwd, mcp.as_ref()).await {
                Ok((id, options)) => {
                    signal(&ready_closure, Ok(()));
                    // Ship the session's initial config selectors (model / reasoning),
                    // advertised in the `session/new` response.
                    if let Some(tx) = &config_initial {
                        if !options.is_empty() {
                            let _ = tx.send(options);
                        }
                    }
                    id
                }
                Err(e) => {
                    signal(&ready_closure, Err(AcpError::Protocol(e.to_string())));
                    return Ok(());
                }
            };
            run_turns(&conn, session_id, &mut cmd_rx, &active_sink, &usage_cell).await;
            Ok(())
        })
        .await;

    if let Err(e) = result {
        // If readiness hadn't fired yet, the agent never came up: the subprocess
        // failed to spawn (Node/Claude Code missing or unreachable) or the
        // transport died mid-handshake. Surface the friendly "is Node.js
        // installed?" hint, keeping the raw cause in the detail. If readiness had
        // already fired, this is a later teardown; just trace it.
        if !signal(&ready, Err(AcpError::Spawn(e.to_string()))) {
            tracing::debug!("ACP connection ended: {e}");
        }
    }
}

/// `initialize` (restricted caps) → auth (if required) → `session/new` with the
/// MCP grounding server. Returns the session id and the session's initial config
/// selectors (model / reasoning), if the agent advertises any.
async fn start_session(
    conn: &ConnectionTo<Agent>,
    cwd: &std::path::Path,
    mcp: Option<&McpGrounding>,
) -> Result<(SessionId, Vec<AcpConfigOption>), agent_client_protocol::Error> {
    let init = conn
        .send_request(
            InitializeRequest::new(ProtocolVersion::V1)
                .client_capabilities(restricted_capabilities())
                .client_info(Implementation::new("red", env!("CARGO_PKG_VERSION"))),
        )
        .block_task()
        .await?;

    // If the agent advertises an auth method, run the first one; for Claude
    // Code's subscription this triggers its own browser `/login`. Red never sees
    // the tokens. (When already logged in, no methods are advertised.)
    if let Some(method) = init.auth_methods.first() {
        conn.send_request(AuthenticateRequest::new(method.id().clone()))
            .block_task()
            .await?;
    }

    let mut request = NewSessionRequest::new(cwd.to_path_buf());
    if let Some(grounding) = mcp {
        let server =
            McpServerHttp::new(grounding.name.clone(), grounding.url.clone()).headers(vec![
                HttpHeader::new("Authorization", format!("Bearer {}", grounding.token)),
            ]);
        request = request.mcp_servers(vec![McpServer::Http(server)]);
    }
    let session = conn.send_request(request).block_task().await?;
    let options = session
        .config_options
        .as_deref()
        .map(map_config_options)
        .unwrap_or_default();
    tracing::debug!(
        config_options = options.len(),
        "acp: session opened (initial config options)"
    );
    Ok((session.session_id, options))
}

/// A static name for a session update variant, for diagnostic logging.
fn update_kind(update: &SessionUpdate) -> &'static str {
    match update {
        SessionUpdate::AgentMessageChunk(_) => "agent_message_chunk",
        SessionUpdate::AgentThoughtChunk(_) => "agent_thought_chunk",
        SessionUpdate::ToolCall(_) => "tool_call",
        SessionUpdate::ToolCallUpdate(_) => "tool_call_update",
        SessionUpdate::UsageUpdate(_) => "usage_update",
        SessionUpdate::AvailableCommandsUpdate(_) => "available_commands_update",
        SessionUpdate::ConfigOptionUpdate(_) => "config_option_update",
        SessionUpdate::CurrentModeUpdate(_) => "current_mode_update",
        SessionUpdate::Plan(_) => "plan",
        _ => "other",
    }
}

/// Pull prompts off the command channel and drive one ACP turn at a time. Streamed
/// updates flow through the connection's notification handler into `active_sink`.
async fn run_turns(
    conn: &ConnectionTo<Agent>,
    session_id: SessionId,
    cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>,
    active_sink: &SinkCell,
    usage_cell: &UsageCell,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        let (text, sink, done) = match cmd {
            Cmd::Prompt { text, sink, done } => (text, sink, done),
            // A Cancel with no active turn; nothing to do.
            Cmd::Cancel => continue,
            // Config changes (model / reasoning) happen between turns: issue the
            // request and reply with the refreshed option set.
            Cmd::SetConfig {
                config_id,
                value,
                done,
            } => {
                let reply = conn
                    .send_request(SetSessionConfigOptionRequest::new(
                        session_id.clone(),
                        config_id,
                        value.as_str(),
                    ))
                    .block_task()
                    .await
                    .map(|resp| map_config_options(&resp.config_options))
                    .map_err(|e| AcpError::Protocol(e.to_string()));
                let _ = done.send(reply);
                continue;
            }
        };
        *lock(active_sink) = Some(sink);
        *lock(usage_cell) = AcpUsage::default();

        let prompt = conn
            .send_request(PromptRequest::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new(text))],
            ))
            .block_task();
        tokio::pin!(prompt);

        // Await the turn while staying responsive to cancel. A closed channel
        // (None) during a turn also means "stop", so cancel and let it wind down.
        let outcome = loop {
            tokio::select! {
                result = &mut prompt => break result,
                ctl = cmd_rx.recv() => match ctl {
                    Some(Cmd::Cancel) | None => {
                        let _ = conn.send_notification(CancelNotification::new(session_id.clone()));
                    }
                    Some(Cmd::Prompt { done, .. }) => {
                        // The UI serializes turns; reject a concurrent one rather
                        // than interleave it into the running prompt. `Busy` marks it
                        // as the benign race it is (not surfaced as a user error).
                        let _ = done.send(Err(AcpError::Busy));
                    }
                    // The UI disables the selectors mid-turn, but reject defensively
                    // rather than mutate config while a prompt is streaming.
                    Some(Cmd::SetConfig { done, .. }) => {
                        let _ = done.send(Err(AcpError::Busy));
                    }
                },
            }
        };

        *lock(active_sink) = None;
        let usage = *lock(usage_cell);
        let reply = match outcome {
            Ok(response) => Ok(AcpTurnResult {
                usage,
                stop: map_stop(response.stop_reason),
            }),
            Err(e) => Err(AcpError::Protocol(e.to_string())),
        };
        let _ = done.send(reply);
    }
}

/// Map one streamed update onto an [`AcpDelta`] (and record usage), and forward the
/// agent's advertised slash commands out of band. Returns without sending for
/// updates we don't surface (e.g. mode changes).
fn handle_update(
    update: &SessionUpdate,
    active_sink: &SinkCell,
    usage_cell: &UsageCell,
    commands: &Option<mpsc::UnboundedSender<Vec<AcpCommand>>>,
    config: &Option<mpsc::UnboundedSender<Vec<AcpConfigOption>>>,
) {
    // Diagnostic: name every non-streaming update the agent sends, so we can tell
    // whether it advertises slash commands / config selectors at all.
    if !matches!(
        update,
        SessionUpdate::AgentMessageChunk(_) | SessionUpdate::AgentThoughtChunk(_)
    ) {
        tracing::debug!(kind = update_kind(update), "acp session update");
    }
    // Slash commands arrive out of band (no active turn), so they go on their own
    // channel rather than the turn sink.
    if let SessionUpdate::AvailableCommandsUpdate(update) = update {
        tracing::debug!(
            count = update.available_commands.len(),
            "acp: commands update"
        );
        if let Some(tx) = commands {
            let list = update
                .available_commands
                .iter()
                .map(|c| AcpCommand {
                    name: c.name.clone(),
                    description: c.description.clone(),
                })
                .collect();
            let _ = tx.send(list);
        }
        return;
    }
    // Config selectors (model / reasoning) likewise update out of band.
    if let SessionUpdate::ConfigOptionUpdate(update) = update {
        tracing::debug!(count = update.config_options.len(), "acp: config update");
        if let Some(tx) = config {
            let _ = tx.send(map_config_options(&update.config_options));
        }
        return;
    }
    let delta = match update {
        SessionUpdate::AgentMessageChunk(chunk) => Some(AcpDelta::Text(text_of(&chunk.content))),
        SessionUpdate::AgentThoughtChunk(chunk) => {
            Some(AcpDelta::Thinking(text_of(&chunk.content)))
        }
        SessionUpdate::ToolCall(call) => {
            // Every ACP tool call is top-level. We do NOT nest calls under a running
            // subagent: the agent runs subagents in *parallel* (and doesn't surface
            // their inner calls to us), so a temporal "what's open now" guess wrongly
            // nested independent subagents under each other. Delegations render as
            // sibling nodes; only the direct-provider path, which sets `parent`
            // explicitly, ever nests.
            let kind = match subagent_task(call) {
                Some(task) => ActivityKind::Subagent { task },
                None => ActivityKind::Tool {
                    name: call.title.clone(),
                    args_summary: acp_args_summary(&call.raw_input),
                },
            };
            Some(AcpDelta::ActivityStarted {
                id: call.tool_call_id.0.to_string(),
                parent: None,
                kind,
                status: map_tool_status(&call.status),
            })
        }
        // Forward status changes and/or streamed progress. A status-less update
        // carries progress content (the node's latest line of work), which refreshes
        // the detail without touching the lifecycle.
        SessionUpdate::ToolCallUpdate(update) => {
            let status = update.fields.status.as_ref().map(map_tool_status);
            let detail = acp_progress_detail(&update.fields.content);
            (status.is_some() || detail.is_some()).then(|| AcpDelta::ActivityUpdated {
                id: update.tool_call_id.0.to_string(),
                status,
                detail,
            })
        }
        SessionUpdate::Plan(plan) => Some(AcpDelta::PlanUpdated {
            steps: plan.entries.iter().map(map_plan_entry).collect(),
        }),
        SessionUpdate::UsageUpdate(usage) => {
            *lock(usage_cell) = AcpUsage {
                used_tokens: usage.used,
                context_tokens: usage.size,
                cost_usd: usage.cost.as_ref().map(|c| c.amount),
            };
            None
        }
        _ => None,
    };
    if let Some(delta) = delta {
        if let Some(tx) = lock(active_sink).as_ref() {
            let _ = tx.send(delta);
        }
    }
}

/// Map an ACP tool-call status onto the shared activity lifecycle. A just-opened
/// or in-progress call reads as `Running` in the timeline; `Pending` (awaiting
/// approval / streaming input) shows as `Pending`.
fn map_tool_status(status: &ToolCallStatus) -> ActivityStatus {
    match status {
        ToolCallStatus::Pending => ActivityStatus::Pending,
        ToolCallStatus::InProgress => ActivityStatus::Running,
        ToolCallStatus::Completed => ActivityStatus::Ok,
        ToolCallStatus::Failed => ActivityStatus::Failed,
        _ => ActivityStatus::Running,
    }
}

/// A one-line summary of a tool call's raw input for the timeline: the SQL's first
/// line or the table name, matching the direct-provider path. `None` when there's
/// no salient scalar argument.
fn acp_args_summary(raw_input: &Option<serde_json::Value>) -> Option<String> {
    let input = raw_input.as_ref()?;
    let salient = input
        .get("sql")
        .or_else(|| input.get("table"))
        .and_then(|v| v.as_str())?;
    let line = salient.split('\n').find(|l| !l.trim().is_empty())?.trim();
    Some(clip(line, 79))
}

/// A one-line progress summary from a tool-call update's streamed content: the first
/// non-empty line of its latest text block. This is the closest thing ACP gives us to
/// a subagent's "ongoing work" (it doesn't forward a delegate's inner tool calls), so
/// it surfaces as the node's live detail. `None` when the update carries no text.
fn acp_progress_detail(content: &Option<Vec<ToolCallContent>>) -> Option<String> {
    let blocks = content.as_ref()?;
    // The freshest text block, read back-to-front.
    let text = blocks.iter().rev().find_map(|c| match c {
        ToolCallContent::Content(inner) => {
            let t = text_of(&inner.content);
            (!t.trim().is_empty()).then_some(t)
        }
        _ => None,
    })?;
    let line = text.split('\n').find(|l| !l.trim().is_empty())?.trim();
    Some(clip(line, 100))
}

/// If this tool call is a delegated subagent (Claude Code's `Task` tool), return a
/// short description of its task; otherwise `None`. ACP carries no subagent tool
/// *kind*, so we key off the Task tool's signature: a `subagent_type` in the raw
/// input, or a bare `Task` title. Heuristic and centralized so it's easy to tune as
/// agents' conventions shift; a false negative just renders a plain tool row.
fn subagent_task(call: &ToolCall) -> Option<String> {
    let raw = call.raw_input.as_ref();
    let has_subagent_type = raw.and_then(|v| v.get("subagent_type")).is_some();
    let title = call.title.trim();
    let title_is_task = {
        let lower = title.to_ascii_lowercase();
        lower == "task" || lower.starts_with("task(") || lower.starts_with("task ")
    };
    if !has_subagent_type && !title_is_task {
        return None;
    }
    // Prefer a human description, then the subagent type, then the title itself.
    let field = |key: &str| {
        raw.and_then(|v| v.get(key))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let task = field("description")
        .or_else(|| field("prompt"))
        .or_else(|| field("subagent_type"))
        .unwrap_or(title);
    Some(clip(task, 120))
}

/// Truncate to `max` chars on a char boundary, appending an ellipsis when cut.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

/// Map an ACP plan entry onto a [`PlanStep`] for the turn's checklist.
fn map_plan_entry(entry: &PlanEntry) -> PlanStep {
    PlanStep {
        title: entry.content.clone(),
        status: match entry.status {
            PlanEntryStatus::Pending => PlanStepStatus::Pending,
            PlanEntryStatus::InProgress => PlanStepStatus::InProgress,
            PlanEntryStatus::Completed => PlanStepStatus::Completed,
            _ => PlanStepStatus::Pending,
        },
    }
}

/// Map ACP `config_options` to Red's [`AcpConfigOption`], keeping only single-select
/// selectors (the only kind on the stable build) and flattening grouped choices.
fn map_config_options(options: &[SessionConfigOption]) -> Vec<AcpConfigOption> {
    options.iter().filter_map(map_config_option).collect()
}

fn map_config_option(option: &SessionConfigOption) -> Option<AcpConfigOption> {
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    let choices = match &select.options {
        SessionConfigSelectOptions::Ungrouped(opts) => opts.iter().map(map_choice).collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|g| g.options.iter())
            .map(map_choice)
            .collect(),
        _ => Vec::new(),
    };
    Some(AcpConfigOption {
        id: option.id.0.to_string(),
        name: option.name.clone(),
        category: map_category(option.category.as_ref()),
        current_value: select.current_value.0.to_string(),
        choices,
    })
}

fn map_choice(
    option: &agent_client_protocol::schema::SessionConfigSelectOption,
) -> AcpConfigChoice {
    AcpConfigChoice {
        value: option.value.0.to_string(),
        name: option.name.clone(),
        description: option.description.clone(),
    }
}

fn map_category(category: Option<&SessionConfigOptionCategory>) -> AcpConfigCategory {
    match category {
        Some(SessionConfigOptionCategory::Model) => AcpConfigCategory::Model,
        Some(SessionConfigOptionCategory::ThoughtLevel) => AcpConfigCategory::Reasoning,
        Some(SessionConfigOptionCategory::Mode) => AcpConfigCategory::Mode,
        _ => AcpConfigCategory::Other,
    }
}

fn text_of(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(t) => t.text.clone(),
        other => format!("[{other:?}]"),
    }
}

/// No filesystem, no terminal: the agent is corralled to the MCP DB tools.
fn restricted_capabilities() -> ClientCapabilities {
    ClientCapabilities::default()
        .fs(FileSystemCapabilities::new()
            .read_text_file(false)
            .write_text_file(false))
        .terminal(false)
}

/// Decide one permission request (M-S2): `true` allows the agent's tool call.
/// Red's read-only DB tools auto-allow; anything else is forwarded to the user
/// (via `permissions`) and blocks on their answer, defaulting to deny when no UI
/// is wired or the sink has gone away.
async fn decide_permission(
    request: &RequestPermissionRequest,
    allow_tools: &[String],
    permissions: &Option<mpsc::UnboundedSender<AcpPermission>>,
) -> bool {
    if is_auto_allowed(&request.tool_call, allow_tools) {
        return true;
    }
    let Some(tx) = permissions else {
        return false;
    };
    let (decide, decided) = oneshot::channel();
    let perm = AcpPermission {
        title: tool_title(&request.tool_call),
        detail: tool_detail(&request.tool_call),
        decide,
    };
    if tx.send(perm).is_err() {
        return false;
    }
    // The UI answers (or the sender is dropped on teardown → deny).
    decided.await.unwrap_or(false)
}

/// Whether a tool call is one of Red's known read-only DB tools and so may run
/// without prompting. A mutating tool kind is never auto-allowed even if its
/// title matches; the gate for a future write tool stays closed by default.
fn is_auto_allowed(tool_call: &ToolCallUpdate, allow_tools: &[String]) -> bool {
    if matches!(
        tool_call.fields.kind,
        Some(ToolKind::Edit | ToolKind::Delete | ToolKind::Move | ToolKind::Execute)
    ) {
        return false;
    }
    let title = tool_title(tool_call);
    allow_tools
        .iter()
        .any(|name| title_names_tool(&title, name))
}

/// Whether `title` *is* the tool `tool`, not merely mentions it. ACP doesn't carry
/// the structured MCP tool name into the permission request, so we fall back to the
/// call's title. But the MCP convention is that the title IS the tool name,
/// optionally server-qualified: `run_select`, `red-db: run_select`, or
/// `mcp__red-db__run_select`. We reduce the title to that trailing identifier and
/// require it to equal the tool name exactly. That keeps the legitimate forms while
/// refusing to auto-allow a tool whose title merely *contains* our name inside a
/// longer human sentence (e.g. an unrelated `"fetch run_select docs"` Read tool);
/// a bare-token match would wrongly green-light it. (Residual: a malicious agent
/// can still title its own tool exactly `run_select`; the non-write `ToolKind`
/// guard in `is_auto_allowed` and deny-default elsewhere are the backstop.)
fn title_names_tool(title: &str, tool: &str) -> bool {
    let tool = tool.to_ascii_lowercase();
    let lower = title.to_ascii_lowercase();
    // Strip a `server:`/path qualifier, then an `mcp__server__` prefix, leaving the
    // bare tool identifier, which must match the tool name in full.
    let ident = lower
        .rsplit([':', '/'])
        .next()
        .unwrap_or_default()
        .trim()
        .rsplit("__")
        .next()
        .unwrap_or_default()
        .trim();
    ident == tool
}

/// The agent's human-readable title for a tool call (used for both matching and
/// the user prompt); falls back to a generic label when absent.
fn tool_title(tool_call: &ToolCallUpdate) -> String {
    tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "a tool".to_string())
}

/// A compact one-line rendering of the tool's raw input for the prompt, if any.
fn tool_detail(tool_call: &ToolCallUpdate) -> Option<String> {
    tool_call
        .fields
        .raw_input
        .as_ref()
        .map(|v| v.to_string())
        .filter(|s| s != "null" && s != "{}")
}

/// Turn an allow/deny decision into a concrete ACP outcome by picking the
/// matching option the agent offered. Prefers the "once" variant; denying with no
/// reject option falls back to `Cancelled`, which the agent treats as a refusal.
fn resolve(options: &[PermissionOption], allow: bool) -> RequestPermissionOutcome {
    let (primary, secondary) = if allow {
        (
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        )
    } else {
        (
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        )
    };
    match pick(options, primary).or_else(|| pick(options, secondary)) {
        Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
        None => RequestPermissionOutcome::Cancelled,
    }
}

/// The id of the first option of `kind`, if the agent offered one.
fn pick(options: &[PermissionOption], kind: PermissionOptionKind) -> Option<PermissionOptionId> {
    options
        .iter()
        .find(|o| o.kind == kind)
        .map(|o| o.option_id.clone())
}

fn map_stop(stop: StopReason) -> AcpStop {
    match stop {
        StopReason::EndTurn => AcpStop::EndTurn,
        StopReason::Cancelled => AcpStop::Cancelled,
        StopReason::MaxTokens => AcpStop::MaxTokens,
        StopReason::Refusal => AcpStop::Refusal,
        _ => AcpStop::Other,
    }
}

/// Fire the take-once readiness signal (idempotent; later calls are no-ops).
/// Fire the take-once readiness signal. Returns `true` if this call delivered the
/// result (the signal was still pending: startup hadn't completed), `false` if
/// readiness had already fired (so this is a later teardown).
fn signal(ready: &ReadyCell, result: Result<(), AcpError>) -> bool {
    if let Some(tx) = lock(ready).take() {
        let _ = tx.send(result);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{PermissionOption, ToolCallId, ToolCallUpdateFields};
    use serde_json::json;

    fn call(title: &str, kind: Option<ToolKind>) -> ToolCallUpdate {
        let mut fields = ToolCallUpdateFields::new().title(title.to_string());
        fields.kind = kind;
        ToolCallUpdate::new(ToolCallId::new("call-1"), fields)
    }

    fn db_tools() -> Vec<String> {
        ["list_schema", "describe_table", "run_select", "explain"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    #[test]
    fn tool_status_maps_onto_the_activity_lifecycle() {
        assert_eq!(
            map_tool_status(&ToolCallStatus::Pending),
            ActivityStatus::Pending
        );
        assert_eq!(
            map_tool_status(&ToolCallStatus::InProgress),
            ActivityStatus::Running
        );
        assert_eq!(
            map_tool_status(&ToolCallStatus::Completed),
            ActivityStatus::Ok
        );
        assert_eq!(
            map_tool_status(&ToolCallStatus::Failed),
            ActivityStatus::Failed
        );
    }

    #[test]
    fn args_summary_extracts_sql_or_table_first_line() {
        assert_eq!(
            acp_args_summary(&Some(json!({ "sql": "SELECT 1\nFROM t" }))),
            Some("SELECT 1".to_string())
        );
        assert_eq!(
            acp_args_summary(&Some(json!({ "table": "public.users" }))),
            Some("public.users".to_string())
        );
        assert_eq!(acp_args_summary(&Some(json!({ "other": 1 }))), None);
        assert_eq!(acp_args_summary(&None), None);
    }

    #[test]
    fn detects_subagents_by_type_or_task_title() {
        use agent_client_protocol::schema::ToolCall;
        // A Task tool call carrying a subagent_type is a subagent; the description
        // becomes its label.
        let mut task = ToolCall::new(ToolCallId::new("t1"), "Task");
        task.raw_input = Some(json!({
            "subagent_type": "explorer",
            "description": "Map the schema",
        }));
        assert_eq!(subagent_task(&task).as_deref(), Some("Map the schema"));

        // A bare "Task" title with no input still counts; falls back to the title.
        let bare = ToolCall::new(ToolCallId::new("t2"), "Task");
        assert_eq!(subagent_task(&bare).as_deref(), Some("Task"));

        // An ordinary DB tool is not a subagent.
        let mut run = ToolCall::new(ToolCallId::new("t3"), "run_select");
        run.raw_input = Some(json!({ "sql": "SELECT 1" }));
        assert_eq!(subagent_task(&run), None);
    }

    #[test]
    fn plan_entry_maps_content_and_status() {
        use agent_client_protocol::schema::PlanEntryPriority;
        let entry = PlanEntry::new(
            "Count the users",
            PlanEntryPriority::Medium,
            PlanEntryStatus::InProgress,
        );
        let step = map_plan_entry(&entry);
        assert_eq!(step.title, "Count the users");
        assert_eq!(step.status, PlanStepStatus::InProgress);
    }

    #[test]
    fn auto_allows_known_readonly_tools() {
        let tools = db_tools();
        // The agent's MCP title carries the tool name.
        assert!(is_auto_allowed(
            &call("run_select", Some(ToolKind::Read)),
            &tools
        ));
        assert!(is_auto_allowed(
            &call("red-db: describe_table", None),
            &tools
        ));
    }

    #[test]
    fn auto_allow_requires_a_whole_token_not_a_substring() {
        let tools = db_tools();
        // A look-alike that merely *contains* a known tool name is NOT auto-allowed;
        // a loose substring match would wrongly wave these through.
        assert!(!is_auto_allowed(
            &call("run_select_then_drop", None),
            &tools
        ));
        assert!(!is_auto_allowed(&call("describe_table_evil", None), &tools));
    }

    #[test]
    fn auto_allow_requires_the_title_to_be_the_tool_not_to_mention_it() {
        let tools = db_tools();
        // A non-DB tool whose title merely *mentions* our tool name inside a longer
        // human sentence must NOT auto-allow; only the bare (optionally
        // server-qualified) identifier counts.
        assert!(!is_auto_allowed(
            &call("fetch run_select docs from the web", Some(ToolKind::Read)),
            &tools
        ));
        assert!(!is_auto_allowed(
            &call("read file run_select.sql", Some(ToolKind::Read)),
            &tools
        ));
        // The fully MCP-qualified form is still recognised.
        assert!(is_auto_allowed(
            &call("mcp__red-db__run_select", Some(ToolKind::Read)),
            &tools
        ));
    }

    #[test]
    fn prompts_for_unknown_or_mutating_tools() {
        let tools = db_tools();
        // Not one of ours → prompt.
        assert!(!is_auto_allowed(
            &call("write_file", Some(ToolKind::Edit)),
            &tools
        ));
        // Named like ours but flagged mutating → never silently allowed.
        assert!(!is_auto_allowed(
            &call("run_select", Some(ToolKind::Execute)),
            &tools
        ));
    }

    #[test]
    fn resolve_picks_allow_then_reject_then_cancels() {
        let opts = vec![
            PermissionOption::new("ok", "Allow", PermissionOptionKind::AllowOnce),
            PermissionOption::new("no", "Deny", PermissionOptionKind::RejectOnce),
        ];
        match resolve(&opts, true) {
            RequestPermissionOutcome::Selected(s) => assert_eq!(&*s.option_id.0, "ok"),
            other => panic!("expected allow, got {other:?}"),
        }
        match resolve(&opts, false) {
            RequestPermissionOutcome::Selected(s) => assert_eq!(&*s.option_id.0, "no"),
            other => panic!("expected reject, got {other:?}"),
        }
        // No reject option offered → deny falls back to Cancelled.
        let allow_only = vec![PermissionOption::new(
            "ok",
            "Allow",
            PermissionOptionKind::AllowOnce,
        )];
        assert!(matches!(
            resolve(&allow_only, false),
            RequestPermissionOutcome::Cancelled
        ));
    }

    #[test]
    fn detail_renders_raw_input_but_skips_empties() {
        let mut fields = ToolCallUpdateFields::new().title("run_select".to_string());
        fields.raw_input = Some(json!({ "sql": "SELECT 1" }));
        let detail = tool_detail(&ToolCallUpdate::new(ToolCallId::new("c"), fields)).unwrap();
        assert!(detail.contains("SELECT 1"));

        let mut empty = ToolCallUpdateFields::new();
        empty.raw_input = Some(json!({}));
        assert!(tool_detail(&ToolCallUpdate::new(ToolCallId::new("c"), empty)).is_none());
    }
}
