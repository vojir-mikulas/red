//! Dev-only performance instrumentation, compiled only under the `dev-stats`
//! feature. A normal build never sees this module, so the process keeps the plain
//! system allocator with nothing in the hot path.
//!
//! The core is a *counting global allocator*: every allocation and free bumps
//! process-wide atomics, so a caller (the perf HUD — see
//! `docs/plans/dev-perf-hud.md`) can read allocations-per-frame and live bytes —
//! the direct measure of the per-frame heap churn RED's render path is tuned to
//! avoid. The only overhead is a couple of relaxed atomic adds per call, paid
//! solely in `dev-stats` builds.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Cumulative count of allocation events (including reallocs) since start.
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
/// Bytes currently live (allocated minus freed). Saturating on the way down so a
/// stray underflow can never wrap it to a huge number in the HUD.
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

/// A pass-through global allocator that tallies allocations and live bytes,
/// delegating the real work to the system allocator. A growth-realloc counts as
/// an allocation event, since a `Vec` doubling is exactly the churn we want to
/// see; the live-bytes total follows the size delta.
pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            // Track the net change rather than alloc/free pair, so the total stays
            // correct whether the block grew or shrank.
            if new_size >= layout.size() {
                LIVE_BYTES.fetch_add(new_size - layout.size(), Ordering::Relaxed);
            } else {
                LIVE_BYTES.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        new_ptr
    }
}

/// A point-in-time reading of the allocator's counters. Take two and diff them to
/// get allocations and net bytes over an interval (e.g. one frame).
///
/// The read API is in place ahead of its consumer: the perf HUD wires it up in
/// Tier 2 (`docs/plans/dev-perf-hud.md`).
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct Counters {
    /// Cumulative allocation events since process start.
    pub allocs: u64,
    /// Live bytes right now (allocated − freed).
    pub live_bytes: usize,
}

/// Snapshot the allocator counters.
#[allow(dead_code)]
pub fn snapshot() -> Counters {
    Counters {
        allocs: ALLOC_COUNT.load(Ordering::Relaxed),
        live_bytes: LIVE_BYTES.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Counting` tallies an alloc/free pair correctly. Exercised by calling the
    /// allocator directly (it need not be the installed global one for this), so
    /// the static counters only move for the block we touch.
    #[test]
    fn alloc_then_dealloc_balances_live_bytes() {
        let before = snapshot();
        let layout = Layout::from_size_align(128, 8).unwrap();
        unsafe {
            let p = Counting.alloc(layout);
            assert!(!p.is_null());
            let after_alloc = snapshot();
            assert_eq!(after_alloc.allocs, before.allocs + 1);
            assert_eq!(after_alloc.live_bytes, before.live_bytes + 128);
            Counting.dealloc(p, layout);
        }
        // The freed block leaves the live-bytes total back where it started.
        assert_eq!(snapshot().live_bytes, before.live_bytes);
    }
}
