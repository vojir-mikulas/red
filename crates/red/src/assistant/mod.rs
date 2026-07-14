//! The AI assistant panel: a right-docked, grounded chat sidebar (the AI-assistant
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
//! carries its own transcript, streaming state, and **provider binding**, so one
//! chat can run on the Claude subscription (ACP) while another runs on the API-key
//! backend, live simultaneously. The composer/transcript show the *active* chat;
//! background chats keep streaming (events route by `conversation_id` to whichever
//! chat owns them) and a switcher lists them all.
//!
//! The module is split by concern: this file holds the data model; `state`
//! holds the [`crate::app::AppState`] behavior (turn dispatch, event sinks,
//! history/persistence); `render` holds the views; `text` holds the pure domain
//! helpers and their unit tests.

mod render;
mod state;
mod text;

use std::cell::RefCell;
use std::rc::Rc;

use flint::{CodeEditor, TextInput};
use gpui::{Entity, Pixels, Point, ScrollHandle, SharedString};

use text::extract_sql;

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
    pub(super) fn prompt(self) -> &'static str {
        match self {
            QuickAction::ExplainError => "Explain the error from my last query and how to fix it.",
        }
    }

    /// The chip's label.
    pub(super) fn label(self) -> &'static str {
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
    /// The turn's activity timeline (assistant only): tool calls, subagents, and
    /// proposed writes, in call order, each resolved by id as updates stream. Drawn
    /// inline above the answer text and persisted with the conversation so a
    /// reopened chat shows what the agent *did*, not just what it said.
    pub(crate) activity: Vec<red_core::ActivityNode>,
    /// The agent's plan checklist for this turn (assistant only), shown at the top
    /// of the bubble and ticked off as the agent works. Empty when the agent
    /// published no plan (always empty on the API-key path today).
    pub(crate) plan: Vec<red_core::PlanStep>,
    /// Frame-stable render artifacts (parsed Markdown + first SQL block), filled
    /// lazily and reused while `text` is unchanged; see [`ChatMessage::markdown`].
    cache: RefCell<MessageCache>,
    /// Selectable, copyable text leaves for a *settled* bubble: one
    /// [`flint::SelectableLabel`] per Markdown text leaf (paragraph / heading / list
    /// item / table cell) in document order, or the single label for a user turn.
    /// Empty while the turn streams (the live bubble uses plain, non-selectable
    /// `StyledText`); built once the turn settles or a saved chat is restored (see
    /// [`crate::app::AppState::build_chat_selectables`]). The transcript is repainted
    /// every frame, so these persist to hold selection state and avoid per-frame
    /// entity churn.
    selectables: Vec<Entity<flint::SelectableLabel>>,
    /// The theme (`theme.text`) the assistant leaves' run colors were baked for, so a
    /// theme switch rebuilds them rather than showing stale colors. `None` for a
    /// never-built or plain (color-inherited) message. See [`ChatMessage::selectables_for`].
    selectable_theme: Option<gpui::Hsla>,
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
    pub(super) fn new(role: ChatRole, text: String, thinking: String) -> Self {
        Self {
            role,
            text,
            thinking,
            activity: Vec::new(),
            plan: Vec::new(),
            cache: RefCell::new(MessageCache::default()),
            selectables: Vec::new(),
            selectable_theme: None,
        }
    }

    /// The prebuilt selectable leaves for this bubble, but only if they're current
    /// for `theme_key` (an assistant bubble bakes theme colors into its runs, so a
    /// theme switch invalidates them). `None` means "render plain, non-selectable"
    /// — nothing built yet, empty text, or a stale theme.
    pub(super) fn selectables_for(
        &self,
        theme_key: gpui::Hsla,
    ) -> Option<&[Entity<flint::SelectableLabel>]> {
        if self.selectables.is_empty() || self.selectable_theme != Some(theme_key) {
            return None;
        }
        Some(&self.selectables)
    }

    /// Whether this bubble's selectable leaves are already built for `theme_key`
    /// (so [`crate::app::AppState::build_chat_selectables`] can skip it).
    pub(super) fn selectables_current(&self, theme_key: gpui::Hsla) -> bool {
        !self.selectables.is_empty() && self.selectable_theme == Some(theme_key)
    }

    /// Store freshly-built selectable leaves and the theme they were built for.
    pub(super) fn set_selectables(
        &mut self,
        leaves: Vec<Entity<flint::SelectableLabel>>,
        theme_key: gpui::Hsla,
    ) {
        self.selectables = leaves;
        self.selectable_theme = Some(theme_key);
    }

    /// Drop this bubble's selectable leaves, freeing the live GPUI entity per
    /// Markdown leaf. Used to keep only a trailing window of bubbles selectable in a
    /// long chat; the bubble then repaints as plain, non-selectable text (the render
    /// path already falls back to that when there are no leaves). The frame cache
    /// (parsed Markdown) is deliberately kept — the transcript re-renders every
    /// bubble each frame, so dropping it would force a re-parse rather than save
    /// memory. No-op when nothing was held.
    pub(super) fn shed_selectables(&mut self) -> bool {
        if self.selectables.is_empty() {
            return false;
        }
        self.selectables = Vec::new();
        self.selectable_theme = None;
        true
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
    pub(super) fn markdown(&self) -> Rc<Vec<crate::markdown::Block>> {
        self.refresh_cache();
        self.cache.borrow().blocks.clone().expect("just refreshed")
    }

    /// The first fenced SQL block in this (settled) bubble, cached across frames.
    pub(super) fn sql_block(&self) -> Option<SharedString> {
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
    /// Which agent this chat runs on: the agent profile's id (`"subscription"`,
    /// `"anthropic"`, `"codex"`, …). Chosen at creation (defaulting to the resolved
    /// default agent) and persisted as the conversation's binding (M-S5); turns carry
    /// it so the right backend handles them (M-S6). Locked once the first message is
    /// sent. (Field name kept as `provider`; it's the serialized key saved chats
    /// already use.)
    pub(crate) provider: String,
    /// The chat's title, derived from its first user message; the saved file's
    /// display name. `None` until the first turn is sent.
    pub(crate) title: Option<String>,
    /// The backing file's stem once this chat has been saved (M-S5), so later turns
    /// overwrite the same file. `None` for a never-saved chat.
    pub(crate) file_stem: Option<String>,
    /// Unix seconds this chat was first saved, kept stable across re-saves.
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
    /// Whether a background chat finished a turn the user hasn't looked at yet;
    /// drives the history sidebar's unread dot. Set when a turn finishes on a chat
    /// that isn't the active one, cleared the moment it's switched to. In-memory
    /// only; a fresh session starts everything read.
    pub(crate) unread: bool,
    /// The agent's advertised slash commands (subscription path only), driving the
    /// composer's `/`-command picker. Populated by `AiCommandsAvailable` once the
    /// agent's session opens, so empty until this chat sends its first turn, and
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
    pub(super) fn streaming_text_chars(&self) -> usize {
        match self.messages.last() {
            Some(m) if m.role == ChatRole::Assistant => m.text.chars().count(),
            _ => 0,
        }
    }

    /// Whether this chat has nothing sent yet: the panel's single editable draft.
    pub(super) fn is_draft(&self) -> bool {
        self.messages.is_empty()
    }

    /// Whether this chat needs the user's attention while it isn't shown: a parked
    /// permission prompt the agent is blocked on. Drives the switcher's dot.
    pub(super) fn needs_attention(&self) -> bool {
        self.pending_permission.is_some()
    }

    /// Ensure the trailing bubble is an assistant bubble (deltas append to it).
    pub(super) fn assistant_bubble(&mut self) -> &mut ChatMessage {
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

/// Which conversation a history-sidebar row refers to: an open chat (by its
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
    pub(super) key: RowKey,
    pub(crate) input: Entity<TextInput>,
    #[allow(dead_code)]
    pub(super) sub: gpui::Subscription,
}

/// The lifecycle state a history row reflects through its leading dot, replacing
/// the provider glyph (which now lives in the subtitle text). See [`render::status_dot`].
#[derive(Clone, Copy, PartialEq)]
pub(super) enum RowStatus {
    /// The single never-sent chat: a hollow circle.
    Draft,
    /// A turn is streaming right now: a pulsing dot.
    Streaming,
    /// A background turn finished that the user hasn't switched to: a filled dot.
    Unread,
    /// Nothing pending: a quiet muted dot.
    Idle,
}

/// One flattened row of the merged history sidebar: an open chat (the draft, or a
/// sent one) or a saved-but-closed conversation. Built fresh each render.
pub(super) struct HistoryRow {
    pub(super) key: RowKey,
    /// Index into `chats` for an open row (drives switch); `None` for a saved one.
    pub(super) open_index: Option<usize>,
    /// Index into `loaded_conversations` for a saved row (drives restore).
    pub(super) saved_index: Option<usize>,
    pub(super) title: String,
    pub(super) subtitle: String,
    pub(super) status: RowStatus,
    pub(super) active: bool,
    pub(super) attention: bool,
    /// The single editable draft: no rename/delete affordances; named live.
    pub(super) draft: bool,
}

/// Which agent-menu the header has open: `Switch` re-binds the current draft to
/// the picked agent (the header's agent label dropdown); `New` starts a fresh chat
/// on it (the `+` button). Both anchor a `ContextMenu` at the click position.
#[derive(Clone, Copy)]
pub(crate) enum AgentMenuKind {
    Switch,
    New,
}

/// All the assistant panel's state. Present iff the panel is open.
pub(crate) struct AssistantState {
    /// The prompt box, a multiline composer. Enter sends a turn on the active
    /// chat; Shift+Enter inserts a newline (see Flint `CodeEditor::submit_on_enter`).
    pub(crate) input: Entity<CodeEditor>,
    /// The API-key box, shown in the setup view when no key is configured.
    pub(crate) key_input: Entity<TextInput>,
    /// The history sidebar's search box; filters the merged list by title.
    pub(crate) list_search: Entity<TextInput>,
    /// Submit listeners (prompt + key); held here so closing the panel drops them.
    #[allow(dead_code)]
    pub(super) sub: gpui::Subscription,
    #[allow(dead_code)]
    pub(super) key_sub: gpui::Subscription,
    /// Re-renders the sidebar as the search query changes.
    #[allow(dead_code)]
    pub(super) search_sub: gpui::Subscription,
    /// The open conversations (M-S6). Never empty while the panel is open.
    pub(crate) chats: Vec<ChatSession>,
    /// Index of the active chat in `chats`: the one the composer/transcript show.
    pub(crate) active: usize,
    /// Whether the history sidebar (open chats + saved conversations) is shown in
    /// place of the active transcript.
    pub(crate) show_list: bool,
    /// An in-progress inline title rename, if any.
    pub(crate) renaming: Option<Rename>,
    /// The active chat's slash commands, mirrored here so the composer's completion
    /// provider (a plain closure with no access to `AppState`) can read them. Kept
    /// in sync with the active chat by [`crate::app::AppState::sync_command_completions`].
    pub(crate) completion_commands: Rc<RefCell<Vec<red_service::AiCommand>>>,
    /// Which config selector's dropdown is currently open (its `config_id`), if any.
    /// `flint::Select` is stateless, so the open state lives here.
    pub(crate) open_config: Option<String>,
    /// The header's open agent menu (the agent-label switcher or the `+` new-chat
    /// picker) and where it's anchored, or `None` when closed.
    pub(crate) agent_menu: Option<(AgentMenuKind, Point<Pixels>)>,
    /// Each agent's most recent advertised selectors (model / reasoning / mode),
    /// keyed by agent id. An agent only advertises these once its session opens (on
    /// the first turn), so a brand-new chat has none yet; the composer renders the
    /// cache for the chat's agent as a provisional set so the dropdowns show *before*
    /// the chat sends its first turn. Seeded from `state.json` on open (so a returning
    /// user sees them immediately) and replaced by a chat's own live options once its
    /// session opens. Empty for an agent whose session has never run on this machine.
    pub(crate) provider_config_options:
        std::collections::HashMap<String, Vec<red_service::AiConfigOption>>,
    /// User overrides for subagent-card collapse, keyed by the subagent's activity
    /// id. Absent means the default (expanded while running, collapsed once done);
    /// present pins the user's explicit choice. In-memory only, panel-lifetime.
    pub(crate) subagent_collapse: std::collections::HashMap<SharedString, bool>,
    /// Shared "who owns the live selection" cell across every transcript
    /// [`flint::SelectableLabel`], so highlighting text in one block clears the
    /// highlight in the others — a single selection at a time across the whole chat.
    pub(crate) selection_group: flint::SelectionGroup,
    /// Monotonic id handed to each selectable leaf as it's built, unique within the
    /// panel so [`selection_group`](Self::selection_group) can tell them apart.
    pub(crate) next_selection_id: u64,
}

impl AssistantState {
    /// The active chat (the one shown). `chats` is never empty, so this can't fail.
    pub(super) fn active(&self) -> &ChatSession {
        &self.chats[self.active.min(self.chats.len() - 1)]
    }

    /// The active chat, mutably.
    pub(super) fn active_mut(&mut self) -> &mut ChatSession {
        let i = self.active.min(self.chats.len() - 1);
        &mut self.chats[i]
    }

    /// Find a chat by its conversation id (events route here, not just to active).
    pub(super) fn find_mut(&mut self, conversation_id: u64) -> Option<&mut ChatSession> {
        self.chats
            .iter_mut()
            .find(|c| c.conversation_id == conversation_id)
    }
}
