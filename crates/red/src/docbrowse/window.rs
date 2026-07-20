//! The browse grid's windowed document buffer: one contiguous run of documents
//! in absolute-ordinal space, ordered by `_id`, extended by keyset seeks and
//! relocated by exact `skip` jumps. It is the document analogue of the SQL grid's
//! keyed run (`crate::result::buffer`), and shares the virtual-scroll window
//! arithmetic in [`crate::gridwindow`] so a collection of any size scrolls
//! smoothly without materializing more than the resident window.
//!
//! It is deliberately simpler than the SQL run in two ways the document shape
//! allows: `_id` is a single, always-indexed key, so the run is *always* keyed
//! (no offset fallback); and a far jump lands by `skip` at an exact ordinal, so
//! the run's ordinals are always precise (the SQL run interpolates and carries an
//! `estimated` flag; this one never does).

use std::cell::Cell;
use std::collections::VecDeque;
use std::ops::Range;
use std::rc::Rc;

use gpui::{Pixels, UniformListScrollHandle, point, px};
use red_core::doc::{DocSeek, DocValue, Document};
use red_service::{Command, CommandSender, Epoch};

use crate::gridwindow::{WINDOW, WindowView, centered_base, scrollbar_metrics, window_decision};

/// Documents fetched per keyset window. Every derived span scales from it: the
/// resident margin (`MARGIN_PAGES * PAGE` rows kept beyond the viewport), the
/// fling threshold (`FLING_PAGES * PAGE` rows/frame past which fetching pauses),
/// and the jump gap (`JUMP_PAGES * PAGE` rows past the run that relocates it).
const PAGE: usize = 200;
const MARGIN_PAGES: usize = 2;
const FLING_PAGES: usize = 3;
const JUMP_PAGES: usize = 2;

/// The addressing a fetch needs beyond the run's own state: which browse (epoch),
/// which namespace, and the active filter. Borrowed, so a paint builds it for
/// free from the [`CollView`](super::CollView) without cloning strings until an
/// actual [`Command`] is issued.
pub(super) struct FetchCtx<'a> {
    pub epoch: Epoch,
    pub db: &'a str,
    pub coll: &'a str,
    pub filter: Option<&'a str>,
}

/// The windowed document run. Holds at most ~`2 * MARGIN_PAGES * PAGE` documents
/// regardless of collection size; everything beyond the viewport margin is
/// evicted each paint.
pub(super) struct DocWindow {
    /// The collection's total document count (honoring the filter), or `None`
    /// until the first window reports it. Drives the grid's `row_count` and the
    /// scrollbar, and pins the run's anchor when a short window touches the end.
    total: Option<usize>,
    /// Ordinal of `docs.front()` in the whole (filtered) collection.
    anchor: usize,
    docs: VecDeque<Document>,
    /// The run's first document is the collection's true first (`_id`-least).
    at_start: bool,
    /// The run's last document is the collection's true last.
    at_end: bool,
    /// Monotonic per-request id; a reply whose `seq` isn't the latest in-flight
    /// one is stale and dropped.
    seq: u64,
    pending: Option<u64>,
    /// Set when the in-flight fetch failed: hold off re-issuing until the viewport
    /// moves again, so a deterministic error doesn't retry (and toast) every paint.
    halted: bool,
    /// The previous paint's first visible ordinal, to gauge scroll velocity.
    last_start: Option<usize>,
    /// Absolute ordinal of the virtual window's list-local row 0. `Rc<Cell>` so
    /// the scrollbar's scrub closure can relocate it (see [`Self::place_window`]).
    window_base: Rc<Cell<usize>>,
}

impl DocWindow {
    pub(super) fn new() -> Self {
        Self {
            total: None,
            anchor: 0,
            docs: VecDeque::new(),
            at_start: false,
            at_end: false,
            seq: 0,
            pending: None,
            halted: false,
            last_start: None,
            window_base: Rc::new(Cell::new(0)),
        }
    }

    /// The collection's total document count once known.
    pub(super) fn total(&self) -> Option<usize> {
        self.total
    }

    /// Rows the grid lays out: the known total, or (while the first window loads)
    /// the resident count so an in-flight browse still renders what it has.
    pub(super) fn row_count(&self) -> usize {
        self.total.unwrap_or(self.docs.len())
    }

    pub(super) fn anchor(&self) -> usize {
        self.anchor
    }

    /// The document at absolute ordinal `ord`, if resident.
    pub(super) fn doc_at(&self, ord: usize) -> Option<&Document> {
        ord.checked_sub(self.anchor).and_then(|i| self.docs.get(i))
    }

    /// The resident run and the ordinal of its first document, for a render that
    /// clones the visible slice into its closure.
    pub(super) fn resident(&self) -> (usize, &VecDeque<Document>) {
        (self.anchor, &self.docs)
    }

    fn first_id(&self) -> Option<DocValue> {
        self.docs.front().map(|d| d.id.clone())
    }

    fn last_id(&self) -> Option<DocValue> {
        self.docs.back().map(|d| d.id.clone())
    }

    /// A shared handle on the window base, for the scrollbar scrub closure.
    pub(super) fn window_base_handle(&self) -> Rc<Cell<usize>> {
        self.window_base.clone()
    }

    /// The absolute ordinal of the virtual window's list-local row 0 this frame.
    pub(super) fn window_base(&self) -> usize {
        self.window_base.get()
    }

    /// Reset for a fresh browse (new collection, or a changed filter): drop the
    /// run and its count so the next paint re-seeds from ordinal 0.
    pub(super) fn reset(&mut self) {
        self.total = None;
        self.anchor = 0;
        self.docs.clear();
        self.at_start = false;
        self.at_end = false;
        self.pending = None;
        self.halted = false;
        self.last_start = None;
        self.window_base.set(0);
    }

    /// Reset and fetch the first window (also asking for the count). The seed the
    /// browse starts from before the grid's scroll takes over; issuing it up front
    /// (rather than waiting for the first paint) means the List/JSON modes have
    /// documents even if the grid never paints.
    pub(super) fn seed(&mut self, ctx: &FetchCtx, sender: &CommandSender) {
        self.reset();
        self.issue(DocSeek::Forward { after: None }, true, ctx, sender);
    }

    /// Trim the run to `lo..hi`. Popping an end forfeits its `at_*` flag: the run
    /// no longer touches that end of the collection.
    fn evict(&mut self, lo: usize, hi: usize) {
        while self.anchor < lo && !self.docs.is_empty() {
            self.docs.pop_front();
            self.anchor += 1;
            self.at_start = false;
        }
        while self.anchor + self.docs.len() > hi && !self.docs.is_empty() {
            self.docs.pop_back();
            self.at_end = false;
        }
    }

    fn issue(&mut self, seek: DocSeek, want_total: bool, ctx: &FetchCtx, sender: &CommandSender) {
        self.seq += 1;
        self.pending = Some(self.seq);
        sender.send(Command::DocFetchRun {
            epoch: ctx.epoch,
            db: ctx.db.to_string(),
            coll: ctx.coll.to_string(),
            filter: ctx.filter.map(str::to_string),
            seek,
            limit: PAGE,
            seq: self.seq,
            want_total,
        });
    }

    /// Ensure the documents covering `range` are loaded (or requested), evicting
    /// everything beyond a margin around it. Called per paint with the
    /// about-to-render window, so the run tracks the viewport.
    ///
    /// Returns `false` when fetching was skipped because the viewport is flinging;
    /// the caller schedules another paint so the resting window still loads.
    pub(super) fn ensure(
        &mut self,
        range: Range<usize>,
        ctx: &FetchCtx,
        sender: &CommandSender,
    ) -> bool {
        let cap = self.total.unwrap_or(usize::MAX);
        let margin = MARGIN_PAGES * PAGE;
        let lo = range.start.saturating_sub(margin);
        let hi = range.end.saturating_add(margin).min(cap);
        self.evict(lo, hi);

        if range.is_empty() {
            return true;
        }

        // A failed fetch halts re-issuing; movement is the retry signal.
        if self.halted && self.last_start != Some(range.start) {
            self.halted = false;
        }

        // While the viewport flies past documents the user won't dwell on, don't
        // spawn a fetch for each one; wait until the scroll slows near its rest.
        let settled = self
            .last_start
            .is_none_or(|prev| range.start.abs_diff(prev) <= FLING_PAGES * PAGE);
        self.last_start = Some(range.start);
        if !settled {
            return false;
        }

        self.request(range, ctx, sender);
        true
    }

    /// Issue (at most) one fetch toward covering `range` plus its margins. One
    /// request in flight at a time; a seek's boundary is the previous reply's, so
    /// they can't pipeline anyway.
    fn request(&mut self, range: Range<usize>, ctx: &FetchCtx, sender: &CommandSender) {
        if self.pending.is_some() || self.halted {
            return;
        }
        let cap = self.total.unwrap_or(usize::MAX);
        let margin = MARGIN_PAGES * PAGE;
        let lo = range.start.saturating_sub(margin);
        let hi = range.end.saturating_add(margin).min(cap);

        if self.docs.is_empty() {
            if range.start == 0 {
                // Seed the browse: the first window also asks for the count.
                let want_total = self.total.is_none();
                self.issue(DocSeek::Forward { after: None }, want_total, ctx, sender);
            } else if !self.at_end {
                // `at_end` with no rows means a jump found nothing there (the
                // collection shrank under the estimate); don't re-jump every paint.
                self.issue(
                    DocSeek::Jump {
                        skip: range.start as u64,
                    },
                    self.total.is_none(),
                    ctx,
                    sender,
                );
            }
            return;
        }

        let run_start = self.anchor;
        let run_end = self.anchor + self.docs.len();

        // Far from the run: relocate it with one exact jump rather than chaining
        // keyset seeks across the gap.
        let jump_gap = JUMP_PAGES * PAGE;
        if range.start >= run_end + jump_gap || range.end + jump_gap <= run_start {
            self.issue(
                DocSeek::Jump {
                    skip: range.start as u64,
                },
                false,
                ctx,
                sender,
            );
            return;
        }

        let need_fwd = run_end < hi && !self.at_end;
        let need_back = run_start > lo && !self.at_start;
        // The direction still uncovered inside the viewport goes first; margins
        // prefetch after.
        let seek = if range.start < run_start && need_back {
            self.first_id().map(|before| DocSeek::Backward { before })
        } else if need_fwd {
            self.last_id()
                .map(|after| DocSeek::Forward { after: Some(after) })
        } else if need_back {
            self.first_id().map(|before| DocSeek::Backward { before })
        } else {
            None
        };
        if let Some(seek) = seek {
            self.issue(seek, false, ctx, sender);
        }
    }

    /// Land one `DocRunReady` reply. The echoed `seq` must be the in-flight one
    /// and (for an extension) the echoed boundary must still be the run's;
    /// eviction may have moved it since the request, in which case the reply is
    /// dropped and the next paint re-requests from the new boundary.
    pub(super) fn apply(
        &mut self,
        seek: DocSeek,
        docs: Vec<Document>,
        seq: u64,
        total: Option<usize>,
    ) {
        if self.pending != Some(seq) {
            return;
        }
        self.pending = None;
        if let Some(total) = total {
            self.total = Some(total);
        }
        let n = docs.len();
        let short = n < PAGE;
        match seek {
            DocSeek::Forward { after } => {
                if after != self.last_id() {
                    return;
                }
                if self.docs.is_empty() {
                    self.anchor = 0;
                    self.at_start = true;
                }
                self.docs.extend(docs);
                self.at_end = short;
                if short && let Some(total) = self.total {
                    // The run now touches the true last document, so its ordinals
                    // count back from the total.
                    self.anchor = total.saturating_sub(self.docs.len());
                }
            }
            DocSeek::Backward { before } => {
                if self.docs.is_empty() || Some(before) != self.first_id() {
                    return;
                }
                // The reply is ascending; pushing each to the front in reverse
                // restores order ahead of the run.
                for doc in docs.into_iter().rev() {
                    self.docs.push_front(doc);
                }
                self.anchor = self.anchor.saturating_sub(n);
                if short || self.anchor == 0 {
                    self.at_start = true;
                    self.anchor = 0;
                }
            }
            DocSeek::Jump { skip } => {
                self.docs = docs.into();
                self.at_end = short;
                let skip = skip as usize;
                self.anchor = match self.total {
                    Some(total) if short => total.saturating_sub(self.docs.len()),
                    Some(total) => skip.min(total.saturating_sub(self.docs.len())),
                    None => skip,
                };
                self.at_start = self.anchor == 0;
            }
        }
    }

    /// The in-flight fetch failed: free the slot so fetching can resume, but hold
    /// off until the viewport moves (see [`Self::halted`]).
    pub(super) fn run_failed(&mut self, seq: u64) {
        if self.pending == Some(seq) {
            self.pending = None;
            self.halted = true;
        }
    }

    /// Resolve (and possibly re-center) the virtual-scroll window for this frame.
    /// Recenters on the viewport when the scroll nears a window edge, compensating
    /// the list's pixel offset so the visible rows hold still, and returns the base
    /// and length to feed the list plus the scrollbar's fraction/thumb. Call once
    /// per render, before building the `Table`, so `row_count`, `window_base`, and
    /// the list's pixel offset all agree within the frame.
    pub(super) fn prepare_window(
        &self,
        scroll: &UniformListScrollHandle,
        row_height: Pixels,
    ) -> WindowView {
        let total = self.row_count();
        let rh = f32::from(row_height).max(1.0);

        let (offset_x, offset_y, viewport_h) = {
            let st = scroll.0.borrow();
            let off = st.base_handle.offset();
            let vh = st
                .last_item_size
                .map(|s| f32::from(s.item.height))
                .unwrap_or(0.0);
            (off.x, f32::from(off.y), vh)
        };
        let viewport_rows = (viewport_h / rh).ceil() as usize;

        let len = total.min(WINDOW);
        let local_first = (-offset_y / rh).round().max(0.0) as usize;
        let base_now = self.window_base.get().min(total.saturating_sub(len));
        let abs_first = base_now + local_first;

        let (base, reanchor) =
            window_decision(total, self.window_base.get(), local_first, viewport_rows);
        if let Some(new_local_first) = reanchor {
            let st = scroll.0.borrow();
            st.base_handle
                .set_offset(point(offset_x, px(-(new_local_first as f32 * rh))));
        }
        self.window_base.set(base);

        let (fraction, thumb) = scrollbar_metrics(total, abs_first, viewport_rows);
        WindowView {
            base,
            len,
            fraction,
            thumb,
        }
    }
}

/// Re-center the window on absolute ordinal `target` and set the list's pixel
/// offset directly (no `scroll_to_item`, which degenerates on a multi-million-row
/// f32 canvas). Used by the scrollbar scrub; the next paint's `ensure` turns the
/// far jump into one exact `skip` fetch.
pub(super) fn place_window(
    window_base: &Cell<usize>,
    scroll: &UniformListScrollHandle,
    total: usize,
    target: usize,
    row_height: f32,
) {
    let base = centered_base(total, target);
    window_base.set(base);
    let local = target - base;
    let st = scroll.0.borrow();
    let x = st.base_handle.offset().x;
    st.base_handle
        .set_offset(point(x, px(-(local as f32 * row_height))));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: i64) -> Document {
        Document {
            id: DocValue::Int64(id),
            fields: vec![("v".into(), DocValue::Int64(id))],
        }
    }

    fn docs(ids: impl IntoIterator<Item = i64>) -> Vec<Document> {
        ids.into_iter().map(doc).collect()
    }

    /// A window with its in-flight request marked, as `issue` would set it.
    fn pending(mut w: DocWindow, seq: u64) -> DocWindow {
        w.seq = seq;
        w.pending = Some(seq);
        w
    }

    const TOTAL: usize = 10_000;

    #[test]
    fn forward_from_start_is_exact() {
        let mut w = pending(DocWindow::new(), 1);
        w.apply(
            DocSeek::Forward { after: None },
            docs(1..=PAGE as i64),
            1,
            Some(TOTAL),
        );
        assert_eq!(w.anchor, 0);
        assert!(w.at_start && !w.at_end);
        assert_eq!(w.doc_at(0).unwrap().id, DocValue::Int64(1));
        assert_eq!(w.doc_at(PAGE - 1).unwrap().id, DocValue::Int64(PAGE as i64));
        assert_eq!(w.row_count(), TOTAL);

        // Extend forward from the boundary `_id`.
        w = pending(w, 2);
        let after = Some(DocValue::Int64(PAGE as i64));
        w.apply(
            DocSeek::Forward { after },
            docs(PAGE as i64 + 1..=2 * PAGE as i64),
            2,
            None,
        );
        assert_eq!(w.docs.len(), 2 * PAGE);
        assert_eq!(w.anchor, 0);
    }

    #[test]
    fn short_forward_pins_the_run_to_the_end() {
        let mut w = pending(DocWindow::new(), 1);
        w.total = Some(TOTAL);
        w.anchor = 9_950;
        w.docs = docs(9_951..=9_960).into();
        w = pending(w, 2);
        let after = Some(DocValue::Int64(9_960));
        w.apply(DocSeek::Forward { after }, docs(9_961..=9_995), 2, None);
        assert!(w.at_end);
        assert_eq!(w.anchor + w.docs.len(), TOTAL);
    }

    #[test]
    fn backward_prepends_ascending_rows_in_order() {
        let mut w = pending(DocWindow::new(), 1);
        w.total = Some(TOTAL);
        w.anchor = 500;
        w.docs = docs(501..=700).into();
        let before = DocValue::Int64(501);
        w.apply(
            DocSeek::Backward { before },
            docs(501 - PAGE as i64..=500),
            1,
            None,
        );
        assert_eq!(w.anchor, 500 - PAGE);
        assert_eq!(
            w.doc_at(500 - PAGE).unwrap().id,
            DocValue::Int64(501 - PAGE as i64)
        );
        // Contiguous across the seam.
        assert_eq!(w.doc_at(499).unwrap().id, DocValue::Int64(500));
        assert_eq!(w.doc_at(500).unwrap().id, DocValue::Int64(501));
    }

    #[test]
    fn jump_lands_at_the_exact_ordinal() {
        let mut w = pending(DocWindow::new(), 1);
        w.total = Some(TOTAL);
        w.docs = docs(1..=10).into();
        w.apply(
            DocSeek::Jump { skip: 6_700 },
            docs(6_701..=6_700 + PAGE as i64),
            1,
            None,
        );
        assert_eq!(w.anchor, 6_700);
        assert!(!w.at_start && !w.at_end);
        assert_eq!(w.doc_at(6_700).unwrap().id, DocValue::Int64(6_701));
    }

    #[test]
    fn stale_and_mismatched_replies_are_dropped() {
        let mut w = pending(DocWindow::new(), 2);
        w.docs = docs(1..=10).into();

        // Wrong seq: a reply for a superseded request.
        w.apply(DocSeek::Jump { skip: 50 }, docs(51..=60), 1, None);
        assert_eq!(w.anchor, 0);
        assert_eq!(w.docs.len(), 10);

        // Right seq but the boundary moved (eviction): dropped, slot freed.
        w = pending(w, 3);
        let after = Some(DocValue::Int64(999));
        w.apply(DocSeek::Forward { after }, docs(1_000..=1_004), 3, None);
        assert_eq!(w.docs.len(), 10);
        assert!(w.pending.is_none());
    }

    #[test]
    fn eviction_trims_the_run_and_forfeits_end_flags() {
        let mut w = DocWindow::new();
        w.total = Some(TOTAL);
        w.anchor = 0;
        w.at_start = true;
        w.docs = docs(1..=1000).into();
        w.evict(300, 800);
        assert_eq!(w.anchor, 300);
        assert_eq!(w.docs.len(), 500);
        assert!(!w.at_start && !w.at_end);
        assert_eq!(w.first_id(), Some(DocValue::Int64(301)));
        assert_eq!(w.last_id(), Some(DocValue::Int64(800)));
    }
}
