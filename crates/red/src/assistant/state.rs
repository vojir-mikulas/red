//! The assistant panel's behavior on [`AppState`]: opening/closing the panel, turn
//! dispatch, the conversation-history surface (switch / new / restore / rename /
//! delete + persistence), the streaming reveal ticker, and the event sinks driven
//! from `on_event` (`on_ai_*`). The view lives in `render`; the pure helpers these
//! lean on live in `text`.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use flint::prelude::*;
use flint::{CodeEditor, CodeEditorEvent, TextInput, TextInputEvent};
use gpui::{prelude::*, AsyncApp, Context, SharedString, WeakEntity, Window};

use crate::app::{ActiveConn, AppState, Phase};

use super::text::{
    default_config_changes, derive_title, expand_slash_report, render_transcript, report_theme,
    slash_candidates, summarize_schema,
};
use super::{
    AssistantState, ChatMessage, ChatRole, ChatSession, PendingPermission, QuickAction, Rename,
    RowKey,
};

/// Streaming reveal cadence: the assistant's answer types out at this tick rate
/// (≈40fps), decoupling the on-screen reveal from the uneven network bursts the
/// model's text actually arrives in (the ChatGPT-style steady stream).
const REVEAL_TICK: Duration = Duration::from_millis(24);
/// Reveal speed: each tick uncovers `remaining / DIVISOR` more characters (a
/// natural ease-out: fast when far behind, slowing as it catches up), but never
/// fewer than `MIN_STEP`, so a big backlog drains quickly and the tail still moves.
const REVEAL_DIVISOR: usize = 6;
const REVEAL_MIN_STEP: usize = 2;

impl AppState {
    /// Whether the AI assistant is enabled for the current context (M-S7): the
    /// active connection's `ai_enabled` override, falling back to the global
    /// `[ai] enabled`. `false` is a true kill switch: the panel can't be opened,
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
    /// Drives the "writes" safety badge; `Write` means the agent can propose
    /// data changes (each one still gated by per-statement approval).
    pub(crate) fn ai_tier_effective(&self) -> red_core::AiTier {
        let global = red_core::AiTier::parse(&self.settings.ai.tier);
        match &self.phase {
            Phase::Connected(active) => active.config.ai_tier.unwrap_or(global),
            _ => global,
        }
    }

    /// Whether the agent `id` runs over ACP (an external agent that owns its own
    /// auth: Claude subscription, Codex, a local agent). Resolved against the
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
            // can't be reopened; the action's owner is no longer in the focus path).
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
                        .placeholder("Message Claude Agent (/ for commands)")
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
                // The composer has no gutter markers, so this never fires.
                CodeEditorEvent::RunLine(_) => {}
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
            // Seed the per-agent config cache from disk, so the composer can draw the
            // model/reasoning dropdowns for a returning user *before* the first turn
            // opens a live session (Feature: preselect a model without chatting first).
            let provider_config_options = self
                .local_state
                .ai_config_all()
                .iter()
                .map(|(agent, opts)| (agent.clone(), super::text::from_stored(opts)))
                .collect();
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
                provider_config_options,
                subagent_collapse: std::collections::HashMap::new(),
                selection_group: Rc::new(std::cell::Cell::new(0)),
                next_selection_id: 1,
            });
            self.focus_assistant = true;
        }
        cx.notify();
    }

    /// The agent id a new chat starts on: the resolved default agent, falling back
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
    /// just a canned prompt; `ai_context` already folds in the live error / editor
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
                // It's no longer a draft, so drop any preserved prompt text.
                chat.draft.clear();
                // Sending is explicit: always jump to the new message + the reply.
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
        // Make the just-recorded user turn selectable right away (its text is final).
        self.build_chat_selectables(conversation_id, cx);
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

    /// Go to the panel's single draft: the one chat with nothing sent yet (the
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
    /// the open set bounded without deleting the saved file; it's still reopenable
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

    /// Set the active chat's provider, but only before its first message; the
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
                    let mut msg = ChatMessage::new(role, m.text.clone(), m.thinking.clone());
                    msg.activity = m.activity.clone();
                    msg.plan = m.plan.clone();
                    msg
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
        // Make the restored transcript's text selectable/copyable.
        self.build_chat_selectables(id, cx);
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
            TextInputEvent::Change
            | TextInputEvent::Tab
            | TextInputEvent::BackTab
            | TextInputEvent::Up
            | TextInputEvent::Down => {}
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
    /// conversation storage" affordance). Files there are plain JSON: readable,
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

    /// Open `sql` in a fresh query tab in the workspace. Only a genuine read-only
    /// query (see [`crate::sql::is_read_only`]) runs automatically; anything else
    /// (including a data-modifying CTE or a side-effecting function that merely *leads*
    /// with a read keyword) is loaded for the user to run by hand, so an agent (which
    /// reaches this via `open_query`) can never silently execute a write on a writable
    /// connection. Shared by the assistant's "Open in a query tab" chip and the tool.
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
        if crate::sql::is_read_only(&sql) {
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

    /// Persist the agent's `save_query` request to the saved-queries library, then
    /// toast the outcome so the user knows it landed (they reopen it with ⇧⌘O).
    pub(crate) fn on_ai_save_query(
        &mut self,
        _conversation_id: u64,
        name: String,
        description: Option<String>,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        let _ = match crate::queries::save(&name, description.as_deref(), &sql) {
            Ok(_) => self.notify(
                flint::ToastVariant::Success,
                format!("Saved query “{name}” to your library."),
                cx,
            ),
            Err(e) => self.notify(
                flint::ToastVariant::Error,
                format!("Couldn't save query “{name}”: {e}"),
                cx,
            ),
        };
        cx.notify();
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
    /// central default (settings) once per fresh session, so a new chat opens on the
    /// user's last-chosen model/reasoning without retroactively touching other chats.
    pub(crate) fn on_ai_config_options_available(
        &mut self,
        conversation_id: u64,
        options: Vec<red_service::AiConfigOption>,
        cx: &mut Context<Self>,
    ) {
        // Which agent advertised these? Cache the live set under it so a brand-new
        // chat on the same agent can render the selectors before opening its own
        // session, and persist it so the dropdowns show on the next launch too.
        let agent = self
            .assistant
            .as_ref()
            .and_then(|s| {
                s.chats
                    .iter()
                    .find(|c| c.conversation_id == conversation_id)
            })
            .map(|c| c.provider.clone());
        if let (Some(agent), Some(state)) = (agent.as_ref(), self.assistant.as_mut()) {
            state
                .provider_config_options
                .insert(agent.clone(), options.clone());
        }
        if let Some(agent) = agent.as_ref() {
            self.local_state
                .set_ai_config(agent, super::text::to_stored(&options));
        }
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
        let mode = self.settings.ai.subscription_mode.clone();
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
        // A fresh session opens *during* the first turn, so its config selectors
        // arrive mid-turn. Applying a selector change then is both pointless (the
        // running turn's model is already fixed) and rejected by the backend as
        // "a turn is already in progress". Defer until the turn ends; `on_ai_finished`
        // re-invokes this once streaming stops. `config_defaults_applied` stays false
        // so the deferred apply still fires.
        if chat.streaming {
            return;
        }
        chat.config_defaults_applied = true;
        let to_apply = default_config_changes(&chat.config_options, &model, &reasoning, &mode);
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
        let agent = state.active().provider.clone();
        let chat = state.active_mut();
        // Optimistic local update + remember the category for the settings write.
        let mut category = None;
        for opt in &mut chat.config_options {
            if opt.id == config_id {
                opt.current_value = value.clone();
                category = Some(opt.category);
            }
        }
        // Mirror the pick into the pre-session cache for this agent too, so a pick
        // made before the chat opens its session (rendered from the cache) shows
        // immediately and the category is still found when the chat has no live
        // options yet.
        if let Some(cached) = state.provider_config_options.get_mut(&agent) {
            for opt in cached {
                if opt.id == config_id {
                    opt.current_value = value.clone();
                    category = category.or(Some(opt.category));
                }
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
            Some(red_service::AiConfigCategory::Mode) => {
                self.settings.ai.subscription_mode = value.clone();
                self.save_settings();
            }
            _ => {}
        }
        self.send_set_config_option(conversation_id, config_id, value, cx);
        cx.notify();
    }

    /// Send the backend a config change for one conversation (no settings write; the
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

    /// Copy the whole active chat to the clipboard as Markdown, so it pastes into a
    /// notes app (Notion, Obsidian, …) as styled blocks — headings, lists, code, and
    /// GFM tables intact. The OS clipboard here carries plain text only (no rich/HTML
    /// flavor), but Markdown is what those apps re-style on paste, so this is the
    /// reliable "copy the styled stuff" path for a whole conversation.
    pub(crate) fn copy_conversation(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        let mut out = String::new();
        for msg in &state.active().messages {
            let text = msg.text.trim();
            if text.is_empty() {
                continue;
            }
            let who = match msg.role {
                ChatRole::User => "You",
                ChatRole::Assistant => "Agent",
            };
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            // Bold role label, then the turn verbatim (assistant turns are already
            // Markdown; user turns are plain text, which is valid Markdown too).
            out.push_str("**");
            out.push_str(who);
            out.push_str(":**\n\n");
            out.push_str(text);
        }
        if out.trim().is_empty() {
            self.notify(
                flint::ToastVariant::Info,
                "This chat has nothing to copy yet.",
                cx,
            );
            return;
        }
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(out));
        self.notify(
            flint::ToastVariant::Success,
            "Copied the chat as Markdown.",
            cx,
        );
        cx.notify();
    }

    /// Build the selectable, copyable text leaves for a chat's *settled* messages
    /// (Feature: highlight and copy transcript text). One [`flint::SelectableLabel`]
    /// per Markdown text leaf for an assistant turn, or a single plain label for a
    /// user turn. Idempotent: a message already built for the current theme is
    /// skipped, and an empty/streaming bubble is left alone (the live bubble renders
    /// as plain `StyledText`). Called when a turn settles and when a chat is restored.
    fn build_chat_selectables(&mut self, conversation_id: u64, cx: &mut Context<Self>) {
        let theme = cx.theme().clone();
        let theme_key = theme.text;
        let Some(state) = self.assistant.as_mut() else {
            return;
        };
        // All of a chat's leaves share one selection group so only one shows a
        // highlight at a time; each gets a unique id from the panel's counter.
        let group = state.selection_group.clone();
        let mut next_id = state.next_selection_id;
        {
            let Some(chat) = state
                .chats
                .iter_mut()
                .find(|c| c.conversation_id == conversation_id)
            else {
                return;
            };
            // The live (still-streaming) trailing assistant bubble isn't settled yet;
            // don't freeze selectables for it — it repaints as plain text until it ends.
            let last = chat.messages.len().saturating_sub(1);
            for (i, msg) in chat.messages.iter_mut().enumerate() {
                if msg.text.trim().is_empty() || msg.selectables_current(theme_key) {
                    continue;
                }
                if i == last && msg.role == ChatRole::Assistant && chat.streaming {
                    continue;
                }
                let leaves = match msg.role {
                    // A user turn is plain text; one label, color inherited from the
                    // parent (so it survives a theme switch without a rebuild).
                    ChatRole::User => {
                        let id = next_id;
                        next_id += 1;
                        vec![cx.new(|cx| {
                            flint::SelectableLabel::new(msg.text.clone(), cx)
                                .selection_group(group.clone(), id)
                        })]
                    }
                    // An assistant turn is Markdown; walk it the same way the transcript
                    // renders it, minting one selectable label per text leaf in order.
                    ChatRole::Assistant => {
                        let blocks = msg.markdown();
                        let mut leaves = Vec::new();
                        let _ = crate::markdown::render_blocks_with(
                            &blocks,
                            &theme,
                            &mut |text, runs| {
                                if !text.is_empty() {
                                    let id = next_id;
                                    next_id += 1;
                                    leaves.push(cx.new(|cx| {
                                        flint::SelectableLabel::new(text, cx)
                                            .with_runs(runs)
                                            .selection_group(group.clone(), id)
                                    }));
                                }
                                gpui::div().into_any_element()
                            },
                        );
                        leaves
                    }
                };
                msg.set_selectables(leaves, theme_key);
            }
        }
        // Persist the advanced counter so later builds keep minting fresh ids.
        if let Some(state) = self.assistant.as_mut() {
            state.next_selection_id = next_id;
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
        // Route to whichever chat owns the turn, not just the active one, and
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
                red_service::AiDelta::ActivityStarted {
                    id,
                    parent,
                    kind,
                    status,
                } => {
                    let node = red_core::ActivityNode {
                        id,
                        kind,
                        status,
                        detail: None,
                        children: Vec::new(),
                    };
                    let bubble = chat.assistant_bubble();
                    match parent
                        .as_deref()
                        .and_then(|p| find_activity_mut(&mut bubble.activity, p))
                    {
                        Some(parent_node) => parent_node.children.push(node),
                        None => bubble.activity.push(node),
                    }
                }
                red_service::AiDelta::ActivityUpdated { id, status, detail } => {
                    if let Some(node) =
                        find_activity_mut(&mut chat.assistant_bubble().activity, &id)
                    {
                        // `status` is `None` for a detail-only refresh (streamed progress).
                        if let Some(status) = status {
                            node.status = status;
                        }
                        if detail.is_some() {
                            node.detail = detail;
                        }
                    }
                }
                red_service::AiDelta::PlanUpdated { steps } => {
                    chat.assistant_bubble().plan = steps;
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
    /// whether the ticker should fire again (false once it's caught up; a new burst
    /// will restart it via `ensure_reveal_ticker`).
    fn tick_reveal(&mut self, conversation_id: u64, cx: &mut Context<Self>) -> bool {
        // Returns (advanced?, keep_going?); `advanced` gates the repaint so a
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
            // The turn is over: settle any still-running activity node (e.g. a
            // subagent the agent never sent a terminal update for before ending its
            // turn) so it stops showing a live "working" pulse.
            for m in &mut chat.messages {
                settle_running_nodes(&mut m.activity);
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
            // Apply any config defaults deferred because they arrived mid-turn (a
            // fresh session opens during the first turn); now streaming has stopped,
            // the set will land instead of being rejected as "turn in progress".
            self.apply_default_config(conversation_id, cx);
            // The answer text is final: build its selectable, copyable leaves.
            self.build_chat_selectables(conversation_id, cx);
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
            // Stop any live "working" pulse now the turn has ended.
            for m in &mut chat.messages {
                settle_running_nodes(&mut m.activity);
            }
            // A prompt can't outlive its turn: drop any unanswered one, and deny it
            // on the backend so a parked agent decision sink isn't left blocking.
            chat.pending_permission.take().map(|p| p.request_id)
        });
        if let Some(stranded) = stranded {
            if let Some(request_id) = stranded {
                self.deny_stranded_permission(conversation_id, request_id);
            }
            // Whatever answer arrived before the error is final: make it selectable.
            self.build_chat_selectables(conversation_id, cx);
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

    /// A `generate_report` tool wrote a standalone HTML report (Feature C): surface it
    /// as a card in the owning chat's transcript, with an "Open" button, rather than
    /// auto-opening it in the browser. The card is a `Report` activity node on the
    /// turn's bubble, so it persists with the conversation; the user opens it on demand
    /// via [`open_report`](Self::open_report).
    pub(crate) fn on_ai_report_ready(
        &mut self,
        conversation_id: u64,
        path: String,
        title: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let attached = self
            .with_chat_mut(conversation_id, |chat| {
                let bubble = chat.assistant_bubble();
                bubble.activity.push(red_core::ActivityNode {
                    id: format!("report-{path}"),
                    kind: red_core::ActivityKind::Report {
                        path: path.clone(),
                        title,
                    },
                    status: red_core::ActivityStatus::Ok,
                    detail: None,
                    children: Vec::new(),
                });
            })
            .is_some();
        // A report for a chat that's gone (evicted) can't be shown as a card; fall back
        // to opening it so the work isn't silently lost.
        if !attached {
            let _ = crate::app::open_in_os(std::path::Path::new(&path));
        }
        cx.notify();
    }

    /// Open a report card's HTML file in the system browser (the card's "Open"
    /// button). The file was written service-side; the UI owns the OS hand-off.
    pub(crate) fn open_report(&mut self, path: String, cx: &mut Context<Self>) {
        if let Err(e) = crate::app::open_in_os(std::path::Path::new(&path)) {
            self.notify(
                flint::ToastVariant::Error,
                format!("Couldn't open the report: {e}"),
                cx,
            );
        }
    }

    /// Answer the active chat's pending tool-permission prompt (its Allow/Deny
    /// buttons). The agent is blocked on this; denying is the safe default if it's
    /// dismissed.
    /// Toggle a subagent card between expanded and collapsed, pinning the user's
    /// choice by the subagent's activity id (overriding the status-based default).
    pub(crate) fn set_subagent_collapsed(
        &mut self,
        id: SharedString,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.assistant.as_mut() {
            state.subagent_collapse.insert(id, collapsed);
            cx.notify();
        }
    }

    pub(crate) fn answer_permission(&mut self, allow: bool, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_mut() else {
            return;
        };
        let conversation_id = state.active().conversation_id;
        // Only consume the prompt once we know we can deliver the answer. If the
        // connection dropped while the buttons were on screen, the agent (and its
        // parked decision sink) is already gone; just clear the stale prompt rather
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
                            ", showing a result of {rows} row(s) × {cols} column(s): {}",
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
            // Paint AI-generated reports in Red's active theme (Ayu, GitHub Dark, …).
            theme: Some(Box::new(report_theme(cx.theme()))),
            // Where generated reports are written (Settings → AI agent → Report folder).
            // Empty means "use the system temp dir", so don't send a path at all then.
            report_dir: {
                let dir = self.settings.ai.report_dir.trim();
                (!dir.is_empty()).then(|| std::path::PathBuf::from(dir))
            },
        }
    }
}

/// Keep the transcript pinned to the newest message *only while the user is already
/// at (or within a line of) the bottom*, so streaming text follows the view, but a
/// user who scrolled up to read history isn't yanked down. The offset/max are from
/// the last paint (the user's current position); `scroll_to_bottom` applies on the
/// next paint, after the new content has grown the transcript.
fn follow_if_at_bottom(chat: &ChatSession) {
    let offset = chat.scroll.offset().y;
    let max = chat.scroll.max_offset().y;
    // `offset` is ≤ 0 (0 at top, more negative further down); `max` ≥ 0 is the
    // bottom extent. Nothing to scroll yet (`max == 0`) counts as "at bottom".
    if max <= gpui::px(0.) || offset <= gpui::px(24.) - max {
        chat.scroll.scroll_to_bottom();
    }
}

/// Find an activity node by id anywhere in a timeline (depth-first), so a status
/// update resolves the right node whether it's top-level or nested under a
/// subagent. Ids are unique within a turn, so the first match is the one.
fn find_activity_mut<'a>(
    nodes: &'a mut [red_core::ActivityNode],
    id: &str,
) -> Option<&'a mut red_core::ActivityNode> {
    for node in nodes {
        if node.id == id {
            return Some(node);
        }
        if let Some(found) = find_activity_mut(&mut node.children, id) {
            return Some(found);
        }
    }
    None
}

/// Flip any still-`Running`/`Pending` activity node to `Ok` (recursively), used when
/// a turn ends: an unresolved node — e.g. a subagent the agent never sent a terminal
/// update for before ending its turn — would otherwise show a live "working" pulse
/// forever. `Ok` is the least-bad settle (it ran; we have no per-node failure signal,
/// and the turn-level error, if any, is surfaced separately).
fn settle_running_nodes(nodes: &mut [red_core::ActivityNode]) {
    use red_core::ActivityStatus::{Ok as StatusOk, Pending, Running};
    for node in nodes {
        if matches!(node.status, Running | Pending) {
            node.status = StatusOk;
        }
        settle_running_nodes(&mut node.children);
    }
}

/// Persist one chat to its flat file (one JSON per conversation), titled from its
/// first user message. Called after each finished turn and when a chat is closed. A
/// chat with no real assistant reply yet (only a pending/aborted user turn) isn't
/// saved; there's nothing worth keeping. Best-effort: a write failure is logged,
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
                activity: m.activity.clone(),
                plan: m.plan.clone(),
            })
            .collect(),
        path: Default::default(),
        stem: stem.clone(),
    };
    if let Err(e) = crate::conversations::save(&stem, &conv) {
        tracing::warn!("failed to persist conversation: {e}");
    }
}
