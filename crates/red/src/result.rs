//! The result grid: a virtualized, horizontally-scrolling table backed by a
//! random-access window buffer. The grid never holds the whole result — its
//! load-on-scroll callback fetches the pages around the viewport and evicts the
//! rest, so memory stays flat over a multi-million-row result. Cell ranges select
//! and copy as TSV; clicking a column header sorts (re-running with `ORDER BY`).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use std::path::PathBuf;

use flint::prelude::*;
use gpui::{
    div, point, prelude::*, px, ClipboardItem, Context, Hsla, Pixels, ScrollHandle,
    UniformListScrollHandle,
};
use red_core::{Column as ResultColumn, ExportFormat, KeySpec, Value};
use red_service::{Command, CommandSender, RunFetch};

use crate::app::{ActiveConn, AppState, Phase};
use crate::assets::FONT_MONO;

/// Rows per fetched page, and how far beyond the viewport to keep resident before
/// evicting. The buffer holds at most ~`2*MARGIN` rows regardless of total.
const PAGE: usize = 200;
const MARGIN: usize = 400;

/// Skip fetching while the viewport is moving faster than this many rows per
/// paint — a flung scrollbar across a multi-million-row result would otherwise
/// spawn a deep-`OFFSET` page query every frame at a different offset, none of
/// which the user ever dwells on. Fetching resumes once the scroll slows to near
/// its resting position. A deliberate drag moves far fewer rows/frame than this.
const FLING_ROWS: usize = 3 * PAGE;

/// In keyed mode: a viewport landing this far beyond the resident run abandons
/// run-extension (which would chain seeks across the gap) and jumps — one
/// key-space interpolated seek that replaces the run.
const JUMP_GAP: usize = 2 * PAGE;

/// Physical rows the list (`uniform_list`) is laid out over at once. GPUI places
/// each row at `index * row_height` in `f32`, which is only exact up to 2^24
/// (~16.7M) px; past that, positions quantize — rows overlap, double up, and the
/// wheel sticks. So the list never spans the whole result: it lays out at most
/// `WINDOW` rows (a `WINDOW * row_height` canvas, well under the ceiling), and
/// `window_base` slides that window across a result of any size (tens of
/// millions of rows). The fraction-mapped scrollbar drives long jumps; wheel
/// scrolling re-centers the window near its edges (see `prepare_window`).
const WINDOW: usize = 100_000;

/// When the viewport scrolls within this many rows of a window edge — and more
/// result exists beyond that edge — the window re-centers on the viewport,
/// compensating the list's pixel offset so the visible rows don't move.
const REANCHOR_MARGIN: usize = 5_000;

/// Monotonic id for each opened result. Tags `OpenResult`/`FetchPage` so the
/// backend can drop stale page fetches and the grid can ignore late replies for
/// a result it has already replaced (table switch / re-sort). Starts at 1 — `0`
/// is the backend's "no live result" sentinel.
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

fn next_epoch() -> u64 {
    NEXT_EPOCH.fetch_add(1, Ordering::Relaxed)
}

/// The row buffer behind the grid. Two modes, chosen per open result:
///
/// - **Offset** (no seek key — editor SQL, sorted re-opens): a sparse map of
///   `(offset, limit)` pages. Deep pages are O(offset).
/// - **Keyed** (a table browse with a resolved [`KeySpec`]): one contiguous
///   *run* of rows extended from its boundary keys by indexed seeks — O(page)
///   at any depth — and relocated by key-space jumps for far scrolls.
///
/// Either way the buffer holds at most ~`2*MARGIN` rows; everything beyond the
/// viewport margin is evicted each paint.
#[derive(Default)]
struct GridBuffer {
    mode: BufferMode,
    /// The previous paint's first visible row, to gauge scroll velocity (see
    /// `FLING_ROWS`). `None` until the first paint.
    last_start: Option<usize>,
}

enum BufferMode {
    Offset(OffsetPages),
    Keyed(KeyedRun),
}

impl Default for BufferMode {
    fn default() -> Self {
        BufferMode::Offset(OffsetPages::default())
    }
}

/// Offset mode: absolute row index → cells, plus the set of in-flight page
/// requests (so the same page isn't fetched twice).
#[derive(Default)]
struct OffsetPages {
    rows: HashMap<usize, Vec<Value>>,
    requested: HashSet<usize>,
}

/// Keyed mode: one contiguous run of rows, `anchor..anchor + rows.len()` in
/// ordinal space. Extension is run-relative — a forward fetch appends rows
/// strictly after the run's last key, a backward fetch prepends before its
/// first — so ordinals are counted from the anchor, not refetched by offset.
struct KeyedRun {
    /// Index of the key column within a row (for reading boundary keys).
    key_col: usize,
    /// Ordinal of the run's first row. Exact after contiguous scroll from a
    /// known end; an estimate (`estimated`) after an interpolated jump.
    anchor: usize,
    rows: VecDeque<Vec<Value>>,
    /// Ordinals are interpolation estimates (the gutter renders them with `≈`).
    /// Cleared whenever the run touches a true end of the result, which pins
    /// the anchor exactly again.
    estimated: bool,
    /// The run's first row is the result's true first row.
    at_start: bool,
    /// The run's last row is the result's true last row.
    at_end: bool,
    /// Monotonic per-request id; a reply whose `seq` isn't the latest in-flight
    /// one is stale and dropped.
    seq: u64,
    pending: Option<u64>,
    /// Set when the in-flight fetch failed: hold off re-issuing (a
    /// deterministic error would otherwise retry — and toast — every paint)
    /// until the viewport moves again.
    halted: bool,
}

impl KeyedRun {
    fn new(key_col: usize) -> Self {
        Self {
            key_col,
            anchor: 0,
            rows: VecDeque::new(),
            estimated: false,
            at_start: false,
            at_end: false,
            seq: 0,
            pending: None,
            halted: false,
        }
    }

    fn key_of(&self, row: &[Value]) -> Value {
        row.get(self.key_col).cloned().unwrap_or(Value::Null)
    }

    fn first_key(&self) -> Option<Value> {
        self.rows.front().map(|r| self.key_of(r))
    }

    fn last_key(&self) -> Option<Value> {
        self.rows.back().map(|r| self.key_of(r))
    }

    /// Trim the run to `lo..hi`. Popping an end forfeits its `at_*` flag — the
    /// run no longer touches that end of the result.
    fn evict(&mut self, lo: usize, hi: usize) {
        while self.anchor < lo && !self.rows.is_empty() {
            self.rows.pop_front();
            self.anchor += 1;
            self.at_start = false;
        }
        while self.anchor + self.rows.len() > hi && !self.rows.is_empty() {
            self.rows.pop_back();
            self.at_end = false;
        }
    }

    fn issue(&mut self, fetch: RunFetch, epoch: u64, sender: &CommandSender) {
        self.seq += 1;
        self.pending = Some(self.seq);
        sender.send(Command::FetchRun {
            epoch,
            fetch,
            limit: PAGE,
            seq: self.seq,
        });
    }

    /// Issue (at most) one fetch toward covering `range` plus its margins. One
    /// request in flight at a time — a seek's start is the previous reply's
    /// boundary key, so they can't pipeline anyway.
    fn request(&mut self, range: Range<usize>, total: usize, epoch: u64, sender: &CommandSender) {
        if self.pending.is_some() || self.halted {
            return;
        }
        let lo = range.start.saturating_sub(MARGIN);
        let hi = (range.end + MARGIN).min(total);

        if self.rows.is_empty() {
            if range.start == 0 {
                self.issue(RunFetch::Forward { after: None }, epoch, sender);
            } else if !self.at_end {
                // `at_end` with no rows means a jump found nothing there (the
                // data shrank under the estimate) — don't re-jump every paint.
                self.issue(
                    RunFetch::Jump {
                        ordinal: range.start,
                    },
                    epoch,
                    sender,
                );
            }
            return;
        }

        let run_start = self.anchor;
        let run_end = self.anchor + self.rows.len();

        // Far from the run → relocate it rather than chain seeks across the gap.
        if range.start >= run_end + JUMP_GAP || range.end + JUMP_GAP <= run_start {
            self.issue(
                RunFetch::Jump {
                    ordinal: range.start,
                },
                epoch,
                sender,
            );
            return;
        }

        let need_fwd = run_end < hi && !self.at_end;
        let need_back = run_start > lo && !self.at_start;
        // The direction still uncovered inside the viewport goes first; margins
        // prefetch after.
        let fetch = if range.start < run_start && need_back {
            self.first_key().map(|before| RunFetch::Backward { before })
        } else if need_fwd {
            self.last_key()
                .map(|after| RunFetch::Forward { after: Some(after) })
        } else if need_back {
            self.first_key().map(|before| RunFetch::Backward { before })
        } else {
            None
        };
        if let Some(fetch) = fetch {
            self.issue(fetch, epoch, sender);
        }
    }

    /// Land one `ResultRunLoaded` reply. The echoed `seq` must be the in-flight
    /// one and (for extensions) the echoed boundary must still be the run's —
    /// eviction may have moved it since the request, in which case the reply is
    /// dropped and the next paint re-requests from the new boundary.
    fn apply(
        &mut self,
        fetch: RunFetch,
        rows: Vec<Vec<Value>>,
        estimated: bool,
        seq: u64,
        total: usize,
    ) {
        if self.pending != Some(seq) {
            return;
        }
        self.pending = None;
        let n = rows.len();
        let short = n < PAGE;
        match fetch {
            RunFetch::Forward { after } => {
                if after != self.last_key() {
                    return;
                }
                if self.rows.is_empty() {
                    // Seeded from the result's start: ordinal 0, exact.
                    self.anchor = 0;
                    self.at_start = true;
                    self.estimated = false;
                }
                self.rows.extend(rows);
                self.at_end = short;
                if short {
                    // The run now touches the true last row, so its ordinals
                    // count back from `total` — exact again.
                    self.anchor = total.saturating_sub(self.rows.len());
                    self.estimated = false;
                }
            }
            RunFetch::Backward { before } => {
                if self.rows.is_empty() || Some(before) != self.first_key() {
                    return;
                }
                // Rows arrive descending; pushing each to the front restores
                // ascending order.
                for row in rows {
                    self.rows.push_front(row);
                }
                self.anchor = self.anchor.saturating_sub(n);
                if short || self.anchor == 0 {
                    // Touched the true first row: pin the anchor at 0, exact.
                    self.at_start = true;
                    self.anchor = 0;
                    self.estimated = false;
                }
            }
            RunFetch::Jump { ordinal } => {
                self.rows = rows.into();
                self.at_end = short;
                self.estimated = estimated;
                self.anchor = if short {
                    self.estimated = false;
                    total.saturating_sub(self.rows.len())
                } else {
                    ordinal.min(total.saturating_sub(self.rows.len()))
                };
                self.at_start = self.anchor == 0;
            }
        }
    }
}

impl GridBuffer {
    /// The cells at ordinal `ix`, if resident.
    fn row(&self, ix: usize) -> Option<&Vec<Value>> {
        match &self.mode {
            BufferMode::Offset(pages) => pages.rows.get(&ix),
            BufferMode::Keyed(run) => ix.checked_sub(run.anchor).and_then(|i| run.rows.get(i)),
        }
    }

    /// Whether the resident rows' ordinals are interpolation estimates.
    fn is_estimated(&self) -> bool {
        match &self.mode {
            BufferMode::Offset(_) => false,
            BufferMode::Keyed(run) => run.estimated,
        }
    }

    /// Whether this result pages by keyset runs (vs. `OFFSET`) — shown in the
    /// footer so the active paging mode is visible at a glance.
    fn is_keyed(&self) -> bool {
        matches!(&self.mode, BufferMode::Keyed(_))
    }

    /// The in-flight run fetch failed: free the slot so fetching can resume,
    /// but hold off until the viewport moves (see `KeyedRun::halted`).
    fn run_failed(&mut self, seq: u64) {
        if let BufferMode::Keyed(run) = &mut self.mode {
            if run.pending == Some(seq) {
                run.pending = None;
                run.halted = true;
            }
        }
    }

    /// Drop a freshly-arrived `OFFSET` page in and clear its in-flight mark.
    /// A no-op in keyed mode (keyed grids never request pages).
    fn insert_page(&mut self, offset: usize, rows: Vec<Vec<Value>>) {
        if let BufferMode::Offset(pages) = &mut self.mode {
            pages.requested.remove(&(offset / PAGE));
            for (i, row) in rows.into_iter().enumerate() {
                pages.rows.insert(offset + i, row);
            }
        }
    }

    /// Land a keyset run reply (keyed mode only).
    fn apply_run(
        &mut self,
        fetch: RunFetch,
        rows: Vec<Vec<Value>>,
        estimated: bool,
        seq: u64,
        total: usize,
    ) {
        if let BufferMode::Keyed(run) = &mut self.mode {
            run.apply(fetch, rows, estimated, seq, total);
        }
    }

    /// Ensure the rows covering `range` are loaded (or requested), evicting
    /// everything beyond a margin around it. Called per paint with the
    /// about-to-render window, so the buffer tracks the viewport.
    ///
    /// Returns `false` when fetching was skipped because the viewport is flinging
    /// (see `FLING_ROWS`) — the caller schedules another paint so the resting
    /// window still loads once the scroll settles.
    fn ensure(
        &mut self,
        range: Range<usize>,
        total: usize,
        epoch: u64,
        sender: &CommandSender,
    ) -> bool {
        let lo = range.start.saturating_sub(MARGIN);
        let hi = (range.end + MARGIN).min(total);
        match &mut self.mode {
            BufferMode::Offset(pages) => {
                pages.rows.retain(|k, _| *k >= lo && *k < hi);
                pages
                    .requested
                    .retain(|p| p * PAGE + PAGE > lo && p * PAGE < hi);
            }
            BufferMode::Keyed(run) => run.evict(lo, hi),
        }

        if range.is_empty() {
            return true;
        }

        // A failed fetch halts re-issuing; movement is the retry signal.
        if let BufferMode::Keyed(run) = &mut self.mode {
            if run.halted && self.last_start != Some(range.start) {
                run.halted = false;
            }
        }

        // While the viewport is flying past rows the user won't dwell on, don't
        // spawn a fetch for each one; wait until the scroll slows near its
        // destination.
        let settled = self
            .last_start
            .is_none_or(|prev| range.start.abs_diff(prev) <= FLING_ROWS);
        self.last_start = Some(range.start);
        if !settled {
            return false;
        }

        match &mut self.mode {
            BufferMode::Offset(pages) => pages.request(range, total, epoch, sender),
            BufferMode::Keyed(run) => run.request(range, total, epoch, sender),
        }
        true
    }
}

impl OffsetPages {
    /// Request any missing pages covering `range` (the offset path).
    fn request(&mut self, range: Range<usize>, total: usize, epoch: u64, sender: &CommandSender) {
        let first = range.start / PAGE;
        let last = (range.end - 1) / PAGE;
        for page in first..=last {
            let offset = page * PAGE;
            if offset >= total || self.requested.contains(&page) {
                continue;
            }
            let end = (offset + PAGE).min(total);
            if (offset..end).all(|i| self.rows.contains_key(&i)) {
                continue;
            }
            self.requested.insert(page);
            sender.send(Command::FetchPage {
                offset,
                limit: PAGE,
                epoch,
            });
        }
    }
}

/// The virtual-scroll window resolved for one render (see
/// [`ResultGrid::prepare_window`]).
struct WindowView {
    /// Absolute ordinal of list-local index 0.
    base: usize,
    /// Physical rows fed to `uniform_list` this frame (`total.min(WINDOW)`).
    len: usize,
    /// Scrollbar thumb position, 0..=1, over the *whole* result.
    fraction: f32,
    /// Scrollbar thumb size (viewport / total).
    thumb: f32,
}

/// Pure window arithmetic, factored out of [`ResultGrid::prepare_window`] for
/// testing. Given the result `total`, the current window `base`, the viewport's
/// top row in list-local coordinates, and the viewport height in rows, returns
/// the base to use this frame and — when it changed — the list-local row the
/// pixel offset must be re-anchored onto so the visible rows don't move.
///
/// The window re-centers on the viewport once it scrolls within
/// [`REANCHOR_MARGIN`] of an edge that has more result beyond it.
fn window_decision(
    total: usize,
    base: usize,
    local_first: usize,
    viewport_rows: usize,
) -> (usize, Option<usize>) {
    if total <= WINDOW {
        return (0, None);
    }
    let max_base = total - WINDOW;
    let base = base.min(max_base);
    let abs_first = base + local_first;
    let near_top = base > 0 && local_first < REANCHOR_MARGIN;
    let near_bottom = base < max_base && local_first + viewport_rows + REANCHOR_MARGIN > WINDOW;
    if near_top || near_bottom {
        let desired = abs_first.saturating_sub(WINDOW / 2).min(max_base);
        if desired != base {
            return (desired, Some(abs_first - desired));
        }
    }
    (base, None)
}

/// All the state for one open result. The grid columns include a leading row-
/// number gutter, so table-column index `0` is the gutter and data column `n` is
/// table column `n + 1`.
pub(crate) struct ResultGrid {
    pub label: String,
    base_sql: String,
    columns: Vec<ResultColumn>,
    total: usize,
    ready: bool,
    error: Option<String>,
    /// `(data column, ascending)` — `None` is unsorted.
    sort: Option<(usize, bool)>,
    selection: Option<CellRange>,
    /// The `(schema, table)` this result browses, when it's a plain table
    /// preview — sent with `OpenResult` so the backend can resolve a seek key.
    /// `None` for editor SQL and for sorted re-opens (which wrap the SQL).
    table: Option<(String, String)>,
    buffer: Rc<RefCell<GridBuffer>>,
    sender: CommandSender,
    scroll: UniformListScrollHandle,
    h_scroll: ScrollHandle,
    /// The overlay scrollbar's in-flight drag.
    scrollbar: ScrollbarState,
    /// Virtual-scroll window: the absolute ordinal that list-local index 0 maps
    /// to. `Rc` so the scrollbar's scrub closure can move it; `Cell` because
    /// `Table`/`uniform_list` are stateless across frames, so the base lives
    /// here. See `WINDOW` and `prepare_window`.
    window_base: Rc<Cell<usize>>,
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
    fn query_time(&self) -> Duration {
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
    fn prepare_window(&self, row_height: Pixels) -> WindowView {
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
                if let Some(value) = buffer.row(r).and_then(|row| row.get(dcol)) {
                    out.push_str(&cell_string(value));
                }
            }
            out.push('\n');
        }
        Some(out)
    }
}

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
fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
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

/// True for a canonical `8-4-4-4-12` hex UUID — dimmed like the design's id columns.
fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes().iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// One grid cell, colored by value kind (NULL italic-faint, numbers accented,
/// UUIDs dimmed, JSON-ish text cyan — mirroring the design's typed cells).
fn render_cell(value: Option<&Value>, c: CellColors) -> gpui::AnyElement {
    match value {
        None | Some(Value::Null) => div()
            .italic()
            .text_color(c.faint)
            .child("NULL")
            .into_any_element(),
        Some(Value::Integer(n)) => div()
            .text_color(c.num)
            .child(n.to_string())
            .into_any_element(),
        Some(Value::Real(x)) => div()
            .text_color(c.num)
            .child(x.to_string())
            .into_any_element(),
        Some(Value::Text(s)) => {
            let trimmed = s.trim_start();
            let color = if is_uuid(s) {
                c.muted
            } else if trimmed.starts_with('{') || trimmed.starts_with('[') {
                c.cyan
            } else {
                c.text
            };
            div().text_color(color).child(s.clone()).into_any_element()
        }
        Some(Value::Blob(b)) => div()
            .text_color(c.faint)
            .child(format!("<{} bytes>", b.len()))
            .into_any_element(),
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

    /// The results pane: an empty state, an error, or the live windowed grid.
    pub(crate) fn render_result(
        &self,
        active: &ActiveConn,
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
        let cell_colors = CellColors {
            text,
            muted,
            num,
            cyan,
            faint,
        };
        let container = div().size_full().flex().flex_col().bg(bg);

        let grid = match &active.active().result {
            Some(grid) => grid,
            None => {
                return container.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.))
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
                    .font_family(FONT_MONO)
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(px(11.))
                            .text_color(red)
                            .child("Query failed")
                            .child(div().text_color(faint).child(format!("· {elapsed}"))),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(text)
                            .child(err.clone()),
                    ),
            );
        }

        let status = if !grid.ready {
            div()
                .text_color(faint)
                .child(format!("running… {elapsed}"))
        } else {
            div()
                .text_color(faint)
                .child(format!("{} rows · {elapsed}", grid.total))
        };
        let view = cx.entity().downgrade();
        let toolbar = div()
            .flex_shrink_0()
            .h(px(30.))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(border)
            .font_family(FONT_MONO)
            .text_size(px(11.))
            .child(div().text_color(muted).child(grid.label.clone()))
            .child(status)
            .child(
                div()
                    .ml_auto()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new("result-copy", "Copy")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.copy_result_selection(cx))),
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
                    ),
            );

        if !grid.ready {
            return container.child(toolbar);
        }

        // Row-number gutter + one fixed-width, sortable column per result column.
        // Each header carries the engine's declared type as a dim subtitle, like
        // the design's typed headers (`email` + `text`).
        let mut columns = vec![Column::new("#").width(px(56.)).align_end()];
        for c in &grid.columns {
            let mut col = Column::new(c.name.clone()).width(px(180.)).sortable();
            if let Some(t) = &c.decl_type {
                if !t.is_empty() {
                    col = col.subtitle(t.to_lowercase());
                }
            }
            columns.push(col);
        }
        let sort = grid.sort.map(|(c, asc)| (c + 1, asc));
        let total = grid.total;
        let ncols = grid.columns.len();
        let buffer_range = grid.buffer.clone();
        let buffer_row = grid.buffer.clone();
        let sender = grid.sender.clone();
        let epoch = grid.epoch;
        let (sort_view, cell_view) = (view.clone(), view.clone());

        // Resolve (and possibly re-center) the virtual-scroll window for this
        // frame; everything below works in list-local coordinates offset by
        // `base`, so the list only ever lays out `win.len` rows.
        let row_height = self.settings.density().row_height();
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

        let table = Table::<()>::new("result-grid", columns)
            .row_count(win.len)
            .row_height(row_height)
            .font_family(FONT_MONO)
            .grid_lines(true)
            .track_scroll(&grid.scroll)
            .track_horizontal_scroll(&grid.h_scroll)
            .horizontal(true)
            .selected_cells(local_selection)
            .sort(sort)
            .sort_carets(
                move || crate::icons::icon("sort-asc", px(9.), accent).into_any_element(),
                move || crate::icons::icon("sort-desc", px(9.), accent).into_any_element(),
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
            .on_sort(move |table_col, _, cx| {
                sort_view
                    .update(cx, |this, cx| this.result_sort(table_col, cx))
                    .ok();
            })
            .on_cell_click(move |row, table_col, event, _, cx| {
                let extend = event.modifiers().shift;
                let abs_row = base + row;
                cell_view
                    .update(cx, |this, cx| {
                        this.result_select(abs_row, table_col, extend, cx)
                    })
                    .ok();
            })
            .render_row(move |ix, _, _| {
                // `ix` is list-local; the gutter and buffer are absolute.
                let abs = base + ix;
                let mut out = Vec::with_capacity(ncols + 1);
                let buffer = buffer_row.borrow();
                // After an interpolated jump the run's ordinals are estimates;
                // the gutter marks them `≈` until a true end pins them exact.
                let gutter = if buffer.is_estimated() {
                    format!("≈{}", group_digits(abs + 1))
                } else {
                    group_digits(abs + 1)
                };
                out.push(div().text_color(faint).child(gutter).into_any_element());
                match buffer.row(abs) {
                    Some(row) => {
                        for c in 0..ncols {
                            out.push(render_cell(row.get(c), cell_colors));
                        }
                    }
                    None => {
                        for _ in 0..ncols {
                            out.push(div().text_color(faint).child("·").into_any_element());
                        }
                    }
                }
                out
            });

        // Footer: a strong row count, the column count, and the result's label —
        // the design's "N rows · K columns" status strip under the grid.
        let footer = div()
            .flex_shrink_0()
            .h(px(28.))
            .flex()
            .items_center()
            .gap_2()
            .px_3p5()
            .bg(bg)
            .border_t_1()
            .border_color(border)
            .font_family(FONT_MONO)
            .text_size(px(11.))
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
            .child(div().ml_auto().text_color(dim).child(grid.label.clone()));

        // The draggable, fraction-mapped scrollbar: the thumb
        // mirrors the list's position; a scrub jumps the viewport, and the
        // buffer's `ensure` turns the far jump into one key-space seek (keyed
        // results) or one OFFSET page (fallback).
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
                // Re-center the window on the target, then place it at the top of
                // the viewport by setting the list's pixel offset directly — no
                // `scroll_to_item` into a multi-million-row (f32-degenerate)
                // canvas.
                let base = if total > WINDOW {
                    target.saturating_sub(WINDOW / 2).min(total - WINDOW)
                } else {
                    0
                };
                scrub_window.set(base);
                let local = target - base;
                let st = scrub_scroll.0.borrow();
                let x = st.base_handle.offset().x;
                st.base_handle
                    .set_offset(point(x, px(-(local as f32 * rh))));
                drop(st);
                scrub_view.update(cx, |_, cx| cx.notify()).ok();
            });

        container
            .child(toolbar)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .bg(bg_app)
                    .relative()
                    .child(table)
                    .child(scrollbar),
            )
            .child(footer)
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    /// A small result fits in one window: never windowed, never re-anchored.
    #[test]
    fn small_result_is_never_windowed() {
        assert_eq!(window_decision(WINDOW, 0, 0, 30), (0, None));
        assert_eq!(window_decision(WINDOW, 0, WINDOW - 1, 30), (0, None));
        assert_eq!(window_decision(500, 0, 400, 30), (0, None));
    }

    /// At rest in the middle of a window, with margins on both sides, nothing
    /// moves.
    #[test]
    fn mid_window_holds_still() {
        let total = 50_000_000;
        assert_eq!(
            window_decision(total, 1_000_000, WINDOW / 2, 30),
            (1_000_000, None)
        );
    }

    /// Scrolling near the bottom edge re-centers the window forward and reports
    /// the local row to pin the offset to (so the visible rows don't jump).
    #[test]
    fn near_bottom_recenters_forward() {
        let total = 50_000_000;
        let base = 1_000_000;
        let local_first = WINDOW - 100; // viewport top is 100 rows from the edge
        let (new_base, reanchor) = window_decision(total, base, local_first, 30);
        let abs_first = base + local_first;
        // The window slid forward and the viewport sits near its middle again.
        assert!(new_base > base);
        assert_eq!(reanchor, Some(abs_first - new_base));
        assert_eq!(new_base + reanchor.unwrap(), abs_first); // same absolute row
        assert_eq!(reanchor.unwrap(), WINDOW / 2);
    }

    /// Scrolling back near the top edge re-centers the window backward.
    #[test]
    fn near_top_recenters_backward() {
        let total = 50_000_000;
        let base = 1_000_000;
        let local_first = 100;
        let (new_base, reanchor) = window_decision(total, base, local_first, 30);
        let abs_first = base + local_first;
        assert!(new_base < base);
        assert_eq!(new_base + reanchor.unwrap(), abs_first);
        assert_eq!(reanchor.unwrap(), WINDOW / 2);
    }

    /// Near the result's true start the window can't slide further: clamps to 0,
    /// and once the viewport is genuinely at the top it stays put.
    #[test]
    fn clamps_at_result_start() {
        let total = 50_000_000;
        // base small, viewport near window top: desired base clamps at 0.
        let (new_base, _) = window_decision(total, 10_000, 100, 30);
        assert_eq!(new_base, 0);
        // base 0 with the viewport at the very top: nothing to do.
        assert_eq!(window_decision(total, 0, 0, 30), (0, None));
    }

    /// Near the result's true end the base clamps to `total - WINDOW`.
    #[test]
    fn clamps_at_result_end() {
        let total = 50_000_000;
        let max_base = total - WINDOW;
        // Already at the last window, viewport near the bottom: no further slide.
        assert_eq!(
            window_decision(total, max_base, WINDOW - 50, 30),
            (max_base, None)
        );
    }
}

#[cfg(test)]
mod keyed_run_tests {
    use super::*;

    /// A run row whose key (column 0) is `id`.
    fn row(id: i64) -> Vec<Value> {
        vec![Value::Integer(id), Value::Text(format!("row {id}"))]
    }

    fn rows(ids: impl IntoIterator<Item = i64>) -> Vec<Vec<Value>> {
        ids.into_iter().map(row).collect()
    }

    /// A run pretending its in-flight request is `seq` (as `issue` would set).
    fn pending(mut run: KeyedRun, seq: u64) -> KeyedRun {
        run.seq = seq;
        run.pending = Some(seq);
        run
    }

    const TOTAL: usize = 10_000;

    #[test]
    fn forward_from_start_is_exact() {
        let mut run = pending(KeyedRun::new(0), 1);
        run.apply(
            RunFetch::Forward { after: None },
            rows(1..=PAGE as i64),
            false,
            1,
            TOTAL,
        );
        assert_eq!(run.anchor, 0);
        assert!(run.at_start && !run.at_end && !run.estimated);
        assert_eq!(run.rows.len(), PAGE);

        // Extend forward from the boundary key.
        run = pending(run, 2);
        run.apply(
            RunFetch::Forward {
                after: Some(Value::Integer(PAGE as i64)),
            },
            rows(PAGE as i64 + 1..=2 * PAGE as i64),
            false,
            2,
            TOTAL,
        );
        assert_eq!(run.rows.len(), 2 * PAGE);
        assert_eq!(run.anchor, 0);
    }

    #[test]
    fn short_forward_pins_the_run_to_the_end() {
        let mut run = pending(KeyedRun::new(0), 1);
        // An estimated run near the bottom gets a short (final) page: the
        // anchor re-pins so the run ends exactly at `total`.
        run.anchor = 9_950;
        run.estimated = true;
        run.rows = rows(99_001..=99_010).into();
        run.apply(
            RunFetch::Forward {
                after: Some(Value::Integer(99_010)),
            },
            rows(99_011..=99_015), // short: the result ends here
            false,
            1,
            TOTAL,
        );
        assert!(run.at_end);
        assert!(!run.estimated, "touching the true end makes ordinals exact");
        assert_eq!(run.anchor + run.rows.len(), TOTAL);
    }

    #[test]
    fn backward_prepends_descending_rows_in_order() {
        let mut run = pending(KeyedRun::new(0), 1);
        run.anchor = 500;
        run.rows = rows(501..=700).into();
        let page = 501 - PAGE as i64..=500;
        run.apply(
            RunFetch::Backward {
                before: Value::Integer(501),
            },
            rows(page.rev()), // a full page, arriving descending: 500, 499, …
            false,
            1,
            TOTAL,
        );
        assert_eq!(run.anchor, 500 - PAGE);
        assert!(!run.at_start, "a full page doesn't touch the start");
        let head: Vec<_> = run.rows.iter().take(2).map(|r| r[0].clone()).collect();
        assert_eq!(
            head,
            vec![
                Value::Integer(501 - PAGE as i64),
                Value::Integer(502 - PAGE as i64)
            ]
        );
        // The run stays contiguous across the seam.
        assert_eq!(run.rows[PAGE - 1][0], Value::Integer(500));
        assert_eq!(run.rows[PAGE][0], Value::Integer(501));
    }

    #[test]
    fn short_backward_pins_the_run_to_the_start() {
        let mut run = pending(KeyedRun::new(0), 1);
        run.anchor = 80; // estimate was high — only 3 rows actually precede
        run.estimated = true;
        run.rows = rows(4..=10).into();
        run.apply(
            RunFetch::Backward {
                before: Value::Integer(4),
            },
            rows((1..=3).rev()),
            false,
            1,
            TOTAL,
        );
        assert!(run.at_start && !run.estimated);
        assert_eq!(run.anchor, 0);
    }

    #[test]
    fn jump_replaces_the_run_with_estimated_ordinals() {
        let mut run = pending(KeyedRun::new(0), 1);
        run.anchor = 0;
        run.rows = rows(1..=200).into();
        run.apply(
            RunFetch::Jump { ordinal: 6_700 },
            rows(66_000..66_000 + PAGE as i64),
            true,
            1,
            TOTAL,
        );
        assert_eq!(run.anchor, 6_700);
        assert!(run.estimated && !run.at_start && !run.at_end);
        assert_eq!(run.rows.len(), PAGE);
    }

    #[test]
    fn stale_replies_are_dropped() {
        let mut run = pending(KeyedRun::new(0), 2);
        run.rows = rows(1..=10).into();

        // Wrong seq: a reply for a superseded request.
        run.apply(
            RunFetch::Jump { ordinal: 50 },
            rows(51..=60),
            true,
            1,
            TOTAL,
        );
        assert_eq!(run.anchor, 0);
        assert_eq!(run.rows.len(), 10);

        // Right seq but the boundary moved (eviction): dropped too.
        run = pending(run, 3);
        run.apply(
            RunFetch::Forward {
                after: Some(Value::Integer(999)),
            },
            rows(1_000..=1_004),
            false,
            3,
            TOTAL,
        );
        assert_eq!(run.rows.len(), 10, "mismatched boundary reply is dropped");
        assert!(run.pending.is_none(), "but the in-flight slot frees up");
    }

    #[test]
    fn eviction_trims_the_run_and_forfeits_end_flags() {
        let mut run = KeyedRun::new(0);
        run.anchor = 0;
        run.at_start = true;
        run.rows = rows(1..=1000).into();
        run.evict(300, 800);
        assert_eq!(run.anchor, 300);
        assert_eq!(run.rows.len(), 500);
        assert!(!run.at_start && !run.at_end);
        assert_eq!(run.first_key(), Some(Value::Integer(301)));
        assert_eq!(run.last_key(), Some(Value::Integer(800)));
    }
}
