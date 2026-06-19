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
//!
//! The panel hosts **several conversations at once** (M-S6): each [`ChatSession`]
//! carries its own transcript, streaming state, and **provider binding** — so one
//! chat can run on the Claude subscription (ACP) while another runs on the API-key
//! backend, live simultaneously. The composer/transcript show the *active* chat;
//! background chats keep streaming (events route by `conversation_id` to whichever
//! chat owns them) and a switcher lists them all.

use std::time::Duration;

use flint::prelude::*;
use flint::{CodeEditor, CodeEditorEvent, TextInput, TextInputEvent};
use gpui::{
    div, prelude::*, px, Animation, AnimationExt, AnyElement, AsyncApp, Context, Entity,
    ScrollHandle, SharedString, WeakEntity, Window,
};

use crate::app::{ActiveConn, AppState, Phase};

/// Cap on schema objects folded into the grounding summary, so a database with
/// thousands of tables doesn't blow the context window. The model pulls full
/// detail on demand via `describe_table`, so a names-only overview is enough.
const SCHEMA_SUMMARY_CAP: usize = 200;

/// Streaming reveal cadence: the assistant's answer types out at this tick rate
/// (≈40fps), decoupling the on-screen reveal from the uneven network bursts the
/// model's text actually arrives in — the ChatGPT-style steady stream.
const REVEAL_TICK: Duration = Duration::from_millis(24);
/// Reveal speed: each tick uncovers `remaining / DIVISOR` more characters (a
/// natural ease-out — fast when far behind, slowing as it catches up), but never
/// fewer than `MIN_STEP`, so a big backlog drains quickly and the tail still moves.
const REVEAL_DIVISOR: usize = 6;
const REVEAL_MIN_STEP: usize = 2;

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

/// One open conversation. The panel holds several of these (M-S6); the active one
/// drives the composer/transcript while the rest keep streaming in the background.
/// Each carries its own provider binding, so chats on different backends coexist.
pub(crate) struct ChatSession {
    /// Scroll position of this chat's transcript, kept across frames.
    pub(crate) scroll: ScrollHandle,
    /// The rendered conversation, oldest first.
    pub(crate) messages: Vec<ChatMessage>,
    /// Stable id tying this chat's turns together so the backend keeps history
    /// separate and routes deltas back to the right thread.
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
    /// Which backend this chat runs on (`"subscription"`, `"anthropic"`, …). Chosen
    /// at creation (defaulting from `[ai] provider`) and persisted as the
    /// conversation's provider binding (M-S5); turns carry it so the right backend
    /// handles them (M-S6). Locked once the first message is sent.
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
    /// A never-sent chat's prepared prompt, preserved across switches so the one
    /// "draft" keeps its text when you leave it and come back. Empty once the chat
    /// has sent a turn; the composer mirrors it while the draft is active.
    pub(crate) draft: String,
    /// Characters of the streaming assistant bubble currently revealed. The model's
    /// text arrives in uneven network bursts; a steady reveal ticker walks this up
    /// to the received length so the answer types out smoothly (ChatGPT-style)
    /// rather than jumping in chunks. Reset at the start of each turn.
    pub(crate) revealed: usize,
    /// Whether a reveal ticker is currently scheduled for this chat (so deltas don't
    /// spawn a second one). See `ensure_reveal_ticker`.
    pub(crate) revealing: bool,
}

impl ChatSession {
    /// A fresh, empty chat on `provider` with the given stable id.
    fn new(conversation_id: u64, provider: String) -> Self {
        ChatSession {
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
            draft: String::new(),
            revealed: 0,
            revealing: false,
        }
    }

    /// Char length of the streaming assistant bubble's text (the reveal target).
    fn streaming_text_chars(&self) -> usize {
        match self.messages.last() {
            Some(m) if m.role == ChatRole::Assistant => m.text.chars().count(),
            _ => 0,
        }
    }

    /// Whether this chat has nothing sent yet — the panel's single editable draft.
    fn is_draft(&self) -> bool {
        self.messages.is_empty()
    }

    /// Whether this chat runs on the Claude subscription (ACP) path.
    fn is_subscription(&self) -> bool {
        self.provider.eq_ignore_ascii_case("subscription")
    }

    /// The backend this chat's turns route to (M-S6).
    fn provider_kind(&self) -> red_service::AiProviderKind {
        if self.is_subscription() {
            red_service::AiProviderKind::Subscription
        } else {
            red_service::AiProviderKind::ApiKey
        }
    }

    /// A short backend label for the per-chat indicator.
    fn provider_label(&self) -> &'static str {
        if self.is_subscription() {
            "Subscription"
        } else {
            "API key"
        }
    }

    /// Whether this chat needs the user's attention while it isn't shown — a parked
    /// permission prompt the agent is blocked on. Drives the switcher's dot.
    fn needs_attention(&self) -> bool {
        self.pending_permission.is_some()
    }

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

/// Which conversation a history-sidebar row refers to — an open chat (by its
/// stable id) or a saved-but-closed conversation (by its file stem). Used to
/// target rename/delete without threading indices around.
#[derive(Clone, PartialEq)]
pub(crate) enum RowKey {
    Open(u64),
    Saved(String),
}

/// An in-progress inline rename in the history sidebar: which row, and the field
/// holding the edited title. Enter commits, Esc cancels.
pub(crate) struct Rename {
    key: RowKey,
    pub(crate) input: Entity<TextInput>,
    #[allow(dead_code)]
    sub: gpui::Subscription,
}

/// One flattened row of the merged history sidebar — an open chat (the draft, or a
/// sent one) or a saved-but-closed conversation. Built fresh each render.
struct HistoryRow {
    key: RowKey,
    /// Index into `chats` for an open row (drives switch); `None` for a saved one.
    open_index: Option<usize>,
    /// Index into `loaded_conversations` for a saved row (drives restore).
    saved_index: Option<usize>,
    title: String,
    subtitle: String,
    subscription: bool,
    active: bool,
    attention: bool,
    /// The single editable draft — no rename/delete affordances; named live.
    draft: bool,
}

/// Whether a saved conversation's recorded provider is the subscription path.
fn provider_is_subscription(provider: &str) -> bool {
    provider.eq_ignore_ascii_case("subscription")
}

/// All the assistant panel's state. Present iff the panel is open.
pub(crate) struct AssistantState {
    /// The prompt box — a multiline composer. Enter sends a turn on the active
    /// chat; Shift+Enter inserts a newline (see Flint `CodeEditor::submit_on_enter`).
    pub(crate) input: Entity<CodeEditor>,
    /// The API-key box, shown in the setup view when no key is configured.
    pub(crate) key_input: Entity<TextInput>,
    /// The history sidebar's search box; filters the merged list by title.
    pub(crate) list_search: Entity<TextInput>,
    /// Submit listeners (prompt + key); held here so closing the panel drops them.
    #[allow(dead_code)]
    sub: gpui::Subscription,
    #[allow(dead_code)]
    key_sub: gpui::Subscription,
    /// Re-renders the sidebar as the search query changes.
    #[allow(dead_code)]
    search_sub: gpui::Subscription,
    /// The open conversations (M-S6). Never empty while the panel is open.
    pub(crate) chats: Vec<ChatSession>,
    /// Index of the active chat in `chats` — the one the composer/transcript show.
    pub(crate) active: usize,
    /// Whether the history sidebar (open chats + saved conversations) is shown in
    /// place of the active transcript.
    pub(crate) show_list: bool,
    /// An in-progress inline title rename, if any.
    pub(crate) renaming: Option<Rename>,
}

impl AssistantState {
    /// The active chat (the one shown). `chats` is never empty, so this can't fail.
    fn active(&self) -> &ChatSession {
        &self.chats[self.active.min(self.chats.len() - 1)]
    }

    /// The active chat, mutably.
    fn active_mut(&mut self) -> &mut ChatSession {
        let i = self.active.min(self.chats.len() - 1);
        &mut self.chats[i]
    }

    /// Find a chat by its conversation id (events route here, not just to active).
    fn find_mut(&mut self, conversation_id: u64) -> Option<&mut ChatSession> {
        self.chats
            .iter_mut()
            .find(|c| c.conversation_id == conversation_id)
    }
}

impl AppState {
    /// Whether the AI assistant is enabled for the current context (M-S7): the
    /// active connection's `ai_enabled` override, falling back to the global
    /// `[ai] enabled`. `false` is a true kill switch — the panel can't be opened,
    /// its status-bar toggle is hidden, and the backend refuses turns and starts
    /// no agent. The tier (`off`/`schema`/`read`) is a separate, in-panel concern;
    /// this gate is purely on/off.
    pub(crate) fn ai_enabled(&self) -> bool {
        let global = self.settings.ai.enabled;
        match &self.phase {
            Phase::Connected(active) => active.config.ai_enabled.unwrap_or(global),
            _ => global,
        }
    }

    /// Open or close the assistant panel (⌘L). Only meaningful while connected and
    /// while the assistant is enabled for this connection (M-S7).
    pub(crate) fn toggle_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Connected(_)) || !self.ai_enabled() {
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
            // A multiline composer: no gutter, Enter sends, Shift+Enter newlines.
            let input = cx.new(|cx| {
                CodeEditor::new(cx)
                    .gutter(false)
                    .submit_on_enter(true)
                    .a11y_label("Assistant prompt")
                    .placeholder("Ask about this database…")
            });
            let sub = cx.subscribe(&input, |this, _, e: &CodeEditorEvent, cx| match e {
                // Enter (or ⌘↵) sends; Esc stops an in-flight turn from the keyboard
                // (a no-op when nothing is streaming).
                CodeEditorEvent::Submit | CodeEditorEvent::Run => this.submit_assistant(cx),
                CodeEditorEvent::Escape => this.cancel_assistant(cx),
            });
            let key_input = cx.new(|cx| TextInput::new(cx).obscured().with_placeholder("sk-ant-…"));
            let key_sub = cx.subscribe(&key_input, |this, _, e: &TextInputEvent, cx| {
                if matches!(e, TextInputEvent::Submit) {
                    this.save_ai_key(cx);
                }
            });
            let list_search = cx.new(|cx| {
                TextInput::new(cx)
                    .bare()
                    .tab_stop(false)
                    .with_placeholder("Search conversations…")
            });
            // A Change on the search box re-renders the filtered list.
            let search_sub = cx.subscribe(&list_search, |this, _, e: &TextInputEvent, cx| {
                if matches!(e, TextInputEvent::Change) {
                    if let Some(state) = this.assistant.as_ref() {
                        if state.show_list {
                            cx.notify();
                        }
                    }
                }
            });
            let provider = self.default_ai_provider();
            self.assistant = Some(AssistantState {
                input,
                key_input,
                list_search,
                sub,
                key_sub,
                search_sub,
                chats: vec![ChatSession::new(conversation_id, provider)],
                active: 0,
                show_list: false,
                renaming: None,
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

    /// The default backend for a new chat: `[ai] provider`, or `anthropic`.
    fn default_ai_provider(&self) -> String {
        if self.settings.ai.provider.is_empty() {
            "anthropic".to_string()
        } else {
            self.settings.ai.provider.clone()
        }
    }

    /// Send the prompt box's contents as one turn on the active chat.
    pub(crate) fn submit_assistant(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        if state.active().streaming {
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

    /// Record a user turn and dispatch it to the backend on the active chat. The
    /// caller has already resolved the message text (typed, or a quick-action
    /// prompt). The chat's own provider binding (M-S6) decides which backend runs
    /// it, so concurrent chats on different backends each route correctly.
    fn send_turn(&mut self, message: String, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        if state.active().streaming || message.trim().is_empty() {
            return;
        }
        let conversation_id = state.active().conversation_id;
        let provider = state.active().provider_kind();
        let (session, mut context) = {
            let Phase::Connected(active) = &self.phase else {
                return;
            };
            (active.session, self.ai_context(active, cx))
        };

        if let Some(state) = self.assistant.as_mut() {
            let chat = state.active_mut();
            // A reopened chat seeds its prior transcript into this one turn so the
            // model resumes coherently despite a fresh backend session (M-S5).
            context.prior_transcript = chat.pending_seed.take();
            // Title the chat from its first user message (used as the saved name).
            if chat.title.is_none() {
                chat.title = Some(derive_title(&message));
            }
            chat.messages.push(ChatMessage {
                role: ChatRole::User,
                text: message.clone(),
                thinking: String::new(),
            });
            chat.error = None;
            chat.status = None;
            chat.streaming = true;
            // Fresh turn: the next assistant bubble reveals from the start.
            chat.revealed = 0;
            // It's no longer a draft — drop any preserved prompt text.
            chat.draft.clear();
        }

        self.service.send_to(
            session,
            red_service::Command::AiTurn {
                conversation_id,
                provider,
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
        let conversation_id = state.active().conversation_id;
        if let Phase::Connected(active) = &self.phase {
            self.service.send_to(
                active.session,
                red_service::Command::AiReauthenticate { conversation_id },
            );
        }
        if let Some(state) = self.assistant.as_mut() {
            let chat = state.active_mut();
            chat.error = None;
            chat.status = Some("Restarting the assistant — sign in if the browser opens.".into());
        }
        cx.notify();
    }

    // --- conversation history (M-S5) ---------------------------------------

    /// Save the active chat's composer text into it, but only while it's the one
    /// editable draft (nothing sent yet). This is what lets the draft keep its
    /// prepared prompt when you switch away and come back, and what makes a cleared
    /// composer drop the draft out of the history list. A no-op for a sent chat.
    fn stash_active_draft(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_mut() else {
            return;
        };
        let i = state.active.min(state.chats.len() - 1);
        if state.chats[i].is_draft() {
            let text = state.input.read(cx).content();
            state.chats[i].draft = text;
        }
    }

    /// Load the composer with `text` (a chat's preserved draft, empty for a sent
    /// chat) and put the caret ready to type.
    fn load_composer(&mut self, text: String, cx: &mut Context<Self>) {
        if let Some(state) = self.assistant.as_ref() {
            state.input.update(cx, |i, cx| i.set_content(text, cx));
        }
    }

    /// Go to the panel's single draft — the one chat with nothing sent yet (the
    /// "prepared prompt"). Reuses the existing empty chat if there is one rather
    /// than spawning duplicates, so "new chat" always lands on the same draft.
    pub(crate) fn new_chat(&mut self, cx: &mut Context<Self>) {
        let provider = self.default_ai_provider();
        self.new_chat_with(provider, cx);
    }

    /// Go to the draft, binding it to `provider` if a fresh one is created. If a
    /// draft already exists its provider is left as-is (change it via the empty
    /// chat's provider picker); this just avoids piling up empty chats.
    pub(crate) fn new_chat_with(&mut self, provider: String, cx: &mut Context<Self>) {
        self.stash_active_draft(cx);
        let id = self.next_conversation_id;
        let existing = self
            .assistant
            .as_ref()
            .and_then(|s| s.chats.iter().position(|c| c.is_draft()));
        let mut created = false;
        if let Some(state) = self.assistant.as_mut() {
            let idx = match existing {
                Some(i) => i,
                None => {
                    state.chats.push(ChatSession::new(id, provider));
                    created = true;
                    state.chats.len() - 1
                }
            };
            state.active = idx;
            state.show_list = false;
            state.renaming = None;
            let text = state.chats[idx].draft.clone();
            state.input.update(cx, |i, cx| i.set_content(text, cx));
        }
        if created {
            self.next_conversation_id += 1;
        }
        self.focus_assistant = true;
        cx.notify();
    }

    /// Switch the active chat to the one at `index` (a sidebar row click), keeping
    /// the outgoing draft's text and restoring the incoming chat's.
    pub(crate) fn switch_chat(&mut self, index: usize, cx: &mut Context<Self>) {
        self.stash_active_draft(cx);
        let text = if let Some(state) = self.assistant.as_mut() {
            if index >= state.chats.len() {
                return;
            }
            state.active = index;
            state.show_list = false;
            state.renaming = None;
            state.chats[index].draft.clone()
        } else {
            return;
        };
        self.load_composer(text, cx);
        self.focus_assistant = true;
        cx.notify();
    }

    /// Toggle the history sidebar. Opening it stashes the live draft (so a cleared
    /// composer drops the draft from the list) and loads saved conversations from
    /// disk so external edits/deletions show up.
    pub(crate) fn toggle_chat_list(&mut self, cx: &mut Context<Self>) {
        let opening = self.assistant.as_ref().is_some_and(|s| !s.show_list);
        if opening {
            self.open_history_sidebar(cx);
        } else if let Some(state) = self.assistant.as_mut() {
            state.show_list = false;
            state.renaming = None;
            self.focus_assistant = true;
            cx.notify();
        }
    }

    /// Open the merged history sidebar (open chats + saved conversations). The
    /// command-palette "conversation history" entry routes here too, so there's one
    /// place history lives. Loads the saved files on demand.
    pub(crate) fn open_history_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.assistant.is_none() {
            return;
        }
        self.stash_active_draft(cx);
        self.loaded_conversations = crate::conversations::load();
        if let Some(state) = self.assistant.as_mut() {
            state.show_list = true;
            state.renaming = None;
            state.list_search.update(cx, |i, cx| i.set_content("", cx));
        }
        cx.notify();
    }

    /// Close one open chat (the switcher's per-row ✕), persisting it first. Keeps
    /// the open set bounded without deleting the saved file — it's still reopenable
    /// from history. If it was the last chat, a fresh empty one takes its place so
    /// the panel always has an active conversation.
    pub(crate) fn close_chat(&mut self, conversation_id: u64, cx: &mut Context<Self>) {
        // Mint a replacement id up front to avoid borrowing `self` twice.
        let replacement_id = self.next_conversation_id;
        let replacement_provider = self.default_ai_provider();
        if let Some(state) = self.assistant.as_mut() {
            let Some(idx) = state
                .chats
                .iter()
                .position(|c| c.conversation_id == conversation_id)
            else {
                return;
            };
            persist_chat(&mut state.chats[idx]);
            state.chats.remove(idx);
            if state.chats.is_empty() {
                state
                    .chats
                    .push(ChatSession::new(replacement_id, replacement_provider));
                state.active = 0;
                self.next_conversation_id += 1;
            } else if state.active >= state.chats.len() {
                state.active = state.chats.len() - 1;
            } else if idx < state.active {
                state.active -= 1;
            }
        }
        cx.notify();
    }

    /// Set the active chat's provider, but only before its first message — the
    /// binding is locked once a turn is sent (a backend conversation is bound to
    /// it). Drives the empty-chat provider picker (M-S6).
    pub(crate) fn set_active_chat_provider(&mut self, provider: String, cx: &mut Context<Self>) {
        if let Some(state) = self.assistant.as_mut() {
            let chat = state.active_mut();
            if chat.messages.is_empty() {
                chat.provider = provider;
            }
        }
        self.focus_assistant = true;
        cx.notify();
    }

    /// Reopen a saved conversation (history-picker activation). If it's already open
    /// in a chat, just switch to it; otherwise open it as a new chat, switching to
    /// it. The visible transcript comes back as-is; a fresh conversation id + the
    /// prior transcript folded into the next turn (`pending_seed`) means the backend
    /// starts a clean session that's still grounded in what was said before.
    pub(crate) fn restore_conversation(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(conv) = self.loaded_conversations.get(index).cloned() else {
            return;
        };
        // Already open? Switch to it rather than opening a duplicate.
        if let Some(state) = self.assistant.as_ref() {
            if let Some(i) = state
                .chats
                .iter()
                .position(|c| c.file_stem.as_deref() == Some(conv.stem.as_str()))
            {
                self.switch_chat(i, cx);
                return;
            }
        }
        self.stash_active_draft(cx);
        let id = self.next_conversation_id;
        self.next_conversation_id += 1;
        let seed = render_transcript(&conv.messages);
        if let Some(state) = self.assistant.as_mut() {
            let mut chat = ChatSession::new(id, conv.provider.clone());
            chat.messages = conv
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
            chat.title = Some(conv.title.clone());
            chat.file_stem = Some(conv.stem.clone());
            chat.created_unix = Some(conv.created_unix);
            chat.pending_seed = seed;
            state.chats.push(chat);
            state.active = state.chats.len() - 1;
            state.show_list = false;
            state.renaming = None;
        }
        // A restored chat is sent, so the composer starts empty.
        self.load_composer(String::new(), cx);
        self.focus_assistant = true;
        cx.notify();
    }

    /// Remove a history-sidebar row: delete its saved file (if any) and close the
    /// chat if it's open. Used by the per-row trash; the merged list *is* the
    /// history, so removing a row deletes the conversation for good.
    pub(crate) fn delete_conversation_row(&mut self, key: RowKey, cx: &mut Context<Self>) {
        let stem = match &key {
            RowKey::Open(id) => self
                .assistant
                .as_ref()
                .and_then(|s| s.chats.iter().find(|c| c.conversation_id == *id))
                .and_then(|c| c.file_stem.clone()),
            RowKey::Saved(stem) => Some(stem.clone()),
        };
        if let Some(stem) = &stem {
            if let Some(dir) = crate::conversations::conversations_dir() {
                let path = dir.join(format!("{stem}.json"));
                if let Err(e) = crate::conversations::delete(&path) {
                    tracing::warn!("failed to delete conversation: {e}");
                }
            }
            // Forget the just-deleted file so the list won't re-list it.
            self.loaded_conversations.retain(|c| &c.stem != stem);
        }
        if let RowKey::Open(id) = key {
            // Clear the stem first so closing doesn't re-save the deleted file.
            if let Some(state) = self.assistant.as_mut() {
                if let Some(chat) = state.find_mut(id) {
                    chat.file_stem = None;
                    chat.messages.clear();
                }
            }
            self.close_chat(id, cx);
        }
        cx.notify();
    }

    /// Begin renaming a row's title inline (its pencil button). Seeds a field with
    /// the current title; Enter commits, Esc cancels.
    pub(crate) fn begin_rename(&mut self, key: RowKey, title: String, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            TextInput::new(cx)
                .bare()
                .tab_stop(false)
                .with_content(title)
        });
        let sub = cx.subscribe(&input, |this, _, e: &TextInputEvent, cx| match e {
            TextInputEvent::Submit => this.commit_rename(cx),
            TextInputEvent::Cancel => this.cancel_rename(cx),
            TextInputEvent::Change => {}
        });
        if let Some(state) = self.assistant.as_mut() {
            state.renaming = Some(Rename { key, input, sub });
        }
        self.focus_rename = true;
        cx.notify();
    }

    /// Commit the inline rename to the open chat and/or its saved file.
    pub(crate) fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(rename) = self.assistant.as_mut().and_then(|s| s.renaming.take()) else {
            return;
        };
        let title = rename.input.read(cx).content().trim().to_string();
        if !title.is_empty() {
            match &rename.key {
                RowKey::Open(id) => {
                    if let Some(state) = self.assistant.as_mut() {
                        if let Some(chat) = state.find_mut(*id) {
                            chat.title = Some(title.clone());
                            // Rewrite the saved file's title if it's been saved.
                            if chat.file_stem.is_some() {
                                persist_chat(chat);
                            }
                        }
                    }
                }
                RowKey::Saved(stem) => {
                    if let Some(conv) = self
                        .loaded_conversations
                        .iter_mut()
                        .find(|c| &c.stem == stem)
                    {
                        conv.title = title.clone();
                        if let Err(e) = crate::conversations::save(stem, conv) {
                            tracing::warn!("failed to rename conversation: {e}");
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    /// Abandon an in-progress inline rename.
    pub(crate) fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.assistant.as_mut() {
            state.renaming = None;
        }
        cx.notify();
    }

    /// Delete the active chat's saved file (the panel's Delete action) and close
    /// that chat. A never-saved chat just closes. The file is also user-deletable by
    /// hand — the next history open simply won't list it.
    pub(crate) fn delete_current_conversation(&mut self, cx: &mut Context<Self>) {
        let (conversation_id, stem) = match self.assistant.as_ref() {
            Some(state) => {
                let chat = state.active();
                (chat.conversation_id, chat.file_stem.clone())
            }
            None => return,
        };
        if let Some(stem) = stem {
            if let Some(dir) = crate::conversations::conversations_dir() {
                let path = dir.join(format!("{stem}.json"));
                if let Err(e) = crate::conversations::delete(&path) {
                    tracing::warn!("failed to delete conversation: {e}");
                }
            }
        }
        // Drop the chat without re-persisting the just-deleted file. Reuse
        // `close_chat`'s bookkeeping, but clear the stem first so it isn't re-saved.
        if let Some(state) = self.assistant.as_mut() {
            if let Some(chat) = state.find_mut(conversation_id) {
                chat.file_stem = None;
                chat.messages.clear();
            }
        }
        self.close_chat(conversation_id, cx);
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

    /// Whether the active chat has been saved (gates the Delete affordance).
    pub(crate) fn assistant_has_saved_chat(&self) -> bool {
        self.assistant
            .as_ref()
            .is_some_and(|s| s.active().file_stem.is_some())
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
        // The API-key path lives under the canonical `anthropic` name (the same
        // name `ai_config` reads, so mixed-provider chats find it).
        if let Err(e) = crate::secrets::set_ai_key("anthropic", &key) {
            tracing::warn!("failed to store AI key in keychain: {e}");
        }
        if let Some(state) = self.assistant.as_ref() {
            state.key_input.update(cx, |i, cx| i.set_content("", cx));
        }
        self.ai_configured = true;
        self.ai_api_key_available = true;
        self.service
            .send_global(red_service::Command::ConfigureAi(crate::app::ai_config(
                &self.settings,
            )));
        self.focus_assistant = true;
        cx.notify();
    }

    /// Stop the active chat's in-flight turn (the Stop button).
    pub(crate) fn cancel_assistant(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        if !state.active().streaming {
            return;
        }
        let conversation_id = state.active().conversation_id;
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
        // Under a reduced-motion preference, skip the typewriter entirely: text
        // appears the instant it arrives.
        let reduce_motion = cx.reduce_motion();
        let grew_text = {
            let Some(state) = self.assistant.as_mut() else {
                return;
            };
            // Route to whichever chat owns the turn — not just the active one, so a
            // background chat keeps streaming while another is shown (M-S6).
            let Some(chat) = state.find_mut(conversation_id) else {
                return;
            };
            let mut grew = false;
            match delta {
                red_service::AiDelta::Text(t) => {
                    chat.assistant_bubble().text.push_str(&t);
                    grew = true;
                }
                red_service::AiDelta::Thinking(t) => chat.assistant_bubble().thinking.push_str(&t),
                red_service::AiDelta::ToolStarted { name } => {
                    chat.status = Some(format!("Running {name}…").into());
                }
                red_service::AiDelta::ToolFinished { name, ok } => {
                    chat.status = Some(
                        if ok {
                            format!("{name} ✓")
                        } else {
                            format!("{name} failed")
                        }
                        .into(),
                    );
                }
            }
            // Reduced motion reveals everything at once; otherwise the ticker walks
            // `revealed` up to the received length (started below).
            if grew && reduce_motion {
                chat.revealed = chat.streaming_text_chars();
            }
            grew
        };
        cx.notify();
        if grew_text && !reduce_motion {
            self.ensure_reveal_ticker(conversation_id, cx);
        }
    }

    /// Start the steady reveal ticker for a chat if one isn't already running and
    /// there's text waiting to be revealed. The ticker reschedules itself until the
    /// reveal catches up to the received text (see `tick_reveal`); a later burst
    /// restarts it. Cheap to call on every delta.
    fn ensure_reveal_ticker(&mut self, conversation_id: u64, cx: &mut Context<Self>) {
        {
            let Some(state) = self.assistant.as_mut() else {
                return;
            };
            let Some(chat) = state.find_mut(conversation_id) else {
                return;
            };
            if chat.revealing || chat.revealed >= chat.streaming_text_chars() {
                return;
            }
            chat.revealing = true;
        }
        cx.spawn(
            async move |this: WeakEntity<Self>, cx: &mut AsyncApp| loop {
                cx.background_executor().timer(REVEAL_TICK).await;
                let keep_going = this
                    .update(cx, |this, cx| this.tick_reveal(conversation_id, cx))
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            },
        )
        .detach();
    }

    /// One reveal step: uncover more of the streaming bubble and repaint. Returns
    /// whether the ticker should fire again (false once it's caught up — a new burst
    /// will restart it via `ensure_reveal_ticker`).
    fn tick_reveal(&mut self, conversation_id: u64, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.assistant.as_mut() else {
            return false;
        };
        let Some(chat) = state.find_mut(conversation_id) else {
            return false;
        };
        let target = chat.streaming_text_chars();
        if chat.revealed >= target {
            chat.revealing = false;
            return false;
        }
        let remaining = target - chat.revealed;
        let step = (remaining / REVEAL_DIVISOR).max(REVEAL_MIN_STEP);
        chat.revealed = (chat.revealed + step).min(target);
        let caught_up = chat.revealed >= target;
        if caught_up {
            chat.revealing = false;
        }
        cx.notify();
        !caught_up
    }

    pub(crate) fn on_ai_finished(
        &mut self,
        conversation_id: u64,
        usage: red_service::AiUsage,
        cx: &mut Context<Self>,
    ) {
        let mut finished = false;
        if let Some(state) = self.assistant.as_mut() {
            if let Some(chat) = state.find_mut(conversation_id) {
                chat.streaming = false;
                chat.status = None;
                chat.pending_permission = None;
                // Keep a non-empty reading; a turn that reports nothing (some
                // refusals / cancels) leaves the prior footer in place.
                if usage != red_service::AiUsage::default() {
                    chat.last_usage = Some(usage);
                }
                // Persist the now-complete exchange so it survives a restart (M-S5).
                persist_chat(chat);
                finished = true;
            }
        }
        if finished {
            cx.notify();
            // Drain any still-hidden tail now that no more text is coming.
            self.ensure_reveal_ticker(conversation_id, cx);
        }
    }

    pub(crate) fn on_ai_error(
        &mut self,
        conversation_id: u64,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.assistant.as_mut() {
            if let Some(chat) = state.find_mut(conversation_id) {
                chat.streaming = false;
                chat.status = None;
                chat.error = Some(message.into());
                // A prompt can't outlive its turn — drop any unanswered one.
                chat.pending_permission = None;
                cx.notify();
            }
        }
    }

    /// The agent asked to run a tool Red didn't auto-allow (M-S2): show the prompt
    /// on its originating chat (the switcher flags a background one).
    pub(crate) fn on_ai_permission_request(
        &mut self,
        conversation_id: u64,
        request_id: u64,
        title: String,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.assistant.as_mut() {
            if let Some(chat) = state.find_mut(conversation_id) {
                chat.pending_permission = Some(PendingPermission {
                    request_id,
                    title: title.into(),
                    detail: detail.map(Into::into),
                });
                cx.notify();
            }
        }
    }

    /// Answer the active chat's pending tool-permission prompt (its Allow/Deny
    /// buttons). The agent is blocked on this; denying is the safe default if it's
    /// dismissed.
    pub(crate) fn answer_permission(&mut self, allow: bool, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_mut() else {
            return;
        };
        let conversation_id = state.active().conversation_id;
        let Some(pending) = state.active_mut().pending_permission.take() else {
            return;
        };
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
        let chat = state.active();
        // Subscription mode (Claude Code over ACP) needs no API key and bills the
        // user's Pro/Max plan; the header reflects the active chat's backend.
        let is_subscription = chat.is_subscription();

        let header = self.render_assistant_header(state, &theme, cx);

        // Setup view: no provider usable yet (no key, default isn't subscription).
        if !self.ai_configured {
            return self.render_assistant_setup(state, header, &theme, cx);
        }

        // The chat-list switcher replaces the transcript while open (M-S6).
        if state.show_list {
            return self.render_assistant_list(state, header, &theme, cx);
        }

        // Transcript.
        let mut body = div()
            .id("assistant-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&chat.scroll)
            .flex()
            .flex_col()
            .gap_3()
            .p_3();

        if chat.messages.is_empty() {
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
            // Before the first message, the chat's backend can still be switched —
            // offer the picker when more than one provider is available (M-S6).
            if let Some(picker) = self.render_provider_picker(chat, &theme, cx) {
                body = body.child(picker);
            }
        }
        // The trailing assistant bubble types out while the turn streams (or while
        // the reveal is still draining just after it finishes); the rest show whole.
        let last = chat.messages.len().saturating_sub(1);
        for (i, msg) in chat.messages.iter().enumerate() {
            let live =
                i == last && msg.role == ChatRole::Assistant && (chat.streaming || chat.revealing);
            let reveal = live.then_some(chat.revealed);
            body = body.child(self.render_bubble(msg, reveal, &theme, cx));
        }
        if let Some(status) = &chat.status {
            body = body.child(
                div()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_muted)
                    .child(status.clone()),
            );
        }
        if let Some(err) = &chat.error {
            body = body.child(
                div()
                    .text_size(theme.scale(11.5))
                    .text_color(theme.red)
                    .child(err.clone()),
            );
        }

        // Composer: a multiline prompt box with a send (or stop) icon button. The
        // box is a fixed few lines tall and scrolls internally for longer prompts.
        let action: AnyElement = if chat.streaming {
            div()
                .id("assistant-stop")
                .size(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .tooltip(flint::Tooltip::text("Stop (Esc)"))
                .hover(|s| s.border_color(theme.red))
                .child(crate::icons::icon("x", theme.scale(14.), theme.text_muted))
                .on_click(cx.listener(|this, _, _, cx| this.cancel_assistant(cx)))
                .into_any_element()
        } else {
            div()
                .id("assistant-send")
                .size(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .bg(theme.red)
                .cursor_pointer()
                .tooltip(flint::Tooltip::text(
                    "Send (Enter · Shift+Enter for a new line)",
                ))
                .hover(|s| s.opacity(0.9))
                .child(crate::icons::icon("send", theme.scale(15.), theme.bg_app))
                .on_click(cx.listener(|this, _, _, cx| this.submit_assistant(cx)))
                .into_any_element()
        };

        let composer = div()
            .flex_shrink_0()
            .flex()
            .items_end()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h(px(64.))
                    .child(state.input.clone()),
            )
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
            .when_some(chat.pending_permission.as_ref(), |col, pending| {
                col.child(self.render_permission(pending, &theme, cx))
            })
            .when_some(self.render_quick_actions(chat, &theme, cx), |col, chips| {
                col.child(chips)
            })
            .child(composer)
            .when_some(chat.last_usage, |col, usage| {
                col.child(render_usage(&usage, &theme))
            })
            .into_any_element()
    }

    /// The panel header: title + active provider badge + action buttons.
    fn render_assistant_header(
        &self,
        state: &AssistantState,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chat = state.active();
        let is_subscription = chat.is_subscription();

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

        let close = icon_btn("assistant-close", "x", "Close assistant")
            .on_click(cx.listener(|this, _, window, cx| this.close_assistant(window, cx)));

        // The agent owns `/login`, so "Switch account" restarts it (M-S4); only
        // meaningful on the subscription path.
        let reauth = is_subscription.then(|| {
            icon_btn("assistant-reauth", "key-round", "Sign in / switch account")
                .on_click(cx.listener(|this, _, _, cx| this.reauthenticate_assistant(cx)))
        });

        // The chat switcher (M-S6): toggles a list of all open chats. A red dot
        // flags a background chat that needs attention (a parked permission).
        let needs_attention = state
            .chats
            .iter()
            .enumerate()
            .any(|(i, c)| i != state.active && c.needs_attention());
        // The toggle opens the merged history sidebar (open chats + saved); the
        // tooltip carries the open-chat count and a dot flags background attention.
        let list_tip = if state.chats.len() > 1 {
            SharedString::from(format!("History ({} open)", state.chats.len()))
        } else {
            SharedString::from("History")
        };
        let list_btn = self.ai_configured.then(|| {
            div()
                .id("assistant-list")
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .size(px(20.))
                .rounded(px(4.))
                .cursor_pointer()
                .tooltip(flint::Tooltip::text(list_tip))
                .hover(|s| s.bg(theme.bg_elevated))
                .child(crate::icons::icon(
                    if state.show_list {
                        "panel-left-close"
                    } else {
                        "panel-left-open"
                    },
                    theme.scale(13.),
                    theme.text_muted,
                ))
                .when(needs_attention, |b| {
                    b.child(
                        div()
                            .absolute()
                            .top(px(2.))
                            .right(px(2.))
                            .size(px(5.))
                            .rounded_full()
                            .bg(theme.red),
                    )
                })
                .on_click(cx.listener(|this, _, _, cx| this.toggle_chat_list(cx)))
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
            .when_some(list_btn, |row, l| row.child(l))
            .when_some(new_chat, |row, n| row.child(n))
            .when_some(delete, |row, d| row.child(d))
            .child(close);

        div()
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
            .child(header_actions)
            .into_any_element()
    }

    /// The setup view: no provider is usable yet (no API key, and the default isn't
    /// the subscription). Offer an inline key entry (stored in the OS keyring).
    fn render_assistant_setup(
        &self,
        state: &AssistantState,
        header: AnyElement,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
        div()
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
            .into_any_element()
    }

    /// The merged history sidebar: the single editable draft, the open chats, and
    /// the saved conversations on disk — one searchable list. Clicking a row opens
    /// or restores it; each non-draft row can be renamed or deleted in place.
    fn render_assistant_list(
        &self,
        state: &AssistantState,
        header: AnyElement,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let query = state.list_search.read(cx).content().trim().to_lowercase();
        let matches = |title: &str| query.is_empty() || title.to_lowercase().contains(&query);

        // Stems already open, so a saved conversation isn't listed twice.
        let open_stems: Vec<&str> = state
            .chats
            .iter()
            .filter_map(|c| c.file_stem.as_deref())
            .collect();

        // Flatten into ordered rows: the draft first, then open chats, then saved.
        let mut rows: Vec<HistoryRow> = Vec::new();
        for (i, c) in state.chats.iter().enumerate() {
            if c.is_draft() {
                let title = derive_title(&c.draft);
                if c.draft.trim().is_empty() || !matches(&title) {
                    continue;
                }
                rows.push(HistoryRow {
                    key: RowKey::Open(c.conversation_id),
                    open_index: Some(i),
                    saved_index: None,
                    title,
                    subtitle: "Draft".to_string(),
                    subscription: c.is_subscription(),
                    active: i == state.active,
                    attention: false,
                    draft: true,
                });
            }
        }
        for (i, c) in state.chats.iter().enumerate() {
            if c.is_draft() {
                continue;
            }
            let title = c
                .title
                .clone()
                .unwrap_or_else(|| "Untitled chat".to_string());
            if !matches(&title) {
                continue;
            }
            let turns = c
                .messages
                .iter()
                .filter(|m| m.role == ChatRole::User)
                .count();
            let mut subtitle = format!("{} · {turns} turns", c.provider_label());
            if c.streaming {
                subtitle.push_str(" · streaming");
            }
            rows.push(HistoryRow {
                key: RowKey::Open(c.conversation_id),
                open_index: Some(i),
                saved_index: None,
                title,
                subtitle,
                subscription: c.is_subscription(),
                active: i == state.active,
                attention: c.needs_attention(),
                draft: false,
            });
        }
        for (j, conv) in self.loaded_conversations.iter().enumerate() {
            if open_stems.contains(&conv.stem.as_str()) || !matches(&conv.title) {
                continue;
            }
            let turns = conv.messages.iter().filter(|m| m.role == "user").count();
            let label = if provider_is_subscription(&conv.provider) {
                "Subscription"
            } else {
                "API key"
            };
            rows.push(HistoryRow {
                key: RowKey::Saved(conv.stem.clone()),
                open_index: None,
                saved_index: Some(j),
                title: conv.title.clone(),
                subtitle: format!("{label} · {turns} turns"),
                subscription: provider_is_subscription(&conv.provider),
                active: false,
                attention: false,
                draft: false,
            });
        }

        let mut list = div()
            .id("assistant-chat-list")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .p_2();
        if rows.is_empty() {
            let hint = if query.is_empty() {
                "No conversations yet — they're kept here as you chat."
            } else {
                "No conversations match your search."
            };
            list = list.child(
                div()
                    .p_2()
                    .text_size(theme.scale(11.5))
                    .text_color(theme.text_muted)
                    .child(hint),
            );
        } else {
            for row in rows {
                list = list.child(self.render_history_row(row, state.renaming.as_ref(), theme, cx));
            }
        }

        // Search box, docked under the header.
        let search = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1p5()
            .px_3()
            .h(px(30.))
            .border_b_1()
            .border_color(theme.border)
            .child(crate::icons::icon(
                "search",
                theme.scale(12.),
                theme.text_muted,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(state.list_search.clone()),
            );

        // Footer: a single "New chat" that lands on the draft.
        let new_button = div()
            .id("assistant-new-chat-footer")
            .flex()
            .items_center()
            .justify_center()
            .gap_1p5()
            .h(px(30.))
            .rounded(px(6.))
            .border_1()
            .border_color(theme.border)
            .text_size(theme.scale(11.5))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| s.border_color(theme.red).text_color(theme.red))
            .child(crate::icons::icon(
                "plus",
                theme.scale(11.),
                theme.text_muted,
            ))
            .child("New chat")
            .on_click(cx.listener(|this, _, _, cx| this.new_chat(cx)));
        let footer = div()
            .flex_shrink_0()
            .p_2()
            .border_t_1()
            .border_color(theme.border)
            .child(new_button);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_panel_2)
            .border_l_1()
            .border_color(theme.border)
            .child(header)
            .child(search)
            .child(list)
            .child(footer)
            .into_any_element()
    }

    /// One row of the merged history sidebar (see [`HistoryRow`]). Click opens or
    /// restores the conversation; a pencil renames it inline and a trash deletes it
    /// (both hidden for the live draft). While a row is being renamed, its title is
    /// replaced by an edit field.
    fn render_history_row(
        &self,
        row: HistoryRow,
        renaming: Option<&Rename>,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id_key = match &row.key {
            RowKey::Open(id) => format!("open-{id}"),
            RowKey::Saved(stem) => format!("saved-{stem}"),
        };
        let renaming_here = renaming.filter(|r| r.key == row.key);

        let mut el = div()
            .id(SharedString::from(format!("history-row-{id_key}")))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded(px(5.))
            .when(row.active, |r| r.bg(theme.bg_elevated))
            .hover(|s| s.bg(theme.bg_elevated));

        // Provider glyph: sparkles for subscription, a key for API-key.
        el = el.child(crate::icons::icon(
            if row.subscription {
                "sparkles"
            } else {
                "key-round"
            },
            theme.scale(13.),
            theme.text_muted,
        ));

        if let Some(rename) = renaming_here {
            // Inline rename: the title becomes an edit field (Enter commits).
            el = el.child(div().flex_1().min_w(px(0.)).child(rename.input.clone()));
            return el.into_any_element();
        }

        // Clicking the row body opens/restores it.
        let open_index = row.open_index;
        let saved_index = row.saved_index;
        el = el
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(i) = open_index {
                    this.switch_chat(i, cx);
                } else if let Some(j) = saved_index {
                    this.restore_conversation(j, cx);
                }
            }));

        let title = if row.title.trim().is_empty() {
            "Untitled chat".to_string()
        } else {
            row.title.clone()
        };
        let text = div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(title.clone()),
            )
            .child(
                div()
                    .text_size(theme.scale(10.))
                    .text_color(theme.text_muted)
                    .child(row.subtitle.clone()),
            );
        el = el.child(text);

        if row.attention {
            el = el.child(div().size(px(6.)).rounded_full().bg(theme.red));
        }

        // Rename + delete affordances (not for the live draft, which is named live).
        if !row.draft {
            let small_btn = |id: String, glyph: &'static str, tip: &'static str| {
                div()
                    .id(SharedString::from(id))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(18.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .tooltip(flint::Tooltip::text(tip))
                    .hover(|s| s.bg(theme.bg_panel))
                    .child(crate::icons::icon(
                        glyph,
                        theme.scale(11.),
                        theme.text_muted,
                    ))
            };
            let key_rename = row.key.clone();
            el = el.child(
                small_btn(format!("history-rename-{id_key}"), "edit", "Rename").on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.begin_rename(key_rename.clone(), title.clone(), cx)
                    }),
                ),
            );
            let key_delete = row.key.clone();
            el = el.child(
                small_btn(format!("history-delete-{id_key}"), "trash", "Delete").on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.delete_conversation_row(key_delete.clone(), cx)
                    }),
                ),
            );
        }

        el.into_any_element()
    }

    /// The empty-chat provider picker (M-S6): two pills to bind a new chat to a
    /// backend before its first message. Shown only when both backends are usable
    /// (an API key is present *and* the subscription is an option) — otherwise the
    /// chat's single available provider needs no choice.
    fn render_provider_picker(
        &self,
        chat: &ChatSession,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // Only offer a choice when there's actually more than one provider.
        if !self.ai_api_key_available {
            return None;
        }
        let pill = |id: &'static str, label: &'static str, provider: &'static str, on: bool| {
            div()
                .id(id)
                .px_2()
                .h(px(24.))
                .flex()
                .items_center()
                .rounded(px(5.))
                .border_1()
                .text_size(theme.scale(11.))
                .cursor_pointer()
                .when(on, |s| s.border_color(theme.red).text_color(theme.red))
                .when(!on, |s| {
                    s.border_color(theme.border).text_color(theme.text_muted)
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_active_chat_provider(provider.to_string(), cx)
                }))
        };
        let sub = chat.is_subscription();
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(
                    div()
                        .text_size(theme.scale(10.5))
                        .text_color(theme.text_muted)
                        .child("Backend for this chat:"),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(pill("ai-pick-apikey", "API key", "anthropic", !sub))
                        .child(pill(
                            "ai-pick-subscription",
                            "Claude subscription",
                            "subscription",
                            sub,
                        )),
                )
                .into_any_element(),
        )
    }

    /// The context-action chips (M-S4): "Explain error" when the active result
    /// failed, "Optimize query" when the editor holds SQL. Shared by both providers
    /// (they ride the same `AiTurn`). Hidden while a turn streams, or when neither
    /// applies. Docked above the composer so they're reachable regardless of scroll.
    fn render_quick_actions(
        &self,
        chat: &ChatSession,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if chat.streaming {
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

    /// One chat bubble. `reveal` is `Some(n)` for the live, still-typing assistant
    /// bubble — only its first `n` characters show and a blinking caret trails them;
    /// `None` renders the whole message (every settled turn).
    fn render_bubble(
        &self,
        msg: &ChatMessage,
        reveal: Option<usize>,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let live = reveal.is_some();
        // The text currently on screen: the revealed prefix while typing, else all.
        let shown: &str = match reveal {
            Some(n) => take_chars(&msg.text, n),
            None => &msg.text,
        };
        let (label, label_color) = match msg.role {
            ChatRole::User => ("You", theme.text_muted),
            ChatRole::Assistant => ("Assistant", theme.red),
        };

        // Label row: the author, plus a copy-to-clipboard affordance for the
        // message text (assistant turns can be long; this beats hand-selecting).
        // Hidden while typing — the text isn't final yet.
        let mut label_row = div().flex().items_center().justify_between().child(
            div()
                .text_size(theme.scale(10.5))
                .text_color(label_color)
                .child(label),
        );
        if !live && !msg.text.trim().is_empty() {
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

        // Answer text. Assistant turns are Markdown — render them (on the revealed
        // prefix while typing); user turns are plain.
        if msg.role == ChatRole::Assistant {
            if !shown.is_empty() {
                bubble = bubble.child(crate::markdown::render(shown, theme));
            }
            // A blinking caret trails the revealed text while the model is typing
            // (and signals "still working" through tool calls / token gaps).
            if live {
                bubble = bubble.child(stream_caret(theme, cx.reduce_motion()));
            }
        } else {
            bubble = bubble.child(
                div()
                    .text_size(theme.scale(12.5))
                    .text_color(theme.text)
                    .child(msg.text.clone()),
            );
        }

        // "Insert into editor" for the first fenced SQL block in a *settled*
        // assistant turn (suppressed while still typing).
        if !live && msg.role == ChatRole::Assistant {
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

/// The first `n` characters of `s` (a byte-safe prefix), or all of it when shorter.
/// Drives the streaming reveal — slicing on a char boundary so multibyte text never
/// panics mid-codepoint.
fn take_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The streaming caret: a small block trailing the typed-out answer. It pulses
/// (ChatGPT-style) to read as "still generating"; under a reduced-motion
/// preference it rests solid.
fn stream_caret(theme: &flint::Theme, reduce_motion: bool) -> AnyElement {
    let bar = div().w(px(7.)).h(px(15.)).rounded(px(1.5)).bg(theme.text);
    if reduce_motion {
        return bar.into_any_element();
    }
    bar.with_animation(
        "ai-stream-caret",
        Animation::new(Duration::from_millis(1100)).repeat(),
        |bar, delta| {
            // A smooth 1→0→1 pulse over the period (cosine), floored so it never
            // fully vanishes.
            let o = 0.2 + 0.8 * (0.5 + 0.5 * (delta * std::f32::consts::TAU).cos());
            bar.opacity(o)
        },
    )
    .into_any_element()
}

/// Persist one chat to its flat file (one JSON per conversation), titled from its
/// first user message. Called after each finished turn and when a chat is closed. A
/// chat with no real assistant reply yet (only a pending/aborted user turn) isn't
/// saved — there's nothing worth keeping. Best-effort: a write failure is logged,
/// never surfaced mid-turn.
fn persist_chat(chat: &mut ChatSession) {
    // Need at least one assistant turn with content to be worth saving.
    let has_answer = chat
        .messages
        .iter()
        .any(|m| m.role == ChatRole::Assistant && !m.text.trim().is_empty());
    if !has_answer {
        return;
    }
    let title = chat
        .title
        .clone()
        .unwrap_or_else(|| "Untitled chat".to_string());
    // Choose a stable file stem the first time, then reuse it so later turns
    // overwrite in place rather than spawning a new file per turn.
    let stem = chat
        .file_stem
        .get_or_insert_with(|| crate::conversations::unique_stem(&title))
        .clone();
    let now = crate::conversations::now_unix();
    let created = *chat.created_unix.get_or_insert(now);
    let conv = crate::conversations::Conversation {
        title,
        provider: chat.provider.clone(),
        created_unix: created,
        updated_unix: now,
        messages: chat
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

    #[test]
    fn provider_kind_maps_from_binding() {
        let api = ChatSession::new(0, "anthropic".to_string());
        assert_eq!(api.provider_kind(), red_service::AiProviderKind::ApiKey);
        assert!(!api.is_subscription());
        let sub = ChatSession::new(1, "subscription".to_string());
        assert_eq!(
            sub.provider_kind(),
            red_service::AiProviderKind::Subscription
        );
        assert!(sub.is_subscription());
        // Case-insensitive, and an unknown name falls back to the API-key path.
        let weird = ChatSession::new(2, "SUBSCRIPTION".to_string());
        assert!(weird.is_subscription());
        let other = ChatSession::new(3, "openai".to_string());
        assert_eq!(other.provider_kind(), red_service::AiProviderKind::ApiKey);
    }
}
