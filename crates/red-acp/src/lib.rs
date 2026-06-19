//! ACP backend for the assistant — drives the user's Claude **subscription** by
//! running Claude Code as an external agent over the Agent Client Protocol, the
//! same way Zed does. The agent owns its OAuth/billing (`/login`); Red never sees
//! the tokens. This is the second provider behind the shipped `red-ai` API-key
//! path; both feed the same `Command::AiTurn`/`Event::AiDelta` plumbing.
//!
//! The crate is pure transport: it speaks ACP to the agent and emits the same
//! delta categories `red-ai` does ([`AcpDelta`]). Grounding (the DB tools) is a
//! localhost MCP server the *service* hosts and hands to the agent via
//! [`McpGrounding`] — `red-acp` only forwards its URL + nonce in `session/new`.
//!
//! See `examples/spike.rs` for a standalone end-to-end run against a real agent.

mod conversation;
mod types;

pub use conversation::AcpConversation;
pub use types::{
    AcpConfig, AcpDelta, AcpError, AcpPermission, AcpStop, AcpTurnResult, AcpUsage, McpGrounding,
    DEFAULT_AGENT_COMMAND,
};
