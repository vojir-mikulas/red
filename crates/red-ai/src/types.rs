//! Provider-agnostic conversation, tool, and streaming types. These are the only
//! types that cross the [`AiProvider`](crate::AiProvider) seam: no vendor wire
//! format leaks above it, the same way `DatabaseDriver` is the only seam to
//! database engines. A second provider (OpenAI, a local Ollama endpoint) maps its
//! own wire format to and from these.

use serde::{Deserialize, Serialize};

/// One tool the model may call, named and described in plain terms with a JSON
/// Schema for its input. The service builds the catalog (see the assistant tool
/// table); the provider renders it into its own wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema (`{"type":"object", ...}`) describing the tool's input.
    pub input_schema: serde_json::Value,
}

/// Who authored a conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One content block of a message. The agentic loop appends the model's
/// `Assistant` turn (text + any `ToolUse` blocks) verbatim, then a `User` turn
/// carrying one `ToolResult` per `ToolUse`, and re-asks: the standard Messages
/// API tool-use loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Model- or user-authored prose.
    Text { text: String },
    /// A summarized-thinking block. Carried so the assistant turn round-trips
    /// unchanged on the next loop iteration; the Messages API rejects a tool-use
    /// follow-up whose thinking blocks were dropped or edited. `signature` is the
    /// opaque attestation echoed back verbatim.
    Thinking { text: String, signature: String },
    /// An encrypted thinking block, opaque to us; echoed back as-is.
    RedactedThinking { data: String },
    /// The model wants to run a tool. `id` ties it to its result.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The harness's reply to a `ToolUse`, fed back on the next turn.
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// One conversation message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

/// Everything one model turn needs. The system prompt and tool catalog are stable
/// across a conversation, so the provider caches them (Anthropic `cache_control`).
#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub model: String,
    pub max_tokens: u32,
    /// Visible "thinking…" summary affordance when `true` (Anthropic adaptive
    /// thinking with `display: "summarized"`).
    pub show_thinking: bool,
    pub system: String,
    pub tools: Vec<ToolDef>,
    pub messages: Vec<Message>,
}

/// A streamed increment of the current turn, pushed over the provider's channel
/// as tokens arrive so the UI renders progressively.
#[derive(Debug, Clone)]
pub enum Delta {
    /// A chunk of summarized thinking text (only when `show_thinking`).
    Thinking(String),
    /// A chunk of visible answer text.
    Text(String),
    /// The model began a tool call (name known; input still streaming).
    ToolUseStarted { id: String, name: String },
}

/// Why a turn ended. Drives the agentic loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished its answer; the loop ends.
    EndTurn,
    /// The model requested one or more tools; run them and re-ask.
    ToolUse,
    /// Hit the `max_tokens` ceiling.
    MaxTokens,
    /// Safety classifier or model declined.
    Refusal,
    /// Anything else the provider reports.
    Other,
}

/// Token accounting for a turn, surfaced to the UI footer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
}

/// The assembled result of one model turn: the full assistant message (echoed
/// back unchanged on the next loop iteration) plus why it stopped.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub message: Message,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

/// Errors a provider can raise. Kept coarse: the UI shows the message; the
/// service maps it to an `AiError` event.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("missing API key for {0}")]
    MissingKey(String),
    #[error("authentication failed (check API key)")]
    Auth,
    #[error("rate limited; retry shortly")]
    RateLimited,
    #[error("network error: {0}")]
    Network(String),
    /// The model declined the request (safety classifier / refusal).
    #[error("the model declined to respond")]
    Refused,
    #[error("provider error: {0}")]
    Provider(String),
    /// The turn was cancelled out-of-band (user pressed stop).
    #[error("cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, AiError>;
