// SPDX-License-Identifier: GPL-3.0-or-later

//! The connected shell: top bar · nested resizable split (schema | editor /
//! results) · status bar. The panes are labeled placeholders in M2 — their
//! contents arrive in M3 (schema tree), M4 (SQL editor), M5 (result grid). The
//! split sizes are caller-owned state on [`ActiveConn`].

use flint::prelude::*;
use gpui::{div, prelude::*, px, Axis, Context, Hsla};

use crate::app::{ActiveConn, AppState, Phase};
use crate::assets::{FONT_MONO, FONT_UI};

/// A labeled placeholder pane for a feature that lands in a later milestone.
fn placeholder(
    title: &'static str,
    milestone: &'static str,
    bg: Hsla,
    muted: Hsla,
    faint: Hsla,
) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .bg(bg)
        .child(div().text_sm().text_color(muted).child(title))
        .child(div().text_xs().text_color(faint).child(milestone))
}

impl AppState {
    pub(crate) fn render_shell(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (bg_app, bg_panel, bg_panel_2) = (theme.bg_app, theme.bg_panel, theme.bg_panel_2);
        let (muted, faint) = (theme.text_muted, theme.text_faint);
        let view = cx.entity().downgrade();
        let config = &active.config;

        // --- top bar ---
        let brand = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(px(14.))
                    .text_color(theme.red)
                    .child("RED"),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child("Roughly Enough Data"),
            );

        // ⌘K palette is out of v0.1 scope — an inert visual placeholder only.
        let omni = div()
            .w(px(420.))
            .h(px(24.))
            .px_2p5()
            .rounded(px(6.))
            .bg(theme.bg_input)
            .border_1()
            .border_color(theme.border)
            .flex()
            .items_center()
            .text_size(px(12.))
            .text_color(theme.text_faint)
            .child("Search tables and commands  (⌘K — coming soon)");

        let topbar_right = div()
            .flex()
            .items_center()
            .gap_2()
            .child(Badge::new(config.kind.to_string()))
            .child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(config.name.clone()),
            )
            .child(
                Button::new("disconnect", "Disconnect")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx))),
            );

        let topbar = div()
            .flex_shrink_0()
            .h(px(38.))
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .bg(theme.bg_panel)
            .border_b_1()
            .border_color(theme.border)
            .child(brand)
            .child(div().flex_1().flex().justify_center().child(omni))
            .child(topbar_right);

        // --- nested split: schema | (editor / results) ---
        let inner = {
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            SplitPane::new("shell-split-v", Axis::Vertical)
                .size(active.editor_h)
                .drag(active.editor_drag)
                .min_first(px(80.))
                .on_drag_start(move |anchor, _, cx| {
                    start
                        .update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                a.editor_drag = Some(anchor);
                            }
                            cx.notify();
                        })
                        .ok();
                })
                .on_resize(move |size, _, cx| {
                    resize
                        .update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                a.editor_h = size;
                            }
                            cx.notify();
                        })
                        .ok();
                })
                .on_drag_end(move |_, cx| {
                    end.update(cx, |this, cx| {
                        if let Phase::Connected(a) = &mut this.phase {
                            a.editor_drag = None;
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .first(placeholder("SQL editor", "(M4)", bg_app, muted, faint))
                .second(placeholder("Result grid", "(M5)", bg_panel, muted, faint))
        };

        let outer = {
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            SplitPane::new("shell-split-h", Axis::Horizontal)
                .size(active.sidebar_w)
                .drag(active.sidebar_drag)
                .min_first(px(160.))
                .max_first(px(480.))
                .on_drag_start(move |anchor, _, cx| {
                    start
                        .update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                a.sidebar_drag = Some(anchor);
                            }
                            cx.notify();
                        })
                        .ok();
                })
                .on_resize(move |size, _, cx| {
                    resize
                        .update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                a.sidebar_w = size;
                            }
                            cx.notify();
                        })
                        .ok();
                })
                .on_drag_end(move |_, cx| {
                    end.update(cx, |this, cx| {
                        if let Phase::Connected(a) = &mut this.phase {
                            a.sidebar_drag = None;
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .first(placeholder(
                    "Schema explorer",
                    "(M3)",
                    bg_panel_2,
                    muted,
                    faint,
                ))
                .second(inner)
        };

        let body = div().flex_1().min_h(px(0.)).child(outer);

        // --- status bar ---
        let status_left = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().size(px(6.)).rounded_full().bg(theme.green))
            .child(config.name.clone())
            .when(config.read_only, |row| {
                row.child(div().text_color(theme.yellow).child("read-only"))
            });

        let status_right = div()
            .flex()
            .items_center()
            .gap_3()
            .child(format!("{} {}", config.kind, active.version))
            .child(
                div()
                    .id("theme-swatch")
                    .cursor_pointer()
                    .child("Theme")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_theme(cx))),
            );

        let statusbar = div()
            .flex_shrink_0()
            .h(px(25.))
            .flex()
            .items_center()
            .justify_between()
            .px_2()
            .bg(theme.bg_panel_2)
            .border_t_1()
            .border_color(theme.border)
            .font_family(FONT_MONO)
            .text_size(px(11.))
            .text_color(theme.text_muted)
            .child(status_left)
            .child(status_right);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_app)
            .font_family(FONT_UI)
            .child(topbar)
            .child(body)
            .child(statusbar)
    }
}
