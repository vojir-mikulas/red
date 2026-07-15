//! The AI assistant provider seam. `red-ai` is to language models what
//! `red-driver` is to database engines: one object-safe trait, one impl per
//! backend, and no vendor wire format above the seam. The service holds a
//! provider as `Arc<dyn AiProvider>` and drives a single model turn at a time;
//! the agentic loop (model → tool call → model) lives on the service thread, not
//! here.
//!
//! [`AnthropicProvider`] is the first impl: the Claude Messages API over SSE with
//! adaptive thinking, tool use, and prompt-cached system + tools. OpenAI and a
//! local (Ollama / OpenAI-compatible) provider drop in behind the same trait.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;

mod anthropic;
mod types;

pub use anthropic::{AnthropicProvider, is_safe_base_url};
pub use types::{
    AiError, ContentBlock, Delta, Message, Result, Role, StopReason, ToolDef, TurnOutcome,
    TurnRequest, Usage,
};

/// Default deep-reasoning model.
pub const MODEL_OPUS: &str = "claude-opus-4-8";
/// Cheap / fast lane.
pub const MODEL_HAIKU: &str = "claude-haiku-4-5";

/// A cloneable cancel flag the service flips when the user stops a turn. Cheap to
/// poll; the provider checks it between streamed chunks and bails with
/// [`AiError::Cancelled`].
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// One language-model backend. `stream_turn` runs exactly **one** turn: it streams
/// incremental text / thinking over `tx` as tokens arrive and returns the fully
/// assembled assistant message plus why it stopped. The caller inspects
/// [`TurnOutcome::stop_reason`]; on [`StopReason::ToolUse`] it runs the requested
/// tools, appends their results, and calls `stream_turn` again.
#[async_trait]
pub trait AiProvider: Send + Sync {
    async fn stream_turn(
        &self,
        req: &TurnRequest,
        tx: &UnboundedSender<Delta>,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome>;
}
