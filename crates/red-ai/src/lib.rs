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
    AiError, ContentBlock, ContextManagement, Delta, DocumentSource, Message, Result, Role,
    StopReason, ToolDef, TurnOutcome, TurnRequest, Usage,
};

/// Beta opt-in for context editing (`clear_tool_uses_20250919`).
const CONTEXT_EDITING_BETA: &str = "context-management-2025-06-27";
/// Beta opt-in for server-side compaction (`compact_20260112`).
const COMPACTION_BETA: &str = "compact-2026-01-12";

/// Default deep-reasoning model.
pub const MODEL_OPUS: &str = "claude-opus-4-8";
/// Cheap / fast lane.
pub const MODEL_HAIKU: &str = "claude-haiku-4-5";

/// The context window `model` gives you, when we know it.
///
/// Spelled out rather than inferred from a prefix, because the families are not
/// uniform: `claude-opus-4-6` carries a million tokens and `claude-opus-4-5`
/// carries two hundred thousand, so a prefix rule would be confidently wrong.
/// An unrecognized model -- a newer release, a proxied endpoint, a local server
/// behind an Anthropic-compatible wire -- returns `None`, and the panel then
/// shows what it counted instead of a percentage it made up.
pub fn context_window(model: &str) -> Option<u64> {
    const MILLION: u64 = 1_000_000;
    const TWO_HUNDRED_K: u64 = 200_000;
    match model {
        "claude-fable-5" | "claude-mythos-5" | "claude-opus-5" | "claude-opus-4-8"
        | "claude-opus-4-7" | "claude-opus-4-6" | "claude-sonnet-5" | "claude-sonnet-4-6" => {
            Some(MILLION)
        }
        "claude-haiku-4-5" | "claude-opus-4-5" | "claude-sonnet-4-5" => Some(TWO_HUNDRED_K),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unknown model must size no window: a wrong percentage is worse than
    /// none, and the panel already renders "we don't know" honestly.
    #[test]
    fn an_unknown_model_sizes_no_window() {
        assert_eq!(context_window("llama-3-70b"), None);
        assert_eq!(context_window(""), None);
        assert_eq!(context_window("claude-opus-4-9"), None);
        // The families are not uniform, which is why this is a table and not a
        // prefix match.
        assert_eq!(context_window("claude-opus-4-6"), Some(1_000_000));
        assert_eq!(context_window("claude-opus-4-5"), Some(200_000));
        assert!(context_window(MODEL_OPUS).is_some());
        assert!(context_window(MODEL_HAIKU).is_some());
    }
}

/// A cloneable cancel flag the service flips when the user stops a turn.
///
/// Both pollable and *awaitable*. The flag alone was not enough: a provider parked
/// in `stream.next()` on a stalled connection never woke to read it, so Stop did
/// nothing and the chat stayed streaming forever. [`cancelled`](Self::cancelled)
/// gives a future to `select!` the read against, which makes cancellation abortive
/// rather than cooperative.
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        // `notify_waiters` would miss a task that has not parked yet;
        // `notify_one`+the flag check in `cancelled` covers both orderings.
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Resolves once [`cancel`](Self::cancel) has been called — immediately if it
    /// already has. Never resolves spuriously, so it is safe as a `select!` arm.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            // Register interest *before* re-checking the flag, so a `cancel` racing
            // this loop cannot slip between the check and the wait.
            let waiter = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            waiter.await;
        }
    }
}

/// What a backend can do beyond streaming a plain turn. Every field defaults to
/// the conservative answer, so a provider that says nothing gets the behaviour
/// that is safe everywhere and the caller falls back to doing the work itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// The backend will keep a long conversation inside its own context window
    /// when asked (see [`ContextManagement`]). When `false` the caller has to
    /// trim the history before it overflows.
    pub context_management: bool,
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
        req: &TurnRequest<'_>,
        tx: &UnboundedSender<Delta>,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome>;

    /// What this backend supports beyond a plain turn. Defaults to "nothing
    /// extra", which is what a new provider should get for free.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }
}
