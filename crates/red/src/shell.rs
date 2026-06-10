// SPDX-License-Identifier: GPL-3.0-or-later

//! The connected shell: top bar · nested resizable split (schema | editor /
//! results) · status bar. The pane contents are live: schema tree (M3), SQL
//! editor (M4), interim result grid (M3, real grid in M5). The split sizes are
//! caller-owned state on [`ActiveConn`].

use flint::prelude::*;
use gpui::{div, prelude::*, px, Axis, Context, WindowControlArea};

/// Left inset of the top bar. On macOS it clears the seamless traffic lights
/// overlapping this strip; elsewhere the native caption bar is separate, so only
/// normal padding is needed. Mirrors Nyx.
#[cfg(target_os = "macos")]
const TITLEBAR_LEFT_INSET: f32 = 72.;
#[cfg(not(target_os = "macos"))]
const TITLEBAR_LEFT_INSET: f32 = 12.;

use crate::app::{ActiveConn, AppState, Phase};
use crate::assets::{FONT_MONO, FONT_UI};

impl AppState {
    pub(crate) fn render_shell(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Owned snapshot so building the pane contents below (which borrow `cx`
        // mutably) doesn't clash with the theme tokens used throughout this fn.
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();

        // Live pane contents: schema explorer (M3) · SQL editor (M4) · interim
        // preview grid (M3, replaced by the real grid in M5).
        let schema_pane = self.render_schema(active, cx);
        let editor_pane = self.render_editor(active, cx);
        let results_pane = self.render_result(active, cx);

        let config = &active.config;

        // --- top bar ---
        // ⌘K palette is out of v0.1 scope — an inert search pill (styled to match
        // the design) with the keyboard hint, so the chrome reads complete.
        let kbd = div()
            .px_1p5()
            .rounded(px(4.))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_soft)
            .font_family(FONT_MONO)
            .text_size(px(10.))
            .text_color(theme.text_muted)
            .child("⌘K");
        let omni = div()
            .w(px(440.))
            .h(px(24.))
            .px_2p5()
            .rounded(px(6.))
            .bg(theme.bg_input)
            .border_1()
            .border_color(theme.border_soft)
            .flex()
            .items_center()
            .gap_2()
            .text_size(px(12.))
            .text_color(theme.text_faint)
            .child(crate::icons::icon("search", px(13.), theme.text_dim))
            .child(div().flex_1().child("Search tables and commands"))
            .child(kbd);

        let disconnect = div()
            .id("disconnect")
            .flex()
            .items_center()
            .gap_1p5()
            .h(px(24.))
            .px_2p5()
            .rounded(px(6.))
            .border_1()
            .border_color(theme.border_soft)
            .text_size(px(11.5))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| s.border_color(theme.red).text_color(theme.red))
            .child(crate::icons::icon("power", px(13.), theme.text_muted))
            .child("Disconnect")
            .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx)));

        let topbar_right = div().flex().items_center().child(disconnect);

        // The top bar doubles as the window drag region (seamless traffic lights
        // sit in the left inset); interactive children keep their own hitboxes.
        let topbar = div()
            .id("topbar")
            .window_control_area(WindowControlArea::Drag)
            .flex_shrink_0()
            .h(px(38.))
            .flex()
            .items_center()
            .gap_3()
            .pl(px(TITLEBAR_LEFT_INSET))
            .pr_3()
            .bg(theme.bg_panel)
            .border_b_1()
            .border_color(theme.border)
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
                .first(editor_pane)
                .second(results_pane)
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
                .first(schema_pane)
                .second(inner)
        };

        let body = div().flex_1().min_h(px(0.)).child(outer);

        // --- status bar: endpoint · db · read-only | rows · cols · UTF-8 · SQL ·
        // engine · theme — the design's information-dense bottom strip ---
        let counts = active.active().result.as_ref().and_then(|g| g.status_counts());

        let status_left = div()
            .flex()
            .items_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .px_2()
                    .child(div().size(px(6.)).rounded_full().bg(theme.green))
                    .child(config.display_target()),
            )
            .child(div().px_2().child(config.name.clone()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .text_color(if config.read_only {
                        theme.yellow
                    } else {
                        theme.text_muted
                    })
                    .when(config.read_only, |d| {
                        d.child(crate::icons::icon("lock", px(11.), theme.yellow))
                    })
                    .child(if config.read_only {
                        "Read-only"
                    } else {
                        "Read/Write"
                    }),
            );

        let status_right = div()
            .flex()
            .items_center()
            .when_some(counts, |row, (rows, cols)| {
                row.child(
                    div()
                        .px_2()
                        .text_color(theme.text)
                        .child(format!("{} rows", crate::result::group_digits(rows))),
                )
                .child(div().px_2().child(format!("{cols} columns")))
            })
            .child(div().px_2().child("UTF-8"))
            .child(div().px_2().child("SQL"))
            .child(
                div()
                    .px_2()
                    .child(format!("{} {}", config.kind, active.version)),
            )
            .child(
                div()
                    .id("status-settings")
                    .flex()
                    .items_center()
                    .px_2()
                    .cursor_pointer()
                    .text_color(theme.text_muted)
                    .hover(|s| s.text_color(theme.text))
                    .child(crate::icons::icon("settings", px(13.), theme.text_muted))
                    .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx))),
            );

        let statusbar = div()
            .flex_shrink_0()
            .h(px(25.))
            .flex()
            .items_center()
            .justify_between()
            .px_1()
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
