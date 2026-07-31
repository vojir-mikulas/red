//! The assistant panel's views: the docked transcript + composer, the setup view,
//! the merged history sidebar and its rows, the agent picker, quick-action chips,
//! the permission prompt, and one chat bubble. Render-only helpers (the streaming
//! caret, the status dot, the usage footer, character slicing) live alongside. The
//! behavior these buttons fire lives in `state`; the pure helpers in `text`.

use std::collections::HashMap;
use std::time::Duration;

use flint::prelude::*;
use gpui::{
    Animation, AnimationExt, AnyElement, Context, MouseButton, Pixels, Point, SharedString, canvas,
    div, prelude::*, px,
};

use crate::app::{AppState, Phase};

use super::text::{bubble_key, derive_title};
use super::{
    AgentMenuKind, AssistantState, ChatMessage, ChatRole, ChatSession, HistoryRow,
    PendingPermission, PendingSandbox, QuickAction, Rename, RowKey, RowStatus,
};

/// The outer height every control in the composer's two toolbar rows is drawn
/// at — the selector slots, the switch pills, and send/stop. They sit a few
/// pixels apart in a narrow sidebar, where differing heights read as a
/// misalignment rather than as deliberate hierarchy. Scaled through
/// `Theme::scale` at each use so the whole toolbar tracks the UI font size.
const COMPOSER_CONTROL: f32 = 22.;

/// The session-config selectors the composer always keeps a slot for, in the
/// order they're drawn, each with the name to fall back on before the agent has
/// said what it calls the control. They're drawn even before the agent advertises
/// them so the composer's shape is stable from the first frame; an unadvertised
/// slot is inert and reads "—".
const CONFIG_SLOTS: [(red_service::AiConfigCategory, &str); 3] = [
    (red_service::AiConfigCategory::Model, "Model"),
    (red_service::AiConfigCategory::Reasoning, "Thinking"),
    // The agent's permission mode (default / accept edits / auto / bypass),
    // advertised as a `Mode` selector; round-trips like the others.
    (red_service::AiConfigCategory::Mode, "Mode"),
];

impl AppState {
    /// The config controls the composer should render for `chat`: the chat's own
    /// advertised set once its session is up, else this agent's cached set (seeded
    /// from disk on open, or from an earlier session this run) so the controls
    /// still show before the first turn. A pre-session pick persists via settings
    /// and applies on session open.
    fn config_options<'a>(
        &self,
        state: &'a AssistantState,
        chat: &'a ChatSession,
    ) -> &'a [red_service::AiConfigOption] {
        if chat.config_options.is_empty() && self.agent_is_acp(&chat.provider) {
            state
                .provider_config_options
                .get(&chat.provider)
                .map(Vec::as_slice)
                .unwrap_or(&[])
        } else {
            chat.config_options.as_slice()
        }
    }

    /// The composer's settings row: one equal-width, captioned slot per selector in
    /// [`CONFIG_SLOTS`], so Model / Thinking / Mode read as a set rather than as
    /// three differently-sized dropdowns that reflow whenever a label changes
    /// length. Every slot is always present on the subscription path — a selector
    /// the agent hasn't advertised (yet) renders inert with a placeholder instead
    /// of vanishing.
    ///
    /// `None` on the API-key path, which has no session config at all: the model is
    /// a setting there, and there is no thinking level or permission mode to pick.
    /// Dimmed and non-interactive while a turn streams (config applies between turns).
    fn render_config_row(
        &self,
        state: &AssistantState,
        chat: &ChatSession,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.agent_is_acp(&chat.provider) {
            return None;
        }
        let theme = cx.theme().clone();
        let options = self.config_options(state, chat);
        let mut row = div()
            .flex()
            .items_stretch()
            .gap_1p5()
            .min_w(px(0.))
            .px_2()
            .pt_1();
        for slot in CONFIG_SLOTS {
            let opt = options
                .iter()
                .find(|o| o.category == slot.0 && !o.boolean && !o.choices.is_empty());
            row = row.child(self.render_config_slot(state, chat, slot, opt, &theme, cx));
        }
        Some(row.into_any_element())
    }

    /// One slot in the settings row: a borderless [`Select`] filling a bordered
    /// box. `opt` is `None` for a selector the agent hasn't advertised, which
    /// draws the same box with a dim "—".
    ///
    /// The slots carry no printed caption — three of them in a sidebar this narrow
    /// is a line of vertical space for a label the fixed left-to-right order
    /// already teaches. What each one controls is on its hover tooltip, and the
    /// open dropdown names it too.
    ///
    /// The slot is a *block* container on purpose: its children then stretch to
    /// the slot's width, which is what makes every dropdown the same size no
    /// matter how long its current label is.
    fn render_config_slot(
        &self,
        state: &AssistantState,
        chat: &ChatSession,
        slot: (red_service::AiConfigCategory, &str),
        opt: Option<&red_service::AiConfigOption>,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (cat, name) = slot;
        let streaming = chat.streaming;
        // Prefer the agent's own wording for the control over RED's fallback.
        let title = opt.map_or_else(|| name.to_string(), |o| o.name.clone());
        let slot = div()
            .id(SharedString::from(format!("ai-config-slot-{cat:?}")))
            .flex_1()
            .min_w(px(0.))
            // One height for every control in the composer's two toolbar rows —
            // see `COMPOSER_CONTROL`. Taffy is border-box, so this is the outer
            // height and the trigger inside gets the 2px less.
            .h(theme.scale(COMPOSER_CONTROL))
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg_elevated);

        let Some(opt) = opt else {
            // Advertised-later placeholder: same box, same rhythm, nothing to click.
            return slot
                .opacity(0.6)
                .tooltip(flint::Tooltip::text(SharedString::from(format!(
                    "{title} — offered once the agent's session is up"
                ))))
                .child(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .px_2()
                        .text_size(theme.font_size_xs())
                        .text_color(theme.text_faint)
                        .child("—"),
                )
                .into_any_element();
        };
        let slot = slot.tooltip(flint::Tooltip::text(SharedString::from(title)));

        let selected = opt
            .choices
            .iter()
            .position(|c| c.value == opt.current_value)
            .unwrap_or(usize::MAX);
        let is_open = !streaming && state.open_config.as_deref() == Some(opt.id.as_str());
        let mut select = Select::new(SharedString::from(format!("ai-config-{}", opt.id)))
            .selected(selected)
            .open(is_open)
            // The slot draws the box; the trigger only draws its label + chevron,
            // filling the slot minus its border.
            .seamless()
            .height(theme.scale(COMPOSER_CONTROL - 2.))
            // Neutral, not accent-colored: these toolbar dropdowns shouldn't
            // compete with the Send button for emphasis.
            .accent(false)
            // Lucide disclosure + check glyphs, matching the app's other dropdowns.
            .chevron(crate::icons::icon(
                "chevron-down",
                theme.scale(13.),
                theme.text_dim,
            ))
            .check(crate::icons::icon("check", theme.scale(13.), theme.text))
            .placeholder(crate::i18n::tr!("assistant.default", "Default"));
        for choice in &opt.choices {
            select = select.option(SharedString::from(choice.name.clone()));
        }
        if !streaming {
            let view = cx.entity();
            let id_toggle = opt.id.clone();
            select = select.on_toggle(move |_, cx| {
                view.update(cx, |this, cx| {
                    if let Some(s) = this.assistant.as_mut() {
                        s.open_config = if s.open_config.as_deref() == Some(id_toggle.as_str()) {
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
        slot.when(streaming, |d| d.opacity(0.5))
            .child(select)
            .into_any_element()
    }

    /// The composer's on/off switches: one per boolean session-config control the
    /// agent advertises. On Claude Code that's fast mode — a higher-throughput
    /// decode on the Opus models that support it — but nothing here is specific to
    /// it: any boolean the agent offers gets a switch.
    ///
    /// When the agent advertises none (it isn't connected yet, or its model has no
    /// such option), a disabled "Fast" switch stands in so the control is
    /// discoverable rather than appearing out of nowhere later.
    fn render_config_switches(
        &self,
        state: &AssistantState,
        chat: &ChatSession,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.agent_is_acp(&chat.provider) {
            return None;
        }
        let streaming = chat.streaming;
        let switches: Vec<&red_service::AiConfigOption> = self
            .config_options(state, chat)
            .iter()
            .filter(|o| o.boolean)
            .collect();

        if switches.is_empty() {
            return Some(
                pill(theme)
                    .id("ai-fast-placeholder")
                    .opacity(0.5)
                    .tooltip(flint::Tooltip::text(
                        "Fast mode: offered by the agent once its session is up, on \
                         models that support it.",
                    ))
                    .child(crate::icons::icon(
                        "zap",
                        theme.scale(11.),
                        theme.text_faint,
                    ))
                    .child(
                        div()
                            .text_size(theme.scale(10.))
                            .text_color(theme.text_faint)
                            .child(crate::i18n::tr!("assistant.fast", "Fast")),
                    )
                    .child(flint::Toggle::new("ai-fast-off", false).disabled(true))
                    .into_any_element(),
            );
        }

        let mut row = div().flex().items_center().gap_1p5();
        for opt in switches {
            let on = opt.current_value == "true";
            // A `zap` for anything fast-mode-shaped, a plain dot otherwise, so the
            // switch reads at a glance without relying on the truncated label.
            let fast = opt.id.to_lowercase().contains("fast")
                || opt.name.to_lowercase().contains("fast")
                || opt.name.to_lowercase().contains("speed");
            let tint = if on { theme.accent } else { theme.text_faint };
            let mut toggle =
                flint::Toggle::new(SharedString::from(format!("ai-switch-{}", opt.id)), on)
                    .label(SharedString::from(opt.name.clone()))
                    .disabled(streaming);
            if !streaming {
                let view = cx.entity();
                let id = opt.id.clone();
                toggle = toggle.on_change(move |next, _, cx| {
                    let (id, value) = (id.clone(), next.to_string());
                    view.update(cx, |this, cx| this.change_config_option(id, value, cx));
                });
            }
            row = row.child(
                pill(theme)
                    .id(SharedString::from(format!("ai-switch-pill-{}", opt.id)))
                    .when(on, |p| p.border_color(theme.accent.opacity(0.5)))
                    .when(streaming, |p| p.opacity(0.5))
                    .tooltip(flint::Tooltip::text(SharedString::from(format!(
                        "{} — {}",
                        opt.name,
                        if on { "on" } else { "off" }
                    ))))
                    .child(crate::icons::icon(
                        if fast { "zap" } else { "toggle-left" },
                        theme.scale(11.),
                        tint,
                    ))
                    .child(
                        div()
                            .max_w(px(64.))
                            .truncate()
                            .text_size(theme.scale(10.))
                            .text_color(tint)
                            .child(SharedString::from(opt.name.clone())),
                    )
                    .child(toggle),
            );
        }
        Some(row.into_any_element())
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
            let view = self.render_assistant_setup(state, header, &theme, cx);
            return self.with_agent_menu(view, cx);
        }

        // The chat-list switcher replaces the transcript while open.
        if state.show_list {
            let view = self.render_assistant_list(state, header, &theme, cx);
            return self.with_agent_menu(view, cx);
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
                 the schema and runs capped, read-only SELECTs through RED's tools."
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
            body = body.children(self.render_knowledge_prompt(&theme, cx));
            // The chat's agent is chosen in the composer's agent dropdown (shown on
            // a draft when more than one agent is usable); no separate body picker.
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
                .size(theme.scale(COMPOSER_CONTROL))
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
                .size(theme.scale(COMPOSER_CONTROL))
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

        // A bordered, rounded composer card (Zed-style), stacked top to bottom:
        // the attachment chips, the multiline input, a settings row of equal-width
        // selector slots, and a status row carrying the usage ring, the agent's
        // switches, and send/stop.
        let composer = div()
            .flex_shrink_0()
            .m_2()
            .flex()
            .flex_col()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg_input)
            .children(self.render_attachment_chips(chat, &theme, cx))
            // No height here: the editor is built with `.rows(4..=8)`, so it sizes
            // itself to the draft and the card grows with it.
            .child(
                div()
                    .min_w(px(0.))
                    .px_2p5()
                    .pt_1p5()
                    .child(state.input.clone()),
            )
            .when_some(self.render_config_row(state, chat, cx), |card, row| {
                card.child(row)
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .pt_1()
                    .pb_1p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w(px(0.))
                            .child(self.render_attach_button(&theme, cx))
                            .child(render_usage(chat.last_usage.as_ref(), &theme))
                            .children(self.render_knowledge_chip(&theme, cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .flex_shrink_0()
                            .when_some(
                                self.render_config_switches(state, chat, &theme, cx),
                                |row, switches| row.child(switches),
                            )
                            .when_some(self.render_sandbox_toggle(chat, &theme, cx), |row, t| {
                                row.child(t)
                            })
                            .child(action),
                    ),
            );

        // The drop target is the whole panel, not the text box: a target the size
        // of a line is a target people miss.
        //
        // The highlight is an **overlay**, not a border on the panel itself.
        // Turning the panel's own border on and off changes its box, which
        // reflows the transcript and the composer under the cursor mid-drag —
        // exactly when the user needs the layout to hold still.
        let dropping = state.drop_active;
        let view = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.bg_panel_2)
            .border_l_1()
            .border_color(theme.border)
            .on_drag_move::<gpui::ExternalPaths>(cx.listener(
                |this, e: &gpui::DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                    // GPUI fires this for every element under the drag, so gate on
                    // the cursor actually being over the panel; without it the
                    // highlight sticks after the drag leaves.
                    let (b, p) = (e.bounds, e.event.position);
                    let inside = p.x >= b.origin.x
                        && p.x < b.origin.x + b.size.width
                        && p.y >= b.origin.y
                        && p.y < b.origin.y + b.size.height;
                    this.set_assistant_drop_active(inside, cx);
                },
            ))
            .on_drop::<gpui::ExternalPaths>(cx.listener(
                |this, paths: &gpui::ExternalPaths, _, cx| {
                    this.set_assistant_drop_active(false, cx);
                    this.attach_paths(paths.paths().to_vec(), cx);
                },
            ))
            // The same target takes in-app references. From the user's side it is
            // one mechanism -- "drop things here" -- which is the right mental
            // model even though the two payloads land in different places.
            .on_drag_move::<super::refs::ContextRef>(cx.listener(
                |this, e: &gpui::DragMoveEvent<super::refs::ContextRef>, _, cx| {
                    let (b, p) = (e.bounds, e.event.position);
                    let inside = p.x >= b.origin.x
                        && p.x < b.origin.x + b.size.width
                        && p.y >= b.origin.y
                        && p.y < b.origin.y + b.size.height;
                    this.set_assistant_drop_active(inside, cx);
                },
            ))
            .on_drop::<super::refs::ContextRef>(cx.listener(
                |this, reference: &super::refs::ContextRef, _, cx| {
                    this.set_assistant_drop_active(false, cx);
                    this.add_reference(reference.clone(), cx);
                },
            ))
            // A tab already drags for reordering, and an element carries one
            // payload — so the panel reads that payload instead of the tab strip
            // growing a second one.
            .on_drag_move::<crate::editor::TabDrag>(cx.listener(
                |this, e: &gpui::DragMoveEvent<crate::editor::TabDrag>, _, cx| {
                    let (b, p) = (e.bounds, e.event.position);
                    let inside = p.x >= b.origin.x
                        && p.x < b.origin.x + b.size.width
                        && p.y >= b.origin.y
                        && p.y < b.origin.y + b.size.height;
                    this.set_assistant_drop_active(inside, cx);
                },
            ))
            .on_drop::<crate::editor::TabDrag>(cx.listener(
                |this, drag: &crate::editor::TabDrag, _, cx| {
                    // Dropping a tab here references it; it must not also be read
                    // as a reorder by whatever sits underneath.
                    cx.stop_propagation();
                    this.set_assistant_drop_active(false, cx);
                    this.reference_tab(drag.0, cx);
                },
            ))
            .child(header)
            .child(body)
            .when_some(chat.pending_permission.as_ref(), |col, pending| {
                col.child(self.render_permission(pending, &theme, cx))
            })
            .when_some(chat.pending_sandbox.as_ref(), |col, pending| {
                col.child(self.render_sandbox_review(pending, &theme, cx))
            })
            .when_some(self.render_quick_actions(chat, &theme, cx), |col, chips| {
                col.child(chips)
            })
            .child(composer)
            // Last child so it sits over the panel, and deliberately
            // non-interactive (no id, no handlers) so it never eats the drop it
            // is advertising.
            .when(dropping, |v| {
                v.child(
                    div()
                        .absolute()
                        .inset_0()
                        .border_1()
                        .border_dashed()
                        .border_color(theme.accent.opacity(0.55))
                        .bg(theme.accent.opacity(0.04)),
                )
            })
            .into_any_element();
        self.with_agent_menu(view, cx)
    }

    /// The composer's `+` button: the discoverable half of attaching a file (the
    /// other is dropping one on the panel, which nobody finds by looking).
    fn render_attach_button(&self, theme: &flint::Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("assistant-attach")
            .size(theme.scale(COMPOSER_CONTROL))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .tooltip(flint::Tooltip::text(
                "Attach a file (or drop one on this panel)",
            ))
            .hover(|s| s.border_color(theme.border_strong))
            .child(crate::icons::icon(
                "plus",
                theme.scale(13.),
                theme.text_muted,
            ))
            .on_click(cx.listener(|this, _, _, cx| this.pick_attachments(cx)))
            .into_any_element()
    }

    /// The staged-attachment row above the composer input, plus whatever the last
    /// refusal was. Absent when there is nothing to show, so the composer keeps
    /// its shape.
    fn render_attachment_chips(
        &self,
        chat: &ChatSession,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if chat.attachments.is_empty() && chat.references.is_empty() && chat.attach_error.is_none()
        {
            return None;
        }
        let mut row = div().flex().flex_wrap().gap_1p5();
        // References share the row with attachments: from the user's side both
        // are "things I put on this message", distinguished by their icon.
        for (i, reference) in chat.references.iter().enumerate() {
            row = row.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1p5()
                    .py(px(2.))
                    .rounded(px(4.))
                    .bg(theme.bg_panel)
                    .border_1()
                    .border_color(theme.border)
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(crate::icons::icon(
                        reference.icon(),
                        theme.scale(11.),
                        theme.accent,
                    ))
                    .child(SharedString::from(reference.label()))
                    .child(
                        div()
                            .id(("assistant-reference-remove", i))
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.red))
                            .child(crate::icons::icon("x", theme.scale(11.), theme.text_faint))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_reference(i, cx);
                            })),
                    ),
            );
        }
        for (i, attachment) in chat.attachments.iter().enumerate() {
            row = row.child(
                div()
                    .id(("assistant-attachment-chip", i))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1p5()
                    .py(px(2.))
                    .rounded(px(4.))
                    .bg(theme.bg_panel)
                    .border_1()
                    .border_color(theme.border)
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    // "Is that the right screenshot?" is a question only the
                    // picture answers; a generated filename never does.
                    .when_some(image_preview(attachment), |chip, preview| {
                        chip.tooltip(preview)
                    })
                    .child(crate::icons::icon(
                        attachment.kind.icon(),
                        theme.scale(11.),
                        theme.text_faint,
                    ))
                    .child(SharedString::from(attachment.name.clone()))
                    .child(div().text_color(theme.text_faint).child(SharedString::from(
                        super::attach::human_bytes(attachment.bytes),
                    )))
                    // Tabular data is usually better asked *about* than read, so
                    // the doorway into the import pipeline sits on the chip.
                    .when(attachment.is_importable(), |chip| {
                        chip.child(
                            div()
                                .id(("assistant-attachment-import", i))
                                .cursor_pointer()
                                .text_color(theme.text_faint)
                                .tooltip(flint::Tooltip::text(
                                    "Import into the open table instead of sending it",
                                ))
                                .hover(|s| s.text_color(theme.accent))
                                .child("import")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.import_attachment(i, cx);
                                })),
                        )
                    })
                    .child(
                        div()
                            .id(("assistant-attachment-remove", i))
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.red))
                            .child(crate::icons::icon("x", theme.scale(11.), theme.text_faint))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_attachment(i, cx);
                            })),
                    ),
            );
        }
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .px_2p5()
                .pt_2()
                .child(row)
                .when_some(chat.attach_error.clone(), |col, why| {
                    col.child(
                        div()
                            .text_size(theme.scale(10.5))
                            .text_color(theme.red)
                            .child(why),
                    )
                })
                .into_any_element(),
        )
    }

    /// The composer's "knowledge: 1.2 KB" chip, beside the usage ring. Shown only
    /// when a knowledge file is actually in the prompt, so it is never a mystery
    /// why the agent knows something it could not have read off the schema. Clicking
    /// it opens the file.
    fn render_knowledge_chip(
        &self,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let bytes = self.knowledge_bytes()?;
        Some(
            div()
                .id("assistant-knowledge-chip")
                .flex()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .cursor_pointer()
                .tooltip(flint::Tooltip::text(
                    "This connection's knowledge file is in the agent's prompt. Click to edit.",
                ))
                .hover(|s| s.opacity(0.8))
                .child(crate::icons::icon(
                    "file-text",
                    theme.scale(10.),
                    theme.text_faint,
                ))
                .child(
                    div()
                        .text_size(theme.scale(10.))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(fmt_kb(bytes))),
                )
                .on_click(cx.listener(|this, _, _, cx| this.open_knowledge_editor(cx)))
                .into_any_element(),
        )
    }

    /// The empty state's one line about the knowledge file, shown only while the
    /// connection has none: what the agent is missing, a button that offers to draft
    /// one, and a link to write it by hand.
    ///
    /// Exactly one line, and only when there is nothing: once a file exists the
    /// footer chip carries the signal, and a panel that keeps asking for a glossary
    /// is a panel people learn to ignore.
    fn render_knowledge_prompt(
        &self,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !matches!(self.phase, crate::app::Phase::Connected(_))
            || self.knowledge_bytes().is_some()
        {
            return None;
        }
        let link = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .text_size(theme.scale(11.5))
                .text_color(theme.accent)
                .cursor_pointer()
                .hover(|s| s.opacity(0.8))
                .child(label)
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(
                    div()
                        .text_size(theme.scale(11.5))
                        .text_color(theme.text_faint)
                        .child(crate::i18n::tr!(
                            "knowledge.empty_state",
                            "This connection has no knowledge file. The agent will infer \
                             everything from the schema."
                        )),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .when(self.can_learn_database(), |row| {
                            row.child(link("ai-learn-db", "Learn this database").on_click(
                                cx.listener(|this, _, _, cx| this.learn_this_database(cx)),
                            ))
                        })
                        .child(link("ai-edit-knowledge", "Edit").on_click(
                            cx.listener(|this, _, _, cx| this.open_knowledge_editor(cx)),
                        )),
                )
                .into_any_element(),
        )
    }

    /// Overlay the header's open agent menu (switch / new-chat picker) on top of the
    /// panel `view`, or pass it through untouched when no menu is open. A relative,
    /// full-size wrapper lets the menu's backdrop cover the whole panel to catch an
    /// outside click.
    fn with_agent_menu(&self, view: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let Some((kind, pos)) = self.assistant.as_ref().and_then(|s| s.agent_menu) else {
            return view;
        };
        div()
            .relative()
            .size_full()
            .child(view)
            .child(self.render_agent_menu(kind, pos, cx))
            .into_any_element()
    }

    /// The header's agent menu: one row per usable agent. `Switch` re-binds the
    /// current draft (or, if it's already sent, starts a fresh chat on the pick);
    /// `New` always starts a fresh chat on the pick. Anchored at the click, with a
    /// full-cover backdrop that dismisses it — mirroring the result cell menu.
    fn render_agent_menu(
        &self,
        kind: AgentMenuKind,
        pos: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self
            .assistant
            .as_ref()
            .map(|s| s.active().provider.clone())
            .unwrap_or_default();
        let mut menu = ContextMenu::new("ai-agent-menu");
        for (i, agent) in self.usable_agents.iter().enumerate() {
            let id = agent.id.clone();
            let label = match kind {
                AgentMenuKind::Switch => SharedString::from(agent.name.clone()),
                AgentMenuKind::New => SharedString::from(format!("New chat with {}", agent.name)),
            };
            let mut item = ContextMenuItem::new(SharedString::from(format!("ai-agent-{i}")), label);
            // Mark the chat's current agent when switching (no "current" on the
            // new-chat picker, where every option makes a fresh chat).
            if matches!(kind, AgentMenuKind::Switch) && agent.id == current {
                item = item.shortcut("current");
            }
            item = item.on_click(cx.listener(move |this, _, _, cx| {
                if let Some(s) = this.assistant.as_mut() {
                    s.agent_menu = None;
                }
                match kind {
                    AgentMenuKind::Switch => {
                        // A draft re-binds in place; a sent chat can't change agent,
                        // so the pick opens a new chat on it instead.
                        let draft = this
                            .assistant
                            .as_ref()
                            .is_some_and(|s| s.active().is_draft());
                        if draft {
                            this.set_active_chat_provider(id.clone(), cx);
                        } else {
                            this.new_chat_with(id.clone(), cx);
                        }
                    }
                    AgentMenuKind::New => this.new_chat_with(id.clone(), cx),
                }
                cx.notify();
            }));
            menu = menu.item(item);
        }
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(s) = this.assistant.as_mut() {
                        s.agent_menu = None;
                    }
                    cx.notify();
                }),
            )
            .child(floating(div().occlude().child(menu)).at(pos))
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
        // A choice of agent exists only with more than one usable: the header agent
        // label becomes a switcher and the `+` offers a per-agent new-chat menu.
        let multi = self.usable_agents.len() > 1;

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

        // The chat switcher: toggles a list of all open chats. A red dot
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

        // With one agent the `+` just starts a chat; with several it drops a
        // "New chat with <agent>" menu so you pick the agent up front.
        let new_chat_tip = if multi {
            "New chat with…"
        } else {
            "New chat"
        };
        let new_chat = self.ai_configured.then(|| {
            icon_btn("assistant-new-chat", "plus", new_chat_tip).on_click(cx.listener(
                move |this, ev: &gpui::ClickEvent, _, cx| {
                    if this.usable_agents.len() > 1 {
                        let pos = ev.position();
                        if let Some(s) = this.assistant.as_mut() {
                            s.agent_menu = Some((AgentMenuKind::New, pos));
                        }
                        cx.notify();
                    } else {
                        this.new_chat(cx);
                    }
                },
            ))
        });

        // Copy the whole conversation as Markdown (pastes styled into Notion etc.);
        // only meaningful once the chat has content.
        let copy_chat = (self.ai_configured && !state.active().messages.is_empty()).then(|| {
            icon_btn("assistant-copy-chat", "copy", "Copy chat as Markdown")
                .on_click(cx.listener(|this, _, _, cx| this.copy_conversation(cx)))
        });

        // The connection's knowledge file: what the agent is told about this
        // database before it reads a single row. Tinted when one exists, so the
        // header answers "does the agent know anything about this?" at a glance.
        let has_knowledge = self.knowledge_bytes().is_some();
        let knowledge_btn = matches!(self.phase, crate::app::Phase::Connected(_)).then(|| {
            div()
                .id("assistant-knowledge")
                .flex()
                .items_center()
                .justify_center()
                .size(px(20.))
                .rounded(px(4.))
                .cursor_pointer()
                .tooltip(flint::Tooltip::text(if has_knowledge {
                    "Database knowledge (in this chat's prompt)"
                } else {
                    "Database knowledge (none written yet)"
                }))
                .hover(|s| s.bg(theme.bg_elevated))
                .child(crate::icons::icon(
                    "file-text",
                    theme.scale(13.),
                    if has_knowledge {
                        theme.accent
                    } else {
                        theme.text_muted
                    },
                ))
                .on_click(cx.listener(|this, _, _, cx| this.open_knowledge_editor(cx)))
        });

        // Deletion lives only in the history sidebar (each row's trash); the chat
        // view never deletes the conversation it's showing.
        let header_actions = div()
            .flex()
            .items_center()
            .gap_1()
            .when_some(copy_chat, |row, c| row.child(c))
            .when_some(knowledge_btn, |row, k| row.child(k))
            .when_some(list_btn, |row, l| row.child(l))
            .when_some(new_chat, |row, n| row.child(n));

        // The agent label: sparkles + name. With more than one usable agent it
        // becomes a dropdown trigger (a chevron, hover, and a click that opens the
        // switch menu at the cursor); a draft re-binds, a sent chat opens a new one.
        let agent_inner = div()
            .flex()
            .items_center()
            .gap_1p5()
            .min_w(px(0.))
            .child(crate::icons::icon(
                "sparkles",
                theme.scale(14.),
                theme.accent,
            ))
            .child(div().min_w_0().truncate().child(agent_name))
            .when(multi, |d| {
                d.child(crate::icons::icon(
                    "chevron-down",
                    theme.scale(13.),
                    theme.text_muted,
                ))
            });
        let agent_label: AnyElement = if multi {
            div()
                .id("assistant-agent-switch")
                .flex()
                .items_center()
                .min_w(px(0.))
                .px_1()
                .rounded(px(4.))
                .cursor_pointer()
                .tooltip(flint::Tooltip::text("Switch agent"))
                .hover(|s| s.bg(theme.bg_elevated))
                .child(agent_inner)
                .on_click(cx.listener(|this, ev: &gpui::ClickEvent, _, cx| {
                    let pos = ev.position();
                    if let Some(s) = this.assistant.as_mut() {
                        s.agent_menu = Some((AgentMenuKind::Switch, pos));
                    }
                    cx.notify();
                }))
                .into_any_element()
        } else {
            agent_inner.into_any_element()
        };

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
                    .min_w(px(0.))
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(agent_label)
                    // A "writes" badge when this connection opted into the write tier
                    //, so the user knows the agent can propose data changes
                    // (each one still gated by per-statement approval).
                    .when(chat.sandbox, |row| {
                        row.child(
                            div()
                                .id("ai-sandbox-badge")
                                .flex()
                                .items_center()
                                .gap_1()
                                .px_1p5()
                                .rounded(theme.radius_sm)
                                .bg(theme.accent.opacity(0.14))
                                .text_size(theme.scale(10.))
                                .text_color(theme.accent)
                                // "Am I in a transaction right now" must never be
                                // ambiguous, so it is stated in the header rather
                                // than only on the card at the end.
                                .tooltip(flint::Tooltip::text(
                                    "Writes run in one transaction you commit or roll back at \
                                     the end of each turn.",
                                ))
                                .child("review"),
                        )
                    })
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
                                .child(crate::i18n::tr!("assistant.writes", "writes"))
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
            .child(crate::i18n::tr!("assistant.save_key", "Save key"))
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
                            .child(crate::i18n::tr!(
                                "assistant.add_an_anthropic_api_key_to_use_the_agent",
                                "Add an Anthropic API key to use the agent."
                            )),
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
            .child(crate::i18n::tr!("assistant.new_chat", "New chat"))
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

    /// The context-action chips: "Explain error" when the active result
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

    /// The tool-permission prompt: what the agent wants to do, plus
    /// Allow/Deny. Docked above the composer so it's visible regardless of scroll;
    /// the agent is blocked until the user answers.
    /// The Sources line under a settled assistant bubble: one numbered chip per
    /// data-returning call this turn made, each pointing at its node in the
    /// timeline above.
    ///
    /// The count is the point. "This paragraph cites three queries" and "this
    /// paragraph cites nothing" are different claims about an answer, and without
    /// this they render identically -- the trace was always there, collapsed, two
    /// clicks away, which is the same as absent.
    ///
    /// Deliberately labelled *Sources*, never *Verified*: a citation proves a
    /// source exists, not that the sentence follows from it. No checkmarks, no
    /// green.
    fn render_sources(
        &self,
        msg: &ChatMessage,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let sources = collect_sources(&msg.activity);
        if sources.is_empty() {
            // A turn that read nothing gets no footer at all, rather than an empty
            // one: chrome that says nothing still costs a line and teaches the eye
            // to skip the row.
            return None;
        }
        let highlighted = self
            .assistant
            .as_ref()
            .and_then(|s| s.highlighted_source.clone());
        let mut row = div().flex().flex_wrap().items_center().gap_1p5().child(
            div()
                .text_size(theme.scale(10.))
                .text_color(theme.text_faint)
                .child("Sources"),
        );
        for source in sources {
            let on = highlighted.as_ref() == Some(&source.id);
            let click_id = source.id.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("src-{}", source.id.as_str())))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1p5()
                    .rounded(theme.radius_sm)
                    .border_1()
                    .border_color(if on { theme.accent } else { theme.border })
                    .bg(if on {
                        theme.accent.opacity(0.12)
                    } else {
                        theme.bg_elevated
                    })
                    .text_size(theme.scale(10.))
                    .text_color(if on { theme.accent } else { theme.text_muted })
                    .cursor_pointer()
                    .hover(|s| s.border_color(theme.accent))
                    .tooltip(flint::Tooltip::text(SharedString::from(source.peek())))
                    .child(SharedString::from(format!("[{}]", source.ordinal)))
                    .child(SharedString::from(source.label()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.highlight_source(click_id.clone(), cx)
                    })),
            );
        }
        Some(row.into_any_element())
    }

    /// The composer's review-transaction switch, shown only on a chat that hasn't
    /// sent a turn yet.
    ///
    /// Locked after the first turn because a conversation cannot change what its
    /// earlier writes were run under, and shown *disabled with the reason* where
    /// the mode can't be honoured rather than hidden — "why isn't this here" is a
    /// worse question than "here's why not".
    fn render_sandbox_toggle(
        &self,
        chat: &ChatSession,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !chat.is_draft() {
            return None;
        }
        let blocked = self.sandbox_available();
        let on = chat.sandbox && blocked.is_none();
        let tint = if on { theme.accent } else { theme.text_faint };
        let tip: SharedString = match blocked {
            Some(why) => format!("Review transaction unavailable: {why}").into(),
            None => "Run this chat's writes in one transaction you commit or roll back at the                      end of each turn, instead of approving each statement."
                .into(),
        };
        Some(
            pill(theme)
                .id("ai-sandbox-toggle")
                .when(on, |p| p.border_color(theme.accent.opacity(0.5)))
                .when(blocked.is_some(), |p| p.opacity(0.5))
                .tooltip(flint::Tooltip::text(tip))
                .child(crate::icons::icon("lock", theme.scale(11.), tint))
                .child(
                    div()
                        .text_size(theme.scale(10.))
                        .text_color(tint)
                        .child("review"),
                )
                .when(blocked.is_none(), |p| {
                    p.cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_sandbox_mode(cx)))
                })
                .into_any_element(),
        )
    }

    /// The review card: what the turn changed, and the two answers.
    ///
    /// The framing is deliberate. This is not "did that work?" but "do you want
    /// this?", asked once about the whole change rather than N times about
    /// statements — five approvals can each be reasonable and the sequence still be
    /// wrong. Rolling back is free and is the left-hand, unaccented button;
    /// committing is the one that costs something.
    fn render_sandbox_review(
        &self,
        pending: &PendingSandbox,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let remaining = pending
            .expires_at
            .saturating_duration_since(std::time::Instant::now());
        let secs = remaining.as_secs();

        let mut rows = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .font_family(theme.mono_family.clone())
            .text_size(theme.scale(10.5));
        for entry in &pending.statements {
            rows = rows.child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(theme.text_muted)
                            .child(SharedString::from(entry.sql.clone())),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(theme.text_faint)
                            .child(SharedString::from(format!("{} row(s)", entry.rows))),
                    ),
            );
        }

        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(theme.bg_panel)
            .border_t_1()
            .border_color(theme.accent.opacity(0.5))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(crate::icons::icon("lock", theme.scale(13.), theme.accent))
                    .child(SharedString::from(format!(
                        "The agent made {} change(s) in a transaction. Nothing is committed yet.",
                        pending.statements.len()
                    ))),
            )
            .child(rows)
            .child(
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_faint)
                    // Everything outside the transaction stayed: a generated
                    // report, a saved query, an exported file. Rolling back does
                    // not un-write those, and implying otherwise would be a lie
                    // about the one thing this feature sells.
                    .child(
                        "Rolling back undoes the database changes only. Files the agent wrote \
                         (reports, exports, saved queries) stay.",
                    ),
            )
            // Past the halfway mark, count down: an expiry should never be a
            // surprise, because a rollback the user didn't choose still costs them
            // the turn.
            .when(secs <= SANDBOX_COUNTDOWN_FROM_SECS, |card| {
                card.child(
                    div()
                        .text_size(theme.scale(10.5))
                        .text_color(theme.yellow)
                        .child(SharedString::from(format!(
                            "Rolls back automatically in {secs}s - an open transaction holds \
                             locks."
                        ))),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("ai-sandbox-rollback")
                            .px_3()
                            .h(px(26.))
                            .flex()
                            .items_center()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(theme.border)
                            .text_size(theme.scale(12.))
                            .text_color(theme.text_muted)
                            .cursor_pointer()
                            .hover(|s| s.border_color(theme.text).text_color(theme.text))
                            .child("Roll back")
                            .on_click(
                                cx.listener(|this, _, _, cx| this.resolve_sandbox(false, cx)),
                            ),
                    )
                    .child(
                        div()
                            .id("ai-sandbox-commit")
                            .px_3()
                            .h(px(26.))
                            .flex()
                            .items_center()
                            .rounded(px(6.))
                            .bg(theme.accent)
                            .text_size(theme.scale(12.))
                            .text_color(theme.bg_app)
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .child(SharedString::from(format!(
                                "Commit {} row(s)",
                                pending.total_rows
                            )))
                            .on_click(cx.listener(|this, _, _, cx| this.resolve_sandbox(true, cx))),
                    ),
            )
            .into_any_element()
    }

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
        if let Some(preview) = &pending.preview {
            card = card.child(render_write_preview(preview, theme));
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

        // What the user attached, above their words: it is what the sentence is
        // about. Metadata only, because that is all the transcript stores.
        if !msg.attachments.is_empty() {
            let mut chips = div().flex().flex_wrap().gap_1p5();
            for (ai, attachment) in msg.attachments.iter().enumerate() {
                chips = chips.child(
                    div()
                        .id(SharedString::from(format!(
                            "ai-attachment-{}-{ai}",
                            bubble_key(index)
                        )))
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_1p5()
                        .py(px(2.))
                        .rounded(px(4.))
                        .bg(theme.bg_panel)
                        .border_1()
                        .border_color(theme.border)
                        .text_size(theme.scale(10.5))
                        .text_color(theme.text_muted)
                        .when_some(stored_image_preview(attachment), |chip, preview| {
                            chip.tooltip(preview)
                        })
                        .child(crate::icons::icon(
                            stored_attachment_icon(&attachment.kind),
                            theme.scale(11.),
                            theme.text_faint,
                        ))
                        .child(SharedString::from(attachment.name.clone()))
                        .child(div().text_color(theme.text_faint).child(SharedString::from(
                            super::attach::human_bytes(attachment.bytes),
                        ))),
                );
            }
            bubble = bubble.child(chips);
        }

        // The agent's plan checklist (assistant only), at the top of the turn so the
        // intended steps are visible and tick off as it works.
        if msg.role == ChatRole::Assistant && !msg.plan.is_empty() {
            bubble = bubble.child(render_plan(&msg.plan, theme));
        }

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

        // The turn's activity timeline (assistant only): tool calls, subagents, and
        // proposed writes, in call order, each with a live status glyph. Reports are
        // excluded here — they render as a card below the answer (see the bottom of
        // this fn) — so skip the timeline when a turn's only activity is a report.
        let has_timeline = msg
            .activity
            .iter()
            .any(|n| !matches!(n.kind, red_core::ActivityKind::Report { .. }));
        if msg.role == ChatRole::Assistant && has_timeline {
            let empty = HashMap::new();
            let collapse = self
                .assistant
                .as_ref()
                .map(|s| &s.subagent_collapse)
                .unwrap_or(&empty);
            let highlighted = self
                .assistant
                .as_ref()
                .and_then(|s| s.highlighted_source.as_ref());
            bubble = bubble.child(render_activity(
                &msg.activity,
                collapse,
                highlighted,
                theme,
                0,
                cx,
            ));
        }
        // Sources sit *above* the answer, where a reader meets them before the
        // number rather than after having already believed it.
        if !live
            && msg.role == ChatRole::Assistant
            && let Some(footer) = self.render_sources(msg, theme, cx)
        {
            bubble = bubble.child(footer);
        }

        // Answer text. Assistant turns are Markdown, so render them (on the revealed
        // prefix while typing); user turns are plain.
        if msg.role == ChatRole::Assistant {
            if !shown.is_empty() {
                // A settled bubble renders from its cached parse (frame-stable) and,
                // when its selectable leaves are built, routes each text leaf through
                // a pooled `SelectableLabel` so prose can be highlighted and copied.
                // The live one still parses its revealed prefix fresh each tick as
                // plain text (not yet final), but that's a single message.
                let md = if live {
                    crate::markdown::render(shown, theme)
                } else if let Some(leaves) = msg.selectables_for(theme.text) {
                    let blocks = msg.markdown();
                    let mut it = leaves.iter();
                    crate::markdown::render_blocks_with(&blocks, theme, &mut |text, runs| {
                        if text.is_empty() {
                            return div().into_any_element();
                        }
                        // Consume the prebuilt leaves in document order (the build pass
                        // walked the same blocks). A drift falls back to plain text.
                        match it.next() {
                            Some(e) => e.clone().into_any_element(),
                            None => gpui::StyledText::new(SharedString::from(text))
                                .with_runs(runs)
                                .into_any_element(),
                        }
                    })
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
            // A user turn is plain text; render its pooled selectable label when built
            // (color inherited from this div), else the plain string.
            let body = div().text_size(theme.scale(12.5)).text_color(theme.text);
            let body = match msg.selectables_for(theme.text).and_then(|l| l.first()) {
                Some(e) => body.child(e.clone()),
                None => body.child(msg.text.clone()),
            };
            bubble = bubble.child(body);
        }

        // SQL affordances for the first fenced SQL block in a *settled* assistant
        // turn (suppressed while still typing): insert it into the active editor, or
        // open it in a fresh query tab (a read-only SELECT runs there automatically).
        if !live
            && msg.role == ChatRole::Assistant
            && let Some(sql) = msg.sql_block()
        {
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
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.ai_insert_sql(insert_sql.clone(), cx)
                        })),
                    )
                    .child(
                        chip(
                            SharedString::from(format!("ai-open-{key}")),
                            "table",
                            "Open in a query tab",
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_query_in_tab(sql.clone(), cx)
                        })),
                    ),
            );
        }

        // Generated reports: a prominent card per report, at the very bottom of the
        // turn (below the answer), so they don't get lost among the tool calls above.
        // Each carries an "Open" button; the report is never opened automatically.
        if msg.role == ChatRole::Assistant {
            for node in &msg.activity {
                if let red_core::ActivityKind::Report { path, title } = &node.kind {
                    bubble = bubble.child(div().mt_1().child(render_report_card(
                        node.id.as_str(),
                        path,
                        title.as_deref(),
                        theme,
                        cx,
                    )));
                }
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

/// The agent's plan checklist for a turn: a bordered box of steps, each with a
/// status glyph, completed steps dimmed. Shown at the top of the assistant bubble.
fn render_plan(steps: &[red_core::PlanStep], theme: &flint::Theme) -> AnyElement {
    use red_core::PlanStepStatus::*;
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .p(px(8.))
        .rounded(theme.radius_sm)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .text_size(theme.scale(10.))
                .text_color(theme.text_muted)
                .child(crate::i18n::tr!("assistant.plan", "Plan")),
        );
    for step in steps {
        // Lucide status glyphs: a dashed ring for not-started, a spinner arc while
        // running, a checked ring when done.
        let (icon_name, color) = match step.status {
            Pending => ("circle-dashed", theme.text_muted),
            InProgress => ("loader-circle", theme.accent),
            Completed => ("circle-check", theme.green),
        };
        let title_color = if step.status == Completed {
            theme.text_muted
        } else {
            theme.text
        };
        col = col.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(theme.scale(11.))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .child(crate::icons::icon(icon_name, theme.scale(12.), color)),
                )
                .child(div().text_color(title_color).child(step.title.clone())),
        );
    }
    col.into_any_element()
}

/// The lucide icon name + color for an activity node, shared by the row and the
/// subagent card so a delegate and its children read on the same scale.
fn activity_glyph(
    status: red_core::ActivityStatus,
    theme: &flint::Theme,
) -> (&'static str, gpui::Hsla) {
    use red_core::ActivityStatus::*;
    match status {
        Pending => ("circle-dashed", theme.text_muted),
        Running => ("loader-circle", theme.accent),
        Ok => ("circle-check", theme.green),
        // Ran fine, but its result carries a caveat: amber and a different glyph,
        // so one glance at the timeline shows the query was flagged.
        Warned => ("alert-triangle", theme.yellow),
        Failed => ("circle-x", theme.red),
        Denied => ("ban", theme.yellow),
    }
}

/// The turn's activity timeline: one row per node (tool call / write), with a
/// subagent drawn as a bordered, collapsible card wrapping its own delegated
/// children, so a delegation is unmistakably visible in the chat rather than a flat
/// run of rows. `collapse` carries the user's per-subagent expand/collapse overrides.
fn render_activity(
    nodes: &[red_core::ActivityNode],
    collapse: &HashMap<SharedString, bool>,
    // Which node a Sources chip is currently pointing at.
    //
    // Passed down rather than fetched here. This runs during `render`, which
    // holds `AppState` leased, so reaching back through the entity's own handle
    // for it aborted the process the moment any turn produced a timeline -- see
    // the `lease_guard_tests` note in `connect.rs`.
    highlighted: Option<&red_core::ActivityId>,
    theme: &flint::Theme,
    depth: usize,
    cx: &mut Context<AppState>,
) -> AnyElement {
    let mut col = div().flex().flex_col().gap(px(2.));
    for node in nodes {
        if let red_core::ActivityKind::Subagent { task } = &node.kind {
            // Every subagent is a bordered, collapsible card: it carries either its
            // delegated children (direct-provider path) or its live streamed progress
            // (the ACP path), so it reads as a distinct unit of work, never an empty
            // box. Parallel subagents are siblings here (see the ACP relay).
            col = col.child(render_subagent_card(
                node,
                task,
                collapse,
                highlighted,
                theme,
                depth,
                cx,
            ));
            continue;
        }
        if matches!(node.kind, red_core::ActivityKind::Report { .. }) {
            // Reports render as a prominent card *below* the answer (see
            // `render_bubble`), not in this timeline where they're easy to miss next to
            // the tool calls. Skip them here.
            continue;
        }
        col = col.child(render_activity_row(node, theme, depth, highlighted));
        if !node.children.is_empty() {
            col = col.child(render_activity(
                &node.children,
                collapse,
                highlighted,
                theme,
                depth + 1,
                cx,
            ));
        }
    }
    col.into_any_element()
}

/// A subagent's task text for display, or `None` when it's just the generic tool
/// name ("Task") — so the label reads "Subagent" rather than "Subagent Task".
fn subagent_task_label(task: &str) -> Option<&str> {
    let t = task.trim();
    (!t.is_empty() && !t.eq_ignore_ascii_case("task")).then_some(t)
}

/// A small pulsing accent dot that reads as "still working", for a running
/// subagent's status slot. Rests solid under a reduced-motion preference.
fn running_dot(node_id: &str, theme: &flint::Theme, reduce_motion: bool) -> AnyElement {
    let dot = div().size(px(7.)).rounded_full().bg(theme.accent);
    if reduce_motion {
        return dot.into_any_element();
    }
    dot.with_animation(
        SharedString::from(format!("subagent-pulse-{node_id}")),
        Animation::new(Duration::from_millis(1100)).repeat(),
        |dot, delta| {
            let o = 0.25 + 0.75 * (0.5 + 0.5 * (delta * std::f32::consts::TAU).cos());
            dot.opacity(o)
        },
    )
    .into_any_element()
}

/// A delegated subagent: a bordered, elevated, collapsible card. The header carries
/// the sparkle mark, its task, and a status slot — a **pulsing dot while it works**,
/// then ✓/✗ when it finishes. The body (shown while expanded) carries either its
/// delegated children (direct-provider path) or its **live streamed progress** (the
/// ACP path, which is all that protocol exposes), with a "Working…" hint until the
/// first line arrives. Expanded while running so ongoing work stays visible; auto-
/// collapses once done to keep the transcript tidy. This is the "clearly still
/// working, with its current progress" surface.
fn render_subagent_card(
    node: &red_core::ActivityNode,
    task: &str,
    collapse: &HashMap<SharedString, bool>,
    // Threaded through for the nested tool calls; see `render_activity`.
    highlighted: Option<&red_core::ActivityId>,
    theme: &flint::Theme,
    depth: usize,
    cx: &mut Context<AppState>,
) -> AnyElement {
    use red_core::ActivityStatus::{Denied, Failed, Ok as StatusOk, Pending, Running, Warned};
    let id = SharedString::from(node.id.as_str());
    let done = matches!(node.status, StatusOk | Warned | Failed | Denied);
    let running = matches!(node.status, Running | Pending);
    // Default: expanded while working (so its progress shows), collapsed once done;
    // a stored override wins.
    let collapsed = collapse.get(&id).copied().unwrap_or(done);
    let chevron = if collapsed { "chevron" } else { "chevron-down" };

    // Status slot: a pulsing dot while working, else the terminal glyph.
    let status_slot = if running {
        div()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(px(13.))
            .child(running_dot(node.id.as_str(), theme, cx.reduce_motion()))
            .into_any_element()
    } else {
        let (glyph, glyph_color) = activity_glyph(node.status, theme);
        div()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(px(13.))
            .child(crate::icons::icon(glyph, theme.scale(12.), glyph_color))
            .into_any_element()
    };

    let mut header = div()
        .id(SharedString::from(format!("subagent-{}", node.id)))
        .flex()
        .items_center()
        .gap(px(6.))
        .cursor_pointer()
        .text_size(theme.scale(11.))
        .child(crate::icons::icon(
            chevron,
            theme.scale(11.),
            theme.text_muted,
        ))
        .child(crate::icons::icon(
            "sparkles",
            theme.scale(11.),
            theme.accent,
        ))
        .child(
            div()
                .flex_none()
                .text_color(theme.accent)
                .child(crate::i18n::tr!("assistant.subagent", "Subagent")),
        );
    if let Some(label) = subagent_task_label(task) {
        header = header.child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme.text_muted)
                .child(label.to_string()),
        );
    } else {
        // No real task label: still push the status slot to the right.
        header = header.child(div().flex_1());
    }
    // A "working" word beside the pulse makes the running state unmistakable.
    if running {
        header = header.child(
            div()
                .flex_none()
                .text_color(theme.text_muted)
                .child(crate::i18n::tr!("assistant.working_badge", "working")),
        );
    } else if collapsed && !node.children.is_empty() {
        let n = node.children.len();
        header = header.child(
            div()
                .flex_none()
                .text_color(theme.text_muted)
                .child(format!("{n} step{}", if n == 1 { "" } else { "s" })),
        );
    }
    header = header.child(status_slot);
    let toggle = id.clone();
    header = header.on_click(cx.listener(move |this, _, _, cx| {
        this.set_subagent_collapsed(toggle.clone(), !collapsed, cx)
    }));

    let mut card = div()
        .ml(px(depth as f32 * 14.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .p(px(8.))
        .rounded(theme.radius_sm)
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg_elevated)
        .child(header);

    if !collapsed {
        // The delegate's own tool calls (direct path), nested inside the card.
        if !node.children.is_empty() {
            card = card.child(render_activity(
                &node.children,
                collapse,
                highlighted,
                theme,
                0,
                cx,
            ));
        }
        // Its current progress / result line (the ACP path's ongoing-work signal),
        // or a "Working…" hint while running before the first line arrives.
        if let Some(detail) = &node.detail {
            card = card.child(
                div()
                    .text_size(theme.scale(11.))
                    .text_color(if node.status == Failed {
                        theme.red
                    } else {
                        theme.text_muted
                    })
                    .font_family(theme.mono_family.clone())
                    .child(detail.clone()),
            );
        } else if running && node.children.is_empty() {
            card = card.child(
                div()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_muted)
                    .child(crate::i18n::tr!("assistant.working", "Working…")),
            );
        }
    }
    card.into_any_element()
}

/// A generated report: a bordered card carrying a document icon, the report's title
/// (or a generic label), and an accent "Open" button that hands the HTML file to the
/// system browser. Unlike the old behaviour, the report never opens itself — it stays
/// in the transcript so the user can open it whenever they like, and it persists with
/// the conversation.
fn render_report_card(
    node_id: &str,
    path: &str,
    title: Option<&str>,
    theme: &flint::Theme,
    cx: &mut Context<AppState>,
) -> AnyElement {
    let label = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Report")
        .to_string();
    let open_path = path.to_string();
    let open = div()
        .id(SharedString::from(format!("report-open-{node_id}")))
        .flex_none()
        .px_2()
        .h(px(22.))
        .flex()
        .items_center()
        .gap_1()
        .rounded(px(5.))
        .bg(theme.accent)
        .text_size(theme.scale(11.))
        .text_color(theme.on_accent)
        .cursor_pointer()
        .hover(|s| s.opacity(0.9))
        .child(crate::icons::icon(
            "external-link",
            theme.scale(11.),
            theme.on_accent,
        ))
        .child(crate::i18n::tr!("assistant.open", "Open"))
        .on_click(cx.listener(move |this, _, _, cx| this.open_report(open_path.clone(), cx)));

    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .p(px(8.))
        .rounded(theme.radius_sm)
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg_elevated)
        .text_size(theme.scale(11.))
        .child(crate::icons::icon(
            "file-text",
            theme.scale(13.),
            theme.accent,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().truncate().text_color(theme.text).child(label))
                .child(
                    div()
                        .text_size(theme.scale(10.))
                        .text_color(theme.text_muted)
                        .child(crate::i18n::tr!("assistant.html_report", "HTML report")),
                ),
        )
        .child(open)
        .into_any_element()
}

/// One activity row: a status glyph, the node's label, its argument summary, and a
/// trailing detail (row count / error) once known — all muted so the trace sits
/// quietly beneath the answer.
fn render_activity_row(
    node: &red_core::ActivityNode,
    theme: &flint::Theme,
    depth: usize,
    highlighted: Option<&red_core::ActivityId>,
) -> AnyElement {
    use red_core::ActivityStatus::Failed;
    let (glyph, glyph_color) = activity_glyph(node.status, theme);
    let (primary, secondary) = match &node.kind {
        red_core::ActivityKind::Tool { name, args_summary } => (name.clone(), args_summary.clone()),
        red_core::ActivityKind::Subagent { task } => ("Subagent".to_string(), Some(task.clone())),
        red_core::ActivityKind::Write { sql } => (
            "Write".to_string(),
            sql.lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string()),
        ),
        // Reports render as a card in `render_activity`; this is only a defensive
        // fallback so the match stays exhaustive.
        red_core::ActivityKind::Report { title, .. } => ("Report".to_string(), title.clone()),
        red_core::ActivityKind::Compacted { dropped } => (
            "Context compacted".to_string(),
            Some(format!(
                "{dropped} earlier tool result{} dropped to stay inside the context window",
                if *dropped == 1 { "" } else { "s" }
            )),
        ),
    };

    // A Sources chip points here: ring the row so the answer's `[3]` and the call
    // that produced it are visibly the same thing.
    let on = highlighted == Some(&node.id);
    let mut row = div()
        .flex()
        .items_center()
        .gap(px(6.))
        .pl(px(depth as f32 * 14.))
        .when(on, |r| {
            r.rounded(theme.radius_sm)
                .bg(theme.accent.opacity(0.12))
                .border_1()
                .border_color(theme.accent.opacity(0.5))
        })
        .text_size(theme.scale(11.))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .child(crate::icons::icon(glyph, theme.scale(12.), glyph_color)),
        )
        .children(node.source_ordinal.map(|n| {
            div()
                .flex_none()
                .text_color(theme.accent)
                .font_family(theme.mono_family.clone())
                .child(SharedString::from(format!("[{n}]")))
        }))
        .child(div().text_color(theme.text).child(primary));
    if let Some(secondary) = secondary {
        row = row.child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme.text_muted)
                .font_family(theme.mono_family.clone())
                .child(secondary),
        );
    }
    if let Some(detail) = &node.detail {
        row = row.child(
            div()
                .text_color(if node.status == Failed {
                    theme.red
                } else {
                    theme.text_muted
                })
                .child(detail.clone()),
        );
    }
    row.into_any_element()
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

/// The hover preview for a staged attachment, or `None` when it is not an image
/// (there is nothing to look at) or the file has gone (a preview of a missing
/// file is a broken box, which is worse than no preview).
fn image_preview(
    attachment: &super::attach::Attachment,
) -> Option<impl Fn(&mut gpui::Window, &mut gpui::App) -> gpui::AnyView + 'static + use<>> {
    (attachment.kind == super::attach::AttachmentKind::Image && attachment.path.exists())
        .then(|| preview_builder(attachment.path.clone(), attachment.name.clone()))
}

/// The same, for an attachment a saved chat remembers. Its path can be stale --
/// the file may have been moved or deleted since -- which is exactly why the
/// existence check is not optional.
fn stored_image_preview(
    attachment: &crate::conversations::StoredAttachment,
) -> Option<impl Fn(&mut gpui::Window, &mut gpui::App) -> gpui::AnyView + 'static + use<>> {
    let path = std::path::PathBuf::from(&attachment.path);
    (attachment.kind == "image" && !attachment.path.is_empty() && path.exists())
        .then(|| preview_builder(path, attachment.name.clone()))
}

fn preview_builder(
    path: std::path::PathBuf,
    name: String,
) -> impl Fn(&mut gpui::Window, &mut gpui::App) -> gpui::AnyView + 'static {
    let name = SharedString::from(name);
    move |_window, cx| {
        let (path, name) = (path.clone(), name.clone());
        cx.new(|_| ImagePreview { path, name }).into()
    }
}

/// A hover preview of an attached image: the picture itself, bounded, above its
/// filename.
///
/// Worth its own view rather than a text tooltip, because "is that the right
/// screenshot?" is a question only the picture answers, and the filename a
/// screenshot tool generates never does.
struct ImagePreview {
    path: std::path::PathBuf,
    name: SharedString,
}

impl gpui::Render for ImagePreview {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.bg_elevated)
            .child(
                // Bounded so a 4K screenshot does not become a 4K tooltip; the
                // aspect ratio is the image's own.
                gpui::img(self.path.clone())
                    .max_w(px(280.))
                    .max_h(px(280.))
                    .rounded(px(3.)),
            )
            .child(
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(self.name.clone()),
            )
    }
}

/// The chip icon for a persisted attachment's kind string. Falls back to the
/// generic file glyph, so a chat saved by a future version that knows more kinds
/// still renders.
fn stored_attachment_icon(kind: &str) -> &'static str {
    match kind {
        "image" => "image",
        "text" => "file-text",
        _ => "file",
    }
}

/// How full the context window is, as a 0..=1 fraction, when both the tokens in
/// context and the window they sit in are known. `None` when the model is one
/// neither backend could size — better a bare token count than a percentage of a
/// guess.
fn context_fraction(usage: &red_service::AiUsage) -> Option<f32> {
    (usage.context_tokens > 0)
        .then(|| (usage.context_used_tokens as f32 / usage.context_tokens as f32).clamp(0., 1.))
}

/// The full accounting, as the ring's hover tooltip: the context share, the token
/// breakdown, and the running session cost. This is the detail the composer's
/// footer used to spend a whole strip on; the ring carries it now. One line, since
/// the tooltip is a single text run.
///
/// The token figures are the conversation's totals, not the last exchange's: the
/// number worth looking at is what this chat has cost so far.
fn usage_tooltip(usage: &red_service::AiUsage) -> String {
    let mut parts: Vec<String> = Vec::new();
    match context_fraction(usage) {
        Some(f) => parts.push(format!(
            "Context {} of {} ({:.0}%)",
            compact_count(usage.context_used_tokens),
            compact_count(usage.context_tokens),
            f * 100.
        )),
        // Window unknown: the counts are all there is to say.
        None if usage.context_used_tokens > 0 => {
            parts.push(format!(
                "{} in context",
                compact_count(usage.context_used_tokens)
            ));
        }
        None => {}
    }
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
        parts.push(format!("${cost:.4} this session"));
    }
    if parts.is_empty() {
        "No usage reported yet".to_string()
    } else {
        parts.join(" · ")
    }
}

/// The usage gauge in the composer's status row: a ring filled by the share of
/// the context window in use, with the percentage beside it. The full token and
/// cost breakdown moved into its hover tooltip — the numbers matter when you go
/// looking for them, the "am I running out of room" signal matters at a glance.
///
/// The ring stays put with no usage yet (an empty track and a dim "—") so the row
/// doesn't jump the moment the first turn finishes.
fn render_usage(usage: Option<&red_service::AiUsage>, theme: &flint::Theme) -> AnyElement {
    let fraction = usage.and_then(context_fraction);
    // Amber past three fifths, red past nine tenths. Amber is deliberately early:
    // it is the hint that the chat is filling up while there is still room to
    // finish the thought, not the warning that it is too late. Red is where
    // starting a new chat stops being optional.
    let color = match fraction {
        Some(f) if f >= 0.9 => theme.red,
        Some(f) if f >= 0.6 => theme.yellow,
        Some(_) => theme.accent,
        None => theme.text_faint,
    };
    let label = match (fraction, usage) {
        (Some(f), _) => format!("{:.0}%", f * 100.),
        // Window unknown: fall back to the raw count of what's in context.
        (None, Some(u)) if u.context_used_tokens > 0 => compact_count(u.context_used_tokens),
        _ => "—".to_string(),
    };
    div()
        .id("assistant-usage")
        .flex()
        .items_center()
        .gap_1p5()
        .min_w(px(0.))
        .tooltip(flint::Tooltip::text(SharedString::from(usage.map_or_else(
            || "No usage reported yet".to_string(),
            usage_tooltip,
        ))))
        .child(usage_ring(fraction, color, theme.border_strong))
        .child(
            div()
                .truncate()
                .text_size(theme.scale(10.))
                .text_color(if fraction.is_some() {
                    theme.text_muted
                } else {
                    theme.text_faint
                })
                .child(SharedString::from(label)),
        )
        .into_any_element()
}

/// A 16px donut: a full-circle track plus a `fraction` arc from 12 o'clock,
/// clockwise. Painted rather than assembled from divs — an arc is the one shape
/// a box model can't make.
fn usage_ring(fraction: Option<f32>, color: gpui::Hsla, track: gpui::Hsla) -> AnyElement {
    const SIZE: f32 = 16.;
    const STROKE: f32 = 2.5;
    let filled = fraction.unwrap_or(0.).clamp(0., 1.);
    // The size goes on the canvas itself: it has no intrinsic one, and a bare
    // `canvas` would lay out zero-high and paint the ring off its own bounds.
    canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            let center = gpui::point(
                bounds.origin.x + px(SIZE / 2.),
                bounds.origin.y + px(SIZE / 2.),
            );
            let radius = (SIZE - STROKE) / 2.;
            paint_arc(window, center, radius, 1., track, STROKE);
            if filled > 0. {
                paint_arc(window, center, radius, filled, color, STROKE);
            }
        },
    )
    .size(px(SIZE))
    .flex_shrink_0()
    .into_any_element()
}

/// Stroke `turns` of a circle (0..=1) clockwise from 12 o'clock as a polyline —
/// `PathBuilder` has no arc primitive, and at ring scale the segments read as a
/// smooth curve.
fn paint_arc(
    window: &mut gpui::Window,
    center: Point<Pixels>,
    radius: f32,
    turns: f32,
    color: gpui::Hsla,
    stroke: f32,
) {
    // Enough segments that a full circle stays smooth at this size, scaled down
    // for shorter arcs so a 5% sliver doesn't pay for 48 of them.
    let steps = ((48. * turns).ceil() as usize).max(2);
    let mut pb = gpui::PathBuilder::stroke(px(stroke));
    for i in 0..=steps {
        let angle = -std::f32::consts::FRAC_PI_2
            + std::f32::consts::TAU * turns * (i as f32 / steps as f32);
        let p = gpui::point(
            center.x + px(radius * angle.cos()),
            center.y + px(radius * angle.sin()),
        );
        if i == 0 {
            pb.move_to(p);
        } else {
            pb.line_to(p);
        }
    }
    if let Ok(path) = pb.build() {
        window.paint_path(path, color);
    }
}

/// The affected-row preview above the Allow/Deny row: what an approved write
/// would actually touch.
///
/// The count is the point, so it is the largest thing here. A count of **zero**
/// is styled as a warning rather than as a reassuring small number: a write that
/// matches nothing is nearly always a wrong predicate, and it is exactly the case
/// a bare "Allow this?" hid best.
fn render_write_preview(preview: &red_service::WritePreview, theme: &flint::Theme) -> AnyElement {
    let mut col = div().flex().flex_col().gap_1p5();
    let many = preview.statements.len() > 1;
    for (i, stmt) in preview.statements.iter().enumerate() {
        col = col.child(render_statement_preview(stmt, many.then_some(i + 1), theme));
    }
    if preview.not_previewed > 0 {
        col = col.child(
            div()
                .text_size(theme.scale(10.5))
                .text_color(theme.text_faint)
                // Said out loud, because silence here would read as "the rest
                // affect nothing".
                .child(SharedString::from(format!(
                    "{} more statement(s) were not previewed.",
                    preview.not_previewed
                ))),
        );
    }
    col.into_any_element()
}

/// One statement's line: the count (or why there isn't one) and up to a few of
/// the rows it matched.
fn render_statement_preview(
    stmt: &red_service::StatementPreview,
    number: Option<usize>,
    theme: &flint::Theme,
) -> AnyElement {
    let empty = stmt.matches == Some(0);
    let prefix = number.map_or(String::new(), |n| format!("{n}. "));
    let headline = match (stmt.matches, stmt.total) {
        (Some(n), Some(total)) => format!(
            "{prefix}Affects {} of {} rows in {}.",
            group_digits(n),
            group_digits(total),
            stmt.table
        ),
        (Some(n), None) => format!(
            "{prefix}Affects {} row(s) in {}.",
            group_digits(n),
            stmt.table
        ),
        // No number: say why, and never let the blank read as zero.
        (None, _) => format!(
            "{prefix}{} in {}.",
            stmt.note.as_deref().unwrap_or("could not preview"),
            stmt.table
        ),
    };
    let color = if empty {
        theme.yellow
    } else if stmt.matches.is_some() {
        theme.text
    } else {
        theme.text_faint
    };

    let mut block = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(theme.scale(11.5))
                .text_color(color)
                .child(SharedString::from(headline)),
        )
        .when(empty, |d| {
            d.child(
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.yellow)
                    .child(
                        "This matches no rows - the predicate is probably wrong. Denying is the \
                         safe answer.",
                    ),
            )
        });

    if !stmt.sample.is_empty() {
        let mut table = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .font_family(theme.mono_family.clone())
            .text_size(theme.scale(10.))
            .child(sample_row(&stmt.columns, theme.text_faint));
        for row in &stmt.sample {
            table = table.child(sample_row(row, theme.text_muted));
        }
        // The sample is a recognition aid, not a result: if more matched than fit,
        // say how many are missing rather than implying that was all of them.
        if let Some(rest) = stmt
            .matches
            .map(|n| n.saturating_sub(stmt.sample.len() as u64))
            .filter(|rest| *rest > 0)
        {
            table = table.child(
                div()
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!("… {} more", group_digits(rest)))),
            );
        }
        block = block.child(table);
    }
    block.into_any_element()
}

/// One line of the sample table. Cells are already rendered and capped by the
/// backend (a blob arrives as `<N bytes>`), so this only lays them out.
fn sample_row(cells: &[String], color: gpui::Hsla) -> AnyElement {
    div()
        .flex()
        .gap_3()
        .text_color(color)
        .children(cells.iter().map(|c| {
            div()
                .max_w(px(140.))
                .truncate()
                .child(SharedString::from(c.clone()))
        }))
        .into_any_element()
}

/// `4213 → 4,213`. A row count is the number the whole prompt turns on, and four
/// undelimited digits are read as three often enough to matter here.
fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One control in the composer's toolbar row: a bordered pill at the shared
/// [`COMPOSER_CONTROL`] height, so the agent's own switches and RED's
/// review-transaction toggle read as one row rather than as two systems.
fn pill(theme: &flint::Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        .h(theme.scale(COMPOSER_CONTROL))
        .px_1p5()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg_elevated)
}

/// One citable call from a turn: what the chip shows and what it points at.
struct Source {
    id: red_core::ActivityId,
    ordinal: u32,
    name: String,
    args: Option<String>,
    detail: Option<String>,
}

impl Source {
    /// The chip's text: the tool name, which is what the reader recognises.
    fn label(&self) -> String {
        self.name.clone()
    }

    /// The hover peek: the tool, what it was called with, and what came back --
    /// enough to judge a number without leaving the paragraph.
    fn peek(&self) -> String {
        let mut out = self.name.clone();
        if let Some(args) = &self.args {
            out.push('\n');
            out.push_str(args);
        }
        if let Some(detail) = &self.detail {
            out.push_str("\n→ ");
            out.push_str(detail);
        }
        out
    }
}

/// The turn's citable calls, in source order.
///
/// Top level only: a subagent's own calls are its children and never reach the
/// parent's prose, so listing them here would offer a citation the parent could
/// not have made.
fn collect_sources(nodes: &[red_core::ActivityNode]) -> Vec<Source> {
    let mut out: Vec<Source> = nodes
        .iter()
        .filter_map(|node| {
            let ordinal = node.source_ordinal?;
            let (name, args) = match &node.kind {
                red_core::ActivityKind::Tool { name, args_summary } => {
                    (name.clone(), args_summary.clone())
                }
                _ => return None,
            };
            Some(Source {
                id: node.id.clone(),
                ordinal,
                name,
                args,
                detail: node.detail.clone(),
            })
        })
        .collect();
    out.sort_by_key(|s| s.ordinal);
    out
}

/// When the review card starts counting down. Half of the 120s default: early
/// enough to react, late enough not to nag from the first frame.
const SANDBOX_COUNTDOWN_FROM_SECS: u64 = 60;

/// A knowledge file's size for the composer chip: `900 → 900 B`, `1234 → 1.2 KB`.
/// Kilobytes are the useful unit here (a file worth writing is a few, and the cap
/// is 32), so anything larger still reads in KB rather than rounding to "0.1 MB".
fn fmt_kb(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.)
    }
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

    #[test]
    fn context_fraction_needs_a_reported_window() {
        // The subscription path reports both, so the ring can fill.
        let acp = red_service::AiUsage {
            context_used_tokens: 50_000,
            context_tokens: 200_000,
            ..Default::default()
        };
        assert_eq!(context_fraction(&acp), Some(0.25));
        // Over-full clamps rather than overdrawing the ring.
        let full = red_service::AiUsage {
            context_used_tokens: 300_000,
            context_tokens: 200_000,
            ..Default::default()
        };
        assert_eq!(context_fraction(&full), Some(1.0));
        // An unrecognized model sizes no window, so there is nothing to fill —
        // and the spend so far must not be mistaken for one.
        let unknown_model = red_service::AiUsage {
            input_tokens: 4_000,
            output_tokens: 900,
            context_used_tokens: 3_100,
            ..Default::default()
        };
        assert_eq!(context_fraction(&unknown_model), None);
        assert_eq!(context_fraction(&red_service::AiUsage::default()), None);
    }

    #[test]
    fn usage_tooltip_carries_the_detail_the_ring_drops() {
        // The API-key path now sizes the window from the model, and the token
        // figures are the conversation's totals rather than the last turn's.
        let api_key = red_service::AiUsage {
            input_tokens: 12_000,
            output_tokens: 3_400,
            cache_read_input_tokens: 88_000,
            context_used_tokens: 30_800,
            context_tokens: 200_000,
            cost_usd: None,
        };
        assert_eq!(
            usage_tooltip(&api_key),
            "Context 30.8k of 200.0k (15%) · 12.0k in · 3.4k out · 88.0k cached"
        );
        // The subscription path reports a session cost alongside.
        let acp = red_service::AiUsage {
            input_tokens: 30_800,
            context_used_tokens: 30_800,
            context_tokens: 200_000,
            cost_usd: Some(0.1647),
            ..Default::default()
        };
        assert_eq!(
            usage_tooltip(&acp),
            "Context 30.8k of 200.0k (15%) · 30.8k in · $0.1647 this session"
        );
        // No window: fall back to what is known.
        let unknown_model = red_service::AiUsage {
            input_tokens: 4_000,
            output_tokens: 900,
            cache_read_input_tokens: 2_000,
            context_used_tokens: 5_100,
            ..Default::default()
        };
        assert_eq!(
            usage_tooltip(&unknown_model),
            "5.1k in context · 4.0k in · 900 out · 2.0k cached"
        );
        // Nothing reported reads as such rather than as an empty tooltip.
        assert_eq!(
            usage_tooltip(&red_service::AiUsage::default()),
            "No usage reported yet"
        );
    }
}
