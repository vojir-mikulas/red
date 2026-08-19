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
use gpui::{App, AsyncApp, Context, SharedString, WeakEntity, Window, prelude::*};

use crate::app::{ActiveConn, AppState, Phase};

use super::text::{
    config_changes_needed, derive_title, desired_config, expand_slash_report, render_transcript,
    report_theme, slash_candidates, summarize_schema, to_stored,
};
use super::{
    AssistantState, ChatMessage, ChatRole, ChatSession, PendingPermission, PendingSandbox,
    QuickAction, Rename, RowKey,
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

/// How many trailing bubbles keep their selectable Markdown leaves resident. Each
/// leaf is a live GPUI entity, so an unbounded conversation would otherwise grow
/// thousands of them; older bubbles beyond this window shed theirs and repaint as
/// plain text. Sized so a normal chat stays fully selectable and only a very long
/// one sheds its distant history. See [`ChatMessage::shed_selectables`].
const SELECTABLE_BUBBLE_WINDOW: usize = 60;

impl AppState {
    /// Whether the AI assistant is enabled for the current context: the
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

    /// The AI access tier in effect for the current context: the active
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
    /// while the assistant is enabled for this connection.
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
            let conversation_id = red_service::ConversationId::new(self.next_conversation_id);
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
                        // Sized to what's been typed rather than to a fixed box:
                        // roomy enough at rest to draft in, and it grows with a
                        // longer prompt (wrapped rows count) before it starts
                        // scrolling — without letting the composer eat the
                        // transcript.
                        .rows(4..=8)
                        .edit_menu_labels(crate::editor::edit_menu_labels())
                        .a11y_label(crate::i18n::tr!("assistant.agent_prompt", "Agent prompt"))
                        .placeholder(crate::i18n::tr!(
                            "assistant.message_claude_agent_for_commands",
                            "Message Claude Agent (/ for commands)"
                        ))
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
                // The composer has no gutter markers and doesn't opt into
                // `emit_nav`, so neither of these fires.
                CodeEditorEvent::RunLine(_) | CodeEditorEvent::Up | CodeEditorEvent::Down => {}
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
                    .with_placeholder(crate::i18n::tr!(
                        "assistant.search_conversations",
                        "Search conversations…"
                    ))
            });
            // A Change on the search box re-renders the filtered list.
            let search_sub = cx.subscribe(&list_search, |this, _, e: &TextInputEvent, cx| {
                if matches!(e, TextInputEvent::Change)
                    && let Some(state) = this.assistant.as_ref()
                    && state.show_list
                {
                    cx.notify();
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
                _sub: sub,
                _key_sub: key_sub,
                _search_sub: search_sub,
                chats: vec![ChatSession::new(conversation_id, provider)],
                active: 0,
                show_list: false,
                drop_active: false,
                renaming: None,
                completion_commands,
                open_config: None,
                agent_menu: None,
                provider_config_options,
                subagent_collapse: std::collections::HashMap::new(),
                selection_group: Rc::new(std::cell::Cell::new(0)),
                next_selection_id: 1,
                highlighted_source: None,
            });
            self.focus_assistant = true;
        }
        cx.notify();
    }

    /// The agent id a new chat starts on: the last agent the user actually ran a
    /// chat on (so a fresh chat picks up where they left off — no settings detour),
    /// then the resolved default agent, then the first usable agent. Each candidate
    /// is skipped unless it's currently usable (e.g. an API agent with no key while
    /// an ACP agent is ready).
    /// The agent an out-of-panel request should run on: the open chat's agent when
    /// there is one, otherwise the same default a new chat would pick. Lets the
    /// confirm dialog's advisory review use "the user's agent" without needing a
    /// chat to be open.
    pub(crate) fn assistant_agent_id(&self) -> String {
        self.assistant
            .as_ref()
            .map(|state| state.active().provider.clone())
            .unwrap_or_else(|| self.default_ai_provider())
    }

    /// The schema context for the confirm dialog's advisory review, focused on the
    /// one table the statement targets.
    ///
    /// Deliberately *not* `summarize_schema`, which is only a list of
    /// object names. A reviewer given nothing but names cannot do the job it exists
    /// for: it can't tell an inverted predicate from a correct one without the
    /// columns, and it can't say what a `DROP` would strand without the foreign
    /// keys. Asked to judge from names alone it correctly answers "nothing to add"
    /// every time, which is exactly how this looked in practice.
    ///
    /// So this sends the two things the user cannot easily see and the model can
    /// actually reason from: the target's columns (when the tree has loaded them)
    /// and every foreign key touching it, in both directions. The connection-wide
    /// FK graph is loaded once at connect, so the inbound references are always
    /// available even for a table that was never expanded.
    pub(crate) fn review_schema_context(&self, table: Option<&str>, cx: &App) -> String {
        let Phase::Connected(active) = &self.phase else {
            return String::new();
        };
        // The bare table name, since the FK graph and the detail map are keyed by it
        // rather than by the qualified reference as written in the SQL.
        let Some(bare) = table.map(|t| {
            t.rsplit('.')
                .next()
                .unwrap_or(t)
                .trim_matches(['"', '`', '[', ']'].as_slice())
        }) else {
            return summarize_schema(&active.schema.read(cx).schemas);
        };

        let mut out = String::new();
        if let Some((_, detail)) = active
            .schema
            .read(cx)
            .details
            .iter()
            .find(|((_, name), _)| name.eq_ignore_ascii_case(bare))
        {
            out.push_str(&format!("Columns of {bare}:\n"));
            for c in &detail.columns {
                let ty = c.type_name.as_deref().unwrap_or("?");
                let pk = if c.primary_key { " PRIMARY KEY" } else { "" };
                let nn = if c.not_null { " NOT NULL" } else { "" };
                out.push_str(&format!("  {} {ty}{pk}{nn}\n", c.name));
            }
        }

        // Both directions matter and they mean different things: outbound keys say
        // what this table depends on, inbound keys say what breaks if it goes.
        let mut inbound = Vec::new();
        let mut outbound = Vec::new();
        for e in &active.schema.read(cx).fk_graph {
            let cols = |pairs: &[(String, String)]| {
                pairs
                    .iter()
                    .map(|(a, b)| format!("{a} -> {b}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            if e.to_table.eq_ignore_ascii_case(bare) {
                inbound.push(format!(
                    "  {} references it ({})",
                    e.from_table,
                    cols(&e.columns)
                ));
            } else if e.from_table.eq_ignore_ascii_case(bare) {
                outbound.push(format!(
                    "  it references {} ({})",
                    e.to_table,
                    cols(&e.columns)
                ));
            }
        }
        if !inbound.is_empty() {
            out.push_str(&format!("\nForeign keys pointing at {bare}:\n"));
            out.push_str(&inbound.join("\n"));
            out.push('\n');
        }
        if !outbound.is_empty() {
            out.push_str(&format!("\nForeign keys from {bare}:\n"));
            out.push_str(&outbound.join("\n"));
            out.push('\n');
        }
        // Nothing focused to say (an unexpanded table on a connection whose FK graph
        // failed to load): fall back to the catalog so the model has *something*.
        if out.is_empty() {
            return summarize_schema(&active.schema.read(cx).schemas);
        }
        out
    }

    fn default_ai_provider(&self) -> String {
        let usable = |id: &str| self.usable_agents.iter().any(|a| a.id == id);
        if let Some(last) = self.local_state.last_agent()
            && usable(last)
        {
            return last.to_string();
        }
        let default = self.settings.ai.resolved_default_agent();
        if usable(&default) {
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

    /// A one-tap context action: "Explain error" / "Optimize query". Each is
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
    pub(crate) fn send_turn(&mut self, message: String, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        let conversation_id = state.active().conversation_id;
        let agent = state.active().provider.clone();
        self.dispatch_turn(conversation_id, agent, message, cx);
    }

    /// The shared turn-dispatch core: record the user message on whichever chat owns
    /// `conversation_id` (sidebar *or* agent tab), then send `Command::AiTurn`. The
    /// chat's own agent binding decides which backend runs it, so concurrent
    /// chats on different agents each route correctly.
    fn dispatch_turn(
        &mut self,
        conversation_id: red_service::ConversationId,
        agent: String,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if message.trim().is_empty() {
            return;
        }
        // Re-read the knowledge file first: it shapes every answer this turn, and
        // the user may have edited it in another editor since the last one.
        self.refresh_knowledge();
        let (session, mut context) = {
            let Phase::Connected(active) = &self.phase else {
                return;
            };
            (active.session, self.ai_context(active, cx))
        };
        // Read the chat's mode while recording the turn, so the command carries what
        // this conversation was created with rather than a global default.
        let mut sandbox_mode = false;
        // Taken off the chat as the turn is recorded: staged files belong to the
        // draft, and the draft is what is being sent.
        let mut staged: Vec<super::attach::Attachment> = Vec::new();
        let mut pointed_at: Vec<super::refs::ContextRef> = Vec::new();
        let sent = self
            .with_chat_mut(conversation_id, |chat| {
                if chat.streaming {
                    return false;
                }
                sandbox_mode = chat.sandbox;
                staged = std::mem::take(&mut chat.attachments);
                chat.attach_error = None;
                pointed_at = std::mem::take(&mut chat.references);
                // A reopened chat seeds its prior transcript into this one turn so
                // the model resumes coherently despite a fresh session.
                context.prior_transcript = chat.pending_seed.take();
                // Title the chat from its first user message (used as the saved name).
                if chat.title.is_none() {
                    chat.title = Some(derive_title(&message));
                }
                let mut turn = ChatMessage::new(ChatRole::User, message.clone(), String::new());
                turn.attachments = staged.iter().map(stored_attachment).collect();
                chat.messages.push(turn);
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
        // Resolved here rather than at drop time: a tab's SQL can change between
        // the two, and what the model should get is what is there now.
        context.references = self.resolve_references(&pointed_at, cx);
        let session_config = self.session_config_for(conversation_id, &agent);
        let command = move |attachments| red_service::Command::AiTurn {
            sandbox: sandbox_mode,
            conversation_id,
            agent,
            message,
            attachments,
            context,
            session_config,
        };
        if staged.is_empty() {
            self.service.send_to(session, command(Vec::new()));
        } else {
            // Reading a 20 MB PDF on the UI thread would drop frames, so the turn
            // is sent once the bytes land. The user message is already on screen
            // and the chat already reads as streaming, which is honest: the turn
            // *has* started, it is just waiting on a disk read.
            cx.spawn(
                async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                    let read = cx
                        .background_executor()
                        .spawn(async move { super::attach::read_all(&staged) })
                        .await;
                    let _ = this.update(cx, |this, cx| match read {
                        Ok(attachments) => this.service.send_to(session, command(attachments)),
                        // Nothing was sent, so nothing will come back to settle the
                        // turn: settle it here, with the reason.
                        Err(why) => {
                            this.with_chat_mut(conversation_id, |chat| {
                                chat.streaming = false;
                                chat.error = Some(why.into());
                            });
                            cx.notify();
                        }
                    });
                },
            )
            .detach();
        }
        // Make the just-recorded user turn selectable right away (its text is final).
        self.build_chat_selectables(conversation_id, cx);
        cx.notify();
    }

    /// What the agent's session should open on, sent with the turn that starts it:
    /// the model / thinking / mode this agent's composer is showing and any switch
    /// (fast mode) the user has flipped, so a pick made *before* the first message
    /// is honoured by that message rather than by the one after it.
    ///
    /// Empty once the session is up (the composer sets config directly from then on)
    /// and on the API-key path, which has no session config. Empty too before this
    /// agent has ever advertised its controls: RED can't name an option it has never
    /// been told about, which is exactly what the composer's inert placeholders say.
    fn session_config_for(
        &self,
        conversation_id: red_service::ConversationId,
        agent: &str,
    ) -> Vec<red_service::AiConfigChange> {
        if !self.agent_is_acp(agent) {
            return Vec::new();
        }
        let Some(state) = self.assistant.as_ref() else {
            return Vec::new();
        };
        let live_session = state
            .chats
            .iter()
            .find(|c| c.conversation_id == conversation_id)
            .is_some_and(|c| !c.config_options.is_empty());
        if live_session {
            return Vec::new();
        }
        let Some(cached) = state.provider_config_options.get(agent) else {
            return Vec::new();
        };
        desired_config(
            cached,
            &self.settings.ai.subscription_model,
            &self.settings.ai.subscription_reasoning,
            &self.settings.ai.subscription_mode,
            self.local_state.ai_switches(agent),
        )
    }

    /// Stage `paths` on the active chat, refusing what cannot be sent.
    ///
    /// The one entry point for both the picker and an OS drop, which is also the
    /// security boundary: a path only ever arrives from the user, never from the
    /// model.
    pub(crate) fn attach_paths(&mut self, paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        let Some(conversation_id) = self.assistant.as_ref().map(|s| s.active().conversation_id)
        else {
            return;
        };
        let mut refused: Vec<String> = Vec::new();
        self.with_chat_mut(conversation_id, |chat| {
            for path in paths {
                if chat.attachments.len() >= super::attach::MAX_ATTACHMENTS {
                    refused.push(format!(
                        "A turn can carry {} files; the rest were not attached.",
                        super::attach::MAX_ATTACHMENTS
                    ));
                    break;
                }
                match super::attach::classify(&path) {
                    // Attaching the same file twice is a slip, not a request for
                    // two copies of it in the prompt.
                    Ok(a) if chat.attachments.iter().any(|x| x.path == a.path) => {}
                    Ok(a) => chat.attachments.push(a),
                    Err(why) => refused.push(why),
                }
            }
            chat.attach_error = (!refused.is_empty()).then(|| refused.join(" ").into());
        });
        cx.notify();
    }

    /// Remove one staged attachment from the active chat (the chip's ✕).
    pub(crate) fn remove_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(conversation_id) = self.assistant.as_ref().map(|s| s.active().conversation_id)
        else {
            return;
        };
        self.with_chat_mut(conversation_id, |chat| {
            if index < chat.attachments.len() {
                chat.attachments.remove(index);
            }
            chat.attach_error = None;
        });
        cx.notify();
    }

    /// Hand a staged CSV/JSON attachment to RED's own import pipeline instead of
    /// to the model.
    ///
    /// The better answer for tabular data past a certain size is a table the
    /// agent can query, so this is a doorway into a workflow RED already has
    /// rather than an AI feature. The mapping and the confirm are the existing
    /// ones; only the file's origin differs.
    pub(crate) fn import_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        let path = self
            .assistant
            .as_ref()
            .and_then(|s| s.active().attachments.get(index))
            .map(|a| a.path.clone());
        let Some(path) = path else { return };
        // The pipeline maps a file's columns onto a table's, so it needs a table:
        // the one the user is looking at.
        let target = match &self.phase {
            crate::app::Phase::Connected(a) => {
                a.active_result().and_then(|g| g.read(cx).import_target())
            }
            _ => None,
        };
        let Some((target, columns)) = target else {
            self.notify(
                flint::ToastVariant::Info,
                "Open the table you want to import into, then try again",
                cx,
            );
            return;
        };
        self.begin_import_peek(path, target, columns);
    }

    /// Turn the chips into the wire form, resolving the ones that name live UI
    /// against what is on screen **now**.
    ///
    /// A tab whose SQL changed since it was dropped sends the current SQL; a tab
    /// that has since been closed drops out rather than sending a stale copy of
    /// something the user can no longer see.
    fn resolve_references(
        &self,
        refs: &[super::refs::ContextRef],
        cx: &Context<Self>,
    ) -> Vec<red_service::ContextRefSpec> {
        use super::refs::ContextRef;
        let gutter = self.gutter();
        let crate::app::Phase::Connected(active) = &self.phase else {
            // Structure references still make sense to a service that has a
            // session; the tab-bound ones cannot resolve without one.
            return refs.iter().filter_map(ContextRef::static_spec).collect();
        };
        refs.iter()
            .filter_map(|r| match r {
                other if other.static_spec().is_some() => other.static_spec(),
                ContextRef::Tab { index, .. } => {
                    let tab = active.tabs.get(*index)?;
                    let sql = tab.editor.read(cx).content().to_string();
                    (!sql.trim().is_empty()).then(|| red_service::ContextRefSpec::Sql {
                        label: format!("Tab \"{}\"", tab.title),
                        sql,
                    })
                }
                ContextRef::Rows { index } => {
                    let tab = active.tabs.get(*index)?;
                    let text = tab.result.as_ref()?.read(cx).selection_text(gutter)?;
                    Some(red_service::ContextRefSpec::Rows {
                        label: format!("Selected rows in \"{}\"", tab.title),
                        text,
                    })
                }
                // Every arm above is either static or handled; a `_` here would
                // silently swallow a reference kind added later.
                ContextRef::Table { .. }
                | ContextRef::Column { .. }
                | ContextRef::Schema { .. } => None,
            })
            .collect()
    }

    /// Point the agent at the tab at `index`, naming the chip after it.
    pub(crate) fn reference_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let title = match &self.phase {
            crate::app::Phase::Connected(a) => a.tabs.get(index).map(|t| t.title.clone()),
            _ => None,
        };
        let Some(title) = title else { return };
        self.add_reference(super::refs::ContextRef::Tab { index, title }, cx);
    }

    /// Point the agent at whatever is selected in the active result grid (the
    /// grid's "Ask AI about these rows").
    pub(crate) fn reference_selected_rows(&mut self, cx: &mut Context<Self>) {
        let index = match &self.phase {
            crate::app::Phase::Connected(a) => {
                use crate::app::TabWorkspace as _;
                a.focused_tab_index()
            }
            _ => None,
        };
        let Some(index) = index else { return };
        self.add_reference(super::refs::ContextRef::Rows { index }, cx);
    }

    /// Stage a reference on the active chat (a drop, or "Ask AI about this").
    ///
    /// A `Rows` reference on a connection whose tier withholds row data is
    /// refused here, with the reason, rather than accepted and silently emptied
    /// at send.
    pub(crate) fn add_reference(
        &mut self,
        reference: super::refs::ContextRef,
        cx: &mut Context<Self>,
    ) {
        let reads_allowed = matches!(
            self.ai_tier_effective(),
            red_core::AiTier::Read | red_core::AiTier::Write
        );
        if reference.is_data() && !reads_allowed {
            self.notify(
                flint::ToastVariant::Info,
                "This connection's agent cannot read row data (raise its access tier in Settings)",
                cx,
            );
            return;
        }
        // A reference has to land somewhere the user can see; a chip staged into a
        // closed panel is a change with no feedback.
        let Some(conversation_id) = self.assistant.as_ref().map(|s| s.active().conversation_id)
        else {
            self.notify(
                flint::ToastVariant::Info,
                "Open the assistant panel first",
                cx,
            );
            return;
        };
        self.with_chat_mut(conversation_id, |chat| {
            // Pointing at the same thing twice is a slip, not a request for two
            // copies of it in the prompt.
            if !chat.references.iter().any(|r| r.same_target(&reference)) {
                chat.references.push(reference);
            }
        });
        cx.notify();
    }

    /// Remove one staged reference from the active chat (the chip's ✕).
    pub(crate) fn remove_reference(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(conversation_id) = self.assistant.as_ref().map(|s| s.active().conversation_id)
        else {
            return;
        };
        self.with_chat_mut(conversation_id, |chat| {
            if index < chat.references.len() {
                chat.references.remove(index);
            }
        });
        cx.notify();
    }

    /// Set (and repaint) the panel's drop highlight, skipping the notify when
    /// nothing changed — `on_drag_move` fires on every pointer move.
    pub(crate) fn set_assistant_drop_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if let Some(state) = self.assistant.as_mut()
            && state.drop_active != active
        {
            state.drop_active = active;
            cx.notify();
        }
    }

    /// Open the OS file picker and stage whatever comes back (the composer's `+`).
    pub(crate) fn pick_attachments(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let Ok(Ok(Some(paths))) = paths.await else {
                    return;
                };
                let _ = this.update(cx, |this, cx| this.attach_paths(paths, cx));
            },
        )
        .detach();
    }

    /// Run `f` against the [`ChatSession`] that owns `conversation_id` (events route
    /// here, not just to the active chat). Returns `f`'s result, or `None` if no
    /// chat matches.
    fn with_chat_mut<R>(
        &mut self,
        conversation_id: red_service::ConversationId,
        f: impl FnOnce(&mut ChatSession) -> R,
    ) -> Option<R> {
        self.assistant
            .as_mut()
            .and_then(|state| state.find_mut(conversation_id))
            .map(f)
    }

    // --- conversation history ---------------------------------------

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
    /// than spawning duplicates, so "new chat" always lands on the same draft. Binds
    /// a freshly-created draft to the new-chat default (the last-used agent); an
    /// existing draft keeps its agent — switch it via the composer's agent dropdown.
    pub(crate) fn new_chat(&mut self, cx: &mut Context<Self>) {
        let provider = self.default_ai_provider();
        self.go_to_draft(provider, false, cx);
    }

    /// Go to the draft on a *specific* agent (the "New chat with \<agent\>" entry).
    /// Rebinds an existing draft to `provider` too — it has nothing sent, so the
    /// binding isn't locked — and records it as the new-chat default.
    pub(crate) fn new_chat_with(&mut self, provider: String, cx: &mut Context<Self>) {
        self.go_to_draft(provider, true, cx);
    }

    /// Shared "go to the single draft" core. `rebind` re-points an existing draft at
    /// `provider` (the explicit "New chat with …" path); the plain "+" leaves an
    /// existing draft on whatever agent it was already on. Whichever agent the draft
    /// ends up bound to becomes the remembered new-chat default.
    fn go_to_draft(&mut self, provider: String, rebind: bool, cx: &mut Context<Self>) {
        self.stash_active_draft(cx);
        let id = red_service::ConversationId::new(self.next_conversation_id);
        let acp = self.agent_is_acp(&provider);
        let existing = self
            .assistant
            .as_ref()
            .and_then(|s| s.chats.iter().position(|c| c.is_draft()));
        let mut created = false;
        let mut bound: Option<String> = None;
        if let Some(state) = self.assistant.as_mut() {
            let idx = match existing {
                Some(i) => {
                    if rebind {
                        state.chats[i].provider = provider.clone();
                        // Review-transaction mode belongs to the API-key path (the
                        // subscription agent gets no write tools), so a draft moved
                        // onto an ACP agent must not keep claiming it in the header.
                        if acp {
                            state.chats[i].sandbox = false;
                        }
                        bound = Some(provider.clone());
                    }
                    i
                }
                None => {
                    bound = Some(provider.clone());
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
        if let Some(agent) = bound {
            self.local_state.set_last_agent(&agent);
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
    pub(crate) fn close_chat(
        &mut self,
        conversation_id: red_service::ConversationId,
        cx: &mut Context<Self>,
    ) {
        // Tell the backend to drop this conversation's history/agent so its
        // state doesn't linger for the whole session. Session-less: it's keyed by
        // conversation_id on the shared AI state, and works even while disconnected.
        self.service
            .send_global(red_service::Command::AiForget { conversation_id });
        // Mint a replacement id up front to avoid borrowing `self` twice.
        let replacement_id = red_service::ConversationId::new(self.next_conversation_id);
        let replacement_provider = self.default_ai_provider();
        // A chat closed with an Allow/Deny prompt on screen has to answer it, the way
        // the finished and errored paths do. Closing without an answer strands the
        // agent's decision sink, and enough of those in one run exhaust the pending
        // cap — after which every later approval is auto-denied with no prompt at all.
        let stranded = self
            .assistant
            .as_mut()
            .and_then(|s| {
                s.chats
                    .iter_mut()
                    .find(|c| c.conversation_id == conversation_id)
            })
            .and_then(|c| c.pending_permission.take())
            .map(|p| p.request_id);
        if let Some(request_id) = stranded {
            self.deny_stranded_permission(conversation_id, request_id);
        }
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
    /// it). Drives the empty-chat provider picker.
    pub(crate) fn set_active_chat_provider(&mut self, provider: String, cx: &mut Context<Self>) {
        let mut picked = None;
        let acp = self.agent_is_acp(&provider);
        if let Some(state) = self.assistant.as_mut() {
            let chat = state.active_mut();
            if chat.messages.is_empty() {
                chat.provider = provider.clone();
                // A subscription agent gets no write tools, so it has nothing to hold
                // in a review transaction; drop a mode the previous agent could honour.
                if acp {
                    chat.sandbox = false;
                }
                picked = Some(provider);
            }
        }
        // Remember the explicit choice as the new-chat default, so the next chat
        // starts on it too.
        if let Some(agent) = picked {
            self.local_state.set_last_agent(&agent);
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
        if let Some(state) = self.assistant.as_ref()
            && let Some(i) = state
                .chats
                .iter()
                .position(|c| c.file_stem.as_deref() == Some(conv.stem.as_str()))
        {
            self.switch_chat(i, cx);
            return;
        }
        self.stash_active_draft(cx);
        let id = red_service::ConversationId::new(self.next_conversation_id);
        self.next_conversation_id += 1;
        let seed = render_transcript(&conv.messages);
        if let Some(state) = self.assistant.as_mut() {
            let mut chat = ChatSession::new(id, conv.provider.clone());
            chat.sandbox = conv.sandbox;
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
                    msg.attachments = m.attachments.clone();
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
            if let Some(state) = self.assistant.as_mut()
                && let Some(chat) = state.find_mut(id)
            {
                chat.file_stem = None;
                chat.messages.clear();
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
            state.renaming = Some(Rename {
                key,
                input,
                _sub: sub,
            });
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
                    if let Some(state) = self.assistant.as_mut()
                        && let Some(chat) = state.find_mut(*id)
                    {
                        chat.title = Some(title.clone());
                        // Rewrite the saved file's title if it's been saved.
                        if chat.file_stem.is_some() {
                            persist_chat(chat);
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
        if let Phase::Connected(active) = &self.phase
            && let Some(tab) = active.active()
        {
            tab.editor.update(cx, |e, cx| e.set_content(sql, cx));
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
        if let Phase::Connected(active) = &self.phase
            && let Some(tab) = active.active()
        {
            tab.editor
                .update(cx, |e, cx| e.set_content(sql.clone(), cx));
        }
        if crate::sql::is_read_only(&sql, self.active_dialect()) {
            self.run_editor_query(cx);
        }
        cx.notify();
    }

    /// The agent's `open_query` tool fired: open the SQL in a new query tab.
    pub(crate) fn on_ai_open_query(
        &mut self,
        _conversation_id: red_service::ConversationId,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        self.open_query_in_tab(sql, cx);
    }

    /// Persist the agent's `save_query` request to the saved-queries library, then
    /// toast the outcome so the user knows it landed (they reopen it with ⇧⌘O).
    pub(crate) fn on_ai_save_query(
        &mut self,
        _conversation_id: red_service::ConversationId,
        name: String,
        description: Option<String>,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        let _ = match red_config::queries::save(&name, description.as_deref(), &sql) {
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

    /// Store the agent's advertised slash commands on their chat. Refreshes
    /// the composer's command mirror if it's the active chat, so `/` offers them.
    pub(crate) fn on_ai_commands_available(
        &mut self,
        conversation_id: red_service::ConversationId,
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
        conversation_id: red_service::ConversationId,
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

    /// Apply the central defaults (model/reasoning/mode from settings, plus any
    /// switch the user has explicitly flipped) to a chat's fresh session, once. For
    /// each control whose wanted value is advertised and differs from the agent's
    /// current pick, send a set so the new chat lands on the user's last choice.
    /// Guarded by `config_defaults_applied` so a later `ConfigOptionUpdate` doesn't
    /// re-apply over a mid-chat manual change.
    ///
    /// This is the *catch-up* path. The session's opening state is set before the
    /// first prompt instead (`Command::AiTurn::session_config`), so by the time this
    /// runs there is usually nothing left to change.
    fn apply_default_config(
        &mut self,
        conversation_id: red_service::ConversationId,
        cx: &mut Context<Self>,
    ) {
        let model = self.settings.ai.subscription_model.clone();
        let reasoning = self.settings.ai.subscription_reasoning.clone();
        let mode = self.settings.ai.subscription_mode.clone();
        let agent = self.assistant.as_ref().and_then(|s| {
            s.chats
                .iter()
                .find(|c| c.conversation_id == conversation_id)
                .map(|c| c.provider.clone())
        });
        let switches = agent
            .as_deref()
            .and_then(|a| self.local_state.ai_switches(a))
            .cloned();
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
        let to_apply = config_changes_needed(
            &chat.config_options,
            &model,
            &reasoning,
            &mode,
            switches.as_ref(),
        );
        for change in to_apply {
            self.send_set_config_option(
                conversation_id,
                change.config_id,
                change.value,
                change.boolean,
                cx,
            );
        }
    }

    /// The composer changed a config control (a dropdown pick or a switch flip):
    /// optimistically reflect it on the chat, persist the choice as the central
    /// default for future chats (last choice wins; existing chats untouched), and
    /// tell the backend to apply it to this session.
    ///
    /// A pick made on a draft has no live session to change — the agent only starts
    /// with the first turn — so persisting is what makes it real: it rides out with
    /// that turn as `session_config` and is applied before the prompt. Switches are
    /// remembered per agent rather than as one global default, and only once the
    /// user has actually flipped one; an untouched switch is left to the agent.
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
        // Optimistic local update + remember the category and kind for the
        // settings write / the wire encoding.
        let mut category = None;
        let mut boolean = false;
        for opt in &mut chat.config_options {
            if opt.id == config_id {
                opt.current_value = value.clone();
                category = Some(opt.category);
                boolean = opt.boolean;
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
                    boolean |= opt.boolean;
                }
            }
        }
        // Keep the on-disk cache in step with the pick, so the control redraws in the
        // state the user left it in on the next launch rather than snapping back to
        // whatever the agent last advertised.
        let cached = state
            .provider_config_options
            .get(&agent)
            .map(|opts| to_stored(opts));
        if let Some(cached) = cached {
            self.local_state.set_ai_config(&agent, cached);
        }
        // Persist as the default for new chats (not retroactive): selectors globally,
        // switches per agent (they're that agent's own control, and only an explicit
        // flip is ever re-asserted).
        if boolean {
            self.local_state
                .set_ai_switch(&agent, &config_id, value == "true");
        }
        match category.filter(|_| !boolean) {
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
        // These three write and save without a full effects pass, so they have to
        // republish themselves; see `Settings::publish`.
        self.settings.publish(cx);
        self.send_set_config_option(conversation_id, config_id, value, boolean, cx);
        cx.notify();
    }

    /// Send the backend a config change for one conversation (no settings write; the
    /// callers decide whether this is a user choice or a default being applied).
    fn send_set_config_option(
        &mut self,
        conversation_id: red_service::ConversationId,
        config_id: String,
        value: String,
        boolean: bool,
        _cx: &mut Context<Self>,
    ) {
        if let Phase::Connected(active) = &self.phase {
            self.service.send_to(
                active.session,
                red_service::Command::AiSetConfigOption {
                    conversation_id,
                    config_id,
                    value,
                    boolean,
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
    fn build_chat_selectables(
        &mut self,
        conversation_id: red_service::ConversationId,
        cx: &mut Context<Self>,
    ) {
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
            // Only a trailing window of bubbles keeps its selectable leaves resident:
            // each leaf is a live GPUI entity, so an unbounded chat would accumulate
            // thousands. Older bubbles shed theirs (freed here) and repaint as plain,
            // non-selectable text. Generous enough that a normal-length chat is wholly
            // selectable; only a very long one sheds its distant history.
            let keep_from = chat.messages.len().saturating_sub(SELECTABLE_BUBBLE_WINDOW);
            for (i, msg) in chat.messages.iter_mut().enumerate() {
                if i < keep_from {
                    msg.shed_selectables();
                    continue;
                }
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
        conversation_id: red_service::ConversationId,
        delta: red_service::AiDelta,
        cx: &mut Context<Self>,
    ) {
        // Under a reduced-motion preference, skip the typewriter entirely: text
        // appears the instant it arrives.
        let reduce_motion = cx.reduce_motion();
        // Route to whichever chat owns the turn, not just the active one, and
        // across both surfaces (sidebar + agent tabs), so a background chat keeps
        // streaming while another is shown.
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
                    source_ordinal,
                } => {
                    let node = red_core::ActivityNode {
                        id,
                        kind,
                        status,
                        source_ordinal,
                        detail: None,
                        children: Vec::new(),
                    };
                    let bubble = chat.assistant_bubble();
                    match parent
                        .as_ref()
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
    fn ensure_reveal_ticker(
        &mut self,
        conversation_id: red_service::ConversationId,
        cx: &mut Context<Self>,
    ) {
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
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(REVEAL_TICK).await;
                let keep_going = this
                    .update(cx, |this, cx| this.tick_reveal(conversation_id, cx))
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// One reveal step: uncover more of the streaming bubble and repaint. Returns
    /// whether the ticker should fire again (false once it's caught up; a new burst
    /// will restart it via `ensure_reveal_ticker`).
    fn tick_reveal(
        &mut self,
        conversation_id: red_service::ConversationId,
        cx: &mut Context<Self>,
    ) -> bool {
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
        conversation_id: red_service::ConversationId,
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
            // Persist the now-complete exchange so it survives a restart.
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
        conversation_id: red_service::ConversationId,
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

    /// The agent asked to run a tool RED didn't auto-allow: show the prompt
    /// on its originating chat (the switcher flags a background one).
    /// The turn finished with writes sitting in an uncommitted transaction: show
    /// the review card. Nothing is durable until the user answers.
    pub(crate) fn on_ai_sandbox_ready(
        &mut self,
        conversation_id: red_service::ConversationId,
        statements: Vec<red_service::SandboxEntry>,
        total_rows: u64,
        expires_in_secs: u64,
        cx: &mut Context<Self>,
    ) {
        let updated = self
            .with_chat_mut(conversation_id, |chat| {
                chat.streaming = false;
                chat.pending_sandbox = Some(PendingSandbox {
                    statements,
                    total_rows,
                    expires_at: std::time::Instant::now()
                        + std::time::Duration::from_secs(expires_in_secs),
                });
            })
            .is_some();
        if updated {
            cx.notify();
        }
    }

    /// The user's Commit / Roll back, or an expiry, landed. Clear the card and say
    /// what happened: a rollback is not a failure, but it is not a no-op either.
    pub(crate) fn on_ai_sandbox_resolved(
        &mut self,
        conversation_id: red_service::ConversationId,
        committed: bool,
        rows: u64,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.with_chat_mut(conversation_id, |chat| chat.pending_sandbox = None);
        let _ = match error {
            Some(e) => self.notify(
                flint::ToastVariant::Error,
                format!("The review transaction could not be resolved: {e}"),
                cx,
            ),
            None if committed => self.notify(
                flint::ToastVariant::Success,
                format!("Committed {rows} row(s) from the agent's changes."),
                cx,
            ),
            None => self.notify(
                flint::ToastVariant::Info,
                format!("Rolled back {rows} row(s); nothing was applied."),
                cx,
            ),
        };
        cx.notify();
    }

    /// The sandbox hit its deadline before the user answered. Rolled back, because
    /// an open transaction holds locks and committing unasked is not an option.
    pub(crate) fn on_ai_sandbox_expired(
        &mut self,
        conversation_id: red_service::ConversationId,
        cx: &mut Context<Self>,
    ) {
        self.with_chat_mut(conversation_id, |chat| chat.pending_sandbox = None);
        let _ = self.notify(
            flint::ToastVariant::Warning,
            "The agent's changes were rolled back: the review transaction timed out. An open              transaction holds locks, so it can't wait indefinitely.",
            cx,
        );
        cx.notify();
    }

    /// Point the timeline at the activity node a Sources chip names, or clear the
    /// highlight when the same chip is clicked again.
    pub(crate) fn highlight_source(&mut self, id: red_core::ActivityId, cx: &mut Context<Self>) {
        if let Some(state) = self.assistant.as_mut() {
            state.highlighted_source =
                (state.highlighted_source.as_ref() != Some(&id)).then_some(id);
        }
        cx.notify();
    }

    /// Answer the active chat's review card.
    pub(crate) fn resolve_sandbox(&mut self, commit: bool, cx: &mut Context<Self>) {
        let Some(state) = self.assistant.as_ref() else {
            return;
        };
        let conversation_id = state.active().conversation_id;
        if let Phase::Connected(active) = &self.phase {
            self.service.send_to(
                active.session,
                red_service::Command::AiSandboxResolve {
                    conversation_id,
                    commit,
                },
            );
        }
        // The card stays until the backend confirms: the transaction is not gone
        // until the engine says so, and clearing early would claim otherwise.
        cx.notify();
    }

    /// Flip the active *draft* chat into (or out of) review-transaction mode.
    /// Locked once a chat has sent a turn: a conversation cannot change what its
    /// earlier writes were run under.
    pub(crate) fn toggle_sandbox_mode(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.assistant.as_mut() {
            let chat = state.active_mut();
            if chat.is_draft() {
                chat.sandbox = !chat.sandbox;
            }
        }
        cx.notify();
    }

    /// Whether review-transaction mode can be offered at all: a SQL connection on
    /// an engine with real transactions, at the write tier, not read-only, on the
    /// API-key path (the ACP transport never advertises write tools).
    pub(crate) fn sandbox_available(&self) -> Option<&'static str> {
        let Phase::Connected(active) = &self.phase else {
            return Some("Connect to a database first.");
        };
        if !matches!(
            active.config.kind,
            red_core::DbKind::Postgres | red_core::DbKind::Mysql | red_core::DbKind::Sqlite
        ) {
            return Some("This engine has no multi-statement transaction to hold changes in.");
        }
        if active.config.read_only {
            return Some("This connection is read-only, so the agent cannot write at all.");
        }
        if self.ai_tier_effective() != red_core::AiTier::Write {
            return Some("The agent is not at the write tier on this connection.");
        }
        if self
            .assistant
            .as_ref()
            .is_some_and(|s| self.agent_is_acp(&s.active().provider))
        {
            return Some(
                "A subscription agent is never offered write tools, so it has nothing                          to hold.",
            );
        }
        None
    }

    pub(crate) fn on_ai_permission_request(
        &mut self,
        conversation_id: red_service::ConversationId,
        request_id: red_service::RequestId,
        title: String,
        detail: Option<String>,
        preview: Option<red_service::WritePreview>,
        cx: &mut Context<Self>,
    ) {
        if self
            .with_chat_mut(conversation_id, |chat| {
                chat.pending_permission = Some(PendingPermission {
                    request_id,
                    title: title.into(),
                    detail: detail.map(Into::into),
                    preview,
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
        conversation_id: red_service::ConversationId,
        path: String,
        title: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let attached = self
            .with_chat_mut(conversation_id, |chat| {
                let bubble = chat.assistant_bubble();
                bubble.activity.push(red_core::ActivityNode {
                    id: format!("report-{path}").into(),
                    kind: red_core::ActivityKind::Report {
                        path: path.clone(),
                        title,
                    },
                    status: red_core::ActivityStatus::Ok,
                    // A report is a document RED produced, not evidence for a
                    // figure in the answer.
                    source_ordinal: None,
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
    fn deny_stranded_permission(
        &self,
        conversation_id: red_service::ConversationId,
        request_id: red_service::RequestId,
    ) {
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
            .and_then(|r| r.read(cx).error())
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
            if reads_allowed
                && let Some(grid) = t.result.as_ref().map(|g| g.read(cx))
                && let Some((rows, cols)) = grid.status_counts()
            {
                let names: Vec<String> = (0..cols)
                    .filter_map(|c| grid.column_meta(c).map(|(name, _)| name))
                    .collect();
                // Deliberately NOT localized: this string is spliced into the
                // model's grounding context (`ai_context`), not rendered. A
                // translated (or pseudolocale) build would silently change the
                // language of the prompt the model reasons over, so it stays
                // hardcoded English.
                s.push_str(&format!(
                    ", showing a result of {rows} row(s) × {cols} column(s): {}",
                    names.join(", ")
                ));
            }
            s
        });
        red_service::AiContext {
            schema_summary: summarize_schema(&active.schema.read(cx).schemas),
            // The key the query history and the recent-keys store are filed under;
            // the service has no other way to know which saved connection this is.
            conn_id: active.conn_id.clone(),
            // Filled in by the service, which owns the cursor registry.
            open_cursors: None,
            // What the user wrote down about this database: glossary, join rules,
            // gotchas. Refreshed from disk by `dispatch_turn` just before this runs.
            knowledge: active.knowledge.clone(),
            current_tab,
            editor_sql,
            last_error,
            // Filled per-turn by `dispatch_turn` from the chat's chips; `ai_context`
            // describes what is on screen, references are what was pointed at.
            references: Vec::new(),
            // Set per-turn by `send_turn` only on the first turn after a restore.
            prior_transcript: None,
            connection: format!(
                "{} database \"{}\"",
                active.config.kind, active.config.database
            ),
            read_only: active.config.read_only,
            // Paint AI-generated reports in RED's active theme (Ayu, GitHub Dark, …).
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
    id: &red_core::ActivityId,
) -> Option<&'a mut red_core::ActivityNode> {
    for node in nodes {
        if node.id == *id {
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
/// One staged attachment as the transcript remembers it: name, size, kind and
/// where it came from — never the bytes.
fn stored_attachment(a: &super::attach::Attachment) -> crate::conversations::StoredAttachment {
    crate::conversations::StoredAttachment {
        name: a.name.clone(),
        bytes: a.bytes,
        path: a.path.to_string_lossy().into_owned(),
        kind: match a.kind {
            super::attach::AttachmentKind::Text => "text",
            super::attach::AttachmentKind::Image => "image",
            super::attach::AttachmentKind::Pdf => "pdf",
        }
        .to_string(),
    }
}

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
        sandbox: chat.sandbox,
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
                attachments: m.attachments.clone(),
            })
            .collect(),
        path: Default::default(),
        stem: stem.clone(),
    };
    if let Err(e) = crate::conversations::save(&stem, &conv) {
        tracing::warn!("failed to persist conversation: {e}");
    }
}
