//! The connected shell: top bar · nested resizable split (schema | editor /
//! results) · status bar. The panes are the schema tree, the SQL editor, and the
//! result grid. The split sizes are caller-owned state on [`ActiveConn`].

use flint::prelude::*;
use gpui::{div, prelude::*, px, Axis, Context, MouseButton, Window};

/// Left inset of the top bar. On macOS it clears the seamless traffic lights
/// overlapping this strip and leaves a little breathing room between them and
/// the connection switcher; elsewhere the native caption bar is separate, so
/// only normal padding is needed. Mirrors Nyx.
#[cfg(target_os = "macos")]
const TITLEBAR_LEFT_INSET: f32 = 88.;
#[cfg(not(target_os = "macos"))]
const TITLEBAR_LEFT_INSET: f32 = 12.;

use crate::app::{ActiveConn, AppState, Phase, SplitHalf};

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
            .border_color(theme.border)
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
        .tooltip(crate::keymap::localize_hint("Settings  ⌘,"))
        .a11y_label("Settings")
        .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)));

        // The self-update pill ("Downloading…" / "Restart to update") sits to the
        // left of the disconnect + settings controls so it never covers them.
        let topbar_right = div()
            .flex()
            .items_center()
            .gap_2()
            .children(self.render_update_pill(cx))
            .child(disconnect)
            .child(settings_gear)
            // On a client-decorated window (Linux/Wayland) our own min/max/close
            // buttons live here; `None` on macOS/Windows where the OS draws them.
            .children(crate::window_chrome::window_controls(window, &theme));

        // The top bar doubles as the window drag region (seamless traffic lights
        // sit in the left inset); interactive children keep their own hitboxes.
        // `draggable` wires the move grab (macOS uses the hit-test; Linux uses an
        // explicit `start_window_move`) and the double-click zoom.
        let topbar = crate::window_chrome::draggable(div().id("topbar"), window, view.clone())
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

        // --- work area: schema | (one or two side-by-side editor/result panes) ---
        // A single pane normally; when `active.split` is set, two halves in a
        // horizontal split (see `render_work_body`).
        let inner = self.render_work_body(active, window, cx);

        // Two independent left side-panels, History (leftmost) then Schema, each
        // closable and separately resizable (Zed's multi-panel left dock). Each
        // wraps the rest in a leading-sized horizontal split; closed, it drops out
        // and the next pane fills the space. The status-bar toggles bring a panel
        // back, restoring its retained width. `workspace` is the bare, self-sizing
        // (`size_full`) result; the `flex_1` fill wrapper is applied below, *after*
        // deciding whether the assistant dock wraps it. (Wrapping a `flex_1` element
        // inside the dock's non-flex pane would collapse it: the pane stretches a
        // `size_full` child but doesn't grow a `flex_1` one.)
        let show_history = active.history_open;
        let show_schema = !active.sidebar_collapsed;
        let show_columns = active.columns_open;

        // Innermost-left column boundary: Columns (inline FK expansion) | (editor /
        // results), closest to the work area, since it's contextual to the result.
        let with_columns = if show_columns {
            let columns_pane = self.render_columns_panel(active, cx);
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            SplitPane::new("shell-split-columns", Axis::Horizontal)
                .size(active.columns_w)
                .gutter(px(1.))
                .drag(active.columns_drag)
                .min_first(px(180.))
                .max_first(px(480.))
                .on_drag_start(move |anchor, _, cx| {
                    start
                        .update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                a.columns_drag = Some(anchor);
                            }
                            cx.notify();
                        })
                        .ok();
                })
                .on_resize(move |size, _, cx| {
                    resize
                        .update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                a.columns_w = size;
                            }
                            cx.notify();
                        })
                        .ok();
                })
                .on_drag_end(move |_, cx| {
                    end.update(cx, |this, cx| {
                        if let Phase::Connected(a) = &mut this.phase {
                            a.columns_drag = None;
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .first(columns_pane)
                .second(inner)
                .into_any_element()
        } else {
            inner.into_any_element()
        };

        // Innermost column boundary: Schema | (columns | editor / results).
        let with_schema = if show_schema {
            let schema_pane = self.render_schema(active, window, cx);
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            SplitPane::new("shell-split-schema", Axis::Horizontal)
                .size(active.sidebar_w)
                .gutter(px(1.))
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
                .second(with_columns)
                .into_any_element()
        } else {
            with_columns
        };

        // Outermost column boundary: History | (schema | editor / results).
        let workspace = if show_history {
            let history_pane = self.render_history(active, cx);
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            SplitPane::new("shell-split-history", Axis::Horizontal)
                .size(active.history_w)
                .gutter(px(1.))
                .drag(active.history_drag)
                .min_first(px(180.))
                .max_first(px(480.))
                .on_drag_start(move |anchor, _, cx| {
                    start
                        .update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                a.history_drag = Some(anchor);
                            }
                            cx.notify();
                        })
                        .ok();
                })
                .on_resize(move |size, _, cx| {
                    resize
                        .update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                a.history_w = size;
                            }
                            cx.notify();
                        })
                        .ok();
                })
                .on_drag_end(move |_, cx| {
                    end.update(cx, |this, cx| {
                        if let Phase::Connected(a) = &mut this.phase {
                            a.history_drag = None;
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .first(history_pane)
                .second(with_schema)
                .into_any_element()
        } else {
            with_schema
        };

        // With the assistant open, dock it to the right of the whole workspace via
        // a resizable split (same shape as the inspector dock, one level up). Width
        // is app-owned (`assistant_w`), so it survives close/reopen.
        let body = if self.assistant.is_some() {
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            let panel = self.render_assistant(cx);
            div().flex_1().min_h(px(0.)).child(
                SplitPane::new("shell-split-assistant", Axis::Horizontal)
                    .sized(SplitSide::Trailing)
                    .size(self.assistant_w)
                    .gutter(px(1.))
                    .drag(self.assistant_drag)
                    .min_first(px(320.))
                    .max_first(px(760.))
                    .on_drag_start(move |anchor, _, cx| {
                        start
                            .update(cx, |this, cx| {
                                this.assistant_drag = Some(anchor);
                                cx.notify();
                            })
                            .ok();
                    })
                    .on_resize(move |size, _, cx| {
                        resize
                            .update(cx, |this, cx| {
                                this.assistant_w = size;
                                cx.notify();
                            })
                            .ok();
                    })
                    .on_drag_end(move |_, cx| {
                        end.update(cx, |this, cx| {
                            this.assistant_drag = None;
                            cx.notify();
                        })
                        .ok();
                    })
                    .first(workspace)
                    .second(panel),
            )
        } else {
            div().flex_1().min_h(px(0.)).child(workspace)
        };

        // --- status bar: endpoint · db · read-only | rows · cols · UTF-8 · SQL ·
        // engine (the design's information-dense bottom strip) ---
        let counts = active.active_result().and_then(|g| g.status_counts());

        // Endpoint + connection name can be arbitrarily long (a deep SQLite path,
        // a verbose `user@host:port/database`). They sit in a `flex_1 min_w_0`
        // group and truncate with an ellipsis so the window can shrink without
        // shoving the right-hand status / assistant button off-screen. The dot and
        // the read-only badge stay `flex_shrink_0`; only the text gives way.
        let status_left = div()
            .flex()
            .items_center()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .gap_1p5()
                    .px_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .size(px(6.))
                            .rounded_full()
                            .bg(theme.green),
                    )
                    .child(div().min_w_0().truncate().child(config.display_target())),
            )
            .child(div().min_w_0().truncate().px_2().child(config.name.clone()))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
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

        // Schema + History dock toggles, pinned to the far-left of the status bar so
        // they stay reachable whether the dock is shown or hidden.
        let sidebar_toggle = div()
            .id("toggle-sidebar")
            .mr_1()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .size(px(20.))
            .rounded(px(4.))
            .cursor_pointer()
            .tooltip(Tooltip::text(crate::keymap::localize_hint(
                "Toggle schema  ⌘B",
            )))
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

        // History panel toggle, accent-tinted while the panel is open.
        let history_toggle = div()
            .id("toggle-history")
            .mr_1()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .size(px(20.))
            .rounded(px(4.))
            .cursor_pointer()
            .tooltip(Tooltip::text(crate::keymap::localize_hint(
                "Toggle history  ⌘Y",
            )))
            .hover(|s| s.bg(theme.bg_elevated))
            .child(crate::icons::icon(
                "history",
                theme.scale(14.),
                if active.history_open {
                    theme.accent
                } else {
                    theme.text_muted
                },
            ))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_history(cx)));

        // Columns panel toggle (inline FK expansion), accent-tinted while open.
        let columns_toggle = div()
            .id("toggle-columns")
            .mr_1()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .size(px(20.))
            .rounded(px(4.))
            .cursor_pointer()
            .tooltip(Tooltip::text(crate::keymap::localize_hint(
                "Toggle reference columns  ⇧⌘C",
            )))
            .hover(|s| s.bg(theme.bg_elevated))
            .child(crate::icons::icon(
                "columns",
                theme.scale(14.),
                if active.columns_open {
                    theme.accent
                } else {
                    theme.text_muted
                },
            ))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_columns_panel(cx)));

        // Assistant toggle, pinned to the far-right of the status bar (mirrors the
        // schema sidebar toggle on the left). Accent-tinted while the panel is open.
        // Hidden entirely when the assistant is disabled for this connection (the
        // M-S7 kill switch): no entry point, not just a no-op button.
        let assistant_enabled = self.ai_enabled();
        let assistant_open = self.assistant.is_some();
        let assistant_toggle = div()
            .id("toggle-assistant")
            .ml_1()
            .flex()
            .items_center()
            .justify_center()
            .size(px(20.))
            .rounded(px(4.))
            .cursor_pointer()
            .tooltip(Tooltip::text(crate::keymap::localize_hint(
                "Toggle agent  ⌘L",
            )))
            .hover(|s| s.bg(theme.bg_elevated))
            .child(crate::icons::icon(
                "sparkles",
                theme.scale(14.),
                if assistant_open {
                    theme.accent
                } else {
                    theme.text_muted
                },
            ))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_assistant(window, cx)));

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
                // The left group flexes and clips; its children truncate so the
                // right group is never pushed past the window edge.
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .overflow_hidden()
                    .child(history_toggle)
                    .child(sidebar_toggle)
                    .child(columns_toggle)
                    .child(status_left),
            )
            .child(
                // Counts + assistant toggle stay fixed-width and always visible.
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .child(status_right)
                    .children(assistant_enabled.then_some(assistant_toggle)),
            );

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

    /// The work area right of the schema dock: a single editor/result pane, or,
    /// when `active.split` is set, two halves in a resizable horizontal split. Each
    /// half is a full editor-over-result pane for its own tab (see [`Self::render_half`]).
    fn render_work_body(
        &self,
        active: &ActiveConn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(s) = active.split.as_ref() else {
            // Single pane: the ordinary layout, behaviourally unchanged.
            return self.render_half(
                active,
                SplitHalf::Primary,
                active.active_tab,
                true,
                window,
                cx,
            );
        };

        let (focus, width, drag) = (s.focus, s.width, s.drag);
        let primary_focused = focus == SplitHalf::Primary;
        // Each half renders its pane's active tab (its strip shows only its own tabs).
        let primary_tab = active
            .pane_active(SplitHalf::Primary)
            .unwrap_or(active.active_tab);
        let secondary_tab = active
            .pane_active(SplitHalf::Secondary)
            .unwrap_or(s.secondary);
        let first = self.render_half(
            active,
            SplitHalf::Primary,
            primary_tab,
            primary_focused,
            window,
            cx,
        );
        let second = self.render_half(
            active,
            SplitHalf::Secondary,
            secondary_tab,
            !primary_focused,
            window,
            cx,
        );

        let view = cx.entity().downgrade();
        let start = view.clone();
        let resize = view.clone();
        let end = view.clone();
        div()
            .size_full()
            .child(
                SplitPane::new("shell-split-halves", Axis::Horizontal)
                    .size(width)
                    .gutter(px(1.))
                    .drag(drag)
                    .min_first(px(320.))
                    .on_drag_start(move |anchor, _, cx| {
                        start
                            .update(cx, |this, cx| {
                                if let Phase::Connected(a) = &mut this.phase {
                                    if let Some(s) = &mut a.split {
                                        s.drag = Some(anchor);
                                    }
                                }
                                cx.notify();
                            })
                            .ok();
                    })
                    .on_resize(move |size, _, cx| {
                        resize
                            .update(cx, |this, cx| {
                                if let Phase::Connected(a) = &mut this.phase {
                                    if let Some(s) = &mut a.split {
                                        s.width = size;
                                    }
                                }
                                cx.notify();
                            })
                            .ok();
                    })
                    .on_drag_end(move |_, cx| {
                        end.update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                if let Some(s) = &mut a.split {
                                    s.drag = None;
                                }
                            }
                            cx.notify();
                        })
                        .ok();
                    })
                    .first(first)
                    .second(second),
            )
            .into_any_element()
    }

    /// One split half: the tab `tab_idx` rendered as the editor-over-result vertical
    /// split, wrapped so a click anywhere in it focuses the half (`half`) and, while
    /// split, an accent outline marks the focused one. The editor/result ratio is
    /// shared between halves (both read `editor_h`).
    fn render_half(
        &self,
        active: &ActiveConn,
        half: SplitHalf,
        tab_idx: usize,
        is_focused: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let is_split = active.split.is_some();
        let editor_pane = self.render_editor(active, tab_idx, half, is_focused, cx);
        let results_pane = self.render_results_slot(active, tab_idx, half, is_focused, window, cx);

        let view = cx.entity().downgrade();
        let start = view.clone();
        let resize = view.clone();
        let end = view.clone();
        let vsplit = SplitPane::new(format!("shell-split-v-{}", half.index()), Axis::Vertical)
            .size(active.editor_h)
            .gutter(px(1.))
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
            .second(results_pane);

        // The unique wrapper id scopes the two halves' child element ids apart; the
        // mouse-down aims run/export/filter at whichever half was clicked.
        let accent = theme.accent;
        let border = theme.border;
        let drop_view = view.clone();
        div()
            .id(("split-half", half.index()))
            .size_full()
            .flex()
            .flex_col()
            .when(is_split, move |d| {
                d.border_1()
                    .border_color(if is_focused { accent } else { border })
                    // A tab dragged from either strip and dropped onto this half's
                    // body moves (or swaps) it here. The strips handle their own
                    // drops (reorder) and stop propagation, so this fires only for
                    // the editor/result area. `drag_over` tints the hovered half.
                    .drag_over::<crate::editor::TabDrag>(move |s, _, _, _| {
                        s.border_color(accent).bg(accent.opacity(0.06))
                    })
                    .on_drop::<crate::editor::TabDrag>(move |drag, _window, cx| {
                        let from = drag.0;
                        drop_view
                            .update(cx, |this, cx| this.move_tab_to_half(from, half, cx))
                            .ok();
                    })
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.set_split_focus(half, cx)),
            )
            .child(vsplit)
            .into_any_element()
    }

    /// The lower pane for tab `tab_idx`: its query plan (Track B4) when one is open,
    /// else the result grid; both share the slot. Picks per-tab (not per-focus) so
    /// each half shows its own tab's view.
    fn render_results_slot(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        half: SplitHalf,
        is_focused: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let tab = active.tabs.get(tab_idx);
        if tab.is_some_and(|t| t.plan.is_some()) {
            self.render_plan(active, tab_idx, cx)
        } else {
            self.render_result(active, tab_idx, half, is_focused, window, cx)
                .into_any_element()
        }
    }
}
