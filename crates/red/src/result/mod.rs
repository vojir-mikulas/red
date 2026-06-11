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
use red_core::{Column as ResultColumn, ExportFormat, KeySpec, Value};
use red_service::{Command, CommandSender, RunFetch};

use crate::app::{AppState, Phase};

use buffer::{next_epoch, window_decision, BufferMode, GridBuffer, KeyedRun, WindowView, WINDOW};
pub(crate) use render::group_digits;

/// All the state for one open result. The grid columns include a leading row-
/// number gutter, so table-column index `0` is the gutter and data column `n` is
/// table column `n + 1`.
pub(crate) struct ResultGrid {
    pub label: String,
    base_sql: String,
    pub(in crate::result) columns: Vec<ResultColumn>,
    pub(in crate::result) total: usize,
    pub(in crate::result) ready: bool,
    pub(in crate::result) error: Option<String>,
    /// `(data column, ascending)` — `None` is unsorted.
    pub(in crate::result) sort: Option<(usize, bool)>,
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
    ) -> Self {
        Self {
            label,
            base_sql,
            columns: Vec::new(),
            total: 0,
            ready: false,
            error: None,
            sort: None,
            selection: None,
            table,
            buffer: Rc::new(RefCell::new(GridBuffer::default())),
            sender,
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

    /// The SQL to open: the base query, wrapped with `ORDER BY <pos>` when sorted.
    /// Ordering by output position is robust to column-name quoting/aliases.
    fn effective_sql(&self) -> String {
        match self.sort {
            // The derived table needs an alias — MySQL and Postgres both reject an
            // unaliased subquery in `FROM`.
            Some((col, asc)) => format!(
                "SELECT * FROM ({}) AS _red_sort ORDER BY {} {}",
                strip_trailing(&self.base_sql),
                col + 1,
                if asc { "ASC" } else { "DESC" }
            ),
            None => self.base_sql.clone(),
        }
    }

    /// Whether the open query has landed (ready or errored) vs. still running —
    /// drives the shell's live query-timer ticker.
    pub(crate) fn is_ready(&self) -> bool {
        self.ready
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
        let key_col = key.and_then(|k| columns.iter().position(|c| c.name == k.column));
        self.columns = columns;
        self.total = total;
        self.ready = true;
        self.error = None;
        self.stop_timer();
        self.window_base.set(0);
        let mut buffer = self.buffer.borrow_mut();
        *buffer = GridBuffer::default();
        if let Some(key_col) = key_col {
            buffer.mode = BufferMode::Keyed(KeyedRun::new(key_col));
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
        *self.buffer.borrow_mut() = GridBuffer::default();
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

    /// The current selection as TSV (NULL → empty), skipping the gutter column.
    /// Unloaded cells contribute blanks rather than blocking the copy.
    fn selection_tsv(&self) -> Option<String> {
        let (r0, c0, r1, c1) = self.selection?.bounds();
        let ncol = self.columns.len();
        let dc0 = c0.max(1);
        if dc0 > c1 {
            return None;
        }
        let buffer = self.buffer.borrow();
        let mut out = String::new();
        for r in r0..=r1 {
            for (i, tc) in (dc0..=c1).enumerate() {
                let dcol = tc - 1;
                if dcol >= ncol {
                    continue;
                }
                if i > 0 {
                    out.push('\t');
                }
                if let Some(value) = buffer.row(r).and_then(|row| row.values.get(dcol)) {
                    out.push_str(&cell_string(value));
                }
            }
            out.push('\n');
        }
        Some(out)
    }
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

/// Trim trailing whitespace + a single terminator, so the SQL nests as a subquery.
fn strip_trailing(sql: &str) -> &str {
    sql.trim().strip_suffix(';').unwrap_or(sql.trim()).trim()
}

/// A value as a plain TSV/clipboard string (NULL → empty).
fn cell_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
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
        let sender = self.service.command_sender();
        let opened = match &mut self.phase {
            Phase::Connected(active) => {
                let grid = ResultGrid::new(label.into(), base_sql, table, sender);
                let opened = (grid.effective_sql(), grid.epoch, grid.table.clone());
                active.active_mut().result = Some(grid);
                opened
            }
            _ => return,
        };
        let (sql, epoch, table) = opened;
        self.service.send(Command::OpenResult { sql, epoch, table });
        self.start_query_ticker(cx);
        cx.notify();
    }

    /// Backend reported the open result's columns + total (+ resolved seek key).
    pub(crate) fn on_result_ready(
        &mut self,
        columns: Vec<ResultColumn>,
        total: usize,
        epoch: u64,
        key: Option<KeySpec>,
        cx: &mut Context<Self>,
    ) {
        if let Phase::Connected(active) = &mut self.phase {
            // Route by epoch: the result may belong to a background tab. A late
            // reply for a replaced/closed result finds no match and is dropped.
            if let Some(grid) = active.result_by_epoch(epoch) {
                grid.on_ready(columns, total, key);
            }
        }
        cx.notify();
    }

    /// A keyset run fetch failed — free the grid's in-flight slot so scrolling
    /// can fetch again (the error itself arrives separately as a toast).
    pub(crate) fn on_result_run_failed(&mut self, epoch: u64, seq: u64) {
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.result_by_epoch(epoch) {
                grid.buffer.borrow_mut().run_failed(seq);
            }
        }
    }

    /// A keyset run window arrived — extend/relocate the grid's run and repaint.
    pub(crate) fn on_result_run(
        &mut self,
        epoch: u64,
        fetch: RunFetch,
        rows: Vec<Vec<Value>>,
        estimated: bool,
        seq: u64,
        cx: &mut Context<Self>,
    ) {
        if let Phase::Connected(active) = &mut self.phase {
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
        offset: usize,
        rows: Vec<Vec<Value>>,
        epoch: u64,
        cx: &mut Context<Self>,
    ) {
        if let Phase::Connected(active) = &mut self.phase {
            // Route by epoch so a background tab's page lands in its own grid; a
            // page for a superseded result finds no match and is dropped.
            if let Some(grid) = active.result_by_epoch(epoch) {
                grid.buffer.borrow_mut().insert_page(offset, rows);
            }
        }
        cx.notify();
    }

    /// Record a result error against the active tab's grid (also surfaced as a
    /// toast). Errors aren't epoch-tagged yet, so they attach to the focused tab.
    pub(crate) fn on_result_error(&mut self, message: &str) {
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = &mut active.active_mut().result {
                grid.error = Some(message.to_string());
                grid.ready = true;
                grid.stop_timer();
            }
        }
    }

    /// Header click on a data column: toggle / set sort and re-open the result.
    pub(crate) fn result_sort(&mut self, table_col: usize, cx: &mut Context<Self>) {
        if table_col == 0 {
            return; // the row-number gutter isn't sortable
        }
        let dcol = table_col - 1;
        let reopen = match &mut self.phase {
            Phase::Connected(active) => match &mut active.active_mut().result {
                Some(grid) => {
                    let old_epoch = grid.epoch;
                    grid.sort = match grid.sort {
                        Some((c, asc)) if c == dcol => Some((c, !asc)),
                        _ => Some((dcol, true)),
                    };
                    grid.selection = None;
                    grid.ready = false;
                    grid.restart_timer();
                    grid.reset_buffer();
                    // New SQL → new epoch, so pages still in flight for the old
                    // ordering are dropped rather than landing in the wrong rows.
                    grid.epoch = next_epoch();
                    Some((grid.effective_sql(), grid.epoch, old_epoch))
                }
                None => None,
            },
            _ => None,
        };
        if let Some((sql, epoch, old_epoch)) = reopen {
            // Evict the superseded SQL so the backend's result map can't grow.
            self.service.send(Command::CloseResult { epoch: old_epoch });
            // No `table`: sorted SQL isn't ordered by the PK, so it pages by
            // `OFFSET` — composite `(sort_col, pk)` seek isn't implemented.
            self.service.send(Command::OpenResult {
                sql,
                epoch,
                table: None,
            });
            self.start_query_ticker(cx);
        }
        cx.notify();
    }

    /// Cell click: set the selection anchor, or extend it on shift-click.
    pub(crate) fn result_select(
        &mut self,
        row: usize,
        table_col: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        if table_col == 0 {
            return;
        }
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = &mut active.active_mut().result {
                grid.selection = match (extend, grid.selection) {
                    (true, Some(mut range)) => {
                        range.focus = (row, table_col);
                        Some(range)
                    }
                    _ => Some(CellRange::single(row, table_col)),
                };
            }
        }
        cx.notify();
    }

    /// Prompt for a save path, then stream the active tab's result there in `format`.
    pub(crate) fn export_result(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
        let epoch = match &self.phase {
            Phase::Connected(a) => a.active().result.as_ref().map(|g| g.epoch),
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
                this.update(cx, |this, cx| {
                    this.service.send(Command::Export {
                        format,
                        path,
                        epoch,
                    });
                    this.toast = Some(("Exporting…".into(), ToastVariant::Info));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// "Go to row N" from the palette prompt — `one_based` is the row number the
    /// user typed (1-based). Scrolls the active result's grid to that exact row,
    /// clamped to the result's bounds. No-op when no result is open.
    pub(crate) fn go_to_row(&mut self, one_based: usize, cx: &mut Context<Self>) {
        let row_height = f32::from(self.settings.density().row_height());
        if let Phase::Connected(active) = &self.phase {
            if let Some(grid) = active.active().result.as_ref() {
                grid.go_to_row(one_based.saturating_sub(1), row_height);
            }
        }
        cx.notify();
    }

    pub(crate) fn copy_result_selection(&mut self, cx: &mut Context<Self>) {
        let tsv = match &self.phase {
            Phase::Connected(active) => active
                .active()
                .result
                .as_ref()
                .and_then(ResultGrid::selection_tsv),
            _ => None,
        };
        if let Some(tsv) = tsv {
            cx.write_to_clipboard(ClipboardItem::new_string(tsv));
        }
    }
}
