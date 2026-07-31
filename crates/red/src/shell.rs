//! The connected shell: top bar · nested resizable split (schema | editor /
//! results) · status bar. The panes are the schema tree, the SQL editor, and the
//! result grid. The split sizes are caller-owned state on [`ActiveConn`].

use flint::Theme;
use flint::prelude::*;
use gpui::{Axis, Context, MouseButton, SharedString, WeakEntity, Window, div, prelude::*, px};

/// Left inset of the top bar. On macOS it clears the seamless traffic lights
/// overlapping this strip and leaves a little breathing room between them and
/// the connection switcher; elsewhere the native caption bar is separate, so
/// only normal padding is needed. Mirrors Nyx.
#[cfg(target_os = "macos")]
pub(crate) const TITLEBAR_LEFT_INSET: f32 = 88.;
#[cfg(not(target_os = "macos"))]
pub(crate) const TITLEBAR_LEFT_INSET: f32 = 12.;

use std::rc::Rc;

use crate::app::{ActiveConn, AppState, Phase, TabWorkspace};
use crate::editor::TabDrag;
use crate::panes::{MIN_PANE_WEIGHT, drop_overlay, path_id};
use crate::tabstrip::{StripTab, TabStrip};

impl AppState {
    pub(crate) fn render_shell(
        &self,
        active: &ActiveConn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
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

        // Mutations | (schema | columns | editor / results): sits between History and
        // Schema, since it's about the connection rather than the focused result.
        let with_mutations = if active.server_open {
            let pane = self.render_server_panel(active, cx);
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            SplitPane::new("shell-split-mutations", Axis::Horizontal)
                .size(active.server_w)
                .gutter(px(1.))
                .drag(active.server_drag)
                .min_first(px(240.))
                .max_first(px(560.))
                .on_drag_start(move |anchor, _, cx| {
                    start
                        .update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                a.server_drag = Some(anchor);
                            }
                            cx.notify();
                        })
                        .ok();
                })
                .on_resize(move |size, _, cx| {
                    resize
                        .update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                a.server_w = size;
                            }
                            cx.notify();
                        })
                        .ok();
                })
                .on_drag_end(move |_, cx| {
                    end.update(cx, |this, cx| {
                        if let Phase::Connected(a) = &mut this.phase {
                            a.server_drag = None;
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .first(pane)
                .second(with_schema)
                .into_any_element()
        } else {
            with_schema
        };

        // Outermost column boundary: History | (mutations | schema | editor / results).
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
                .second(with_mutations)
                .into_any_element()
        } else {
            with_mutations
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
        // The endpoint chip *is* the Server panel's control: clicking
        // `red@localhost:3307` opens what that server is doing. The panel is about
        // the connection, so it hangs off the thing that names the connection
        // rather than off a separate icon whose refresh glyph promised the wrong
        // action. Not offered on an engine with no server behind it (SQLite is a
        // file), where the chip stays plain text.
        let can_open_server = self.has_server_panel();
        let running = self.running_mutations();
        let endpoint_hue = if active.server_open {
            theme.accent
        } else {
            theme.text_muted
        };
        let endpoint = div()
            .id("statusbar-endpoint")
            .flex()
            .items_center()
            .min_w_0()
            .gap_1p5()
            .px_2()
            .rounded(px(4.))
            .text_color(endpoint_hue)
            .child(
                // Connection state, not server state: it stays green while
                // connected so the two signals never fight over one dot.
                div()
                    .flex_shrink_0()
                    .size(px(6.))
                    .rounded_full()
                    .bg(theme.green),
            )
            .child(div().min_w_0().truncate().child(config.display_target()))
            // Background work outliving its submit keeps its amber marker, moved
            // here from the retired icon so it is still visible with the panel shut.
            .when(running > 0, |d| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .text_size(theme.scale(10.))
                        .text_color(theme.yellow)
                        .child(format!("{running} running")),
                )
            })
            .when(can_open_server, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(theme.bg_elevated))
                    .when(active.server_open, |s| s.bg(theme.bg_elevated))
                    .tooltip(Tooltip::text(if running > 0 {
                        format!("Server sessions and background work ({running} running)")
                    } else {
                        "Server sessions and background work".to_string()
                    }))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_server_panel(cx)))
            });

        // The Stop for an in-flight write. Reads/exports have their own stops (the
        // grid's ticker, the transfer toast); a write had none, which left the whole
        // write-cancellation path in the service unreachable — and the default
        // `statement_timeout` is 0, so a write stuck on a lock had no other way out.
        let stop_write = active.write_in_flight.then(|| {
            div()
                .id("statusbar-stop-write")
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_1()
                .px_2()
                .cursor_pointer()
                .text_color(theme.red)
                .hover(|s| s.bg(theme.bg_elevated))
                .tooltip(Tooltip::text(crate::i18n::tr!(
                    "shell.stop_write.help",
                    "Ask the server to stop the running statement"
                )))
                .child(crate::icons::icon("circle-x", theme.scale(10.), theme.red))
                .child(crate::i18n::tr!("shell.stop_write", "Stop"))
                .on_click(cx.listener(|this, _, _, cx| this.stop_write(cx)))
        });

        let status_left = div()
            .flex()
            .items_center()
            .min_w_0()
            .child(endpoint)
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
        // kill switch): no entry point, not just a no-op button.
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
                    .child(status_left)
                    .children(stop_write),
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

        // The schema tree's right-click menu renders here, at the root, so its
        // full-window dismiss catcher covers the whole shell and its
        // window-coordinate anchor isn't offset by the sidebar's origin.
        let schema_menu = active
            .schema
            .menu
            .as_ref()
            .map(|m| self.render_schema_menu(active, m, cx));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_app)
            .font_family(theme.font_family.clone())
            .child(topbar)
            .child(body)
            .child(statusbar)
            .children(schema_menu)
            // Same reasoning as the schema menu: anchored in window coordinates
            // from the run bar's caret, so it renders at the root.
            .children(self.render_watch_menu(cx))
    }

    /// The top bar (connection switcher · self-update pill · disconnect ·
    /// settings gear · window controls), shared by [`Self::render_shell`] (the
    /// SQL workspace) and [`Self::render_redis_shell`] (the KV placeholder) —
    /// it's engine-agnostic chrome, not part of the SQL-specific work area.
    pub(crate) fn render_topbar(
        &self,
        theme: &Theme,
        view: &WeakEntity<Self>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
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
            .child(crate::i18n::tr!("shell.disconnect", "Disconnect"))
            .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx)));

        // Settings gear lives in the top bar (mirrors the welcome screen's
        // top-right placement) rather than the status strip.
        let settings_gear = IconButton::new(
            "shell-settings",
            crate::icons::icon("settings", theme.scale(16.), theme.text_muted),
        )
        .size(IconButtonSize::Sm)
        .tooltip(crate::keymap::localize_hint("Settings  ⌘,"))
        .a11y_label(crate::i18n::tr!("common.settings", "Settings"))
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

    /// The Redis work area: the pane tree, rendered as nested [`SplitStack`]s
    /// whose leaves are panes (see [`Self::render_kv_pane`]). Rows and columns
    /// come from the same shared tree the SQL and Mongo shells use — a key browse
    /// stacked over a console is just a vertical split.
    fn render_kv_body(
        &self,
        active: &ActiveConn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(v) = active.kv_view.as_ref() else {
            return div().flex_1().into_any_element();
        };
        if let Some(zoomed) = v.layout.zoomed()
            && let Some(tab_idx) = v.pane_active(zoomed)
        {
            return self.render_kv_pane(active, zoomed, tab_idx, true, window, cx);
        }
        self.render_kv_node(active, v.layout.tree().root(), &mut Vec::new(), window, cx)
    }

    /// One node of the Redis pane tree (mirrors [`Self::render_pane_node`]).
    fn render_kv_node(
        &self,
        active: &ActiveConn,
        node: &crate::panes::Node,
        path: &mut crate::panes::SplitPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let session = active.session;
        let (axis, children) = match node {
            crate::panes::Node::Leaf(pane) => {
                let tab_idx = active
                    .kv_view
                    .as_ref()
                    .and_then(|v| v.pane_active(*pane))
                    .unwrap_or(0);
                let focused = active
                    .kv_view
                    .as_ref()
                    .is_some_and(|v| v.layout.focus() == *pane);
                return self.render_kv_pane(active, *pane, tab_idx, focused, window, cx);
            }
            crate::panes::Node::Split { axis, children } => (*axis, children),
        };
        let drag = active
            .kv_view
            .as_ref()
            .and_then(|v| v.layout.divider_drag(path));
        let mut stack = SplitStack::new(
            format!("kv-panes-{}", path_id(path)),
            match axis {
                crate::panes::SplitAxis::Horizontal => Axis::Horizontal,
                crate::panes::SplitAxis::Vertical => Axis::Vertical,
            },
        )
        .gutter(px(1.))
        .min(match axis {
            crate::panes::SplitAxis::Horizontal => px(Self::MIN_KV_PANE_W),
            crate::panes::SplitAxis::Vertical => px(Self::MIN_KV_PANE_H),
        })
        .drag(drag);
        for (i, child) in children.iter().enumerate() {
            path.push(i);
            let element = self.render_kv_node(active, &child.node, path, window, cx);
            path.pop();
            stack = stack.child(child.weight, element);
        }
        let owned = path.clone();
        let (start_path, resize_path) = (owned.clone(), owned);
        let start = cx.entity().downgrade();
        let resize = start.clone();
        let end = start.clone();
        stack
            .on_drag_start(move |drag, _, cx| {
                let path = start_path.clone();
                start
                    .update(cx, |this, cx| {
                        if let Some(v) = this
                            .conn_mut(Some(session))
                            .and_then(|a| a.kv_view.as_mut())
                        {
                            v.layout.begin_divider_drag(path, drag);
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .on_resize(move |gutter, leading, _, cx| {
                let path = resize_path.clone();
                resize
                    .update(cx, |this, cx| {
                        if let Some(v) = this
                            .conn_mut(Some(session))
                            .and_then(|a| a.kv_view.as_mut())
                        {
                            v.layout.set_weight(&path, gutter, leading, MIN_PANE_WEIGHT);
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .on_drag_end(move |_, cx| {
                end.update(cx, |this, cx| {
                    if let Some(v) = this
                        .conn_mut(Some(session))
                        .and_then(|a| a.kv_view.as_mut())
                    {
                        v.layout.end_divider_drag();
                    }
                    cx.notify();
                })
                .ok();
            })
            .into_any_element()
    }

    /// One Redis pane: its own tab strip (only this pane's tabs) over the active
    /// tab's panel body, plus the drop zones that turn a tab drag into a split. A
    /// mouse-down anywhere in the pane focuses it, so buttons and inputs act on
    /// the pane the user just touched.
    fn render_kv_pane(
        &self,
        active: &ActiveConn,
        pane: crate::app::PaneId,
        tab_idx: usize,
        focused: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::kvbrowse::KvPanel;
        let theme = cx.theme().clone();
        let (border, accent, muted) = (theme.border, theme.accent, theme.text_faint);
        let session = active.session;
        let Some(v) = active.kv_view.as_ref() else {
            return div().flex_1().into_any_element();
        };
        let is_split = v.layout.is_split();

        let active_idx = v.pane_active(pane);
        let Some(ui) = v.layout.ui(pane) else {
            return div().flex_1().into_any_element();
        };
        let tabs: Vec<StripTab> = v
            .pane_tab_indices(pane)
            .into_iter()
            .map(|i| StripTab {
                index: i,
                title: v.tabs[i].title.clone().into(),
                pinned: v.tabs[i].pinned,
                active: Some(i) == active_idx,
            })
            .collect();
        let ids: Vec<u64> = v.tabs.iter().map(|t| t.id).collect();
        let menu_ids = ids.clone();
        let strip = TabStrip::new(
            "kv",
            pane,
            ui.tab_scroll.clone(),
            move |this, i, cx| {
                this.kv_set_split_focus(session, pane, cx);
                this.kv_activate_tab(session, i, cx);
            },
            move |this, i, cx| this.kv_close_tab(session, i, cx),
            move |this, cx| {
                this.kv_set_split_focus(session, pane, cx);
                this.kv_new_empty_tab(session, cx);
            },
            move |this, from, cx| this.kv_drop_tab(session, from, pane, cx),
            move |this, slot, cx| this.kv_set_tab_drop_target(session, pane, slot, cx),
            move |this, cx| this.kv_clear_tab_drop_target(session, cx),
        )
        .tabs(tabs)
        .gap(v.layout.gap_in(pane))
        .new_tab_tooltip(crate::keymap::localize_hint("New tab  ⌘T"))
        .on_menu(move |this, i, position, cx| {
            // The menu addresses tabs by stable id, since positions shift.
            if let Some(&id) = menu_ids.get(i) {
                this.kv_open_tab_menu(session, id, position, cx);
            }
        });
        let strip = self.render_tab_strip(strip, cx);

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

        let target = v
            .layout
            .drop_target()
            .filter(|t| t.pane == pane && cx.has_active_drag());
        div()
            .id(("kv-pane", pane.0 as usize))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .when(is_split && focused, |d| {
                d.border_1().border_color(accent.opacity(0.5))
            })
            .when(is_split && !focused, |d| d.border_1().border_color(border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.kv_set_split_focus(session, pane, cx)),
            )
            .on_drag_move::<TabDrag>(cx.listener(
                move |this, e: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                    let Some(&TabDrag(from)) = e.dragged_item().downcast_ref::<TabDrag>() else {
                        return;
                    };
                    this.kv_aim_tab_drop(session, from, pane, e.bounds, e.event.position, cx);
                },
            ))
            .on_drop::<TabDrag>(cx.listener(move |this, drag: &TabDrag, _, cx| {
                if let Some(zone) = this.kv_resolved_drop_zone(session, pane, cx) {
                    this.kv_drop_tab_on_pane(session, drag.0, pane, zone, cx);
                }
            }))
            .child(strip)
            .child(div().flex_1().min_h(px(0.)).flex().child(panel))
            .children(target.map(|t| drop_overlay(t.zone, t.allowed, accent, muted)))
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
    ) -> impl IntoElement + use<> {
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
                    .child(crate::i18n::tr!(
                        "shell.tab_chooser_hint",
                        "Choose what to open in this tab  ·  press 1–6 or ↵"
                    )),
            )
            .child(cards)
    }

    /// The Redis History dock (left, ⌘Y): a collapsible "Recently viewed keys"
    /// section (browser-history for the keyspace) over a "Commands" section
    /// (past console commands), with a search box on top. Keys re-open the
    /// inspector; commands seed the console. Pure adapter over the shared
    /// [`crate::history_panel`] renderer, sharing the `query_history` store.
    fn render_kv_history(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        use crate::history_panel::{HistoryPanelSpec, HistoryRow, HistorySection};

        let session = active.session;
        let commands = self.query_history.for_conn(&active.conn_id);
        let keys: Vec<crate::kvbrowse::RecentKey> = active
            .kv_view
            .as_ref()
            .map(|v| v.recent_keys.clone())
            .unwrap_or_default();
        let (keys_collapsed, cmds_collapsed) = active
            .kv_view
            .as_ref()
            .map(|v| (v.recent_keys_collapsed, v.commands_collapsed))
            .unwrap_or((false, false));
        let has_any = !keys.is_empty() || !commands.is_empty();

        let query = active
            .history_search
            .read(cx)
            .content()
            .trim()
            .to_lowercase();
        let searching = !query.is_empty();

        // Recently-viewed keys → rows (badged with the value type).
        let key_rows: Vec<HistoryRow> = keys
            .into_iter()
            .filter(|r| !searching || r.key.to_lowercase().contains(&query))
            .map(|r| {
                let (key, kv_type, ttl) = (r.key.clone(), r.kv_type.clone(), r.ttl);
                let remove_key = r.key.clone();
                HistoryRow {
                    primary: r.key.into(),
                    secondary: red_config::history::relative_time(r.viewed_unix).into(),
                    badge: Some(kv_type.label().to_string().into()),
                    nav_index: None,
                    activate: Rc::new(move |this: &mut AppState, _replace, cx| {
                        this.kv_open_recent_key(session, key.clone(), kv_type.clone(), ttl, cx);
                    }),
                    delete: Some(Rc::new(move |this: &mut AppState, cx| {
                        this.kv_remove_recent_key(session, remove_key.clone(), cx);
                    })),
                }
            })
            .collect();

        // Past console commands → rows.
        let cmd_rows: Vec<HistoryRow> = commands
            .into_iter()
            .filter(|e| !searching || e.sql.to_lowercase().contains(&query))
            .map(|entry| {
                let cmd = entry.sql.clone();
                let id = entry.id;
                HistoryRow {
                    primary: crate::editor::history_label(&entry.sql).into(),
                    secondary: red_config::history::relative_time(entry.ran_unix).into(),
                    badge: None,
                    nav_index: None,
                    activate: Rc::new(move |this: &mut AppState, _replace, cx| {
                        this.kv_seed_console(session, cmd.clone(), cx);
                    }),
                    delete: Some(Rc::new(move |this: &mut AppState, cx| {
                        this.delete_history(id, cx)
                    })),
                }
            })
            .collect();

        let mut sections: Vec<HistorySection> = Vec::new();
        if !key_rows.is_empty() {
            sections.push(HistorySection {
                key: "recent-keys",
                title: Some("Recently viewed keys".into()),
                collapsed: !searching && keys_collapsed,
                toggle: Some(Rc::new(move |this: &mut AppState, cx| {
                    this.kv_toggle_recent_keys(session, cx)
                })),
                rows: key_rows,
            });
        }
        if !cmd_rows.is_empty() {
            sections.push(HistorySection {
                key: "commands",
                title: Some("Commands".into()),
                collapsed: !searching && cmds_collapsed,
                toggle: Some(Rc::new(move |this: &mut AppState, cx| {
                    this.kv_toggle_commands(session, cx)
                })),
                rows: cmd_rows,
            });
        }

        let spec = HistoryPanelSpec {
            sections,
            empty_text: if searching {
                "No matches".into()
            } else {
                "Nothing yet".into()
            },
            show_clear: has_any,
            on_clear: Rc::new(move |this: &mut AppState, cx| {
                this.clear_history(cx);
                this.kv_clear_recent_keys(session, cx);
            }),
            search: Some(active.history_search.clone()),
            nav: None,
            selected: None,
        };
        self.render_history_panel(spec, cx)
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
                (pinned, v.layout.is_split(), has_left, has_right, has_others)
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
    /// SQL workspace: the keyspace browser (R1)
    /// instead of the editor/grid/schema tree, which all assume a
    /// `DatabaseDriver` session.
    pub(crate) fn render_redis_shell(
        &self,
        active: &ActiveConn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
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
        let actions_menu = active
            .kv_view
            .as_ref()
            .and_then(|v| v.actions_menu)
            .map(|pos| self.render_kv_actions_menu(active, pos, cx));
        let auto_menu = active
            .kv_view
            .as_ref()
            .and_then(|v| v.auto_menu)
            .map(|pos| self.render_kv_auto_menu(active, pos, cx));

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
        // when the assistant is disabled for this connection (kill
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
                    // The keyspace size lives here (moved off the busy browse
                    // toolbar), and only while a Browse tab is focused: the stable,
                    // unfiltered `DBSIZE` — never the churny per-filter "so far"
                    // count — plus a windowing note once the resident cap has
                    // evicted the oldest scanned keys.
                    .children(active.kv_view.as_ref().and_then(|v| {
                        let browse = v.active_browse()?;
                        let base = match v.db_size {
                            Some(n) => {
                                format!("~{} keys", crate::result::group_digits(n as usize))
                            }
                            None => "counting keys…".to_string(),
                        };
                        let label = if browse.evicted {
                            format!("{base} · showing recent 20k")
                        } else {
                            base
                        };
                        Some(div().px_2().text_color(theme.text).child(label))
                    }))
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
            .children(actions_menu)
            .children(auto_menu)
    }

    /// Smallest a query pane may be squeezed to, and the floor below which a
    /// drop zone stops offering a split.
    ///
    /// This is a *legibility* floor, not a comfort one. It is tempting to reuse
    /// the old two-pane divider minimum (320px), but that answered a different
    /// question -- how far an existing pane may be dragged down -- and using it
    /// here refuses every split past the first: two panes in a default window are
    /// about 430px each, and half of that is under 320. The user asking for a
    /// third column has already decided it is worth the room.
    pub(crate) const MIN_PANE_W: f32 = 180.;
    pub(crate) const MIN_PANE_H: f32 = 120.;
    /// The Redis and MongoDB panels are denser than a query pane, so they stay
    /// usable in less room.
    pub(crate) const MIN_KV_PANE_W: f32 = 160.;
    pub(crate) const MIN_KV_PANE_H: f32 = 110.;

    /// What a query pane's drop zones measure against.
    pub(crate) const PANE_LIMITS: crate::panes::PaneLimits = crate::panes::PaneLimits {
        min_w: Self::MIN_PANE_W,
        min_h: Self::MIN_PANE_H,
        strip_h: crate::tabstrip::STRIP_H,
    };

    /// The same, for the Redis and MongoDB panes.
    pub(crate) const KV_PANE_LIMITS: crate::panes::PaneLimits = crate::panes::PaneLimits {
        min_w: Self::MIN_KV_PANE_W,
        min_h: Self::MIN_KV_PANE_H,
        strip_h: crate::tabstrip::STRIP_H,
    };

    /// The work area right of the schema dock: the pane tree, rendered as nested
    /// [`SplitStack`]s whose leaves are panes (see [`Self::render_pane`]).
    ///
    /// A zoomed pane short-circuits the whole walk: it fills the area alone, and
    /// the layout is restored untouched when the zoom is toggled off.
    fn render_work_body(
        &self,
        active: &ActiveConn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if let Some(zoomed) = active.layout.zoomed()
            && let Some(tab_idx) = active.pane_active(zoomed)
        {
            return self.render_pane(active, zoomed, tab_idx, true, window, cx);
        }
        self.render_pane_node(
            active,
            active.layout.tree().root(),
            &mut Vec::new(),
            window,
            cx,
        )
    }

    /// One node of the pane tree: a pane, or a split whose children are laid out
    /// along its axis with draggable dividers between them. `path` is the child
    /// index chain from the root, which names the split a divider belongs to.
    fn render_pane_node(
        &self,
        active: &ActiveConn,
        node: &crate::panes::Node,
        path: &mut crate::panes::SplitPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (axis, children) = match node {
            crate::panes::Node::Leaf(pane) => {
                let tab_idx = active.pane_active(*pane).unwrap_or(0);
                let focused = active.layout.focus() == *pane;
                return self.render_pane(active, *pane, tab_idx, focused, window, cx);
            }
            crate::panes::Node::Split { axis, children } => (*axis, children),
        };
        let mut stack = SplitStack::new(
            format!("sql-panes-{}", path_id(path)),
            match axis {
                crate::panes::SplitAxis::Horizontal => Axis::Horizontal,
                crate::panes::SplitAxis::Vertical => Axis::Vertical,
            },
        )
        .gutter(px(1.))
        .min(match axis {
            crate::panes::SplitAxis::Horizontal => px(Self::MIN_PANE_W),
            crate::panes::SplitAxis::Vertical => px(Self::MIN_PANE_H),
        })
        .drag(active.layout.divider_drag(path));
        for (i, child) in children.iter().enumerate() {
            path.push(i);
            let element = self.render_pane_node(active, &child.node, path, window, cx);
            path.pop();
            stack = stack.child(child.weight, element);
        }
        // The handlers own a copy of the path: `path` itself is scratch, reused as
        // the walk unwinds.
        let owned = path.clone();
        let (start_path, resize_path) = (owned.clone(), owned);
        let start = cx.entity().downgrade();
        let resize = start.clone();
        let end = start.clone();
        stack
            .on_drag_start(move |drag, _, cx| {
                let path = start_path.clone();
                start
                    .update(cx, |this, cx| {
                        if let Phase::Connected(a) = &mut this.phase {
                            a.layout.begin_divider_drag(path, drag);
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .on_resize(move |gutter, leading, _, cx| {
                let path = resize_path.clone();
                resize
                    .update(cx, |this, cx| {
                        if let Phase::Connected(a) = &mut this.phase {
                            // The minimum is a *fraction* here; the stack has no
                            // pixel context to convert it, so approximate against
                            // the whole split rather than letting a pane vanish.
                            a.layout.set_weight(&path, gutter, leading, MIN_PANE_WEIGHT);
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .on_drag_end(move |_, cx| {
                end.update(cx, |this, cx| {
                    if let Phase::Connected(a) = &mut this.phase {
                        a.layout.end_divider_drag();
                    }
                    cx.notify();
                })
                .ok();
            })
            .into_any_element()
    }

    /// One pane: the tab `tab_idx` rendered as the editor-over-result vertical
    /// split, wrapped so a click anywhere in it focuses the pane and, while the
    /// work area is divided, an accent outline marks the focused one. Each pane
    /// owns its editor/result ratio.
    fn render_pane(
        &self,
        active: &ActiveConn,
        pane: crate::app::PaneId,
        tab_idx: usize,
        is_focused: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let is_split = active.layout.is_split();

        // An ER diagram takes the half whole *below the tab strip*. Unlike a query
        // plan, which shares the result slot with the grid and keeps the editor above
        // it, there is no SQL behind a diagram, so the editor would only be shrinking
        // the canvas — but the strip stays, or the diagram would be a tab you can see
        // no way out of.
        // A schema comparison takes the half like the other read-only bodies.
        if active
            .tabs
            .get(tab_idx)
            .is_some_and(|t| matches!(t.view, Some(crate::app::TabView::SchemaDiff(_))))
        {
            let body = div()
                .size_full()
                .flex()
                .flex_col()
                .bg(theme.bg_app)
                .child(self.render_sql_tab_strip(active, pane, cx))
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .child(self.render_schema_diff(active, tab_idx, cx)),
                )
                .into_any_element();
            return self.wrap_pane(body, active, pane, is_focused, is_split, &theme, cx);
        }

        // The health report takes the half the same way: it is about the
        // connection, not about any query.
        if active
            .tabs
            .get(tab_idx)
            .is_some_and(|t| t.health().is_some())
        {
            let body = div()
                .size_full()
                .flex()
                .flex_col()
                .bg(theme.bg_app)
                .child(self.render_sql_tab_strip(active, pane, cx))
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .child(self.render_health(active, tab_idx, cx)),
                )
                .into_any_element();
            return self.wrap_pane(body, active, pane, is_focused, is_split, &theme, cx);
        }

        // A DDL view takes the half the same way, and for the same reason: there
        // is no query behind a definition, so an editor above it is dead space.
        if active.tabs.get(tab_idx).is_some_and(|t| t.ddl().is_some()) {
            let body = div()
                .size_full()
                .flex()
                .flex_col()
                .bg(theme.bg_app)
                .child(self.render_sql_tab_strip(active, pane, cx))
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .child(self.render_ddl(active, tab_idx, cx)),
                )
                .into_any_element();
            return self.wrap_pane(body, active, pane, is_focused, is_split, &theme, cx);
        }

        if active.tabs.get(tab_idx).is_some_and(|t| t.is_er()) {
            let canvas = self.render_er(active, tab_idx, cx);
            let body = div()
                .size_full()
                .flex()
                .flex_col()
                .bg(theme.bg_app)
                .child(self.render_sql_tab_strip(active, pane, cx))
                .child(div().flex_1().min_h(px(0.)).child(canvas))
                .into_any_element();
            // The visible-table describe wants `&mut self`, so it can't run inside the
            // frame it belongs to. Deferring also means it reads the viewport rect the
            // `canvas` in this very frame captured, rather than the previous one's.
            let view = cx.entity().downgrade();
            cx.defer(move |cx| {
                view.update(cx, |this, cx| this.er_fetch_visible_details(tab_idx, cx))
                    .ok();
            });
            return self.wrap_pane(body, active, pane, is_focused, is_split, &theme, cx);
        }

        let editor_pane = self.render_editor(active, tab_idx, pane, is_focused, cx);
        let results_pane = self.render_results_slot(active, tab_idx, pane, is_focused, window, cx);

        let view = cx.entity().downgrade();
        let start = view.clone();
        let resize = view.clone();
        let end = view.clone();
        // Each pane owns its editor/result ratio, so dragging one pane's divider
        // leaves the others where the user put them.
        let (editor_h, editor_drag) = active
            .layout
            .ui(pane)
            .map_or((px(300.), None), |u| (u.editor_h, u.editor_drag));
        let vsplit = SplitPane::new(format!("shell-split-v-{}", pane.0), Axis::Vertical)
            .size(editor_h)
            .gutter(px(1.))
            .drag(editor_drag)
            .min_first(px(80.))
            .on_drag_start(move |anchor, _, cx| {
                start
                    .update(cx, |this, cx| {
                        if let Phase::Connected(a) = &mut this.phase
                            && let Some(u) = a.layout.ui_mut(pane)
                        {
                            u.editor_drag = Some(anchor);
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .on_resize(move |size, _, cx| {
                resize
                    .update(cx, |this, cx| {
                        if let Phase::Connected(a) = &mut this.phase
                            && let Some(u) = a.layout.ui_mut(pane)
                        {
                            u.editor_h = size;
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .on_drag_end(move |_, cx| {
                end.update(cx, |this, cx| {
                    if let Phase::Connected(a) = &mut this.phase
                        && let Some(u) = a.layout.ui_mut(pane)
                    {
                        u.editor_drag = None;
                    }
                    cx.notify();
                })
                .ok();
            })
            .first(editor_pane)
            .second(results_pane);

        self.wrap_pane(
            vsplit.into_any_element(),
            active,
            pane,
            is_focused,
            is_split,
            &theme,
            cx,
        )
    }

    /// Wrap a pane's body in the chrome every pane shares: the id that scopes its
    /// child element ids apart from its siblings', the focus outline shown while
    /// the work area is divided, the tab drop zones, and the mouse-down that aims
    /// run/export/filter at whichever pane was clicked.
    ///
    /// The drop zones are what make splitting a gesture: a tab dragged over the
    /// middle of a pane moves into it, and one dragged near an edge splits a new
    /// pane off that side — including the first split of an undivided area. The
    /// strips handle their own drops (a reorder) and stop propagation, so this
    /// fires only for the body.
    #[allow(clippy::too_many_arguments, reason = "render plumbing, not an API")]
    fn wrap_pane(
        &self,
        body: gpui::AnyElement,
        active: &ActiveConn,
        pane: crate::app::PaneId,
        is_focused: bool,
        is_split: bool,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let accent = theme.accent;
        let border = theme.border;
        let muted = theme.text_faint;
        // Only paint a zone for the pane the cursor is actually over.
        let target = active
            .layout
            .drop_target()
            .filter(|t| t.pane == pane && cx.has_active_drag());
        div()
            .id(("sql-pane", pane.0 as usize))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .when(is_split, |d| {
                d.border_1()
                    .border_color(if is_focused { accent } else { border })
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.set_split_focus(pane, cx)),
            )
            .on_drag_move::<crate::editor::TabDrag>(cx.listener(
                move |this, e: &gpui::DragMoveEvent<crate::editor::TabDrag>, _, cx| {
                    // The zones depend on *which* tab is in flight: a pane's only
                    // tab has nowhere to go within its own pane.
                    let Some(&crate::editor::TabDrag(from)) =
                        e.dragged_item().downcast_ref::<crate::editor::TabDrag>()
                    else {
                        return;
                    };
                    this.aim_tab_drop(from, pane, e.bounds, e.event.position, cx);
                },
            ))
            .on_drop::<crate::editor::TabDrag>(cx.listener(
                move |this, drag: &crate::editor::TabDrag, _, cx| {
                    // `None` means the zone was refused; the muted highlight
                    // already said so, so the drop leaves the layout alone.
                    if let Some(zone) = this.resolved_drop_zone(pane, cx) {
                        this.drop_tab_on_pane(drag.0, pane, zone, cx);
                    }
                },
            ))
            .child(body)
            .children(target.map(|t| crate::panes::drop_overlay(t.zone, t.allowed, accent, muted)))
            .into_any_element()
    }

    /// The lower pane for tab `tab_idx`: its query plan when one is open,
    /// else the result grid; both share the slot. Picks per-tab (not per-focus) so
    /// each pane shows its own tab's view.
    fn render_results_slot(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        pane: crate::app::PaneId,
        is_focused: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let tab = active.tabs.get(tab_idx);
        if tab.is_some_and(|t| t.plan.is_some()) {
            self.render_plan(active, tab_idx, cx)
        } else {
            self.render_result(active, tab_idx, pane, is_focused, window, cx)
                .into_any_element()
        }
    }
}
