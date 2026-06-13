//! The result grid: a virtualized, horizontally-scrolling table backed by a
//! random-access window buffer. The grid never holds the whole result — its
//! load-on-scroll callback fetches the pages around the viewport and evicts the
//! rest, so memory stays flat over a multi-million-row result. Cell ranges select
//! and copy as TSV; clicking a column header sorts (re-running with `ORDER BY`).
//!
//! Split across three files: [`buffer`] (the windowed paging core), [`render`]
//! (cell rendering + the results-pane view), and this module ([`ResultGrid`]
//! state plus the `AppState` command handlers that drive it).

mod buffer;
mod render;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use flint::prelude::*;
use gpui::{point, px, ClipboardItem, Context, Pixels, ScrollHandle, UniformListScrollHandle};
use red_core::{Column as ResultColumn, ExportFormat, KeySpec, ResultFilter, Value};
use red_service::{Command, CommandSender, RunFetch, SessionId, SortKey};

use crate::app::{AppState, ExportProgress, Notification, Phase};

use buffer::{next_epoch, window_decision, BufferMode, GridBuffer, KeyedRun, WindowView, WINDOW};
pub(crate) use render::group_digits;

/// Mint a fresh, process-unique epoch for a non-grid consumer (the plan view,
/// Track B4) so its echoed replies are dropped once superseded — shares the
/// grid's monotonic source so the two never collide.
pub(crate) fn new_epoch() -> u64 {
    next_epoch()
}

/// All the state for one open result. When the row-number gutter is shown
/// (`grid.row_numbers`) it occupies table column `0`, so data column `n` sits at
/// table column `n + 1`; with the gutter hidden the data columns start at `0`. The
/// offset is [`AppState::gutter`] and selection/copy/sort map through it.
pub(crate) struct ResultGrid {
    pub label: String,
    base_sql: String,
    pub(in crate::result) columns: Vec<ResultColumn>,
    pub(in crate::result) total: usize,
    pub(in crate::result) ready: bool,
    pub(in crate::result) error: Option<String>,
    /// `(data column, ascending)` — `None` is unsorted.
    pub(in crate::result) sort: Option<(usize, bool)>,
    /// The active result filter (Track B2), pushed into the query on (re)open.
    /// `None` is unfiltered. Survives a re-sort (both ride the same `OpenResult`).
    pub(in crate::result) filter: Option<ResultFilter>,
    pub(in crate::result) selection: Option<CellRange>,
    /// The `(schema, table)` this result browses, when it's a plain table
    /// preview — sent with `OpenResult` so the backend can resolve a seek key.
    /// `None` for editor SQL and for sorted re-opens (which wrap the SQL).
    table: Option<(String, String)>,
    pub(in crate::result) buffer: Rc<RefCell<GridBuffer>>,
    pub(in crate::result) sender: CommandSender,
    pub(in crate::result) scroll: UniformListScrollHandle,
    pub(in crate::result) h_scroll: ScrollHandle,
    /// The overlay scrollbar's in-flight drag.
    pub(in crate::result) scrollbar: ScrollbarState,
    /// Virtual-scroll window: the absolute ordinal that list-local index 0 maps
    /// to. `Rc` so the scrollbar's scrub closure can move it; `Cell` because
    /// `Table`/`uniform_list` are stateless across frames, so the base lives
    /// here. See `WINDOW` and `prepare_window`.
    pub(in crate::result) window_base: Rc<Cell<usize>>,
    /// Rows per fetched page (the `grid.page_size` in effect when this result was
    /// opened) — used to (re)build the buffer in either paging mode. A live result
    /// keeps the page it was opened with; a settings change applies to the next open.
    page_size: usize,
    /// Identifies the current open SQL; bumped on every (re)open so stale page
    /// fetches and late `ResultReady`/`ResultPageLoaded` replies are ignored.
    pub(crate) epoch: u64,
    /// When the current query was issued — drives the live "running…" timer.
    query_started: Instant,
    /// Frozen wall-clock time the query took, set once it lands (ready or error).
    /// `None` while still running, so the elapsed time keeps counting up.
    query_elapsed: Option<Duration>,
}

impl ResultGrid {
    pub fn new(
        label: String,
        base_sql: String,
        table: Option<(String, String)>,
        sender: CommandSender,
        page_size: usize,
    ) -> Self {
        Self {
            label,
            base_sql,
            columns: Vec::new(),
            total: 0,
            ready: false,
            error: None,
            sort: None,
            filter: None,
            selection: None,
            table,
            buffer: Rc::new(RefCell::new(GridBuffer::new(page_size))),
            sender,
            page_size,
            scroll: UniformListScrollHandle::new(),
            h_scroll: ScrollHandle::new(),
            scrollbar: ScrollbarState::new(),
            window_base: Rc::new(Cell::new(0)),
            epoch: next_epoch(),
            query_started: Instant::now(),
            query_elapsed: None,
        }
    }

    /// Wall-clock time the query has taken: frozen once it lands, otherwise the
    /// live elapsed time since it was issued (the running counter).
    pub(in crate::result) fn query_time(&self) -> Duration {
        self.query_elapsed
            .unwrap_or_else(|| self.query_started.elapsed())
    }

    /// Restart the timer for a re-run (re-sort) of the same grid.
    fn restart_timer(&mut self) {
        self.query_started = Instant::now();
        self.query_elapsed = None;
    }

    /// Freeze the elapsed time — the query has landed (ready or error).
    fn stop_timer(&mut self) {
        if self.query_elapsed.is_none() {
            self.query_elapsed = Some(self.query_started.elapsed());
        }
    }

    /// Whether the open query has landed (ready or errored) vs. still running —
    /// drives the shell's live query-timer ticker.
    pub(crate) fn is_ready(&self) -> bool {
        self.ready
    }

    /// Drop any cell selection — used when the gutter offset changes under it (the
    /// selection is stored in table-column coordinates, see `AppState::set_row_numbers`).
    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// The `(absolute row, data column)` of the cell under the keyboard cursor —
    /// the selection's focus, mapped through the gutter and clamped to the data
    /// columns. `None` when nothing is selected or the result has no columns. The
    /// detail inspector resolves the focused cell through this.
    pub(crate) fn cursor_cell(&self, gutter: usize) -> Option<(usize, usize)> {
        let focus = self.selection?.focus;
        let ncols = self.columns.len();
        (ncols > 0).then(|| (focus.0, focus.1.saturating_sub(gutter).min(ncols - 1)))
    }

    /// A data column's `(name, declared type)` — for the inspector header.
    pub(crate) fn column_meta(&self, col: usize) -> Option<(String, Option<String>)> {
        self.columns
            .get(col)
            .map(|c| (c.name.clone(), c.decl_type.clone()))
    }

    /// The resident value at `(row, col)`, cloned. `None` when the row is off the
    /// resident window (evicted) or the column is out of range. A whole resident
    /// cell is bounded by the driver's display cap, so this clone is cheap; a
    /// `Value::Capped` comes back as itself so the caller can tell it's partial.
    pub(crate) fn cell_value(&self, row: usize, col: usize) -> Option<Value> {
        self.buffer
            .borrow()
            .row(row)
            .and_then(|r| r.values.get(col).cloned())
    }

    /// Total rows in the open result (0 until `ResultReady`) — for the
    /// go-to-row prompt's range hint and bound.
    pub(crate) fn total_rows(&self) -> usize {
        self.total
    }

    /// `(rows, columns)` once the result is ready — for the shell's status bar.
    pub fn status_counts(&self) -> Option<(usize, usize)> {
        self.ready
            .then_some((self.total, self.columns.len()))
            .filter(|_| self.error.is_none())
    }

    /// Install the open result's metadata and reset the buffer into the right
    /// mode: keyed when the backend resolved a seek key that names a result
    /// column, offset otherwise.
    fn on_ready(&mut self, columns: Vec<ResultColumn>, total: usize, key: Option<KeySpec>) {
        // Keyed only when every key column (lead, then tiebreaker) is present in
        // the result — a `SELECT *` table browse always satisfies this.
        let key_cols = key.as_ref().and_then(|k| {
            let cols: Vec<usize> = k
                .column_names()
                .iter()
                .filter_map(|name| columns.iter().position(|c| &c.name == name))
                .collect();
            (cols.len() == k.column_names().len()).then_some(cols)
        });
        self.columns = columns;
        self.total = total;
        self.ready = true;
        self.error = None;
        self.stop_timer();
        self.window_base.set(0);
        let page = self.page_size;
        let mut buffer = self.buffer.borrow_mut();
        *buffer = GridBuffer::new(page);
        if let Some(key_cols) = key_cols {
            buffer.mode = BufferMode::Keyed(KeyedRun::new(key_cols, page));
        }
    }

    /// The current virtual-scroll window. Recenters it on the viewport when the
    /// scroll nears a window edge (compensating the list's pixel offset so the
    /// visible rows hold still), and returns the base + length to feed the list
    /// plus the scrollbar's fraction/thumb. Call once per render, *before*
    /// building the `Table`, so `row_count`, `window_base`, and the list's pixel
    /// offset all agree within the frame.
    pub(in crate::result) fn prepare_window(&self, row_height: Pixels) -> WindowView {
        let total = self.total;
        let rh = f32::from(row_height).max(1.0);

        let (offset_x, offset_y, viewport_h) = {
            let st = self.scroll.0.borrow();
            let off = st.base_handle.offset();
            let vh = st
                .last_item_size
                .map(|s| f32::from(s.item.height))
                .unwrap_or(0.0);
            (off.x, f32::from(off.y), vh)
        };
        let viewport_rows = (viewport_h / rh).ceil() as usize;

        let len = total.min(WINDOW);
        // The viewport's top row, in list-local then absolute coordinates.
        let local_first = (-offset_y / rh).round().max(0.0) as usize;
        let abs_first = self.window_base.get().min(total.saturating_sub(len)) + local_first;

        let (base, reanchor) =
            window_decision(total, self.window_base.get(), local_first, viewport_rows);
        if let Some(new_local_first) = reanchor {
            // The window slid; shift the list's pixel offset by the same amount
            // so the rows on screen don't move — the user only ever sees one
            // continuous scroll.
            let st = self.scroll.0.borrow();
            st.base_handle
                .set_offset(point(offset_x, px(-(new_local_first as f32 * rh))));
        }
        self.window_base.set(base);

        // Scrollbar position is absolute (fraction of the whole result), not of
        // the window — so the thumb reflects where we are in all 50M rows.
        let denom = total.saturating_sub(viewport_rows).max(1) as f32;
        let fraction = (abs_first as f32 / denom).clamp(0.0, 1.0);
        let thumb = if total > 0 {
            (viewport_rows as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            1.0
        };

        WindowView {
            base,
            len,
            fraction,
            thumb,
        }
    }

    fn reset_buffer(&mut self) {
        *self.buffer.borrow_mut() = GridBuffer::new(self.page_size);
        self.window_base.set(0);
    }

    /// Jump the grid to `ordinal` (0-based) — the explicit "go to row N". Places
    /// the virtual-scroll window so the row sits at the viewport top, then, for a
    /// keyed result, forces an **exact** relocation (keyset auto-jumps would only
    /// interpolate). An offset result needs no special fetch: positioning alone
    /// makes the next paint request the exact `OFFSET` page at `ordinal`.
    pub(in crate::result) fn go_to_row(&self, ordinal: usize, row_height: f32) {
        let target = ordinal.min(self.total.saturating_sub(1));
        place_window(
            &self.window_base,
            &self.scroll,
            self.total,
            target,
            row_height,
        );
        if let BufferMode::Keyed(run) = &mut self.buffer.borrow_mut().mode {
            run.jump_exact(target, self.epoch, &self.sender);
        }
    }

    /// How many whole rows the on-screen viewport shows — the PageUp/PageDown
    /// step. Reads the list's last measured viewport height (0 before first paint).
    pub(in crate::result) fn viewport_rows(&self, row_height: f32) -> usize {
        let rh = row_height.max(1.0);
        let st = self.scroll.0.borrow();
        let vh = st
            .last_item_size
            .map(|s| f32::from(s.item.height))
            .unwrap_or(0.0);
        (vh / rh).floor() as usize
    }

    /// The absolute ordinal of the first row currently visible at the viewport
    /// top — where a fresh keyboard cursor starts when nothing is selected yet.
    pub(in crate::result) fn first_visible_row(&self, row_height: f32) -> usize {
        let rh = row_height.max(1.0);
        let st = self.scroll.0.borrow();
        let off_y = f32::from(st.base_handle.offset().y);
        let local_first = (-off_y / rh).round().max(0.0) as usize;
        (self.window_base.get() + local_first).min(self.total.saturating_sub(1))
    }

    /// Keep the keyboard cursor on screen after it moves to absolute ordinal
    /// `abs_row`. Two regimes: if the row left the resident buffer window, reuse
    /// the proven `go_to_row` jump (recenter + keyed exact relocation); if it's
    /// still in the window but scrolled out of the viewport, nudge the list's
    /// pixel offset by the minimum so the row sits at the near edge — no full
    /// recenter, so a one-row step never restyles the whole window.
    pub(in crate::result) fn scroll_cursor_into_view(&self, abs_row: usize, row_height: f32) {
        if self.total == 0 {
            return;
        }
        let rh = row_height.max(1.0);
        let len = self.total.min(WINDOW);
        let base = self.window_base.get();
        if abs_row < base || abs_row >= base + len {
            // Off the resident window — recenter (and, when keyed, fetch exactly).
            self.go_to_row(abs_row, rh);
            return;
        }
        let st = self.scroll.0.borrow();
        let off = st.base_handle.offset();
        let vh = st
            .last_item_size
            .map(|s| f32::from(s.item.height))
            .unwrap_or(0.0);
        let viewport_rows = (vh / rh).floor().max(1.0) as usize;
        let local = abs_row - base;
        let local_first = (-f32::from(off.y) / rh).round().max(0.0) as usize;
        let new_first = if local < local_first {
            local
        } else if local >= local_first + viewport_rows {
            local + 1 - viewport_rows
        } else {
            return; // already visible — leave the scroll untouched
        };
        st.base_handle
            .set_offset(point(off.x, px(-(new_first as f32 * rh))));
    }

    /// A glance at this grid's footprint for the dev perf HUD: resident rows,
    /// paging mode, in-flight fetches, and the last query's wall-clock time.
    #[cfg(feature = "dev-stats")]
    pub(crate) fn dev_snapshot(&self) -> crate::dev_stats::GridSnapshot {
        let buffer = self.buffer.borrow();
        crate::dev_stats::GridSnapshot {
            resident_rows: buffer.resident_rows(),
            mode: buffer.mode_label(),
            in_flight: buffer.in_flight(),
            last_query_ms: self.query_time().as_secs_f32() * 1000.0,
        }
    }

    /// How to satisfy a copy of the current selection (NULL → empty, gutter
    /// column skipped). When every selected cell is resident and untruncated the
    /// clipboard text is built straight from the buffer ([`CopyPlan::Ready`]);
    /// when the selection touches a cell the grid clipped for display, or reaches
    /// rows scrolled out of the window (a whole-column select), those rows must be
    /// re-fetched in full first ([`CopyPlan::Refetch`]). `None` when the selection
    /// is empty or covers only the gutter. `gutter` is the data-column table offset
    /// (`1` with the row-number gutter shown, else `0`).
    fn copy_plan(&self, gutter: usize) -> Option<CopyPlan> {
        let (r0, c0, r1, c1) = self.selection?.bounds();
        let ncol = self.columns.len();
        let dc0 = c0.max(gutter);
        if dc0 > c1 {
            return None;
        }
        let dcol_lo = dc0 - gutter;
        let dcol_hi = (c1 - gutter).min(ncol.saturating_sub(1));
        let buffer = self.buffer.borrow();
        // Any selected row that's off-window (not resident) or holds a clipped
        // display stand-in forces a full re-fetch; otherwise the buffer already
        // has the real values. `any` short-circuits at the first such row, so a
        // whole-column select doesn't scan the entire result here.
        let needs_full = (r0..=r1).any(|r| match buffer.row(r) {
            None => true,
            Some(row) => (dcol_lo..=dcol_hi).any(|c| row.is_truncated(c)),
        });
        if needs_full {
            return Some(CopyPlan::Refetch {
                epoch: self.epoch,
                offset: r0,
                limit: r1 - r0 + 1,
                dcol_lo,
                dcol_hi,
            });
        }
        let mut out = String::new();
        for r in r0..=r1 {
            for (i, dcol) in (dcol_lo..=dcol_hi).enumerate() {
                if i > 0 {
                    out.push('\t');
                }
                if let Some(value) = buffer.row(r).and_then(|row| row.values.get(dcol)) {
                    out.push_str(&cell_string(value));
                }
            }
            out.push('\n');
        }
        Some(CopyPlan::Ready(out))
    }
}

/// A copy awaiting its full-row re-fetch (see [`CopyPlan::Refetch`]). Holds the
/// selected data-column span so the [`Event::CopyRowsLoaded`] reply can be turned
/// into the clipboard text.
///
/// [`Event::CopyRowsLoaded`]: red_service::Event::CopyRowsLoaded
pub(crate) struct PendingCopy {
    pub(crate) id: u64,
    pub(crate) dcol_lo: usize,
    pub(crate) dcol_hi: usize,
}

/// How [`ResultGrid::copy_plan`] resolves a selection copy.
pub(crate) enum CopyPlan {
    /// Ready to copy now — the assembled TSV.
    Ready(String),
    /// The selection holds display-clipped cells; re-fetch the rows in full
    /// (`CopyRows`) and assemble the clipboard text from the reply. `dcol_lo..=dcol_hi`
    /// are the selected data columns (the re-fetched rows carry every column).
    Refetch {
        epoch: u64,
        offset: usize,
        limit: usize,
        dcol_lo: usize,
        dcol_hi: usize,
    },
}

/// Assemble TSV from freshly re-fetched rows over data columns `dcol_lo..=dcol_hi`
/// (NULL → empty) — the [`CopyPlan::Refetch`] counterpart to the buffer path.
pub(crate) fn rows_tsv(rows: &[Vec<Value>], dcol_lo: usize, dcol_hi: usize) -> String {
    let mut out = String::new();
    for row in rows {
        for (i, dcol) in (dcol_lo..=dcol_hi).enumerate() {
            if i > 0 {
                out.push('\t');
            }
            if let Some(value) = row.get(dcol) {
                out.push_str(&cell_string(value));
            }
        }
        out.push('\n');
    }
    out
}

/// Position the virtual-scroll window so absolute ordinal `target` sits at the
/// viewport top: re-center the window on it and set the list's pixel offset
/// directly (no `scroll_to_item`, which degenerates on a multi-million-row f32
/// canvas). Shared by the scrollbar scrub and the explicit "go to row" jump.
pub(in crate::result) fn place_window(
    window_base: &Cell<usize>,
    scroll: &UniformListScrollHandle,
    total: usize,
    target: usize,
    row_height: f32,
) {
    let base = if total > WINDOW {
        target.saturating_sub(WINDOW / 2).min(total - WINDOW)
    } else {
        0
    };
    window_base.set(base);
    let local = target - base;
    let st = scroll.0.borrow();
    let x = st.base_handle.offset().x;
    st.base_handle
        .set_offset(point(x, px(-(local as f32 * row_height))));
}

/// A value as a plain TSV/clipboard string (NULL → empty).
fn cell_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
        // A capped blob copies as its summary; a capped text would re-fetch full
        // before reaching here (`copy_plan`), so its head is only a defensive form.
        Value::Capped(c) if c.blob => format!("<{} bytes>", c.len),
        Value::Capped(c) => format!("{}…", c.head),
    }
}

impl AppState {
    /// Open `base_sql` as the grid's result (preview or editor run). Resets sort +
    /// selection and asks the backend for the row count + columns. `table` names
    /// the browsed `(schema, table)` for a plain preview, so the backend can
    /// resolve a keyset seek key; editor runs pass `None`.
    pub(crate) fn open_result(
        &mut self,
        label: impl Into<String>,
        base_sql: String,
        table: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let opened = match &mut self.phase {
            Phase::Connected(active) if active.active().is_some() => {
                // Bind the grid's load-on-scroll sender to this workspace's session.
                let sender = self.service.command_sender(active.session);
                let grid = ResultGrid::new(
                    label.into(),
                    base_sql,
                    table,
                    sender,
                    self.settings.grid.page_size,
                );
                let opened = (grid.base_sql.clone(), grid.epoch, grid.table.clone());
                // Safe: the guard above ensured a focused tab exists. A fresh run
                // replaces any open plan (Track B4) with its grid.
                let tab = active.active_mut().unwrap();
                tab.result = Some(grid);
                tab.plan = None;
                opened
            }
            _ => return,
        };
        let (sql, epoch, table) = opened;
        // A fresh open is never sorted or filtered — the backend keys it from `table`.
        self.send_active(Command::OpenResult {
            sql,
            epoch,
            table,
            sort: None,
            filter: None,
        });
        self.start_query_ticker(cx);
        cx.notify();
    }

    /// Backend reported the open result's columns + total (+ resolved seek key).
    pub(crate) fn on_result_ready(
        &mut self,
        session: Option<SessionId>,
        columns: Vec<ResultColumn>,
        total: usize,
        epoch: u64,
        key: Option<KeySpec>,
        cx: &mut Context<Self>,
    ) {
        // Route to the event's session (it may be a backgrounded workspace), then
        // by epoch within it. A late reply for a closed result finds no match.
        if let Some(active) = self.conn_mut(session) {
            if let Some(grid) = active.result_by_epoch(epoch) {
                grid.on_ready(columns, total, key);
            }
        }
        cx.notify();
    }

    /// A keyset run fetch failed — free the grid's in-flight slot so scrolling
    /// can fetch again (the error itself arrives separately as a toast).
    pub(crate) fn on_result_run_failed(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        seq: u64,
    ) {
        if let Some(active) = self.conn_mut(session) {
            if let Some(grid) = active.result_by_epoch(epoch) {
                grid.buffer.borrow_mut().run_failed(seq);
            }
        }
    }

    /// A keyset run window arrived — extend/relocate the grid's run and repaint.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn on_result_run(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        fetch: RunFetch,
        rows: Vec<Vec<Value>>,
        estimated: bool,
        seq: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.conn_mut(session) {
            if let Some(grid) = active.result_by_epoch(epoch) {
                let total = grid.total;
                grid.buffer
                    .borrow_mut()
                    .apply_run(fetch, rows, estimated, seq, total);
            }
        }
        cx.notify();
    }

    /// A page arrived — drop it into the buffer and repaint.
    pub(crate) fn on_result_page(
        &mut self,
        session: Option<SessionId>,
        offset: usize,
        rows: Vec<Vec<Value>>,
        epoch: u64,
        cx: &mut Context<Self>,
    ) {
        // Route by session then epoch so a background tab's page lands in its own
        // grid; a page for a superseded result finds no match and is dropped.
        if let Some(active) = self.conn_mut(session) {
            if let Some(grid) = active.result_by_epoch(epoch) {
                grid.buffer.borrow_mut().insert_page(offset, rows);
            }
        }
        cx.notify();
    }

    /// Record a result error against the session's focused tab grid (also surfaced
    /// as a toast). Errors aren't epoch-tagged, so they attach to the focused tab.
    pub(crate) fn on_result_error(&mut self, session: Option<SessionId>, message: &str) {
        if let Some(active) = self.conn_mut(session) {
            if let Some(grid) = active.active_result_mut() {
                grid.error = Some(message.to_string());
                grid.ready = true;
                grid.stop_timer();
            }
        }
    }

    /// Table-column index of the first *data* column: `1` when the row-number
    /// gutter occupies column 0, else `0`. A data column `d` sits at table column
    /// `d + gutter`; selection/copy/sort all map through this offset.
    pub(crate) fn gutter(&self) -> usize {
        self.settings.grid.row_numbers as usize
    }

    /// Header click on a data column: toggle / set sort and re-open the result.
    pub(crate) fn result_sort(&mut self, table_col: usize, cx: &mut Context<Self>) {
        let gutter = self.gutter();
        if table_col < gutter {
            return; // the row-number gutter isn't sortable
        }
        let dcol = table_col - gutter;
        let reopen = match &mut self.phase {
            Phase::Connected(active) => match active.active_result_mut() {
                Some(grid) => {
                    let old_epoch = grid.epoch;
                    let asc = match grid.sort {
                        Some((c, asc)) if c == dcol => !asc,
                        _ => true,
                    };
                    grid.sort = Some((dcol, asc));
                    grid.selection = None;
                    grid.ready = false;
                    grid.restart_timer();
                    grid.reset_buffer();
                    // New SQL → new epoch, so pages still in flight for the old
                    // ordering are dropped rather than landing in the wrong rows.
                    grid.epoch = next_epoch();
                    // Carry the table ref + sort down so the backend resolves the
                    // composite `(sort_col, pk)` keyset key (or wraps for OFFSET).
                    let sort = SortKey {
                        position: dcol + 1,
                        column: grid.columns[dcol].name.clone(),
                        descending: !asc,
                    };
                    Some((
                        grid.base_sql.clone(),
                        grid.table.clone(),
                        sort,
                        grid.filter.clone(),
                        grid.epoch,
                        old_epoch,
                    ))
                }
                None => None,
            },
            _ => None,
        };
        if let Some((sql, table, sort, filter, epoch, old_epoch)) = reopen {
            // Evict the superseded SQL so the backend's result map can't grow.
            self.send_active(Command::CloseResult { epoch: old_epoch });
            self.send_active(Command::OpenResult {
                sql,
                epoch,
                table,
                sort: Some(sort),
                filter,
            });
            self.start_query_ticker(cx);
        }
        cx.notify();
    }

    /// Apply (or clear) the result filter (Track B2): re-open the active grid with
    /// `filter` pushed into the query, preserving the current header-click sort. A
    /// new epoch drops pages still in flight for the prior (un)filtered ordering.
    /// `None` clears the filter. A no-op when the filter is unchanged.
    pub(crate) fn apply_result_filter(
        &mut self,
        filter: Option<ResultFilter>,
        cx: &mut Context<Self>,
    ) {
        let reopen = match &mut self.phase {
            Phase::Connected(active) => match active.active_result_mut() {
                Some(grid) if grid.filter != filter => {
                    let old_epoch = grid.epoch;
                    grid.filter = filter;
                    grid.selection = None;
                    grid.ready = false;
                    grid.restart_timer();
                    grid.reset_buffer();
                    grid.epoch = next_epoch();
                    // Preserve the header-click sort across the re-open.
                    let sort = grid.sort.map(|(dcol, asc)| SortKey {
                        position: dcol + 1,
                        column: grid.columns[dcol].name.clone(),
                        descending: !asc,
                    });
                    Some((
                        grid.base_sql.clone(),
                        grid.table.clone(),
                        sort,
                        grid.filter.clone(),
                        grid.epoch,
                        old_epoch,
                    ))
                }
                _ => None,
            },
            _ => None,
        };
        if let Some((sql, table, sort, filter, epoch, old_epoch)) = reopen {
            self.send_active(Command::CloseResult { epoch: old_epoch });
            self.send_active(Command::OpenResult {
                sql,
                epoch,
                table,
                sort,
                filter,
            });
            self.start_query_ticker(cx);
        }
        cx.notify();
    }

    /// The active result's current filter, for the toolbar chip / filter-bar seed.
    pub(crate) fn active_result_filter(&self) -> Option<ResultFilter> {
        match &self.phase {
            Phase::Connected(active) => active.active_result().and_then(|g| g.filter.clone()),
            _ => None,
        }
    }

    /// Cell click: set the selection anchor, or extend it on shift-click. A click
    /// in the row-number gutter (table column `0`) selects the whole row (every
    /// data column); shift-click there extends the block across rows.
    pub(crate) fn result_select(
        &mut self,
        row: usize,
        table_col: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let gutter = self.gutter();
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                let ncols = grid.columns.len();
                grid.selection = if gutter == 1 && table_col == 0 {
                    // Gutter click: span every data column (table cols
                    // `gutter..=ncols`); an empty result has no columns to select.
                    (ncols > 0).then(|| match (extend, grid.selection) {
                        (true, Some(mut range)) => {
                            range.focus = (row, ncols);
                            range
                        }
                        _ => CellRange {
                            anchor: (row, 1),
                            focus: (row, ncols),
                        },
                    })
                } else {
                    Some(match (extend, grid.selection) {
                        (true, Some(mut range)) => {
                            range.focus = (row, table_col);
                            range
                        }
                        _ => CellRange::single(row, table_col),
                    })
                };
            }
        }
        cx.notify();
    }

    /// Move the keyboard cell cursor over the active grid (arrows, Home/End,
    /// PageUp/Down, ⌘arrows). `extend` (Shift held) grows the selection from its
    /// anchor; otherwise the cursor becomes a fresh single-cell selection. The
    /// cursor lives in absolute ordinals while the list is windowed, so it then
    /// re-centers the window to follow (see [`ResultGrid::scroll_cursor_into_view`]).
    /// No-op until the result is ready and has columns.
    pub(crate) fn result_cursor_move(
        &mut self,
        mv: TableNav,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let row_height = f32::from(self.settings.grid.density.row_height());
        let gutter = self.gutter();
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                if !grid.ready || grid.error.is_some() || grid.columns.is_empty() {
                    return;
                }
                let ncols = grid.columns.len();
                let last_row = grid.total.saturating_sub(1);
                let page = grid.viewport_rows(row_height).max(1);
                // Data columns occupy table indices `gutter..=ncols-1+gutter`.
                let (first_col, last_col) = (gutter, ncols + gutter - 1);
                // The cursor is the selection's focus; with nothing selected yet
                // it starts at the first visible row's first data column.
                let (row, col) = match grid.selection {
                    Some(r) => r.focus,
                    None => (grid.first_visible_row(row_height), first_col),
                };
                let col = col.clamp(first_col, last_col);
                let (new_row, new_col) = match mv {
                    TableNav::Up => (row.saturating_sub(1), col),
                    TableNav::Down => ((row + 1).min(last_row), col),
                    TableNav::Left => (row, (col - 1).max(first_col)),
                    TableNav::Right => (row, (col + 1).min(last_col)),
                    TableNav::RowStart => (row, first_col),
                    TableNav::RowEnd => (row, last_col),
                    TableNav::PageUp => (row.saturating_sub(page), col),
                    TableNav::PageDown => ((row + page).min(last_row), col),
                    TableNav::First => (0, col),
                    TableNav::Last => (last_row, col),
                };
                grid.selection = Some(match (extend, grid.selection) {
                    (true, Some(mut range)) => {
                        range.focus = (new_row, new_col);
                        range
                    }
                    _ => CellRange::single(new_row, new_col),
                });
                grid.scroll_cursor_into_view(new_row, row_height);
            }
        }
        cx.notify();
    }

    /// ⌘/Ctrl-click on a header: select that whole data column (every row). With
    /// `extend` (⌘/Ctrl+Shift-click), grow the existing selection to span every
    /// column between its anchor and this one — full-height, so it reads as a
    /// multi-column block. The selection spans the full result, so copying it
    /// re-fetches the off-window rows in full (see [`ResultGrid::copy_plan`]). The
    /// gutter isn't selectable.
    pub(crate) fn result_select_column(
        &mut self,
        table_col: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let gutter = self.gutter();
        if table_col < gutter {
            return;
        }
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                let last = grid.total.saturating_sub(1);
                grid.selection = match (extend, grid.selection) {
                    // Keep the anchor column, pull the focus to this one, and force
                    // full height so the block stays a clean column span.
                    (true, Some(mut range)) => {
                        range.anchor = (0, range.anchor.1.max(gutter));
                        range.focus = (last, table_col);
                        Some(range)
                    }
                    _ => Some(CellRange {
                        anchor: (0, table_col),
                        focus: (last, table_col),
                    }),
                };
            }
        }
        cx.notify();
    }

    /// Prompt for a save path, then stream the active tab's result there in `format`.
    pub(crate) fn export_result(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
        let epoch = match &self.phase {
            Phase::Connected(a) => a.active_result().map(|g| g.epoch),
            _ => None,
        };
        let Some(epoch) = epoch else {
            return;
        };
        let name = match format {
            ExportFormat::Csv => "red-export.csv",
            ExportFormat::Json => "red-export.json",
        };
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some(name));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update(cx, |this, cx| this.start_export(format, path, epoch, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Begin an export once the save path is chosen: allocate its id, fire the
    /// command off to the backend, and stand up the persistent progress toast
    /// (its `✕` is a Cancel — see [`AppState::close_notification`]). The total is
    /// the open result's row count, already known from `ResultReady`.
    fn start_export(
        &mut self,
        format: ExportFormat,
        path: PathBuf,
        epoch: u64,
        cx: &mut Context<Self>,
    ) {
        let total = match &self.phase {
            Phase::Connected(a) => a.active_result().map(|g| g.total_rows()).unwrap_or(0),
            _ => 0,
        };
        let id = self.next_export_id;
        self.next_export_id += 1;
        self.send_active(Command::Export {
            format,
            path,
            epoch,
            id,
        });
        self.push_notification(
            Notification {
                id: 0,
                variant: ToastVariant::Info,
                message: "Exporting… 0%".into(),
                auto_dismiss: None,
                export: Some(ExportProgress { id, rows: 0, total }),
            },
            cx,
        );
    }

    // --- export progress events ---

    /// The notification id of the export toast carrying `export_id`, if it's still
    /// on screen.
    fn export_notification_id(&self, export_id: u64) -> Option<u64> {
        self.notifications
            .iter()
            .find(|n| n.export.as_ref().is_some_and(|e| e.id == export_id))
            .map(|n| n.id)
    }

    /// `ExportProgress`: advance the export toast's row count + percentage.
    pub(crate) fn on_export_progress(&mut self, id: u64, rows: usize, cx: &mut Context<Self>) {
        if let Some(n) = self
            .notifications
            .iter_mut()
            .find(|n| n.export.as_ref().is_some_and(|e| e.id == id))
        {
            if let Some(export) = &mut n.export {
                export.rows = rows;
                let pct = rows
                    .saturating_mul(100)
                    .checked_div(export.total)
                    .unwrap_or(0)
                    .min(100);
                n.message = format!("Exporting… {pct}%").into();
            }
        }
        cx.notify();
    }

    /// `ExportFinished`: drop the progress toast, leave an auto-dismissing success.
    pub(crate) fn on_export_finished(
        &mut self,
        id: u64,
        path: String,
        rows: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(nid) = self.export_notification_id(id) {
            self.dismiss(nid, cx);
        }
        self.notify(
            ToastVariant::Success,
            format!("Exported {rows} row(s) to {path}"),
            cx,
        );
    }

    /// `ExportCancelled`: drop the progress toast, leave an auto-dismissing notice.
    pub(crate) fn on_export_cancelled(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(nid) = self.export_notification_id(id) {
            self.dismiss(nid, cx);
        }
        self.notify(ToastVariant::Info, "Export cancelled", cx);
    }

    /// "Go to row N" from the palette prompt — `one_based` is the row number the
    /// user typed (1-based). Scrolls the active result's grid to that exact row,
    /// clamped to the result's bounds. No-op when no result is open.
    pub(crate) fn go_to_row(&mut self, one_based: usize, cx: &mut Context<Self>) {
        let row_height = f32::from(self.settings.grid.density.row_height());
        if let Phase::Connected(active) = &self.phase {
            if let Some(grid) = active.active_result() {
                grid.go_to_row(one_based.saturating_sub(1), row_height);
            }
        }
        cx.notify();
    }

    pub(crate) fn copy_result_selection(&mut self, cx: &mut Context<Self>) {
        let gutter = self.gutter();
        let plan = match &self.phase {
            Phase::Connected(active) => active.active_result().and_then(|g| g.copy_plan(gutter)),
            _ => None,
        };
        match plan {
            // Everything selected is resident in full — copy straight away.
            Some(CopyPlan::Ready(tsv)) => {
                cx.write_to_clipboard(ClipboardItem::new_string(tsv));
            }
            // The selection touches display-clipped text; re-fetch the rows in
            // full, then `on_copy_rows` assembles the clipboard from the reply.
            Some(CopyPlan::Refetch {
                epoch,
                offset,
                limit,
                dcol_lo,
                dcol_hi,
            }) => {
                let id = self.next_copy_id;
                self.next_copy_id += 1;
                self.pending_copy = Some(PendingCopy {
                    id,
                    dcol_lo,
                    dcol_hi,
                });
                self.send_active(Command::CopyRows {
                    offset,
                    limit,
                    epoch,
                    id,
                });
            }
            None => {}
        }
    }

    /// A `CopyRows` reply landed: if it's the copy still pending, assemble the
    /// untruncated selection and put it on the clipboard. A superseded reply (the
    /// user copied again before this returned) finds a stale id and is dropped.
    pub(crate) fn on_copy_rows(&mut self, id: u64, rows: Vec<Vec<Value>>, cx: &mut Context<Self>) {
        // The detail inspector draws full values from the same `CopyRows` path; if
        // this reply is its in-flight fetch, it claims it (and never reaches the
        // clipboard). Ids come from one counter, so the two never collide.
        if self.on_inspect_rows(id, &rows) {
            cx.notify();
            return;
        }
        let Some(pending) = self.pending_copy.take_if(|p| p.id == id) else {
            return;
        };
        let tsv = rows_tsv(&rows, pending.dcol_lo, pending.dcol_hi);
        cx.write_to_clipboard(ClipboardItem::new_string(tsv));
    }
}
