//! The assistant panel's views: the docked transcript + composer, the setup view,
//! the merged history sidebar and its rows, the agent picker, quick-action chips,
//! the permission prompt, and one chat bubble. Render-only helpers (the streaming
//! caret, the status dot, the usage footer, character slicing) live alongside. The
//! behavior these buttons fire lives in `state`; the pure helpers in `text`.

use std::time::Duration;

use flint::prelude::*;
use gpui::{div, prelude::*, px, Animation, AnimationExt, AnyElement, Context, SharedString};

use crate::app::{AppState, Phase};

use super::text::{bubble_key, derive_title};
use super::{
    AssistantState, ChatMessage, ChatRole, ChatSession, HistoryRow, PendingPermission, QuickAction,
    Rename, RowKey, RowStatus,
};

impl AppState {
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
        // Before this chat opens its own session it has no advertised selectors yet;
        // fall back to the last subscription session's set so the dropdowns still
        // show. A pre-session pick persists via settings and applies on session open.
        let options = if chat.config_options.is_empty() && self.agent_is_acp(&chat.provider) {
            &state.last_config_options
        } else {
            &chat.config_options
        };
        let mut row = div().flex().items_center().gap_1p5().min_w(px(0.));
        let mut any = false;
        for cat in [
            red_service::AiConfigCategory::Model,
            red_service::AiConfigCategory::Reasoning,
            // The agent's permission mode (default / accept edits / auto / bypass),
            // advertised as a `Mode` selector; round-trips like the others.
            red_service::AiConfigCategory::Mode,
        ] {
            let Some(opt) = options
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
                // Neutral, not accent-colored: these toolbar dropdowns shouldn't
                // compete with the Send button for emphasis.
                .accent(false)
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
            row = row.child(
                div()
                    // Each dropdown may shrink below its label width so all of them
                    // (plus Send) fit the narrowest sidebar; the label truncates.
                    .min_w(px(0.))
                    .when(streaming, |d| d.opacity(0.5))
                    .child(select),
            );
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
                 subscription (Claude Code). The first message starts the agent, which reads \
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
            // Before the first message, the chat's agent can still be switched:
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
            body = body.child(self.render_bubble(i, msg, reveal, &theme, cx));
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
                .size(px(24.))
                // Hold a square 1:1 regardless of how the toolbar row compresses.
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .tooltip(flint::Tooltip::text("Stop (Esc)"))
                .hover(|s| s.border_color(theme.red))
                .child(crate::icons::icon("x", theme.scale(13.), theme.text_muted))
                .on_click(cx.listener(|this, _, _, cx| this.cancel_assistant(cx)))
                .into_any_element()
        } else {
            div()
                .id("assistant-send")
                .size(px(24.))
                // Hold a square 1:1 regardless of how the toolbar row compresses.
                .flex_shrink_0()
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
                .child(crate::icons::icon("send", theme.scale(13.), theme.bg_app))
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

        // Deletion lives only in the history sidebar (each row's trash); the chat
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
                                    "This connection allows the agent to propose writes; \
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
    /// the saved conversations on disk, in one searchable list. Clicking a row opens
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
                "No conversations yet. They're kept here as you chat."
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
    /// than one agent is usable (a single agent needs no choice). The chips wrap, so
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

    /// One chat bubble. `index` is the bubble's position in the transcript, used as a
    /// stable per-frame element-id basis for the copy/insert chips (so equal-length
    /// messages never collide). `reveal` is `Some(n)` for the live, still-typing
    /// assistant bubble: only its first `n` characters show and a blinking caret
    /// trails them; `None` renders the whole message (every settled turn).
    fn render_bubble(
        &self,
        index: usize,
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
        // Hidden while typing, since the text isn't final yet.
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
                    .id(SharedString::from(format!("ai-copy-{}", bubble_key(index))))
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

        // Answer text. Assistant turns are Markdown, so render them (on the revealed
        // prefix while typing); user turns are plain.
        if msg.role == ChatRole::Assistant {
            if !shown.is_empty() {
                // A settled bubble renders from its cached parse (frame-stable); the
                // live one still parses its revealed prefix fresh each tick, but
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
                let key = bubble_key(index);
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

/// The first `n` characters of `s` (a byte-safe prefix), or all of it when shorter.
/// Drives the streaming reveal, slicing on a char boundary so multibyte text never
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compacts_token_counts() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_200), "1.2k");
        assert_eq!(compact_count(2_000_000), "2.0M");
    }
}
