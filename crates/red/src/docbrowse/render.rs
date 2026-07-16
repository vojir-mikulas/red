//! Rendering for the MongoDB shell, split out of `docbrowse/mod.rs`: the tabbed
//! work area (a per-half tab strip over the active collection tab, in an optional
//! split), the `database -> collection` sidebar tree, and each collection's
//! Documents / Query / Schema / Indexes panels — including the three Compass-style
//! document render modes (Table / List / JSON). A second `impl AppState` block
//! over the parent's state (`use super::*`).

use std::rc::Rc;

use flint::Theme;
use flint::prelude::*;
use gpui::{
    App, Axis, Context, MouseButton, Pixels, SharedString, UniformListScrollHandle, WeakEntity,
    Window, div, list, prelude::*, px,
};
use red_core::doc::{CollKind, DocPlan, Document};
use red_service::SessionId;

use crate::app::{ActiveConn, AppState, Phase, SplitHalf, TabWorkspace};

use super::*;

impl AppState {
    pub(crate) fn render_mongo_shell(
        &self,
        active: &ActiveConn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let topbar = self.render_topbar(&theme, &view, window, cx);
        let workspace = self.render_doc_body(active, window, cx);

        // Dock the assistant to the right of the workspace when it's open, the
        // same resizable split the SQL/Redis shells use.
        let body = if self.assistant.is_some() {
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            let panel = self.render_assistant(cx);
            div().flex_1().min_h(px(0.)).child(
                SplitPane::new("doc-split-assistant", Axis::Horizontal)
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

        // A destructive write awaiting confirmation overlays everything.
        let confirm = active
            .doc_view
            .as_ref()
            .and_then(|v| {
                v.pending_write.as_ref().map(|(_, write, prompt)| {
                    // Only a single-document delete is suppressible by the setting;
                    // drops and many-writes stay behind the server's confirm gate.
                    let suppressible = matches!(write, DocWrite::Delete { many: false, .. });
                    (v.session, prompt.clone(), suppressible)
                })
            })
            .map(|(session, prompt, suppressible)| {
                self.render_doc_confirm(session, prompt, suppressible, &theme, &view)
            });

        // The open tab context menu overlays as a floating panel + dismiss catcher.
        let tab_menu = active
            .doc_view
            .as_ref()
            .and_then(|v| v.tab_menu)
            .map(|(id, pos)| self.render_doc_tab_menu(active, id, pos, cx));

        // The documents-toolbar "Actions" dropdown, likewise a floating overlay.
        let actions_menu = active
            .doc_view
            .as_ref()
            .and_then(|v| v.actions_menu)
            .map(|pos| self.render_doc_actions_menu(active, pos, cx));

        let statusbar = self.render_doc_statusbar(active, &theme, cx);

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_app)
            .text_color(theme.text)
            .child(topbar)
            .child(div().flex().flex_1().min_h(px(0.)).child(body))
            .child(statusbar)
            .children(confirm)
            .children(tab_menu)
            .children(actions_menu)
    }

    /// The whole work body: the `database -> collection` sidebar tree (a
    /// resizable, ⌘B-collapsible left dock) over the tabbed work area.
    fn render_doc_body(
        &self,
        active: &ActiveConn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let Some(v) = active.doc_view.as_ref() else {
            return div().size_full().into_any_element();
        };
        let work = self.render_doc_work(active, window, cx);
        // Collapsed: the tree is hidden (⌘B), the work area fills the width.
        if active.sidebar_collapsed {
            return div()
                .flex()
                .size_full()
                .child(div().flex_1().min_w(px(0.)).h_full().child(work))
                .into_any_element();
        }
        let tree_filter = v.tree_filter.read(cx).content().to_string();
        let tree = self.render_doc_tree(v, &tree_filter, &theme, &view);
        let tree_pane = div()
            .size_full()
            .border_r_1()
            .border_color(theme.border)
            .child(tree);
        let start = view.clone();
        let resize = view.clone();
        let end = view.clone();
        div()
            .size_full()
            .child(
                SplitPane::new("doc-split-tree", Axis::Horizontal)
                    .size(active.sidebar_w)
                    .gutter(px(1.))
                    .drag(active.sidebar_drag)
                    .min_first(px(180.))
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
                    .first(tree_pane)
                    .second(div().size_full().child(work)),
            )
            .into_any_element()
    }

    /// The footer status bar (mirrors the SQL/Redis shells): dock toggle + a
    /// connection dot / target / name / read-only badge on the left, and the
    /// focused collection's document range, the engine + version, and the
    /// assistant toggle on the right.
    fn render_doc_statusbar(
        &self,
        active: &ActiveConn,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let config = &active.config;

        // Left: collection-tree dock toggle (⌘B).
        let sidebar_toggle = div()
            .id("doc-toggle-sidebar")
            .mr_1()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .size(px(20.))
            .rounded(px(4.))
            .cursor_pointer()
            .tooltip(Tooltip::text(crate::keymap::localize_hint(
                "Toggle collections  ⌘B",
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

        let read_only_badge = config.read_only.then(|| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .ml_2()
                .text_color(theme.yellow)
                .child(crate::icons::icon("lock", theme.scale(11.), theme.yellow))
                .child("read-only")
        });
        let status_left = div()
            .flex()
            .items_center()
            .gap_2()
            .min_w_0()
            .child(
                div()
                    .size(px(6.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(theme.green),
            )
            .child(div().min_w_0().truncate().child(config.display_target()))
            .child(div().min_w_0().truncate().child(config.name.clone()))
            .children(read_only_badge);

        // Right: the focused collection's document range.
        let counts = active
            .doc_view
            .as_ref()
            .and_then(|v| v.focused_coll())
            .map(|c| {
                if c.loading {
                    "loading…".to_string()
                } else if c.docs.is_empty() {
                    "0 documents".to_string()
                } else {
                    let start = c.skip + 1;
                    let end = c.skip + c.docs.len() as u64;
                    let total = c.total.map(|t| format!(" of {t}")).unwrap_or_default();
                    format!("{start}\u{2013}{end}{total}")
                }
            });
        let status_right = div()
            .flex()
            .items_center()
            .gap_3()
            .children(counts.map(|c| div().flex_shrink_0().child(c)))
            .child(
                div()
                    .flex_shrink_0()
                    .child(format!("{} {}", config.kind, active.version)),
            );

        let assistant_enabled = self.ai_enabled();
        let assistant_open = self.assistant.is_some();
        let assistant_toggle = div()
            .id("doc-toggle-assistant")
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

        div()
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
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .overflow_hidden()
                    .child(sidebar_toggle)
                    .child(status_left),
            )
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .child(status_right)
                    .children(assistant_enabled.then_some(assistant_toggle)),
            )
            .into_any_element()
    }

    /// The documents toolbar's "Actions" dropdown: the secondary/rare actions
    /// (Explain, and on a writable connection New / Drop) collected off the
    /// toolbar so it never overflows. A positioned `ContextMenu` over a dismiss
    /// catcher, floated inside the viewport (mirrors the Redis actions menu).
    fn render_doc_actions_menu(
        &self,
        active: &ActiveConn,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let session = active.session;
        let writable = !active.config.read_only;
        let mut menu = ContextMenu::new("doc-actions-menu").item(
            ContextMenuItem::new("doc-act-explain", "Explain query").on_click(cx.listener(
                move |this, _, _, cx| {
                    this.doc_close_actions_menu(session, cx);
                    this.doc_run_explain(session, cx);
                },
            )),
        );
        if writable {
            menu = menu
                .separator()
                .item(
                    ContextMenuItem::new("doc-act-new", "New document").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.doc_close_actions_menu(session, cx);
                            this.doc_new_document(session, cx);
                        },
                    )),
                )
                .item(
                    ContextMenuItem::new("doc-act-drop", "Drop collection…").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.doc_close_actions_menu(session, cx);
                            this.doc_drop_current(session, cx);
                        },
                    )),
                );
        }
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.doc_close_actions_menu(session, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| this.doc_close_actions_menu(session, cx)),
            )
            .child(
                floating(div().occlude().child(menu.into_any_element()))
                    .at(pos)
                    .anchor(gpui::Anchor::TopRight),
            )
            .into_any_element()
    }

    /// The tabbed work area: one half, or two side-by-side halves when split
    /// (mirrors the Redis `render_kv_body`).
    fn render_doc_work(
        &self,
        active: &ActiveConn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(v) = active.doc_view.as_ref() else {
            return div().flex_1().into_any_element();
        };
        let Some(s) = v.split.as_ref() else {
            let idx = v.pane_active(SplitHalf::Primary).unwrap_or(v.active_tab);
            return self.render_doc_half(active, SplitHalf::Primary, idx, true, window, cx);
        };
        let (width, drag) = (s.width, s.drag);
        let primary_focused = s.focus == SplitHalf::Primary;
        let primary_tab = v.pane_active(SplitHalf::Primary).unwrap_or(v.active_tab);
        let secondary_tab = v.pane_active(SplitHalf::Secondary).unwrap_or(s.secondary);
        let first = self.render_doc_half(
            active,
            SplitHalf::Primary,
            primary_tab,
            primary_focused,
            window,
            cx,
        );
        let second = self.render_doc_half(
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
                SplitPane::new("doc-split-halves", Axis::Horizontal)
                    .size(width)
                    .gutter(px(1.))
                    .drag(drag)
                    .min_first(px(360.))
                    .on_drag_start(move |anchor, _, cx| {
                        start
                            .update(cx, |this, cx| {
                                if let Phase::Connected(a) = &mut this.phase
                                    && let Some(v) = &mut a.doc_view
                                    && let Some(s) = &mut v.split
                                {
                                    s.drag = Some(anchor);
                                }
                                cx.notify();
                            })
                            .ok();
                    })
                    .on_resize(move |size, _, cx| {
                        resize
                            .update(cx, |this, cx| {
                                if let Phase::Connected(a) = &mut this.phase
                                    && let Some(v) = &mut a.doc_view
                                    && let Some(s) = &mut v.split
                                {
                                    s.width = size;
                                }
                                cx.notify();
                            })
                            .ok();
                    })
                    .on_drag_end(move |_, cx| {
                        end.update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase
                                && let Some(v) = &mut a.doc_view
                                && let Some(s) = &mut v.split
                            {
                                s.drag = None;
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
    /// the SQL/Redis strips) over the active tab's collection panel.
    fn render_doc_half(
        &self,
        active: &ActiveConn,
        half: SplitHalf,
        tab_idx: usize,
        focused: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::editor::{TabDrag, TabDragPreview};
        let theme = cx.theme().clone();
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
        let Some(v) = active.doc_view.as_ref() else {
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
            let group = SharedString::from(format!("doc-tab-{i}"));
            let bar_before = dragging && drop_target == Some(i);
            let bar_after = dragging && Some(i) == last_in_pane && drop_target == Some(i + 1);
            div()
                .id(("doc-tab", i))
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
                    this.doc_set_split_focus(session, half, cx);
                    this.doc_activate_tab(session, i, cx);
                }))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _, _, cx| {
                        if !pinned {
                            this.doc_close_tab(session, i, cx);
                        }
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        let pos = event.position;
                        this.doc_open_tab_menu(session, id, pos, cx);
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
                        .update(cx, |this, cx| {
                            this.doc_set_tab_drop_target(session, gap, cx)
                        })
                        .ok();
                })
                .on_drop::<TabDrag>(move |drag, _window, cx| {
                    let from = drag.0;
                    cx.stop_propagation();
                    drop_view
                        .update(cx, |this, cx| this.doc_drop_tab(session, from, half, cx))
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
                                .id(("doc-tab-close", i))
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(15.))
                                .rounded(px(3.))
                                .text_color(faint)
                                .hover(|s| s.bg(bg_hover).text_color(text))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.doc_close_tab(session, i, cx);
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
            .id("doc-tabstrip")
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
                        .update(cx, |this, cx| this.doc_clear_tab_drop_target(session, cx))
                        .ok();
                }
            })
            .on_drop::<TabDrag>(move |drag, _window, cx| {
                let from = drag.0;
                cx.stop_propagation();
                strip_drop_view
                    .update(cx, |this, cx| this.doc_drop_tab(session, from, half, cx))
                    .ok();
            })
            .children(unpinned_tabs);
        let pinned_strip = (!pinned_tabs.is_empty()).then(|| {
            div()
                .id("doc-tabstrip-pinned")
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
                    .id("doc-new")
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
                        this.doc_set_split_focus(session, half, cx);
                        this.doc_new_empty_tab(session, cx);
                    }))
                    .child(crate::icons::icon("plus", theme.scale(13.), faint)),
            );

        let panel = match v.coll_at(tab_idx) {
            Some(current) => self.render_doc_collection(
                session,
                current,
                v.read_only,
                active.inspector_w,
                active.inspector_drag,
                &theme,
                &view,
            ),
            None => render_doc_empty(&theme),
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
                        .update(cx, |this, cx| this.doc_set_split_focus(session, half, cx))
                        .ok();
                })
            })
            .child(strip)
            .child(div().flex_1().min_h(px(0.)).flex().child(panel))
            .into_any_element()
    }

    /// The tab right-click context menu (mirrors the Redis `render_kv_tab_menu`).
    fn render_doc_tab_menu(
        &self,
        active: &ActiveConn,
        id: u64,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::app::TabCloseScope;
        let session = active.session;
        let (pinned, is_split, has_left, has_right, has_others) = active
            .doc_view
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
            .doc_view
            .as_ref()
            .map(|v| v.tabs.len() > 1)
            .unwrap_or(false);
        // Only a collection tab can be duplicated (a blank tab has nothing to copy).
        let is_coll = active
            .doc_view
            .as_ref()
            .and_then(|v| v.tabs.iter().find(|t| t.id == id))
            .map(|t| matches!(t.state, MongoTabState::Collection(_)))
            .unwrap_or(false);
        let move_label = if is_split {
            "Move to other pane"
        } else {
            "Open in split"
        };
        let menu = ContextMenu::new("doc-tab-context-menu")
            .item(
                ContextMenuItem::new("doc-tab-duplicate", "Duplicate tab")
                    .disabled(!is_coll)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.doc_duplicate_tab(session, id, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("doc-tab-pin", if pinned { "Unpin tab" } else { "Pin tab" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.doc_toggle_tab_pin(session, id, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("doc-tab-move", move_label).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.doc_move_tab_to_other_half(session, id, cx);
                    },
                )),
            )
            .separator()
            .item(
                ContextMenuItem::new("doc-tab-close", "Close")
                    .disabled(!closable)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.doc_close_tab_by_id(session, id, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("doc-tab-close-others", "Close Others")
                    .disabled(!has_others)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.doc_close_tab_group(session, id, TabCloseScope::Others, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("doc-tab-close-left", "Close Left")
                    .disabled(!has_left)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.doc_close_tab_group(session, id, TabCloseScope::Left, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("doc-tab-close-right", "Close Right")
                    .disabled(!has_right)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.doc_close_tab_group(session, id, TabCloseScope::Right, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("doc-tab-close-all", "Close All")
                    .disabled(!closable)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.doc_close_tab_group(session, id, TabCloseScope::All, cx);
                    })),
            );
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.doc_close_tab_menu(session, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| this.doc_close_tab_menu(session, cx)),
            )
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

    /// The destructive-write confirm modal: a scrim over a centered card with the
    /// prompt and Cancel / Confirm.
    fn render_doc_confirm(
        &self,
        session: SessionId,
        prompt: String,
        suppressible: bool,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let cancel_view = view.clone();
        let confirm_view = view.clone();
        // The "Don't ask again" opt-out only rides on the client-gated single delete;
        // the server-gated drop/many confirms ignore the setting, so offering it there
        // would be a checkbox that does nothing.
        let toggle_view = view.clone();
        let dont_ask = suppressible.then(|| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Checkbox::new("doc-delete-dont-ask", false)
                        .mark(crate::icons::icon("check", px(12.), theme.on_accent))
                        .on_change(move |checked: &bool, _, cx| {
                            toggle_view
                                .update(cx, |this, cx| this.set_confirm_destructive(!*checked, cx))
                                .ok();
                        }),
                )
                .child(
                    div()
                        .text_size(theme.scale(12.))
                        .text_color(theme.text_muted)
                        .child("Don't ask again"),
                )
        });
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0., 0., 0., 0.5))
            .child(
                div()
                    .w(px(420.))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .bg(theme.bg_panel)
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .child(div().font_weight(gpui::FontWeight::MEDIUM).child("Confirm"))
                    .child(div().text_color(theme.text_muted).child(prompt))
                    .children(dont_ask)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("doc-confirm-cancel", "Cancel")
                                    .size(ButtonSize::Sm)
                                    .variant(ButtonVariant::Secondary)
                                    .on_click(move |_, _, cx| {
                                        cancel_view
                                            .update(cx, |this, cx| {
                                                this.doc_cancel_write(session, cx)
                                            })
                                            .ok();
                                    }),
                            )
                            .child(
                                Button::new("doc-confirm-ok", "Confirm")
                                    .size(ButtonSize::Sm)
                                    .variant(ButtonVariant::Danger)
                                    .on_click(move |_, _, cx| {
                                        confirm_view
                                            .update(cx, |this, cx| {
                                                this.doc_confirm_write(session, cx)
                                            })
                                            .ok();
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The `database -> collection` tree (left dock), a Flint `Tree` so it gets
    /// keyboard + vim navigation for free (see [`AppState::doc_tree_nav`]).
    /// Databases are click / Enter to expand; a collection row opens it in a tab
    /// (⌘-click / new-tab handled in `on_select`).
    fn render_doc_tree(
        &self,
        v: &MongoView,
        filter: &str,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let session = v.session;
        let flat = v.flatten_doc_tree(filter);
        let items: Vec<TreeItem> = flat.iter().map(|r| r.item).collect();
        let selected_ix = v
            .tree_selected
            .as_ref()
            .and_then(|s| flat.iter().position(|r| r.sel.as_ref() == Some(s)));
        let rows = Rc::new(flat);

        let error = v
            .error
            .as_ref()
            .map(|e| div().px_2().py_1().text_color(theme.red).child(e.clone()));

        let (nav_view, toggle_view, select_view) = (view.clone(), view.clone(), view.clone());
        let rows_render = rows.clone();
        let rows_toggle = rows.clone();
        let rows_select = rows.clone();

        let tree = Tree::new("doc-db-tree")
            .rows(items)
            .row_height(px(24.))
            .indent(px(14.))
            .track_scroll(&v.tree_scroll)
            // The tree owns the sidebar's focus handle; ↑/↓ / ←/→ / Enter (plus
            // hjkl/g/G) drive selection, expansion, and opening.
            .focus_handle(v.tree_focus.clone())
            .vim_nav(self.vim_mode())
            .on_nav(move |nav, _window, cx| {
                nav_view
                    .update(cx, |this, cx| this.doc_tree_nav(session, nav, cx))
                    .ok();
            })
            .selected(selected_ix)
            .disclosure(|expanded, _window, cx| {
                let name = if expanded { "chevron-down" } else { "chevron" };
                crate::icons::icon(name, cx.theme().scale(12.), cx.theme().text_muted)
                    .into_any_element()
            })
            .render_row(move |ix, _window, cx| render_doc_tree_row(&rows_render[ix], cx))
            .on_toggle(move |ix, _window, cx| {
                if let Some(DocTreeSel::Db(db)) = rows_toggle[ix].sel.clone() {
                    toggle_view
                        .update(cx, |this, cx| this.doc_toggle_db(session, db, cx))
                        .ok();
                }
            })
            // A body click acts on the row: a database toggles, a collection opens
            // (⌘/Ctrl-click opens it in a new tab). The click also plants tree
            // focus so arrows continue from here.
            .on_select(move |ix, event, window, cx| {
                let Some(sel) = rows_select[ix].sel.clone() else {
                    return;
                };
                let new_tab = event.modifiers().secondary();
                select_view
                    .update(cx, |this, cx| {
                        if let Some(v) = this
                            .conn_mut(Some(session))
                            .and_then(|a| a.doc_view.as_mut())
                        {
                            v.tree_selected = Some(sel.clone());
                            let handle = v.tree_focus.clone();
                            window.focus(&handle, cx);
                        }
                        match sel {
                            DocTreeSel::Db(db) => this.doc_toggle_db(session, db, cx),
                            DocTreeSel::Coll { db, coll } => {
                                this.doc_open_collection(session, db, coll, new_tab, cx)
                            }
                        }
                    })
                    .ok();
            });

        // The search box narrows the tree live (see `flatten_doc_tree`); ⌘F from
        // the tree / root focuses it.
        let filter_row = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .px_2()
            .pt_2()
            .pb_1()
            .child(div().flex_1().child(v.tree_filter.clone()));

        div()
            .id("doc-db-tree-dock")
            .size_full()
            .flex()
            .flex_col()
            .text_size(theme.scale(13.))
            .child(filter_row)
            .children(error)
            .child(div().flex_1().min_h(px(0.)).child(tree))
            .into_any_element()
    }

    /// One collection tab's body: a header (collection name + panel picker, plus
    /// the view-mode toggle / filter bar / pager on the Documents panel) over the
    /// selected panel.
    #[allow(clippy::too_many_arguments)]
    fn render_doc_collection(
        &self,
        session: SessionId,
        current: &CollView,
        read_only: bool,
        inspector_w: Pixels,
        inspector_drag: Option<DragAnchor>,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let header = self.render_doc_header(session, current, theme, view);
        let body = match current.panel {
            DocPanel::Documents => self.render_doc_documents(
                session,
                current,
                read_only,
                inspector_w,
                inspector_drag,
                theme,
                view,
            ),
            DocPanel::Query => self.render_doc_query(session, current, theme, view),
            DocPanel::Schema => render_doc_schema_panel(current, theme),
            DocPanel::Indexes => render_doc_indexes_panel(current, theme),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_doc_header(
        &self,
        session: SessionId,
        current: &CollView,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let picker_view = view.clone();
        let selected_ix = DocPanel::ALL
            .iter()
            .position(|(p, _)| *p == current.panel)
            .unwrap_or(0);
        let picker = DocPanel::ALL
            .iter()
            .fold(Segmented::new("doc-panel"), |seg, (_, label)| {
                seg.segment(*label)
            })
            .selected(selected_ix)
            .on_select(move |ix, _, cx| {
                let panel = DocPanel::ALL
                    .get(ix)
                    .map(|(p, _)| *p)
                    .unwrap_or(DocPanel::Documents);
                picker_view
                    .update(cx, |this, cx| this.doc_set_panel(session, panel, cx))
                    .ok();
            });

        // Row 1: collection name + the panel picker. Kept sparse so it never
        // crowds; the paging count lives in the footer status bar.
        let title_row = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .flex_shrink_0()
                    .child(format!("{}.{}", current.db, current.coll)),
            )
            .child(picker)
            .child(div().flex_1());

        let mut header = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .child(title_row);

        // Row 2: the documents toolbar — view-mode toggle, the filter bar (the one
        // flex-growing element), Run, compact pager, and an Actions dropdown that
        // holds Explain / New / Drop so the row can't overflow.
        if current.panel == DocPanel::Documents {
            let mode_view = view.clone();
            let mode_ix = DocViewMode::ALL
                .iter()
                .position(|(m, _)| *m == current.view_mode)
                .unwrap_or(0);
            let mode = DocViewMode::ALL
                .iter()
                .fold(Segmented::new("doc-view-mode"), |seg, (_, label)| {
                    seg.segment(*label)
                })
                .selected(mode_ix)
                .on_select(move |ix, _, cx| {
                    let mode = DocViewMode::ALL
                        .get(ix)
                        .map(|(m, _)| *m)
                        .unwrap_or(DocViewMode::Table);
                    mode_view
                        .update(cx, |this, cx| this.doc_set_view_mode(session, mode, cx))
                        .ok();
                });

            let run_view = view.clone();
            let prev_view = view.clone();
            let next_view = view.clone();
            let actions_view = view.clone();
            let actions_button = div()
                .id("doc-actions")
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_1()
                .h(px(24.))
                .px(px(8.))
                .rounded(px(4.))
                .border_1()
                .border_color(theme.border)
                .bg(theme.bg_panel)
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_elevated))
                .text_color(theme.text_muted)
                .text_size(theme.scale(12.))
                .on_mouse_down(
                    MouseButton::Left,
                    move |ev: &gpui::MouseDownEvent, _, cx| {
                        let pos = ev.position;
                        actions_view
                            .update(cx, |this, cx| this.doc_open_actions_menu(session, pos, cx))
                            .ok();
                    },
                )
                .child("Actions")
                .child(crate::icons::icon(
                    "chevron-down",
                    theme.scale(11.),
                    theme.text_muted,
                ));

            let toolbar = div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .pb_2()
                .child(mode)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(120.))
                        .child(current.filter_input.clone()),
                )
                .child(
                    Button::new("doc-run-filter", "Run")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Secondary)
                        .on_click(move |_, _, cx| {
                            run_view
                                .update(cx, |this, cx| this.doc_apply_filter(session, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new("doc-prev", "Prev")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Secondary)
                        .disabled(current.skip == 0 || current.loading)
                        .on_click(move |_, _, cx| {
                            prev_view
                                .update(cx, |this, cx| this.doc_page(session, false, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new("doc-next", "Next")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Secondary)
                        .disabled(current.exhausted || current.loading)
                        .on_click(move |_, _, cx| {
                            next_view
                                .update(cx, |this, cx| this.doc_page(session, true, cx))
                                .ok();
                        }),
                )
                .child(actions_button);
            header = header.child(toolbar);
        }
        header.into_any_element()
    }

    /// The Documents panel: the explain readout (when requested) over the chosen
    /// document render mode, with the inspector docked right when a row is open or
    /// a new document is being composed.
    #[allow(clippy::too_many_arguments)]
    fn render_doc_documents(
        &self,
        session: SessionId,
        current: &CollView,
        read_only: bool,
        inspector_w: Pixels,
        inspector_drag: Option<DragAnchor>,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let content = if current.docs.is_empty() && !current.loading {
            doc_centered_hint("No documents.", theme)
        } else {
            match current.view_mode {
                DocViewMode::Table => self.render_doc_grid(session, current, theme, view),
                DocViewMode::List => self.render_doc_list(session, current, read_only, theme, view),
                DocViewMode::Json => render_doc_json(current, theme),
            }
        };
        let show_inspector = current.inspector.is_some() || current.inspector_insert;
        let content_area = if show_inspector {
            // The inspector floats *over* the grid (docked to the right), so the
            // grid keeps its full width instead of being squeezed by a split. Its
            // left edge is a drag handle that resizes the overlay, reusing the same
            // per-connection width/anchor the SQL/Redis inspectors do.
            let panel = self.render_doc_inspector_overlay(
                session,
                current,
                read_only,
                inspector_w,
                theme,
                view,
            );
            let mut area = div()
                .relative()
                .flex_1()
                .min_h(px(0.))
                // The grid underneath keeps the full width.
                .child(div().size_full().flex().min_h(px(0.)).child(content))
                .child(panel);
            // While dragging the handle, a full-cover overlay tracks the cursor and
            // ends the drag on release, mirroring `SplitPane`'s divider drag.
            if let Some(anchor) = inspector_drag {
                let (resize, end, end_out) = (view.clone(), view.clone(), view.clone());
                area = area.child(
                    div()
                        .id("doc-inspector-drag")
                        .occlude()
                        .absolute()
                        .inset_0()
                        .cursor_ew_resize()
                        .on_mouse_move(move |event: &gpui::MouseMoveEvent, _, cx| {
                            // Trailing-sized: dragging the handle left grows the panel.
                            let delta = f32::from(event.position.x - anchor.start_coord);
                            let raw = f32::from(anchor.start_size) - delta;
                            let w = px(raw.clamp(280., 1400.));
                            resize
                                .update(cx, |this, cx| {
                                    if let Phase::Connected(a) = &mut this.phase {
                                        a.inspector_w = w;
                                    }
                                    cx.notify();
                                })
                                .ok();
                        })
                        .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                            end.update(cx, |this, cx| {
                                if let Phase::Connected(a) = &mut this.phase {
                                    a.inspector_drag = None;
                                }
                                cx.notify();
                            })
                            .ok();
                        })
                        .on_mouse_up_out(MouseButton::Left, move |_, _, cx| {
                            end_out
                                .update(cx, |this, cx| {
                                    if let Phase::Connected(a) = &mut this.phase {
                                        a.inspector_drag = None;
                                    }
                                    cx.notify();
                                })
                                .ok();
                        }),
                );
            }
            area.into_any_element()
        } else {
            div()
                .flex_1()
                .min_h(px(0.))
                .child(content)
                .into_any_element()
        };

        let explain = current
            .explain
            .as_ref()
            .map(|plan| render_explain_box(session, plan, theme, view));

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .children(explain)
            .child(content_area)
            .into_any_element()
    }

    /// The Query panel: the aggregation-pipeline editor over its results grid.
    fn render_doc_query(
        &self,
        session: SessionId,
        current: &CollView,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let run_view = view.clone();
        let toolbar = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .child(
                div()
                    .flex_1()
                    .text_color(theme.text_muted)
                    .child("Aggregation pipeline (extended JSON array of stages)"),
            )
            .child(
                Button::new("doc-run-agg", "Run")
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Primary)
                    .disabled(current.query_loading)
                    .on_click(move |_, _, cx| {
                        run_view
                            .update(cx, |this, cx| this.doc_run_aggregate(session, cx))
                            .ok();
                    }),
            );

        let editor = div()
            .h(px(160.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .child(current.query_editor.clone());

        let results = if current.query_docs.is_empty() {
            doc_centered_hint(
                if current.query_loading {
                    "Running..."
                } else {
                    "Run a pipeline to see results."
                },
                theme,
            )
        } else {
            render_docs_table(
                "doc-query-grid",
                &current.query_docs,
                &current.query_columns,
                &current.query_scroll,
                theme,
            )
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(toolbar)
            .child(editor)
            .child(results)
            .into_any_element()
    }

    fn render_doc_grid(
        &self,
        session: SessionId,
        current: &CollView,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let columns: Vec<Column> = current
            .columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                if i == 0 {
                    Column::new(name.clone()).width(px(220.))
                } else {
                    Column::new(name.clone()).flex()
                }
            })
            .collect();

        let docs = Rc::new(current.docs.clone());
        let cols = Rc::new(current.columns.clone());
        let render_docs = docs.clone();
        let render_cols = cols.clone();
        let text = theme.text;
        let faint = theme.text_faint;
        let select_view = view.clone();
        let nav_view = view.clone();
        // The keyboard cursor drives the highlight once the grid has been touched;
        // before that it falls back to the inspected row.
        let selected = current.cursor.or(current.inspector);

        Table::<()>::new("doc-grid", columns)
            .row_count(current.docs.len())
            .grid_lines(true)
            .text_size(theme.scale(12.))
            .track_scroll(&current.scroll)
            .focus_handle(current.list_focus.clone())
            // Vim motions (hjkl/g/G/Ctrl-d/Ctrl-u) ride alongside the arrow keys
            // when the user has turned vim navigation on.
            .vim_nav(self.vim_mode())
            .on_nav(move |nav, _extend, _window, cx| {
                nav_view
                    .update(cx, |this, cx| this.doc_grid_nav(session, nav, cx))
                    .ok();
            })
            .selected(selected)
            .on_select(move |ix, _click, window, cx| {
                select_view
                    .update(cx, |this, cx| {
                        // A click plants the keyboard cursor and focuses the grid so
                        // arrows continue from the clicked row.
                        if let Some(current) = this.doc_focused_coll_mut(session) {
                            current.cursor = Some(ix);
                            let handle = current.list_focus.clone();
                            window.focus(&handle, cx);
                        }
                        this.doc_toggle_inspector(session, ix, cx);
                    })
                    .ok();
            })
            .render_row(move |ix, _window, _cx| {
                let Some(doc) = render_docs.get(ix) else {
                    return Vec::new();
                };
                render_cols
                    .iter()
                    .map(|col| match cell_string(doc, col) {
                        Some(text_val) => div()
                            .min_w_0()
                            .truncate()
                            .text_color(text)
                            .child(text_val)
                            .into_any_element(),
                        None => div().text_color(faint).child("\u{2014}").into_any_element(),
                    })
                    .collect()
            })
            .into_any_element()
    }

    /// The List render mode: one expandable card per document with per-document
    /// Edit / Clone / Delete actions on hover. An expanded card shows its fields as
    /// a selectable, copyable [`SelectableLabel`] block (held in
    /// `current.list_labels`, keyed by row index). Rows are drawn through a
    /// variable-height virtualized [`gpui::list`], so only on-screen cards are laid
    /// out (60fps on a full page).
    fn render_doc_list(
        &self,
        session: SessionId,
        current: &CollView,
        read_only: bool,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        // Per-row data the list closure indexes into (built once per render, not
        // per painted frame).
        let rows: Rc<Vec<DocCardRow>> = Rc::new(
            current
                .docs
                .iter()
                .enumerate()
                .map(|(i, doc)| DocCardRow {
                    id_label: SharedString::from(format!("_id: {}", doc.id.to_cell(CELL_CAP))),
                    expanded: current.expanded_rows.contains(&i),
                    label: current.list_labels.get(&i).cloned(),
                })
                .collect(),
        );
        let row_h = theme.scale(28.);
        let text_size = theme.scale(12.);
        let theme = theme.clone();
        let view = view.clone();
        let list_el = list(current.list_state.clone(), move |ix, _window, _cx| {
            let Some(row) = rows.get(ix) else {
                return div().into_any_element();
            };
            let header = doc_list_header(
                ix,
                row.expanded,
                &row.id_label,
                session,
                read_only,
                row_h,
                &theme,
                &view,
            );
            let mut card = div()
                .flex()
                .flex_col()
                .border_b_1()
                .border_color(theme.border)
                .child(header);
            if row.expanded
                && let Some(label) = &row.label
            {
                card = card.child(
                    div()
                        .px_3()
                        .pb_2()
                        .pl(px(28.))
                        .font_family(theme.mono_family.clone())
                        .text_color(theme.text)
                        .child(label.clone()),
                );
            }
            card.into_any_element()
        })
        .size_full();

        div()
            .id("doc-list")
            .size_full()
            .flex()
            .flex_col()
            .min_h(px(0.))
            .text_size(text_size)
            .child(div().flex_1().min_h(px(0.)).child(list_el))
            .into_any_element()
    }

    /// The inspector as a floating panel docked to the right edge, over the grid
    /// (so the grid keeps its width). Its left edge is a grab strip that starts a
    /// resize drag; the tracking overlay lives in [`Self::render_doc_documents`].
    fn render_doc_inspector_overlay(
        &self,
        session: SessionId,
        current: &CollView,
        read_only: bool,
        inspector_w: Pixels,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let start = view.clone();
        let handle = div()
            .id("doc-inspector-handle")
            .group("doc-inspector-handle")
            .flex_shrink_0()
            .w(px(6.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_ew_resize()
            .on_mouse_down(
                MouseButton::Left,
                move |event: &gpui::MouseDownEvent, _, cx| {
                    let anchor = DragAnchor {
                        start_coord: event.position.x,
                        start_size: inspector_w,
                    };
                    start
                        .update(cx, |this, cx| {
                            if let Phase::Connected(a) = &mut this.phase {
                                a.inspector_drag = Some(anchor);
                            }
                            cx.notify();
                        })
                        .ok();
                },
            )
            .child(
                div()
                    .w(px(1.))
                    .h_full()
                    .bg(theme.border)
                    .group_hover("doc-inspector-handle", |s| s.bg(theme.accent)),
            );

        // The grab strip's own 1px line is the left divider, so the panel itself
        // draws no left border (that would double it up).
        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .w(inspector_w)
            .occlude()
            .flex()
            .bg(theme.bg_panel)
            .child(handle)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.render_doc_inspector(session, current, read_only, theme, view)),
            )
            .into_any_element()
    }

    /// The raw-document inspector. On a writable connection it's an editable
    /// extended-JSON editor with Save / Delete (⌘↵ saves); on a read-only one it
    /// falls back to the pretty-printed read-only view.
    fn render_doc_inspector(
        &self,
        session: SessionId,
        current: &CollView,
        read_only: bool,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let insert = current.inspector_insert;
        let title = if insert { "New document" } else { "Document" };

        let close_view = view.clone();
        let close = Button::new("doc-inspector-close", "Close")
            .size(ButtonSize::Sm)
            .variant(ButtonVariant::Ghost)
            .on_click(move |_, _, cx| {
                close_view
                    .update(cx, |this, cx| this.doc_close_inspector(session, cx))
                    .ok();
            });

        let mut header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(title),
            );

        if read_only {
            return div()
                .flex()
                .flex_col()
                .size_full()
                .child(header.child(close))
                .child(self.render_doc_readonly_body(current, theme))
                .into_any_element();
        }

        // Form / Raw surface toggle (mirrors Compass's field-by-field vs JSON).
        let mode_view = view.clone();
        let mode_ix = match current.inspector_mode {
            InspectorMode::Form => 0,
            InspectorMode::Raw => 1,
        };
        header = header.child(
            Segmented::new("doc-inspector-mode")
                .segment("Form")
                .segment("Raw")
                .selected(mode_ix)
                .on_select(move |ix, _, cx| {
                    let mode = if ix == 1 {
                        InspectorMode::Raw
                    } else {
                        InspectorMode::Form
                    };
                    mode_view
                        .update(cx, |this, cx| {
                            this.doc_set_inspector_mode(session, mode, cx)
                        })
                        .ok();
                }),
        );

        let save_view = view.clone();
        header = header.child(
            Button::new("doc-save", if insert { "Insert" } else { "Save" })
                .size(ButtonSize::Sm)
                .variant(ButtonVariant::Primary)
                .on_click(move |_, _, cx| {
                    save_view
                        .update(cx, |this, cx| this.doc_save_document(session, cx))
                        .ok();
                }),
        );
        if !insert {
            let inspected = current.inspector;
            let delete_view = view.clone();
            header = header.child(
                Button::new("doc-delete", "Delete")
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Danger)
                    .on_click(move |_, _, cx| {
                        if let Some(row) = inspected {
                            delete_view
                                .update(cx, |this, cx| this.doc_delete_row(session, row, cx))
                                .ok();
                        }
                    }),
            );
        }
        header = header.child(close);

        let body = match current.inspector_mode {
            InspectorMode::Form => self.render_doc_form(session, current, view, theme),
            InspectorMode::Raw => div()
                .flex_1()
                .min_h(px(0.))
                .child(current.inspector_editor.clone())
                .into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// The read-only pretty-printed document view, shown on read-only
    /// connections in place of the editor.
    fn render_doc_readonly_body(&self, current: &CollView, theme: &Theme) -> gpui::AnyElement {
        const MAX_LINES: usize = 5_000;
        let json = current
            .inspector
            .and_then(|row| current.docs.get(row))
            .map(|d| pretty_extjson(&d.to_doc_value()))
            .unwrap_or_default();
        let lines: Vec<SharedString> = json
            .lines()
            .take(MAX_LINES)
            .map(|l| SharedString::from(l.to_string()))
            .collect();
        div()
            .id("doc-inspector-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_scroll()
            .p_3()
            .flex()
            .flex_col()
            .font_family(theme.mono_family.clone())
            .text_size(theme.scale(12.))
            .text_color(theme.text)
            .children(
                lines
                    .into_iter()
                    .map(|line| div().flex_shrink_0().child(line)),
            )
            .into_any_element()
    }
}

// --- panel bodies (free, no `&self` needed) ----------------------------------

/// The blank-tab body: a hint pointing at the sidebar.
fn render_doc_empty(theme: &Theme) -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .text_color(theme.text_faint)
        .child(crate::icons::icon(
            "table",
            theme.scale(28.),
            theme.text_faint,
        ))
        .child("Select a collection from the sidebar to open it here.")
        .into_any_element()
}

/// A centered muted hint filling the panel body (loading / empty states).
fn doc_centered_hint(text: &str, theme: &Theme) -> gpui::AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h(px(0.))
        .items_center()
        .justify_center()
        .text_color(theme.text_faint)
        .child(text.to_string())
        .into_any_element()
}

/// One three-column row (Field | middle | trailing), shared by the schema and
/// index panels. `header` styles it as the muted, bordered column header.
fn doc_row3(
    lead: impl Into<SharedString>,
    middle: impl Into<SharedString>,
    trail: impl Into<SharedString>,
    theme: &Theme,
    header: bool,
) -> gpui::AnyElement {
    let color = if header { theme.text_muted } else { theme.text };
    div()
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py(px(5.))
        .when(header, |d| d.border_b_1().border_color(theme.border))
        .child(
            div()
                .w(px(240.))
                .flex_shrink_0()
                .truncate()
                .text_color(color)
                .child(lead.into()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_color(color)
                .child(middle.into()),
        )
        .child(
            div()
                .w(px(90.))
                .flex_shrink_0()
                .text_color(theme.text_muted)
                .child(trail.into()),
        )
        .into_any_element()
}

/// The Schema panel: one row per inferred field path with its type distribution
/// (`string 82% . int 18%`) and present-ratio, or a hint while the sample loads.
fn render_doc_schema_panel(current: &CollView, theme: &Theme) -> gpui::AnyElement {
    let Some(schema) = current.schema.as_ref() else {
        return doc_centered_hint("Sampling schema...", theme);
    };
    if schema.fields.is_empty() {
        return doc_centered_hint("No fields sampled.", theme);
    }
    let rows = schema.fields.iter().map(|f| {
        let total: u64 = f.types.iter().map(|(_, c)| c).sum();
        let types = f
            .types
            .iter()
            .map(|(t, c)| {
                let pct = if total > 0 {
                    (*c as f64 * 100.0 / total as f64).round() as u64
                } else {
                    0
                };
                format!("{} {pct}%", t.label())
            })
            .collect::<Vec<_>>()
            .join("  \u{b7}  ");
        let present = format!("{:.0}%", f.present_ratio * 100.0);
        doc_row3(f.path.clone(), types, present, theme, false)
    });
    div()
        .id("doc-schema")
        .size_full()
        .overflow_y_scroll()
        .text_size(theme.scale(12.))
        .child(doc_row3("Field", "Types", "Present", theme, true))
        .children(rows)
        .child(
            div()
                .px_3()
                .py_2()
                .text_color(theme.text_faint)
                .child(format!("sampled {} documents", schema.sampled)),
        )
        .into_any_element()
}

/// The Indexes panel: one row per index with its keys and properties, or a hint
/// while the list loads.
fn render_doc_indexes_panel(current: &CollView, theme: &Theme) -> gpui::AnyElement {
    let Some(indexes) = current.indexes.as_ref() else {
        return doc_centered_hint("Loading indexes...", theme);
    };
    if indexes.is_empty() {
        return doc_centered_hint("No indexes.", theme);
    }
    let rows = indexes.iter().map(|idx| {
        let keys = idx
            .keys
            .iter()
            .map(|(field, order)| format!("{field}: {order}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut props = Vec::new();
        if idx.unique {
            props.push("unique".to_string());
        }
        if idx.sparse {
            props.push("sparse".to_string());
        }
        if idx.partial {
            props.push("partial".to_string());
        }
        if let Some(ttl) = idx.ttl {
            props.push(format!("ttl {ttl}s"));
        }
        doc_row3(idx.name.clone(), keys, props.join(", "), theme, false)
    });
    div()
        .id("doc-indexes")
        .size_full()
        .overflow_y_scroll()
        .text_size(theme.scale(12.))
        .child(doc_row3("Index", "Keys", "Properties", theme, true))
        .children(rows)
        .into_any_element()
}

/// A read-only sampled-column table over a document window, used by the Query
/// results panel.
fn render_docs_table(
    id: &'static str,
    docs: &[Document],
    columns: &[String],
    scroll: &UniformListScrollHandle,
    theme: &Theme,
) -> gpui::AnyElement {
    let cols: Vec<Column> = columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            if i == 0 {
                Column::new(name.clone()).width(px(220.))
            } else {
                Column::new(name.clone()).flex()
            }
        })
        .collect();
    let render_docs = Rc::new(docs.to_vec());
    let render_cols = Rc::new(columns.to_vec());
    let text = theme.text;
    let faint = theme.text_faint;
    Table::<()>::new(id, cols)
        .row_count(docs.len())
        .grid_lines(true)
        .text_size(theme.scale(12.))
        .track_scroll(scroll)
        .render_row(move |ix, _, _| {
            let Some(doc) = render_docs.get(ix) else {
                return Vec::new();
            };
            render_cols
                .iter()
                .map(|col| match cell_string(doc, col) {
                    Some(t) => div()
                        .min_w_0()
                        .truncate()
                        .text_color(text)
                        .child(t)
                        .into_any_element(),
                    None => div().text_color(faint).child("\u{2014}").into_any_element(),
                })
                .collect()
        })
        .into_any_element()
}

/// One List-mode card's data, indexed by the virtualized list's item closure.
struct DocCardRow {
    id_label: SharedString,
    expanded: bool,
    /// The selectable field block, present once the card has been expanded.
    label: Option<gpui::Entity<flint::SelectableLabel>>,
}

/// The JSON render mode: one selectable [`SelectableLabel`] per document (its
/// pretty extended JSON), drawn through a variable-height virtualized
/// [`gpui::list`] (`current.json_list` / `current.json_labels`). Only on-screen
/// documents are laid out, so scrolling a full page stays smooth, and the text
/// stays selectable + copyable.
fn render_doc_json(current: &CollView, theme: &Theme) -> gpui::AnyElement {
    let labels = current.json_labels.clone();
    let (mono, text_size, text, border) = (
        theme.mono_family.clone(),
        theme.scale(12.),
        theme.text,
        theme.border,
    );
    let list_el = list(current.json_list.clone(), move |ix, _window, _cx| {
        let Some(label) = labels.get(ix) else {
            return div().into_any_element();
        };
        div()
            .w_full()
            .px_3()
            .py_2()
            .when(ix > 0, |d| d.border_t_1().border_color(border))
            .font_family(mono.clone())
            .text_size(text_size)
            .text_color(text)
            .child(label.clone())
            .into_any_element()
    })
    .size_full();
    div()
        .id("doc-json")
        .size_full()
        .flex()
        .flex_col()
        .min_h(px(0.))
        .child(div().flex_1().min_h(px(0.)).child(list_el))
        .into_any_element()
}

/// The explain readout strip: a headline that flags a `COLLSCAN` (red) or names
/// the index used (green), the examined/returned counts, the winning-plan stage
/// chain, and a Close button.
fn render_explain_box(
    session: SessionId,
    plan: &DocPlan,
    theme: &Theme,
    view: &WeakEntity<AppState>,
) -> gpui::AnyElement {
    let (headline, color) = if plan.collscan {
        ("COLLSCAN - no index used".to_string(), theme.red)
    } else if let Some(ix) = &plan.index_used {
        (format!("uses index {ix}"), theme.green)
    } else {
        ("indexed plan".to_string(), theme.text)
    };
    let stats = match (plan.docs_examined, plan.n_returned) {
        (Some(e), Some(r)) => format!("examined {e}, returned {r}"),
        (Some(e), None) => format!("examined {e}"),
        _ => String::new(),
    };
    let stage_line = plan
        .stages
        .iter()
        .map(|s| match &s.detail {
            Some(detail) => format!("{}({detail})", s.stage),
            None => s.stage.clone(),
        })
        .collect::<Vec<_>>()
        .join("  \u{203a}  ");

    let close_view = view.clone();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2()
        .flex_shrink_0()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(color)
                        .child(headline),
                )
                .child(div().flex_1())
                .child(div().text_color(theme.text_muted).child(stats))
                .child(
                    Button::new("doc-explain-close", "Close")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Ghost)
                        .on_click(move |_, _, cx| {
                            close_view
                                .update(cx, |this, cx| this.doc_dismiss_explain(session, cx))
                                .ok();
                        }),
                ),
        )
        .child(
            div()
                .text_color(theme.text_muted)
                .text_size(theme.scale(11.))
                .child(stage_line),
        )
        .into_any_element()
}

// --- free helpers ------------------------------------------------------------

/// Render one collection-tree row's content (icon + label + badges); the Flint
/// `Tree` draws the chevron, indent, and selection highlight around it. A
/// collection open in a tab is tinted with the accent.
fn render_doc_tree_row(row: &DocTreeRow, cx: &App) -> gpui::AnyElement {
    let theme = cx.theme();
    let (text, muted, faint, accent) =
        (theme.text, theme.text_muted, theme.text_faint, theme.accent);
    let icon_size = theme.scale(12.);
    match &row.kind {
        DocTreeKind::Db { name } => div()
            .flex()
            .flex_1()
            .items_center()
            .gap_1()
            .child(crate::icons::icon("database", icon_size, muted))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(text)
                    .child(name.clone()),
            )
            .into_any_element(),
        DocTreeKind::Coll {
            name,
            kind,
            count,
            open,
        } => {
            let name_color = if *open { accent } else { text };
            let icon_color = if *open { accent } else { muted };
            div()
                .flex()
                .flex_1()
                .items_center()
                .gap_1()
                .child(crate::icons::icon("table", icon_size, icon_color))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_color(name_color)
                        .child(name.clone()),
                )
                .children(coll_kind_badge(*kind).map(|label| {
                    div()
                        .text_color(faint)
                        .text_size(theme.scale(10.))
                        .child(label)
                }))
                .child(div().text_color(faint).child(fmt_count(*count)))
                .into_any_element()
        }
        DocTreeKind::Placeholder(txt) => div()
            .flex()
            .flex_1()
            .items_center()
            .text_color(faint)
            .child(*txt)
            .into_any_element(),
    }
}

/// A short badge label for a non-plain collection kind (a view or time-series),
/// or `None` for an ordinary collection.
fn coll_kind_badge(kind: CollKind) -> Option<&'static str> {
    match kind {
        CollKind::Collection => None,
        CollKind::View => Some("view"),
        CollKind::Timeseries => Some("ts"),
    }
}

/// Build one List-mode document header row: the expand chevron + `_id`, and the
/// Edit / Clone / Delete actions on hover.
#[allow(clippy::too_many_arguments)]
fn doc_list_header(
    doc_ix: usize,
    expanded: bool,
    id_label: &SharedString,
    session: SessionId,
    read_only: bool,
    row_h: Pixels,
    theme: &Theme,
    view: &WeakEntity<AppState>,
) -> gpui::AnyElement {
    let i = doc_ix;
    let chevron = if expanded { "chevron-down" } else { "chevron" };
    let group = SharedString::from(format!("doc-card-{i}"));
    // Only the chevron + `_id` region toggles expansion, so the action buttons
    // stay click-safe.
    let toggle_view = view.clone();
    let toggle_region = div()
        .id(("doc-card-toggle", i))
        .flex()
        .items_center()
        .gap_2()
        .flex_1()
        .min_w_0()
        .cursor_pointer()
        .on_click(move |_, _, cx| {
            toggle_view
                .update(cx, |this, cx| this.doc_toggle_row(session, i, cx))
                .ok();
        })
        .child(crate::icons::icon(
            chevron,
            theme.scale(11.),
            theme.text_muted,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme.mono_family.clone())
                .text_color(theme.text)
                .child(id_label.clone()),
        );
    let edit_view = view.clone();
    let mut actions = div()
        .flex()
        .items_center()
        .gap_1()
        .invisible()
        .group_hover(group.clone(), |s| s.visible())
        .child(
            Button::new(("doc-card-edit", i), "Edit")
                .size(ButtonSize::Sm)
                .variant(ButtonVariant::Ghost)
                .on_click(move |_, _, cx| {
                    edit_view
                        .update(cx, |this, cx| this.doc_toggle_inspector(session, i, cx))
                        .ok();
                }),
        );
    if !read_only {
        let clone_view = view.clone();
        let del_view = view.clone();
        actions = actions
            .child(
                Button::new(("doc-card-clone", i), "Clone")
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Ghost)
                    .on_click(move |_, _, cx| {
                        clone_view
                            .update(cx, |this, cx| this.doc_clone_document(session, i, cx))
                            .ok();
                    }),
            )
            .child(
                Button::new(("doc-card-del", i), "Delete")
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Danger)
                    .on_click(move |_, _, cx| {
                        del_view
                            .update(cx, |this, cx| this.doc_delete_row(session, i, cx))
                            .ok();
                    }),
            );
    }
    div()
        .h(row_h)
        .group(group)
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .hover(|s| s.bg(theme.bg_hover))
        .child(toggle_region)
        .child(actions)
        .into_any_element()
}
