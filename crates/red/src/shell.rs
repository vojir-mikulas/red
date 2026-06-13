//! The connected shell: top bar · nested resizable split (schema | editor /
//! results) · status bar. The panes are the schema tree, the SQL editor, and the
//! result grid. The split sizes are caller-owned state on [`ActiveConn`].

use flint::prelude::*;
use gpui::{div, prelude::*, px, Axis, Context, Window, WindowControlArea};

/// Left inset of the top bar. On macOS it clears the seamless traffic lights
/// overlapping this strip and leaves a little breathing room between them and
/// the connection switcher; elsewhere the native caption bar is separate, so
/// only normal padding is needed. Mirrors Nyx.
#[cfg(target_os = "macos")]
const TITLEBAR_LEFT_INSET: f32 = 88.;
#[cfg(not(target_os = "macos"))]
const TITLEBAR_LEFT_INSET: f32 = 12.;

use crate::app::{ActiveConn, AppState, Phase};

impl AppState {
    pub(crate) fn render_shell(
        &self,
        active: &ActiveConn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Owned snapshot so building the pane contents below (which borrow `cx`
        // mutably) doesn't clash with the theme tokens used throughout this fn.
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();

        // Pane contents: SQL editor · result grid. The schema explorer is built
        // below, only when the sidebar is shown.
        let editor_pane = self.render_editor(active, cx);
        // The lower pane shows the query plan (Track B4) when one is open, else the
        // result grid — the two share the slot; running a query clears the plan.
        let results_pane = if self.has_active_plan() {
            self.render_plan(active, cx).into_any_element()
        } else {
            self.render_result(active, window, cx).into_any_element()
        };

        let config = &active.config;

        // --- top bar ---
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
            .text_size(theme.scale(11.5))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| s.border_color(theme.red).text_color(theme.red))
            .child(crate::icons::icon(
                "power",
                theme.scale(13.),
                theme.text_muted,
            ))
            .child("Disconnect")
            .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx)));

        // Settings gear lives in the top bar (mirrors the welcome screen's
        // top-right placement) rather than the status strip.
        let settings_gear = IconButton::new(
            "shell-settings",
            crate::icons::icon("settings", theme.scale(16.), theme.text_muted),
        )
        .size(IconButtonSize::Sm)
        .tooltip("Settings  ⌘,")
        .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)));

        let topbar_right = div()
            .flex()
            .items_center()
            .gap_2()
            .child(disconnect)
            .child(settings_gear);

        // The top bar doubles as the window drag region (seamless traffic lights
        // sit in the left inset); interactive children keep their own hitboxes.
        let topbar = div()
            .id("topbar")
            .window_control_area(WindowControlArea::Drag)
            // Double-clicking the drag region performs the native titlebar action
            // (zoom/minimize per System Settings on macOS); other platforms zoom.
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    #[cfg(target_os = "macos")]
                    window.titlebar_double_click();
                    #[cfg(not(target_os = "macos"))]
                    window.zoom_window();
                }
            })
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
            // The connection switcher sits on the left, right of the traffic
            // lights (Zed's project-switcher slot).
            .child(self.switcher.clone())
            // Spacer keeps the disconnect control flush right.
            .child(div().flex_1())
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

        // When collapsed, the schema pane (and its resize split) drop out entirely
        // and the editor/results fill the width; the status-bar toggle brings it
        // back, restoring the retained `sidebar_w`.
        let body = if active.sidebar_collapsed {
            div().flex_1().min_h(px(0.)).child(inner)
        } else {
            let schema_pane = self.render_schema(active, window, cx);
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            let outer = SplitPane::new("shell-split-h", Axis::Horizontal)
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
                .second(inner);
            div().flex_1().min_h(px(0.)).child(outer)
        };

        // --- status bar: endpoint · db · read-only | rows · cols · UTF-8 · SQL ·
        // engine — the design's information-dense bottom strip ---
        let counts = active.active_result().and_then(|g| g.status_counts());

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
                        d.child(crate::icons::icon("lock", theme.scale(11.), theme.yellow))
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
            );

        // Sidebar collapse toggle, pinned to the far-left of the status bar so it
        // stays reachable whether the schema pane is shown or hidden.
        let sidebar_toggle = div()
            .id("toggle-sidebar")
            .mr_1()
            .flex()
            .items_center()
            .justify_center()
            .size(px(20.))
            .rounded(px(4.))
            .cursor_pointer()
            .tooltip(Tooltip::text("Toggle sidebar  ⌘B"))
            .hover(|s| s.bg(theme.bg_elevated))
            .child(crate::icons::icon(
                if active.sidebar_collapsed {
                    "panel-left-open"
                } else {
                    "panel-left-close"
                },
                theme.scale(14.),
                theme.text_muted,
            ))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)));

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
            .font_family(theme.font_family.clone())
            .text_size(theme.scale(11.))
            .text_color(theme.text_muted)
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(sidebar_toggle)
                    .child(status_left),
            )
            .child(status_right);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_app)
            .font_family(theme.font_family.clone())
            .child(topbar)
            .child(body)
            .child(statusbar)
    }
}
