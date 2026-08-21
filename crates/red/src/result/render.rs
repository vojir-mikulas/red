//! Result-cell rendering and the results-pane view: colors cells by value kind
//! (numbers accented, UUIDs dimmed, JSON-ish text cyan) and assembles the
//! toolbar · grid · footer · scrollbar that make up the pane.

use flint::TextInput;
use flint::prelude::*;
use gpui::{
    Axis, Entity, Hsla, MouseButton, Pixels, Point, SharedString, Window, div, point, prelude::*,
    px,
};
use red_core::valuefmt::ClipboardFormat;
use red_core::{CmpOp, ExportFormat, Value};

use super::buffer::{CellKind, DisplayCell};
use super::edit::EditSlot;
use super::{DATA_COL_WIDTH, DRAFT_ZONE_ROWS, HeaderStyle, gutter_width};
use crate::app::{ActiveConn, AppState, Phase};

/// Group a number's digits in threes (`1234567` → `1,234,567`) so large row
/// numbers and totals read at a glance.
pub(crate) fn group_digits(n: usize) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// A query duration as a compact label: sub-second in milliseconds, otherwise
/// seconds with two decimals (`842 ms`, `1.27 s`).
fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
}

/// One `min`/`max`/`sum`/`avg` value for the stats bar: a NULL aggregate shows as
/// an em dash, anything else as its display string truncated so a long text min
/// can't run the bar off-screen.
fn fmt_stat_value(v: &red_core::Value) -> String {
    let s = match v {
        red_core::Value::Null => return "—".to_string(),
        other => other.to_string(),
    };
    const MAX: usize = 40;
    if s.chars().count() > MAX {
        format!("{}…", s.chars().take(MAX).collect::<String>())
    } else {
        s
    }
}

/// One `label value` segment of the stats bar (e.g. `count 12,345`).
fn stat_seg(label: &str, value: String, label_color: Hsla, value_color: Hsla) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(div().text_color(label_color).child(label.to_string()))
        .child(div().text_color(value_color).child(value))
        .into_any_element()
}

/// The faint `·` separator between stats-bar segments.
fn stat_dot(color: Hsla) -> gpui::AnyElement {
    div().text_color(color).child("·").into_any_element()
}

/// Colors a result cell carries, keyed by value kind (so the grid reads at a
/// glance the way the design does: numbers orange, UUIDs dimmed, JSON cyan).
#[derive(Clone, Copy)]
pub(in crate::result) struct CellColors {
    pub(in crate::result) text: Hsla,
    pub(in crate::result) muted: Hsla,
    pub(in crate::result) num: Hsla,
    pub(in crate::result) cyan: Hsla,
    pub(in crate::result) faint: Hsla,
    /// The brand accent, used to mark a foreign-key cell as navigable.
    pub(in crate::result) accent: Hsla,
}

/// One grid cell, colored by its pre-classified [`CellKind`] (NULL italic-faint,
/// numbers accented, UUIDs dimmed, JSON-ish text cyan, mirroring the design's
/// typed cells). The display string and kind were computed once when the row
/// landed in the buffer, so this only picks a color and clones a `SharedString`
/// (an `Arc` bump); no per-frame formatting, copying, or classification.
pub(in crate::result) fn render_cell(
    cell: &DisplayCell,
    c: CellColors,
    null_display: &SharedString,
    struck: bool,
    is_fk: bool,
) -> gpui::AnyElement {
    let kind_color = match cell.kind {
        CellKind::Null | CellKind::Blob => c.faint,
        CellKind::Num => c.num,
        CellKind::Text => c.text,
        CellKind::Uuid => c.muted,
        CellKind::Json => c.cyan,
    };
    // A foreign-key cell reads in the brand accent to signal it's a
    // navigable reference, except NULL/blob, which keep their faint style cue.
    let color = if is_fk && !matches!(cell.kind, CellKind::Null | CellKind::Blob) {
        c.accent
    } else {
        kind_color
    };
    // The buffer stores a placeholder for NULL; the user's chosen rendering (`∅`,
    // `NULL`, blank, …) is substituted here so it stays a settings concern only.
    let text = if cell.kind == CellKind::Null {
        null_display.clone()
    } else {
        cell.text.clone()
    };
    // Color independence (WCAG 1.4.1): NULL and binary blobs carry a *style* cue
    // (italic), not just a faint color, so they're still distinguishable in
    // grayscale or to a color-blind user. The other kinds (numbers, UUIDs, JSON)
    // are disambiguated without color by their text shape and the declared type
    // shown in each column header's subtitle, and are spoken with their value by
    // the grid's accessible-name announcement.
    let italic = matches!(cell.kind, CellKind::Null | CellKind::Blob);
    div()
        .text_color(if struck { c.faint } else { color })
        // A row pending deletion reads struck-through, so the marking is
        // legible without relying on the soft red tint alone.
        .when(struck, |d| d.line_through())
        .when(italic, |d| d.italic())
        .child(text)
        .into_any_element()
}

impl AppState {
    /// The in-panel recovery offered under a failed query when the connection has
    /// no namespace bound and the engine requires one: name the databases on the
    /// server and let one click bind the tab and re-run.
    ///
    /// This is the point of the whole feature. MySQL's database segment is
    /// optional (so the tree can browse the whole server), so an unqualified query
    /// on such a connection returns error 1046 with nothing to act on. Turning
    /// that into a picker is what makes the dead end a two-click fix.
    fn render_namespace_fix(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let names: Vec<String> = active
            .schema
            .read(cx)
            .schemas
            .iter()
            .map(|s| s.name.clone())
            .collect();
        if names.is_empty() {
            return None; // the tree hasn't loaded yet; nothing honest to offer
        }
        let theme = cx.theme();
        let (muted, size_11) = (theme.text_muted, theme.scale(11.));
        let label = active.config.kind.namespace_caps().label.to_lowercase();
        let view = cx.entity().downgrade();

        let buttons = names.into_iter().take(12).map(move |name| {
            let view = view.clone();
            Button::new(SharedString::from(format!("ns-fix-{name}")), name.clone())
                .variant(ButtonVariant::Ghost)
                .size(ButtonSize::Sm)
                .on_click(move |_, _, cx| {
                    let name = name.clone();
                    view.update(cx, |this, cx| {
                        // Bind the connection (not just this tab): an unbound
                        // connection means *no* tab has a target, so binding
                        // once fixes them all.
                        this.set_active_namespace(Some(name), cx);
                        this.run_editor_query(cx);
                    })
                    .ok();
                })
        });

        Some(
            div()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap_2()
                .pt_2()
                .text_size(size_11)
                .text_color(muted)
                .child(format!(
                    "Unqualified table names have no target {label}. \
                     Pick one to set it and re-run, or write them as db.table."
                ))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .children(buttons.collect::<Vec<_>>()),
                ),
        )
    }

    /// The results pane: an empty state, an error, or the live windowed grid.
    /// Render the result pane for the tab at `tab_idx`, shown in pane `pane`.
    /// `is_focused` is whether that pane currently has focus: only the focused
    /// pane hosts the shared single-instance overlays (inspector, filter/find
    /// bars, the cell menu, the stats bar, inline + draft editing) so they never
    /// render twice.
    pub(crate) fn render_result(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        pane: crate::app::PaneId,
        is_focused: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // Built up front so *every* return path below can keep them on screen.
        // Applying a filter re-opens the result, and a re-open passes through
        // "not ready" (and possibly "failed") — dropping the bars there would
        // yank the control the user is typing in out from under them each time
        // they press Apply, and would hide the engine's message plus the bar's
        // own "Revert filter" exactly when they're needed.
        //
        // They also need `cx` mutably, which the `cx.theme()` borrow below rules
        // out for the rest of this function.
        let filter_bar = is_focused
            .then(|| self.render_filter_bar(window, cx))
            .flatten();
        let find_bar = is_focused
            .then(|| self.render_find_bar(crate::find::FindTarget::Grid, cx))
            .flatten();
        // A query that failed with no namespace bound, on an engine that *needs*
        // one, is the "No database selected" dead end. Offer the picker right in
        // the error panel rather than making the user hunt for the run bar.
        // Detected from state, not by matching the engine's message text.
        let namespace_fix = (active.config.kind.namespace_caps().required
            && active.namespace_for_send().is_none())
        .then(|| self.render_namespace_fix(active, cx))
        .flatten();
        let theme = cx.theme();
        let (bg, border, border_soft) = (theme.bg_panel, theme.border, theme.border_soft);
        let (muted, faint, dim, text) = (
            theme.text_muted,
            theme.text_faint,
            theme.text_dim,
            theme.text,
        );
        let (red, accent) = (theme.red, theme.accent);
        // Scaled chrome sizes snapshotted here (Pixels is Copy) so the result
        // pane's status/empty/error text tracks the UI font size even inside the
        // `'static` row closures below.
        let (size_11, size_12) = (theme.scale(11.), theme.scale(12.));
        // Chrome (toolbar/stats/footer) follows the sans UI font; the data grid
        // cells follow the mono font, both rendered at the configured base size.
        let ui_family = theme.font_family.clone();
        let mono_family = theme.mono_family.clone();
        // The focus + cell-cursor keys live on the `Table` itself (see its
        // `.focus_handle`/`.on_nav` below); the pane draws no focus ring.
        let container = div().size_full().relative().flex().flex_col().bg(bg);

        // Stage this frame's parent-owned inputs on the grid before taking the read
        // borrow below: once `grid` is a reference read out of `cx`, nothing here can
        // borrow the context mutably again (and a child view could not pull them
        // during its own render anyway).
        self.push_grid_frame(tab_idx, pane, is_focused, cx);
        let grid_entity = active.tabs.get(tab_idx).and_then(|t| t.result.clone());
        let grid = match grid_entity.as_ref() {
            Some(grid) => grid.read(cx),
            None => {
                return container.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(size_12)
                        .text_color(faint)
                        .child(crate::i18n::tr!(
                            "result.empty_hint",
                            "Double-click a table or run a query to see rows"
                        )),
                );
            }
        };

        let elapsed = format_duration(grid.query_time());

        // A failed query gets a full-pane panel rather than the cramped toolbar
        // status slot: syntax errors are multi-line and would otherwise clip.
        if let Some(err) = &grid.error {
            // The filter bar stays above the error panel: a rejected predicate is
            // the likeliest cause, and the bar is where the message and the
            // one-click way back to the last good filter live.
            return container.children(filter_bar).child(
                div()
                    .id("result-error")
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .overflow_y_scroll()
                    .font_family(mono_family.clone())
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(size_11)
                            .text_color(red)
                            .child(crate::i18n::tr!("result.query_failed", "Query failed"))
                            .child(div().text_color(faint).child(format!("· {elapsed}"))),
                    )
                    .child(div().text_size(size_12).text_color(text).child(err.clone()))
                    .children(namespace_fix),
            );
        }

        let status = if !grid.ready {
            div().text_color(faint).child(format!("running… {elapsed}"))
        } else {
            // Filtered: read as "matched of total" so the narrowing is quantified
            // (the unfiltered total is the one captured on the last unfiltered
            // open; a browse that was born filtered, e.g. an FK follow, has none).
            let rows = match grid.filtered_of() {
                Some(whole) => format!(
                    "{} of {} rows",
                    group_digits(grid.total),
                    group_digits(whole)
                ),
                None => format!("{} rows", group_digits(grid.total)),
            };
            div().text_color(faint).child(format!("{rows} · {elapsed}"))
        };
        let view = cx.entity().downgrade();
        let toolbar = div()
            .flex_shrink_0()
            // No fixed height: the 24px buttons define the strip and the equal
            // padding brackets them evenly. A fixed height taller than the
            // buttons left slack that GPUI distributed unevenly, sinking the
            // buttons off-center.
            .py(px(3.))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(border)
            .font_family(ui_family.clone())
            .text_size(size_11)
            .child(div().text_color(muted).child(grid.label.clone()))
            .child(status)
            .child(
                // Per-cell actions (Inspect · Copy) moved to the cell's right-click
                // context menu; the toolbar keeps the result-wide CSV/JSON exports.
                div()
                    .ml_auto()
                    .flex()
                    .items_center()
                    .gap_1()
                    // "+ Row" appends a draft (insert) row, shown only on an
                    // editable keyed browse of a writable connection.
                    .when(self.insert_enabled() && grid.insertable_browse(), |d| {
                        d.child(
                            Button::new("result-add-row", "+ Row")
                                .variant(ButtonVariant::Ghost)
                                .size(ButtonSize::Sm)
                                .on_click(cx.listener(|this, _, _, cx| this.add_draft_row(cx))),
                        )
                    })
                    .child(
                        // ⌘⇧F: open / focus the filter bar. With a filter applied
                        // the button becomes a chip naming it (`WHERE amount > 100`)
                        // with its own ✕, so a narrowed grid can't be mistaken for a
                        // whole one and an FK-follow filter is visible too.
                        match &grid.filter {
                            None => Button::new("result-filter", "Filter")
                                .variant(ButtonVariant::Ghost)
                                .size(ButtonSize::Sm)
                                .tooltip(crate::i18n::tr!(
                                    "result.filter_rows_tip",
                                    "Filter rows (⌘⇧F)"
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_filter_bar(window, cx)
                                }))
                                .into_any_element(),
                            Some(filter) => div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Button::new(
                                        "result-filter",
                                        crate::filter::filter_summary(filter),
                                    )
                                    .variant(ButtonVariant::Secondary)
                                    .size(ButtonSize::Sm)
                                    .tooltip(crate::filter::filter_tooltip(filter))
                                    .on_click(cx.listener(
                                        |this, _, window, cx| this.toggle_filter_bar(window, cx),
                                    )),
                                )
                                .child(
                                    IconButton::new(
                                        "result-filter-clear",
                                        crate::icons::icon("close", size_11, muted),
                                    )
                                    .size(IconButtonSize::Sm)
                                    .tooltip(crate::i18n::tr!(
                                        "result.clear_filter",
                                        "Clear filter"
                                    ))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.clear_result_filter(cx)),
                                    ),
                                )
                                .into_any_element(),
                        },
                    )
                    .when(self.insert_enabled() && grid.insertable_browse(), |t| {
                        // Import a CSV/JSONL file into this table. Shown only on an
                        // editable keyed browse of a writable connection, like "+ Row"
                        // (import is a bulk insert). The grid's columns are the target.
                        t.child(
                            Button::new("result-import", "Import")
                                .variant(ButtonVariant::Ghost)
                                .size(ButtonSize::Sm)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.import_into_result(cx)),
                                ),
                        )
                    })
                    .child(
                        // CSV / JSON / HTML are grouped into one "Export" dropdown
                        // to keep the toolbar uncluttered; it opens a menu at the
                        // cursor (see `render_export_menu`). HTML is a plain themed
                        // export alongside CSV/JSON; AI-authored *reports* are a
                        // separate, on-demand thing the assistant generates.
                        Button::new("result-export", "Export ▾")
                            .variant(if self.export_menu.is_some() {
                                ButtonVariant::Secondary
                            } else {
                                ButtonVariant::Ghost
                            })
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, ev: &gpui::ClickEvent, _, cx| {
                                this.export_menu = Some(ev.position());
                                cx.notify();
                            })),
                    )
                    .child(
                        // The less-used actions (Stats toggle, "Copy to…") collapse
                        // into one "More" dropdown at the end of the row to keep the
                        // toolbar uncluttered (see `render_more_menu`).
                        Button::new("result-more", "More ▾")
                            .variant(if self.more_menu.is_some() {
                                ButtonVariant::Secondary
                            } else {
                                ButtonVariant::Ghost
                            })
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, ev: &gpui::ClickEvent, _, cx| {
                                this.more_menu = Some(ev.position());
                                cx.notify();
                            })),
                    ),
            );

        if !grid.ready {
            // Still running: keep the bars mounted so a re-open (which every
            // Apply performs) doesn't make the filter form flicker out and back.
            return container
                .child(toolbar)
                .children(filter_bar)
                .children(find_bar);
        }

        let data_cols: Vec<usize> = grid.visible.clone();
        let ncols = grid.columns.len();

        // Footer: a strong row count, the column count, and the result's label
        // (the design's "N rows · K columns" status strip under the grid).
        let footer = div()
            .flex_shrink_0()
            // Tall enough to seat the 24px Sm Submit/Revert buttons with breathing
            // room (the old 28px strip clipped them).
            .h(px(38.))
            .flex()
            .items_center()
            .gap_2()
            .px_3p5()
            .bg(bg)
            .border_t_1()
            .border_color(border)
            .font_family(ui_family.clone())
            .text_size(size_11)
            .child(div().text_color(text).child(format!("{}", grid.total)))
            .child(
                div()
                    .text_color(dim)
                    .child(crate::i18n::tr!("result.rows_unit", "rows")),
            )
            .child(div().text_color(border_soft).child("·"))
            // "12 of 17 columns" whenever some are hidden, mirroring the filtered
            // row count's "n of N": a column that is simply absent from the header
            // is otherwise indistinguishable from one the query never returned.
            .child(div().text_color(dim).child(if data_cols.len() < ncols {
                format!("{} of {ncols} columns", data_cols.len())
            } else {
                format!("{ncols} columns")
            }))
            .child(div().text_color(border_soft).child("·"))
            // Which paging mode this result got (keyset = seek key resolved;
            // offset = the O(offset) fallback); the at-a-glance diagnostic.
            .child(
                div()
                    .text_color(dim)
                    .child(if grid.buffer.borrow().is_keyed() {
                        "keyset"
                    } else {
                        "offset"
                    }),
            )
            // Staged-edit controls: a count + Submit / Revert, shown only
            // when the change-set is non-empty. Submit opens the confirm preview.
            .when_some(grid.pending.summary(), |f, summary| {
                f.child(div().text_color(border_soft).child("·"))
                    .child(div().text_color(accent).child(summary))
                    .child(
                        Button::new("changes-submit", "Submit")
                            .variant(ButtonVariant::Primary)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.submit_changes(cx))),
                    )
                    .child(
                        Button::new("changes-revert", "Revert")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.revert_changes(cx))),
                    )
            })
            // Why this table's rows can't be edited, on a connection whose engine
            // otherwise could. Silence here would read as "editing is broken"; the
            // engine's own reason ("the Memory engine has no UPDATE/DELETE") is the
            // difference between a dead end and an explanation.
            .when_some(
                self.row_edit_enabled(cx)
                    .then(|| grid.not_editable_note())
                    .flatten(),
                |f, note| {
                    f.child(div().text_color(border_soft).child("·")).child(
                        div()
                            .text_color(dim)
                            .child(format!("read-only rows: {note}")),
                    )
                },
            )
            .child(div().ml_auto().text_color(dim).child(grid.label.clone()));

        // The draggable, fraction-mapped scrollbar: the thumb mirrors the list's
        // position; a scrub jumps the viewport, and the buffer's `ensure` turns
        // the far jump into one key-space seek (keyed results) or one OFFSET page
        // (fallback).

        // The grid body is its own view: it renders from state staged on the grid
        // (see `push_grid_frame`), and the pane just mounts it between the bars and
        // the drafts.
        let grid_pane = container
            .child(toolbar)
            // The filter bar sits between the toolbar and the grid when open;
            // narrowing re-opens the result so the grid below just repaints. The
            // find bar sits alongside it and only highlights loaded rows. Both are
            // built at the top of this function (single-instance overlays, so they
            // render in the focused pane only).
            .children(filter_bar)
            .children(find_bar)
            .children(grid_entity.clone())
            // Draft (insert) rows pinned below the grid.
            .when_some(
                is_focused
                    .then(|| self.render_draft_rows(grid, cx))
                    .flatten(),
                |c, drafts| c.child(drafts),
            )
            // The column-stats bar (a thin summary line) sits just above the footer
            // when the toggle is on.
            .when(is_focused && grid.stats_bar, |c| {
                c.child(self.render_stats_bar(grid, cx))
            })
            .child(footer);
        // NB: the cell / export / more dropdowns are *not* mounted here. Their
        // dismiss backdrop must cover the whole window (so a click anywhere outside
        // closes them, and they can't linger alongside a modal), which a pane-local
        // `inset_0` can't do — so they're mounted at the app root instead (see
        // `app::render`), on top of every other overlay.

        // With the detail inspector open, dock it to the right of the grid via a
        // resizable split: the grid flexes, the inspector carries the user-set
        // width (caller-owned, like the sidebar/editor splits). The inspector never
        // occludes the grid, so the cursor and its live updates stay visible.
        // Closed, the grid keeps the full pane.
        if is_focused && self.inspector.is_some() {
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            div().size_full().child(
                SplitPane::new("result-split-inspector", Axis::Horizontal)
                    .sized(SplitSide::Trailing)
                    .size(active.inspector_w)
                    .gutter(px(1.))
                    .drag(active.inspector_drag)
                    .min_first(px(260.))
                    .max_first(px(720.))
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
                    .first(div().size_full().child(grid_pane))
                    .second(self.render_inspector(active, cx)),
            )
        } else {
            div().size_full().child(grid_pane)
        }
    }

    /// The column-stats bar: a thin summary line below the grid showing the
    /// selected column's pushed-down aggregates (count · distinct · nulls · min ·
    /// max, plus sum · avg for numerics). Shown only while the toggle is on; the
    /// values come from the grid's per-column `stats` view (loading / ready /
    /// failed), computed entirely by the engine.
    fn render_stats_bar(&self, grid: &super::ResultGrid, cx: &Context<Self>) -> gpui::AnyElement {
        use super::StatsState;
        let theme = cx.theme();
        let (dim, text, faint, muted, sep) = (
            theme.text_dim,
            theme.text,
            theme.text_faint,
            theme.text_muted,
            theme.border_soft,
        );
        let size_11 = theme.scale(11.);
        let row = div()
            .flex_shrink_0()
            .h(px(26.))
            .flex()
            .items_center()
            .gap_2()
            .px_3p5()
            .bg(theme.bg_panel)
            .border_t_1()
            .border_color(theme.border)
            .font_family(theme.font_family.clone())
            .text_size(size_11);

        let Some(view) = grid.stats.as_ref() else {
            return row
                .child(div().text_color(faint).child(crate::i18n::tr!(
                    "result.stats_pick_column",
                    "Select a column to summarize"
                )))
                .into_any_element();
        };
        // The column name leads, then the summary (or its loading/failed state).
        let row = row.child(div().text_color(muted).child(view.column.clone()));
        match &view.state {
            StatsState::Loading => row
                .child(stat_dot(sep))
                .child(
                    div()
                        .text_color(faint)
                        .child(crate::i18n::tr!("result.stats_computing", "computing…")),
                )
                .into_any_element(),
            StatsState::Failed => row
                .child(stat_dot(sep))
                .child(div().text_color(faint).child(crate::i18n::tr!(
                    "result.stats_unavailable",
                    "stats unavailable"
                )))
                .into_any_element(),
            StatsState::Ready(s) => {
                let nulls = (s.total - s.non_null).max(0);
                let mut row = row.child(stat_dot(sep)).child(stat_seg(
                    "count",
                    group_digits(s.total.max(0) as usize),
                    dim,
                    text,
                ));
                // distinct: the computed count, or a `—  [compute]` affordance when
                // the guard withheld the (potentially full-scan) count-distinct.
                row = match s.distinct {
                    Some(d) => row.child(stat_dot(sep)).child(stat_seg(
                        "distinct",
                        group_digits(d.max(0) as usize),
                        dim,
                        text,
                    )),
                    None => row.child(stat_dot(sep)).child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_color(dim)
                                    .child(crate::i18n::tr!("result.stats_distinct", "distinct")),
                            )
                            .child(div().text_color(faint).child("—"))
                            .child(
                                Button::new("stats-distinct", "compute")
                                    .variant(ButtonVariant::Ghost)
                                    .size(ButtonSize::Sm)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.compute_column_distinct(cx)
                                    })),
                            ),
                    ),
                };
                row = row
                    .child(stat_dot(sep))
                    .child(stat_seg("nulls", group_digits(nulls as usize), dim, text))
                    .child(stat_dot(sep))
                    .child(stat_seg("min", fmt_stat_value(&s.min), dim, text))
                    .child(stat_dot(sep))
                    .child(stat_seg("max", fmt_stat_value(&s.max), dim, text));
                if let Some(sum) = &s.sum {
                    row = row.child(stat_dot(sep)).child(stat_seg(
                        "sum",
                        fmt_stat_value(sum),
                        dim,
                        text,
                    ));
                }
                if let Some(avg) = &s.avg {
                    row = row.child(stat_dot(sep)).child(stat_seg(
                        "avg",
                        fmt_stat_value(avg),
                        dim,
                        text,
                    ));
                }
                row.into_any_element()
            }
        }
    }

    /// The draft (insert) rows zone, pinned below the grid: one row per
    /// staged `INSERT`, each cell click-to-edit, a leading ✕ to drop the draft.
    /// Tracks the grid's horizontal scroll so its columns stay column-aligned with
    /// the grid, and scrolls vertically on its own handle past
    /// [`DRAFT_ZONE_ROWS`] drafts. `None` when there are no drafts.
    fn render_draft_rows(
        &self,
        grid: &super::ResultGrid,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if grid.pending.inserts.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (faint, text, accent, border, line, bg) = (
            theme.text_faint,
            theme.text,
            theme.accent,
            theme.border,
            theme.border_soft,
            theme.bg_panel,
        );
        // Matches the grid's open editor (see `render_grid`): the cell being typed
        // into wears the input background, so both editors read the same.
        let edit_bg = theme.bg_input;
        let null_display: SharedString = self.settings.data.null_display.clone().into();
        let cell_colors = CellColors {
            text,
            muted: theme.text_muted,
            num: theme.orange,
            cyan: theme.cyan,
            faint,
            accent: theme.accent,
        };
        let row_height = self.settings.data.density.row_height();
        let mono_family = theme.mono_family.clone();
        let cell_size = theme.font_size;
        let gutter_px = gutter_width(grid.total);
        // Draft rows always carry a leading action cell holding the remove-X,
        // even when the row-number gutter is hidden — otherwise there is no way
        // to discard a staged insert.
        let ncols = grid.columns.len();
        let content_w = grid.content_width(gutter_px);
        // The draft zone lays its cells out against the same display order and
        // per-column widths as the grid above it.
        let draft_cols: Vec<usize> = grid.visible.clone();
        // The frozen band, mirrored from the grid: the draft rows' leading cells
        // have to hold the same left edge, or a staged insert's cells would slide
        // out from under the columns they belong to. The leading action cell rides
        // with the band (it is the gutter's counterpart here), so the split always
        // holds at least one cell.
        let frozen_lead = if grid.frozen_slots() > 0 || self.settings.data.row_numbers {
            1 + grid.frozen_slots()
        } else {
            0
        };
        let scroll_w: f32 = draft_cols
            .iter()
            .skip(grid.frozen_slots())
            .map(|&c| grid.width_of(c))
            .sum();
        let offset_x = grid.h_scroll.offset().x;
        // Indexed by *data* column, like the loop variable below, so a reordered
        // grid still gives each cell the width of the column it holds.
        let widths: Vec<f32> = (0..ncols).map(|c| grid.width_of(c)).collect();
        // The cell of an open editor that targets a draft row.
        let draft_inline: Option<(usize, usize, Entity<TextInput>)> =
            grid.grid_edit.as_ref().and_then(|e| match &e.slot {
                EditSlot::Draft { index, data_col } => Some((*index, *data_col, e.input.clone())),
                EditSlot::Row { .. } => None,
            });

        let mut rows = Vec::with_capacity(grid.pending.inserts.len());
        for (index, draft) in grid.pending.inserts.iter().enumerate() {
            let mut cells = Vec::with_capacity(ncols + 1);
            cells.push(
                div()
                    .w(px(gutter_px))
                    .flex_shrink_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_r_1()
                    .border_color(line)
                    .child(
                        div()
                            .id(("draft-remove", index))
                            .cursor_pointer()
                            .child(
                                // gpui's `svg()` paints only when the svg element's
                                // *own* `text_color` is set — it does not inherit
                                // from an ancestor div — so the base colour and the
                                // hover recolour both live directly on the svg.
                                gpui::svg()
                                    .path("icons/circle-x.svg")
                                    .size(theme.scale(14.))
                                    .flex_none()
                                    .text_color(faint)
                                    .hover(|s| s.text_color(accent)),
                            )
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.remove_draft_row(index, cx)),
                            ),
                    )
                    .into_any_element(),
            );
            // Display order, matching the grid above: a draft row that laid its
            // cells out per data column would shear away from the columns it
            // belongs to the moment one was hidden or moved.
            for &c in &draft_cols {
                if let Some((di, dc, input)) = &draft_inline
                    && *di == index
                    && *dc == c
                {
                    cells.push(
                        div()
                            .relative()
                            .w(px(widths.get(c).copied().unwrap_or(DATA_COL_WIDTH)))
                            .flex_shrink_0()
                            .h_full()
                            .px_2p5()
                            .flex()
                            .items_center()
                            .border_r_1()
                            .border_color(line)
                            .bg(edit_bg)
                            .child(input.clone())
                            // Anchor the FK picker below this draft cell.
                            .when_some(
                                grid.cell_suggest
                                    .as_ref()
                                    .and_then(|_| grid.cell_suggest_bounds.clone()),
                                |d, anchor| d.child(super::suggest::anchor_canvas(anchor)),
                            )
                            .into_any_element(),
                    );
                    continue;
                }
                // A column the engine computes takes no value on insert, so it gets no
                // editor: an offered one could only ever produce an engine error at
                // submit time.
                if !grid.insertable_column(c) {
                    cells.push(
                        div()
                            .w(px(widths.get(c).copied().unwrap_or(DATA_COL_WIDTH)))
                            .flex_shrink_0()
                            .h_full()
                            .px_2p5()
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            .border_r_1()
                            .border_color(line)
                            .text_color(faint)
                            .italic()
                            .child(crate::i18n::tr!("result.column_computed", "computed"))
                            .into_any_element(),
                    );
                    continue;
                }
                let content = match draft.cells.get(&c) {
                    Some(v) => render_cell(
                        &DisplayCell::from_value(v),
                        cell_colors,
                        &null_display,
                        false,
                        false,
                    ),
                    None => div()
                        .text_color(faint)
                        .italic()
                        .child(crate::i18n::tr!("result.column_default", "default"))
                        .into_any_element(),
                };
                cells.push(
                    div()
                        .id(("draft-cell", index * ncols + c))
                        .w(px(widths.get(c).copied().unwrap_or(DATA_COL_WIDTH)))
                        .flex_shrink_0()
                        .h_full()
                        .px_2p5()
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .border_r_1()
                        .border_color(line)
                        .cursor_pointer()
                        .child(content)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.begin_draft_edit(index, c, cx)),
                        )
                        .into_any_element(),
                );
            }
            rows.push(
                div()
                    .flex()
                    .items_center()
                    // Whole-width while the zone is the thing that x-scrolls; with
                    // a band frozen the row carries its own track instead and fills
                    // the pane, exactly like the rows above it.
                    .when(frozen_lead == 0, |d| d.w(px(content_w)))
                    .when(frozen_lead > 0, |d| d.w_full())
                    .h(row_height)
                    .border_b_1()
                    .border_color(line)
                    .map(|row| {
                        super::pinned::split_row(row, cells, frozen_lead, scroll_w, offset_x)
                    }),
            );
        }

        // Two nested scroll containers, one per axis: the outer tracks the grid's
        // `h_scroll` (so the drafts' columns stay under the grid's), the inner owns
        // the zone's vertical offset. `restrict_scroll_to_axis` on both keeps a
        // single-axis wheel from being redirected into the other container's axis,
        // which — with the two nested — would otherwise scroll both at once.
        let row_h = f32::from(row_height);
        let mut vscroll = div()
            .id("draft-rows-scroll")
            // Fixed to the columns' combined width and unshrinkable, so the rows
            // keep their extent inside the (narrower) horizontal viewport instead
            // of being squeezed to fit it — that extent is what x-scrolls. At
            // least the viewport's width, so a wheel over the strip beside
            // narrower columns still lands on this (the vertical) container.
            // Frozen columns move that extent inside each row, so the zone itself
            // is then just as wide as the pane.
            .when(frozen_lead == 0, |d| {
                d.w(px(content_w)).min_w(gpui::relative(1.))
            })
            .when(frozen_lead > 0, |d| d.w_full())
            .flex_shrink_0()
            .max_h(px(row_h * DRAFT_ZONE_ROWS))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .track_scroll(&grid.draft_scroll)
            .children(rows);
        vscroll.style().restrict_scroll_to_axis = Some(true);
        // The x-scrolling wrapper exists only while the zone scrolls as a whole.
        // With a band frozen, wrapping the split rows in it would scroll the band
        // away with them.
        let scroller = if frozen_lead > 0 {
            div()
                .id("draft-rows")
                .w_full()
                .flex_shrink_0()
                .child(vscroll)
        } else {
            let mut hscroll = div()
                .id("draft-rows")
                .w_full()
                .flex_shrink_0()
                .overflow_x_scroll()
                .track_scroll(&grid.h_scroll)
                .child(vscroll);
            hscroll.style().restrict_scroll_to_axis = Some(true);
            hscroll
        };

        // The zone's own scrollbar, so a change-set taller than the zone reads as
        // scrollable rather than truncated. Every draft row is exactly
        // `row_height` tall, so the thumb comes straight from the row counts.
        let ndrafts = grid.pending.inserts.len() as f32;
        let content_h = ndrafts * row_h;
        let viewport_h = ndrafts.min(DRAFT_ZONE_ROWS) * row_h;
        let max_scroll = content_h - viewport_h;
        let fraction = if max_scroll > 0. {
            (-f32::from(grid.draft_scroll.offset().y) / max_scroll).clamp(0., 1.)
        } else {
            0.
        };
        let scrub_handle = grid.draft_scroll.clone();
        let scrub_view = cx.entity();

        Some(
            div()
                // A column, so the zone's height follows its rows while the
                // scroller still stretches to the pane's width; `relative` seats
                // the scrollbar overlay at the zone's right edge.
                .relative()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .bg(bg)
                .border_t_1()
                .border_color(border)
                .font_family(mono_family)
                .text_size(cell_size)
                .child(scroller)
                .child(
                    Scrollbar::new("draft-scrollbar", &grid.draft_scrollbar)
                        .fraction(fraction)
                        .thumb(viewport_h / content_h)
                        .on_scrub(move |fraction, _, cx| {
                            let off = scrub_handle.offset();
                            scrub_handle.set_offset(point(off.x, px(-fraction * max_scroll)));
                            scrub_view.update(cx, |_, cx| cx.notify());
                        }),
                )
                .into_any_element(),
        )
    }

    /// The result cell's right-click context menu: the per-cell actions (Inspect
    /// · Copy) that used to sit in the toolbar, anchored at `pos` (the cursor).
    /// Both act on the cell the right-click just selected. A full-cover backdrop
    /// closes the menu on an outside click.
    /// The "Copy as" submenu: the same selection in whichever text shape the user
    /// is pasting into. Every entry routes through one
    /// [`copy_result_selection_as`](AppState::copy_result_selection_as), so a
    /// format is added here by naming it, not by growing a second copy path.
    ///
    /// The SQL form is offered only on a table browse: an `INSERT` needs a target
    /// table, and editor SQL has no single one to name.
    fn copy_as_submenu(&self, cx: &mut Context<Self>) -> Submenu {
        let entries: &[(&'static str, &str, ClipboardFormat)] = &[
            (
                "tsv-headers",
                "TSV with headers",
                ClipboardFormat::TsvHeaders,
            ),
            ("csv", "CSV", ClipboardFormat::Csv),
            ("json", "JSON", ClipboardFormat::Json),
            ("markdown", "Markdown table", ClipboardFormat::Markdown),
            ("in-list", "IN (…) list", ClipboardFormat::InList),
        ];
        let mut sub = Submenu::new("cell-copy-as", "Copy as");
        for (id, label, format) in entries {
            let format = *format;
            sub = sub.item(
                ContextMenuItem::new(format!("cell-copy-as-{id}"), *label).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.cell_menu = None;
                        this.copy_result_selection_as(format, cx);
                        cx.notify();
                    },
                )),
            );
        }
        if self.copy_as_sql_target(cx).is_some() {
            sub = sub.item(
                ContextMenuItem::new("cell-copy-as-sql", "SQL INSERT").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.cell_menu = None;
                        this.copy_result_selection_as(ClipboardFormat::Sql, cx);
                        cx.notify();
                    },
                )),
            );
        }
        sub
    }

    pub(crate) fn render_cell_menu(
        &self,
        pos: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // Editing entries appear only when the focused cell / row is
        // editable on a writable connection's keyed browse.
        let editable_cell = self.active_edit_target(cx).is_some();
        // `row_edit_enabled` already resolves against the active result's reported
        // edit contract, which is what "can this browse's rows change" means.
        let editable_browse = self.row_edit_enabled(cx);
        let mut menu = ContextMenu::new("result-cell-menu")
            .item(
                ContextMenuItem::new("cell-inspect", "Inspect")
                    .shortcut(crate::keymap::localize_hint("⌘I"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.cell_menu = None;
                        this.open_inspector(cx);
                        cx.notify();
                    })),
            )
            .item(
                ContextMenuItem::new("cell-copy", "Copy")
                    .shortcut(crate::keymap::localize_hint("⌘C"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.cell_menu = None;
                        this.copy_result_selection(cx);
                        cx.notify();
                    })),
            )
            .submenu(self.copy_as_submenu(cx))
            // Point the agent at what is selected instead of describing it. Row
            // data, so the tier ladder gates it — `add_reference` says so rather
            // than accepting a chip that would resolve to nothing.
            .item(
                ContextMenuItem::new("cell-ask-ai", "Ask AI about these rows").on_click(
                    cx.listener(|this, _, _, cx| {
                        this.cell_menu = None;
                        this.reference_selected_rows(cx);
                        cx.notify();
                    }),
                ),
            );
        // "Filter by": narrow the result to the focused cell's value
        // without writing SQL. Each item builds a `ResultFilter::Cmp` term, which
        // the driver renders and escapes, so the cell's value never reaches the
        // query as text. Hidden when no cell is focused (or its row was evicted).
        if let Some((column, value)) = self.cell_filter_target(cx) {
            let name = column.name.clone();
            let shown = crate::filter::value_label(&value);
            let null = matches!(value, Value::Null);
            let mut sub = Submenu::new("cell-filter", format!("Filter by {name}"));
            // A NULL cell can only meaningfully be compared for nullness, so the
            // `= value` pair would just be two spellings of the same two items.
            if !null {
                sub = sub
                    .item(
                        ContextMenuItem::new("cell-filter-eq", format!("= {shown}")).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.cell_menu = None;
                                this.filter_by_cell(CmpOp::Eq, false, cx);
                            }),
                        ),
                    )
                    .item(
                        ContextMenuItem::new("cell-filter-ne", format!("<> {shown}")).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.cell_menu = None;
                                this.filter_by_cell(CmpOp::Ne, false, cx);
                            }),
                        ),
                    )
                    // Substring rather than equality: the common follow-up when
                    // the exact cell value is too narrow.
                    .item(
                        ContextMenuItem::new("cell-filter-contains", format!("contains {shown}"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cell_menu = None;
                                this.filter_by_cell(CmpOp::Contains, false, cx);
                            })),
                    );
            }
            sub = sub
                .item(
                    ContextMenuItem::new("cell-filter-null", "IS NULL").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.cell_menu = None;
                            this.filter_by_cell(CmpOp::IsNull, false, cx);
                        },
                    )),
                )
                .item(
                    ContextMenuItem::new("cell-filter-not-null", "IS NOT NULL").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.cell_menu = None;
                            this.filter_by_cell(CmpOp::IsNotNull, false, cx);
                        }),
                    ),
                );
            // Narrow *further* rather than replacing, the multi-term flow the bar's
            // Column mode also builds. Only offered when there's a built filter to
            // add to; against a Contains/WHERE filter there is no structure to join.
            if self.can_add_cell_filter_term(cx) && !null {
                sub = sub.item(
                    ContextMenuItem::new("cell-filter-and", format!("Add: = {shown}")).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.cell_menu = None;
                            this.filter_by_cell(CmpOp::Eq, true, cx);
                        }),
                    ),
                );
            }
            menu = menu.separator().submenu(sub);
        }
        // FK navigation: jump to the referenced row or list the tables that
        // reference this one. Both need the FK graph to have edges for the focused
        // column/table.
        let (fk_forward, fk_reverse) = self.fk_menu(cx);
        if fk_forward.is_some() || !fk_reverse.is_empty() {
            menu = menu.separator();
        }
        if let Some(target) = fk_forward {
            menu = menu.item(
                ContextMenuItem::new("fk-forward", format!("Go to {target}")).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.cell_menu = None;
                        this.follow_fk_forward(cx);
                        cx.notify();
                    }),
                ),
            );
        }
        for (i, rev) in fk_reverse.into_iter().enumerate() {
            let label = format!("Show rows in {} ({})", rev.table, rev.from_column);
            menu = menu.item(ContextMenuItem::new(format!("fk-rev-{i}"), label).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.cell_menu = None;
                    this.follow_fk_reverse(
                        rev.schema.clone(),
                        rev.table.clone(),
                        rev.from_column.clone(),
                        rev.to_column.clone(),
                        cx,
                    );
                    cx.notify();
                }),
            ));
        }
        // Inline FK expansion: pull the focused FK cell's referenced
        // columns into the grid (a ✓ marks ones already shown), hide a joined
        // column, or clear them all. The per-column list comes from the referenced
        // table's prefetched detail; the Columns panel is the fuller, recursive UI.
        let ref_menu = self.reference_menu(cx);
        let joined_path = self.focused_joined_path(cx);
        let has_expansion = self.active_has_expansion(cx);
        if ref_menu.as_ref().is_some_and(|m| !m.columns.is_empty())
            || joined_path.is_some()
            || has_expansion
        {
            menu = menu.separator();
        }
        if let Some(ref_menu) = ref_menu
            && !ref_menu.columns.is_empty()
        {
            // The referenced table's columns can run long (every column of a
            // wide table), so they live in a hover-opened flyout rather than
            // padding out the main menu. `ContextMenu` opens it on hover and
            // closes it again when a sibling row is entered.
            let mut sub = Submenu::new("ref-cols", format!("Show from {}", ref_menu.ref_table));
            for (i, item) in ref_menu.columns.into_iter().enumerate() {
                let mark = if item.shown { "✓ " } else { "    " };
                let path = item.path;
                sub = sub.item(
                    ContextMenuItem::new(format!("ref-col-{i}"), format!("{mark}{}", item.label))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.cell_menu = None;
                            this.toggle_reference_column(path.clone(), cx);
                            cx.notify();
                        })),
                );
            }
            menu = menu.submenu(sub);
        }
        if let Some(path) = joined_path {
            menu = menu.item(
                ContextMenuItem::new("ref-hide", format!("Hide {}", path.join("."))).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.cell_menu = None;
                        this.toggle_reference_column(path.clone(), cx);
                        cx.notify();
                    }),
                ),
            );
        }
        if has_expansion {
            menu = menu.item(
                ContextMenuItem::new("ref-clear", "Hide all reference columns").on_click(
                    cx.listener(|this, _, _, cx| {
                        this.cell_menu = None;
                        this.clear_reference_columns(cx);
                        cx.notify();
                    }),
                ),
            );
        }
        if editable_cell {
            menu = menu
                .item(
                    ContextMenuItem::new("cell-edit", "Edit cell")
                        .shortcut("↵")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cell_menu = None;
                            this.begin_grid_edit(cx);
                            cx.notify();
                        })),
                )
                .item(
                    ContextMenuItem::new("cell-null", "Set NULL")
                        .shortcut(crate::keymap::localize_hint("⌥⌘0"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cell_menu = None;
                            this.set_cell_null(cx);
                            cx.notify();
                        })),
                );
        }
        // Pinning is display-only, so it is offered on every result (editor SQL
        // included) and never gated on the edit contract. The label names what the
        // click will do to the row under the cursor.
        menu = menu.separator().item(
            ContextMenuItem::new(
                "row-pin",
                if self.cursor_row_pinned(cx) {
                    "Unpin row"
                } else {
                    "Pin row"
                },
            )
            .shortcut(crate::keymap::localize_hint("⌥⌘P"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.cell_menu = None;
                this.toggle_pin_rows(cx);
                cx.notify();
            })),
        );
        if editable_browse {
            menu = menu.item(
                ContextMenuItem::new("row-delete", "Toggle row deletion")
                    .shortcut(crate::keymap::localize_hint("⌘⌫"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.cell_menu = None;
                        this.toggle_delete_rows(cx);
                        cx.notify();
                    })),
            );
        }
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.cell_menu = None;
                    cx.notify();
                }),
            )
            .child(floating(div().occlude().child(menu)).at(pos))
    }

    /// The result header's right-click menu, acting on the column at display
    /// `slot`: its width, its visibility, and where it sits among the others.
    ///
    /// Everything here is display-only. Nothing re-runs the query, so a user can
    /// rearrange a 60-column result without paying for a round trip, and the
    /// grid's data-keyed state (widths, staged edits, stats) is untouched.
    pub(crate) fn render_header_menu(
        &self,
        pos: Point<Pixels>,
        slot: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let grid = match &self.phase {
            Phase::Connected(active) => active.active_result(),
            _ => None,
        };
        // Read once into a plain reference: the rest of this builder treats the
        // grid as data, and `Option<&ResultGrid>` stays `Copy` the way the field
        // borrow used to be.
        let grid = grid.as_ref().map(|g| g.read(cx));
        let (name, visible_len, hidden) = match grid {
            Some(grid) => (
                grid.data_col_at(slot)
                    .and_then(|dc| grid.columns().get(dc))
                    .map(|c| c.name.clone())
                    .unwrap_or_default(),
                grid.visible_len(),
                grid.hidden_columns()
                    .into_iter()
                    .filter_map(|dc| grid.columns().get(dc).map(|c| (dc, c.name.clone())))
                    .collect::<Vec<_>>(),
            ),
            None => (String::new(), 0, Vec::new()),
        };
        let frozen = grid.map(|g| g.frozen_slots()).unwrap_or(0);

        let mut menu = ContextMenu::new("result-header-menu")
            .item(
                ContextMenuItem::new("header-fit", "Fit this column").on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.header_menu = None;
                        let style = HeaderStyle::new(&this.settings);
                        this.with_grid(cx, |grid| {
                            if let Some(dc) = grid.data_col_at(slot) {
                                grid.auto_fit(dc, style);
                            }
                        });
                        cx.notify();
                    },
                )),
            )
            .item(
                ContextMenuItem::new("header-fit-all", "Fit all columns").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.header_menu = None;
                        let style = HeaderStyle::new(&this.settings);
                        this.with_grid(cx, |grid| grid.auto_fit_all(style));
                        cx.notify();
                    },
                )),
            )
            .item(
                ContextMenuItem::new("header-reset-widths", "Reset column widths").on_click(
                    cx.listener(|this, _, _, cx| {
                        this.header_menu = None;
                        this.with_grid(cx, |grid| grid.reset_widths());
                        cx.notify();
                    }),
                ),
            )
            .separator();

        // Reorder. Offered only where there is somewhere to go, so the menu never
        // shows an item that would do nothing.
        if slot > 0 {
            menu = menu.item(
                ContextMenuItem::new("header-move-left", "Move left").on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.header_menu = None;
                        this.with_grid(cx, |grid| grid.move_column(slot, slot - 1));
                        cx.notify();
                    },
                )),
            );
        }
        if slot + 1 < visible_len {
            menu = menu.item(
                ContextMenuItem::new("header-move-right", "Move right").on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.header_menu = None;
                        this.with_grid(cx, |grid| grid.move_column(slot, slot + 1));
                        cx.notify();
                    },
                )),
            );
        }
        if slot > 0 {
            menu = menu.item(
                ContextMenuItem::new("header-move-first", "Move to front").on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.header_menu = None;
                        this.with_grid(cx, |grid| grid.move_column(slot, 0));
                        cx.notify();
                    },
                )),
            );
        }

        // Freezing. A frozen column holds the left edge while the rest scrolls
        // under it; the band is contiguous, so pinning the n-th column pins
        // everything up to it and unpinning one releases it and everything after.
        // Withheld when there is only one column, where a frozen band would leave
        // nothing to scroll.
        if visible_len > 1 {
            menu = menu.separator();
            menu = if slot < frozen {
                menu.item(
                    ContextMenuItem::new("header-unpin", format!("Unpin {name}")).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.header_menu = None;
                            this.with_grid(cx, |grid| grid.unpin_column_at(slot));
                            cx.notify();
                        }),
                    ),
                )
            } else {
                menu.item(
                    ContextMenuItem::new("header-pin", format!("Pin {name} left")).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.header_menu = None;
                            let gutter = this.gutter();
                            let pinned =
                                this.with_grid(cx, |grid| grid.pin_column_at(slot, gutter));
                            // The band has a ceiling (see `MAX_FROZEN_FRACTION`);
                            // a refusal that said nothing would read as a dead menu
                            // item.
                            if pinned == Some(false) {
                                this.notify(
                                    ToastVariant::Info,
                                    crate::i18n::tr!(
                                        "result.pin_column_too_wide",
                                        "Frozen columns can take at most half the grid; \
                                         narrow one or unpin another first"
                                    ),
                                    cx,
                                );
                            }
                            cx.notify();
                        }),
                    ),
                )
            };
            if frozen > 0 {
                menu = menu.item(
                    ContextMenuItem::new("header-unpin-all", "Unpin all columns").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.header_menu = None;
                            this.with_grid(cx, |grid| grid.unpin_all_columns());
                            cx.notify();
                        }),
                    ),
                );
            }
        }

        // Hiding the last visible column would leave no header to right-click, so
        // the grid refuses it and the item is withheld rather than offered dead.
        if visible_len > 1 {
            menu = menu.separator().item(
                ContextMenuItem::new("header-hide", format!("Hide {name}")).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.header_menu = None;
                        this.with_grid(cx, |grid| {
                            grid.hide_slot(slot);
                        });
                        cx.notify();
                    },
                )),
            );
        }
        if !hidden.is_empty() {
            let mut sub = Submenu::new("header-show", format!("Show hidden ({})", hidden.len()));
            for (dc, col_name) in hidden {
                sub = sub.item(
                    ContextMenuItem::new(format!("header-show-{dc}"), col_name).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.header_menu = None;
                            this.with_grid(cx, |grid| grid.show_column(dc));
                            cx.notify();
                        }),
                    ),
                );
            }
            menu = menu.submenu(sub).item(
                ContextMenuItem::new("header-show-all", "Show all columns").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.header_menu = None;
                        this.with_grid(cx, |grid| grid.show_all_columns());
                        cx.notify();
                    },
                )),
            );
        }

        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.header_menu = None;
                    cx.notify();
                }),
            )
            .child(floating(div().occlude().child(menu)).at(pos))
    }

    /// The result toolbar's "Export" dropdown: CSV / JSON / HTML grouped into one
    /// menu, anchored at `pos` (where the button was clicked). A full-cover backdrop
    /// dismisses it on an outside click, mirroring [`Self::render_cell_menu`].
    pub(crate) fn render_export_menu(
        &self,
        pos: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let menu = ContextMenu::new("result-export-menu")
            .item(
                ContextMenuItem::new("export-csv", "CSV").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.export_menu = None;
                        this.export_result(ExportFormat::Csv, cx);
                        cx.notify();
                    },
                )),
            )
            .item(
                ContextMenuItem::new("export-json", "JSON").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.export_menu = None;
                        this.export_result(ExportFormat::Json, cx);
                        cx.notify();
                    },
                )),
            )
            .item(
                ContextMenuItem::new("export-html", "HTML").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.export_menu = None;
                        this.export_result(ExportFormat::Html, cx);
                        cx.notify();
                    },
                )),
            )
            .item(
                ContextMenuItem::new("export-sql", "SQL (INSERT)").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.export_menu = None;
                        this.export_result(ExportFormat::Sql, cx);
                        cx.notify();
                    },
                )),
            )
            .item(
                ContextMenuItem::new("export-xlsx", "Excel (.xlsx)").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.export_menu = None;
                        this.export_result(ExportFormat::Xlsx, cx);
                        cx.notify();
                    },
                )),
            );
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.export_menu = None;
                    cx.notify();
                }),
            )
            .child(floating(div().occlude().child(menu)).at(pos))
    }

    /// The result toolbar's "More" dropdown; the less-used actions (the Stats
    /// toggle and "Copy to…") collected into one menu, anchored at `pos`. A
    /// full-cover backdrop dismisses it, mirroring [`Self::render_cell_menu`].
    pub(crate) fn render_more_menu(
        &self,
        pos: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // "Copy to…" needs a ready result as its source; the Stats toggle is
        // always available and carries a leading check while its bar is on.
        let ready = matches!(&self.phase, Phase::Connected(a) if a.active_result().is_some_and(|g| g.read(cx).ready));
        let stats_on = matches!(&self.phase, Phase::Connected(a) if a.active_result().is_some_and(|g| g.read(cx).stats_bar));
        let stats_label = if stats_on { "✓ Stats" } else { "Stats" };
        let mut menu = ContextMenu::new("result-more-menu").item(
            ContextMenuItem::new("more-stats", stats_label).on_click(cx.listener(
                |this, _, _, cx| {
                    this.more_menu = None;
                    this.toggle_stats_bar(cx);
                    cx.notify();
                },
            )),
        );
        if ready {
            menu = menu.item(ContextMenuItem::new("more-copy-to", "Copy to…").on_click(
                cx.listener(|this, _, _, cx| {
                    this.more_menu = None;
                    this.open_copy_picker(cx);
                    cx.notify();
                }),
            ));
        }
        // Offered only with rows pinned: the strip is the only thing this entry
        // acts on, and an always-present "Unpin all" would imply pinning lives here.
        let pinned = self.pinned_row_count(cx);
        if pinned > 0 {
            menu = menu.item(
                ContextMenuItem::new("more-unpin-all", format!("Unpin all rows ({pinned})"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.more_menu = None;
                        this.unpin_all_rows(cx);
                        cx.notify();
                    })),
            );
        }
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.more_menu = None;
                    cx.notify();
                }),
            )
            .child(floating(div().occlude().child(menu)).at(pos))
    }
}
