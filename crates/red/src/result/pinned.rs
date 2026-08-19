//! Pinned rows: hold a row under the header while the grid scrolls away from it.
//!
//! A pin is *display* state, like a column width. It never re-runs the query and
//! never changes what the result holds: the row keeps its place in the list, and
//! the strip is a second view of it.
//!
//! Two properties make that work over a windowed buffer:
//!
//! - **A pin owns a snapshot of its cells.** The buffer keeps a window and evicts
//!   everything outside it, which is precisely when a pin earns its keep, so the
//!   strip draws from the snapshot rather than from the buffer. The snapshot is
//!   the already-classified [`DisplayCell`]s, so capturing one is an `Arc` bump
//!   per cell rather than a copy of the row's values.
//! - **A pin is identified by its key, not by its position.** Ordinals move under
//!   a re-sort, a filter change or a watch tick; the seek key does not. Identity
//!   columns come from [`ResultGrid::watch_key_cols`], so a pin and a watch agree
//!   on what "the same row" means. Without a usable key (editor SQL) the only
//!   identity available is the position, which cannot survive a re-open, and such
//!   a pin is dropped there rather than silently following a different row.

use std::collections::HashSet;
use std::rc::Rc;

use flint::prelude::*;
use gpui::{AnyElement, App, Context, Hsla, SharedString, div, prelude::*, px};
use red_core::Value;

use crate::app::{AppState, Phase};

use super::buffer::{DisplayCell, Row};
use super::render::{CellColors, render_cell};
use super::{DATA_COL_WIDTH, ResultGrid, gutter_width};

/// How many rows can be pinned at once.
///
/// The strip sits above the results and never scrolls, so an uncapped pin list
/// would push the grid off its own pane. The ceiling matches the draft zone's
/// visible rows, which is the same trade in the other direction.
pub(in crate::result) const MAX_PINNED_ROWS: usize = 6;

/// One pinned row: how to recognise it, where it was last seen, and what to draw.
pub(in crate::result) struct PinnedRow {
    /// The row's identity: its seek-key column values, or `None` when the result
    /// has no usable key and the pin is identified by position alone.
    key: Option<Vec<Value>>,
    /// Where the row was last seen. `None` while its position is unknown: it
    /// drifted (a re-sort or a re-run moved it) and has not been found again. The
    /// strip still draws the pin; only the affordances that need an address (the
    /// gutter marker, click-to-select) go quiet until it is re-homed.
    ordinal: Option<usize>,
    /// Render-ready cells captured while the row was resident, indexed by *data*
    /// column like the buffer's own rows.
    display: Vec<DisplayCell>,
}

impl PinnedRow {
    /// Whether `row` is this pin's row, compared over the identity columns.
    ///
    /// Compares values rather than a formatted key so that re-homing a drifted
    /// pin (which walks the whole resident window) allocates nothing per row. A
    /// key-less pin matches whatever sits at its ordinal, which is exactly what
    /// "identified by position" means.
    fn matches(&self, row: &Row, key_cols: Option<&[usize]>) -> bool {
        let (Some(key), Some(cols)) = (self.key.as_ref(), key_cols) else {
            return self.key.is_none();
        };
        cols.len() == key.len()
            && cols
                .iter()
                .zip(key)
                .all(|(&c, want)| row.values.get(c) == Some(want))
    }
}

/// Lay `cells` into `row`, holding the first `frozen` of them against the left
/// edge and letting the rest ride a `scroll_w`-wide track shifted by `offset_x`.
///
/// The mirror of what the table does to its own rows when columns are frozen
/// (Flint's `Table::pinned_columns`), applied here because the strip's rows reach
/// the table as finished elements it cannot take apart. `frozen == 0` is the
/// ordinary case: the row is its cells, and the table's own track scrolls them.
pub(in crate::result) fn split_row<E: ParentElement>(
    row: E,
    cells: Vec<AnyElement>,
    frozen: usize,
    scroll_w: f32,
    offset_x: gpui::Pixels,
) -> E {
    if frozen == 0 {
        return row.children(cells);
    }
    let mut cells = cells;
    let rest = cells.split_off(frozen.min(cells.len()));
    row.child(
        div()
            .flex()
            .items_center()
            .h_full()
            .flex_shrink_0()
            .children(cells),
    )
    .child(
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left(offset_x)
                    .h_full()
                    .w(px(scroll_w))
                    .flex()
                    .items_center()
                    .children(rest),
            ),
    )
}

/// What a pin request did, so the caller can explain a refusal.
pub(in crate::result) enum PinOutcome {
    Changed,
    /// The row is not in the buffer's window, so there is nothing to snapshot.
    /// Only reachable from a keyboard cursor left on an evicted row.
    NotResident,
    /// Already holding [`MAX_PINNED_ROWS`].
    AtCapacity,
}

impl ResultGrid {
    pub(in crate::result) fn pinned_len(&self) -> usize {
        self.pinned_rows.len()
    }

    /// Whether the row at `abs` is pinned, by last-known position.
    pub(in crate::result) fn is_pinned(&self, abs: usize) -> bool {
        self.pinned_rows.iter().any(|p| p.ordinal == Some(abs))
    }

    /// The pinned rows the strip should draw: the ones whose row is not on
    /// screen, in result order.
    ///
    /// A pin whose position is unknown (adrift after a re-sort) always shows: not
    /// knowing where a row is is not the same as knowing it is visible.
    /// Each carries its index in [`pinned_rows`](Self::pinned_rows), which is
    /// what the strip's unpin control acts on.
    pub(in crate::result) fn offscreen_pins(&self, row_height: f32) -> Vec<(usize, &PinnedRow)> {
        let first = self.first_visible_row(row_height);
        // Inclusive of the partly-drawn last row: it is on screen, and the count
        // shrinks by one as the strip grows, so treating it as visible is also the
        // choice that cannot oscillate frame to frame.
        let last = first + self.viewport_rows(row_height);
        self.pinned_rows
            .iter()
            .enumerate()
            .filter(|(_, p)| match p.ordinal {
                Some(ord) => ord < first || ord > last,
                None => true,
            })
            .collect()
    }

    /// The pinned rows' last-known positions, for the grid's gutter marker.
    ///
    /// By position rather than by identity because the marker is only ever drawn
    /// for a row that is on screen, whose ordinal is therefore current; resolving
    /// identity per painted row would cost a comparison per pin per frame.
    pub(in crate::result) fn pinned_ordinals(&self) -> HashSet<usize> {
        self.pinned_rows.iter().filter_map(|p| p.ordinal).collect()
    }

    /// Pin the row at `abs`, or unpin it when it is already pinned.
    pub(in crate::result) fn toggle_pin(&mut self, abs: usize) -> PinOutcome {
        if let Some(ix) = self.pinned_rows.iter().position(|p| p.ordinal == Some(abs)) {
            self.pinned_rows.remove(ix);
            return PinOutcome::Changed;
        }
        if self.pinned_rows.len() >= MAX_PINNED_ROWS {
            return PinOutcome::AtCapacity;
        }
        let key_cols = self.watch_key_cols();
        // Clone the handle, not the field: the buffer is borrowed for the read
        // while `pinned_rows` is written, and both live on `self`.
        let buffer = Rc::clone(&self.buffer);
        let buffer = buffer.borrow();
        let Some(row) = buffer.row(abs) else {
            return PinOutcome::NotResident;
        };
        let key = key_cols.map(|cols| {
            cols.iter()
                .map(|&c| row.values.get(c).cloned().unwrap_or(Value::Null))
                .collect()
        });
        self.pinned_rows.push(PinnedRow {
            key,
            ordinal: Some(abs),
            display: row.display.clone(),
        });
        self.sort_pins();
        PinOutcome::Changed
    }

    pub(in crate::result) fn unpin_all(&mut self) {
        self.pinned_rows.clear();
    }

    /// Keep the strip in the result's own order, so pinning row 90 after row 5
    /// does not read as a stack. Pins whose position is unknown sink to the
    /// bottom rather than jumping around as they are re-homed.
    fn sort_pins(&mut self) {
        self.pinned_rows
            .sort_by_key(|p| p.ordinal.unwrap_or(usize::MAX));
    }

    /// Re-read the pinned rows from the buffer: refresh the cells of every pin
    /// still sitting where it was, and re-home the ones that moved.
    ///
    /// Called when rows land (a page, a keyset run), so a pinned row shows the
    /// same values as the grid rather than the values it had when it was pinned.
    /// A pin that is merely off-window keeps its snapshot *and* its ordinal: the
    /// row has not moved, it is only out of the buffer.
    pub(in crate::result) fn refresh_pins(&mut self) {
        if self.pinned_rows.is_empty() {
            return;
        }
        let key_cols = self.watch_key_cols();
        let cols = key_cols.as_deref();
        let buffer = Rc::clone(&self.buffer);
        let buffer = buffer.borrow();
        for pin in &mut self.pinned_rows {
            let Some(row) = pin.ordinal.and_then(|o| buffer.row(o)) else {
                continue;
            };
            if pin.matches(row, cols) {
                pin.display = row.display.clone();
            } else {
                // Something else is at that ordinal now, so the pin has no
                // trustworthy address until the scan below finds it again.
                pin.ordinal = None;
            }
        }
        // Re-homing walks the resident window (a few pages, not the result), and
        // only while a pin is actually adrift. A key-less pin has no identity to
        // search by, so it stays where it is and is dropped on the next re-open.
        if cols.is_none() || self.pinned_rows.iter().all(|p| p.ordinal.is_some()) {
            return;
        }
        let pins = &mut self.pinned_rows;
        buffer.for_each_resident(|ord, row| {
            for pin in pins.iter_mut() {
                if pin.ordinal.is_none() && pin.matches(row, cols) {
                    pin.ordinal = Some(ord);
                    pin.display = row.display.clone();
                }
            }
        });
        drop(buffer);
        self.sort_pins();
    }

    /// Carry the pins across a (re)open: a keyed pin survives a re-sort, a filter
    /// change or a watch tick and is re-homed once its row lands; a key-less one
    /// is only a position, and positions do not survive. A different column set
    /// invalidates every snapshot, so those pins go too.
    pub(in crate::result) fn carry_pins(&mut self, shape_changed: bool) {
        if shape_changed {
            self.pinned_rows.clear();
            return;
        }
        self.pinned_rows.retain(|p| p.key.is_some());
        self.forget_pin_positions();
    }

    /// Mark every pin's position unknown, leaving its snapshot alone: the rows
    /// are about to be re-fetched and may land anywhere, so until each is seen
    /// again there is no honest ordinal to claim.
    pub(in crate::result) fn forget_pin_positions(&mut self) {
        for pin in &mut self.pinned_rows {
            pin.ordinal = None;
        }
    }
}

impl AppState {
    /// Pin or unpin the selected rows (⌥⌘P, and the cell menu's Pin row).
    ///
    /// Bounded by [`MAX_PINNED_ROWS`] rows from the top of the selection rather
    /// than walking it: a whole-column selection spans the entire result, and no
    /// more than the cap can be pinned anyway.
    pub(crate) fn toggle_pin_rows(&mut self, cx: &mut Context<Self>) {
        let mut outcome = None;
        if let Phase::Connected(active) = &mut self.phase {
            outcome = active.with_active_result(cx, |grid| {
                let sel = grid.selection?;
                let (r0, _, r1, _) = sel.bounds();
                let last = r1.min(r0 + MAX_PINNED_ROWS - 1);
                let mut result = None;
                for row in r0..=last {
                    match grid.toggle_pin(row) {
                        // A row of the selection that is off-window is skipped
                        // rather than reported: the rest of the selection is
                        // still pinnable, and the cursor's own row is always
                        // resident.
                        PinOutcome::NotResident => continue,
                        outcome => {
                            let stop = matches!(outcome, PinOutcome::AtCapacity);
                            result = Some(outcome);
                            if stop {
                                break;
                            }
                        }
                    }
                }
                result
            });
        }
        if let Some(Some(PinOutcome::AtCapacity)) = outcome {
            // Transient rather than sticky: the refusal is a limit, not a failure,
            // and the strip in front of the user already shows why.
            self.notify(
                ToastVariant::Info,
                crate::i18n::tr!(
                    "result.pin_limit",
                    "Six pinned rows is the maximum; unpin one to pin another"
                ),
                cx,
            );
        }
        cx.notify();
    }

    /// Whether the row the cell cursor sits on is pinned, so the menu can name
    /// what the click will do.
    pub(in crate::result) fn cursor_row_pinned(&self, cx: &App) -> bool {
        let Phase::Connected(active) = &self.phase else {
            return false;
        };
        active.active_result().is_some_and(|grid| {
            let grid = grid.read(cx);
            grid.selection
                .is_some_and(|sel| grid.is_pinned(sel.bounds().0))
        })
    }

    /// How many rows are pinned on the active result, for the menu that offers
    /// to drop them.
    pub(in crate::result) fn pinned_row_count(&self, cx: &App) -> usize {
        match &self.phase {
            Phase::Connected(active) => active
                .active_result()
                .map(|g| g.read(cx).pinned_len())
                .unwrap_or(0),
            _ => 0,
        }
    }

    /// Drop every pin on the active result (the More menu's "Unpin all rows").
    pub(crate) fn unpin_all_rows(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.with_active_result(cx, |grid| grid.unpin_all());
        }
        cx.notify();
    }

    /// Unpin the row at absolute ordinal `abs` (the grid gutter's pin glyph).
    pub(in crate::result) fn unpin_row_at(&mut self, abs: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.with_active_result(cx, |grid| {
                if let Some(ix) = grid.pinned_rows.iter().position(|p| p.ordinal == Some(abs)) {
                    grid.pinned_rows.remove(ix);
                }
            });
        }
        cx.notify();
    }

    /// Unpin the strip's `index`-th row (its gutter control).
    fn unpin_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.with_active_result(cx, |grid| {
                if index < grid.pinned_rows.len() {
                    grid.pinned_rows.remove(index);
                }
            });
        }
        cx.notify();
    }
}

impl ResultGrid {
    /// The pinned-row strip: one element per pinned row that is currently *off
    /// screen*, handed to this grid's `Table` to draw between the header and the
    /// scrolling rows.
    ///
    /// Only off-screen rows, because a pin is a promise that the row stays
    /// reachable, not that it is drawn twice: a row you can already see needs no
    /// copy above it, and showing one reads as two rows with the same ordinal.
    /// While a pinned row is on screen the gutter's pin glyph is the whole story;
    /// scroll it away and it reappears here.
    ///
    /// Laid out against the same display order and per-column widths as the grid,
    /// so the cells sit under the header cells they belong to; the table's
    /// horizontal track carries the strip sideways for free.
    pub(in crate::result) fn render_pinned_rows(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let grid = self;
        if grid.pinned_rows.is_empty() {
            return Vec::new();
        }
        let showing = grid.offscreen_pins(f32::from(
            crate::settings::Settings::global(cx)
                .data
                .density
                .row_height(),
        ));
        if showing.is_empty() {
            return Vec::new();
        }
        let theme = cx.theme();
        let (faint, line, border, bg) = (
            theme.text_faint,
            theme.border_soft,
            theme.border,
            theme.bg_panel,
        );
        let cell_colors = CellColors {
            text: theme.text,
            muted: theme.text_muted,
            num: theme.orange,
            cyan: theme.cyan,
            faint,
            accent: theme.accent,
        };
        let accent = theme.accent;
        // The grid's staged-edit tints, so a pinned row wears the same marks.
        let dirty_tint = Hsla {
            a: 0.22,
            ..theme.orange
        };
        let delete_tint = Hsla {
            a: 0.16,
            ..theme.red
        };
        let null_display: SharedString = crate::settings::Settings::global(cx)
            .data
            .null_display
            .clone()
            .into();
        let row_height = crate::settings::Settings::global(cx)
            .data
            .density
            .row_height();
        let show_gutter = crate::settings::Settings::global(cx).data.row_numbers;
        let gutter = show_gutter as usize;
        let gutter_px = gutter_width(grid.total);
        let icon_size = theme.scale(12.);
        // Display order and per-data-column widths, matching the grid above: a
        // strip laid out per data column would shear away from its columns the
        // moment one was hidden or moved.
        let pin_cols: Vec<usize> = grid.visible.clone();
        let widths: Vec<f32> = (0..grid.columns.len()).map(|c| grid.width_of(c)).collect();
        let last = showing.len() - 1;
        // With a band frozen the table hands its pinned rows no scrolling track
        // (they are opaque elements to it), so the strip carries the same split
        // itself: the leading cells stay, the rest ride a clipped box shifted by
        // the shared horizontal offset. `0` leaves the row whole, which is what
        // the table's own track then scrolls.
        let frozen = gutter + grid.frozen_slots();
        let scroll_w: f32 = pin_cols
            .iter()
            .skip(grid.frozen_slots())
            .map(|&c| grid.width_of(c))
            .sum();
        let offset_x = grid.h_scroll.offset().x;
        // The staged change-set, so a pinned row shows what the grid shows: an
        // edited cell reads with its new value and a row marked for deletion reads
        // struck through. A strip drawing the row as it was on disk while the grid
        // above drew it as it will be is the one thing a pin must not do. Keyed by
        // absolute ordinal, so an adrift pin (no known position) keeps its snapshot.
        let staged = grid.pending.overlay();

        let mut rows = Vec::with_capacity(showing.len());
        for (position, (index, pin)) in showing.iter().copied().enumerate() {
            let struck = pin.ordinal.is_some_and(|ord| staged.deleted.contains(&ord));
            let mut cells: Vec<AnyElement> = Vec::with_capacity(pin_cols.len() + gutter);
            if show_gutter {
                cells.push(
                    div()
                        .id(("pin-unpin", index))
                        .w(px(gutter_px))
                        .flex_shrink_0()
                        .h_full()
                        .px_2p5()
                        .flex()
                        .items_center()
                        .justify_end()
                        .gap_1()
                        .border_r_1()
                        .border_color(line)
                        .cursor_pointer()
                        .tooltip(Tooltip::text(crate::i18n::tr!(
                            "result.unpin_row",
                            "Unpin row"
                        )))
                        .child(
                            // gpui's `svg()` paints only when the svg element's own
                            // `text_color` is set, so the colour lives on the icon.
                            gpui::svg()
                                .path("icons/pin.svg")
                                .size(icon_size)
                                .flex_none()
                                .text_color(accent),
                        )
                        // The ordinal a pinned row *had*: while it is adrift (a
                        // re-sort moved it and it has not been seen since) there is
                        // no honest number to show.
                        .child(div().text_color(faint).child(match pin.ordinal {
                            Some(ord) => super::group_digits(ord + 1),
                            None => "·".to_string(),
                        }))
                        .on_click({
                            let view = grid.app.clone();
                            move |_, _, cx: &mut App| {
                                if let Some(view) = &view {
                                    view.update(cx, |this, cx| this.unpin_row(index, cx)).ok();
                                }
                            }
                        })
                        .into_any_element(),
                );
            }
            for &c in &pin_cols {
                // A staged value shadows the snapshot, exactly as it shadows the
                // resident row in the grid.
                let cell = pin
                    .ordinal
                    .and_then(|ord| staged.cells.get(&(ord, c)))
                    .or_else(|| pin.display.get(c));
                let dirty = pin
                    .ordinal
                    .is_some_and(|ord| staged.cells.contains_key(&(ord, c)));
                let content = match cell {
                    Some(cell) => render_cell(
                        cell,
                        cell_colors,
                        &null_display,
                        struck,
                        grid.fk_cols.contains(&c),
                    ),
                    None => div().text_color(faint).child("·").into_any_element(),
                };
                // Clicking a pinned cell selects it in the grid *without* scrolling
                // there: the pin exists so the row can be read from where you are.
                // A pin that has drifted carries no row to select, so it is inert.
                let target = pin
                    .ordinal
                    .zip(grid.slot_of(c))
                    .map(|(ord, slot)| (ord, slot + gutter));
                cells.push(
                    div()
                        .id(("pin-cell", index * grid.columns.len().max(1) + c))
                        .w(px(widths.get(c).copied().unwrap_or(DATA_COL_WIDTH)))
                        .flex_shrink_0()
                        .h_full()
                        .px_2p5()
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .border_r_1()
                        .border_color(line)
                        // The same amber wash the grid paints under a staged cell.
                        .when(dirty, |d| d.bg(dirty_tint))
                        .child(content)
                        .when_some(target, |d, (ord, table_col)| {
                            d.cursor_pointer().on_click({
                                let view = grid.app.clone();
                                move |_, _, cx: &mut App| {
                                    if let Some(view) = &view {
                                        view.update(cx, |this, cx| {
                                            this.result_select(ord, table_col, false, cx);
                                        })
                                        .ok();
                                    }
                                }
                            })
                        })
                        .into_any_element(),
                );
            }
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .h(row_height)
                    .flex_shrink_0()
                    .bg(bg)
                    // A row staged for deletion carries the grid's red wash on top
                    // of the strip's own panel background.
                    .when(struck, |d| d.bg(delete_tint))
                    .border_b_1()
                    // The last pinned row carries the stronger line, so the strip
                    // reads as one block held above the scrolling rows rather than
                    // as the grid's first few rows.
                    .border_color(if position == last { border } else { line })
                    .map(|row| split_row(row, cells, frozen, scroll_w, offset_x))
                    .into_any_element(),
            );
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::buffer::GridBuffer;
    use red_core::{Column, KeyKind, KeySpec};
    use red_service::{SessionId, spawn};

    /// A two-column result (`id`, `name`), keyed on `id` unless `keyed` is false
    /// (the editor-SQL case), holding `rows` from ordinal 0.
    fn grid(keyed: bool, rows: Vec<Vec<Value>>) -> ResultGrid {
        let handle = spawn();
        let sender = handle.command_sender(SessionId::new(1));
        let mut grid = ResultGrid::new(
            "channel".into(),
            "SELECT * FROM channel".into(),
            Some(("main".into(), "channel".into())),
            sender,
            100,
        );
        grid.columns = vec![
            Column {
                name: "id".into(),
                decl_type: Some("integer".into()),
            },
            Column {
                name: "name".into(),
                decl_type: Some("text".into()),
            },
        ];
        grid.sync_columns();
        if keyed {
            grid.key = Some(KeySpec::single("id", KeyKind::Int));
        }
        grid.total = rows.len();
        grid.buffer.borrow_mut().insert_page(0, rows);
        grid
    }

    fn row(id: i64, name: &str) -> Vec<Value> {
        vec![Value::Integer(id), Value::Text(name.into())]
    }

    #[test]
    fn a_pinned_row_outlives_its_eviction() {
        let mut g = grid(true, vec![row(10, "Gold"), row(11, "Silver")]);
        assert!(matches!(g.toggle_pin(0), PinOutcome::Changed));
        // The window moved on and took the row with it.
        *g.buffer.borrow_mut() = GridBuffer::new(100);
        g.refresh_pins();
        let pin = &g.pinned_rows[0];
        assert_eq!(pin.ordinal, Some(0), "an evicted row has not moved");
        assert_eq!(pin.display[1].text.as_ref(), "Gold");
    }

    #[test]
    fn a_pin_follows_its_row_across_a_re_sort() {
        let mut g = grid(true, vec![row(10, "Gold"), row(11, "Silver")]);
        g.toggle_pin(0);
        // A re-sort: same columns, new order, ordinals no longer meaningful.
        g.carry_pins(false);
        assert_eq!(g.pinned_rows[0].ordinal, None);
        *g.buffer.borrow_mut() = GridBuffer::new(100);
        g.buffer.borrow_mut().insert_page(
            0,
            vec![row(11, "Silver"), row(12, "Bronze"), row(10, "Gold")],
        );
        g.refresh_pins();
        assert_eq!(g.pinned_rows[0].ordinal, Some(2));
        assert_eq!(g.pinned_rows[0].display[1].text.as_ref(), "Gold");
    }

    #[test]
    fn a_pinned_row_shows_the_values_it_now_has() {
        let mut g = grid(true, vec![row(10, "Gold")]);
        g.toggle_pin(0);
        *g.buffer.borrow_mut() = GridBuffer::new(100);
        g.buffer
            .borrow_mut()
            .insert_page(0, vec![row(10, "Bronze")]);
        g.refresh_pins();
        assert_eq!(g.pinned_rows[0].display[1].text.as_ref(), "Bronze");
    }

    #[test]
    fn a_row_at_the_pinned_ordinal_is_not_the_pinned_row() {
        let mut g = grid(true, vec![row(10, "Gold")]);
        g.toggle_pin(0);
        // A different row took the position, and this one is nowhere resident.
        *g.buffer.borrow_mut() = GridBuffer::new(100);
        g.buffer.borrow_mut().insert_page(0, vec![row(99, "Other")]);
        g.refresh_pins();
        assert_eq!(
            g.pinned_rows[0].ordinal, None,
            "the pin is adrift, not moved"
        );
        assert_eq!(g.pinned_rows[0].display[1].text.as_ref(), "Gold");
    }

    #[test]
    fn a_key_less_pin_does_not_survive_a_re_open() {
        let mut g = grid(false, vec![row(10, "Gold")]);
        g.toggle_pin(0);
        assert_eq!(g.pinned_len(), 1);
        g.carry_pins(false);
        assert_eq!(g.pinned_len(), 0, "a position is not an identity");
    }

    #[test]
    fn a_new_column_set_drops_every_pin() {
        let mut g = grid(true, vec![row(10, "Gold")]);
        g.toggle_pin(0);
        g.carry_pins(true);
        assert_eq!(g.pinned_len(), 0);
    }

    #[test]
    fn pinning_stops_at_the_cap() {
        let rows: Vec<Vec<Value>> = (0..MAX_PINNED_ROWS + 1)
            .map(|i| row(i as i64, "row"))
            .collect();
        let mut g = grid(true, rows);
        for r in 0..MAX_PINNED_ROWS {
            assert!(matches!(g.toggle_pin(r), PinOutcome::Changed));
        }
        assert!(matches!(
            g.toggle_pin(MAX_PINNED_ROWS),
            PinOutcome::AtCapacity
        ));
        assert_eq!(g.pinned_len(), MAX_PINNED_ROWS);
    }

    /// A pinned row that is on screen is not drawn in the strip: the gutter's
    /// pin glyph is the whole story, and a second copy of a row you can already
    /// see reads as two rows with the same ordinal.
    #[test]
    fn the_strip_holds_only_the_rows_that_are_off_screen() {
        let mut g = grid(true, (0..40).map(|i| row(i, "r")).collect());
        g.toggle_pin(0);
        g.toggle_pin(30);
        // Nothing has been painted, so the viewport is empty and only row 0 counts
        // as on screen (`first_visible_row` clamps into the result).
        let showing: Vec<usize> = g
            .offscreen_pins(24.)
            .iter()
            .filter_map(|(_, p)| p.ordinal)
            .collect();
        assert_eq!(showing, vec![30]);
    }

    /// An adrift pin (position unknown after a re-sort) still shows: not knowing
    /// where a row is is not the same as knowing it is visible.
    #[test]
    fn an_adrift_pin_stays_in_the_strip() {
        let mut g = grid(true, vec![row(10, "Gold")]);
        g.toggle_pin(0);
        g.carry_pins(false);
        assert_eq!(g.offscreen_pins(24.).len(), 1);
    }

    #[test]
    fn pinning_a_pinned_row_unpins_it() {
        let mut g = grid(true, vec![row(10, "Gold")]);
        g.toggle_pin(0);
        g.toggle_pin(0);
        assert_eq!(g.pinned_len(), 0);
    }

    #[test]
    fn an_off_window_row_cannot_be_pinned() {
        let mut g = grid(true, vec![row(10, "Gold")]);
        assert!(matches!(g.toggle_pin(500), PinOutcome::NotResident));
        assert_eq!(g.pinned_len(), 0);
    }

    #[test]
    fn the_strip_reads_in_result_order() {
        let mut g = grid(true, vec![row(10, "a"), row(11, "b"), row(12, "c")]);
        g.toggle_pin(2);
        g.toggle_pin(0);
        let ords: Vec<Option<usize>> = g.pinned_rows.iter().map(|p| p.ordinal).collect();
        assert_eq!(ords, vec![Some(0), Some(2)]);
    }
}
