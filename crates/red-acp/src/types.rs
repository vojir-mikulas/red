//! The vocabulary that crosses the `red-acp` seam. Mirrors `red-ai`'s delta
//! shape (text / thinking / tool activity) so the service relay and the panel are
//! reused unchanged — only the backend that produces them differs.

use std::path::PathBuf;

use tokio::sync::{mpsc, oneshot};

/// The default agent: Claude Code in ACP mode, fetched on demand via npx. The
/// agent owns the subscription `/login` and billing — Red never sees the tokens.
pub const DEFAULT_AGENT_COMMAND: &str = "npx -y @agentclientprotocol/claude-agent-acp";

/// One streamed increment of an assistant turn, mapped from an ACP
/// `session/update`. Same categories `red-ai` already streams.
#[derive(Debug, Clone)]
pub enum AcpDelta {
    /// A chunk of visible answer text (`AgentMessageChunk`).
    Text(String),
    /// A chunk of the agent's reasoning (`AgentThoughtChunk`).
    Thinking(String),
    /// The agent began a tool call (`ToolCall`).
    ToolStarted { name: String },
    /// A tool call reached a terminal state (`ToolCallUpdate`); `ok` is false on failure.
    ToolFinished { name: String, ok: bool },
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

/// A localhost MCP server to hand the agent in `session/new` — Red's read-only DB
/// tools. `token` is the bearer nonce sent as the `Authorization` header.
#[derive(Debug, Clone)]
pub struct McpGrounding {
    pub name: String,
    pub url: String,
    pub token: String,
}

/// A tool-call permission the agent asked for that Red did **not** auto-allow
/// (M-S2). The conversation forwards it out of band for a user decision and
/// blocks the agent's tool call until the answer arrives on `decide` — sending
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
    /// means deny-by-default (no UI wired) — the safe choice.
    pub permissions: Option<mpsc::UnboundedSender<AcpPermission>>,
}

/// Errors the ACP backend can raise. Kept coarse — the service maps them to an
/// `AiError` event; the panel shows the message.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// The agent binary/command could not be launched (Node/Claude Code missing).
    #[error("could not start the agent — is Node.js / Claude Code installed? ({0})")]
    Spawn(String),
    /// The agent reported an ACP-level failure.
    #[error("agent error: {0}")]
    Protocol(String),
    /// The conversation's connection has ended (agent exited / was torn down).
    #[error("the assistant connection has ended")]
    Closed,
}
