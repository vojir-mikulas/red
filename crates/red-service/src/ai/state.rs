//! Per-conversation assistant state and the write-approval registry.
//!
//! Two things live here because they are the turn loop's memory rather than its
//! logic: [`AiState`], which outlives any single turn (running history, cancel
//! tokens, the cumulative tool budget, parked approval prompts), and
//! [`ReportSink`], the UI-agnostic announcer a tool uses to hand a finished file
//! or a query back to the app without knowing anything about the UI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use red_ai::{CancelToken, Message};
use tokio::sync::oneshot;

use crate::dispatch::{Events, emit};
use crate::protocol::{ConversationId, ReportTheme, RequestId, SandboxEntry, WritePreview};
use crate::{Event, SessionId};

/// A small, UI-agnostic announcer the `generate_report` tool uses to hand a
/// freshly-written report file to the UI, which surfaces it as a card the user can
/// open. The tool stays UI-free: it just announces a path; the caller turns it into
/// an `AiReportReady` event. Both backends construct one from the
/// `events`/`session`/`conversation_id` they hold; a `disabled()` sink (no channel)
/// drops announcements (tests).
#[derive(Clone)]
pub(crate) struct ReportSink {
    events: Option<Events>,
    session: Option<SessionId>,
    conversation_id: ConversationId,
    /// The active app theme, so `generate_report` can paint the report in RED's
    /// colors. Captured when the sink is built (per turn on the API-key path; at
    /// conversation start on the subscription path).
    theme: Option<ReportTheme>,
    /// Where finished report files are written (Settings → AI agent → Report folder),
    /// captured alongside `theme`. `None` falls back to the system temp dir.
    report_dir: Option<PathBuf>,
}
impl ReportSink {
    pub(crate) fn new(
        events: Events,
        session: Option<SessionId>,
        conversation_id: ConversationId,
        theme: Option<ReportTheme>,
        report_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            events: Some(events),
            session,
            conversation_id,
            theme,
            report_dir,
        }
    }

    /// A no-op sink that drops announcements. For tests and any path with no UI
    /// (the headless `red mcp` transport, which withholds `generate_report`).
    pub(crate) fn disabled() -> Self {
        Self {
            events: None,
            session: None,
            conversation_id: ConversationId::new(0),
            theme: None,
            report_dir: None,
        }
    }

    /// The theme to paint the report with, if the UI supplied one.
    pub(super) fn theme(&self) -> Option<&ReportTheme> {
        self.theme.as_ref()
    }

    /// The directory a finished report should be written to: the user's configured
    /// folder when set and usable (created on demand), else the system temp dir. A
    /// configured folder that can't be created falls back to temp rather than failing
    /// the report; the user still gets their report, just not where they asked.
    pub(super) fn output_dir(&self) -> PathBuf {
        if let Some(dir) = &self.report_dir {
            match std::fs::create_dir_all(dir) {
                Ok(()) => return dir.clone(),
                Err(e) => tracing::warn!(
                    "AI report folder {} is unusable ({e}); writing to the temp dir instead",
                    dir.display()
                ),
            }
        }
        std::env::temp_dir()
    }

    /// Announce a freshly-written report so the UI surfaces it as a card.
    pub(super) fn announce(&self, path: &Path, title: Option<&str>) {
        if let Some(events) = &self.events {
            emit(
                events,
                self.session,
                Event::AiReportReady {
                    conversation_id: self.conversation_id,
                    path: path.display().to_string(),
                    title: title.map(str::to_string),
                },
            );
        }
    }

    /// Ask the UI to open `sql` in a new query tab (the agent's open_query tool).
    pub(super) fn announce_open_query(&self, sql: &str) {
        if let Some(events) = &self.events {
            emit(
                events,
                self.session,
                Event::AiOpenQuery {
                    conversation_id: self.conversation_id,
                    sql: sql.to_string(),
                },
            );
        }
    }

    /// Hand the UI a drafted knowledge file (the agent's `save_knowledge` tool).
    /// The UI opens it for review; this never writes anything.
    pub(super) fn announce_knowledge_draft(&self, body: &str) {
        if let Some(events) = &self.events {
            emit(
                events,
                self.session,
                Event::AiKnowledgeDraft {
                    conversation_id: self.conversation_id,
                    body: body.to_string(),
                },
            );
        }
    }

    pub(super) fn announce_save_query(&self, name: &str, description: Option<&str>, sql: &str) {
        if let Some(events) = &self.events {
            emit(
                events,
                self.session,
                Event::AiSaveQuery {
                    conversation_id: self.conversation_id,
                    name: name.to_string(),
                    description: description.map(str::to_string),
                    sql: sql.to_string(),
                },
            );
        }
    }
}
/// An open sandbox transaction and everything that has run inside it.
///
/// Registered **per session, not per conversation**: two chats writing to one
/// database in two transactions is a deadlock generator, so the second one is
/// refused rather than given its own.
pub(crate) struct SandboxSlot {
    /// The live transaction. `Arc` rather than `Box` so it can be cloned out from
    /// under the state lock before any `await`; single-use is enforced by removing
    /// the slot from the registry, not by the type.
    pub(crate) sandbox: Arc<dyn red_driver::Sandbox>,
    /// Whose turn opened it, so a second conversation's write is refused and the
    /// resolve/expiry events route back to the right chat.
    pub(crate) conversation_id: ConversationId,
    /// What has run so far, in order: the review card's contents.
    pub(crate) log: Vec<SandboxEntry>,
}

impl SandboxSlot {
    /// Rows touched across every statement so far.
    pub(crate) fn total_rows(&self) -> u64 {
        self.log.iter().map(|e| e.rows).sum()
    }
}

/// Per-conversation state shared between the dispatch loop and the spawned turn
/// tasks: the running message history (so follow-up turns keep context), the
/// in-flight cancel tokens (so `AiCancel` can stop a specific turn), and the
/// cumulative tool-call tally (so the resource-guard budget spans the whole
/// conversation, not just one turn).
#[derive(Default)]
pub(crate) struct AiState {
    pub(super) histories: HashMap<ConversationId, Vec<Message>>,
    pub(super) cancels: HashMap<ConversationId, CancelToken>,
    pub(super) tool_calls: HashMap<ConversationId, usize>,
    /// Tokens each conversation has spent so far. Kept here rather than recomputed
    /// per turn because the question the footer answers -- "should I start a new
    /// chat?" -- is about the conversation, not the last exchange.
    pub(super) usage: HashMap<ConversationId, crate::protocol::AiUsage>,
    /// Write-tool approval prompts awaiting the user's Allow/Deny, keyed
    /// by request id. The turn task parks a decision sink here; `AiPermission` takes
    /// it back out and fires it, the API-key analogue of the ACP path's
    /// `AcpManager.pending`.
    pending_perms: HashMap<RequestId, oneshot::Sender<bool>>,
    /// Monotonic counter for the request ids handed out by [`Self::park_permission`].
    /// Handed-out ids are offset by [`AI_REQUEST_BASE`] so they never collide with
    /// the ACP manager's (which counts up from 0); `AiPermission` can then resolve
    /// both sides unconditionally.
    next_request: u64,
    /// Open sandbox transactions, at most one per session. Keyed by session
    /// because that is what a transaction actually belongs to.
    sandboxes: HashMap<SessionId, SandboxSlot>,
    /// Live windowed reads the agent can continue. Here rather than in its own
    /// global so it inherits [`forget`](Self::forget), which already runs on every
    /// path that ends a conversation.
    pub(crate) cursors: super::cursors::CursorRegistry,
}
/// Base offset for API-key permission request ids, keeping them disjoint from the
/// ACP manager's id space so a single `AiPermission` resolves exactly one prompt.
const AI_REQUEST_BASE: u64 = 1 << 48;
/// Cap on outstanding (un-answered) write-approval prompts on the API-key path;
/// past it, deny rather than grow the map. Mirrors the ACP manager's cap.
const MAX_PENDING_PERMS: usize = 32;
impl AiState {
    /// Record an in-flight turn's cancel token so `AiCancel` can reach it.
    pub(crate) fn register(&mut self, conversation_id: ConversationId, token: CancelToken) {
        self.cancels.insert(conversation_id, token);
    }

    /// Park a write-approval decision sink and return the request id to surface, or
    /// `None` (deny) when too many are already outstanding.
    pub(super) fn park_permission(&mut self, decide: oneshot::Sender<bool>) -> Option<RequestId> {
        if self.pending_perms.len() >= MAX_PENDING_PERMS {
            return None;
        }
        let id = RequestId::new(AI_REQUEST_BASE + self.next_request);
        self.next_request += 1;
        self.pending_perms.insert(id, decide);
        Some(id)
    }

    /// Answer a parked write-approval prompt (the panel's Allow/Deny). A stale id
    /// (already resolved, or owned by the ACP path) is a no-op. Also used to forget a
    /// prompt abandoned on cancel (`allow` is irrelevant then; the receiver is gone).
    pub(crate) fn resolve_permission(&mut self, request_id: RequestId, allow: bool) {
        if let Some(decide) = self.pending_perms.remove(&request_id) {
            let _ = decide.send(allow);
        }
    }

    /// Fold one turn's tokens into the conversation's running total and return
    /// the total. `used`/`window` describe how full the model's context is right
    /// now rather than what was spent, so they replace rather than accumulate.
    pub(super) fn charge_usage(
        &mut self,
        conversation_id: ConversationId,
        turn: crate::protocol::AiUsage,
    ) -> crate::protocol::AiUsage {
        let total = self.usage.entry(conversation_id).or_default();
        total.input_tokens += turn.input_tokens;
        total.output_tokens += turn.output_tokens;
        total.cache_read_input_tokens += turn.cache_read_input_tokens;
        total.context_used_tokens = turn.context_used_tokens;
        total.context_tokens = turn.context_tokens;
        *total
    }

    /// Flip the cancel token for an in-flight turn, if any (the panel's Stop).
    pub(crate) fn cancel(&mut self, conversation_id: ConversationId) {
        if let Some(tok) = self.cancels.get(&conversation_id) {
            tok.cancel();
        }
        // A stopped turn must not leave a connection pinned. A cursor legitimately
        // outlives the turn that opened it, but only when that turn *finished*:
        // the user pressing Stop is them saying they want this to end now.
        self.cursors.close_conversation(conversation_id);
    }

    /// Drop all per-conversation state (history, cancel token, cumulative tool tally)
    /// when the UI closes/deletes the conversation, so these maps stay bounded by
    /// what's open rather than every conversation ever touched this session. Cancels
    /// any in-flight turn first so its task winds down. (A turn still racing to its
    /// final history write can re-insert one entry; that's bounded, unlike the prior
    /// unconditional growth.)
    pub(crate) fn forget(&mut self, conversation_id: ConversationId) {
        if let Some(tok) = self.cancels.remove(&conversation_id) {
            tok.cancel();
        }
        self.histories.remove(&conversation_id);
        self.tool_calls.remove(&conversation_id);
        self.usage.remove(&conversation_id);
        // An open cursor holds a connection; a conversation that is gone can
        // never read from it again.
        self.cursors.close_conversation(conversation_id);
    }

    /// Register a freshly-opened sandbox for `session`.
    ///
    /// Returns `false` (and drops nothing) when that session already has one:
    /// **one sandbox per session**, so a second conversation cannot open a second
    /// transaction against the same database and deadlock with the first.
    pub(super) fn open_sandbox(
        &mut self,
        session: SessionId,
        conversation_id: ConversationId,
        sandbox: Arc<dyn red_driver::Sandbox>,
    ) -> bool {
        if self.sandboxes.contains_key(&session) {
            return false;
        }
        self.sandboxes.insert(
            session,
            SandboxSlot {
                sandbox,
                conversation_id,
                log: Vec::new(),
            },
        );
        true
    }

    /// The open sandbox for `session`, if any, and who owns it.
    pub(super) fn sandbox_for(
        &self,
        session: SessionId,
    ) -> Option<(Arc<dyn red_driver::Sandbox>, ConversationId)> {
        self.sandboxes
            .get(&session)
            .map(|slot| (slot.sandbox.clone(), slot.conversation_id))
    }

    /// Record a statement that ran inside `session`'s sandbox. A no-op if the
    /// sandbox is already gone (expired mid-turn), which is why the caller must
    /// not treat a recorded write as durable.
    pub(super) fn record_sandbox_write(&mut self, session: SessionId, sql: &str, rows: u64) {
        if let Some(slot) = self.sandboxes.get_mut(&session) {
            slot.log.push(SandboxEntry {
                sql: sql.to_string(),
                rows,
            });
        }
    }

    /// The current log and row total for `session`'s sandbox.
    pub(super) fn sandbox_log(&self, session: SessionId) -> Option<(Vec<SandboxEntry>, u64)> {
        self.sandboxes
            .get(&session)
            .map(|slot| (slot.log.clone(), slot.total_rows()))
    }

    /// Take `session`'s sandbox out of the registry. Removal *is* the single-use
    /// guarantee: whoever holds the returned slot is the only one who can resolve
    /// it, so a user's Commit racing the deadline's rollback cannot both land.
    pub(crate) fn take_sandbox(&mut self, session: SessionId) -> Option<SandboxSlot> {
        self.sandboxes.remove(&session)
    }

    /// Take whichever session's sandbox `conversation_id` owns. The UI answers by
    /// conversation; the registry is keyed by session.
    pub(crate) fn take_sandbox_for_conversation(
        &mut self,
        conversation_id: ConversationId,
    ) -> Option<(SessionId, SandboxSlot)> {
        let session = *self
            .sandboxes
            .iter()
            .find(|(_, slot)| slot.conversation_id == conversation_id)
            .map(|(session, _)| session)?;
        self.sandboxes.remove(&session).map(|slot| (session, slot))
    }

    /// Charge one tool call against the conversation's cumulative budget. Returns
    /// `false` once the budget (`max`, `0` = unlimited) is exhausted, so the loop
    /// can stop a runaway agent instead of letting it spin tools forever.
    pub(super) fn charge_tool_call(&mut self, conversation_id: ConversationId, max: usize) -> bool {
        let count = self.tool_calls.entry(conversation_id).or_default();
        if max != 0 && *count >= max {
            return false;
        }
        *count += 1;
        true
    }
}
pub(in crate::ai) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
/// Surface a write-approval prompt and block this turn until the user answers it,
/// the API-key path's analogue of the ACP permission flow. Parks a
/// decision sink in [`AiState`], emits an `AiPermissionRequest` carrying the exact
/// SQL, then awaits the answer while polling the turn's cancel token (a cancelled
/// turn, or too many outstanding prompts, denies). Returns whether to run the write.
pub(super) async fn await_write_approval(
    state: &Arc<Mutex<AiState>>,
    events: &Events,
    session: Option<SessionId>,
    conversation_id: ConversationId,
    sql: &str,
    preview: Option<WritePreview>,
    cancel: &CancelToken,
) -> bool {
    let (tx, mut rx) = oneshot::channel();
    let Some(request_id) = lock(state).park_permission(tx) else {
        return false; // too many outstanding prompts → deny
    };
    emit(
        events,
        session,
        Event::AiPermissionRequest {
            conversation_id,
            request_id,
            title: "run this write statement".into(),
            detail: Some(sql.to_string()),
            preview,
        },
    );
    let decision = loop {
        tokio::select! {
            answer = &mut rx => break answer.unwrap_or(false),
            _ = tokio::time::sleep(Duration::from_millis(150)) => {
                if cancel.is_cancelled() {
                    break false;
                }
            }
        }
    };
    // Drop the parked sink if we bailed on cancel; a normal answer already removed
    // it in `resolve_permission`, so this is a harmless no-op then.
    lock(state).resolve_permission(request_id, false);
    decision
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RequestId;

    #[test]
    fn write_approval_registry_parks_resolves_and_offsets_ids() {
        let mut st = AiState::default();
        let (tx, mut rx) = oneshot::channel();
        let id = st.park_permission(tx).expect("a fresh prompt parks");
        // Ids are offset so they never collide with the ACP manager's id space.
        assert!(id.get() >= AI_REQUEST_BASE);
        st.resolve_permission(id, true);
        assert_eq!(rx.try_recv(), Ok(true));
        // Resolving a stale/unknown id is a harmless no-op.
        st.resolve_permission(id, false);
        st.resolve_permission(RequestId::new(424242), false);
    }

    #[test]
    fn tool_call_budget_is_per_conversation_and_capped() {
        let mut state = AiState::default();
        // A cap of 2 admits two calls, then refuses the third on the same conversation.
        assert!(state.charge_tool_call(ConversationId::new(1), 2));
        assert!(state.charge_tool_call(ConversationId::new(1), 2));
        assert!(!state.charge_tool_call(ConversationId::new(1), 2));
        // A different conversation has its own fresh budget.
        assert!(state.charge_tool_call(ConversationId::new(2), 2));
        // `0` means unlimited.
        assert!(state.charge_tool_call(ConversationId::new(3), 0));
        assert!(state.charge_tool_call(ConversationId::new(3), 0));
    }
}
