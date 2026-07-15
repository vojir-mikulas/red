//! The vocabulary that crosses the `red-acp` seam. Mirrors `red-ai`'s delta
//! shape (text / thinking / tool activity) so the service relay and the panel are
//! reused unchanged; only the backend that produces them differs.

use std::path::PathBuf;

use red_core::{ActivityId, ActivityKind, ActivityStatus, PlanStep};
use tokio::sync::{mpsc, oneshot};

/// The default agent: Claude Code in ACP mode, fetched on demand via npx. The
/// agent owns the subscription `/login` and billing; Red never sees the tokens.
pub const DEFAULT_AGENT_COMMAND: &str = "npx -y @agentclientprotocol/claude-agent-acp";

/// One streamed increment of an assistant turn, mapped from an ACP
/// `session/update`. Text and thinking append; the activity/plan variants feed the
/// same agent-activity timeline the direct-provider path builds, keyed by the
/// agent's `tool_call_id`, so the panel renders both backends identically.
#[derive(Debug, Clone)]
pub enum AcpDelta {
    /// A chunk of visible answer text (`AgentMessageChunk`).
    Text(String),
    /// A chunk of the agent's reasoning (`AgentThoughtChunk`).
    Thinking(String),
    /// A tool call opened (`ToolCall`); `id` is its `tool_call_id`, `parent` nests
    /// it under a subagent node (Phase 1) or is `None` at top level.
    ActivityStarted {
        id: ActivityId,
        parent: Option<ActivityId>,
        kind: ActivityKind,
        status: ActivityStatus,
    },
    /// A tool call changed state and/or streamed progress (`ToolCallUpdate`), matched
    /// by `id`. `status` is `None` for a detail-only refresh.
    ActivityUpdated {
        id: ActivityId,
        status: Option<ActivityStatus>,
        detail: Option<String>,
    },
    /// The agent (re)published its plan checklist (`Plan`).
    PlanUpdated { steps: Vec<PlanStep> },
}

/// One slash command the agent advertises (ACP `AvailableCommandsUpdate`), e.g.
/// `login` / "Sign in to your account". The `name` carries no leading slash; the
/// composer adds it. Surfaced so the UI can offer a `/`-triggered command picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpCommand {
    pub name: String,
    pub description: String,
}

/// One session configuration selector the agent advertises (ACP `config_options`),
/// e.g. a model picker or a reasoning-level picker. Only single-select (`Select`)
/// options are surfaced; the UI renders each as a dropdown. The `id`/`value` strings
/// are opaque agent identifiers round-tripped back via `AcpConversation::set_config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpConfigOption {
    pub id: String,
    pub name: String,
    pub category: AcpConfigCategory,
    /// The currently-selected choice's `value`.
    pub current_value: String,
    pub choices: Vec<AcpConfigChoice>,
}

/// One choice within an [`AcpConfigOption`]'s dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpConfigChoice {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

/// What an [`AcpConfigOption`] controls, mapped from ACP's category. Drives where the
/// UI places the dropdown; `Other` covers categories Red doesn't surface yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpConfigCategory {
    Model,
    Reasoning,
    Mode,
    Other,
}

/// Token / cost accounting for a turn, taken from the latest `UsageUpdate`.
#[derive(Debug, Clone, Copy, Default)]
pub struct AcpUsage {
    /// Tokens currently in the agent's context window.
    pub used_tokens: u64,
    /// The agent's total context window size.
    pub context_tokens: u64,
    /// Cumulative session cost in USD, if the agent reports it.
    pub cost_usd: Option<f64>,
}

/// Why an ACP turn ended (mapped from the agent's `StopReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpStop {
    EndTurn,
    Cancelled,
    MaxTokens,
    Refusal,
    Other,
}

/// The result of one completed turn.
#[derive(Debug, Clone, Copy)]
pub struct AcpTurnResult {
    pub usage: AcpUsage,
    pub stop: AcpStop,
}

/// A localhost MCP server to hand the agent in `session/new`: Red's read-only DB
/// tools. `token` is the bearer nonce sent as the `Authorization` header.
#[derive(Debug, Clone)]
pub struct McpGrounding {
    pub name: String,
    pub url: String,
    pub token: String,
}

/// A tool-call permission the agent asked for that Red did **not** auto-allow
/// (M-S2). The conversation forwards it out of band for a user decision and
/// blocks the agent's tool call until the answer arrives on `decide`: sending
/// `true` runs the tool, `false` (or dropping the sender) denies it.
#[derive(Debug)]
pub struct AcpPermission {
    /// What the agent wants to do (the tool call's human-readable title).
    pub title: String,
    /// A compact rendering of the tool's input, if the agent provided any.
    pub detail: Option<String>,
    /// The user's decision sink: `true` allows the call, `false`/drop denies it.
    pub decide: oneshot::Sender<bool>,
}

/// Everything needed to start one ACP conversation.
#[derive(Debug, Clone)]
pub struct AcpConfig {
    /// The shell command that launches the agent (e.g. the npx invocation).
    pub command: String,
    /// The session's working directory (also where the agent loads its own config).
    pub cwd: PathBuf,
    /// The DB grounding server, if a session is connected.
    pub mcp: Option<McpGrounding>,
    /// Tool names to auto-approve without prompting (M-S2): Red's read-only DB
    /// tools. A permission request that matches one of these runs silently; the
    /// agent is already capability-restricted to no filesystem/terminal.
    pub allow_tools: Vec<String>,
    /// Where non-auto-allowed permission requests go for a user decision. `None`
    /// means deny-by-default (no UI wired), the safe choice.
    pub permissions: Option<mpsc::UnboundedSender<AcpPermission>>,
    /// Where the agent's advertised slash commands are forwarded as they arrive
    /// (`AvailableCommandsUpdate`), for the `/`-command picker. Connection-lifetime,
    /// not per-turn: commands typically land right after the session opens. `None`
    /// drops them.
    pub commands: Option<mpsc::UnboundedSender<Vec<AcpCommand>>>,
    /// Where the agent's session config selectors (model / reasoning) are forwarded:
    /// the initial set from `session/new` and any later `ConfigOptionUpdate`.
    /// Connection-lifetime, like `commands`. `None` drops them.
    pub config: Option<mpsc::UnboundedSender<Vec<AcpConfigOption>>>,
}

/// Errors the ACP backend can raise. Kept coarse: the service maps them to an
/// `AiError` event; the panel shows the message.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// The agent binary/command could not be launched (Node/Claude Code missing).
    #[error("could not start the agent; is Node.js / Claude Code installed? ({0})")]
    Spawn(String),
    /// The agent reported an ACP-level failure.
    #[error("agent error: {0}")]
    Protocol(String),
    /// A prompt / config change arrived while a turn was already running. A benign,
    /// expected race (the UI serializes turns and disables selectors mid-turn); it
    /// should be handled quietly, never surfaced to the user as an error.
    #[error("a turn is already in progress")]
    Busy,
    /// The conversation's connection has ended (agent exited / was torn down).
    #[error("the agent connection has ended")]
    Closed,
}
