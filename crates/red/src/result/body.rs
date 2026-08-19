//! The result grid's table body: the columns, the virtual-scroll window and the
//! per-row cell renderer that together make up everything between the toolbar and
//! the footer.
//!
//! Split out of [`render`](super::render) because this is the part that becomes
//! `impl Render for ResultGrid` when the grid becomes a view: it reads the grid
//! and the theme, and every interaction it wires reaches the app through a weak
//! handle rather than a `cx.listener`, so the conversion is a change of receiver
//! rather than a rewrite. Keeping it in its own file means that change stays
//! local. See `docs/plans/todo/zed-architecture-inspiration.md` (Stage C).

use flint::TextInput;
use flint::prelude::*;
use std::rc::Rc;

use gpui::{Entity, Hsla, Pixels, SharedString, div, prelude::*, px};

use super::buffer::CellKind;
use super::edit::EditSlot;
use super::render::{CellColors, group_digits, render_cell};
use super::{ResultGrid, gutter_width};
use crate::app::{ActiveConn, AppState, Pane, PaneId, Phase};
use crate::gridwindow::WindowView;

impl AppState {
    /// Build the result pane's table: the header columns plus the windowed row
    /// renderer. Returns the row height and the resolved scroll window alongside
    /// it, because the scrollbar below the table is positioned from them and
    /// `prepare_window` must run exactly once per frame.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::result) fn render_grid_table(
        &self,
        active: &ActiveConn,
        grid: &ResultGrid,
        tab_idx: usize,
        pane: PaneId,
        cell_colors: CellColors,
        cx: &Context<Self>,
    ) -> (flint::Table<()>, Pixels, WindowView) {
        let is_focused = grid.is_focused;
        let theme = cx.theme();
        let faint = theme.text_faint;
        let (num, cyan, red, accent) = (theme.orange, theme.cyan, theme.red, theme.accent);
        let edit_bg = theme.bg_input;
        let watch_green = theme.green;
        let caret_icon = theme.scale(9.);
        let cell_size = theme.font_size;
        let mono_family = theme.mono_family.clone();
        let view = cx.entity().downgrade();

        // An optional leading row-number gutter, then one fixed-width, sortable
        // column per result column. Each header carries the engine's declared type
        // as a dim subtitle, like the design's typed headers (`email` + `text`).
        // The gutter occupies table column 0 when shown, so a data column's table
        // index is `data + gutter` (see the handlers in `mod.rs`).
        let show_gutter = self.settings.data.row_numbers;
        let gutter = show_gutter as usize;
        let mut columns: Vec<Column> = Vec::with_capacity(grid.columns.len() + gutter);
        if show_gutter {
            columns.push(
                Column::new("#")
                    .width(px(gutter_width(grid.total)))
                    .align_end(),
            );
        }
        // Columns are built from the *display* order, so a hidden column is
        // simply absent and a reordered one moves. Everything the closures below
        // capture is indexed by data column, so each maps back through
        // `data_cols` rather than assuming display position == data column.
        let data_cols: Vec<usize> = grid.visible.clone();
        for &dc in &data_cols {
            let Some(c) = grid.columns.get(dc) else {
                continue;
            };
            let mut col = Column::new(c.name.clone())
                .width(px(grid.width_of(dc)))
                .sortable()
                .resizable();
            if let Some(t) = &c.decl_type
                && !t.is_empty()
            {
                col = col.subtitle(t.to_lowercase());
            }
            columns.push(col);
        }
        // The sort marker sits on whichever slot draws the sorted data column;
        // a sorted-then-hidden column simply shows no marker.
        let sort = grid
            .sort
            .and_then(|(c, asc)| grid.slot_of(c).map(|slot| (slot + gutter, asc)));
        let total = grid.total;
        let buffer_range = grid.buffer.clone();
        let buffer_row = grid.buffer.clone();
        // Forward-FK data columns, snapshotted into the row closure so the
        // paint path stays alloc-free: a membership test, computed off-frame.
        let fk_cols = grid.fk_cols.clone();
        // Inline-expanded reference columns, snapshotted for the cell-bg
        // hook so a faint wash marks them as derived, not base-table, data.
        let joined_cols = grid.joined_cols.clone();
        let joined_tint = Hsla { a: 0.05, ..cyan };
        let bg_cols = data_cols.clone();
        let row_cols = data_cols.clone();
        let sender = grid.sender.clone();
        let epoch = grid.epoch;
        let (sort_view, cell_view, nav_view) = (view.clone(), view.clone(), view.clone());
        let sec_view = view.clone();
        let (drag_start_view, resize_view) = (view.clone(), view.clone());
        let (drag_end_view, auto_fit_view) = (view.clone(), view.clone());
        let header_menu_view = view.clone();

        // Resolve (and possibly re-center) the virtual-scroll window for this
        // frame; everything below works in list-local coordinates offset by
        // `base`, so the list only ever lays out `win.len` rows.
        let row_height = self.settings.data.density.row_height();
        let null_display: SharedString = self.settings.data.null_display.clone().into();
        let win = grid.prepare_window(row_height);
        let base = win.base;
        // Nothing else bounds the horizontal offset once a band is frozen (see
        // `clamp_h_offset`), and the things that can invalidate it — hiding a
        // column, resizing one, resizing the window — all land here as a repaint.
        grid.clamp_h_offset(gutter);
        // The selection is stored in absolute ordinals; translate it into the
        // window's local rows for highlighting (off-window rows just aren't
        // painted). The TSV copy reads the buffer in absolute space, so it stays
        // correct regardless.
        let local_selection = grid.selection.map(|mut r| {
            r.anchor.0 = r.anchor.0.saturating_sub(base);
            r.focus.0 = r.focus.0.saturating_sub(base);
            r
        });

        // Staged-edit overlay: the dirty cells + deleted rows for this
        // frame, shared (via `Rc`) between the cell renderer and the cell-tint hook.
        // Tints: a soft amber under a staged cell, a soft red under a row pending
        // deletion (the selection highlight still wins on top).
        let overlay = Rc::new(grid.pending.overlay());
        let dirty_tint = Hsla { a: 0.22, ..num };
        let delete_tint = Hsla { a: 0.16, ..red };
        let (overlay_cells, overlay_bg) = (overlay.clone(), overlay.clone());
        // Find-in-result highlight : the resident cells matching
        // the open find bar's term get a soft accent tint via the same `cell_bg`
        // hook. The focused match is *also* the grid selection, so the selection
        // highlight marks "current" on top of this. Keyed by `(ordinal, data col)`.
        // Find/edit overlays belong to the focused pane only; the find bar, the
        // inline editor and the stats/draft chrome are single-instance app state.
        let find_hits = grid.find_hits.clone();
        let find_tint = Hsla { a: 0.20, ..accent };
        // Watch-mode change flash: the cells a re-run changed, tinted until their
        // flash window expires. Precomputed here into window-local coordinates so
        // the tint hook stays a set lookup rather than a per-cell key rebuild.
        //
        // Under `reduce_motion` the tint still appears, it just does not fade:
        // motion is the accessibility hazard, colour is the information.
        let watch_hits: std::collections::HashSet<(usize, usize)> = active
            .tabs
            .get(tab_idx)
            .and_then(|t| t.watch.as_ref())
            .map(|w| {
                let now = std::time::Instant::now();
                // Only the *visible* rows (plus a screen of margin so a flash on
                // a row scrolled just off-screen resolves cleanly), not the whole
                // resident window: `win.len` is `total.min(100k)`, so scanning it
                // was ~1M HashMap probes + 200k allocations per frame on a large
                // watched result. The tint hook only ever asks about painted
                // cells, so an off-screen hit would never be read anyway.
                let rh = f32::from(row_height);
                let viewport = grid.viewport_rows(rh).max(1);
                let first = grid.first_visible_row(rh);
                let lo = first.saturating_sub(viewport);
                let hi = first
                    .saturating_add(viewport.saturating_mul(2))
                    .min(win.base.saturating_add(win.len));
                (lo.max(win.base)..hi)
                    .filter_map(|abs| {
                        let key = grid.watch_row_key(abs);
                        let hits: Vec<(usize, usize)> = (0..grid.columns.len())
                            .filter(|&c| w.is_flashing(&key, c, now))
                            .map(|c| (abs, c))
                            .collect();
                        (!hits.is_empty()).then_some(hits)
                    })
                    .flatten()
                    .collect()
            })
            .unwrap_or_default();
        let watch_tint = Hsla {
            a: 0.22,
            ..watch_green
        };
        // The pinned rows' positions, snapshotted for the gutter marker so the
        // paint path is a set lookup rather than a walk of the pins per row.
        let pinned_ords = grid.pinned_ordinals();
        // The row renderer runs outside this method's borrow, so its unpin glyph
        // reaches the app through a handle rather than a listener.
        let unpin_view = view.clone();
        // The open inline editor's target cell (existing rows only; draft rows host
        // their own editor in the bottom zone), so the renderer swaps in its field.
        let inline: Option<(usize, usize, Entity<TextInput>)> = is_focused
            .then_some(grid.grid_edit.as_ref())
            .flatten()
            .and_then(|e| match &e.slot {
                EditSlot::Row { row, data_col, .. } => Some((*row, *data_col, e.input.clone())),
                EditSlot::Draft { .. } => None,
            });
        // The same cell in the *table* coordinates the click handler speaks, so a
        // click that lands inside the open editor can be left to the field (see
        // `on_cell_click`). `None` while no editor is open, or when its column is
        // hidden and it therefore draws nowhere.
        let edit_cell: Option<(usize, usize)> = inline
            .as_ref()
            .and_then(|(row, data_col, _)| Some((*row, grid.slot_of(*data_col)? + gutter)));
        // When the FK picker is open, the editor cell also hosts a
        // bounds-capturing canvas so the dropdown can anchor below it.
        let suggest_anchor: Option<Entity<Option<gpui::Bounds<Pixels>>>> = is_focused
            .then_some(grid.cell_suggest.as_ref())
            .flatten()
            .and_then(|_| grid.cell_suggest_bounds.clone());

        // The focused cell, spoken aloud: the grid reports this as its accessible
        // name (a `Grid` landmark), so a screen reader announces "<column>:
        // <value>, row N of M" each time the cell cursor moves: the one piece of
        // state a blind user needs to read the data. `focus` is in absolute,
        // table-column coordinates (gutter included); the data column behind a
        // display slot comes from the grid's display order. Falls back to the
        // grid's name when there's no cursor.
        let a11y_label: SharedString = grid
            .selection
            .map(|sel| {
                let (row, table_col) = sel.focus;
                let pos = format!("row {} of {}", group_digits(row + 1), group_digits(total));
                if show_gutter && table_col == 0 {
                    return SharedString::from(format!("Row number, {pos}"));
                }
                let Some(data_col) = grid.data_col_at(table_col - gutter) else {
                    return SharedString::from(grid.label.clone());
                };
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
            // Pinned rows ride between the header and the list, inside the
            // table's horizontal track, so they stay column-aligned as the grid
            // scrolls sideways and hold still as it scrolls down. Grid state, not
            // app state, so both split panes show their own.
            .pinned_rows(self.render_pinned_rows(grid, cx))
            // The row-number gutter is frozen whenever it is shown: an ordinal
            // that scrolls away with its row leaves the grid with no fixed
            // reference at all. Pinned columns extend the same band.
            .pinned_columns(gutter + grid.frozen_slots())
            .font_family(mono_family.clone())
            .text_size(cell_size)
            .grid_lines(true)
            .track_scroll(&grid.scroll)
            .track_horizontal_scroll(&grid.h_scroll)
            .horizontal(true)
            // Keyboard cell cursor: the grid's focus handle lives on the table,
            // and arrow/Home/End/Page/⌘-arrow intents drive the selection. Each
            // pane has its own handle so focus never lands on two grids. Absent
            // only on the frame a pane is born (see `PaneUi::grid_focus`), where
            // the grid simply isn't focusable yet.
            .when_some(grid.grid_focus.clone(), |t, handle| t.focus_handle(handle))
            // Vim motions (hjkl/g/G/0/$/Ctrl-d/Ctrl-u) ride alongside the arrow keys
            // when the user has turned vim navigation on.
            .vim_nav(self.vim_mode())
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
                let data_col = table_col
                    .checked_sub(gutter)
                    .and_then(|slot| bg_cols.get(slot).copied())?;
                if overlay_bg.cells.contains_key(&(abs, data_col)) {
                    return Some(dirty_tint);
                }
                if find_hits.contains(&(abs, data_col)) {
                    return Some(find_tint);
                }
                // Below find/edit (which are about what the *user* is doing) and
                // above the joined-column wash (which is static structure).
                if watch_hits.contains(&(abs, data_col)) {
                    return Some(watch_tint);
                }
                // A joined reference column (derived from a referenced table) gets a
                // faint wash: lowest priority, so a find/edit/delete tint wins on top.
                if joined_cols.contains(&data_col) {
                    return Some(joined_tint);
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
            .column_drag(grid.column_drag)
            .on_column_resize_start(move |drag, _window, cx| {
                drag_start_view
                    .update(cx, |this, cx| {
                        if let Phase::Connected(active) = &mut this.phase {
                            active.with_active_result(cx, |grid| grid.column_drag = Some(drag));
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .on_column_resize(move |table_col, width, _window, cx| {
                resize_view
                    .update(cx, |this, cx| {
                        let gutter = this.gutter();
                        // The gutter is not resizable, so a handle only ever
                        // names a data column; the guard is for totality.
                        let Some(slot) = table_col.checked_sub(gutter) else {
                            return;
                        };
                        if let Phase::Connected(active) = &mut this.phase {
                            active.with_active_result(cx, |grid| {
                                if let Some(dc) = grid.data_col_at(slot) {
                                    grid.set_width(dc, f32::from(width));
                                }
                            });
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .on_column_resize_end(move |_window, cx| {
                drag_end_view
                    .update(cx, |this, cx| {
                        if let Phase::Connected(active) = &mut this.phase {
                            active.with_active_result(cx, |grid| grid.column_drag = None);
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .on_column_auto_fit(move |table_col, _window, cx| {
                auto_fit_view
                    .update(cx, |this, cx| {
                        let gutter = this.gutter();
                        let Some(slot) = table_col.checked_sub(gutter) else {
                            return;
                        };
                        if let Phase::Connected(active) = &mut this.phase {
                            active.with_active_result(cx, |grid| {
                                if let Some(dc) = grid.data_col_at(slot) {
                                    grid.auto_fit(dc);
                                }
                            });
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .on_header_secondary(move |table_col, pos, window, cx| {
                header_menu_view
                    .update(cx, |this, cx| {
                        this.set_split_focus(pane, cx);
                        this.focus_pane(Pane::Grid, window, cx);
                        // The gutter's header names no column, so it opens nothing.
                        if let Some(slot) = table_col.checked_sub(gutter) {
                            this.header_menu = Some((pos, slot));
                        }
                        cx.notify();
                    })
                    .ok();
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
                        // Aim subsequent actions at this pane before they resolve.
                        this.set_split_focus(pane, cx);
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
                // A click inside the open inline editor is the *field's*: it places
                // the caret or ends a drag-selection. The cell's own handling would
                // pull focus back to the table, and the commit-on-blur listener
                // would then close the editor the moment the mouse came up, so a
                // mouse selection could never survive its own release.
                if edit_cell == Some((abs_row, table_col)) {
                    return;
                }
                cell_view
                    .update(cx, |this, cx| {
                        // Aim subsequent actions at this pane before they resolve.
                        this.set_split_focus(pane, cx);
                        // Focus the grid so the cell cursor + ⌘C land on this
                        // selection, not a still-focused editor/field.
                        this.focus_pane(Pane::Grid, window, cx);
                        this.result_select(abs_row, table_col, extend, cx);
                        // Double-click edits the cell in place when it's editable
                        //; otherwise it reveals the detail inspector.
                        if inspect {
                            this.begin_grid_edit(cx);
                            if this.grid(cx).is_none_or(|g| g.grid_edit.is_none()) {
                                this.open_inspector(cx);
                            }
                        }
                    })
                    .ok();
            })
            // Right-click selects the cell and opens its context menu (Inspect ·
            // Copy) anchored at the cursor: the per-cell actions that used to live
            // in the toolbar.
            .on_cell_secondary(move |row, table_col, pos, window, cx| {
                let abs_row = base + row;
                sec_view
                    .update(cx, |this, cx| {
                        this.set_split_focus(pane, cx);
                        this.focus_pane(Pane::Grid, window, cx);
                        this.result_select_for_menu(abs_row, table_col, cx);
                        this.cell_menu = Some(pos);
                        cx.notify();
                    })
                    .ok();
            })
            .render_row(move |ix, _, _| {
                // `ix` is list-local; the gutter and buffer are absolute.
                let abs = base + ix;
                let mut out = Vec::with_capacity(row_cols.len() + gutter);
                let buffer = buffer_row.borrow();
                let struck = overlay_cells.deleted.contains(&abs);
                if show_gutter {
                    if struck {
                        // A row staged for deletion trades its ordinal for a clear
                        // deletion marker (the row-through + red tint already read as
                        // "going away"; the gutter glyph names why).
                        out.push(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(red)
                                .child(gpui::svg().path("icons/ban.svg").size(px(13.)).flex_none())
                                .into_any_element(),
                        );
                    } else {
                        // After an interpolated jump the run's ordinals are estimates;
                        // the gutter marks them `≈` until a true end pins them exact.
                        let label = if buffer.is_estimated() {
                            format!("≈{}", group_digits(abs + 1))
                        } else {
                            group_digits(abs + 1)
                        };
                        // A pinned row is also up in the strip, so its ordinal in the
                        // grid carries the same pin glyph: without it the two copies
                        // of the row read as two rows.
                        out.push(if pinned_ords.contains(&abs) {
                            div()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap_1()
                                .child(
                                    // The glyph is also the release: while the row
                                    // is on screen the strip does not carry it, so
                                    // this is the unpin control the user can see.
                                    // It stops the click there, or the cell under
                                    // it would select the row on the way past.
                                    div()
                                        .id(("row-unpin", abs))
                                        .cursor_pointer()
                                        .tooltip(Tooltip::text(crate::i18n::tr!(
                                            "result.unpin_row",
                                            "Unpin row"
                                        )))
                                        .child(
                                            gpui::svg()
                                                .path("icons/pin.svg")
                                                .size(px(11.))
                                                .flex_none()
                                                .text_color(accent),
                                        )
                                        .on_click({
                                            let view = unpin_view.clone();
                                            move |_, _, cx| {
                                                cx.stop_propagation();
                                                view.update(cx, |this, cx| {
                                                    this.unpin_row_at(abs, cx)
                                                })
                                                .ok();
                                            }
                                        }),
                                )
                                .child(div().text_color(faint).child(label))
                                .into_any_element()
                        } else {
                            div().text_color(faint).child(label).into_any_element()
                        });
                    }
                }
                let resident = buffer.row(abs);
                // The display order, not `0..ncols`: the header has one column per
                // *visible* column, and a row that emitted one per *data* column
                // would hand Flint more cells than it has columns to lay them out
                // in. `c` stays a data-column index, which is what the staged-edit
                // overlay, the FK set and the resident row are all keyed by.
                for &c in &row_cols {
                    // The open inline editor takes over its cell. The field is
                    // `bare`, so it fills the cell (the Flint cell wrapper supplies
                    // the height/padding) rather than drawing a smaller box inside.
                    // It carries the input background so it reads as a field, and
                    // so the text selection inside it has an opaque, untinted
                    // surface to contrast against: the cell-cursor wash the cell
                    // paints underneath is close enough in hue to the selection
                    // highlight that the two cancelled each other out.
                    if let Some((er, ec, input)) = &inline
                        && *er == abs
                        && *ec == c
                    {
                        let field = div()
                            .size_full()
                            .flex()
                            .items_center()
                            .bg(edit_bg)
                            .child(input.clone());
                        match &suggest_anchor {
                            Some(anchor) => out.push(
                                field
                                    .relative()
                                    .child(super::suggest::anchor_canvas(anchor.clone()))
                                    .into_any_element(),
                            ),
                            None => out.push(field.into_any_element()),
                        }
                        continue;
                    }
                    let is_fk = fk_cols.contains(&c);
                    // A staged value (dirty cell) shadows the resident one.
                    if let Some(cell) = overlay_cells.cells.get(&(abs, c)) {
                        out.push(render_cell(cell, cell_colors, &null_display, struck, is_fk));
                        continue;
                    }
                    match resident.and_then(|r| r.display.get(c)) {
                        Some(cell) => {
                            out.push(render_cell(cell, cell_colors, &null_display, struck, is_fk))
                        }
                        None => out.push(div().text_color(faint).child("·").into_any_element()),
                    }
                }
                out
            });
        (table, row_height, win)
    }
}
