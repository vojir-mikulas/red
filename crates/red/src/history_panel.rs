//! Shared History-dock renderer for the SQL and Redis shells.
//!
//! Both shells show a left "History" dock with near-identical chrome: a header
//! (title · clear · close) over a scrollable, grouped list of rows, each a mono
//! primary label with a relative-time subline and a hover-revealed ✕. This
//! module owns that chrome once; the two shells build a domain-free
//! [`HistoryPanelSpec`] and hand it to [`AppState::render_history_panel`].
//!
//! The spec is deliberately presentation-only: the caller decides what the rows
//! are, how they group into [`HistorySection`]s, which sections are collapsed,
//! and how the search box narrows them. The renderer just draws. This keeps the
//! two adapters (`history::render_history`, `shell::render_kv_history`) thin and
//! the collapse/search/nav policy where the domain state lives.

use std::rc::Rc;

use flint::prelude::*;
use gpui::{Context, Entity, FocusHandle, KeyDownEvent, SharedString, div, prelude::*, px};

use crate::app::AppState;

/// A row-activate action: `(this, replace, cx)`. `replace` is the ⌘/Ctrl
/// modifier at click time (open-in-place vs. a new tab); docks that don't
/// distinguish just ignore it. Boxed so each caller captures its own target.
pub(crate) type ActivateFn = Rc<dyn Fn(&mut AppState, bool, &mut Context<AppState>)>;

/// A plain row/section action with no arguments beyond `cx` (delete, clear,
/// toggle-collapse). The captured target/id lives inside the closure.
pub(crate) type Action = Rc<dyn Fn(&mut AppState, &mut Context<AppState>)>;

/// One history row, domain-free. `nav_index` is the row's position in the
/// dock's flat keyboard-nav order (`Some` only where a dock wires nav — SQL
/// today); the renderer highlights the row whose `nav_index` matches the spec's
/// `selected`.
pub(crate) struct HistoryRow {
    pub primary: SharedString,
    pub secondary: SharedString,
    pub badge: Option<SharedString>,
    pub nav_index: Option<usize>,
    pub activate: ActivateFn,
    pub delete: Option<Action>,
}

/// A titled group of rows. `title: None` renders the rows with no header and is
/// never collapsible (the SQL dock's single ungrouped list). A titled section
/// shows a disclosure chevron + row count and toggles via `toggle`.
pub(crate) struct HistorySection {
    /// Stable id for the header element (and the caller's collapse map key).
    pub key: &'static str,
    pub title: Option<SharedString>,
    pub collapsed: bool,
    pub toggle: Option<Action>,
    pub rows: Vec<HistoryRow>,
}

/// Optional keyboard-navigation wiring. `on_key` returns `true` when it handled
/// the key (the renderer then stops propagation), matching the SQL dock's
/// existing `on_key_down` contract.
pub(crate) struct HistoryNav {
    pub focus: FocusHandle,
    #[allow(clippy::type_complexity)]
    pub on_key: Rc<dyn Fn(&mut AppState, &KeyDownEvent, &mut Context<AppState>) -> bool>,
}

/// Everything the renderer needs to draw one History dock.
pub(crate) struct HistoryPanelSpec {
    pub sections: Vec<HistorySection>,
    /// Centered message when there are no rows to show (empty history, or no
    /// search matches — the caller picks the wording).
    pub empty_text: SharedString,
    pub show_clear: bool,
    pub on_clear: Action,
    /// The live search input, rendered as a filter strip above the list. The
    /// caller owns it and has already narrowed `sections` to matches.
    pub search: Option<Entity<TextInput>>,
    /// Keyboard nav (focus handle + key handler); `None` disables nav.
    pub nav: Option<HistoryNav>,
    /// Flat index of the keyboard-highlighted row (matched against
    /// `HistoryRow::nav_index`).
    pub selected: Option<usize>,
}

impl AppState {
    /// Render one History dock from a domain-free [`HistoryPanelSpec`]. Shared by
    /// the SQL (`render_history`) and Redis (`render_kv_history`) adapters.
    pub(crate) fn render_history_panel(
        &self,
        spec: HistoryPanelSpec,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let bg_panel = theme.bg_panel;
        let border = theme.border;
        let (text, muted, faint) = (theme.text, theme.text_muted, theme.text_faint);
        let (bg_hover, bg_elevated) = (theme.bg_hover, theme.bg_elevated);
        let ui_family = theme.font_family.clone();
        let mono = theme.mono_family.clone();
        let (size_12, size_11, size_10) = (theme.scale(12.), theme.scale(11.), theme.scale(10.));
        let icon_x = theme.scale(11.);

        let HistoryPanelSpec {
            sections,
            empty_text,
            show_clear,
            on_clear,
            search,
            nav,
            selected,
        } = spec;

        // --- header: "History" · clear · close ---
        let clear_btn = show_clear.then(|| {
            div()
                .id("history-clear")
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
                .on_click(cx.listener(move |this, _, _, cx| on_clear(this, cx)))
        });
        let close_btn = div()
            .id("history-hide")
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

        // --- optional search strip ---
        let search_strip = search.map(|input| {
            div()
                .flex_shrink_0()
                .px_1p5()
                .py_1()
                .border_b_1()
                .border_color(border)
                .child(input)
        });

        // --- body: sections of rows, or the empty state ---
        let is_empty = sections.iter().all(|s| s.rows.is_empty());
        let list = if is_empty {
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .px_4()
                .text_size(size_11)
                .text_color(faint)
                .child(empty_text)
                .into_any_element()
        } else {
            // A running id/nav counter so every row across every section has a
            // unique element id regardless of grouping.
            let mut gidx = 0usize;
            let mut list = div()
                .id("history-list")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .flex()
                .flex_col();
            if let Some(nav) = nav {
                let on_key = nav.on_key.clone();
                list = list
                    .key_context("History")
                    .track_focus(&nav.focus)
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _w, cx| {
                        if on_key(this, event, cx) {
                            cx.stop_propagation();
                        }
                    }));
            }
            for section in sections {
                if section.rows.is_empty() {
                    continue;
                }
                // Titled sections get a clickable disclosure header ("Title · N");
                // an untitled section (SQL's flat list) renders its rows bare.
                if let Some(title) = section.title.clone() {
                    let count = section.rows.len();
                    let collapsed = section.collapsed;
                    let chevron = if collapsed { "chevron" } else { "chevron-down" };
                    let toggle = section.toggle.clone();
                    let mut hdr = div()
                        .id(section.key)
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .pt_2()
                        .pb_1()
                        .font_family(ui_family.clone())
                        .text_size(size_10)
                        .text_color(faint)
                        .child(crate::icons::icon(chevron, size_10, faint))
                        .child(div().flex_1().min_w_0().truncate().child(title))
                        .child(SharedString::from(count.to_string()));
                    if let Some(toggle) = toggle {
                        hdr = hdr
                            .cursor_pointer()
                            .hover(|s| s.text_color(muted))
                            .on_click(cx.listener(move |this, _, _, cx| toggle(this, cx)));
                    }
                    list = list.child(hdr);
                    if collapsed {
                        continue;
                    }
                }
                for row in section.rows {
                    let is_sel = row.nav_index.is_some() && row.nav_index == selected;
                    let group = SharedString::from(format!("hrow-{gidx}"));
                    let activate = row.activate.clone();
                    // Subline: an optional badge (e.g. Redis type) then the
                    // relative-time text, matching the old two docks.
                    let sub = div()
                        .flex()
                        .gap_1()
                        .text_size(size_10)
                        .text_color(faint)
                        .children(row.badge.clone())
                        .child(row.secondary.clone());
                    let mut r = div()
                        .id(("hrow", gidx))
                        .group(group.clone())
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .py_1p5()
                        .when(is_sel, |d| d.bg(bg_hover))
                        .hover(move |s| s.bg(bg_hover))
                        .child(
                            // The label/subline column fills the row and is the
                            // activate hitbox; it clips so a long label never
                            // shoves the ✕ off.
                            div()
                                .id(("hrow-open", gidx))
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .cursor_pointer()
                                .on_click(cx.listener(
                                    move |this, event: &gpui::ClickEvent, _, cx| {
                                        activate(this, event.modifiers().secondary(), cx);
                                    },
                                ))
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(mono.clone())
                                        .text_size(size_12)
                                        .text_color(text)
                                        .child(row.primary.clone()),
                                )
                                .child(sub),
                        );
                    if let Some(delete) = row.delete {
                        r = r.child(
                            // Hover-revealed per-row delete, like the tab close
                            // button.
                            div()
                                .id(("hrow-del", gidx))
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
                                .on_click(cx.listener(move |this, _, _, cx| delete(this, cx)))
                                .child(crate::icons::icon("x", icon_x, faint)),
                        );
                    }
                    list = list.child(r);
                    gidx += 1;
                }
            }
            list.into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(bg_panel)
            .child(header)
            .children(search_strip)
            .child(list)
    }
}
