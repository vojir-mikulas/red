//! The connected shell: top bar · nested resizable split (schema | editor /
//! results) · status bar. The panes are the schema tree, the SQL editor, and the
//! result grid. The split sizes are caller-owned state on [`ActiveConn`].

use flint::prelude::*;
use flint::Theme;
use gpui::{div, prelude::*, px, Axis, Context, MouseButton, SharedString, WeakEntity, Window};

/// Left inset of the top bar. On macOS it clears the seamless traffic lights
/// overlapping this strip and leaves a little breathing room between them and
/// the connection switcher; elsewhere the native caption bar is separate, so
/// only normal padding is needed. Mirrors Nyx.
#[cfg(target_os = "macos")]
pub(crate) const TITLEBAR_LEFT_INSET: f32 = 88.;
#[cfg(not(target_os = "macos"))]
pub(crate) const TITLEBAR_LEFT_INSET: f32 = 12.;

use crate::app::{ActiveConn, AppState, Phase, SplitHalf, TabWorkspace};

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

        let topbar = self.render_topbar(&theme, &view, window, cx);

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

    /// The top bar (connection switcher · self-update pill · disconnect ·
    /// settings gear · window controls), shared by [`Self::render_shell`] (the
    /// SQL workspace) and [`Self::render_redis_shell`] (the KV placeholder) —
    /// it's engine-agnostic chrome, not part of the SQL-specific work area.
    fn render_topbar(
        &self,
        theme: &Theme,
        view: &WeakEntity<Self>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
            .children(crate::window_chrome::window_controls(window, theme));

        // The top bar doubles as the window drag region (seamless traffic lights
        // sit in the left inset); interactive children keep their own hitboxes.
        // `draggable` wires the move grab (macOS uses the hit-test; Linux uses an
        // explicit `start_window_move`) and the double-click zoom.
        crate::window_chrome::draggable(div().id("topbar"), window, view.clone())
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
            .child(topbar_right)
    }

    /// The Redis work body: one pane, or two side-by-side halves when split.
    fn render_kv_body(
        &self,
        active: &ActiveConn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(v) = active.kv_view.as_ref() else {
            return div().flex_1().into_any_element();
        };
        let Some(s) = v.split.as_ref() else {
            // Derive the body's tab from `pane_active` (the same source the strip
            // highlight uses), not `active_tab` directly, so an `active_tab` that
            // is momentarily out of range can't render an empty body under a
            // highlighted strip tab.
            let idx = v.pane_active(SplitHalf::Primary).unwrap_or(v.active_tab);
            return self.render_kv_half(active, SplitHalf::Primary, idx, true, window, cx);
        };
        let (focus, width, drag) = (s.focus, s.width, s.drag);
        let primary_focused = focus == SplitHalf::Primary;
        let primary_tab = v.pane_active(SplitHalf::Primary).unwrap_or(v.active_tab);
        let secondary_tab = v.pane_active(SplitHalf::Secondary).unwrap_or(s.secondary);
        let first = self.render_kv_half(
            active,
            SplitHalf::Primary,
            primary_tab,
            primary_focused,
            window,
            cx,
        );
        let second = self.render_kv_half(
            active,
            SplitHalf::Secondary,
            secondary_tab,
            !primary_focused,
            window,
            cx,
        );
        let start = cx.entity().downgrade();
        let resize = start.clone();
        let end = start.clone();
        div()
            .size_full()
            .child(
                SplitPane::new("kv-split-halves", Axis::Horizontal)
                    .size(width)
                    .gutter(px(1.))
                    .drag(drag)
                    .min_first(px(320.))
                    .on_drag_start(move |anchor, _, cx| {
                        start
                            .update(cx, |this, cx| {
                                if let Phase::Connected(a) = &mut this.phase {
                                    if let Some(v) = &mut a.kv_view {
                                        if let Some(s) = &mut v.split {
                                            s.drag = Some(anchor);
                                        }
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
                                    if let Some(v) = &mut a.kv_view {
                                        if let Some(s) = &mut v.split {
                                            s.width = size;
                                        }
                                    }
                                }
                                cx.notify();
                            })
                            .ok();
                    })
                    .on_drag_end(move |_, cx| {
                        end.update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                if let Some(v) = &mut a.kv_view {
                                    if let Some(s) = &mut v.split {
                                        s.drag = None;
                                    }
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

    /// One split half: its own tab strip (only this half's tabs, styled 1:1 with
    /// the SQL editor's strip) over the active tab's panel body. A mouse-down
    /// anywhere in the half focuses it, so buttons/inputs act on the half the
    /// user just touched (the focus-aware `active_*` routing; see
    /// docs/plans/redis-workflow-parity.md).
    fn render_kv_half(
        &self,
        active: &ActiveConn,
        half: SplitHalf,
        tab_idx: usize,
        focused: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::editor::{TabDrag, TabDragPreview};
        use crate::kvbrowse::KvPanel;
        let theme = cx.theme().clone();
        // Snapshot the same tokens/sizes the SQL strip uses, so the two tab bars
        // are pixel-identical (see `render_editor`).
        let (bg_app, bg_panel, bg_elevated, bg_hover) = (
            theme.bg_app,
            theme.bg_panel,
            theme.bg_elevated,
            theme.bg_hover,
        );
        let border = theme.border;
        let (text, muted, faint) = (theme.text, theme.text_muted, theme.text_faint);
        let accent = theme.accent;
        let icon_close = theme.scale(9.);
        let ui_family = theme.font_family.clone();
        let size_12 = theme.scale(12.);
        let view = cx.entity().downgrade();
        let session = active.session;
        let Some(v) = active.kv_view.as_ref() else {
            return div().flex_1().into_any_element();
        };
        let is_split = v.split.is_some();

        let active_idx = v.pane_active(half);
        let pane_indices = v.pane_tab_indices(half);
        let last_in_pane = pane_indices.last().copied();
        let drop_target = v.tab_drop_target;
        let dragging = cx.has_active_drag();
        let (pinned_indices, unpinned_indices): (Vec<usize>, Vec<usize>) = pane_indices
            .iter()
            .copied()
            .partition(|&i| v.tabs[i].pinned);

        let render_tab = |i: usize| {
            let t = &v.tabs[i];
            let is_active = Some(i) == active_idx;
            let pinned = t.pinned;
            let id = t.id;
            let (tab_bg, tab_text) = if is_active {
                (bg_app, text)
            } else {
                (bg_panel, muted)
            };
            let drag_title: SharedString = t.title.clone().into();
            let move_view = view.clone();
            let drop_view = view.clone();
            let group = SharedString::from(format!("kv-tab-{i}"));
            let bar_before = dragging && drop_target == Some(i);
            let bar_after = dragging && Some(i) == last_in_pane && drop_target == Some(i + 1);
            div()
                .id(("kv-tab", i))
                .group(group.clone())
                .relative()
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .min_w(px(96.))
                .max_w(px(200.))
                .px(px(23.))
                .bg(tab_bg)
                .border_r_1()
                .border_color(border)
                .cursor_pointer()
                .when(!is_active, |d| d.hover(|s| s.bg(bg_elevated)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.kv_set_split_focus(session, half, cx);
                    this.kv_activate_tab(session, i, cx);
                }))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _, _, cx| {
                        if !pinned {
                            this.kv_close_tab(session, i, cx);
                        }
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        let pos = event.position;
                        this.kv_open_tab_menu(session, id, pos, cx);
                    }),
                )
                .on_drag(TabDrag(i), move |_, offset, _window, cx| {
                    let title = drag_title.clone();
                    cx.new(move |_| TabDragPreview {
                        title,
                        offset,
                        bg: bg_elevated,
                        border,
                        text,
                    })
                })
                .on_drag_move::<TabDrag>(move |e, _window, cx| {
                    let b = e.bounds;
                    let p = e.event.position;
                    if p.x < b.origin.x
                        || p.x >= b.origin.x + b.size.width
                        || p.y < b.origin.y
                        || p.y >= b.origin.y + b.size.height
                    {
                        return;
                    }
                    let gap = if p.x < b.origin.x + b.size.width / 2. {
                        i
                    } else {
                        i + 1
                    };
                    move_view
                        .update(cx, |this, cx| this.kv_set_tab_drop_target(session, gap, cx))
                        .ok();
                })
                .on_drop::<TabDrag>(move |drag, _window, cx| {
                    let from = drag.0;
                    cx.stop_propagation();
                    drop_view
                        .update(cx, |this, cx| this.kv_drop_tab(session, from, half, cx))
                        .ok();
                })
                .when(bar_before, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .bottom_0()
                            .w(px(2.))
                            .bg(accent),
                    )
                })
                .when(bar_after, |d| {
                    d.child(
                        div()
                            .absolute()
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .w(px(2.))
                            .bg(accent),
                    )
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .min_w_0()
                        .when(pinned, |d| {
                            d.child(crate::icons::icon("pin", icon_close, faint))
                        })
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family(ui_family.clone())
                                .text_size(size_12)
                                .text_color(tab_text)
                                .child(t.title.clone()),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .right(px(4.))
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .invisible()
                        .group_hover(group, |s| s.visible())
                        .child(
                            div()
                                .id(("kv-tab-close", i))
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(15.))
                                .rounded(px(3.))
                                .text_color(faint)
                                .hover(|s| s.bg(bg_hover).text_color(text))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.kv_close_tab(session, i, cx);
                                }))
                                .child(crate::icons::icon("close", icon_close, faint)),
                        ),
                )
        };
        let pinned_tabs: Vec<_> = pinned_indices.iter().map(|&i| render_tab(i)).collect();
        let unpinned_tabs: Vec<_> = unpinned_indices.iter().map(|&i| render_tab(i)).collect();
        let strip_move_view = view.clone();
        let strip_drop_view = view.clone();
        let tab_viewport = div()
            .id("kv-tabstrip")
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .items_stretch()
            .overflow_x_scroll()
            .track_scroll(&v.tab_scroll)
            .on_drag_move::<TabDrag>(move |e, _window, cx| {
                let b = e.bounds;
                let p = e.event.position;
                let outside = p.y < b.origin.y || p.y >= b.origin.y + b.size.height;
                if outside {
                    strip_move_view
                        .update(cx, |this, cx| this.kv_clear_tab_drop_target(session, cx))
                        .ok();
                }
            })
            .on_drop::<TabDrag>(move |drag, _window, cx| {
                let from = drag.0;
                cx.stop_propagation();
                strip_drop_view
                    .update(cx, |this, cx| this.kv_drop_tab(session, from, half, cx))
                    .ok();
            })
            .children(unpinned_tabs);
        let pinned_strip = (!pinned_tabs.is_empty()).then(|| {
            div()
                .id("kv-tabstrip-pinned")
                .flex_shrink_0()
                .h_full()
                .flex()
                .items_stretch()
                .children(pinned_tabs)
        });
        let strip = div()
            .flex_shrink_0()
            .h(px(35.))
            .flex()
            .items_stretch()
            .bg(bg_panel)
            .border_b_1()
            .border_color(border)
            .children(pinned_strip)
            .child(tab_viewport)
            .child(
                div()
                    .id("kv-new")
                    .flex_shrink_0()
                    .w(px(34.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_l_1()
                    .border_color(border)
                    .cursor_pointer()
                    .tooltip(Tooltip::text(crate::keymap::localize_hint("New tab  ⌘T")))
                    .text_color(faint)
                    .hover(|s| s.bg(bg_elevated).text_color(text))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.kv_set_split_focus(session, half, cx);
                        this.kv_new_empty_tab(session, cx);
                    }))
                    .child(crate::icons::icon("plus", theme.scale(13.), faint)),
            );

        let panel = match v.tabs.get(tab_idx).map(|t| t.state.kind()) {
            Some(Some(KvPanel::Browse)) => self
                .render_kv_browse(active, tab_idx, window, cx)
                .into_any_element(),
            Some(Some(KvPanel::Console)) => self
                .render_kv_console(active, tab_idx, window, cx)
                .into_any_element(),
            Some(Some(KvPanel::PubSub)) => self
                .render_kv_pubsub(active, tab_idx, window, cx)
                .into_any_element(),
            Some(Some(KvPanel::Monitor)) => self
                .render_kv_monitor(active, tab_idx, window, cx)
                .into_any_element(),
            Some(Some(KvPanel::Analysis)) => self
                .render_kv_analysis(active, tab_idx, window, cx)
                .into_any_element(),
            Some(Some(KvPanel::Keyspace)) => self
                .render_kv_keyspace(active, tab_idx, window, cx)
                .into_any_element(),
            // A blank tab (`None` kind): show the type chooser in the body.
            Some(None) => self
                .render_kv_new_tab(active, tab_idx, focused, window, cx)
                .into_any_element(),
            None => div().flex_1().into_any_element(),
        };

        let focus_view = view.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .when(is_split && focused, |d| {
                d.border_1().border_color(accent.opacity(0.5))
            })
            .when(is_split && !focused, |d| d.border_1().border_color(border))
            .when(is_split, |d| {
                d.on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    focus_view
                        .update(cx, |this, cx| this.kv_set_split_focus(session, half, cx))
                        .ok();
                })
            })
            .child(strip)
            .child(div().flex_1().min_h(px(0.)).flex().child(panel))
            .into_any_element()
    }

    /// The blank-tab body: a centered chooser of the six panel kinds. Picking one
    /// converts this tab in place (see [`AppState::kv_set_tab_kind`]).
    fn render_kv_new_tab(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        focused: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use crate::kvbrowse::KV_NEW_TAB_CHOICES;
        let theme = cx.theme().clone();
        let session = active.session;
        let Some(v) = active.kv_view.as_ref() else {
            return div().flex_1();
        };
        let id = v.tabs.get(tab_idx).map(|t| t.id).unwrap_or(0);
        let selected = v.new_tab_sel.min(KV_NEW_TAB_CHOICES.len() - 1);

        // The focused half's chooser owns the keyboard: bind the shared focus
        // handle here (only one chooser ever binds it) and grab focus so the
        // digit/arrow shortcuts work the moment a blank tab opens.
        let focus = v.new_tab_focus.clone();
        if focused && !focus.is_focused(window) {
            window.focus(&focus, cx);
        }

        let cards = div()
            .flex()
            .flex_wrap()
            .justify_center()
            .gap_3()
            .max_w(px(560.))
            .children(
                KV_NEW_TAB_CHOICES
                    .iter()
                    .enumerate()
                    .map(|(i, (kind, hint))| {
                        let view = cx.entity().downgrade();
                        let kind = *kind;
                        let is_sel = focused && i == selected;
                        div()
                            .id(SharedString::from(format!("kv-choose-{}", kind.label())))
                            .w(px(168.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .p_3()
                            .rounded(px(8.))
                            .bg(if is_sel {
                                theme.bg_elevated
                            } else {
                                theme.bg_panel
                            })
                            .border_1()
                            .border_color(if is_sel { theme.accent } else { theme.border })
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.bg_elevated).border_color(theme.accent))
                            .child(
                                // Title row: name on the left, a number-shortcut badge right.
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .font_family(theme.font_family.clone())
                                            .text_size(theme.scale(12.5))
                                            .text_color(theme.text)
                                            .child(kind.label()),
                                    )
                                    .child(
                                        div()
                                            .px(px(5.))
                                            .rounded(px(4.))
                                            .bg(theme.bg_app)
                                            .border_1()
                                            .border_color(theme.border)
                                            .font_family(theme.font_family.clone())
                                            .text_size(theme.scale(10.))
                                            .text_color(theme.text_muted)
                                            .child(format!("{}", i + 1)),
                                    ),
                            )
                            .child(
                                div()
                                    .font_family(theme.font_family.clone())
                                    .text_size(theme.scale(11.))
                                    .text_color(theme.text_muted)
                                    .child(hint.to_string()),
                            )
                            .on_click(move |_, _, cx| {
                                view.update(cx, |this, cx| {
                                    this.kv_set_tab_kind(session, id, kind, cx)
                                })
                                .ok();
                            })
                    }),
            );

        let key_view = cx.entity().downgrade();
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(theme.bg_app)
            // Only the focused half's chooser binds the shared focus handle and
            // its key handler, so a split with two blank tabs never double-binds.
            .when(focused, |d| {
                d.track_focus(&focus)
                    .on_key_down(move |ev: &gpui::KeyDownEvent, _window, cx| {
                        let key = ev.keystroke.key.clone();
                        key_view
                            .update(cx, |this, cx| {
                                if this.kv_new_tab_key(session, id, &key, cx) {
                                    cx.stop_propagation();
                                }
                            })
                            .ok();
                    })
            })
            .child(
                div()
                    .font_family(theme.font_family.clone())
                    .text_size(theme.scale(13.))
                    .text_color(theme.text_muted)
                    .child("Choose what to open in this tab  ·  press 1–6 or ↵"),
            )
            .child(cards)
    }

    /// The Redis History dock (left, ⌘Y): a Keys section (recently-viewed keys,
    /// browser-history for the keyspace) over a Commands section (past console
    /// commands). Keys re-open the inspector; commands seed the console. Reuses
    /// the same `query_history` store + `relative_time` helper as the SQL dock.
    fn render_kv_history(&self, active: &ActiveConn, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let session = active.session;
        let bg_panel = theme.bg_panel;
        let border = theme.border;
        let (text, muted, faint) = (theme.text, theme.text_muted, theme.text_faint);
        let (bg_hover, bg_elevated) = (theme.bg_hover, theme.bg_elevated);
        let ui_family = theme.font_family.clone();
        let mono = theme.mono_family.clone();
        let (size_12, size_11, size_10) = (theme.scale(12.), theme.scale(11.), theme.scale(10.));
        let icon_x = theme.scale(11.);

        let commands = self.query_history.for_conn(&active.conn_id);
        #[allow(clippy::type_complexity)]
        let keys: Vec<(
            String,
            red_core::kv::KvType,
            Option<std::time::Duration>,
            u64,
        )> = active
            .kv_view
            .as_ref()
            .map(|v| {
                v.recent_keys
                    .iter()
                    .map(|r| (r.key.clone(), r.kv_type.clone(), r.ttl, r.viewed_unix))
                    .collect()
            })
            .unwrap_or_default();
        let has_keys = !keys.is_empty();
        let has_cmds = !commands.is_empty();
        let has_any = has_keys || has_cmds;

        let clear_btn = has_any.then(|| {
            div()
                .id("kv-history-clear")
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .size(px(18.))
                .rounded(px(3.))
                .cursor_pointer()
                .text_color(faint)
                .hover(|s| s.bg(bg_elevated).text_color(text))
                .tooltip(Tooltip::text("Clear history"))
                .child(crate::icons::icon("trash", icon_x, faint))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.clear_history(cx);
                    this.kv_clear_recent_keys(session, cx);
                }))
        });
        let close_btn = div()
            .id("kv-history-hide")
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .size(px(18.))
            .rounded(px(3.))
            .cursor_pointer()
            .text_color(faint)
            .hover(|s| s.bg(bg_elevated).text_color(text))
            .tooltip(Tooltip::text(crate::keymap::localize_hint(
                "Hide history  ⌘Y",
            )))
            .child(crate::icons::icon("x", icon_x, faint))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_history(cx)));
        let header = div()
            .flex_shrink_0()
            .h(px(28.))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .bg(bg_panel)
            .border_b_1()
            .border_color(border)
            .font_family(ui_family.clone())
            .text_size(size_11)
            .text_color(muted)
            .child(div().flex_1().min_w_0().truncate().child("History"))
            .children(clear_btn)
            .child(close_btn);

        // A dimmed section label between the two lists.
        let section = |label: &str| {
            div()
                .flex_shrink_0()
                .px_2()
                .pt_2()
                .pb_1()
                .font_family(ui_family.clone())
                .text_size(size_10)
                .text_color(faint)
                .child(label.to_string())
        };

        let key_rows = keys
            .into_iter()
            .enumerate()
            .map(|(i, (key, kv_type, ttl, when))| {
                let label = key.clone();
                let remove_key = key.clone();
                let type_label = kv_type.label().to_string();
                let sub = crate::history::relative_time(when);
                let mono = mono.clone();
                let group = SharedString::from(format!("kv-key-{i}"));
                div()
                    .id(("kv-key-row", i))
                    .group(group.clone())
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1p5()
                    .hover(move |s| s.bg(bg_hover))
                    .child(
                        div()
                            .id(("kv-key-open", i))
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.kv_open_recent_key(
                                    session,
                                    key.clone(),
                                    kv_type.clone(),
                                    ttl,
                                    cx,
                                );
                            }))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(mono)
                                    .text_size(size_12)
                                    .text_color(text)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .text_size(size_10)
                                    .text_color(faint)
                                    .child(type_label)
                                    .child(sub),
                            ),
                    )
                    .child(
                        div()
                            .id(("kv-key-del", i))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(16.))
                            .rounded(px(3.))
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .cursor_pointer()
                            .text_color(faint)
                            .tooltip(Tooltip::text("Remove from history"))
                            .hover(|s| s.bg(bg_elevated).text_color(text))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.kv_remove_recent_key(session, remove_key.clone(), cx);
                            }))
                            .child(crate::icons::icon("x", icon_x, faint)),
                    )
            });

        let cmd_rows = commands.into_iter().enumerate().map(|(i, entry)| {
            let cmd = entry.sql.clone();
            let id = entry.id;
            let label = crate::editor::history_label(&entry.sql);
            let sub = crate::history::relative_time(entry.ran_unix);
            let group = SharedString::from(format!("kv-cmd-{i}"));
            let mono = mono.clone();
            div()
                .id(("kv-cmd-row", i))
                .group(group.clone())
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_1p5()
                .hover(move |s| s.bg(bg_hover))
                .child(
                    div()
                        .id(("kv-cmd-load", i))
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.kv_seed_console(session, cmd.clone(), cx);
                        }))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family(mono)
                                .text_size(size_12)
                                .text_color(text)
                                .child(label),
                        )
                        .child(div().text_size(size_10).text_color(faint).child(sub)),
                )
                .child(
                    div()
                        .id(("kv-cmd-del", i))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(16.))
                        .rounded(px(3.))
                        .invisible()
                        .group_hover(group, |s| s.visible())
                        .cursor_pointer()
                        .text_color(faint)
                        .hover(|s| s.bg(bg_elevated).text_color(text))
                        .on_click(cx.listener(move |this, _, _, cx| this.delete_history(id, cx)))
                        .child(crate::icons::icon("x", icon_x, faint)),
                )
        });

        let body = if !has_any {
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .px_4()
                .text_size(size_11)
                .text_color(faint)
                .child("Nothing yet")
                .into_any_element()
        } else {
            div()
                .id("kv-history-list")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .when(has_keys, |d| d.child(section("Recently viewed keys")))
                .children(key_rows)
                .when(has_cmds, |d| d.child(section("Commands")))
                .children(cmd_rows)
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(bg_panel)
            .child(header)
            .child(body)
    }

    /// The tab right-click context menu (Pin/Unpin · Close · Move to other pane).
    fn render_kv_tab_menu(
        &self,
        active: &ActiveConn,
        id: u64,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::app::TabCloseScope;
        let session = active.session;
        // Pin state, split state, and (relative to this tab's own pane) whether
        // there are tabs to its left/right or any others to close — the same
        // flags the SQL `render_tab_menu` computes to enable/disable items.
        let (pinned, is_split, has_left, has_right, has_others) = active
            .kv_view
            .as_ref()
            .map(|v| {
                let pinned = v
                    .tabs
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.pinned)
                    .unwrap_or(false);
                let (has_left, has_right, has_others) = v
                    .tabs
                    .iter()
                    .position(|t| t.id == id)
                    .map(|idx| {
                        let siblings = v.pane_tab_indices(v.tabs[idx].pane);
                        let p = siblings.iter().position(|&i| i == idx).unwrap_or(0);
                        (p > 0, p + 1 < siblings.len(), siblings.len() > 1)
                    })
                    .unwrap_or((false, false, false));
                (pinned, v.split.is_some(), has_left, has_right, has_others)
            })
            .unwrap_or((false, false, false, false, false));
        let closable = active
            .kv_view
            .as_ref()
            .map(|v| v.tabs.len() > 1)
            .unwrap_or(false);
        let move_label = if is_split {
            "Move to other pane"
        } else {
            "Open in split"
        };
        let menu = ContextMenu::new("kv-tab-context-menu")
            .item(
                ContextMenuItem::new("kv-tab-pin", if pinned { "Unpin tab" } else { "Pin tab" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.kv_toggle_tab_pin(session, id, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("kv-tab-move", move_label).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.kv_move_tab_to_other_half(session, id, cx);
                    },
                )),
            )
            .separator()
            .item(
                ContextMenuItem::new("kv-tab-close", "Close")
                    .disabled(!closable)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.kv_close_tab_by_id(session, id, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("kv-tab-close-others", "Close Others")
                    .disabled(!has_others)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.kv_close_tab_group(session, id, TabCloseScope::Others, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("kv-tab-close-left", "Close Left")
                    .disabled(!has_left)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.kv_close_tab_group(session, id, TabCloseScope::Left, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("kv-tab-close-right", "Close Right")
                    .disabled(!has_right)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.kv_close_tab_group(session, id, TabCloseScope::Right, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("kv-tab-close-all", "Close All")
                    .disabled(!closable)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.kv_close_tab_group(session, id, TabCloseScope::All, cx);
                    })),
            );
        // A full-bleed catcher dismisses the menu on any outside click.
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.kv_close_tab_menu(session, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| this.kv_close_tab_menu(session, cx)),
            )
            // `occlude()` keeps a click on the menu from reaching the dismiss
            // catcher behind it — without it the catcher's mouse-down closes the
            // menu on *press*, so the item's on_click never fires on release.
            .child(
                div()
                    .occlude()
                    .absolute()
                    .left(pos.x)
                    .top(pos.y)
                    .child(menu),
            )
            .into_any_element()
    }

    /// The right-click context menu for a key row (live browse or biggest-keys
    /// sample). Its actions reuse the inspector's existing edit flows — Rename /
    /// Set TTL open the inspector into that inline editor, Delete raises its
    /// confirm bar — so the menu is a shortcut, not a second implementation.
    /// Write items are disabled (not hidden) on a read-only connection, matching
    /// the tab menu's disabled-item convention.
    fn render_kv_key_menu(
        &self,
        active: &ActiveConn,
        km: &crate::kvbrowse::KeyMenu,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::kvbrowse::KeyMenuEdit;
        let session = active.session;
        let writable = !active.config.read_only;
        let key = km.key.clone();
        let kv_type = km.kv_type.clone();
        let ttl = km.ttl;
        let pos = km.pos;
        let favorited = self.redis_key_meta.is_favorite(&active.conn_id, &key);

        let mut menu = ContextMenu::new("kv-key-context-menu")
            .item(
                ContextMenuItem::new("kv-key-open", "Open").on_click(cx.listener({
                    let key = key.clone();
                    let kv_type = kv_type.clone();
                    move |this, _, _, cx| {
                        this.kv_close_key_menu(session, cx);
                        this.kv_open_inspector(session, key.clone(), ttl, kv_type.clone(), cx);
                    }
                })),
            )
            .item(
                ContextMenuItem::new("kv-key-copy", "Copy key name").on_click(cx.listener({
                    let key = key.clone();
                    move |this, _, _, cx| this.kv_copy_key_name(session, key.clone(), cx)
                })),
            )
            .item(
                ContextMenuItem::new("kv-key-console", "Open in Console").on_click(cx.listener({
                    let key = key.clone();
                    let kv_type = kv_type.clone();
                    move |this, _, _, cx| {
                        this.kv_key_menu_open_console(session, kv_type.clone(), key.clone(), cx)
                    }
                })),
            )
            .separator()
            .item(
                ContextMenuItem::new(
                    "kv-key-favorite",
                    if favorited {
                        "★ Unfavorite"
                    } else {
                        "☆ Favorite"
                    },
                )
                .on_click(cx.listener({
                    let key = key.clone();
                    move |this, _, _, cx| this.kv_toggle_key_favorite(session, key.clone(), cx)
                })),
            )
            .item(
                ContextMenuItem::new("kv-key-annotate", "Note & tags…").on_click(cx.listener({
                    let key = key.clone();
                    move |this, _, _, cx| this.kv_open_annotations(session, key.clone(), cx)
                })),
            )
            .separator();

        // Cross-server copy: one item per other open, writable Redis connection
        // (DUMP here → RESTORE ... REPLACE there). Omitted when there's nowhere
        // to copy to.
        for (i, (target, name)) in self.kv_copy_targets(session).into_iter().enumerate() {
            let key = key.clone();
            let id = gpui::SharedString::from(format!("kv-key-copyto-{i}"));
            menu = menu.item(
                ContextMenuItem::new(id, format!("Copy to “{name}”")).on_click(cx.listener(
                    move |this, _, _, cx| this.kv_copy_key_to(session, key.clone(), target, cx),
                )),
            );
        }

        let menu = menu
            .item(
                ContextMenuItem::new("kv-key-rename", "Rename…")
                    .disabled(!writable)
                    .on_click(cx.listener({
                        let key = key.clone();
                        let kv_type = kv_type.clone();
                        move |this, _, _, cx| {
                            this.kv_key_menu_edit(
                                session,
                                key.clone(),
                                kv_type.clone(),
                                ttl,
                                KeyMenuEdit::Rename,
                                cx,
                            )
                        }
                    })),
            )
            .item(
                ContextMenuItem::new("kv-key-ttl", "Set TTL…")
                    .disabled(!writable)
                    .on_click(cx.listener({
                        let key = key.clone();
                        let kv_type = kv_type.clone();
                        move |this, _, _, cx| {
                            this.kv_key_menu_edit(
                                session,
                                key.clone(),
                                kv_type.clone(),
                                ttl,
                                KeyMenuEdit::Ttl,
                                cx,
                            )
                        }
                    })),
            )
            .separator()
            .item(
                ContextMenuItem::new("kv-key-delete", "Delete")
                    .danger()
                    .disabled(!writable)
                    .on_click(cx.listener({
                        let key = key.clone();
                        move |this, _, _, cx| this.kv_request_delete_key(session, key.clone(), cx)
                    })),
            );
        // A full-bleed catcher dismisses the menu on any outside click.
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.kv_close_key_menu(session, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| this.kv_close_key_menu(session, cx)),
            )
            // `occlude()` keeps a click on the menu from reaching the dismiss
            // catcher behind it — without it the catcher's mouse-down closes the
            // menu on *press*, so the item's on_click never fires on release.
            .child(
                div()
                    .occlude()
                    .absolute()
                    .left(pos.x)
                    .top(pos.y)
                    .child(menu),
            )
            .into_any_element()
    }

    /// The "Note & tags" annotation editor popover (see
    /// [`AppState::kv_open_annotations`]): a centered card with a note field and
    /// a comma-separated tags field, Save persists to the key-meta store.
    fn render_kv_annotate(
        &self,
        active: &ActiveConn,
        ann: &crate::kvbrowse::AnnotateState,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let session = active.session;
        let view = cx.entity().downgrade();
        let (save_view, cancel_view) = (view.clone(), view.clone());

        let card = div()
            .occlude()
            .w(px(340.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_lg()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(format!("Note & tags · {}", ann.key)),
            )
            .child(ann.note.clone())
            .child(ann.tags.clone())
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("kv-annotate-save", "Save")
                            .variant(ButtonVariant::Primary)
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                save_view
                                    .update(cx, |this, cx| this.kv_submit_annotations(session, cx))
                                    .ok();
                            }),
                    )
                    .child(
                        Button::new("kv-annotate-cancel", "Cancel")
                            .variant(ButtonVariant::Secondary)
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                cancel_view
                                    .update(cx, |this, cx| this.kv_cancel_annotations(session, cx))
                                    .ok();
                            }),
                    ),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.kv_cancel_annotations(session, cx)),
            )
            .child(card)
            .into_any_element()
    }

    /// The connected shell for a Redis (KV) session: the same top bar as the
    /// SQL workspace: the keyspace browser (R1, see docs/plans/redis.md)
    /// instead of the editor/grid/schema tree, which all assume a
    /// `DatabaseDriver` session.
    pub(crate) fn render_redis_shell(
        &self,
        active: &ActiveConn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let config = &active.config;

        let topbar = self.render_topbar(&theme, &view, window, cx);

        // The work body: one pane, or two side-by-side halves when split. The
        // tab context menu (if open) overlays the whole thing.
        let work = self.render_kv_body(active, window, cx);
        let menu = active
            .kv_view
            .as_ref()
            .and_then(|v| v.tab_menu)
            .map(|(id, pos)| self.render_kv_tab_menu(active, id, pos, cx));
        let key_menu = active
            .kv_view
            .as_ref()
            .and_then(|v| v.key_menu.as_ref())
            .map(|km| self.render_kv_key_menu(active, km, cx));
        let annotate = active
            .kv_view
            .as_ref()
            .and_then(|v| v.annotate.as_ref())
            .map(|ann| self.render_kv_annotate(active, ann, cx));

        // Optional left History dock (⌘Y), mirroring the SQL shell's history
        // dock: a leading resizable SplitPane over the work area.
        let workspace = if active.history_open {
            let history_pane = self.render_kv_history(active, cx);
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            SplitPane::new("kv-split-history", Axis::Horizontal)
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
                .second(work)
                .into_any_element()
        } else {
            work
        };
        // With the agent open, dock it to the right of the whole workspace via a
        // resizable split — the same shape as the SQL shell (`render_shell`).
        // `render_assistant` is engine-agnostic (a chat over `AiTurn` events), so
        // it drops in unchanged; the KV backend grounds the turn (Part 1).
        let body = if self.assistant.is_some() {
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            let panel = self.render_assistant(cx);
            div().flex_1().min_h(px(0.)).child(
                SplitPane::new("kv-split-assistant", Axis::Horizontal)
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
            div().flex_1().min_h(px(0.)).flex().child(workspace)
        }
        .into_any_element();

        // History dock toggle, pinned far-left with an icon (mirrors the SQL
        // shell's status-bar toggle); accent-tinted while the panel is open.
        let history_toggle = div()
            .id("kv-history-toggle")
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

        // Agent toggle, pinned far-right (mirrors the SQL shell). Hidden entirely
        // when the assistant is disabled for this connection (the M-S7 kill
        // switch): no entry point, not just a no-op button.
        let assistant_enabled = self.ai_enabled();
        let assistant_open = self.assistant.is_some();
        let assistant_toggle = div()
            .id("kv-toggle-assistant")
            .ml_1()
            .flex_shrink_0()
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
                // The left group flexes and clips; the history toggle stays fixed
                // far-left while the connection info truncates.
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .min_w_0()
                    .gap_1p5()
                    .child(history_toggle)
                    .child(
                        div()
                            .flex_shrink_0()
                            .size(px(6.))
                            .rounded_full()
                            .bg(theme.green),
                    )
                    .child(div().min_w_0().truncate().child(config.display_target()))
                    .child(div().min_w_0().truncate().px_2().child(config.name.clone())),
            )
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .px_2()
                            .child(format!("{} {}", config.kind, active.version)),
                    )
                    .children(assistant_enabled.then_some(assistant_toggle)),
            );

        // The tab context menu overlays the whole shell, positioned in window
        // coordinates (from the right-click), so it mounts at the root.
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.bg_app)
            .font_family(theme.font_family.clone())
            .child(topbar)
            .child(body)
            .child(statusbar)
            .children(menu)
            .children(key_menu)
            .children(annotate)
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
