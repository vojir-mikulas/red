//! The windowed row buffer behind the grid: the two paging modes (offset and
//! keyset-run), eviction around the viewport, and the virtual-scroll window
//! arithmetic. Holds at most ~`2*MARGIN` rows regardless of result size.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

use red_core::Value;
use red_service::{Command, CommandSender, RunFetch};

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
pub(super) const WINDOW: usize = 100_000;

/// When the viewport scrolls within this many rows of a window edge — and more
/// result exists beyond that edge — the window re-centers on the viewport,
/// compensating the list's pixel offset so the visible rows don't move.
const REANCHOR_MARGIN: usize = 5_000;

/// Monotonic id for each opened result. Tags `OpenResult`/`FetchPage` so the
/// backend can drop stale page fetches and the grid can ignore late replies for
/// a result it has already replaced (table switch / re-sort). Starts at 1 — `0`
/// is the backend's "no live result" sentinel.
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

pub(super) fn next_epoch() -> u64 {
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
///
/// [`KeySpec`]: red_core::KeySpec
#[derive(Default)]
pub(super) struct GridBuffer {
    pub(super) mode: BufferMode,
    /// The previous paint's first visible row, to gauge scroll velocity (see
    /// `FLING_ROWS`). `None` until the first paint.
    last_start: Option<usize>,
}

pub(super) enum BufferMode {
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
pub(super) struct OffsetPages {
    rows: HashMap<usize, Vec<Value>>,
    requested: HashSet<usize>,
}

/// Keyed mode: one contiguous run of rows, `anchor..anchor + rows.len()` in
/// ordinal space. Extension is run-relative — a forward fetch appends rows
/// strictly after the run's last key, a backward fetch prepends before its
/// first — so ordinals are counted from the anchor, not refetched by offset.
pub(super) struct KeyedRun {
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
    pub(super) fn new(key_col: usize) -> Self {
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

    /// Relocate the run to *exactly* `ordinal` — the explicit "go to row N".
    /// Clears the run (so the next paint can't extend a now-stale boundary) and
    /// issues an **exact** jump that bypasses key-space interpolation, landing on
    /// the true row with non-estimated ordinals. The pending mark it sets stops
    /// `request` from racing an interpolated jump in before the reply lands.
    pub(super) fn jump_exact(&mut self, ordinal: usize, epoch: u64, sender: &CommandSender) {
        self.rows.clear();
        self.at_start = false;
        self.at_end = false;
        self.estimated = false;
        self.halted = false;
        self.issue(
            RunFetch::Jump {
                ordinal,
                exact: true,
            },
            epoch,
            sender,
        );
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
                        exact: false,
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
                    exact: false,
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
            RunFetch::Jump { ordinal, .. } => {
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
    pub(super) fn row(&self, ix: usize) -> Option<&Vec<Value>> {
        match &self.mode {
            BufferMode::Offset(pages) => pages.rows.get(&ix),
            BufferMode::Keyed(run) => ix.checked_sub(run.anchor).and_then(|i| run.rows.get(i)),
        }
    }

    /// Whether the resident rows' ordinals are interpolation estimates.
    pub(super) fn is_estimated(&self) -> bool {
        match &self.mode {
            BufferMode::Offset(_) => false,
            BufferMode::Keyed(run) => run.estimated,
        }
    }

    /// Whether this result pages by keyset runs (vs. `OFFSET`) — shown in the
    /// footer so the active paging mode is visible at a glance.
    pub(super) fn is_keyed(&self) -> bool {
        matches!(&self.mode, BufferMode::Keyed(_))
    }

    /// The in-flight run fetch failed: free the slot so fetching can resume,
    /// but hold off until the viewport moves (see `KeyedRun::halted`).
    pub(super) fn run_failed(&mut self, seq: u64) {
        if let BufferMode::Keyed(run) = &mut self.mode {
            if run.pending == Some(seq) {
                run.pending = None;
                run.halted = true;
            }
        }
    }

    /// Drop a freshly-arrived `OFFSET` page in and clear its in-flight mark.
    /// A no-op in keyed mode (keyed grids never request pages).
    pub(super) fn insert_page(&mut self, offset: usize, rows: Vec<Vec<Value>>) {
        if let BufferMode::Offset(pages) = &mut self.mode {
            pages.requested.remove(&(offset / PAGE));
            for (i, row) in rows.into_iter().enumerate() {
                pages.rows.insert(offset + i, row);
            }
        }
    }

    /// Land a keyset run reply (keyed mode only).
    pub(super) fn apply_run(
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
    pub(super) fn ensure(
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
/// `ResultGrid::prepare_window`).
pub(super) struct WindowView {
    /// Absolute ordinal of list-local index 0.
    pub(super) base: usize,
    /// Physical rows fed to `uniform_list` this frame (`total.min(WINDOW)`).
    pub(super) len: usize,
    /// Scrollbar thumb position, 0..=1, over the *whole* result.
    pub(super) fraction: f32,
    /// Scrollbar thumb size (viewport / total).
    pub(super) thumb: f32,
}

/// Pure window arithmetic, factored out of `ResultGrid::prepare_window` for
/// testing. Given the result `total`, the current window `base`, the viewport's
/// top row in list-local coordinates, and the viewport height in rows, returns
/// the base to use this frame and — when it changed — the list-local row the
/// pixel offset must be re-anchored onto so the visible rows don't move.
///
/// The window re-centers on the viewport once it scrolls within
/// [`REANCHOR_MARGIN`] of an edge that has more result beyond it.
pub(super) fn window_decision(
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
            RunFetch::Jump {
                ordinal: 6_700,
                exact: false,
            },
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
            RunFetch::Jump {
                ordinal: 50,
                exact: false,
            },
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
