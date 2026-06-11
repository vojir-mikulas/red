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
use std::time::{Duration, Instant};

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
#[derive(Clone, Copy)]
pub struct Counters {
    /// Cumulative allocation events since process start.
    pub allocs: u64,
    /// Live bytes right now (allocated − freed).
    pub live_bytes: usize,
}

/// Snapshot the allocator counters.
pub fn snapshot() -> Counters {
    Counters {
        allocs: ALLOC_COUNT.load(Ordering::Relaxed),
        live_bytes: LIVE_BYTES.load(Ordering::Relaxed),
    }
}

// --- process RSS ----------------------------------------------------------

/// The process's resident set size in bytes — the true footprint (heap + GPU
/// driver + runtime + code), the thing `live_bytes` can't see. A syscall, so the
/// HUD samples it on a throttle, not every frame. `None` if the platform read
/// failed or isn't implemented.
#[cfg(target_os = "macos")]
pub fn rss_bytes() -> Option<usize> {
    use std::os::raw::{c_int, c_uint};

    // Minimal mach FFI: `task_info(MACH_TASK_BASIC_INFO)` yields `resident_size`.
    // Declared inline so the dev feature pulls in no new crate.
    type KernReturn = c_int;
    type MachPort = c_uint;

    #[repr(C)]
    struct TimeValue {
        seconds: c_int,
        microseconds: c_int,
    }
    #[repr(C)]
    struct MachTaskBasicInfo {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        user_time: TimeValue,
        system_time: TimeValue,
        policy: c_int,
        suspend_count: c_int,
    }

    const MACH_TASK_BASIC_INFO: c_uint = 20;
    const KERN_SUCCESS: KernReturn = 0;

    extern "C" {
        static mach_task_self_: MachPort;
        fn task_info(
            target_task: MachPort,
            flavor: c_uint,
            task_info_out: *mut c_int,
            task_info_out_cnt: *mut c_uint,
        ) -> KernReturn;
    }

    // SAFETY: `info` is zero-initialised plain-old-data sized for the flavor, and
    // `count` is set to its `natural_t` (u32) word count as the API requires.
    unsafe {
        let mut info: MachTaskBasicInfo = std::mem::zeroed();
        let mut count =
            (std::mem::size_of::<MachTaskBasicInfo>() / std::mem::size_of::<c_uint>()) as c_uint;
        let kr = task_info(
            mach_task_self_,
            MACH_TASK_BASIC_INFO,
            (&mut info as *mut MachTaskBasicInfo).cast(),
            &mut count,
        );
        (kr == KERN_SUCCESS).then_some(info.resident_size as usize)
    }
}

/// Linux: resident pages are field 2 of `/proc/self/statm`, in page units.
#[cfg(target_os = "linux")]
pub fn rss_bytes() -> Option<usize> {
    use std::os::raw::{c_int, c_long};
    extern "C" {
        fn sysconf(name: c_int) -> c_long;
    }
    const SC_PAGESIZE: c_int = 30;

    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: usize = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: `sysconf` is a pure query with no preconditions.
    let page = unsafe { sysconf(SC_PAGESIZE) };
    (page > 0).then(|| pages * page as usize)
}

/// Other platforms: not wired up (Windows would use `GetProcessMemoryInfo`).
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn rss_bytes() -> Option<usize> {
    None
}

// --- the HUD's per-frame collector ----------------------------------------

/// How many recent frames the build-time / allocs readouts average over, so the
/// numbers settle into a trend instead of strobing frame to frame.
const RING: usize = 120;

/// How often to re-read process RSS — a syscall, so it's throttled well below the
/// frame rate.
const RSS_INTERVAL: Duration = Duration::from_millis(250);

/// A tiny fixed-size ring of `f32` samples with a running average — the smoother
/// behind the build-ms and allocs/frame readouts.
struct Ring {
    samples: [f32; RING],
    len: usize,
    next: usize,
}

impl Ring {
    fn new() -> Self {
        Self {
            samples: [0.0; RING],
            len: 0,
            next: 0,
        }
    }

    fn push(&mut self, v: f32) {
        self.samples[self.next] = v;
        self.next = (self.next + 1) % RING;
        self.len = (self.len + 1).min(RING);
    }

    fn avg(&self) -> f32 {
        if self.len == 0 {
            return 0.0;
        }
        self.samples[..self.len].iter().sum::<f32>() / self.len as f32
    }
}

/// The per-frame perf collector behind the on-screen HUD. `begin_frame` /
/// `end_frame` bracket `AppState::render`; the getters feed the overlay panel.
/// Holds the previous frame's allocator snapshot and timestamps plus the
/// smoothing rings. Only ever touched on the GPUI main thread.
pub struct DevStats {
    /// Whether the overlay panel is currently shown (toggled by the dev keybind).
    visible: bool,
    frame_start: Instant,
    frame_allocs: u64,
    build_ms: Ring,
    allocs: Ring,
    live_bytes: usize,
    rss: Option<usize>,
    last_rss: Instant,
    last_render: Instant,
    /// Wall-clock gap since the previous render — the repaint cadence during
    /// interaction (meaningless while idle; see the plan's fps caveat).
    interval_ms: f32,
}

impl Default for DevStats {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            visible: false,
            frame_start: now,
            frame_allocs: snapshot().allocs,
            build_ms: Ring::new(),
            allocs: Ring::new(),
            live_bytes: 0,
            // Sample RSS once up front so the panel has a value on first show; it
            // then refreshes on `RSS_INTERVAL`.
            rss: rss_bytes(),
            last_rss: now,
            last_render: now,
            interval_ms: 0.0,
        }
    }
}

impl DevStats {
    /// Show/hide the overlay (the `cmd-alt-p` toggle).
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Mark the start of a render: capture the clock and the allocation count so
    /// `end_frame` can diff them.
    pub fn begin_frame(&mut self) {
        self.frame_start = Instant::now();
        self.frame_allocs = snapshot().allocs;
    }

    /// Close the frame: push this render's build time + allocation churn into the
    /// rings, refresh live bytes, and re-sample RSS on its throttle. The panel
    /// built this frame shows the rings *before* this push (a one-frame lag), so
    /// the HUD's own cost doesn't bias the number it's about to display.
    pub fn end_frame(&mut self) {
        let now = Instant::now();
        let snap = snapshot();
        self.build_ms
            .push(now.duration_since(self.frame_start).as_secs_f32() * 1000.0);
        self.allocs
            .push(snap.allocs.saturating_sub(self.frame_allocs) as f32);
        self.live_bytes = snap.live_bytes;
        self.interval_ms = now.duration_since(self.last_render).as_secs_f32() * 1000.0;
        self.last_render = now;
        if now.duration_since(self.last_rss) >= RSS_INTERVAL {
            self.rss = rss_bytes();
            self.last_rss = now;
        }
    }

    pub fn build_ms(&self) -> f32 {
        self.build_ms.avg()
    }

    pub fn allocs_per_frame(&self) -> f32 {
        self.allocs.avg()
    }

    pub fn live_bytes(&self) -> usize {
        self.live_bytes
    }

    pub fn rss(&self) -> Option<usize> {
        self.rss
    }

    pub fn interval_ms(&self) -> f32 {
        self.interval_ms
    }
}

/// A glance at the active result grid's footprint, for the HUD — confirms the
/// windowed buffer stays flat over a huge result.
pub struct GridSnapshot {
    /// Rows the buffer currently holds resident (bounded by the window margin).
    pub resident_rows: usize,
    /// Paging mode label: `"keyed"` or `"offset"`.
    pub mode: &'static str,
    /// Fetches in flight right now (keyed: 0/1; offset: queued page count).
    pub in_flight: usize,
    /// The open query's latest wall-clock time, in milliseconds.
    pub last_query_ms: f32,
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
