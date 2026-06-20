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

use std::cell::RefCell;
use std::rc::Rc;
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
}

impl QuickAction {
    /// The canned prompt sent for this action.
    fn prompt(self) -> &'static str {
        match self {
            QuickAction::ExplainError => "Explain the error from my last query and how to fix it.",
        }
    }

    /// The chip's label.
    fn label(self) -> &'static str {
        match self {
            QuickAction::ExplainError => "Explain error",
        }
    }
}

/// One rendered turn in the panel. The assistant bubble accumulates streamed text
/// and (optionally) summarized thinking as deltas arrive.
pub(crate) struct ChatMessage {
    pub(crate) role: ChatRole,
    pub(crate) text: String,
    pub(crate) thinking: String,
    /// Frame-stable render artifacts (parsed Markdown + first SQL block), filled
    /// lazily and reused while `text` is unchanged — see [`ChatMessage::markdown`].
    cache: RefCell<MessageCache>,
}

/// Cached, frame-stable derivations of a bubble's `text`. A transcript repaint (e.g.
/// every reveal tick while *another* turn streams, or any `cx.notify`) rebuilds the
/// whole element tree; without this, every settled bubble would re-parse its
/// Markdown and re-scan for SQL each frame. Keyed by `len` so streaming growth
/// invalidates it (the text only ever appends, then freezes when the turn settles).
#[derive(Default)]
struct MessageCache {
    len: usize,
    blocks: Option<Rc<Vec<crate::markdown::Block>>>,
    sql: Option<SharedString>,
}

impl ChatMessage {
    fn new(role: ChatRole, text: String, thinking: String) -> Self {
        Self {
            role,
            text,
            thinking,
            cache: RefCell::new(MessageCache::default()),
        }
    }

    /// Reparse/rescan only when `text` changed since the last fill. `blocks` being
    /// `Some` doubles as the "computed" flag (so a `None` SQL result still counts).
    fn refresh_cache(&self) {
        let fresh = {
            let c = self.cache.borrow();
            c.blocks.is_some() && c.len == self.text.len()
        };
        if fresh {
            return;
        }
        let blocks = Rc::new(crate::markdown::parse(&self.text));
        let sql = extract_sql(&self.text).map(SharedString::from);
        let mut c = self.cache.borrow_mut();
        c.len = self.text.len();
        c.blocks = Some(blocks);
        c.sql = sql;
    }

    /// The parsed Markdown for this (settled) bubble, cached across frames.
    fn markdown(&self) -> Rc<Vec<crate::markdown::Block>> {
        self.refresh_cache();
        self.cache.borrow().blocks.clone().expect("just refreshed")
    }

    /// The first fenced SQL block in this (settled) bubble, cached across frames.
    fn sql_block(&self) -> Option<SharedString> {
        self.refresh_cache();
        self.cache.borrow().sql.clone()
    }
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
    /// Which agent this chat runs on — the agent profile's id (`"subscription"`,
    /// `"anthropic"`, `"codex"`, …). Chosen at creation (defaulting to the resolved
    /// default agent) and persisted as the conversation's binding (M-S5); turns carry
    /// it so the right backend handles them (M-S6). Locked once the first message is
    /// sent. (Field name kept as `provider` — it's the serialized key saved chats
    /// already use.)
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
    /// Whether a background chat finished a turn the user hasn't looked at yet —
    /// drives the history sidebar's unread dot. Set when a turn finishes on a chat
    /// that isn't the active one, cleared the moment it's switched to. In-memory
    /// only; a fresh session starts everything read.
    pub(crate) unread: bool,
    /// The agent's advertised slash commands (subscription path only), driving the
    /// composer's `/`-command picker. Populated by `AiCommandsAvailable` once the
    /// agent's session opens — so empty until this chat sends its first turn, and
    /// always empty on the API-key path. In-memory only.
    pub(crate) commands: Vec<red_service::AiCommand>,
    /// The agent's model / reasoning selectors (subscription path only), driving the
    /// composer dropdowns. Populated by `AiConfigOptionsAvailable` once the session
    /// opens. In-memory only.
    pub(crate) config_options: Vec<red_service::AiConfigOption>,
    /// Whether this chat already applied the central default model/reasoning to its
    /// fresh session (so a later `ConfigOptionUpdate` doesn't re-apply it and stomp a
    /// mid-chat choice). In-memory only.
    pub(crate) config_defaults_applied: bool,
}

impl ChatSession {
    /// A fresh, empty chat on `provider` with the given stable id.
    pub(crate) fn new(conversation_id: u64, provider: String) -> Self {
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
            unread: false,
            commands: Vec::new(),
            config_options: Vec::new(),
            config_defaults_applied: false,
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

    /// Whether this chat needs the user's attention while it isn't shown — a parked
    /// permission prompt the agent is blocked on. Drives the switcher's dot.
    fn needs_attention(&self) -> bool {
        self.pending_permission.is_some()
    }

    /// Ensure the trailing bubble is an assistant bubble (deltas append to it).
    fn assistant_bubble(&mut self) -> &mut ChatMessage {
        if !matches!(self.messages.last(), Some(m) if m.role == ChatRole::Assistant) {
            self.messages.push(ChatMessage::new(
                ChatRole::Assistant,
                String::new(),
                String::new(),
            ));
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

/// The lifecycle state a history row reflects through its leading dot — replacing
/// the provider glyph (which now lives in the subtitle text). See [`status_dot`].
#[derive(Clone, Copy, PartialEq)]
enum RowStatus {
    /// The single never-sent chat — a hollow circle.
    Draft,
    /// A turn is streaming right now — a pulsing dot.
    Streaming,
    /// A background turn finished that the user hasn't switched to — a filled dot.
    Unread,
    /// Nothing pending — a quiet muted dot.
    Idle,
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
    status: RowStatus,
    active: bool,
    attention: bool,
    /// The single editable draft — no rename/delete affordances; named live.
    draft: bool,
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
    /// The active chat's slash commands, mirrored here so the composer's completion
    /// provider (a plain closure with no access to `AppState`) can read them. Kept
    /// in sync with the active chat by [`AppState::sync_command_completions`].
    pub(crate) completion_commands: Rc<RefCell<Vec<red_service::AiCommand>>>,
    /// Which config selector's dropdown is currently open (its `config_id`), if any.
    /// `flint::Select` is stateless, so the open state lives here.
    pub(crate) open_config: Option<String>,
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

    /// The AI access tier in effect for the current context (Feature B): the active
    /// connection's `ai_tier` override, falling back to the global `[ai] tier`.
    /// Drives the "writes" safety badge — `Write` means the agent can propose
    /// data changes (each one still gated by per-statement approval).
    pub(crate) fn ai_tier_effective(&self) -> red_core::AiTier {
        let global = red_core::AiTier::parse(&self.settings.ai.tier);
        match &self.phase {
            Phase::Connected(active) => active.config.ai_tier.unwrap_or(global),
            _ => global,
        }
    }

    /// Whether the agent `id` runs over ACP (an external agent that owns its own
    /// auth — Claude subscription, Codex, a local agent). Resolved against the
    /// configured agents; an id no longer configured (a saved chat bound to a since-
    /// removed agent) falls back to the legacy `"subscription"` built-in convention.
    pub(crate) fn agent_is_acp(&self, id: &str) -> bool {
        self.usable_agents
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.is_acp)
            .unwrap_or_else(|| id.eq_ignore_ascii_case(crate::settings::BUILTIN_ACP_AGENT))
    }

    /// The display name for the agent `id` (the selector/header label). Falls back
    /// to the id itself when the agent is no longer configured.
    pub(crate) fn agent_name(&self, id: &str) -> SharedString {
        self.usable_agents
            .iter()
            .find(|a| a.id == id)
            .map(|a| SharedString::from(a.name.clone()))
            .unwrap_or_else(|| SharedString::from(id.to_string()))
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
            // Shared mirror of the active chat's slash commands, read by the
            // composer's completion provider (a closure with no access to state).
            let completion_commands: Rc<RefCell<Vec<red_service::AiCommand>>> =
                Rc::new(RefCell::new(Vec::new()));
            // A multiline composer: no gutter, Enter sends, Shift+Enter newlines.
            let input = cx.new({
                let commands = completion_commands.clone();
                let detail = completion_commands.clone();
                move |cx| {
                    CodeEditor::new(cx)
                        .gutter(false)
                        .submit_on_enter(true)
                        // The composer card draws the border; the editor stays
                        // borderless so it reads as one surface (Zed-style).
                        .resting_border(false)
                        // Prose composer: wrap long lines to the width instead of
                        // scrolling horizontally.
                        .soft_wrap(true)
                        .a11y_label("Agent prompt")
                        .placeholder("Message Claude Agent — / for commands")
                        // `/`-command picker: offer the agent's commands when the
                        // word under the cursor is a slash command (see
                        // `slash_candidates`); the popup shows each command's name
                        // and a dim description.
                        .completions(move |text, cursor| {
                            slash_candidates(&commands.borrow(), text, cursor)
                        })
                        .completion_detail(move |name| {
                            detail
                                .borrow()
                                .iter()
                                .find(|c| c.name == name)
                                .map(|c| SharedString::from(c.description.clone()))
                        })
                }
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
                completion_commands,
                open_config: None,
            });
            self.focus_assistant = true;
        }
        cx.notify();
    }

    /// The agent id a new chat starts on — the resolved default agent, falling back
    /// to the first usable agent when the default isn't usable (e.g. an API default
    /// with no key while an ACP agent is ready).
    fn default_ai_provider(&self) -> String {
        let default = self.settings.ai.resolved_default_agent();
        if self.usable_agents.iter().any(|a| a.id == default) {
            return default;
        }
        self.usable_agents
            .first()
            .map(|a| a.id.clone())
            .unwrap_or(default)
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
        // `/report …` is a shortcut: expand it into a clear instruction so the agent
        // builds and opens an HTML report (it reads the data, then calls
        // generate_report). Plain English ("make me a report about …") works too.
        let message = expand_slash_report(&message).unwrap_or(message);
        self.send_turn(message, cx);
    }

    /// A one-tap context action (M-S4): "Explain error" / "Optimize query". Each is
    /// just a canned prompt — `ai_context` already folds in the live error / editor
    /// SQL, so the turn is grounded without the user retyping it. Shared by both
    /// providers (it rides the same `AiTurn` path).
    pub(crate) fn assistant_quick_action(&mut self, kind: QuickAction, cx: &mut Context<Self>) {
        self.send_turn(kind.prompt().to_string(), cx);
    }

    /// Record a user turn and dispatch it to the backend on the active *sidebar*
    /// chat. The caller has already resolved the message text (typed, or a
    /// quick-action prompt). Delegates to [`Self::dispatch_turn`], the shared core
    /// used by the sidebar and the agent tabs alike.
    fn send_turn(&mut self, message: String, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        let conversation_id = state.active().conversation_id;
        let agent = state.active().provider.clone();
        self.dispatch_turn(conversation_id, agent, message, cx);
    }

    /// The shared turn-dispatch core: record the user message on whichever chat owns
    /// `conversation_id` (sidebar *or* agent tab), then send `Command::AiTurn`. The
    /// chat's own agent binding (M-S6) decides which backend runs it, so concurrent
    /// chats on different agents each route correctly.
    fn dispatch_turn(
        &mut self,
        conversation_id: u64,
        agent: String,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if message.trim().is_empty() {
            return;
        }
        let (session, mut context) = {
            let Phase::Connected(active) = &self.phase else {
                return;
            };
            (active.session, self.ai_context(active, cx))
        };
        let sent = self
            .with_chat_mut(conversation_id, |chat| {
                if chat.streaming {
                    return false;
                }
                // A reopened chat seeds its prior transcript into this one turn so
                // the model resumes coherently despite a fresh session (M-S5).
                context.prior_transcript = chat.pending_seed.take();
                // Title the chat from its first user message (used as the saved name).
                if chat.title.is_none() {
                    chat.title = Some(derive_title(&message));
                }
                chat.messages.push(ChatMessage::new(
                    ChatRole::User,
                    message.clone(),
                    String::new(),
                ));
                chat.error = None;
                chat.status = None;
                chat.streaming = true;
                // Fresh turn: the next assistant bubble reveals from the start.
                chat.revealed = 0;
                // It's no longer a draft — drop any preserved prompt text.
                chat.draft.clear();
                // Sending is explicit — always jump to the new message + the reply.
                chat.scroll.scroll_to_bottom();
                true
            })
            .unwrap_or(false);
        if !sent {
            return;
        }
        self.service.send_to(
            session,
            red_service::Command::AiTurn {
                conversation_id,
                agent,
                message,
                context,
            },
        );
        cx.notify();
    }

    /// Run `f` against the [`ChatSession`] that owns `conversation_id` (events route
    /// here, not just to the active chat). Returns `f`'s result, or `None` if no
    /// chat matches.
    fn with_chat_mut<R>(
        &mut self,
        conversation_id: u64,
        f: impl FnOnce(&mut ChatSession) -> R,
    ) -> Option<R> {
        self.assistant
            .as_mut()
            .and_then(|state| state.find_mut(conversation_id))
            .map(f)
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
        self.sync_command_completions();
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
            state.chats[index].unread = false;
            state.chats[index].draft.clone()
        } else {
            return;
        };
        self.sync_command_completions();
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
        // Tell the backend to drop this conversation's history/agent (M-S5) so its
        // state doesn't linger for the whole session. Session-less: it's keyed by
        // conversation_id on the shared AI state, and works even while disconnected.
        self.service
            .send_global(red_service::Command::AiForget { conversation_id });
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
        self.sync_command_completions();
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
                .map(|m| {
                    let role = if m.role == "assistant" {
                        ChatRole::Assistant
                    } else {
                        ChatRole::User
                    };
                    ChatMessage::new(role, m.text.clone(), m.thinking.clone())
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
        self.sync_command_completions();
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
        // Recompute the usable-agent list now the `anthropic` built-in has a key,
        // then re-push the config so the backend builds its provider.
        self.usable_agents = crate::app::usable_agents(&self.settings);
        self.ai_configured = !self.usable_agents.is_empty();
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

    /// Open `sql` in a fresh query tab in the workspace. A read-only SELECT runs
    /// automatically (the data lands in the grid); anything else is loaded for the
    /// user to run, so the editor's own write-confirm path still applies. Shared by
    /// the assistant's "Open in a query tab" chip and the agent's open_query tool.
    pub(crate) fn open_query_in_tab(&mut self, sql: String, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Connected(_)) {
            return;
        }
        self.new_query(cx);
        if let Phase::Connected(active) = &self.phase {
            if let Some(tab) = active.active() {
                tab.editor
                    .update(cx, |e, cx| e.set_content(sql.clone(), cx));
            }
        }
        if matches!(crate::sql::classify(&sql), crate::sql::StatementKind::Query) {
            self.run_editor_query(cx);
        }
        cx.notify();
    }

    /// The agent's `open_query` tool fired: open the SQL in a new query tab.
    pub(crate) fn on_ai_open_query(
        &mut self,
        _conversation_id: u64,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        self.open_query_in_tab(sql, cx);
    }

    /// Store the agent's advertised slash commands on their chat (M-S4). Refreshes
    /// the composer's command mirror if it's the active chat, so `/` offers them.
    pub(crate) fn on_ai_commands_available(
        &mut self,
        conversation_id: u64,
        commands: Vec<red_service::AiCommand>,
        cx: &mut Context<Self>,
    ) {
        let updated = self
            .with_chat_mut(conversation_id, |chat| chat.commands = commands)
            .is_some();
        if updated {
            self.sync_command_completions();
            cx.notify();
        }
    }

    /// Store the agent's model / reasoning selectors on their chat, then apply the
    /// central default (settings) once per fresh session — so a new chat opens on the
    /// user's last-chosen model/reasoning without retroactively touching other chats.
    pub(crate) fn on_ai_config_options_available(
        &mut self,
        conversation_id: u64,
        options: Vec<red_service::AiConfigOption>,
        cx: &mut Context<Self>,
    ) {
        let updated = self
            .with_chat_mut(conversation_id, |chat| chat.config_options = options)
            .is_some();
        if !updated {
            return;
        }
        self.apply_default_config(conversation_id, cx);
        cx.notify();
    }

    /// Apply the central default model/reasoning (from settings) to a chat's fresh
    /// session, once. For each defaulted selector whose stored value is advertised and
    /// differs from the agent's current pick, send a set so the new chat lands on the
    /// user's last choice. Guarded by `config_defaults_applied` so a later
    /// `ConfigOptionUpdate` doesn't re-apply over a mid-chat manual change.
    fn apply_default_config(&mut self, conversation_id: u64, cx: &mut Context<Self>) {
        let model = self.settings.ai.subscription_model.clone();
        let reasoning = self.settings.ai.subscription_reasoning.clone();
        let Some(chat) = self
            .assistant
            .as_mut()
            .and_then(|s| s.find_mut(conversation_id))
        else {
            return;
        };
        if chat.config_defaults_applied {
            return;
        }
        chat.config_defaults_applied = true;
        let to_apply = default_config_changes(&chat.config_options, &model, &reasoning);
        for (config_id, value) in to_apply {
            self.send_set_config_option(conversation_id, config_id, value, cx);
        }
    }

    /// The composer dropdown changed a selector: optimistically reflect it on the
    /// chat, persist it as the central default for future chats (last choice wins;
    /// existing chats untouched), and tell the backend to apply it to this session.
    pub(crate) fn change_config_option(
        &mut self,
        config_id: String,
        value: String,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.assistant.as_mut() else {
            return;
        };
        state.open_config = None;
        let conversation_id = state.active().conversation_id;
        let chat = state.active_mut();
        // Optimistic local update + remember the category for the settings write.
        let mut category = None;
        for opt in &mut chat.config_options {
            if opt.id == config_id {
                opt.current_value = value.clone();
                category = Some(opt.category);
            }
        }
        // Persist as the central default for new chats (not retroactive).
        match category {
            Some(red_service::AiConfigCategory::Model) => {
                self.settings.ai.subscription_model = value.clone();
                self.save_settings();
            }
            Some(red_service::AiConfigCategory::Reasoning) => {
                self.settings.ai.subscription_reasoning = value.clone();
                self.save_settings();
            }
            _ => {}
        }
        self.send_set_config_option(conversation_id, config_id, value, cx);
        cx.notify();
    }

    /// Send the backend a config change for one conversation (no settings write — the
    /// callers decide whether this is a user choice or a default being applied).
    fn send_set_config_option(
        &mut self,
        conversation_id: u64,
        config_id: String,
        value: String,
        _cx: &mut Context<Self>,
    ) {
        if let Phase::Connected(active) = &self.phase {
            self.service.send_to(
                active.session,
                red_service::Command::AiSetConfigOption {
                    conversation_id,
                    config_id,
                    value,
                },
            );
        }
    }

    /// Mirror the active chat's slash commands into the shared cell the composer's
    /// completion provider reads. Called whenever the active chat changes or its
    /// commands arrive. Cheap; a no-op when the panel is closed.
    pub(crate) fn sync_command_completions(&self) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        *state.completion_commands.borrow_mut() = state.active().commands.clone();
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
        // Route to whichever chat owns the turn — not just the active one, and
        // across both surfaces (sidebar + agent tabs), so a background chat keeps
        // streaming while another is shown (M-S6).
        let grew_text = self.with_chat_mut(conversation_id, |chat| {
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
            // Keep following the newest text if the user is at the bottom.
            follow_if_at_bottom(chat);
            grew
        });
        let Some(grew_text) = grew_text else {
            return;
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
        let started = self
            .with_chat_mut(conversation_id, |chat| {
                if chat.revealing || chat.revealed >= chat.streaming_text_chars() {
                    return false;
                }
                chat.revealing = true;
                true
            })
            .unwrap_or(false);
        if !started {
            return;
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
        // Returns (advanced?, keep_going?) — `advanced` gates the repaint so a
        // no-op tick (chat gone, or already caught up) doesn't churn a frame.
        let (advanced, keep) = self
            .with_chat_mut(conversation_id, |chat| {
                let target = chat.streaming_text_chars();
                if chat.revealed >= target {
                    chat.revealing = false;
                    return (false, false);
                }
                let remaining = target - chat.revealed;
                let step = (remaining / REVEAL_DIVISOR).max(REVEAL_MIN_STEP);
                chat.revealed = (chat.revealed + step).min(target);
                let caught_up = chat.revealed >= target;
                if caught_up {
                    chat.revealing = false;
                }
                // Follow the growing text while the user is at the bottom.
                follow_if_at_bottom(chat);
                (true, !caught_up)
            })
            .unwrap_or((false, false));
        if advanced {
            cx.notify();
        }
        keep
    }

    pub(crate) fn on_ai_finished(
        &mut self,
        conversation_id: u64,
        usage: red_service::AiUsage,
        cx: &mut Context<Self>,
    ) {
        // A turn finishing on a chat the user isn't looking at is "unread" until
        // they switch to it (drives the history dot). The active chat is, by
        // definition, already in view.
        let active_id = self.assistant.as_ref().map(|s| s.active().conversation_id);
        let finished = self.with_chat_mut(conversation_id, |chat| {
            chat.streaming = false;
            chat.status = None;
            // A prompt can't outlive its turn; deny any still-open one on the backend.
            let stranded = chat.pending_permission.take().map(|p| p.request_id);
            if active_id != Some(chat.conversation_id) {
                chat.unread = true;
            }
            // Keep a non-empty reading; a turn that reports nothing (some
            // refusals / cancels) leaves the prior footer in place.
            if usage != red_service::AiUsage::default() {
                chat.last_usage = Some(usage);
            }
            // Persist the now-complete exchange so it survives a restart (M-S5).
            persist_chat(chat);
            follow_if_at_bottom(chat);
            stranded
        });
        if let Some(stranded) = finished {
            if let Some(request_id) = stranded {
                self.deny_stranded_permission(conversation_id, request_id);
            }
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
        let stranded = self.with_chat_mut(conversation_id, |chat| {
            chat.streaming = false;
            chat.status = None;
            chat.error = Some(message.into());
            // A prompt can't outlive its turn — drop any unanswered one, and deny it
            // on the backend so a parked agent decision sink isn't left blocking.
            chat.pending_permission.take().map(|p| p.request_id)
        });
        if let Some(stranded) = stranded {
            if let Some(request_id) = stranded {
                self.deny_stranded_permission(conversation_id, request_id);
            }
            cx.notify();
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
        if self
            .with_chat_mut(conversation_id, |chat| {
                chat.pending_permission = Some(PendingPermission {
                    request_id,
                    title: title.into(),
                    detail: detail.map(Into::into),
                });
            })
            .is_some()
        {
            cx.notify();
        }
    }

    /// A `generate_report` tool wrote a standalone HTML report (Feature C): open it
    /// in the system browser and note it on the owning chat. The file was produced
    /// service-side; the UI owns the OS hand-off, mirroring export.
    pub(crate) fn on_ai_report_ready(
        &mut self,
        conversation_id: u64,
        path: String,
        cx: &mut Context<Self>,
    ) {
        match crate::app::open_in_os(std::path::Path::new(&path)) {
            Ok(()) => {
                self.with_chat_mut(conversation_id, |chat| {
                    chat.status = Some("Report opened in your browser.".into());
                });
                self.notify(
                    flint::ToastVariant::Success,
                    "Opened report in your browser",
                    cx,
                );
            }
            Err(e) => {
                self.notify(
                    flint::ToastVariant::Error,
                    format!("Report saved to {path}, but couldn't open it: {e}"),
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// Answer the active chat's pending tool-permission prompt (its Allow/Deny
    /// buttons). The agent is blocked on this; denying is the safe default if it's
    /// dismissed.
    pub(crate) fn answer_permission(&mut self, allow: bool, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_mut() else {
            return;
        };
        let conversation_id = state.active().conversation_id;
        // Only consume the prompt once we know we can deliver the answer. If the
        // connection dropped while the buttons were on screen, the agent (and its
        // parked decision sink) is already gone — just clear the stale prompt rather
        // than `take()`-ing it and silently losing the click with nothing sent.
        let Phase::Connected(active) = &self.phase else {
            state.active_mut().pending_permission = None;
            cx.notify();
            return;
        };
        let session = active.session;
        let Some(pending) = state.active_mut().pending_permission.take() else {
            return;
        };
        self.service.send_to(
            session,
            red_service::Command::AiPermission {
                conversation_id,
                request_id: pending.request_id,
                allow,
            },
        );
        cx.notify();
    }

    /// Deny a permission prompt that's being torn down because its turn errored or
    /// finished while it was still on screen, so the backend resolves the parked
    /// agent decision sink instead of leaving the agent blocked on it. A no-op once
    /// disconnected (the sink is dropped → denied on teardown) or if the id was
    /// already resolved (the backend treats an unknown id as a no-op).
    fn deny_stranded_permission(&self, conversation_id: u64, request_id: u64) {
        if let Phase::Connected(active) = &self.phase {
            self.service.send_to(
                active.session,
                red_service::Command::AiPermission {
                    conversation_id,
                    request_id,
                    allow: false,
                },
            );
        }
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
        // What the user is looking at, so they can refer to "this tab" / "these
        // results". The tab name goes at any tier; the result's shape (counts +
        // column names) reflects query output, so it's withheld below `read`.
        let reads_allowed = matches!(
            self.ai_tier_effective(),
            red_core::AiTier::Read | red_core::AiTier::Write
        );
        let current_tab = active.active().map(|t| {
            let mut s = format!("\"{}\"", t.title);
            if reads_allowed {
                if let Some(grid) = t.result.as_ref() {
                    if let Some((rows, cols)) = grid.status_counts() {
                        let names: Vec<String> = (0..cols)
                            .filter_map(|c| grid.column_meta(c).map(|(name, _)| name))
                            .collect();
                        s.push_str(&format!(
                            " — showing a result of {rows} row(s) × {cols} column(s): {}",
                            names.join(", ")
                        ));
                    }
                }
            }
            s
        });
        red_service::AiContext {
            schema_summary: summarize_schema(&active.schema.schemas),
            current_tab,
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

    /// The composer's model + reasoning dropdowns (subscription path), to the left of
    /// Send. One `flint::Select` per advertised Model/Reasoning selector; empty (so
    /// Send sits alone) on the API-key path or before the agent's session opens.
    /// Dimmed and non-interactive while a turn streams.
    fn render_config_selectors(
        &self,
        state: &AssistantState,
        chat: &ChatSession,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let streaming = chat.streaming;
        let open = state.open_config.clone();
        let mut row = div().flex().items_center().gap_1p5().min_w(px(0.));
        let mut any = false;
        for cat in [
            red_service::AiConfigCategory::Model,
            red_service::AiConfigCategory::Reasoning,
        ] {
            let Some(opt) = chat
                .config_options
                .iter()
                .find(|o| o.category == cat && !o.choices.is_empty())
            else {
                continue;
            };
            any = true;
            let selected = opt
                .choices
                .iter()
                .position(|c| c.value == opt.current_value)
                .unwrap_or(usize::MAX);
            let is_open = !streaming && open.as_deref() == Some(opt.id.as_str());
            let mut select = Select::new(SharedString::from(format!("ai-config-{}", opt.id)))
                .selected(selected)
                .open(is_open)
                .placeholder("Default");
            for choice in &opt.choices {
                select = select.option(SharedString::from(choice.name.clone()));
            }
            if !streaming {
                let view = cx.entity();
                let id_toggle = opt.id.clone();
                select = select.on_toggle(move |_, cx| {
                    view.update(cx, |this, cx| {
                        if let Some(s) = this.assistant.as_mut() {
                            s.open_config = if s.open_config.as_deref() == Some(id_toggle.as_str())
                            {
                                None
                            } else {
                                Some(id_toggle.clone())
                            };
                            cx.notify();
                        }
                    });
                });
                let view = cx.entity();
                let id_select = opt.id.clone();
                let values: Vec<String> = opt.choices.iter().map(|c| c.value.clone()).collect();
                select = select.on_select(move |ix, _, cx| {
                    if let Some(value) = values.get(ix).cloned() {
                        let id = id_select.clone();
                        view.update(cx, |this, cx| this.change_config_option(id, value, cx));
                    }
                });
            }
            row = row.child(div().when(streaming, |d| d.opacity(0.5)).child(select));
        }
        if !any {
            return div().into_any_element();
        }
        row.into_any_element()
    }

    /// The assistant panel body, docked right of the workspace by the shell.
    pub(crate) fn render_assistant(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(state) = self.assistant.as_ref() else {
            return div().into_any_element();
        };
        let chat = state.active();
        // An ACP agent (Claude subscription, Codex, a local agent) owns its own auth
        // and bills its own way; the body hint reflects the active chat's backend.
        let is_subscription = self.agent_is_acp(&chat.provider);

        let header = self.render_assistant_header(state, &theme, cx);

        // Setup view: no agent usable yet (no API key, and no ACP agent configured).
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
                "Ask a question about the connected database. The agent can read the \
                 schema and run capped, read-only SELECTs to answer."
            };
            body = body.child(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text_muted)
                    .child(hint),
            );
            // Before the first message, the chat's agent can still be switched —
            // offer the picker when more than one agent is usable (M-S6).
            if let Some(picker) = self.render_agent_picker(chat, &theme, cx) {
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
                .bg(theme.accent)
                .cursor_pointer()
                .tooltip(flint::Tooltip::text(
                    "Send (Enter · Shift+Enter for a new line)",
                ))
                .hover(|s| s.opacity(0.9))
                .child(crate::icons::icon("send", theme.scale(15.), theme.bg_app))
                .on_click(cx.listener(|this, _, _, cx| this.submit_assistant(cx)))
                .into_any_element()
        };

        // A bordered, rounded composer card (Zed-style): the multiline input on top,
        // a slim toolbar row below with the model/reasoning selectors on the left and
        // the send/stop button on the right.
        let composer = div()
            .flex_shrink_0()
            .m_2()
            .flex()
            .flex_col()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg_input)
            .child(
                div()
                    .min_w(px(0.))
                    .h(px(64.))
                    .px_2p5()
                    .pt_1p5()
                    .child(state.input.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .pt_2()
                    .pb_1p5()
                    .child(self.render_config_selectors(state, chat, cx))
                    .child(action),
            );

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
        let agent_name = self.agent_name(&chat.provider);

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
                    "history",
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

        // Deletion lives only in the history sidebar (each row's trash) — the chat
        // view never deletes the conversation it's showing.
        let header_actions = div()
            .flex()
            .items_center()
            .gap_1()
            .when_some(list_btn, |row, l| row.child(l))
            .when_some(new_chat, |row, n| row.child(n));

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
                    .child(crate::icons::icon(
                        "sparkles",
                        theme.scale(14.),
                        theme.accent,
                    ))
                    .child(agent_name)
                    // A "writes" badge when this connection opted into the write tier
                    // (Feature B), so the user knows the agent can propose data changes
                    // (each one still gated by per-statement approval).
                    .when(self.ai_tier_effective() == red_core::AiTier::Write, |row| {
                        row.child(
                            div()
                                .id("ai-writes-badge")
                                .flex()
                                .items_center()
                                .gap_1()
                                .px_1p5()
                                .rounded(theme.radius_sm)
                                .bg(theme.yellow.opacity(0.12))
                                .text_size(theme.scale(10.))
                                .text_color(theme.yellow)
                                .child(crate::icons::icon("edit", theme.scale(10.), theme.yellow))
                                .child("writes")
                                .tooltip(flint::Tooltip::text(
                                    "This connection allows the agent to propose writes — \
                                         each one needs your approval.",
                                )),
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
            .bg(theme.accent)
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
                            .child("Add an Anthropic API key to use the agent."),
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
                    status: RowStatus::Draft,
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
            let mut subtitle = format!("{} · {turns} turns", self.agent_name(&c.provider));
            if c.streaming {
                subtitle.push_str(" · streaming");
            }
            let status = if c.streaming {
                RowStatus::Streaming
            } else if c.unread {
                RowStatus::Unread
            } else {
                RowStatus::Idle
            };
            rows.push(HistoryRow {
                key: RowKey::Open(c.conversation_id),
                open_index: Some(i),
                saved_index: None,
                title,
                subtitle,
                status,
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
            let label = self.agent_name(&conv.provider);
            rows.push(HistoryRow {
                key: RowKey::Saved(conv.stem.clone()),
                open_index: None,
                saved_index: Some(j),
                title: conv.title.clone(),
                subtitle: format!("{label} · {turns} turns"),
                status: RowStatus::Idle,
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
            .hover(|s| s.border_color(theme.accent).text_color(theme.accent))
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

        // Leading status dot (replaces the old provider glyph; the provider now
        // reads in the subtitle): draft is hollow, streaming pulses, an unseen
        // background reply is filled-accent, everything else is a quiet dot.
        el = el.child(status_dot(row.status, theme, cx.reduce_motion()));

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
                        // Don't let the click fall through to the row body (which
                        // would open the chat instead of starting the rename).
                        cx.stop_propagation();
                        this.begin_rename(key_rename.clone(), title.clone(), cx)
                    }),
                ),
            );
            let key_delete = row.key.clone();
            el = el.child(
                small_btn(format!("history-delete-{id_key}"), "trash", "Delete").on_click(
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.delete_conversation_row(key_delete.clone(), cx)
                    }),
                ),
            );
        }

        el.into_any_element()
    }

    /// The empty-chat agent picker (M-S6): one selectable chip per usable agent, to
    /// bind a new chat to an agent before its first message. Shown only when more
    /// than one agent is usable — a single agent needs no choice. The chips wrap, so
    /// a handful of agents lay out cleanly.
    fn render_agent_picker(
        &self,
        chat: &ChatSession,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.usable_agents.len() <= 1 {
            return None;
        }
        let current = chat.provider.clone();
        let chips =
            self.usable_agents.iter().map(|agent| {
                let on = agent.id == current;
                let id = agent.id.clone();
                div()
                    .id(SharedString::from(format!("ai-pick-{}", agent.id)))
                    .px_2()
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .border_1()
                    .text_size(theme.scale(11.))
                    .cursor_pointer()
                    .when(on, |s| {
                        s.border_color(theme.accent).text_color(theme.accent)
                    })
                    .when(!on, |s| {
                        s.border_color(theme.border).text_color(theme.text_muted)
                    })
                    .child(SharedString::from(agent.name.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_active_chat_provider(id.clone(), cx)
                    }))
            });
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(
                    div()
                        .text_size(theme.scale(10.5))
                        .text_color(theme.text_muted)
                        .child("Agent for this chat:"),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1p5()
                        .children(chips),
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
        let has_error = active
            .active()
            .and_then(|t| t.result.as_ref())
            .is_some_and(|r| r.error().is_some());

        let mut actions = Vec::new();
        if has_error {
            actions.push(QuickAction::ExplainError);
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
                    .hover(|s| s.border_color(theme.accent).text_color(theme.accent))
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
                base.bg(theme.accent)
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
                    .child(crate::icons::icon("lock", theme.scale(13.), theme.accent))
                    .child(format!("Allow the agent to run {}?", pending.title)),
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
            ChatRole::Assistant => ("Agent", theme.accent),
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
                // A settled bubble renders from its cached parse (frame-stable); the
                // live one still parses its revealed prefix fresh each tick — but
                // that's a single message, not the whole transcript.
                let md = if live {
                    crate::markdown::render(shown, theme)
                } else {
                    crate::markdown::render_blocks(&msg.markdown(), theme)
                };
                bubble = bubble.child(md);
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

        // SQL affordances for the first fenced SQL block in a *settled* assistant
        // turn (suppressed while still typing): insert it into the active editor, or
        // open it in a fresh query tab (a read-only SELECT runs there automatically).
        if !live && msg.role == ChatRole::Assistant {
            if let Some(sql) = msg.sql_block() {
                let sql = sql.to_string();
                let key = bubble_key(msg);
                let chip = |id: SharedString, glyph: &'static str, label: &'static str| {
                    div()
                        .id(id)
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
                        .hover(|s| s.border_color(theme.accent).text_color(theme.accent))
                        .child(crate::icons::icon(
                            glyph,
                            theme.scale(11.),
                            theme.text_muted,
                        ))
                        .child(label)
                };
                let insert_sql = sql.clone();
                bubble = bubble.child(
                    div()
                        .mt_1()
                        .flex()
                        .flex_wrap()
                        .gap_1p5()
                        .child(
                            chip(
                                SharedString::from(format!("ai-insert-{key}")),
                                "corner-down-left",
                                "Insert into editor",
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.ai_insert_sql(insert_sql.clone(), cx),
                            )),
                        )
                        .child(
                            chip(
                                SharedString::from(format!("ai-open-{key}")),
                                "table",
                                "Open in a query tab",
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.open_query_in_tab(sql.clone(), cx),
                            )),
                        ),
                );
            }
        }

        bubble.into_any_element()
    }
}
/// Keep the transcript pinned to the newest message *only while the user is already
/// at (or within a line of) the bottom* — so streaming text follows the view, but a
/// user who scrolled up to read history isn't yanked down. The offset/max are from
/// the last paint (the user's current position); `scroll_to_bottom` applies on the
/// next paint, after the new content has grown the transcript.
fn follow_if_at_bottom(chat: &ChatSession) {
    let offset = chat.scroll.offset().y;
    let max = chat.scroll.max_offset().y;
    // `offset` is ≤ 0 (0 at top, more negative further down); `max` ≥ 0 is the
    // bottom extent. Nothing to scroll yet (`max == 0`) counts as "at bottom".
    if max <= px(0.) || offset <= px(24.) - max {
        chat.scroll.scroll_to_bottom();
    }
}

/// Candidate slash-command names for the composer's completion popup, or empty when
/// the word under the cursor isn't a slash command. A slash command is a `/` at the
/// start of the input (or after whitespace) followed by the in-progress name; the
/// returned candidate is the bare name (the editor keeps the typed `/`). The word
/// boundary matches the editor's own (alphanumeric + `_`), so the accepted candidate
/// replaces exactly the typed name.
fn slash_candidates(
    commands: &[red_service::AiCommand],
    text: &str,
    cursor: usize,
) -> Vec<SharedString> {
    if commands.is_empty() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let cursor = cursor.min(bytes.len());
    // Walk back over the in-progress command name.
    let mut start = cursor;
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    // The char before the name must be `/`, and that `/` must open the input or
    // follow whitespace — so "and/or" or a file path never triggers the picker.
    if start == 0 || bytes[start - 1] != b'/' {
        return Vec::new();
    }
    let slash = start - 1;
    if slash > 0 && !bytes[slash - 1].is_ascii_whitespace() {
        return Vec::new();
    }
    let prefix = text[start..cursor].to_ascii_lowercase();
    commands
        .iter()
        .filter(|c| c.name.to_ascii_lowercase().starts_with(&prefix))
        .map(|c| SharedString::from(c.name.clone()))
        .collect()
}

/// Which config selectors a fresh session should switch to honor the central
/// defaults: for each Model/Reasoning option whose stored default is non-empty, an
/// advertised choice, and not already current, the `(config_id, value)` to apply.
/// Options without a stored default (or already on it) are left as the agent set them.
fn default_config_changes(
    options: &[red_service::AiConfigOption],
    model_default: &str,
    reasoning_default: &str,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for opt in options {
        let default = match opt.category {
            red_service::AiConfigCategory::Model => model_default,
            red_service::AiConfigCategory::Reasoning => reasoning_default,
            _ => continue,
        };
        if default.is_empty() || default == opt.current_value {
            continue;
        }
        if opt.choices.iter().any(|c| c.value == default) {
            out.push((opt.id.clone(), default.to_string()));
        }
    }
    out
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

/// A history row's leading status dot, sized to the old provider glyph's footprint
/// so the list never reflows as a chat changes state. A draft is a hollow ring; a
/// streaming chat pulses (resting solid under reduced motion); an unseen background
/// reply is a filled accent dot; an idle chat is a quiet muted dot.
fn status_dot(status: RowStatus, theme: &flint::Theme, reduce_motion: bool) -> AnyElement {
    let slot = div().flex().items_center().justify_center().size(px(13.));
    let dot = div().size(px(7.)).rounded_full();
    let inner = match status {
        RowStatus::Draft => dot.border_1().border_color(theme.text_muted),
        RowStatus::Idle => dot.bg(theme.text_muted),
        RowStatus::Unread => dot.bg(theme.accent),
        RowStatus::Streaming => {
            let dot = dot.bg(theme.accent);
            if reduce_motion {
                dot
            } else {
                return slot
                    .child(dot.with_animation(
                        "ai-history-streaming-dot",
                        Animation::new(Duration::from_millis(1100)).repeat(),
                        |dot, delta| {
                            let o = 0.2 + 0.8 * (0.5 + 0.5 * (delta * std::f32::consts::TAU).cos());
                            dot.opacity(o)
                        },
                    ))
                    .into_any_element();
            }
        }
    };
    slot.child(inner).into_any_element()
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

/// Expand a `/report …` composer shortcut into an explicit instruction so the agent
/// reads the data and calls `generate_report`. Returns `None` for a non-`/report`
/// message (sent verbatim). A bare `/report` still asks for a report of whatever's
/// in context; `/reporting` (no separator) is not matched.
fn expand_slash_report(message: &str) -> Option<String> {
    let rest = message.strip_prefix("/report")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let topic = rest.trim();
    let ask = if topic.is_empty() {
        "Create an HTML report for me.".to_string()
    } else {
        format!("Create an HTML report about: {topic}")
    };
    Some(format!(
        "{ask}\n\nRead the data you need with run_select, then call the generate_report tool with \
         the report written as HTML — a heading, a short summary, and the relevant table(s). Where \
         a visual helps, add interactive charts via the tool's `charts` argument (Chart.js config \
         objects) and reference them with <div data-red-chart=\"INDEX\"></div> placeholders. Open \
         it for me."
    ))
}

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
            "Agent"
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
    fn central_defaults_apply_only_when_valid_and_different() {
        let choice = |v: &str| red_service::AiConfigChoice {
            value: v.into(),
            name: v.into(),
            description: None,
        };
        let model = red_service::AiConfigOption {
            id: "model".into(),
            name: "Model".into(),
            category: red_service::AiConfigCategory::Model,
            current_value: "auto".into(),
            choices: vec![choice("auto"), choice("opus"), choice("haiku")],
        };
        let reasoning = red_service::AiConfigOption {
            id: "reasoning".into(),
            name: "Reasoning".into(),
            category: red_service::AiConfigCategory::Reasoning,
            current_value: "default".into(),
            choices: vec![choice("default"), choice("hard")],
        };
        let opts = vec![model, reasoning];

        // A valid, different model default applies; an empty reasoning default is left.
        assert_eq!(
            default_config_changes(&opts, "opus", ""),
            vec![("model".to_string(), "opus".to_string())]
        );
        // Both apply when both differ.
        assert_eq!(
            default_config_changes(&opts, "haiku", "hard"),
            vec![
                ("model".to_string(), "haiku".to_string()),
                ("reasoning".to_string(), "hard".to_string()),
            ]
        );
        // A default equal to the current pick is a no-op; an unknown value is ignored.
        assert!(default_config_changes(&opts, "auto", "nonexistent").is_empty());
        // No stored defaults → nothing to apply.
        assert!(default_config_changes(&opts, "", "").is_empty());
    }

    #[test]
    fn slash_picker_triggers_only_in_command_position() {
        let cmds = vec![
            red_service::AiCommand {
                name: "login".into(),
                description: "Sign in".into(),
            },
            red_service::AiCommand {
                name: "logout".into(),
                description: "Sign out".into(),
            },
            red_service::AiCommand {
                name: "clear".into(),
                description: "Reset".into(),
            },
        ];
        let names = |t: &str, c: usize| -> Vec<String> {
            slash_candidates(&cmds, t, c)
                .into_iter()
                .map(|s| s.to_string())
                .collect()
        };
        // A bare `/` offers everything; the prefix filters.
        assert_eq!(names("/", 1), vec!["login", "logout", "clear"]);
        assert_eq!(names("/lo", 3), vec!["login", "logout"]);
        assert_eq!(names("/cle", 4), vec!["clear"]);
        // After whitespace mid-message still counts as command position.
        assert_eq!(names("hi /lo", 6), vec!["login", "logout"]);
        // A slash glued to a preceding word (path, and/or) does not trigger.
        assert!(names("and/lo", 6).is_empty());
        // No match, and no commands → empty.
        assert!(names("/xyz", 4).is_empty());
        assert!(slash_candidates(&[], "/lo", 3).is_empty());
    }

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
        assert!(seed.contains("Agent: hello"));
        // Empty-text turns are skipped; thinking isn't seeded.
        assert!(!seed.contains("ignored"));
        // An all-empty transcript yields nothing to seed.
        assert!(render_transcript(&[]).is_none());
    }

    #[test]
    fn quick_action_prompt_is_nonempty() {
        assert!(!QuickAction::ExplainError.prompt().trim().is_empty());
    }

    #[test]
    fn chat_carries_its_agent_binding() {
        // The chat stores the agent id verbatim; a turn carries it to the backend,
        // which resolves the kind (the panel no longer maps it). Any id round-trips.
        for id in ["anthropic", "subscription", "codex", "local"] {
            let chat = ChatSession::new(0, id.to_string());
            assert_eq!(chat.provider, id);
        }
    }
}
