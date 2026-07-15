//! The Redis keyspace browser's render layer (guidelines D): the ~20 `render_*`
//! methods split out of `kvbrowse/mod.rs` so the scan/inspect/edit *logic* and the
//! view are separate concerns. A second `impl AppState` block; the state types and
//! free helpers it reads live on the parent module (`use super::*`).

use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use flint::prelude::*;
use gpui::{
    Context, Hsla, MouseButton, SharedString, WeakEntity, Window, div, point, prelude::*, px,
};
use red_core::kv::{
    CollectionKind, KeyMeta, KvCollection, KvElement, KvType, KvValue, PendingEntry, StreamEntry,
};
use red_service::SessionId;

use crate::app::{ActiveConn, AppState, Phase};

use super::*;

impl AppState {
    /// The keyspace browser's body: filter box + header stat + the
    /// virtualized key list. Called from `render_redis_shell`.
    pub(crate) fn render_kv_browse(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let session = active.session;
        let Some(view_ref) = active.kv_view.as_ref() else {
            return div().flex_1();
        };
        let Some(browse) = view_ref.browse_at(tab_idx) else {
            return div().flex_1();
        };

        let writable = !active.config.read_only;
        let fuzzy_query = browse.filter.read(cx).content().to_string();
        let rows: Rc<Vec<KeyMeta>> = browse.visible_rows(cx);

        // Favourite / tag filter state for the toolbar controls. The tag dropdown
        // only appears when this connection actually has tagged keys.
        let fav_only = browse.fav_only;
        let tag_filter = browse.tag_filter.clone();
        let tag_open = browse.tag_filter_open;
        let all_tags = self.redis_key_meta.all_tags(&active.conn_id);

        // In tree mode the list is a flattened namespace trie (folders + keys);
        // in grid mode it's the raw key rows (no per-row allocation — the flat
        // grid's hot path). `disp` is `Some` only in tree mode.
        let tree_mode = browse.tree_mode;
        let disp: Option<Rc<Vec<DispRow>>> = tree_mode.then(|| browse.tree_rows(&rows));
        let row_count = match &disp {
            Some(d) => d.len(),
            None => rows.len(),
        };

        let rows_render = rows.clone();
        let rows_select = rows.clone();
        let rows_menu = rows.clone();
        let disp_render = disp.clone();
        let disp_select = disp.clone();
        let disp_menu = disp.clone();
        // Starred keys for this connection: a ★ prefixes their name in the list.
        let favorites: Rc<HashSet<String>> =
            Rc::new(self.redis_key_meta.favorites(&active.conn_id));
        let fav_render = favorites.clone();
        let visible_range_view = view.clone();
        let select_view = view.clone();
        let menu_view = view.clone();
        let nav_view = view.clone();
        let list_focus = browse.list_focus.clone();

        // The keyboard cursor drives the grid highlight once the list has been
        // touched; before that (and always in tree mode, where nav is disabled)
        // it falls back to the inspected key's row.
        let selected_ix = match &disp {
            Some(d) => browse.inspector.as_ref().and_then(|insp| {
                d.iter().position(
                    |r| matches!(r, DispRow::Key { row, .. } if rows.get(*row).is_some_and(|m| m.key == insp.key)),
                )
            }),
            None => browse.nav_row.filter(|&i| i < row_count).or_else(|| {
                browse
                    .inspector
                    .as_ref()
                    .and_then(|i| rows.iter().position(|r| r.key == i.key))
            }),
        };

        let columns = vec![
            Column::new("Key").flex(),
            Column::new("Type").width(px(72.)),
            Column::new("TTL").width(px(110.)).align_end(),
            Column::new("Size").width(px(80.)).align_end(),
            Column::new("Encoding").width(px(110.)),
        ];

        let key_color = theme.text;
        let dim_color = theme.text_muted;
        let cell_size = theme.scale(12.);
        let row_theme = theme.clone();
        // Per-depth indent; leaves align one chevron-width past their folder.
        let indent = |depth: usize| px(depth as f32 * 14.);

        let table = Table::<()>::new("kv-browse", columns)
            .row_count(row_count)
            .grid_lines(true)
            .text_size(cell_size)
            .track_scroll(&browse.scroll)
            .selected(selected_ix)
            .focus_handle(list_focus)
            .on_nav(move |nav, _extend, _window, cx| {
                // No-op in tree mode (see `kv_browse_nav`): the tree is click-driven.
                nav_view
                    .update(cx, |this, cx| this.kv_browse_nav(session, nav, cx))
                    .ok();
            })
            .on_select(move |ix, _click, window, cx| {
                // A folder toggles; a key (tree leaf or grid row) opens the inspector.
                let key_row = match &disp_select {
                    Some(d) => match d.get(ix) {
                        Some(DispRow::Folder { prefix, .. }) => {
                            let prefix = prefix.clone();
                            select_view
                                .update(cx, |this, cx| {
                                    this.kv_toggle_tree_node(session, prefix, cx)
                                })
                                .ok();
                            return;
                        }
                        Some(DispRow::Key { row, .. }) => rows_select.get(*row),
                        None => return,
                    },
                    None => rows_select.get(ix),
                };
                let Some(row) = key_row else { return };
                let (key, ttl, kv_type) = (row.key.clone(), row.ttl, row.kv_type.clone());
                let grid_ix = disp_select.is_none().then_some(ix);
                select_view
                    .update(cx, |this, cx| {
                        // A grid click plants the keyboard cursor and focuses the
                        // list so arrows continue from it; the tree has no cursor.
                        if let Some(gi) = grid_ix
                            && let Some(b) = this
                                .conn_mut(Some(session))
                                .and_then(|a| a.kv_view.as_mut())
                                .and_then(|v| v.active_browse_mut())
                        {
                            b.nav_row = Some(gi);
                            window.focus(&b.list_focus, cx);
                        }
                        this.kv_open_inspector(session, key, ttl, kv_type, cx)
                    })
                    .ok();
            })
            .on_secondary(move |ix, pos, _window, cx| {
                // Folders carry no context menu; keys do (rename / TTL / delete / …).
                let key_row = match &disp_menu {
                    Some(d) => match d.get(ix) {
                        Some(DispRow::Key { row, .. }) => rows_menu.get(*row),
                        _ => None,
                    },
                    None => rows_menu.get(ix),
                };
                let Some(row) = key_row else { return };
                let (key, kv_type, ttl) = (row.key.clone(), row.kv_type.clone(), row.ttl);
                menu_view
                    .update(cx, |this, cx| {
                        this.kv_open_key_menu(session, key, kv_type, ttl, pos, cx)
                    })
                    .ok();
            })
            .render_row(move |ix, _window, _cx| {
                // Tree mode: render a folder or an indented leaf.
                if let Some(d) = &disp_render {
                    return match d.get(ix) {
                        Some(DispRow::Folder {
                            label,
                            count,
                            expanded,
                            depth,
                            ..
                        }) => {
                            let chevron = if *expanded { "chevron-down" } else { "chevron" };
                            vec![
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .pl(indent(*depth))
                                    .child(crate::icons::icon(chevron, cell_size, dim_color))
                                    .child(div().text_color(key_color).child(label.clone()))
                                    .into_any_element(),
                                div().into_any_element(),
                                div().into_any_element(),
                                div()
                                    .text_color(dim_color)
                                    .child(format!("{count}"))
                                    .into_any_element(),
                                div().into_any_element(),
                            ]
                        }
                        Some(DispRow::Key { row, label, depth }) => {
                            let Some(meta) = rows_render.get(*row) else {
                                return Vec::new();
                            };
                            let shown = if fav_render.contains(&meta.key) {
                                format!("★ {label}")
                            } else {
                                label.clone()
                            };
                            vec![
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(key_color)
                                    .pl(px(*depth as f32 * 14. + 16.))
                                    .child(shown)
                                    .into_any_element(),
                                type_pill(&meta.kv_type, &row_theme).into_any_element(),
                                div()
                                    .text_color(dim_color)
                                    .child(fmt_ttl(meta.ttl))
                                    .into_any_element(),
                                div()
                                    .text_color(dim_color)
                                    .child(fmt_bytes(meta.approx_bytes))
                                    .into_any_element(),
                                div()
                                    .text_color(dim_color)
                                    .truncate()
                                    .child(meta.encoding.clone())
                                    .into_any_element(),
                            ]
                        }
                        None => Vec::new(),
                    };
                }
                // Grid mode: the raw key row (unchanged hot path).
                let Some(row) = rows_render.get(ix) else {
                    return Vec::new();
                };
                let shown = if fav_render.contains(&row.key) {
                    format!("★ {}", row.key)
                } else {
                    row.key.clone()
                };
                vec![
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(key_color)
                        .child(shown)
                        .into_any_element(),
                    type_pill(&row.kv_type, &row_theme).into_any_element(),
                    div()
                        .text_color(dim_color)
                        .child(fmt_ttl(row.ttl))
                        .into_any_element(),
                    div()
                        .text_color(dim_color)
                        .child(fmt_bytes(row.approx_bytes))
                        .into_any_element(),
                    div()
                        .text_color(dim_color)
                        .truncate()
                        .child(row.encoding.clone())
                        .into_any_element(),
                ]
            })
            .on_visible_range(move |range, _window, cx| {
                visible_range_view
                    .update(cx, |this, cx| {
                        this.kv_maybe_load_more(session, range.end, cx)
                    })
                    .ok();
            });

        let new_key_button = writable.then(|| {
            let new_view = view.clone();
            Button::new("kv-new-key", "+ New key")
                .size(ButtonSize::Sm)
                .variant(ButtonVariant::Secondary)
                .on_click(move |_, _, cx| {
                    new_view
                        .update(cx, |this, cx| this.kv_open_create_key(session, cx))
                        .ok();
                })
        });

        // Auto-refresh gets its own toolbar control (promoted out of the actions
        // menu): a pill whose left region toggles auto-refresh on/off and whose
        // right caret opens the interval popover. Accent-tinted + interval-labelled
        // while live, so the toolbar shows the tab is refreshing at a glance.
        let auto_on = browse.auto_refresh.is_some();
        let auto_secs = browse.auto_refresh.map(|d| d.as_secs());
        let auto_hue = if auto_on {
            theme.accent
        } else {
            theme.text_muted
        };
        let toggle_view = view.clone();
        let caret_view = view.clone();
        let auto_button = div()
            .flex()
            .items_center()
            .h(px(24.))
            .rounded(px(5.))
            .border_1()
            .border_color(if auto_on { theme.accent } else { theme.border })
            .bg(theme.bg_elevated)
            .text_size(theme.scale(12.))
            .child(
                div()
                    .id("kv-auto-toggle")
                    .flex()
                    .items_center()
                    .gap_1()
                    .h_full()
                    .px_1p5()
                    .cursor_pointer()
                    .text_color(auto_hue)
                    .hover(|s| s.bg(theme.bg_hover))
                    .tooltip(Tooltip::text(if auto_on {
                        "Auto-refresh on — click to turn off"
                    } else {
                        "Auto-refresh off — click to turn on"
                    }))
                    .child(crate::icons::icon("refresh-cw", theme.scale(13.), auto_hue))
                    .when_some(auto_secs, |s, secs| s.child(format!("{secs}s")))
                    .on_click(move |_, _, cx| {
                        toggle_view
                            .update(cx, |this, cx| this.kv_toggle_auto_refresh(session, cx))
                            .ok();
                    }),
            )
            .child(div().w(px(1.)).h(px(14.)).bg(theme.border))
            .child(
                div()
                    .id("kv-auto-caret")
                    .flex()
                    .items_center()
                    .h_full()
                    .px_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.bg_hover))
                    .tooltip(Tooltip::text("Auto-refresh interval"))
                    .child(crate::icons::icon(
                        "chevron-down",
                        theme.scale(12.),
                        theme.text_muted,
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        move |event: &gpui::MouseDownEvent, _, cx| {
                            let pos = event.position;
                            caret_view
                                .update(cx, |this, cx| this.kv_open_auto_menu(session, pos, cx))
                                .ok();
                        },
                    ),
            );

        // The actions dropdown: Refresh · Import · Expand/Collapse all · Find
        // biggest keys. A button-shaped trigger opening a positioned `ContextMenu`.
        let actions_view = view.clone();
        let actions_button = div()
            .id("kv-actions-btn")
            .flex()
            .items_center()
            .gap_1()
            .h(px(24.))
            .px_2()
            .rounded(px(5.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg_elevated)
            .text_size(theme.scale(12.))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_hover))
            .child("Actions")
            .child(crate::icons::icon(
                "chevron-down",
                theme.scale(12.),
                theme.text_muted,
            ))
            .on_mouse_down(
                MouseButton::Left,
                move |event: &gpui::MouseDownEvent, _, cx| {
                    let pos = event.position;
                    actions_view
                        .update(cx, |this, cx| this.kv_open_actions_menu(session, pos, cx))
                        .ok();
                },
            );

        // An empty key list gets a settled message instead of a blank grid, so
        // "no matches" reads as deliberate (mirrors the other panels' empties).
        let meta_filter = browse.fav_only || browse.tag_filter.is_some();
        let has_filter = browse.pattern.is_some()
            || browse.type_filter.is_some()
            || browse.ttl_filter.is_some()
            || browse.value_needle.is_some()
            || meta_filter
            || (browse.is_fuzzy() && !fuzzy_query.is_empty());
        let empty_msg = if browse.loading {
            "Scanning…"
        } else if browse.mode == QueryMode::Exact && !fuzzy_query.is_empty() {
            "No key with that exact name"
        } else if browse.value_needle.is_some() {
            "No string values match this search"
        } else if browse.fav_only && !browse.exhausted {
            "No favourites among the loaded keys yet — still scanning, or narrow with a prefix"
        } else if browse.fav_only {
            "No favourite keys here yet — star keys from the preview or right-click menu"
        } else if (browse.ttl_filter.is_some() || browse.tag_filter.is_some()) && !browse.exhausted
        {
            // A client-side filter only sees the loaded window; be explicit that a
            // deeper match may exist rather than implying none do.
            "No match among the loaded keys yet — still scanning, or narrow with a prefix"
        } else if has_filter {
            "No keys match this filter"
        } else {
            "No keys in this database"
        };
        let key_area = if row_count == 0 {
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme.scale(12.))
                .text_color(theme.text_muted)
                .child(empty_msg)
        } else {
            div().flex_1().min_w(px(0.)).child(table)
        };

        let main = match &browse.big_keys {
            // Live browse: the key list, plus — when a key is selected — the
            // preview inspector docked right in a resizable split (the trailing
            // pane carries the user-set width, like the SQL detail inspector).
            None => {
                let inspector_el = browse.inspector.as_ref().map(|inspector| {
                    self.render_kv_inspector(
                        session,
                        inspector,
                        !active.config.read_only,
                        &theme,
                        cx,
                    )
                });
                match inspector_el {
                    Some(inspector_el) => {
                        let (start, resize, end) = (view.clone(), view.clone(), view.clone());
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .child(
                                SplitPane::new("kv-split-inspector", gpui::Axis::Horizontal)
                                    .sized(SplitSide::Trailing)
                                    .size(active.inspector_w)
                                    .gutter(px(1.))
                                    .drag(active.inspector_drag)
                                    .min_first(px(240.))
                                    .max_first(px(900.))
                                    .on_drag_start(move |anchor, _, cx| {
                                        start
                                            .update(cx, |this, cx| {
                                                if let Phase::Connected(a) = &mut this.phase {
                                                    a.inspector_drag = Some(anchor);
                                                }
                                                cx.notify();
                                            })
                                            .ok();
                                    })
                                    .on_resize(move |size, _, cx| {
                                        resize
                                            .update(cx, |this, cx| {
                                                if let Phase::Connected(a) = &mut this.phase {
                                                    a.inspector_w = size;
                                                }
                                                cx.notify();
                                            })
                                            .ok();
                                    })
                                    .on_drag_end(move |_, cx| {
                                        end.update(cx, |this, cx| {
                                            if let Phase::Connected(a) = &mut this.phase {
                                                a.inspector_drag = None;
                                            }
                                            cx.notify();
                                        })
                                        .ok();
                                    })
                                    // A flex wrapper so `key_area`'s `flex_1`
                                    // stretches to the pane's full height (a plain
                                    // block parent would leave the table at auto
                                    // height — i.e. collapsed to 0).
                                    .first(div().size_full().flex().min_h(px(0.)).child(key_area))
                                    .second(inspector_el),
                            )
                            .into_any_element()
                    }
                    None => div()
                        .flex_1()
                        .min_h(px(0.))
                        .flex()
                        .child(key_area)
                        .into_any_element(),
                }
            }
            Some(bk) => self
                .render_big_keys(
                    session,
                    bk,
                    browse.inspector.as_ref(),
                    !active.config.read_only,
                    &theme,
                    cx,
                )
                .into_any_element(),
        };

        div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .child({
                        // The combined search field: `[ mode ▾ │ filter… ]` as one
                        // bordered unit. The leading `seamless` Select picks how the
                        // box text is read (Glob / Prefix / Exact / Fuzzy / Value)
                        // and drives the input's placeholder; the `bare()` input
                        // (see `BrowseState::new`) fills the rest. The container owns
                        // the border/background so the two read as a single control.
                        let toggle_view = view.clone();
                        let select_view = view.clone();
                        let selected_ix = QueryMode::ALL
                            .iter()
                            .position(|m| *m == browse.mode)
                            .unwrap_or(0);
                        let mut mode_select = Select::new("kv-query-mode").accent(false).seamless();
                        for m in QueryMode::ALL.iter() {
                            mode_select = mode_select.option(m.label().to_string());
                        }
                        let mode_select = mode_select
                            .selected(selected_ix)
                            .open(browse.mode_open)
                            .on_toggle(move |_, cx| {
                                toggle_view
                                    .update(cx, |this, cx| this.kv_toggle_mode_menu(session, cx))
                                    .ok();
                            })
                            .on_select(move |ix, _, cx| {
                                let Some(mode) = QueryMode::ALL.get(ix).copied() else {
                                    return;
                                };
                                select_view
                                    .update(cx, |this, cx| {
                                        this.kv_set_query_mode(session, mode, cx)
                                    })
                                    .ok();
                            });
                        div()
                            .flex()
                            .flex_1()
                            .items_center()
                            .min_w(px(180.))
                            .h(px(28.))
                            .rounded(theme.radius)
                            .bg(theme.bg_input)
                            .border_1()
                            .border_color(if browse.mode_open {
                                theme.border_strong
                            } else {
                                theme.border
                            })
                            .child(mode_select)
                            .child(div().flex_shrink_0().w(px(1.)).h(px(16.)).bg(theme.border))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(80.))
                                    .px_2()
                                    // The `bare()` input inherits the ambient text
                                    // size, so set it here to match the mode label.
                                    .text_size(theme.font_size)
                                    .child(browse.filter.clone()),
                            )
                    })
                    .child({
                        // Namespace tree ↔ flat grid view toggle. Groups keys by
                        // their `:` hierarchy without a re-scan.
                        let tree_view = view.clone();
                        IconButton::new(
                            "kv-tree-toggle",
                            crate::icons::icon(
                                "schema",
                                theme.scale(14.),
                                if tree_mode {
                                    theme.accent
                                } else {
                                    theme.text_muted
                                },
                            ),
                        )
                        .size(IconButtonSize::Sm)
                        .tooltip(if tree_mode {
                            "Namespace tree (on): keys grouped by their : hierarchy"
                        } else {
                            "Group keys into a namespace tree"
                        })
                        .a11y_label("Toggle namespace tree")
                        .on_click(move |_, _, cx| {
                            tree_view
                                .update(cx, |this, cx| this.kv_toggle_tree_mode(session, cx))
                                .ok();
                        })
                    })
                    .child({
                        // Server-side type filter (`SCAN ... TYPE`): index 0 is
                        // "All types", 1..=6 the concrete types in menu order.
                        // Composes with both the MATCH/fuzzy filter above.
                        let types = kv_filter_types();
                        let selected_ix = match &browse.type_filter {
                            None => 0,
                            Some(t) => types
                                .iter()
                                .position(|x| x == t)
                                .map(|i| i + 1)
                                .unwrap_or(0),
                        };
                        let toggle_view = view.clone();
                        let select_view = view.clone();
                        let mut select = Select::new("kv-type-filter")
                            .accent(false)
                            .option("All types");
                        for t in types.iter() {
                            select = select.option(t.label().to_string());
                        }
                        select
                            .selected(selected_ix)
                            .open(browse.type_filter_open)
                            .on_toggle(move |_, cx| {
                                toggle_view
                                    .update(cx, |this, cx| this.kv_toggle_type_menu(session, cx))
                                    .ok();
                            })
                            .on_select(move |ix, _, cx| {
                                let choice = ix
                                    .checked_sub(1)
                                    .and_then(|i| kv_filter_types().into_iter().nth(i));
                                select_view
                                    .update(cx, |this, cx| {
                                        this.kv_set_type_filter(session, choice, cx)
                                    })
                                    .ok();
                            })
                    })
                    .child({
                        // Client-side TTL filter (index 0 = "Any TTL", 1..=6 the
                        // `TtlFilter` buckets). Prunes the loaded rows at render
                        // time — Redis can't filter by expiry — so it composes with
                        // every server-side filter (see `kv_set_ttl_filter`).
                        let selected_ix = match &browse.ttl_filter {
                            None => 0,
                            Some(f) => TtlFilter::ALL
                                .iter()
                                .position(|x| x == f)
                                .map(|i| i + 1)
                                .unwrap_or(0),
                        };
                        let toggle_view = view.clone();
                        let select_view = view.clone();
                        let mut select =
                            Select::new("kv-ttl-filter").accent(false).option("Any TTL");
                        for f in TtlFilter::ALL.iter() {
                            select = select.option(f.label().to_string());
                        }
                        select
                            .selected(selected_ix)
                            .open(browse.ttl_filter_open)
                            .on_toggle(move |_, cx| {
                                toggle_view
                                    .update(cx, |this, cx| this.kv_toggle_ttl_menu(session, cx))
                                    .ok();
                            })
                            .on_select(move |ix, _, cx| {
                                let choice = ix
                                    .checked_sub(1)
                                    .and_then(|i| TtlFilter::ALL.get(i).copied());
                                select_view
                                    .update(cx, |this, cx| {
                                        this.kv_set_ttl_filter(session, choice, cx)
                                    })
                                    .ok();
                            })
                    })
                    .children((!all_tags.is_empty()).then(|| {
                        // Client-side tag filter (index 0 = "Any tag", 1.. the
                        // connection's tags). Only shown when tags exist, to keep
                        // the toolbar clean.
                        let selected_ix = tag_filter
                            .as_ref()
                            .and_then(|t| all_tags.iter().position(|x| x == t))
                            .map(|i| i + 1)
                            .unwrap_or(0);
                        let toggle_view = view.clone();
                        let select_view = view.clone();
                        let tag_opts = all_tags.clone();
                        let mut select =
                            Select::new("kv-tag-filter").accent(false).option("Any tag");
                        for t in all_tags.iter() {
                            select = select.option(t.clone());
                        }
                        select
                            .selected(selected_ix)
                            .open(tag_open)
                            .on_toggle(move |_, cx| {
                                toggle_view
                                    .update(cx, |this, cx| this.kv_toggle_tag_menu(session, cx))
                                    .ok();
                            })
                            .on_select(move |ix, _, cx| {
                                let choice =
                                    ix.checked_sub(1).and_then(|i| tag_opts.get(i).cloned());
                                select_view
                                    .update(cx, |this, cx| {
                                        this.kv_set_tag_filter(session, choice, cx)
                                    })
                                    .ok();
                            })
                    }))
                    .child({
                        // Favourites-only toggle (star). Accent/gold while active,
                        // mirroring the ★ prefix in the list.
                        let fav_view = view.clone();
                        IconButton::new(
                            "kv-fav-toggle",
                            crate::icons::icon(
                                if fav_only { "star-filled" } else { "star" },
                                theme.scale(14.),
                                if fav_only {
                                    theme.yellow
                                } else {
                                    theme.text_muted
                                },
                            ),
                        )
                        .size(IconButtonSize::Sm)
                        .active(fav_only)
                        .tooltip(if fav_only {
                            "Showing favourites only — click to show all"
                        } else {
                            "Show favourites only"
                        })
                        .a11y_label("Toggle favourites-only filter")
                        .on_click(move |_, _, cx| {
                            fav_view
                                .update(cx, |this, cx| this.kv_toggle_fav_only(session, cx))
                                .ok();
                        })
                    })
                    .children(new_key_button)
                    .child(auto_button)
                    .child(actions_button),
            )
            .child(main)
    }

    /// The "New key" modal (Flint [`Modal`] over a scrim): a name, a segmented
    /// type picker, and the seed inputs the form shows/hides as the type changes
    /// (a hash/stream field, a zset score, a string TTL, a list push end).
    /// Rendered at the window root (see `app::render`) so it overlays the whole
    /// shell; `None` when no browse tab has it open. Its writes reuse the same
    /// [`KvEdit`](red_core::kv::KvEdit) path as inline element editing (see
    /// [`AppState::kv_submit_create_key`]).
    pub(crate) fn render_kv_create_modal(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use crate::connect::labeled_field;

        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let session = active.session;
        let ck = active
            .kv_view
            .as_ref()?
            .active_browse()?
            .create_key
            .as_ref()?;
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();

        // Segmented type picker: choosing a type reshapes the fields below.
        let types = kv_creatable_types();
        let selected_ix = types.iter().position(|t| *t == ck.kv_type).unwrap_or(0);
        let type_view = view.clone();
        let type_picker = types
            .iter()
            .fold(Segmented::new("kv-create-type"), |seg, t| {
                seg.segment(t.label().to_string())
            })
            .selected(selected_ix)
            .on_select(move |ix, _, cx| {
                let choice = kv_creatable_types()
                    .into_iter()
                    .nth(ix)
                    .unwrap_or(KvType::String);
                type_view
                    .update(cx, |this, cx| this.kv_set_create_type(session, choice, cx))
                    .ok();
            });
        let hint = div()
            .text_size(theme.scale(11.))
            .text_color(theme.text_muted)
            .child(kv_create_hint(&ck.kv_type));

        // Per-type conditional fields.
        let show_field = matches!(ck.kv_type, KvType::Hash | KvType::Stream);
        let show_score = matches!(ck.kv_type, KvType::ZSet);
        let show_ttl = matches!(ck.kv_type, KvType::String);
        let list_end = matches!(ck.kv_type, KvType::List).then(|| {
            let head = ck.list_head;
            let toggle_view = view.clone();
            labeled_field("Push to", &theme).child(
                Segmented::new("kv-create-list-end")
                    .segment("Head (LPUSH)")
                    .segment("Tail (RPUSH)")
                    .selected(if head { 0 } else { 1 })
                    .on_select(move |ix, _, cx| {
                        // Each segment sets its end; only flip when it changes.
                        if (ix == 0) != head {
                            toggle_view
                                .update(cx, |this, cx| this.kv_toggle_create_list_head(session, cx))
                                .ok();
                        }
                    }),
            )
        });
        let error_line = ck.error.clone().map(|e| {
            div()
                .text_size(theme.scale(11.))
                .text_color(theme.red)
                .child(e)
        });

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(labeled_field("Key name", &theme).child(ck.name.clone()))
            .child(labeled_field("Type", &theme).child(type_picker).child(hint))
            .children(show_field.then(|| labeled_field("Field", &theme).child(ck.field.clone())))
            .child(labeled_field(kv_value_label(&ck.kv_type), &theme).child(ck.value.clone()))
            .children(show_score.then(|| labeled_field("Score", &theme).child(ck.score.clone())))
            .children(show_ttl.then(|| labeled_field("Expiry (TTL)", &theme).child(ck.ttl.clone())))
            .children(list_end)
            .children(error_line);

        let (save_view, cancel_view, close_view) = (view.clone(), view.clone(), view.clone());
        let footer = div()
            .flex()
            .flex_1()
            .justify_end()
            .gap_2()
            .child(
                Button::new("kv-create-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        cancel_view
                            .update(cx, |this, cx| this.kv_cancel_create_key(session, cx))
                            .ok();
                    }),
            )
            .child(
                Button::new("kv-create-save", "Create")
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        save_view
                            .update(cx, |this, cx| this.kv_submit_create_key(session, cx))
                            .ok();
                    }),
            );

        Some(
            Modal::new("kv-create-key")
                .title("New key")
                .width(px(440.))
                // The shared modal focus handle traps Tab and lets Esc close; the
                // name field is focused on open (see `focus_create_key`).
                .focus_handle(self.modal_focus.clone())
                .on_close(move |_, cx| {
                    close_view
                        .update(cx, |this, cx| this.kv_cancel_create_key(session, cx))
                        .ok();
                })
                .footer(footer)
                .child(body)
                .into_any_element(),
        )
    }

    /// The browse toolbar's actions dropdown (see [`AppState::kv_open_actions_menu`]):
    /// Refresh keys · Expand / Collapse all (tree mode) · an Auto-refresh submenu.
    /// A positioned `ContextMenu` over a full-bleed dismiss catcher, mirroring the
    /// tab/key menus.
    pub(crate) fn render_kv_actions_menu(
        &self,
        active: &ActiveConn,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let session = active.session;
        let writable = !active.config.read_only;
        let browse = active.kv_view.as_ref().and_then(|v| v.active_browse());
        let tree_mode = browse.map(|b| b.tree_mode).unwrap_or(false);
        // Expand-all is meaningless without namespaced keys (a `:` to group on).
        let has_folders = browse
            .map(|b| b.rows.iter().any(|m| m.key.contains(':')))
            .unwrap_or(false);

        let menu = ContextMenu::new("kv-actions-menu")
            .item(
                ContextMenuItem::new("kv-act-refresh", "Refresh keys")
                    .shortcut(crate::keymap::localize_hint("⌘R"))
                    .on_click(cx.listener(move |this, _, _, cx| this.kv_refresh_keys(session, cx))),
            )
            .item(
                ContextMenuItem::new("kv-act-big-keys", "Find biggest keys").on_click(
                    cx.listener(move |this, _, _, cx| this.kv_start_big_keys_sample(session, cx)),
                ),
            )
            .item(
                ContextMenuItem::new("kv-act-import", "Import keys…")
                    .disabled(!writable)
                    .on_click(cx.listener(move |this, _, _, cx| this.kv_open_import(session, cx))),
            )
            .separator()
            .item(
                ContextMenuItem::new("kv-act-expand", "Expand all")
                    .disabled(!tree_mode || !has_folders)
                    .on_click(cx.listener(move |this, _, _, cx| this.kv_expand_all(session, cx))),
            )
            .item(
                ContextMenuItem::new("kv-act-collapse", "Collapse all")
                    .disabled(!tree_mode)
                    .on_click(cx.listener(move |this, _, _, cx| this.kv_collapse_all(session, cx))),
            );

        // The dismiss catcher fills the shell; the menu itself floats via Flint's
        // `floating` helper, which snaps it inside the viewport — so a trigger near
        // the right edge no longer spills the menu off-screen. Anchored top-right so
        // it drops leftward from the right-aligned Actions button.
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.kv_close_actions_menu(session, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| this.kv_close_actions_menu(session, cx)),
            )
            .child(
                floating(div().occlude().child(menu.into_any_element()))
                    .at(pos)
                    .anchor(gpui::Anchor::TopRight),
            )
            .into_any_element()
    }

    /// The auto-refresh interval popover (Off · 2/5/10/30s) opened from the
    /// toolbar's auto-refresh caret. A positioned `ContextMenu` over a full-bleed
    /// dismiss catcher, mirroring [`Self::render_kv_actions_menu`]; a ✓ marks the
    /// active interval.
    pub(crate) fn render_kv_auto_menu(
        &self,
        active: &ActiveConn,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let session = active.session;
        let auto = active
            .kv_view
            .as_ref()
            .and_then(|v| v.active_browse())
            .and_then(|b| b.auto_refresh);

        let mut menu = ContextMenu::new("kv-auto-menu").item({
            let off = ContextMenuItem::new("kv-auto-off", "Off").on_click(
                cx.listener(move |this, _, _, cx| this.kv_set_auto_refresh(session, None, cx)),
            );
            if auto.is_none() {
                off.shortcut("✓")
            } else {
                off
            }
        });
        for secs in [2u64, 5, 10, 30] {
            let dur = Duration::from_secs(secs);
            let item = ContextMenuItem::new(
                SharedString::from(format!("kv-auto-{secs}")),
                SharedString::from(format!("Every {secs}s")),
            )
            .on_click(
                cx.listener(move |this, _, _, cx| this.kv_set_auto_refresh(session, Some(dur), cx)),
            );
            menu = menu.item(if auto == Some(dur) {
                item.shortcut("✓")
            } else {
                item
            });
        }

        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.kv_close_auto_menu(session, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| this.kv_close_auto_menu(session, cx)),
            )
            .child(
                floating(div().occlude().child(menu.into_any_element()))
                    .at(pos)
                    .anchor(gpui::Anchor::TopRight),
            )
            .into_any_element()
    }

    /// The "Import keys" modal (Flint [`Modal`], root-mounted like the New-key
    /// modal): choose a file of Redis commands, then run them as one batch (see
    /// [`AppState::kv_open_import`]). `None` when no connection has it open.
    pub(crate) fn render_kv_import_modal(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use crate::connect::labeled_field;

        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let session = active.session;
        let imp = active.kv_view.as_ref()?.import.as_ref()?;
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();

        let count = imp.commands.len();
        let can_import = count > 0 && !imp.running;

        let choose_view = view.clone();
        let choose_label = if imp.path.is_some() {
            "Change file…"
        } else {
            "Choose file…"
        };
        let choose = Button::new("kv-import-choose", choose_label)
            .variant(ButtonVariant::Secondary)
            .size(ButtonSize::Sm)
            .disabled(imp.running)
            .on_click(move |_, _, cx| {
                choose_view
                    .update(cx, |this, cx| this.kv_import_choose_file(session, cx))
                    .ok();
            });

        let file_row = labeled_field("File", &theme).child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(choose)
                .children(imp.path.clone().map(|p| {
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(theme.scale(12.))
                        .text_color(theme.text_muted)
                        .child(p)
                })),
        );

        // A count line once a file parses, or the inline error, or the running note.
        let status = if imp.running {
            Some(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text_muted)
                    .child("Importing…"),
            )
        } else if let Some(e) = imp.error.clone() {
            Some(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.red)
                    .child(e),
            )
        } else if count > 0 {
            Some(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(format!("{count} command(s) ready to run")),
            )
        } else {
            None
        };

        let hint = div()
            .text_size(theme.scale(11.))
            .text_color(theme.text_faint)
            .child(
                "A text file of Redis commands, one per line (e.g. SET user:1 alice). \
                 Blank lines and lines starting with # are ignored. Commands run in order.",
            );

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(hint)
            .child(file_row)
            .children(status);

        let (run_view, cancel_view, close_view) = (view.clone(), view.clone(), view.clone());
        let footer = div()
            .flex()
            .flex_1()
            .justify_end()
            .gap_2()
            .child(
                Button::new("kv-import-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        cancel_view
                            .update(cx, |this, cx| this.kv_cancel_import(session, cx))
                            .ok();
                    }),
            )
            .child(
                Button::new("kv-import-run", "Import")
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Sm)
                    .disabled(!can_import)
                    .on_click(move |_, _, cx| {
                        run_view
                            .update(cx, |this, cx| this.kv_run_import(session, cx))
                            .ok();
                    }),
            );

        Some(
            Modal::new("kv-import")
                .title("Import keys")
                .width(px(460.))
                .focus_handle(self.modal_focus.clone())
                .on_close(move |_, cx| {
                    close_view
                        .update(cx, |this, cx| this.kv_cancel_import(session, cx))
                        .ok();
                })
                .footer(footer)
                .child(body)
                .into_any_element(),
        )
    }

    /// The delete-key confirmation (Flint [`Modal`], root-mounted like the New-key
    /// and Import modals). Replaces the old inline confirm banner in the inspector
    /// header, whose buttons overflowed a narrow preview pane. `None` unless the
    /// focused Browse tab's inspector is awaiting delete confirmation.
    pub(crate) fn render_kv_delete_modal(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let session = active.session;
        let inspector = active
            .kv_view
            .as_ref()?
            .active_browse()?
            .inspector
            .as_ref()?;
        if !inspector.confirm_delete {
            return None;
        }
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let key = inspector.key.clone();

        let body = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(theme.scale(12.5))
                    .text_color(theme.text)
                    .child(format!("Delete \"{key}\"?")),
            )
            .child(
                div()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_muted)
                    .child("This can't be undone."),
            );

        let (confirm_view, cancel_view, close_view) = (view.clone(), view.clone(), view.clone());
        let footer = div()
            .flex()
            .flex_1()
            .justify_end()
            .gap_2()
            .child(
                Button::new("kv-delete-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        cancel_view
                            .update(cx, |this, cx| this.kv_cancel_delete(session, cx))
                            .ok();
                    }),
            )
            .child(
                Button::new("kv-delete-confirm", "Delete")
                    .variant(ButtonVariant::Danger)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        confirm_view
                            .update(cx, |this, cx| this.kv_confirm_delete(session, cx))
                            .ok();
                    }),
            );

        Some(
            Modal::new("kv-delete-key")
                .title("Delete key")
                .width(px(380.))
                .focus_handle(self.modal_focus.clone())
                .on_close(move |_, cx| {
                    close_view
                        .update(cx, |this, cx| this.kv_cancel_delete(session, cx))
                        .ok();
                })
                .footer(footer)
                .child(body)
                .into_any_element(),
        )
    }

    /// The "find biggest keys" sample's own table (see `BigKeysState`),
    /// replacing the live browse's table+inspector while it's active.
    fn render_big_keys(
        &self,
        session: SessionId,
        bk: &BigKeysState,
        inspector: Option<&KvInspector>,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let view = cx.entity().downgrade();
        let close_view = view.clone();
        let select_view = view.clone();
        let menu_view = view.clone();
        let rows = std::rc::Rc::new(bk.results.clone());
        let rows_render = rows.clone();
        let rows_select = rows.clone();
        let rows_menu = rows.clone();
        let row_count = rows.len();
        let selected_ix = inspector.and_then(|i| rows.iter().position(|r| r.key == i.key));

        let status = if bk.running {
            format!("sampling… {} keys scanned so far", bk.sampled)
        } else {
            format!(
                "sampled {} keys; showing the {} biggest",
                bk.sampled,
                rows.len()
            )
        };

        let columns = vec![
            Column::new("Key").flex(),
            Column::new("Type").width(px(72.)),
            Column::new("Size").width(px(90.)).align_end(),
            Column::new("Encoding").width(px(110.)),
        ];
        let key_color = theme.text;
        let dim_color = theme.text_muted;
        let row_theme = theme.clone();
        let cell_size = theme.scale(12.);

        let table = Table::<()>::new("kv-big-keys", columns)
            .row_count(row_count)
            .grid_lines(true)
            .text_size(cell_size)
            .selected(selected_ix)
            .on_select(move |ix, _click, _window, cx| {
                let Some(row) = rows_select.get(ix) else {
                    return;
                };
                let (key, ttl, kv_type) = (row.key.clone(), row.ttl, row.kv_type.clone());
                select_view
                    .update(cx, |this, cx| {
                        this.kv_open_inspector(session, key, ttl, kv_type, cx)
                    })
                    .ok();
            })
            .on_secondary(move |ix, pos, _window, cx| {
                let Some(row) = rows_menu.get(ix) else {
                    return;
                };
                let (key, kv_type, ttl) = (row.key.clone(), row.kv_type.clone(), row.ttl);
                menu_view
                    .update(cx, |this, cx| {
                        this.kv_open_key_menu(session, key, kv_type, ttl, pos, cx)
                    })
                    .ok();
            })
            .render_row(move |ix, _window, _cx| {
                let Some(row) = rows_render.get(ix) else {
                    return Vec::new();
                };
                vec![
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(key_color)
                        .child(row.key.clone())
                        .into_any_element(),
                    type_pill(&row.kv_type, &row_theme).into_any_element(),
                    div()
                        .text_color(dim_color)
                        .child(fmt_bytes(row.approx_bytes))
                        .into_any_element(),
                    div()
                        .text_color(dim_color)
                        .truncate()
                        .child(row.encoding.clone())
                        .into_any_element(),
                ]
            });

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme.scale(10.5))
                            .text_color(theme.text_muted)
                            .child(status),
                    )
                    .child(
                        Button::new(
                            "kv-big-keys-close",
                            if bk.running {
                                "Stop sampling"
                            } else {
                                "Back to live browse"
                            },
                        )
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            close_view
                                .update(cx, |this, cx| this.kv_close_big_keys(session, cx))
                                .ok();
                        }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(div().flex_1().min_w(px(0.)).child(table))
                    .when_some(inspector, |el, inspector| {
                        // The biggest-keys view keeps a fixed-width preview (no
                        // resize here, unlike the live browse's split).
                        el.child(div().flex_shrink_0().w(px(380.)).h_full().child(
                            self.render_kv_inspector(session, inspector, writable, theme, cx),
                        ))
                    }),
            )
    }

    /// The keyspace-analysis panel: a persisted, point-in-time report (type
    /// distribution, top namespaces by memory, expiry summary) with a
    /// Run/Re-run control (see docs/plans/redis.md's "persistent database
    /// analysis report" gap).
    pub(crate) fn render_kv_analysis(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let session = active.session;
        let Some(st) = active.kv_view.as_ref().and_then(|v| v.analysis_at(tab_idx)) else {
            return div().flex_1();
        };

        let run_view = cx.entity().downgrade();
        let cancel_view = cx.entity().downgrade();
        let run_label = if st.report.is_some() {
            "Re-analyze"
        } else {
            "Analyze keyspace"
        };
        let status = if st.running {
            format!("Scanning… {} keys sampled", st.collected.len())
        } else if let Some(r) = &st.report {
            let scope = if r.truncated {
                format!("sampled {} of {}", r.sampled, r.total_keys.max(r.sampled))
            } else {
                format!("{} keys (full scan)", r.sampled)
            };
            format!(
                "As of {} — {scope}, {} total",
                fmt_ago_secs(crate::conversations::now_unix() as i64 - r.generated_at),
                fmt_bytes(r.total_bytes)
            )
        } else {
            "No analysis yet. Run one to break down types, namespaces and expiry.".to_string()
        };

        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(status),
            )
            .when(st.running, |d| {
                d.child(
                    Button::new("kv-analysis-cancel", "Stop")
                        .variant(ButtonVariant::Secondary)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            cancel_view
                                .update(cx, |this, cx| this.kv_cancel_analysis(session, cx))
                                .ok();
                        }),
                )
            })
            .when(!st.running, |d| {
                d.child(
                    Button::new("kv-analysis-run", run_label)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            run_view
                                .update(cx, |this, cx| this.kv_run_analysis(session, cx))
                                .ok();
                        }),
                )
            });

        let body = match &st.report {
            Some(report) => self
                .render_analysis_report(report, session, &theme, cx)
                .into_any_element(),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_size(theme.scale(11.5))
                .text_color(theme.text_muted)
                .child(if st.running {
                    "Analyzing the keyspace…"
                } else {
                    "Run an analysis to see the report here."
                })
                .into_any_element(),
        };

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(header)
            .child(body)
    }

    /// The report body: type distribution, top namespaces, expiry summary,
    /// each a section of proportion bars. Read-only, scrollable.
    fn render_analysis_report(
        &self,
        report: &red_core::kv::RedisAnalysis,
        session: SessionId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let section_label = |s: &str| {
            div()
                .flex_shrink_0()
                .px_2()
                .pt_2()
                .pb_1()
                .text_size(theme.scale(9.5))
                .text_color(theme.text_muted)
                .child(s.to_string().to_uppercase())
        };

        // A labelled proportion bar: `label` left, `right` note, a fill sized
        // to `value/max` behind it. Reused across the type and namespace lists.
        // Clicking a type or namespace row drills into a filtered Browse tab
        // (see `kv_drill_type`/`kv_drill_namespace`).
        let max_type = report.types.iter().map(|t| t.bytes).max().unwrap_or(0);
        let type_rows: Vec<_> = report
            .types
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let row = bar_row(
                    &t.kv_type,
                    &format!("{} · {}", t.count, fmt_bytes(t.bytes)),
                    t.bytes,
                    max_type,
                    theme.blue,
                    theme,
                );
                let drill_view = cx.entity().downgrade();
                let label = t.kv_type.clone();
                div()
                    .id(("kv-analysis-type", i))
                    .cursor_pointer()
                    .child(row)
                    .on_click(move |_, _, cx| {
                        let label = label.clone();
                        drill_view
                            .update(cx, |this, cx| this.kv_drill_type(session, label, cx))
                            .ok();
                    })
                    .into_any_element()
            })
            .collect();

        let max_ns = report.namespaces.iter().map(|n| n.bytes).max().unwrap_or(0);
        let ns_rows: Vec<_> = report
            .namespaces
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let row = bar_row(
                    &n.prefix,
                    &format!("{} · {}", n.count, fmt_bytes(n.bytes)),
                    n.bytes,
                    max_ns,
                    theme.purple,
                    theme,
                );
                // The "(no prefix)" bucket has no glob to drill into.
                if n.prefix == red_core::kv::NO_PREFIX_LABEL {
                    return row;
                }
                let drill_view = cx.entity().downgrade();
                let prefix = n.prefix.clone();
                div()
                    .id(("kv-analysis-ns", i))
                    .cursor_pointer()
                    .child(row)
                    .on_click(move |_, _, cx| {
                        let prefix = prefix.clone();
                        drill_view
                            .update(cx, |this, cx| this.kv_drill_namespace(session, prefix, cx))
                            .ok();
                    })
                    .into_any_element()
            })
            .collect();

        // Expiry summary: persistent vs. bucketed by how soon.
        let ttl = &report.ttl;
        let ttl_total = ttl.persistent + ttl.with_ttl();
        let ttl_rows: Vec<_> = [
            ("No expiry", ttl.persistent, theme.text_muted),
            ("< 1 hour", ttl.under_hour, theme.red),
            ("< 1 day", ttl.under_day, theme.orange),
            ("< 1 week", ttl.under_week, theme.yellow),
            ("> 1 week", ttl.over_week, theme.green),
        ]
        .into_iter()
        .filter(|(_, n, _)| *n > 0)
        .map(|(label, n, color)| bar_row(label, &n.to_string(), n, ttl_total, color, theme))
        .collect();

        div()
            .id("kv-analysis-report")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(section_label("Types by memory"))
            .children(type_rows)
            .child(section_label(&format!(
                "Top {} namespaces by memory",
                report.namespaces.len()
            )))
            .children(ns_rows)
            .child(section_label("Expiry"))
            .children(ttl_rows)
            .into_any_element()
    }

    /// The value inspector panel: key/type/TTL header, then the value
    /// rendered per type, docked to the right of the keyspace table.
    fn render_kv_inspector(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let view = cx.entity().downgrade();

        // Favourite state comes from the annotation store (the single active
        // connection), keyed by conn id + key — not carried on the inspector.
        let is_fav = if let Phase::Connected(a) = &self.phase {
            self.redis_key_meta.is_favorite(&a.conn_id, &inspector.key)
        } else {
            false
        };

        // --- key name; the rename affordance opens a popover (see the
        // `rename_popover` overlay on the panel root below) rather than swapping
        // the header inline. ---
        let key_row = {
            let rename_view = view.clone();
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(theme.scale(12.))
                        .child(inspector.key.clone()),
                )
                .when(writable, |d| {
                    d.child(
                        IconButton::new(
                            "kv-rename-start",
                            crate::icons::icon("edit", theme.scale(12.), theme.text_muted),
                        )
                        .size(IconButtonSize::Sm)
                        .tooltip("Rename key")
                        .a11y_label("Rename key")
                        .active(inspector.editing_key)
                        .on_click(move |_, _, cx| {
                            rename_view
                                .update(cx, |this, cx| this.kv_start_editing_key(session, cx))
                                .ok();
                        }),
                    )
                })
                .into_any_element()
        };

        // --- TTL; clicking opens the expiry popover (see `ttl_popover` below). ---
        let ttl_row = {
            let ttl_view = view.clone();
            let label = div()
                .text_size(theme.scale(10.5))
                .text_color(theme.text_muted)
                .child(fmt_ttl(inspector.ttl));
            if writable {
                div()
                    .id("kv-ttl-start")
                    .cursor_pointer()
                    .child(label)
                    .on_click(move |_, _, cx| {
                        ttl_view
                            .update(cx, |this, cx| this.kv_start_editing_ttl(session, cx))
                            .ok();
                    })
                    .into_any_element()
            } else {
                label.into_any_element()
            }
        };

        // Favourite (star) toggle, surfaced into the preview so a key can be
        // starred without going back to the list's right-click menu.
        let star_button = {
            let fav_view = view.clone();
            let key = inspector.key.clone();
            IconButton::new(
                "kv-inspector-fav",
                crate::icons::icon(
                    if is_fav { "star-filled" } else { "star" },
                    theme.scale(13.),
                    if is_fav {
                        theme.yellow
                    } else {
                        theme.text_muted
                    },
                ),
            )
            .size(IconButtonSize::Sm)
            .active(is_fav)
            .tooltip(if is_fav {
                "Unfavourite key"
            } else {
                "Favourite key"
            })
            .a11y_label("Toggle favourite")
            .on_click(move |_, _, cx| {
                fav_view
                    .update(cx, |this, cx| {
                        this.kv_toggle_key_favorite(session, key.clone(), cx)
                    })
                    .ok();
            })
        };

        let delete_button = writable.then(|| {
            let delete_view = view.clone();
            IconButton::new(
                "kv-inspector-delete",
                crate::icons::icon("trash", theme.scale(13.), theme.red),
            )
            .size(IconButtonSize::Sm)
            .tooltip("Delete key")
            .a11y_label("Delete key")
            .on_click(move |_, _, cx| {
                delete_view
                    .update(cx, |this, cx| this.kv_request_delete(session, cx))
                    .ok();
            })
        });

        let close_view = view.clone();
        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .child(key_row)
            .child(type_pill(&inspector.kv_type, theme))
            .child(ttl_row)
            .child(star_button)
            .children(delete_button)
            .child(
                IconButton::new(
                    "kv-inspector-close",
                    crate::icons::icon("x", theme.scale(13.), theme.text_muted),
                )
                .size(IconButtonSize::Sm)
                .tooltip("Close")
                .a11y_label("Close inspector")
                .on_click(move |_, _, cx| {
                    close_view
                        .update(cx, |this, cx| this.kv_close_inspector(session, cx))
                        .ok();
                }),
            );

        // Rename / expiry each open as a small popover card anchored under the
        // header, dismissed by an outside click on the panel-wide backdrop, the
        // Cancel button (secondary), or Escape/Submit in the field. Replaces the
        // old inline header-swap so the header stays legible while editing.
        let rename_popover = inspector.editing_key.then(|| {
            self.kv_edit_popover(
                session,
                "kv-rename",
                "Rename key",
                None,
                inspector.rename_editor.clone().into_any_element(),
                "Rename",
                theme,
                cx,
                |this, session, cx| this.kv_submit_rename(session, cx),
                |this, session, cx| this.kv_cancel_editing_key(session, cx),
            )
        });
        let ttl_popover = inspector.editing_ttl.then(|| {
            let ttl_field = div()
                .child(inspector.ttl_editor.clone())
                .children(inspector.ttl_error.clone().map(|e| {
                    div()
                        .text_size(theme.scale(10.))
                        .text_color(theme.red)
                        .child(e)
                }))
                .into_any_element();
            self.kv_edit_popover(
                session,
                "kv-ttl",
                "Set expiry",
                Some("Seconds from now. Blank clears the expiry (persist).".into()),
                ttl_field,
                "Set",
                theme,
                cx,
                |this, session, cx| this.kv_submit_ttl_edit(session, cx),
                |this, session, cx| this.kv_cancel_editing_ttl(session, cx),
            )
        });

        let collection_popover = inspector
            .collection_edit
            .clone()
            .map(|kind| self.render_kv_collection_popover(session, &kind, inspector, theme, cx));

        let body = self.render_kv_value(session, inspector, writable, theme, cx);

        // Fills its parent's width so the caller controls sizing — a resizable
        // split pane in the live browse (see `render_kv_browse`), a fixed-width
        // wrapper in the biggest-keys view.
        div()
            .relative()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.bg_panel)
            .child(header)
            .child(body)
            .children(rename_popover)
            .children(ttl_popover)
            .children(collection_popover)
    }

    /// The add/edit-element popover for a collection key. Shows one or two
    /// inputs (member/field + value/score) plus any inline validation error,
    /// reusing `kv_edit_popover`'s card + backdrop.
    fn render_kv_collection_popover(
        &self,
        session: SessionId,
        kind: &CollectionEditKind,
        inspector: &KvInspector,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let name = inspector.elem_name_editor.clone();
        let value = inspector.elem_value_editor.clone();
        // (title, save label, two_input, name_placeholder)
        let (title, save_label, two_input): (&'static str, &'static str, bool) = match kind {
            CollectionEditKind::AddHashField => ("Add field", "Add", true),
            CollectionEditKind::EditHashField { .. } => ("Edit value", "Save", false),
            CollectionEditKind::AddSetMember => ("Add member", "Add", false),
            CollectionEditKind::EditSetMember { .. } => ("Edit member", "Save", false),
            CollectionEditKind::AddZSetMember => ("Add member", "Add", true),
            CollectionEditKind::EditZSetScore { .. } => ("Edit score", "Save", false),
            CollectionEditKind::AddListHead => ("Prepend item", "Add", false),
            CollectionEditKind::AddListTail => ("Append item", "Add", false),
            CollectionEditKind::EditListIndex { .. } => ("Edit item", "Save", false),
        };
        // The single-input variants that edit `elem_value` (hash value / zset
        // score) rather than `elem_name` (member / list value).
        let single_is_value = matches!(
            kind,
            CollectionEditKind::EditHashField { .. } | CollectionEditKind::EditZSetScore { .. }
        );

        let field = if two_input {
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(name.into_any_element())
                .child(value.into_any_element())
        } else if single_is_value {
            div().child(value.into_any_element())
        } else {
            div().child(name.into_any_element())
        };
        let field = field
            .children(inspector.elem_error.clone().map(|e| {
                div()
                    .text_size(theme.scale(10.))
                    .text_color(theme.red)
                    .child(e)
            }))
            .into_any_element();

        let hint: Option<SharedString> = matches!(kind, CollectionEditKind::AddZSetMember)
            .then(|| "Score is a number (e.g. 1.0).".into());

        self.kv_edit_popover(
            session,
            "kv-elem",
            title,
            hint,
            field,
            save_label,
            theme,
            cx,
            |this, session, cx| this.kv_submit_collection_edit(session, cx),
            |this, session, cx| this.kv_cancel_collection_edit(session, cx),
        )
    }

    /// A small popover card anchored just under the inspector header, for
    /// editing the key name or its expiry. Its own panel-wide backdrop dismisses
    /// on an outside click; the card carries a title, the caller's editor field,
    /// an optional hint, and Save (primary) / Cancel (secondary) buttons.
    #[allow(clippy::too_many_arguments)]
    fn kv_edit_popover(
        &self,
        session: SessionId,
        id: &'static str,
        title: impl Into<SharedString>,
        hint: Option<SharedString>,
        field: gpui::AnyElement,
        save_label: &'static str,
        theme: &Theme,
        cx: &mut Context<Self>,
        on_save: impl Fn(&mut Self, SessionId, &mut Context<Self>) + 'static,
        on_cancel: impl Fn(&mut Self, SessionId, &mut Context<Self>) + Clone + 'static,
    ) -> gpui::AnyElement {
        let view = cx.entity().downgrade();
        let (save_view, cancel_view, backdrop_view) = (view.clone(), view.clone(), view.clone());
        let on_cancel_backdrop = on_cancel.clone();
        let title = title.into();
        let card = div()
            .occlude()
            .w(px(300.))
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
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(title),
            )
            .child(field)
            .children(hint.map(|h| {
                div()
                    .text_size(theme.scale(10.))
                    .text_color(theme.text_muted)
                    .child(h)
            }))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("{id}-save")), save_label)
                            .variant(ButtonVariant::Primary)
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                save_view
                                    .update(cx, |this, cx| on_save(this, session, cx))
                                    .ok();
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{id}-cancel")), "Cancel")
                            .variant(ButtonVariant::Secondary)
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                cancel_view
                                    .update(cx, |this, cx| on_cancel(this, session, cx))
                                    .ok();
                            }),
                    ),
            );
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                backdrop_view
                    .update(cx, |this, cx| on_cancel_backdrop(this, session, cx))
                    .ok();
            })
            .child(floating(card).offset(point(px(8.), px(38.))))
            .into_any_element()
    }

    /// The string inspector's lens toolbar (Auto/Raw/JSON/Hex + the binary
    /// decoders), reusing the SQL inspector's `ValueFormat`. Lets a Redis
    /// string holding msgpack/protobuf/pickle be decoded in place, the same way
    /// a SQL blob cell can (see docs/plans/redis.md's "binary value decoders").
    fn render_kv_str_lens(
        &self,
        session: SessionId,
        current: crate::inspector::ValueFormat,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::inspector::ValueFormat;
        let opt = |id: &'static str, label: &'static str, fmt: ValueFormat| {
            let view = cx.entity().downgrade();
            // Wrapped in a non-shrinking cell so the row overflows (and scrolls)
            // instead of compressing the buttons when the inspector is narrow.
            div().flex_shrink_0().child(
                Button::new(id, label)
                    .variant(if current == fmt {
                        ButtonVariant::Secondary
                    } else {
                        ButtonVariant::Ghost
                    })
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| this.kv_set_str_format(session, fmt, cx))
                            .ok();
                    }),
            )
        };
        div()
            // A narrow inspector can't fit all ten lenses; scroll them
            // horizontally rather than clipping the trailing ones off-panel.
            .id("kv-str-lens")
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .overflow_x_scroll()
            .child(opt("kv-fmt-auto", "Auto", ValueFormat::Auto))
            .child(opt("kv-fmt-raw", "Raw", ValueFormat::Raw))
            .child(opt("kv-fmt-json", "JSON", ValueFormat::Json))
            .child(opt("kv-fmt-hex", "Hex", ValueFormat::Hex))
            .child(opt("kv-fmt-msgpack", "MsgPack", ValueFormat::MsgPack))
            .child(opt("kv-fmt-protobuf", "Protobuf", ValueFormat::Protobuf))
            .child(opt("kv-fmt-pickle", "Pickle", ValueFormat::Pickle))
            .child(opt("kv-fmt-timestamp", "Time", ValueFormat::Timestamp))
            .child(opt("kv-fmt-decompress", "Gzip", ValueFormat::Decompress))
            .child(opt("kv-fmt-bits", "Bits", ValueFormat::Bits))
            .into_any_element()
    }

    /// The inspector's value area: a per-type renderer for a loaded value, a
    /// paged sub-grid for a big collection, or a loading/unsupported note.
    fn render_kv_value(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let text_size = theme.scale(11.5);
        let dim = theme.text_muted;
        let mono = theme.mono_family.clone();
        let view = cx.entity().downgrade();

        // An error reading the value, or a key that vanished between listing
        // and read, gets a settled message rather than a permanent "Loading…".
        if let Some(err) = &inspector.value_error {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_3()
                .text_size(text_size)
                .text_color(theme.red)
                .child(format!("Couldn't read value: {err}"))
                .into_any_element();
        }
        let Some(value) = &inspector.value else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(text_size)
                .text_color(dim)
                .child(if inspector.value_loaded {
                    "This key no longer exists"
                } else {
                    "Loading…"
                })
                .into_any_element();
        };

        match value {
            KvValue::Str(_) if inspector.editing_value => {
                let (save_view, cancel_view) = (view.clone(), view.clone());
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .child(
                        // Full-height editor (it scrolls itself): the value body
                        // becomes editable in place, inheriting the mono
                        // typography just like the read-only preview.
                        div()
                            .id("kv-inspector-string-edit")
                            .flex_1()
                            .min_h(px(0.))
                            .font_family(mono.clone())
                            .text_size(text_size)
                            .line_height(text_size * 1.5)
                            .child(inspector.value_editor.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(
                                Button::new("kv-string-save", "Save")
                                    .variant(ButtonVariant::Primary)
                                    .size(ButtonSize::Sm)
                                    .on_click(move |_, _, cx| {
                                        save_view
                                            .update(cx, |this, cx| {
                                                this.kv_submit_value_edit(session, cx)
                                            })
                                            .ok();
                                    }),
                            )
                            .child(
                                Button::new("kv-string-cancel", "Cancel")
                                    .variant(ButtonVariant::Secondary)
                                    .size(ButtonSize::Sm)
                                    .on_click(move |_, _, cx| {
                                        cancel_view
                                            .update(cx, |this, cx| {
                                                this.kv_cancel_editing_value(session, cx)
                                            })
                                            .ok();
                                    }),
                            ),
                    )
                    .into_any_element()
            }
            KvValue::Str(v) => {
                // A `read_value`-capped string shows only its head; editing that
                // would save the truncated head back over the whole key, so a
                // capped value is view-only until "Load full value" pulls the
                // rest (see `kv_load_full_value`).
                let capped = matches!(v, red_core::Value::Capped(_));
                // Editing makes sense for any textual value; a binary value (a
                // `Value::Blob`, see `cap_string_value`) is view-only. A capped
                // text is editable too — Edit fetches the full value first (see
                // `kv_start_editing_value`) so a save never truncates the key.
                let editable = matches!(v, red_core::Value::Text(_))
                    || matches!(v, red_core::Value::Capped(c) if !c.blob);
                let loading_full = inspector.loading_full_value;
                // Prefer the selectable read-only preview editor (mirrors the
                // SQL cell inspector); fall back to plain, non-selectable text
                // for the frame or two before `kv_rebuild_str_preview` runs.
                let body_el = match &inspector.str_preview {
                    Some(p) => div()
                        .flex_1()
                        .min_h(px(0.))
                        .font_family(mono.clone())
                        .text_size(text_size)
                        .line_height(text_size * 1.5)
                        .child(p.editor.clone())
                        .into_any_element(),
                    None => {
                        let (body, _summary, _wrap) =
                            crate::inspector::format_value_body(v, inspector.str_format);
                        div()
                            .id("kv-inspector-string")
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .p_2()
                            .child(
                                div()
                                    .font_family(mono.clone())
                                    .text_size(text_size)
                                    .child(body),
                            )
                            .into_any_element()
                    }
                };
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .child(self.render_kv_str_lens(session, inspector.str_format, theme, cx))
                    .child(body_el)
                    .child({
                        let (edit_view, load_view, copy_view) =
                            (view.clone(), view.clone(), view.clone());
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(
                                Button::new("kv-string-copy", "Copy")
                                    .variant(ButtonVariant::Secondary)
                                    .size(ButtonSize::Sm)
                                    .on_click(move |_, _, cx| {
                                        copy_view
                                            .update(cx, |this, cx| {
                                                this.kv_copy_string_value(session, cx)
                                            })
                                            .ok();
                                    }),
                            )
                            .when(capped, |d| {
                                d.child(
                                    Button::new(
                                        "kv-string-load-full",
                                        if loading_full {
                                            "Loading…"
                                        } else {
                                            "Load full value"
                                        },
                                    )
                                    .variant(ButtonVariant::Primary)
                                    .size(ButtonSize::Sm)
                                    .disabled(loading_full)
                                    .on_click(
                                        move |_, _, cx| {
                                            load_view
                                                .update(cx, |this, cx| {
                                                    this.kv_load_full_value(session, cx)
                                                })
                                                .ok();
                                        },
                                    ),
                                )
                                // The whole value is bigger than the preview:
                                // say so, so the truncation reads as deliberate.
                                .child(
                                    div()
                                        .text_size(theme.scale(11.))
                                        .text_color(dim)
                                        .child("Preview truncated"),
                                )
                            })
                            .when(writable && editable, |d| {
                                d.child(
                                    Button::new("kv-string-edit", "Edit")
                                        .size(ButtonSize::Sm)
                                        .disabled(loading_full)
                                        .on_click(move |_, _, cx| {
                                            edit_view
                                                .update(cx, |this, cx| {
                                                    this.kv_start_editing_value(session, cx)
                                                })
                                                .ok();
                                        }),
                                )
                            })
                    })
                    .into_any_element()
            }
            KvValue::Stream(_) => self.render_kv_stream(session, inspector, writable, theme, cx),
            KvValue::Unsupported(kind) => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_2()
                .text_size(text_size)
                .text_color(dim)
                .child(format!(
                    "Preview not available for {} keys yet",
                    kind.label()
                ))
                .into_any_element(),
            KvValue::Hash(KvCollection::Loaded(pairs)) => {
                let rows: Rc<Vec<KvElement>> = Rc::new(
                    pairs
                        .iter()
                        .map(|(f, v)| KvElement::Field(f.clone(), v.clone()))
                        .collect(),
                );
                let len = rows.len() as u64;
                self.render_kv_collection_grid(
                    session,
                    CollectionKind::Hash,
                    rows,
                    len,
                    true,
                    writable,
                    inspector,
                    theme,
                    cx,
                )
            }
            KvValue::Set(KvCollection::Loaded(members)) => {
                let rows: Rc<Vec<KvElement>> = Rc::new(
                    members
                        .iter()
                        .map(|m| KvElement::Member(m.clone()))
                        .collect(),
                );
                let len = rows.len() as u64;
                self.render_kv_collection_grid(
                    session,
                    CollectionKind::Set,
                    rows,
                    len,
                    true,
                    writable,
                    inspector,
                    theme,
                    cx,
                )
            }
            KvValue::ZSet(KvCollection::Loaded(pairs)) => {
                let rows: Rc<Vec<KvElement>> = Rc::new(
                    pairs
                        .iter()
                        .map(|(m, s)| KvElement::Scored(m.clone(), *s))
                        .collect(),
                );
                let len = rows.len() as u64;
                self.render_kv_collection_grid(
                    session,
                    CollectionKind::ZSet,
                    rows,
                    len,
                    true,
                    writable,
                    inspector,
                    theme,
                    cx,
                )
            }
            KvValue::List(KvCollection::Loaded(items)) => {
                let rows = items.clone();
                let len = rows.len() as u64;
                self.render_kv_list(session, rows, len, false, writable, theme, cx)
            }
            KvValue::Hash(KvCollection::Large { len }) => self.render_kv_collection_grid(
                session,
                CollectionKind::Hash,
                inspector.collection_rows.clone(),
                *len,
                inspector.collection_exhausted,
                writable,
                inspector,
                theme,
                cx,
            ),
            KvValue::Set(KvCollection::Large { len }) => self.render_kv_collection_grid(
                session,
                CollectionKind::Set,
                inspector.collection_rows.clone(),
                *len,
                inspector.collection_exhausted,
                writable,
                inspector,
                theme,
                cx,
            ),
            KvValue::ZSet(KvCollection::Large { len }) => self.render_kv_collection_grid(
                session,
                CollectionKind::ZSet,
                inspector.collection_rows.clone(),
                *len,
                inspector.collection_exhausted,
                writable,
                inspector,
                theme,
                cx,
            ),
            KvValue::List(KvCollection::Large { len }) => {
                let rows: Vec<String> = inspector
                    .collection_rows
                    .iter()
                    .filter_map(|e| match e {
                        KvElement::Member(v) => Some(v.clone()),
                        _ => None,
                    })
                    .collect();
                self.render_kv_list(session, rows, *len, true, writable, theme, cx)
            }
        }
    }

    /// The hash/set/zset element grid: the same `Table` + `on_visible_range`
    /// load-more shape as the keyspace browser, scoped to one key's elements,
    /// now serving both the small (fully `Loaded`) and big (`Large`, paged)
    /// cases. On a writable connection each row carries inline edit/delete
    /// buttons and the toolbar an Add button (see the element popover).
    #[allow(clippy::too_many_arguments)]
    fn render_kv_collection_grid(
        &self,
        session: SessionId,
        kind: CollectionKind,
        rows: Rc<Vec<KvElement>>,
        total_len: u64,
        complete: bool,
        writable: bool,
        inspector: &KvInspector,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let dim = theme.text_muted;
        let cell_size = theme.scale(11.5);
        let edit_color = theme.text_muted;
        let del_color = theme.red;
        let icon_sz = theme.scale(12.);
        let rows_render = rows.clone();
        let row_count = rows.len();

        let mut columns = match kind {
            CollectionKind::Hash => vec![
                Column::new("Field").width(px(150.)),
                Column::new("Value").flex(),
            ],
            CollectionKind::Set => vec![Column::new("Member").flex()],
            CollectionKind::ZSet => vec![
                Column::new("Member").flex(),
                Column::new("Score").width(px(90.)).align_end(),
            ],
        };
        if writable {
            columns.push(Column::new("").width(px(60.)).align_end());
        }

        let action_view = cx.entity().downgrade();
        let mut table = Table::<()>::new("kv-inspector-grid", columns)
            .row_count(row_count)
            .grid_lines(true)
            .text_size(cell_size)
            .track_scroll(&inspector.collection_scroll)
            .render_row(move |ix, _window, _cx| {
                let Some(el) = rows_render.get(ix) else {
                    return Vec::new();
                };
                let mut cells = match el {
                    KvElement::Field(f, v) => vec![
                        div()
                            .min_w_0()
                            .truncate()
                            .child(f.clone())
                            .into_any_element(),
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(dim)
                            .child(v.clone())
                            .into_any_element(),
                    ],
                    KvElement::Scored(m, s) => vec![
                        div()
                            .min_w_0()
                            .truncate()
                            .child(m.clone())
                            .into_any_element(),
                        div()
                            .text_color(dim)
                            .child(format!("{s}"))
                            .into_any_element(),
                    ],
                    KvElement::Member(m) => vec![
                        div()
                            .min_w_0()
                            .truncate()
                            .child(m.clone())
                            .into_any_element(),
                    ],
                };
                if writable {
                    cells.push(collection_row_actions(
                        &action_view,
                        session,
                        kind,
                        ix,
                        el,
                        edit_color,
                        del_color,
                        icon_sz,
                    ));
                }
                cells
            });
        // Only page a `Large` collection; a fully loaded one has nothing more
        // to fetch (and firing load-more would issue a needless `*SCAN`).
        if !complete {
            let load_view = cx.entity().downgrade();
            table = table.on_visible_range(move |range, _window, cx| {
                load_view
                    .update(cx, |this, cx| {
                        this.kv_inspector_maybe_load_more(session, kind, range.end, cx)
                    })
                    .ok();
            });
        }

        let note = if complete {
            format!("{total_len} elements")
        } else {
            format!("{total_len} elements, paging as you scroll")
        };
        let (add_kind, add_label) = match kind {
            CollectionKind::Hash => (CollectionEditKind::AddHashField, "+ Field"),
            CollectionKind::Set => (CollectionEditKind::AddSetMember, "+ Member"),
            CollectionKind::ZSet => (CollectionEditKind::AddZSetMember, "+ Member"),
        };
        let add_view = cx.entity().downgrade();
        let toolbar = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .child(
                div()
                    .flex_1()
                    .text_size(theme.scale(10.5))
                    .text_color(dim)
                    .child(note),
            )
            .when(writable, |d| {
                d.child(
                    Button::new("kv-coll-add", add_label)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            let k = add_kind.clone();
                            add_view
                                .update(cx, |this, cx| {
                                    this.kv_open_collection_edit(
                                        session,
                                        k,
                                        String::new(),
                                        String::new(),
                                        cx,
                                    )
                                })
                                .ok();
                        }),
                )
            });

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(toolbar)
            .child(div().flex_1().min_h(px(0.)).child(table))
            .into_any_element()
    }

    /// A list key's element view (small fully loaded, or a big list's head
    /// window). Each row carries inline edit (`LSET` by index) / delete
    /// (`LREM`) buttons on a writable connection, plus Prepend/Append in the
    /// toolbar. A big list stays head-only (see `LIST_PREVIEW_COUNT`), so
    /// editing reaches only the head window it shows.
    #[allow(clippy::too_many_arguments)]
    fn render_kv_list(
        &self,
        session: SessionId,
        rows: Vec<String>,
        total_len: u64,
        head_only: bool,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let shown = rows.len();
        let note = if head_only {
            format!("showing the first {shown} of {total_len} items (head only)")
        } else {
            format!("{total_len} items")
        };
        let edit_color = theme.text_muted;
        let del_color = theme.red;
        let icon_sz = theme.scale(12.);

        let items: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let row = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .child(
                        div()
                            .w(px(36.))
                            .flex_shrink_0()
                            .text_color(theme.text_faint)
                            .child(i.to_string()),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(v.clone()));
                if !writable {
                    return row.into_any_element();
                }
                let edit_view = cx.entity().downgrade();
                let del_view = cx.entity().downgrade();
                let value_edit = v.clone();
                let idx = i as i64;
                row.child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .gap_0p5()
                        .child(
                            IconButton::new(
                                SharedString::from(format!("kv-li-edit-{i}")),
                                crate::icons::icon("edit", icon_sz, edit_color),
                            )
                            .size(IconButtonSize::Sm)
                            .tooltip("Edit")
                            .a11y_label("Edit item")
                            .on_click(move |_, _, cx| {
                                let v = value_edit.clone();
                                edit_view
                                    .update(cx, |this, cx| {
                                        this.kv_open_collection_edit(
                                            session,
                                            CollectionEditKind::EditListIndex { index: idx },
                                            v,
                                            String::new(),
                                            cx,
                                        )
                                    })
                                    .ok();
                            }),
                        )
                        .child(
                            IconButton::new(
                                SharedString::from(format!("kv-li-del-{i}")),
                                crate::icons::icon("trash", icon_sz, del_color),
                            )
                            .size(IconButtonSize::Sm)
                            .tooltip("Delete")
                            .a11y_label("Delete item")
                            .on_click(move |_, _, cx| {
                                del_view
                                    .update(cx, |this, cx| {
                                        this.kv_send_element_edit(
                                            session,
                                            move |key| red_core::kv::KvEdit::ListRemoveAt {
                                                key,
                                                index: idx,
                                            },
                                            cx,
                                        )
                                    })
                                    .ok();
                            }),
                        ),
                )
                .into_any_element()
            })
            .collect();

        let head_view = cx.entity().downgrade();
        let tail_view = cx.entity().downgrade();
        let toolbar = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .child(
                div()
                    .flex_1()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(note),
            )
            .when(writable, |d| {
                d.child(
                    Button::new("kv-list-add-head", "+ Head")
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            head_view
                                .update(cx, |this, cx| {
                                    this.kv_open_collection_edit(
                                        session,
                                        CollectionEditKind::AddListHead,
                                        String::new(),
                                        String::new(),
                                        cx,
                                    )
                                })
                                .ok();
                        }),
                )
                .child(
                    Button::new("kv-list-add-tail", "+ Tail")
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            tail_view
                                .update(cx, |this, cx| {
                                    this.kv_open_collection_edit(
                                        session,
                                        CollectionEditKind::AddListTail,
                                        String::new(),
                                        String::new(),
                                        cx,
                                    )
                                })
                                .ok();
                        }),
                )
            });

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(toolbar)
            .child(
                div()
                    .id("kv-inspector-list")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .text_size(theme.scale(11.5))
                    .children(items),
            )
            .into_any_element()
    }

    /// The stream inspector body: a segmented `Entries | Groups` toggle over
    /// either the entries view (loaded list or paged sub-grid) or the
    /// consumer-group management panel (see docs/plans/redis.md's "stream
    /// consumer-group management" gap).
    fn render_kv_stream(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let view = inspector.stream_groups.view;
        let tab = |label: &'static str, this_view: StreamView| {
            let active = view == this_view;
            let tab_view = cx.entity().downgrade();
            div()
                .id(label)
                .px_2()
                .py_0p5()
                .cursor_pointer()
                .text_size(theme.scale(11.))
                .text_color(if active { theme.text } else { theme.text_muted })
                .border_b_2()
                .border_color(if active {
                    theme.accent
                } else {
                    theme.border.opacity(0.)
                })
                .child(label)
                .on_click(move |_, _, cx| {
                    tab_view
                        .update(cx, |this, cx| {
                            this.kv_set_stream_view(session, this_view, cx)
                        })
                        .ok();
                })
        };

        let toggle = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(tab("Entries", StreamView::Entries))
            .child(tab("Groups", StreamView::Groups));

        let body = match view {
            StreamView::Entries => match &inspector.value {
                Some(KvValue::Stream(KvCollection::Loaded(entries))) => {
                    render_loaded_stream(entries, theme)
                }
                Some(KvValue::Stream(KvCollection::Large { len })) => {
                    self.render_kv_stream_grid(session, *len, inspector, theme, cx)
                }
                _ => div().flex_1().into_any_element(),
            },
            StreamView::Groups => {
                self.render_kv_stream_groups(session, inspector, writable, theme, cx)
            }
        };

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(toggle)
            .child(body)
            .into_any_element()
    }

    /// The consumer-group management panel: the stream's groups, and the
    /// selected group's consumers + pending entries with per-entry
    /// `XACK`/`XCLAIM` actions when the connection is writable.
    fn render_kv_stream_groups(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let st = &inspector.stream_groups;
        let dim = theme.text_muted;
        let text_size = theme.scale(11.);

        let note = |msg: &str| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_2()
                .text_size(text_size)
                .text_color(dim)
                .child(msg.to_string())
                .into_any_element()
        };

        if st.groups.is_empty() {
            return if st.loading || !st.loaded {
                note("Loading groups…")
            } else {
                note("No consumer groups on this stream.")
            };
        }

        // The groups list: one clickable row each, the selected one tinted.
        let group_rows: Vec<_> = st
            .groups
            .iter()
            .map(|g| {
                let selected = st.selected.as_deref() == Some(&g.name);
                let select_view = cx.entity().downgrade();
                let name = g.name.clone();
                let lag = g.lag.map(|l| format!(" · lag {l}")).unwrap_or_default();
                div()
                    .id(SharedString::from(format!("kv-group-{}", g.name)))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.accent.opacity(0.12)))
                    .hover(|d| d.bg(theme.bg_hover))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(text_size)
                            .text_color(if selected {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .child(g.name.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(theme.scale(10.))
                            .text_color(dim)
                            .child(format!("{}c · {}p{lag}", g.consumers, g.pending)),
                    )
                    .on_click(move |_, _, cx| {
                        select_view
                            .update(cx, |this, cx| {
                                this.kv_select_stream_group(session, name.clone(), cx)
                            })
                            .ok();
                    })
                    .into_any_element()
            })
            .collect();

        let groups_list = div()
            .id("kv-groups-list")
            .flex_shrink_0()
            .max_h(px(120.))
            .overflow_y_scroll()
            .border_b_1()
            .border_color(theme.border)
            .children(group_rows);

        let detail = st
            .selected
            .as_ref()
            .map(|_| self.render_kv_group_detail(session, inspector, writable, theme, cx));

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(groups_list)
            .children(detail)
            .into_any_element()
    }

    /// The selected group's detail: its consumers, then its pending entries,
    /// each with `Ack`/`Claim` affordances when writable.
    fn render_kv_group_detail(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let st = &inspector.stream_groups;
        let dim = theme.text_muted;
        let text_size = theme.scale(11.);
        let section_label = |s: &str| {
            div()
                .flex_shrink_0()
                .px_2()
                .py_0p5()
                .text_size(theme.scale(9.5))
                .text_color(dim)
                .child(s.to_string().to_uppercase())
        };

        // Consumers.
        let consumer_rows: Vec<_> = st
            .consumers
            .iter()
            .map(|c| {
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .text_size(text_size)
                    .child(div().min_w_0().truncate().child(c.name.clone()))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(theme.scale(10.))
                            .text_color(dim)
                            .child(format!("{}p · idle {}", c.pending, fmt_idle(c.idle))),
                    )
                    .into_any_element()
            })
            .collect();

        let consumers_empty = st.consumers.is_empty();

        // Pending entries.
        let pending_rows: Vec<_> = st
            .pending
            .iter()
            .map(|p| self.render_pending_row(session, inspector, p, writable, theme, cx))
            .collect();
        let pending_empty = st.pending.is_empty();
        let pending_header = format!(
            "Pending ({}{})",
            st.pending.len(),
            if st.pending.len() >= STREAM_PENDING_COUNT {
                "+"
            } else {
                ""
            }
        );

        div()
            .id("kv-group-detail")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(section_label("Consumers"))
            .when(consumers_empty, |d| {
                d.child(
                    div()
                        .px_2()
                        .py_0p5()
                        .text_size(text_size)
                        .text_color(dim)
                        .child("No consumers."),
                )
            })
            .children(consumer_rows)
            .child(section_label(&pending_header))
            .when(pending_empty && !st.detail_loading, |d| {
                d.child(
                    div()
                        .px_2()
                        .py_0p5()
                        .text_size(text_size)
                        .text_color(dim)
                        .child("Nothing pending — all delivered entries are acknowledged."),
                )
            })
            .children(pending_rows)
            .into_any_element()
    }

    /// One pending entry row: id, consumer, idle, delivery-count, plus an
    /// `Ack`/`Claim` action pair (writable only). The row expands to an inline
    /// claim form while this entry is the one being claimed.
    fn render_pending_row(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        entry: &PendingEntry,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let dim = theme.text_muted;
        let text_size = theme.scale(11.);
        let claiming = inspector.stream_groups.claiming.as_deref() == Some(&entry.id);

        let meta = div().flex().items_center().justify_between().gap_2().child(
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(theme.mono_family.clone())
                        .text_size(theme.scale(10.5))
                        .child(entry.id.clone()),
                )
                .child(
                    div()
                        .text_size(theme.scale(9.5))
                        .text_color(dim)
                        .child(format!(
                            "{} · idle {} · delivered {}×",
                            entry.consumer,
                            fmt_idle(entry.idle),
                            entry.delivery_count
                        )),
                ),
        );

        let actions = writable.then(|| {
            let id_ack = entry.id.clone();
            let ack_view = cx.entity().downgrade();
            let id_claim = entry.id.clone();
            let claim_view = cx.entity().downgrade();
            div()
                .flex_shrink_0()
                .flex()
                .gap_1()
                .child(
                    Button::new(SharedString::from(format!("kv-ack-{}", entry.id)), "Ack")
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            ack_view
                                .update(cx, |this, cx| {
                                    this.kv_stream_ack(session, id_ack.clone(), cx)
                                })
                                .ok();
                        }),
                )
                .child(
                    Button::new(
                        SharedString::from(format!("kv-claim-{}", entry.id)),
                        "Claim",
                    )
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        claim_view
                            .update(cx, |this, cx| {
                                this.kv_start_claim(session, id_claim.clone(), cx)
                            })
                            .ok();
                    }),
                )
        });

        let claim_form = claiming.then(|| {
            let (submit_view, cancel_view) = (cx.entity().downgrade(), cx.entity().downgrade());
            div()
                .flex()
                .items_center()
                .gap_1()
                .pt_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(inspector.stream_groups.claim_editor.clone()),
                )
                .child(
                    Button::new("kv-claim-submit", "Claim")
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            submit_view
                                .update(cx, |this, cx| this.kv_submit_claim(session, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new("kv-claim-cancel", "Cancel")
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            cancel_view
                                .update(cx, |this, cx| this.kv_cancel_claim(session, cx))
                                .ok();
                        }),
                )
        });

        div()
            .flex()
            .flex_col()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border.opacity(0.5))
            .text_size(text_size)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(meta)
                    .children(actions),
            )
            .children(claim_form)
            .into_any_element()
    }

    /// The big-stream sub-grid: newest-first entries in a virtualized `Table`
    /// (ID + fields), paging older on scroll via `kv_load_stream_page`. Mirrors
    /// `render_kv_collection_grid`, but keyed off `stream_rows` and continuing
    /// by entry ID rather than a `*SCAN` cursor.
    fn render_kv_stream_grid(
        &self,
        session: SessionId,
        len: u64,
        inspector: &KvInspector,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let view = cx.entity().downgrade();
        let rows = inspector.stream_rows.clone();
        let rows_render = rows.clone();
        let row_count = rows.len();

        let columns = vec![
            Column::new("ID").width(px(160.)),
            Column::new("Fields").flex(),
        ];
        let dim = theme.text_muted;
        let cell_size = theme.scale(11.5);

        let table = Table::<()>::new("kv-inspector-stream", columns)
            .row_count(row_count)
            .grid_lines(true)
            .text_size(cell_size)
            .track_scroll(&inspector.stream_scroll)
            .render_row(move |ix, _window, _cx| match rows_render.get(ix) {
                Some(entry) => vec![
                    div()
                        .min_w_0()
                        .truncate()
                        .child(entry.id.clone())
                        .into_any_element(),
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(dim)
                        .child(fmt_stream_fields(&entry.fields))
                        .into_any_element(),
                ],
                None => Vec::new(),
            })
            .on_visible_range(move |range, _window, cx| {
                view.update(cx, |this, cx| {
                    this.kv_inspector_maybe_load_more_stream(session, range.end, cx)
                })
                .ok();
            });

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .text_size(theme.scale(10.5))
                    .text_color(dim)
                    .child(format!(
                        "{len} entries, newest first — paging as you scroll"
                    )),
            )
            .child(div().flex_1().min_h(px(0.)).child(table))
            .into_any_element()
    }
}

/// A compact human idle-time for the consumer-group view (`XINFO`/`XPENDING`
/// idle is in ms): `"820ms"`, `"3.4s"`, `"5m"`, `"2h"`, `"1d"`. Coarse on
/// purpose — the operator wants "how stuck is this", not millisecond precision.
fn fmt_idle(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 3_600_000 {
        format!("{}m", ms / 60_000)
    } else if ms < 86_400_000 {
        format!("{}h", ms / 3_600_000)
    } else {
        format!("{}d", ms / 86_400_000)
    }
}

/// Flatten a stream entry's field/value pairs into a compact one-line
/// preview (`field=value  field=value`) for the grid's Fields column.
fn fmt_stream_fields(fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(f, v)| format!("{f}={v}"))
        .collect::<Vec<_>>()
        .join("  ")
}

/// A small (< threshold) stream rendered as a plain scrollable list of
/// `ID → fields` rows, newest-first, capped by `SMALL_COLLECTION_THRESHOLD` so
/// it needs no virtualization.
fn render_loaded_stream(entries: &[StreamEntry], theme: &Theme) -> gpui::AnyElement {
    let dim = theme.text_muted;
    let items: Vec<_> = entries
        .iter()
        .map(|e| {
            div()
                .flex()
                .gap_2()
                .px_2()
                .py_0p5()
                .child(
                    div()
                        .w(px(150.))
                        .min_w_0()
                        .truncate()
                        .text_color(dim)
                        .child(e.id.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(fmt_stream_fields(&e.fields)),
                )
                .into_any_element()
        })
        .collect();
    div()
        .id("kv-inspector-loaded-stream")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .text_size(theme.scale(11.5))
        .children(items)
        .into_any_element()
}

/// The inline edit + delete buttons for one hash/set/zset element row (the
/// grid's trailing actions cell). Edit opens the element popover seeded with
/// the row's current content; Delete sends the type's remove edit immediately
/// (a single element is trivially re-addable, so it skips a confirm).
#[allow(clippy::too_many_arguments)]
fn collection_row_actions(
    view: &WeakEntity<AppState>,
    session: SessionId,
    kind: CollectionKind,
    ix: usize,
    el: &KvElement,
    edit_color: Hsla,
    del_color: Hsla,
    icon_sz: gpui::Pixels,
) -> gpui::AnyElement {
    use red_core::kv::KvEdit;
    // Edit: map the element to its popover kind + seeds.
    let (edit_kind, seed_name, seed_value) = match el {
        KvElement::Field(f, v) => (
            CollectionEditKind::EditHashField { field: f.clone() },
            f.clone(),
            v.clone(),
        ),
        KvElement::Scored(m, s) => (
            CollectionEditKind::EditZSetScore { member: m.clone() },
            m.clone(),
            format!("{s}"),
        ),
        KvElement::Member(m) => (
            CollectionEditKind::EditSetMember { old: m.clone() },
            m.clone(),
            String::new(),
        ),
    };
    let ident = match el {
        KvElement::Field(f, _) => f.clone(),
        KvElement::Scored(m, _) => m.clone(),
        KvElement::Member(m) => m.clone(),
    };
    let (edit_view, del_view) = (view.clone(), view.clone());
    div()
        .flex()
        .gap_0p5()
        .justify_end()
        .child(
            IconButton::new(
                SharedString::from(format!("kv-el-edit-{ix}")),
                crate::icons::icon("edit", icon_sz, edit_color),
            )
            .size(IconButtonSize::Sm)
            .tooltip("Edit")
            .a11y_label("Edit element")
            .on_click(move |_, _, cx| {
                let (k, n, v) = (edit_kind.clone(), seed_name.clone(), seed_value.clone());
                edit_view
                    .update(cx, |this, cx| {
                        this.kv_open_collection_edit(session, k, n, v, cx)
                    })
                    .ok();
            }),
        )
        .child(
            IconButton::new(
                SharedString::from(format!("kv-el-del-{ix}")),
                crate::icons::icon("trash", icon_sz, del_color),
            )
            .size(IconButtonSize::Sm)
            .tooltip("Delete")
            .a11y_label("Delete element")
            .on_click(move |_, _, cx| {
                let ident = ident.clone();
                del_view
                    .update(cx, |this, cx| {
                        this.kv_send_element_edit(
                            session,
                            move |key| match kind {
                                CollectionKind::Hash => KvEdit::HashDelete {
                                    key,
                                    fields: vec![ident],
                                },
                                CollectionKind::Set => KvEdit::SetRemove {
                                    key,
                                    members: vec![ident],
                                },
                                CollectionKind::ZSet => KvEdit::ZSetRemove {
                                    key,
                                    members: vec![ident],
                                },
                            },
                            cx,
                        )
                    })
                    .ok();
            }),
        )
        .into_any_element()
}

/// A string value's preview body: pretty-printed if it parses as JSON
/// (a common Redis string payload shape), else the raw text; a capped value
/// shows its prefix plus a "… (N bytes total)" note.
pub(super) fn render_string_preview(value: &red_core::Value) -> String {
    match value {
        red_core::Value::Text(s) => s.to_string(),
        red_core::Value::Capped(cell) => {
            format!("{}\n\n… ({} bytes total, truncated)", cell.head, cell.len)
        }
        other => format!("{other:?}"),
    }
}
