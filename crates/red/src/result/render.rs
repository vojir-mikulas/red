//! Result-cell rendering and the results-pane view: colors cells by value kind
//! (numbers accented, UUIDs dimmed, JSON-ish text cyan) and assembles the
//! toolbar · grid · footer · scrollbar that make up the pane.

use std::rc::Rc;

use flint::prelude::*;
use flint::TextInput;
use gpui::{
    div, prelude::*, px, Axis, Entity, Hsla, MouseButton, Pixels, Point, SharedString, Window,
};
use red_core::ExportFormat;

use super::buffer::{CellKind, DisplayCell};
use super::edit::EditSlot;
use super::{DATA_COL_WIDTH, GUTTER_WIDTH};
use crate::app::{ActiveConn, AppState, Pane, Phase};

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

/// Colors a result cell carries, keyed by value kind (so the grid reads at a
/// glance the way the design does: numbers orange, UUIDs dimmed, JSON cyan).
#[derive(Clone, Copy)]
struct CellColors {
    text: Hsla,
    muted: Hsla,
    num: Hsla,
    cyan: Hsla,
    faint: Hsla,
}

/// One grid cell, colored by its pre-classified [`CellKind`] (NULL italic-faint,
/// numbers accented, UUIDs dimmed, JSON-ish text cyan — mirroring the design's
/// typed cells). The display string and kind were computed once when the row
/// landed in the buffer, so this only picks a color and clones a `SharedString`
/// (an `Arc` bump) — no per-frame formatting, copying, or classification.
fn render_cell(
    cell: &DisplayCell,
    c: CellColors,
    null_display: &SharedString,
    struck: bool,
) -> gpui::AnyElement {
    let color = match cell.kind {
        CellKind::Null | CellKind::Blob => c.faint,
        CellKind::Num => c.num,
        CellKind::Text => c.text,
        CellKind::Uuid => c.muted,
        CellKind::Json => c.cyan,
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
        // A row pending deletion (Track B6) reads struck-through, so the marking is
        // legible without relying on the soft red tint alone.
        .when(struck, |d| d.line_through())
        .when(italic, |d| d.italic())
        .child(text)
        .into_any_element()
}

impl AppState {
    /// The results pane: an empty state, an error, or the live windowed grid.
    pub(crate) fn render_result(
        &self,
        active: &ActiveConn,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, bg_app, border, border_soft) = (
            theme.bg_panel,
            theme.bg_app,
            theme.border,
            theme.border_soft,
        );
        let (muted, faint, dim, text) = (
            theme.text_muted,
            theme.text_faint,
            theme.text_dim,
            theme.text,
        );
        let (num, cyan, red, accent) = (theme.orange, theme.cyan, theme.red, theme.accent);
        // Scaled chrome sizes snapshotted here (Pixels is Copy) so the result
        // pane's status/empty/error text tracks the UI font size even inside the
        // `'static` row closures below.
        let (size_11, size_12) = (theme.scale(11.), theme.scale(12.));
        let caret_icon = theme.scale(9.);
        // Chrome (toolbar/stats/footer) follows the sans UI font; the data grid
        // cells follow the mono font, both rendered at the configured base size.
        let ui_family = theme.font_family.clone();
        let mono_family = theme.mono_family.clone();
        let cell_size = theme.font_size;
        let cell_colors = CellColors {
            text,
            muted,
            num,
            cyan,
            faint,
        };
        // The focus + cell-cursor keys live on the `Table` itself (see its
        // `.focus_handle`/`.on_nav` below); the pane draws no focus ring.
        let container = div().size_full().relative().flex().flex_col().bg(bg);

        let grid = match active.active_result() {
            Some(grid) => grid,
            None => {
                return container.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(size_12)
                        .text_color(faint)
                        .child("Double-click a table or run a query to see rows"),
                );
            }
        };

        let elapsed = format_duration(grid.query_time());

        // A failed query gets a full-pane panel rather than the cramped toolbar
        // status slot — syntax errors are multi-line and would otherwise clip.
        if let Some(err) = &grid.error {
            return container.child(
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
                            .child("Query failed")
                            .child(div().text_color(faint).child(format!("· {elapsed}"))),
                    )
                    .child(div().text_size(size_12).text_color(text).child(err.clone())),
            );
        }

        let status = if !grid.ready {
            div().text_color(faint).child(format!("running… {elapsed}"))
        } else {
            div()
                .text_color(faint)
                .child(format!("{} rows · {elapsed}", grid.total))
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
                    // "+ Row" appends a draft (insert) row — shown only on an
                    // editable keyed browse of a writable connection (Track B6).
                    .when(self.editing_enabled() && grid.editable_browse(), |d| {
                        d.child(
                            Button::new("result-add-row", "+ Row")
                                .variant(ButtonVariant::Ghost)
                                .size(ButtonSize::Sm)
                                .on_click(cx.listener(|this, _, _, cx| this.add_draft_row(cx))),
                        )
                    })
                    .child(
                        // ⌘⇧F — toggle the filter bar. Reads as "filled" while a
                        // filter is applied (Track B2).
                        Button::new("result-filter", "Filter")
                            .variant(if grid.filter.is_some() {
                                ButtonVariant::Secondary
                            } else {
                                ButtonVariant::Ghost
                            })
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_filter_bar(cx))),
                    )
                    .child(
                        Button::new("result-csv", "CSV")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_result(ExportFormat::Csv, cx)
                            })),
                    )
                    .child(
                        Button::new("result-json", "JSON")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_result(ExportFormat::Json, cx)
                            })),
                    )
                    .child(
                        // Save the table as a themed standalone HTML file (a plain
                        // export format alongside CSV/JSON). AI-authored *reports* are
                        // a separate, on-demand thing the assistant generates.
                        Button::new("result-html", "HTML")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_result(ExportFormat::Html, cx)
                            })),
                    ),
            );

        if !grid.ready {
            return container.child(toolbar);
        }

        // An optional leading row-number gutter, then one fixed-width, sortable
        // column per result column. Each header carries the engine's declared type
        // as a dim subtitle, like the design's typed headers (`email` + `text`).
        // The gutter occupies table column 0 when shown, so a data column's table
        // index is `data + gutter` (see the handlers in `mod.rs`).
        let show_gutter = self.settings.grid.row_numbers;
        let gutter = show_gutter as usize;
        let mut columns: Vec<Column> = Vec::with_capacity(grid.columns.len() + gutter);
        if show_gutter {
            columns.push(Column::new("#").width(px(GUTTER_WIDTH)).align_end());
        }
        for c in &grid.columns {
            let mut col = Column::new(c.name.clone())
                .width(px(DATA_COL_WIDTH))
                .sortable();
            if let Some(t) = &c.decl_type {
                if !t.is_empty() {
                    col = col.subtitle(t.to_lowercase());
                }
            }
            columns.push(col);
        }
        let sort = grid.sort.map(|(c, asc)| (c + gutter, asc));
        let total = grid.total;
        let ncols = grid.columns.len();
        let buffer_range = grid.buffer.clone();
        let buffer_row = grid.buffer.clone();
        let sender = grid.sender.clone();
        let epoch = grid.epoch;
        let (sort_view, cell_view, nav_view) = (view.clone(), view.clone(), view.clone());
        let sec_view = view.clone();

        // Resolve (and possibly re-center) the virtual-scroll window for this
        // frame; everything below works in list-local coordinates offset by
        // `base`, so the list only ever lays out `win.len` rows.
        let row_height = self.settings.grid.density.row_height();
        let null_display: SharedString = self.settings.grid.null_display.clone().into();
        let win = grid.prepare_window(row_height);
        let base = win.base;
        // The selection is stored in absolute ordinals; translate it into the
        // window's local rows for highlighting (off-window rows just aren't
        // painted). The TSV copy reads the buffer in absolute space, so it stays
        // correct regardless.
        let local_selection = grid.selection.map(|mut r| {
            r.anchor.0 = r.anchor.0.saturating_sub(base);
            r.focus.0 = r.focus.0.saturating_sub(base);
            r
        });

        // Staged-edit overlay (Track B6): the dirty cells + deleted rows for this
        // frame, shared (via `Rc`) between the cell renderer and the cell-tint hook.
        // Tints: a soft amber under a staged cell, a soft red under a row pending
        // deletion (the selection highlight still wins on top).
        let overlay = Rc::new(grid.pending.overlay());
        let dirty_tint = Hsla { a: 0.22, ..num };
        let delete_tint = Hsla { a: 0.16, ..red };
        let (overlay_cells, overlay_bg) = (overlay.clone(), overlay.clone());
        // Find-in-result highlight (Track B2, Tier 1): the resident cells matching
        // the open find bar's term get a soft accent tint via the same `cell_bg`
        // hook. The focused match is *also* the grid selection, so the selection
        // highlight marks "current" on top of this. Keyed by `(ordinal, data col)`.
        let find_hits: std::collections::HashSet<(usize, usize)> = self
            .find_bar
            .as_ref()
            .map(|b| b.grid_matches.iter().copied().collect())
            .unwrap_or_default();
        let find_tint = Hsla { a: 0.20, ..accent };
        // The open inline editor's target cell (existing rows only; draft rows host
        // their own editor in the bottom zone), so the renderer swaps in its field.
        let inline: Option<(usize, usize, Entity<TextInput>)> =
            self.grid_edit.as_ref().and_then(|e| match &e.slot {
                EditSlot::Row { row, data_col, .. } => Some((*row, *data_col, e.input.clone())),
                EditSlot::Draft { .. } => None,
            });

        // The focused cell, spoken aloud: the grid reports this as its accessible
        // name (a `Grid` landmark), so a screen reader announces "<column>:
        // <value>, row N of M" each time the cell cursor moves — the one piece of
        // state a blind user needs to read the data. `focus` is in absolute,
        // table-column coordinates (gutter included); a data column's index is
        // `table_col - gutter`. Falls back to the grid's name when there's no cursor.
        let a11y_label: SharedString = grid
            .selection
            .map(|sel| {
                let (row, table_col) = sel.focus;
                let pos = format!("row {} of {}", group_digits(row + 1), group_digits(total));
                if show_gutter && table_col == 0 {
                    return SharedString::from(format!("Row number, {pos}"));
                }
                let data_col = table_col - gutter;
                let col_name = grid
                    .columns
                    .get(data_col)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                let value = match grid.buffer.borrow().row(row) {
                    Some(r) => match r.display.get(data_col) {
                        Some(cell) if cell.kind == CellKind::Null => "null".to_string(),
                        Some(cell) => cell.text.to_string(),
                        None => "empty".to_string(),
                    },
                    None => "loading".to_string(),
                };
                SharedString::from(format!("{col_name}: {value}, {pos}"))
            })
            .unwrap_or_else(|| SharedString::from("Results grid"));

        let table = Table::<()>::new("result-grid", columns)
            .row_count(win.len)
            .row_height(row_height)
            .font_family(mono_family.clone())
            .text_size(cell_size)
            .grid_lines(true)
            .track_scroll(&grid.scroll)
            .track_horizontal_scroll(&grid.h_scroll)
            .horizontal(true)
            // Keyboard cell cursor: the grid pane's focus handle lives on the
            // table, and arrow/Home/End/Page/⌘-arrow intents drive the selection.
            .focus_handle(active.grid_focus.clone())
            .on_nav(move |nav, extend, _window, cx| {
                nav_view
                    .update(cx, |this, cx| this.result_cursor_move(nav, extend, cx))
                    .ok();
            })
            .selected_cells(local_selection)
            .cell_bg(move |ix, table_col| {
                let abs = base + ix;
                if overlay_bg.deleted.contains(&abs) {
                    return Some(delete_tint);
                }
                if table_col >= gutter && overlay_bg.cells.contains_key(&(abs, table_col - gutter))
                {
                    return Some(dirty_tint);
                }
                if table_col >= gutter && find_hits.contains(&(abs, table_col - gutter)) {
                    return Some(find_tint);
                }
                None
            })
            .a11y_label(a11y_label)
            .sort(sort)
            .sort_carets(
                move || crate::icons::icon("sort-asc", caret_icon, accent).into_any_element(),
                move || crate::icons::icon("sort-desc", caret_icon, accent).into_any_element(),
            )
            .on_visible_range(move |range, window, _| {
                // `range` is list-local; the buffer is keyed by absolute ordinal.
                let abs = (base + range.start)..(base + range.end);
                let settled = buffer_range.borrow_mut().ensure(abs, total, epoch, &sender);
                // Mid-fling we skipped fetching; ask for another paint so the
                // window that the scroll settles on still gets loaded.
                if !settled {
                    window.refresh();
                }
            })
            .on_sort(move |table_col, window, cx| {
                // ⌘/Ctrl-click a header selects the whole column; add Shift to
                // extend the column span; a plain click sorts. The header path has
                // no click event, so the live modifier state is read off the window.
                let mods = window.modifiers();
                let select_column = mods.secondary();
                let extend = mods.shift;
                sort_view
                    .update(cx, |this, cx| {
                        if select_column {
                            // Focus the grid so the cell cursor + ⌘C land on this
                            // selection rather than a still-focused editor/field.
                            this.focus_pane(Pane::Grid, window, cx);
                            this.result_select_column(table_col, extend, cx);
                        } else {
                            this.result_sort(table_col, cx);
                        }
                    })
                    .ok();
            })
            .on_cell_click(move |row, table_col, event, window, cx| {
                let extend = event.modifiers().shift;
                let inspect = event.click_count() >= 2;
                let abs_row = base + row;
                cell_view
                    .update(cx, |this, cx| {
                        // Focus the grid so the cell cursor + ⌘C land on this
                        // selection, not a still-focused editor/field.
                        this.focus_pane(Pane::Grid, window, cx);
                        this.result_select(abs_row, table_col, extend, cx);
                        // Double-click edits the cell in place when it's editable
                        // (Track B6); otherwise it reveals the detail inspector.
                        if inspect {
                            this.begin_grid_edit(cx);
                            if this.grid_edit.is_none() {
                                this.open_inspector(cx);
                            }
                        }
                    })
                    .ok();
            })
            // Right-click selects the cell and opens its context menu (Inspect ·
            // Copy) anchored at the cursor — the per-cell actions that used to live
            // in the toolbar.
            .on_cell_secondary(move |row, table_col, pos, window, cx| {
                let abs_row = base + row;
                sec_view
                    .update(cx, |this, cx| {
                        this.focus_pane(Pane::Grid, window, cx);
                        this.result_select(abs_row, table_col, false, cx);
                        this.cell_menu = Some(pos);
                        cx.notify();
                    })
                    .ok();
            })
            .render_row(move |ix, _, _| {
                // `ix` is list-local; the gutter and buffer are absolute.
                let abs = base + ix;
                let mut out = Vec::with_capacity(ncols + gutter);
                let buffer = buffer_row.borrow();
                let struck = overlay_cells.deleted.contains(&abs);
                if show_gutter {
                    // After an interpolated jump the run's ordinals are estimates;
                    // the gutter marks them `≈` until a true end pins them exact.
                    let label = if buffer.is_estimated() {
                        format!("≈{}", group_digits(abs + 1))
                    } else {
                        group_digits(abs + 1)
                    };
                    out.push(div().text_color(faint).child(label).into_any_element());
                }
                let resident = buffer.row(abs);
                for c in 0..ncols {
                    // The open inline editor takes over its cell. The field is
                    // `bare`, so it fills the cell (the Flint cell wrapper supplies
                    // the height/padding) rather than drawing a smaller box inside.
                    if let Some((er, ec, input)) = &inline {
                        if *er == abs && *ec == c {
                            out.push(input.clone().into_any_element());
                            continue;
                        }
                    }
                    // A staged value (dirty cell) shadows the resident one.
                    if let Some(cell) = overlay_cells.cells.get(&(abs, c)) {
                        out.push(render_cell(cell, cell_colors, &null_display, struck));
                        continue;
                    }
                    match resident.and_then(|r| r.display.get(c)) {
                        Some(cell) => {
                            out.push(render_cell(cell, cell_colors, &null_display, struck))
                        }
                        None => out.push(div().text_color(faint).child("·").into_any_element()),
                    }
                }
                out
            });

        // Footer: a strong row count, the column count, and the result's label —
        // the design's "N rows · K columns" status strip under the grid.
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
            .child(div().text_color(dim).child("rows"))
            .child(div().text_color(border_soft).child("·"))
            .child(div().text_color(dim).child(format!("{ncols} columns")))
            .child(div().text_color(border_soft).child("·"))
            // Which paging mode this result got (keyset = seek key resolved;
            // offset = the O(offset) fallback) — the at-a-glance diagnostic.
            .child(
                div()
                    .text_color(dim)
                    .child(if grid.buffer.borrow().is_keyed() {
                        "keyset"
                    } else {
                        "offset"
                    }),
            )
            // Staged-edit controls (Track B6): a count + Submit / Revert, shown only
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
            .child(div().ml_auto().text_color(dim).child(grid.label.clone()));

        // The draggable, fraction-mapped scrollbar: the thumb mirrors the list's
        // position; a scrub jumps the viewport, and the buffer's `ensure` turns
        // the far jump into one key-space seek (keyed results) or one OFFSET page
        // (fallback).
        let scrub_scroll = grid.scroll.clone();
        let scrub_window = grid.window_base.clone();
        let scrub_view = view.clone();
        let rh = f32::from(row_height);
        let scrollbar = Scrollbar::new("result-scrollbar", &grid.scrollbar)
            // Position is computed over the whole result (not the f32-bounded
            // window the list lays out), so the thumb is honest at 50M rows.
            .fraction(win.fraction)
            .thumb(win.thumb)
            .on_scrub(move |fraction, _, cx| {
                let target = (fraction as f64 * total.saturating_sub(1) as f64).round() as usize;
                super::place_window(&scrub_window, &scrub_scroll, total, target, rh);
                scrub_view.update(cx, |_, cx| cx.notify()).ok();
            });

        let grid_pane = container
            .child(toolbar)
            // The filter bar (Track B2) sits between the toolbar and the grid when
            // open; narrowing re-opens the result so the grid below just repaints.
            .when_some(self.render_filter_bar(cx), |c, bar| c.child(bar))
            // The find bar (Track B2, Tier 1) sits alongside the filter bar; it
            // only highlights loaded rows, so the grid below just repaints.
            .when_some(self.render_find_bar(crate::find::FindTarget::Grid, cx), |c, bar| {
                c.child(bar)
            })
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .bg(bg_app)
                    .relative()
                    .child(table)
                    .child(scrollbar),
            )
            // Draft (insert) rows pinned below the grid (Track B6).
            .when_some(self.render_draft_rows(grid, cx), |c, drafts| {
                c.child(drafts)
            })
            .child(footer)
            // The cell right-click menu floats above the pane, anchored at the
            // cursor; a full-cover backdrop dismisses it on an outside click.
            .when_some(self.cell_menu, |c, pos| {
                c.child(self.render_cell_menu(pos, cx))
            });

        // With the detail inspector open, dock it to the right of the grid via a
        // resizable split: the grid flexes, the inspector carries the user-set
        // width (caller-owned, like the sidebar/editor splits). The inspector never
        // occludes the grid, so the cursor and its live updates stay visible.
        // Closed, the grid keeps the full pane.
        if self.inspector.is_some() {
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

    /// The draft (insert) rows zone (Track B6), pinned below the grid: one row per
    /// staged `INSERT`, each cell click-to-edit, a leading ✕ to drop the draft.
    /// Shares the grid's horizontal scroll so its columns track the grid's. `None`
    /// when there are no drafts.
    fn render_draft_rows(
        &self,
        grid: &super::ResultGrid,
        cx: &mut Context<Self>,
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
        let null_display: SharedString = self.settings.grid.null_display.clone().into();
        let cell_colors = CellColors {
            text,
            muted: theme.text_muted,
            num: theme.orange,
            cyan: theme.cyan,
            faint,
        };
        let row_height = self.settings.grid.density.row_height();
        let mono_family = theme.mono_family.clone();
        let cell_size = theme.font_size;
        let show_gutter = self.settings.grid.row_numbers;
        let gutter_w = if show_gutter { GUTTER_WIDTH } else { 0.0 };
        let ncols = grid.columns.len();
        let content_w = gutter_w + ncols as f32 * DATA_COL_WIDTH;
        // The cell of an open editor that targets a draft row.
        let draft_inline: Option<(usize, usize, Entity<TextInput>)> =
            self.grid_edit.as_ref().and_then(|e| match &e.slot {
                EditSlot::Draft { index, data_col } => Some((*index, *data_col, e.input.clone())),
                EditSlot::Row { .. } => None,
            });

        let mut rows = Vec::with_capacity(grid.pending.inserts.len());
        for (index, draft) in grid.pending.inserts.iter().enumerate() {
            let mut cells = Vec::with_capacity(ncols + show_gutter as usize);
            if show_gutter {
                cells.push(
                    div()
                        .w(px(GUTTER_WIDTH))
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
                                .text_color(faint)
                                .hover(|s| s.text_color(accent))
                                .child("✕")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.remove_draft_row(index, cx)
                                })),
                        )
                        .into_any_element(),
                );
            }
            for c in 0..ncols {
                if let Some((di, dc, input)) = &draft_inline {
                    if *di == index && *dc == c {
                        cells.push(
                            div()
                                .w(px(DATA_COL_WIDTH))
                                .flex_shrink_0()
                                .h_full()
                                .px_2p5()
                                .flex()
                                .items_center()
                                .border_r_1()
                                .border_color(line)
                                .child(input.clone())
                                .into_any_element(),
                        );
                        continue;
                    }
                }
                let content = match draft.cells.get(&c) {
                    Some(v) => render_cell(
                        &DisplayCell::from_value(v),
                        cell_colors,
                        &null_display,
                        false,
                    ),
                    None => div()
                        .text_color(faint)
                        .italic()
                        .child("default")
                        .into_any_element(),
                };
                cells.push(
                    div()
                        .id(("draft-cell", index * ncols + c))
                        .w(px(DATA_COL_WIDTH))
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
                    .w(px(content_w))
                    .h(row_height)
                    .border_b_1()
                    .border_color(line)
                    .children(cells),
            );
        }

        Some(
            div()
                .id("draft-rows")
                .flex_shrink_0()
                .max_h(px(f32::from(row_height) * 6.0))
                .overflow_x_scroll()
                .overflow_y_scroll()
                .bg(bg)
                .border_t_1()
                .border_color(border)
                .font_family(mono_family)
                .text_size(cell_size)
                .track_scroll(&grid.h_scroll)
                .child(div().flex().flex_col().w(px(content_w)).children(rows))
                .into_any_element(),
        )
    }

    /// The result cell's right-click context menu — the per-cell actions (Inspect
    /// · Copy) that used to sit in the toolbar, anchored at `pos` (the cursor).
    /// Both act on the cell the right-click just selected. A full-cover backdrop
    /// closes the menu on an outside click.
    fn render_cell_menu(&self, pos: Point<Pixels>, cx: &mut Context<Self>) -> impl IntoElement {
        // Editing entries (Track B6) appear only when the focused cell / row is
        // editable on a writable connection's keyed browse.
        let editable_cell = self.active_edit_target().is_some();
        let editable_browse = self.editing_enabled()
            && matches!(&self.phase, Phase::Connected(a) if a.active_result().is_some_and(|g| g.editable_browse()));
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
            );
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
}
