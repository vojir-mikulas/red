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
        // Gather everything that needs an immutable borrow first, then mutate.
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
        let conversation_id = state.conversation_id;

        let (session, context) = {
            let Phase::Connected(active) = &self.phase else {
                return;
            };
            (active.session, self.ai_context(active, cx))
        };

        // Clear the box and record the exchange.
        if let Some(state) = self.assistant.as_ref() {
            state.input.update(cx, |i, cx| i.set_content("", cx));
        }
        if let Some(state) = self.assistant.as_mut() {
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

    pub(crate) fn on_ai_finished(&mut self, conversation_id: u64, cx: &mut Context<Self>) {
        if let Some(state) = self.assistant.as_mut() {
            if state.conversation_id == conversation_id {
                state.streaming = false;
                state.status = None;
                state.pending_permission = None;
                cx.notify();
            }
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
        red_service::AiContext {
            schema_summary: summarize_schema(&active.schema.schemas),
            editor_sql,
            last_error: None,
            selection: None,
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

        // Header: title + close.
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
            .child(close);

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
            .child(composer)
            .into_any_element()
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
}
