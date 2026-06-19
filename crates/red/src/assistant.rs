//! The AI assistant panel — a right-docked, grounded chat sidebar (the AI-assistant
//! M1 slice). It streams a conversation with a model that knows about the connected
//! database and can read it through Red's existing safe seams: the panel never
//! speaks HTTP, it sends `Command::AiTurn` and drains `Event::AiDelta` like every
//! other backend interaction. The backend runs the model → tool → model loop and
//! the read-only tool catalog (see `red-service`'s `ai` module); this file is just
//! the view + the conversation state it streams into.
//!
//! Non-modal, like the inspector: the rest of the UI keeps working while a turn
//! streams. The model can read schema and run capped `SELECT`s on its own; in M1
//! it cannot mutate anything.

use flint::prelude::*;
use flint::{TextInput, TextInputEvent};
use gpui::{div, prelude::*, px, AnyElement, Context, Entity, ScrollHandle, SharedString, Window};

use crate::app::{ActiveConn, AppState, Phase};

/// Cap on schema objects folded into the grounding summary, so a database with
/// thousands of tables doesn't blow the context window. The model pulls full
/// detail on demand via `describe_table`, so a names-only overview is enough.
const SCHEMA_SUMMARY_CAP: usize = 200;

/// Who authored a chat bubble.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatRole {
    User,
    Assistant,
}

/// A one-tap context action shown in the panel (M-S4). Each maps to a canned
/// prompt; the live error / editor SQL it refers to is folded in by `ai_context`.
#[derive(Clone, Copy)]
pub(crate) enum QuickAction {
    /// Explain the last query error and how to fix it (shown when one exists).
    ExplainError,
    /// Review the editor's SQL for correctness and performance (shown when the
    /// editor holds a statement).
    OptimizeQuery,
}

impl QuickAction {
    /// The canned prompt sent for this action.
    fn prompt(self) -> &'static str {
        match self {
            QuickAction::ExplainError => "Explain the error from my last query and how to fix it.",
            QuickAction::OptimizeQuery => {
                "Review the SQL in my editor for correctness and performance, and \
                 suggest an improved version."
            }
        }
    }

    /// The chip's label.
    fn label(self) -> &'static str {
        match self {
            QuickAction::ExplainError => "Explain error",
            QuickAction::OptimizeQuery => "Optimize query",
        }
    }
}

/// One rendered turn in the panel. The assistant bubble accumulates streamed text
/// and (optionally) summarized thinking as deltas arrive.
pub(crate) struct ChatMessage {
    pub(crate) role: ChatRole,
    pub(crate) text: String,
    pub(crate) thinking: String,
}

/// A pending agent tool-permission prompt (M-S2, subscription path): the agent
/// wants to run a tool Red didn't auto-allow. The panel shows Allow/Deny and the
/// answer routes back as `Command::AiPermission`, keyed by `request_id`.
pub(crate) struct PendingPermission {
    pub(crate) request_id: u64,
    pub(crate) title: SharedString,
    pub(crate) detail: Option<SharedString>,
}

/// All the assistant panel's state. Present iff the panel is open.
pub(crate) struct AssistantState {
    /// The prompt box. Submitting it (Enter) sends a turn.
    pub(crate) input: Entity<TextInput>,
    /// The API-key box, shown in the setup view when no key is configured.
    pub(crate) key_input: Entity<TextInput>,
    /// Submit listeners (prompt + key); held here so closing the panel drops them.
    #[allow(dead_code)]
    sub: gpui::Subscription,
    #[allow(dead_code)]
    key_sub: gpui::Subscription,
    /// Scroll position of the transcript, kept across frames.
    pub(crate) scroll: ScrollHandle,
    /// The rendered conversation, oldest first.
    pub(crate) messages: Vec<ChatMessage>,
    /// Stable id tying this panel's turns together so the backend keeps history.
    pub(crate) conversation_id: u64,
    /// True while a turn is streaming (drives the Stop button + busy state).
    pub(crate) streaming: bool,
    /// Transient tool-activity line ("Running run_select…"), shown while streaming.
    pub(crate) status: Option<SharedString>,
    /// The last turn's error, shown inline (not as a global toast).
    pub(crate) error: Option<SharedString>,
    /// A tool-permission prompt awaiting the user's Allow/Deny (M-S2). At most one
    /// is shown at a time; the agent blocks on the answer.
    pub(crate) pending_permission: Option<PendingPermission>,
    /// The most recent finished turn's token/cost accounting (M-S4), shown as a
    /// compact footer. `None` until the first turn completes.
    pub(crate) last_usage: Option<red_service::AiUsage>,
    /// Which backend this chat runs on (`"subscription"`, `"anthropic"`, …), as of
    /// when it opened — persisted as the conversation's provider binding (M-S5).
    pub(crate) provider: String,
    /// The chat's title, derived from its first user message; the saved file's
    /// display name. `None` until the first turn is sent.
    pub(crate) title: Option<String>,
    /// The backing file's stem once this chat has been saved (M-S5), so later turns
    /// overwrite the same file. `None` for a never-saved chat.
    pub(crate) file_stem: Option<String>,
    /// Unix seconds this chat was first saved — kept stable across re-saves.
    pub(crate) created_unix: Option<u64>,
    /// A reopened conversation's prior transcript, folded into the *next* turn's
    /// context so the model resumes where it left off (M-S5). Taken (cleared) when
    /// that turn is sent; the backend session is fresh, so this seeds it once.
    pub(crate) pending_seed: Option<String>,
}

impl AssistantState {
    /// Ensure the trailing bubble is an assistant bubble (deltas append to it).
    fn assistant_bubble(&mut self) -> &mut ChatMessage {
        if !matches!(self.messages.last(), Some(m) if m.role == ChatRole::Assistant) {
            self.messages.push(ChatMessage {
                role: ChatRole::Assistant,
                text: String::new(),
                thinking: String::new(),
            });
        }
        self.messages.last_mut().expect("just ensured")
    }
}

impl AppState {
    /// Open or close the assistant panel (⌘L). Only meaningful while connected.
    pub(crate) fn toggle_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Connected(_)) {
            return;
        }
        if self.assistant.is_some() {
            self.assistant = None;
            // Closing drops the panel's focused input; hand focus back to the root
            // so the ⌘L action keeps routing (otherwise focus is lost and the panel
            // can't be reopened — the action's owner is no longer in the focus path).
            window.focus(&self.root_focus, cx);
        } else {
            let conversation_id = self.next_conversation_id;
            self.next_conversation_id += 1;
            let input =
                cx.new(|cx| TextInput::new(cx).with_placeholder("Ask about this database…"));
            let sub = cx.subscribe(&input, |this, _, e: &TextInputEvent, cx| {
                if matches!(e, TextInputEvent::Submit) {
                    this.submit_assistant(cx);
                }
            });
            let key_input = cx.new(|cx| TextInput::new(cx).obscured().with_placeholder("sk-ant-…"));
            let key_sub = cx.subscribe(&key_input, |this, _, e: &TextInputEvent, cx| {
                if matches!(e, TextInputEvent::Submit) {
                    this.save_ai_key(cx);
                }
            });
            let provider = if self.settings.ai.provider.is_empty() {
                "anthropic".to_string()
            } else {
                self.settings.ai.provider.clone()
            };
            self.assistant = Some(AssistantState {
                input,
                key_input,
                sub,
                key_sub,
                scroll: ScrollHandle::new(),
                messages: Vec::new(),
                conversation_id,
                streaming: false,
                status: None,
                error: None,
                pending_permission: None,
                last_usage: None,
                provider,
                title: None,
                file_stem: None,
                created_unix: None,
                pending_seed: None,
            });
            self.focus_assistant = true;
        }
        cx.notify();
    }

    /// Close the assistant panel (no-op when shut).
    pub(crate) fn close_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.assistant.is_some() {
            self.assistant = None;
            // Return focus to the root so keyboard actions keep routing (see
            // `toggle_assistant`).
            window.focus(&self.root_focus, cx);
            cx.notify();
        }
    }

    /// Send the prompt box's contents as one turn.
    pub(crate) fn submit_assistant(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        if state.streaming {
            return;
        }
        let message = state.input.read(cx).content().trim().to_string();
        if message.is_empty() {
            return;
        }
        // Clear the box; `send_turn` records the exchange and dispatches.
        state.input.update(cx, |i, cx| i.set_content("", cx));
        self.send_turn(message, cx);
    }

    /// A one-tap context action (M-S4): "Explain error" / "Optimize query". Each is
    /// just a canned prompt — `ai_context` already folds in the live error / editor
    /// SQL, so the turn is grounded without the user retyping it. Shared by both
    /// providers (it rides the same `AiTurn` path).
    pub(crate) fn assistant_quick_action(&mut self, kind: QuickAction, cx: &mut Context<Self>) {
        self.send_turn(kind.prompt().to_string(), cx);
    }

    /// Record a user turn and dispatch it to the backend. The caller has already
    /// resolved the message text (typed, or a quick-action prompt).
    fn send_turn(&mut self, message: String, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        if state.streaming || message.trim().is_empty() {
            return;
        }
        let conversation_id = state.conversation_id;
        let (session, mut context) = {
            let Phase::Connected(active) = &self.phase else {
                return;
            };
            (active.session, self.ai_context(active, cx))
        };

        if let Some(state) = self.assistant.as_mut() {
            // A reopened chat seeds its prior transcript into this one turn so the
            // model resumes coherently despite a fresh backend session (M-S5).
            context.prior_transcript = state.pending_seed.take();
            // Title the chat from its first user message (used as the saved name).
            if state.title.is_none() {
                state.title = Some(derive_title(&message));
            }
            state.messages.push(ChatMessage {
                role: ChatRole::User,
                text: message.clone(),
                thinking: String::new(),
            });
            state.error = None;
            state.status = None;
            state.streaming = true;
        }

        self.service.send_to(
            session,
            red_service::Command::AiTurn {
                conversation_id,
                message,
                context,
            },
        );
        cx.notify();
    }

    /// Re-authenticate / switch the subscription account (M-S4). The agent owns
    /// `/login`, so Red asks the backend to restart the conversation's agent; the
    /// next turn re-runs the handshake and the agent pops its own browser login
    /// when it isn't signed in. A no-op on the API-key path.
    pub(crate) fn reauthenticate_assistant(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        let conversation_id = state.conversation_id;
        if let Phase::Connected(active) = &self.phase {
            self.service.send_to(
                active.session,
                red_service::Command::AiReauthenticate { conversation_id },
            );
        }
        if let Some(state) = self.assistant.as_mut() {
            state.error = None;
            state.status = Some("Restarting the assistant — sign in if the browser opens.".into());
        }
        cx.notify();
    }

    // --- conversation history (M-S5) ---------------------------------------

    /// Persist the open chat to its flat file (one JSON per conversation), titled
    /// from its first user message. Called after each finished turn. A chat with no
    /// real assistant reply yet (only a pending/aborted user turn) isn't saved —
    /// there's nothing worth keeping. Best-effort: a write failure is logged, never
    /// surfaced mid-turn.
    fn persist_conversation(&mut self) {
        let Some(state) = self.assistant.as_mut() else {
            return;
        };
        // Need at least one assistant turn with content to be worth saving.
        let has_answer = state
            .messages
            .iter()
            .any(|m| m.role == ChatRole::Assistant && !m.text.trim().is_empty());
        if !has_answer {
            return;
        }
        let title = state
            .title
            .clone()
            .unwrap_or_else(|| "Untitled chat".to_string());
        // Choose a stable file stem the first time, then reuse it so later turns
        // overwrite in place rather than spawning a new file per turn.
        let stem = state
            .file_stem
            .get_or_insert_with(|| crate::conversations::unique_stem(&title))
            .clone();
        let now = crate::conversations::now_unix();
        let created = *state.created_unix.get_or_insert(now);
        let conv = crate::conversations::Conversation {
            title,
            provider: state.provider.clone(),
            created_unix: created,
            updated_unix: now,
            messages: state
                .messages
                .iter()
                .map(|m| crate::conversations::StoredMessage {
                    role: match m.role {
                        ChatRole::User => "user".into(),
                        ChatRole::Assistant => "assistant".into(),
                    },
                    text: m.text.clone(),
                    thinking: m.thinking.clone(),
                })
                .collect(),
            path: Default::default(),
            stem: stem.clone(),
        };
        if let Err(e) = crate::conversations::save(&stem, &conv) {
            tracing::warn!("failed to persist conversation: {e}");
        }
    }

    /// Start a fresh chat (the panel's New-chat affordance): persist the current one
    /// first so nothing is lost, then clear the transcript and mint a new
    /// conversation id so the backend keeps the threads separate.
    pub(crate) fn new_chat(&mut self, cx: &mut Context<Self>) {
        self.persist_conversation();
        let id = self.next_conversation_id;
        self.next_conversation_id += 1;
        if let Some(state) = self.assistant.as_mut() {
            state.messages.clear();
            state.conversation_id = id;
            state.title = None;
            state.file_stem = None;
            state.created_unix = None;
            state.pending_seed = None;
            state.error = None;
            state.status = None;
            state.last_usage = None;
            state.pending_permission = None;
            state.streaming = false;
            state.input.update(cx, |i, cx| i.set_content("", cx));
        }
        self.focus_assistant = true;
        cx.notify();
    }

    /// Reopen a saved conversation into the panel (history-picker activation). The
    /// visible transcript comes back as-is; a fresh conversation id + the prior
    /// transcript folded into the next turn (`pending_seed`) means the backend
    /// starts a clean session that's still grounded in what was said before.
    pub(crate) fn restore_conversation(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(conv) = self.loaded_conversations.get(index).cloned() else {
            return;
        };
        // Persist whatever's open before replacing it.
        self.persist_conversation();
        let id = self.next_conversation_id;
        self.next_conversation_id += 1;
        let seed = render_transcript(&conv.messages);
        if let Some(state) = self.assistant.as_mut() {
            state.messages = conv
                .messages
                .iter()
                .map(|m| ChatMessage {
                    role: if m.role == "assistant" {
                        ChatRole::Assistant
                    } else {
                        ChatRole::User
                    },
                    text: m.text.clone(),
                    thinking: m.thinking.clone(),
                })
                .collect();
            state.conversation_id = id;
            state.title = Some(conv.title.clone());
            state.provider = conv.provider.clone();
            state.file_stem = Some(conv.stem.clone());
            state.created_unix = Some(conv.created_unix);
            state.pending_seed = seed;
            state.error = None;
            state.status = None;
            state.last_usage = None;
            state.pending_permission = None;
            state.streaming = false;
        }
        self.focus_assistant = true;
        cx.notify();
    }

    /// Delete the open chat's saved file (the panel's Delete action) and start a
    /// fresh one. A never-saved chat just resets. The file is also user-deletable by
    /// hand — the next history open simply won't list it.
    pub(crate) fn delete_current_conversation(&mut self, cx: &mut Context<Self>) {
        let stem = self.assistant.as_ref().and_then(|s| s.file_stem.clone());
        if let Some(stem) = stem {
            if let Some(dir) = crate::conversations::conversations_dir() {
                let path = dir.join(format!("{stem}.json"));
                if let Err(e) = crate::conversations::delete(&path) {
                    tracing::warn!("failed to delete conversation: {e}");
                }
            }
        }
        // Reset without re-saving the just-deleted chat.
        let id = self.next_conversation_id;
        self.next_conversation_id += 1;
        if let Some(state) = self.assistant.as_mut() {
            state.messages.clear();
            state.conversation_id = id;
            state.title = None;
            state.file_stem = None;
            state.created_unix = None;
            state.pending_seed = None;
            state.error = None;
            state.status = None;
            state.last_usage = None;
            state.pending_permission = None;
            state.streaming = false;
            state.input.update(cx, |i, cx| i.set_content("", cx));
        }
        cx.notify();
    }

    /// Reveal the conversations directory in the OS file manager (the "Open
    /// conversation storage" affordance). Files there are plain JSON — readable,
    /// hand-editable, deletable. Mirrors the saved-queries / settings reveal.
    pub(crate) fn reveal_conversation_storage(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = crate::conversations::conversations_dir() else {
            self.notify(
                flint::ToastVariant::Error,
                "No config directory available on this platform.",
                cx,
            );
            return;
        };
        // Create it so the reveal lands somewhere even before the first save.
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("failed to create conversations directory: {e}");
        }
        self.reveal_path(&dir, cx);
    }

    /// Whether the open chat has been saved (gates the Delete affordance).
    pub(crate) fn assistant_has_saved_chat(&self) -> bool {
        self.assistant
            .as_ref()
            .is_some_and(|s| s.file_stem.is_some())
    }

    /// Save the API key from the setup view to the OS keyring and (re)configure
    /// the backend provider.
    pub(crate) fn save_ai_key(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        let key = state.key_input.read(cx).content().trim().to_string();
        if key.is_empty() {
            return;
        }
        let provider = if self.settings.ai.provider.is_empty() {
            "anthropic".to_string()
        } else {
            self.settings.ai.provider.clone()
        };
        if let Err(e) = crate::secrets::set_ai_key(&provider, &key) {
            tracing::warn!("failed to store AI key in keychain: {e}");
        }
        if let Some(state) = self.assistant.as_ref() {
            state.key_input.update(cx, |i, cx| i.set_content("", cx));
        }
        self.ai_configured = true;
        self.service
            .send_global(red_service::Command::ConfigureAi(crate::app::ai_config(
                &self.settings,
            )));
        self.focus_assistant = true;
        cx.notify();
    }

    /// Stop an in-flight turn (the Stop button).
    pub(crate) fn cancel_assistant(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        if !state.streaming {
            return;
        }
        let conversation_id = state.conversation_id;
        if let Phase::Connected(active) = &self.phase {
            self.service.send_to(
                active.session,
                red_service::Command::AiCancel { conversation_id },
            );
        }
        cx.notify();
    }

    /// Insert a model-suggested SQL snippet into the active editor tab.
    pub(crate) fn ai_insert_sql(&mut self, sql: String, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &self.phase {
            if let Some(tab) = active.active() {
                tab.editor.update(cx, |e, cx| e.set_content(sql, cx));
            }
        }
        cx.notify();
    }

    // --- event sinks (driven from `on_event`) --------------------------------

    pub(crate) fn on_ai_delta(
        &mut self,
        conversation_id: u64,
        delta: red_service::AiDelta,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.assistant.as_mut() else {
            return;
        };
        if state.conversation_id != conversation_id {
            return;
        }
        match delta {
            red_service::AiDelta::Text(t) => state.assistant_bubble().text.push_str(&t),
            red_service::AiDelta::Thinking(t) => state.assistant_bubble().thinking.push_str(&t),
            red_service::AiDelta::ToolStarted { name } => {
                state.status = Some(format!("Running {name}…").into());
            }
            red_service::AiDelta::ToolFinished { name, ok } => {
                state.status = Some(
                    if ok {
                        format!("{name} ✓")
                    } else {
                        format!("{name} failed")
                    }
                    .into(),
                );
            }
        }
        cx.notify();
    }

    pub(crate) fn on_ai_finished(
        &mut self,
        conversation_id: u64,
        usage: red_service::AiUsage,
        cx: &mut Context<Self>,
    ) {
        let mut finished = false;
        if let Some(state) = self.assistant.as_mut() {
            if state.conversation_id == conversation_id {
                state.streaming = false;
                state.status = None;
                state.pending_permission = None;
                // Keep a non-empty reading; a turn that reports nothing (some
                // refusals / cancels) leaves the prior footer in place.
                if usage != red_service::AiUsage::default() {
                    state.last_usage = Some(usage);
                }
                finished = true;
            }
        }
        if finished {
            // Persist the now-complete exchange so it survives a restart (M-S5).
            self.persist_conversation();
            cx.notify();
        }
    }

    pub(crate) fn on_ai_error(
        &mut self,
        conversation_id: u64,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.assistant.as_mut() {
            if state.conversation_id == conversation_id {
                state.streaming = false;
                state.status = None;
                state.error = Some(message.into());
                // A prompt can't outlive its turn — drop any unanswered one.
                state.pending_permission = None;
                cx.notify();
            }
        }
    }

    /// The agent asked to run a tool Red didn't auto-allow (M-S2): show the prompt.
    pub(crate) fn on_ai_permission_request(
        &mut self,
        conversation_id: u64,
        request_id: u64,
        title: String,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.assistant.as_mut() {
            if state.conversation_id == conversation_id {
                state.pending_permission = Some(PendingPermission {
                    request_id,
                    title: title.into(),
                    detail: detail.map(Into::into),
                });
                cx.notify();
            }
        }
    }

    /// Answer the pending tool-permission prompt (its Allow/Deny buttons). The
    /// agent is blocked on this; denying is the safe default if it's dismissed.
    pub(crate) fn answer_permission(&mut self, allow: bool, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_mut() else {
            return;
        };
        let Some(pending) = state.pending_permission.take() else {
            return;
        };
        let conversation_id = state.conversation_id;
        if let Phase::Connected(active) = &self.phase {
            self.service.send_to(
                active.session,
                red_service::Command::AiPermission {
                    conversation_id,
                    request_id: pending.request_id,
                    allow,
                },
            );
        }
        cx.notify();
    }

    /// Assemble the on-screen grounding for a turn (the UI knows the screen; the
    /// service knows the model).
    fn ai_context(&self, active: &ActiveConn, cx: &Context<Self>) -> red_service::AiContext {
        let editor_sql = active
            .active()
            .map(|t| t.editor.read(cx).content().to_string())
            .filter(|s| !s.trim().is_empty());
        // The active result's last failure, so "Explain error" (and any turn after
        // a failed query) is grounded in what the user just saw.
        let last_error = active
            .active()
            .and_then(|t| t.result.as_ref())
            .and_then(|r| r.error())
            .map(str::to_string);
        red_service::AiContext {
            schema_summary: summarize_schema(&active.schema.schemas),
            editor_sql,
            last_error,
            selection: None,
            // Set per-turn by `send_turn` only on the first turn after a restore.
            prior_transcript: None,
            connection: format!(
                "{} database \"{}\"",
                active.config.kind, active.config.database
            ),
            read_only: active.config.read_only,
        }
    }

    /// The assistant panel body, docked right of the workspace by the shell.
    pub(crate) fn render_assistant(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(state) = self.assistant.as_ref() else {
            return div().into_any_element();
        };
        // Subscription mode (Claude Code over ACP) needs no API key and bills the
        // user's Pro/Max plan; the panel reflects which backend is live.
        let is_subscription = self
            .settings
            .ai
            .provider
            .eq_ignore_ascii_case("subscription");

        // Header: title + (subscription) switch-account + close.
        let close = div()
            .id("assistant-close")
            .flex()
            .items_center()
            .justify_center()
            .size(px(20.))
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_elevated))
            .child(crate::icons::icon("x", theme.scale(13.), theme.text_muted))
            .on_click(cx.listener(|this, _, window, cx| this.close_assistant(window, cx)));

        // The agent owns `/login`, so "Switch account" restarts it (M-S4); only
        // meaningful on the subscription path.
        let reauth = is_subscription.then(|| {
            div()
                .id("assistant-reauth")
                .flex()
                .items_center()
                .justify_center()
                .size(px(20.))
                .rounded(px(4.))
                .cursor_pointer()
                .tooltip(flint::Tooltip::text("Sign in / switch account"))
                .hover(|s| s.bg(theme.bg_elevated))
                .child(crate::icons::icon(
                    "key-round",
                    theme.scale(13.),
                    theme.text_muted,
                ))
                .on_click(cx.listener(|this, _, _, cx| this.reauthenticate_assistant(cx)))
        });
        // History affordances (M-S5), shown only in the chat view (not setup):
        // reopen a past chat, start a fresh one, and delete the current saved chat.
        let icon_btn = |id: &'static str, glyph: &'static str, tip: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .size(px(20.))
                .rounded(px(4.))
                .cursor_pointer()
                .tooltip(flint::Tooltip::text(tip))
                .hover(|s| s.bg(theme.bg_elevated))
                .child(crate::icons::icon(
                    glyph,
                    theme.scale(13.),
                    theme.text_muted,
                ))
        };
        let history = self.ai_configured.then(|| {
            icon_btn("assistant-history", "clock", "Conversation history")
                .on_click(cx.listener(|this, _, _, cx| this.open_conversation_picker(cx)))
        });
        let new_chat = self.ai_configured.then(|| {
            icon_btn("assistant-new-chat", "plus", "New chat")
                .on_click(cx.listener(|this, _, _, cx| this.new_chat(cx)))
        });
        let delete = (self.ai_configured && self.assistant_has_saved_chat()).then(|| {
            icon_btn("assistant-delete", "trash", "Delete this conversation")
                .on_click(cx.listener(|this, _, _, cx| this.delete_current_conversation(cx)))
        });

        let header_actions = div()
            .flex()
            .items_center()
            .gap_1()
            .when_some(reauth, |row, r| row.child(r))
            .when_some(history, |row, h| row.child(h))
            .when_some(new_chat, |row, n| row.child(n))
            .when_some(delete, |row, d| row.child(d))
            .child(close);

        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .h(px(34.))
            .px_3()
            .bg(theme.bg_panel)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(crate::icons::icon("sparkles", theme.scale(14.), theme.red))
                    .child("Assistant")
                    .when(is_subscription, |row| {
                        row.child(
                            div()
                                .text_size(theme.scale(10.5))
                                .text_color(theme.text_muted)
                                .child("· Subscription"),
                        )
                    }),
            )
            .child(header_actions);

        // Setup view: no key configured yet. Offer an inline key entry (stored in
        // the OS keyring, never in settings.toml).
        if !self.ai_configured {
            let save = div()
                .id("assistant-save-key")
                .px_3()
                .h(px(28.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .bg(theme.red)
                .text_size(theme.scale(12.))
                .text_color(theme.bg_app)
                .cursor_pointer()
                .child("Save key")
                .on_click(cx.listener(|this, _, _, cx| this.save_ai_key(cx)));
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(theme.bg_panel_2)
                .border_l_1()
                .border_color(theme.border)
                .child(header)
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_3()
                        .child(
                            div()
                                .text_size(theme.scale(12.5))
                                .text_color(theme.text)
                                .child("Add an Anthropic API key to use the assistant."),
                        )
                        .child(
                            div()
                                .text_size(theme.scale(11.))
                                .text_color(theme.text_muted)
                                .child(
                                    "The key is stored in your OS keychain, never in settings. You \
                                     can also set the ANTHROPIC_API_KEY environment variable.",
                                ),
                        )
                        .child(state.key_input.clone())
                        .child(div().flex().child(save)),
                )
                .into_any_element();
        }

        // Transcript.
        let mut body = div()
            .id("assistant-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&state.scroll)
            .flex()
            .flex_col()
            .gap_3()
            .p_3();

        if state.messages.is_empty() {
            let hint = if is_subscription {
                "Ask a question about the connected database. Chatting via your Claude \
                 subscription (Claude Code) — the first message starts the agent, which reads \
                 the schema and runs capped, read-only SELECTs through Red's tools."
            } else {
                "Ask a question about the connected database. The assistant can read the \
                 schema and run capped, read-only SELECTs to answer."
            };
            body = body.child(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text_muted)
                    .child(hint),
            );
        }
        for msg in &state.messages {
            body = body.child(self.render_bubble(msg, &theme, cx));
        }
        if let Some(status) = &state.status {
            body = body.child(
                div()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_muted)
                    .child(status.clone()),
            );
        }
        if let Some(err) = &state.error {
            body = body.child(
                div()
                    .text_size(theme.scale(11.5))
                    .text_color(theme.red)
                    .child(err.clone()),
            );
        }

        // Composer: input + send/stop.
        let action: AnyElement = if state.streaming {
            div()
                .id("assistant-stop")
                .px_3()
                .h(px(28.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .border_1()
                .border_color(theme.border)
                .text_size(theme.scale(12.))
                .text_color(theme.text_muted)
                .cursor_pointer()
                .hover(|s| s.border_color(theme.red).text_color(theme.red))
                .child("Stop")
                .on_click(cx.listener(|this, _, _, cx| this.cancel_assistant(cx)))
                .into_any_element()
        } else {
            div()
                .id("assistant-send")
                .px_3()
                .h(px(28.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .bg(theme.red)
                .text_size(theme.scale(12.))
                .text_color(theme.bg_app)
                .cursor_pointer()
                .hover(|s| s.opacity(0.9))
                .child("Send")
                .on_click(cx.listener(|this, _, _, cx| this.submit_assistant(cx)))
                .into_any_element()
        };

        let composer = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(theme.border)
            .child(div().flex_1().child(state.input.clone()))
            .child(action);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_panel_2)
            .border_l_1()
            .border_color(theme.border)
            .child(header)
            .child(body)
            .when_some(state.pending_permission.as_ref(), |col, pending| {
                col.child(self.render_permission(pending, &theme, cx))
            })
            .when_some(
                self.render_quick_actions(state, &theme, cx),
                |col, chips| col.child(chips),
            )
            .child(composer)
            .when_some(state.last_usage, |col, usage| {
                col.child(render_usage(&usage, &theme))
            })
            .into_any_element()
    }

    /// The context-action chips (M-S4): "Explain error" when the active result
    /// failed, "Optimize query" when the editor holds SQL. Shared by both providers
    /// (they ride the same `AiTurn`). Hidden while a turn streams, or when neither
    /// applies. Docked above the composer so they're reachable regardless of scroll.
    fn render_quick_actions(
        &self,
        state: &AssistantState,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if state.streaming {
            return None;
        }
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let tab = active.active();
        let has_error = tab
            .and_then(|t| t.result.as_ref())
            .is_some_and(|r| r.error().is_some());
        let has_sql = tab.is_some_and(|t| !t.editor.read(cx).content().trim().is_empty());

        let mut actions = Vec::new();
        if has_error {
            actions.push(QuickAction::ExplainError);
        }
        if has_sql {
            actions.push(QuickAction::OptimizeQuery);
        }
        if actions.is_empty() {
            return None;
        }

        let mut row = div()
            .flex_shrink_0()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1p5()
            .px_2()
            .pt_2();
        for action in actions {
            row = row.child(
                div()
                    .id(SharedString::from(format!("ai-quick-{}", action.label())))
                    .px_2()
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded(px(5.))
                    .border_1()
                    .border_color(theme.border)
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_muted)
                    .cursor_pointer()
                    .hover(|s| s.border_color(theme.red).text_color(theme.red))
                    .child(crate::icons::icon(
                        "sparkles",
                        theme.scale(11.),
                        theme.text_muted,
                    ))
                    .child(action.label())
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.assistant_quick_action(action, cx)),
                    ),
            );
        }
        Some(row.into_any_element())
    }

    /// The tool-permission prompt (M-S2): what the agent wants to do, plus
    /// Allow/Deny. Docked above the composer so it's visible regardless of scroll;
    /// the agent is blocked until the user answers.
    fn render_permission(
        &self,
        pending: &PendingPermission,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let button = |id: &'static str, label: &'static str, accent: bool| {
            let base = div()
                .id(id)
                .px_3()
                .h(px(26.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .text_size(theme.scale(12.))
                .cursor_pointer()
                .child(label);
            if accent {
                base.bg(theme.red)
                    .text_color(theme.bg_app)
                    .hover(|s| s.opacity(0.9))
            } else {
                base.border_1()
                    .border_color(theme.border)
                    .text_color(theme.text_muted)
                    .hover(|s| s.border_color(theme.text).text_color(theme.text))
            }
        };

        let mut card = div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(theme.bg_panel)
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(crate::icons::icon("lock", theme.scale(13.), theme.red))
                    .child(format!("Allow the assistant to run {}?", pending.title)),
            );
        if let Some(detail) = &pending.detail {
            card = card.child(
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .font_family(theme.mono_family.clone())
                    .child(detail.clone()),
            );
        }
        card.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    button("ai-permission-deny", "Deny", false)
                        .on_click(cx.listener(|this, _, _, cx| this.answer_permission(false, cx))),
                )
                .child(
                    button("ai-permission-allow", "Allow", true)
                        .on_click(cx.listener(|this, _, _, cx| this.answer_permission(true, cx))),
                ),
        )
        .into_any_element()
    }

    /// One chat bubble.
    fn render_bubble(
        &self,
        msg: &ChatMessage,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (label, label_color) = match msg.role {
            ChatRole::User => ("You", theme.text_muted),
            ChatRole::Assistant => ("Assistant", theme.red),
        };

        // Label row: the author, plus a copy-to-clipboard affordance for the
        // message text (assistant turns can be long; this beats hand-selecting).
        let mut label_row = div().flex().items_center().justify_between().child(
            div()
                .text_size(theme.scale(10.5))
                .text_color(label_color)
                .child(label),
        );
        if !msg.text.trim().is_empty() {
            let to_copy = msg.text.clone();
            label_row = label_row.child(
                div()
                    .id(SharedString::from(format!("ai-copy-{}", bubble_key(msg))))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(18.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .tooltip(flint::Tooltip::text("Copy message"))
                    .hover(|s| s.bg(theme.bg_elevated))
                    .child(crate::icons::icon(
                        "copy",
                        theme.scale(11.),
                        theme.text_muted,
                    ))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(to_copy.clone()));
                    })),
            );
        }
        let mut bubble = div().flex().flex_col().gap_1().child(label_row);

        // Summarized thinking (assistant only), dim and above the answer.
        if !msg.thinking.trim().is_empty() {
            let mut think = div()
                .flex()
                .flex_col()
                .text_size(theme.scale(11.))
                .text_color(theme.text_muted);
            for line in msg.thinking.lines() {
                think = think.child(div().child(line.to_string()));
            }
            bubble = bubble.child(think);
        }

        // Answer text. Assistant turns are Markdown — render them; user turns are
        // plain. A still-empty assistant bubble shows a streaming ellipsis.
        let answer = if msg.text.is_empty() && msg.role == ChatRole::Assistant {
            div()
                .text_size(theme.scale(12.5))
                .text_color(theme.text_muted)
                .child("…")
                .into_any_element()
        } else if msg.role == ChatRole::Assistant {
            crate::markdown::render(&msg.text, theme)
        } else {
            div()
                .text_size(theme.scale(12.5))
                .text_color(theme.text)
                .child(msg.text.clone())
                .into_any_element()
        };
        bubble = bubble.child(answer);

        // "Insert into editor" for the first fenced SQL block in an assistant turn.
        if msg.role == ChatRole::Assistant {
            if let Some(sql) = extract_sql(&msg.text) {
                let id = SharedString::from(format!("ai-insert-{}", bubble_key(msg)));
                bubble = bubble.child(
                    div()
                        .id(id)
                        .mt_1()
                        .self_start()
                        .px_2()
                        .h(px(22.))
                        .flex()
                        .items_center()
                        .gap_1()
                        .rounded(px(5.))
                        .border_1()
                        .border_color(theme.border)
                        .text_size(theme.scale(11.))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|s| s.border_color(theme.red).text_color(theme.red))
                        .child(crate::icons::icon(
                            "corner-down-left",
                            theme.scale(11.),
                            theme.text_muted,
                        ))
                        .child("Insert into editor")
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.ai_insert_sql(sql.clone(), cx)),
                        ),
                );
            }
        }

        bubble.into_any_element()
    }
}

/// The token/cost footer (M-S4): a compact, dim strip under the composer showing
/// the latest turn's accounting. The subscription path reports the tokens in
/// context plus a running session cost; the API-key path reports per-turn tokens
/// and no cost. Only non-zero/present fields render.
fn render_usage(usage: &red_service::AiUsage, theme: &flint::Theme) -> AnyElement {
    let mut parts: Vec<String> = Vec::new();
    if usage.input_tokens > 0 {
        parts.push(format!("{} in", compact_count(usage.input_tokens)));
    }
    if usage.output_tokens > 0 {
        parts.push(format!("{} out", compact_count(usage.output_tokens)));
    }
    if usage.cache_read_input_tokens > 0 {
        parts.push(format!(
            "{} cached",
            compact_count(usage.cache_read_input_tokens)
        ));
    }
    if let Some(cost) = usage.cost_usd {
        // Sub-cent sessions still read as a real number rather than "$0.00".
        parts.push(format!("${cost:.4}"));
    }
    let label = if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" · ")
    };
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_end()
        .px_3()
        .pb_1p5()
        .text_size(theme.scale(10.))
        .text_color(theme.text_muted)
        .child(label)
        .into_any_element()
}

/// Render a token count compactly: `1234 → 1.2k`, `2_000_000 → 2.0M`.
fn compact_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Cap on a derived chat title's length (characters), so a long first message
/// makes a sensible name rather than a wall of text in the picker.
const TITLE_CAP: usize = 60;

/// Cap on the prior-transcript digest folded back into a reopened chat's first
/// turn (M-S5), so resuming a long conversation doesn't blow the context window.
/// Keeps the most recent turns (the tail), which is what a follow-up references.
const SEED_CAP: usize = 6_000;

/// A one-line title from a chat's first user message: the first non-empty line,
/// whitespace-collapsed and capped. Used as the saved file's display name.
fn derive_title(message: &str) -> String {
    let line = message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > TITLE_CAP {
        let truncated: String = collapsed.chars().take(TITLE_CAP).collect();
        format!("{}…", truncated.trim_end())
    } else if collapsed.is_empty() {
        "Untitled chat".to_string()
    } else {
        collapsed
    }
}

/// Render a saved transcript as a compact `You:` / `Assistant:` digest to seed a
/// reopened chat's next turn (M-S5). Returns `None` for an empty transcript. The
/// digest is capped to its tail ([`SEED_CAP`]) — the recent turns a follow-up
/// actually depends on — so resuming a long chat stays within budget.
fn render_transcript(messages: &[crate::conversations::StoredMessage]) -> Option<String> {
    let mut out = String::new();
    for m in messages {
        let text = m.text.trim();
        if text.is_empty() {
            continue;
        }
        let who = if m.role == "assistant" {
            "Assistant"
        } else {
            "You"
        };
        out.push_str(who);
        out.push_str(": ");
        out.push_str(text);
        out.push_str("\n\n");
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Keep the tail if it's over budget, on a turn-ish boundary where possible.
    if trimmed.len() > SEED_CAP {
        // Step the start forward to a UTF-8 char boundary so slicing can't panic.
        let mut start = trimmed.len() - SEED_CAP;
        while start < trimmed.len() && !trimmed.is_char_boundary(start) {
            start += 1;
        }
        let slice = &trimmed[start..];
        let cut = slice.find("\n\n").map(|i| i + 2).unwrap_or(0);
        return Some(format!("…(earlier turns omitted)\n\n{}", &slice[cut..]));
    }
    Some(trimmed.to_string())
}

/// A cheap stable-ish element key for a bubble (its text length + a prefix hash).
/// Bubbles are rendered in order and the panel rebuilds each frame, so this only
/// needs to be unique among the currently-shown bubbles.
fn bubble_key(msg: &ChatMessage) -> usize {
    msg.text
        .len()
        .wrapping_add(msg.thinking.len().wrapping_mul(31))
}

/// Pull the first fenced ```sql block out of an assistant message, if any.
fn extract_sql(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let start = lower.find("```sql")?;
    let after = start + "```sql".len();
    let rest = &text[after..];
    let body_start = rest.find('\n')? + 1;
    let body = &rest[body_start..];
    let end = body.find("```")?;
    let sql = body[..end].trim();
    if sql.is_empty() {
        None
    } else {
        Some(sql.to_string())
    }
}

/// A compact `schema.table` overview for the system prompt, capped so a huge
/// database stays within budget. Full per-table detail is fetched on demand by
/// the model's `describe_table` tool.
fn summarize_schema(schemas: &[red_core::SchemaMeta]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let mut shown = 0usize;
    let mut total = 0usize;
    for sch in schemas {
        for obj in &sch.objects {
            total += 1;
            if shown < SCHEMA_SUMMARY_CAP {
                let _ = writeln!(out, "{}.{}", sch.name, obj.name);
                shown += 1;
            }
        }
    }
    if total > shown {
        let _ = write!(out, "… and {} more objects", total - shown);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_first_sql_fence() {
        let md = "Here you go:\n```sql\nSELECT 1;\n```\nDone.";
        assert_eq!(extract_sql(md).as_deref(), Some("SELECT 1;"));
        assert_eq!(extract_sql("no code here"), None);
        assert_eq!(extract_sql("```sql\n\n```"), None);
    }

    #[test]
    fn compacts_token_counts() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_200), "1.2k");
        assert_eq!(compact_count(2_000_000), "2.0M");
    }

    #[test]
    fn title_is_first_line_collapsed_and_capped() {
        assert_eq!(derive_title("How many users?"), "How many users?");
        // Leading blank lines skipped; whitespace collapsed.
        assert_eq!(
            derive_title("\n\n  list   the  tables \n"),
            "list the tables"
        );
        // Over-long titles are truncated with an ellipsis.
        let long = "a ".repeat(80);
        let title = derive_title(&long);
        assert!(title.ends_with('…'));
        assert!(title.chars().count() <= TITLE_CAP + 1);
        // Empty input has a sensible fallback.
        assert_eq!(derive_title("   \n  "), "Untitled chat");
    }

    #[test]
    fn transcript_digest_renders_roles_and_skips_empties() {
        let msgs = vec![
            crate::conversations::StoredMessage {
                role: "user".into(),
                text: "hi".into(),
                thinking: String::new(),
            },
            crate::conversations::StoredMessage {
                role: "assistant".into(),
                text: "hello".into(),
                thinking: "ignored".into(),
            },
            crate::conversations::StoredMessage {
                role: "assistant".into(),
                text: "   ".into(),
                thinking: String::new(),
            },
        ];
        let seed = render_transcript(&msgs).expect("non-empty");
        assert!(seed.contains("You: hi"));
        assert!(seed.contains("Assistant: hello"));
        // Empty-text turns are skipped; thinking isn't seeded.
        assert!(!seed.contains("ignored"));
        // An all-empty transcript yields nothing to seed.
        assert!(render_transcript(&[]).is_none());
    }

    #[test]
    fn quick_action_prompts_are_distinct_and_nonempty() {
        let explain = QuickAction::ExplainError.prompt();
        let optimize = QuickAction::OptimizeQuery.prompt();
        assert!(!explain.trim().is_empty());
        assert!(!optimize.trim().is_empty());
        assert_ne!(explain, optimize);
    }
}
