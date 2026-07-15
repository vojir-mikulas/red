//! The Redis keyspace browser (R1, see docs/plans/redis.md): a forward-only
//! list of `SCAN`ned keys with their type/TTL/size/encoding. Deliberately
//! its own thing, not built on the SQL result grid's `GridBuffer`
//! (`crate::result::buffer`) — that's tied to offset/keyset paging an
//! unordered keyspace doesn't have (see the plan's "grid needs a third
//! buffer mode" section). Reuses Flint's `Table` (a generic, domain-free
//! virtualized list on `uniform_list`, the same primitive the SQL grid sits
//! on) directly instead: a plain growing `Vec` is all the "buffer" this
//! needs, no windowing/eviction/margin machinery.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use flint::prelude::*;
use gpui::{
    App, AsyncApp, Context, Entity, FocusHandle, Focusable, Hsla, ScrollHandle, Subscription,
    UniformListScrollHandle, WeakEntity, Window, div, prelude::*, px, relative,
};
use red_core::kv::{
    KeyMeta, KvElement, KvType, KvValue, PendingEntry, ScanBudget, ScanCursor, StreamConsumer,
    StreamEntry, StreamGroup,
};
use red_service::{Command, SessionId};

use crate::app::{AppState, Phase, SplitHalf, SplitState, TabWorkspace, WorkspaceTab};
mod analysis;
mod inspector;
mod render;
mod tabs;
use render::render_string_preview;

/// The `SCAN ... COUNT` hint per round trip (default `10` is far too low for
/// a large keyspace; see docs/plans/redis.md item 3).
const SCAN_COUNT_HINT: u32 = 200;
/// Soft target page size (see `ScanBudget::want`).
const SCAN_WANT: usize = 150;
/// Wall-clock budget per `KvFetchScan` round trip, so a selective `MATCH`
/// pattern on a sparse keyspace can't block the UI thread waiting to fill a
/// page.
const SCAN_BUDGET_MS: u64 = 250;
/// Trigger a load-more once the visible range comes within this many rows of
/// the end of what's loaded.
const LOAD_AHEAD_ROWS: usize = 60;
/// Soft cap on resident rows (see docs/plans/redis.md's "grid needs a third
/// buffer mode": append-only, evict-oldest beyond a cap, since revisiting an
/// evicted row means re-scanning anyway). A very long unfiltered browse
/// session shouldn't grow this list forever.
const MAX_RESIDENT_ROWS: usize = 20_000;
/// How long to wait after the last keystroke before restarting the scan with
/// the typed pattern, so a fast typist doesn't fire one `KvFetchScan` per
/// character. Enter (`TextInputEvent::Submit`) bypasses this and restarts
/// immediately.
const FILTER_DEBOUNCE_MS: u64 = 300;
/// A big list's inspector preview is a single static head window, not an
/// infinite scroll (lists have no `LSCAN`; see docs/plans/redis.md's
/// documented limitation on deep-middle list access).
const LIST_PREVIEW_COUNT: usize = 200;
/// How many stream entries to pull per `KvReadStreamPage` round trip. Unlike
/// a list, a big stream *is* pageable (by entry-ID range, newest-first), so
/// this is a page size the inspector grows on scroll, not a one-shot cap.
const STREAM_PAGE_COUNT: usize = 200;
/// How many pending entries to pull per group in the consumer-group view
/// (`XPENDING ... - + count`). A bounded window, not the whole PEL: a group
/// with a huge backlog still surfaces its head (the oldest, most-stuck
/// entries) without an unbounded fetch, matching the size-triage the rest of
/// the inspector uses.
const STREAM_PENDING_COUNT: usize = 100;

fn scan_budget() -> ScanBudget {
    ScanBudget {
        count_hint: SCAN_COUNT_HINT,
        wall_clock: Duration::from_millis(SCAN_BUDGET_MS),
        want: SCAN_WANT,
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

/// Escape Redis glob metacharacters (`* ? [ ] \`) so a piece of user text
/// matches *literally* inside a `SCAN … MATCH` pattern. Used by
/// [`QueryMode::Prefix`], where the box text is a literal prefix rather than a
/// glob the user wrote themselves.
fn glob_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// How the Browse filter box's text is interpreted — the query-mode dropdown at
/// the head of the filter bar (replaces the old separate fuzzy / value-search
/// toggles). `Glob` and `Prefix` push down to `SCAN … MATCH`; `Exact` resolves a
/// single key via `probe_key` (no scan at all); `Fuzzy` and `Value` filter
/// what the scan loads (see [`BrowseState::mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryMode {
    /// The box text is a raw `SCAN … MATCH` glob the user writes (`user:*`).
    /// The default, and how the filter box always behaved before query modes.
    Glob,
    /// The box text is a literal prefix; scanned as `MATCH <escaped>*` so glob
    /// metacharacters in it match literally.
    Prefix,
    /// The box text is one exact key name, resolved directly by `probe_key`
    /// (bypasses `SCAN`) — the list shows the single hit, or nothing.
    Exact,
    /// Client-side fuzzy match over loaded keys, auto-growing the scan pool
    /// while under-matched (see `kv_maybe_grow_pool`).
    Fuzzy,
    /// Substring search over string *values* (the driver reads scanned string
    /// values); runs on Enter, not per keystroke.
    Value,
}

impl QueryMode {
    /// The modes in dropdown order.
    const ALL: [QueryMode; 5] = [
        QueryMode::Glob,
        QueryMode::Prefix,
        QueryMode::Exact,
        QueryMode::Fuzzy,
        QueryMode::Value,
    ];

    /// The dropdown label.
    fn label(self) -> &'static str {
        match self {
            QueryMode::Glob => "Glob (*)",
            QueryMode::Prefix => "Prefix",
            QueryMode::Exact => "Exact",
            QueryMode::Fuzzy => "Fuzzy",
            QueryMode::Value => "Value",
        }
    }

    /// The filter box's placeholder for this mode (nudges the user to what the
    /// text now means).
    fn placeholder(self) -> &'static str {
        match self {
            QueryMode::Glob => "Filter (MATCH pattern)…",
            QueryMode::Prefix => "Key prefix…",
            QueryMode::Exact => "Exact key name…",
            QueryMode::Fuzzy => "Fuzzy search keys…",
            QueryMode::Value => "Search values (Enter)…",
        }
    }

    /// Build the server-side `SCAN … MATCH` pattern for this mode from the box
    /// text. `None` for an empty box, or for modes that don't scan by pattern
    /// (`Exact` probes; `Fuzzy`/`Value` filter what a plain scan loads).
    fn scan_pattern(self, text: &str) -> Option<String> {
        if text.is_empty() {
            return None;
        }
        match self {
            QueryMode::Glob => Some(text.to_string()),
            QueryMode::Prefix => Some(format!("{}*", glob_escape(text))),
            QueryMode::Exact | QueryMode::Fuzzy | QueryMode::Value => None,
        }
    }
}

/// The concrete Redis types the browse type-filter dropdown offers, in menu
/// order after the leading "All types" entry. Each maps to a `SCAN ... TYPE`
/// argument via [`KvType::label`]. `Other` is deliberately absent — the
/// dropdown is a fixed picker, not a reflection of what's in the keyspace.
fn kv_filter_types() -> [KvType; 6] {
    [
        KvType::String,
        KvType::Hash,
        KvType::List,
        KvType::Set,
        KvType::ZSet,
        KvType::Stream,
    ]
}

/// A client-side filter on a loaded key's remaining TTL (the browse toolbar's
/// `TTL ▾` dropdown). Redis's `SCAN` can't filter by expiry, so — unlike the
/// server-side type/`MATCH` filters — this is a predicate applied to the rows
/// already pulled into the resident window (see [`BrowseState::visible_rows`]).
/// Every scanned key already carries its `PTTL` (fetched in the same pipeline as
/// its type/encoding/size), so the filter reads data that's already present and
/// costs nothing extra server-side. `Permanent` matches keys with no expiry
/// (`ttl == None`); the bucket variants match a finite TTL under/over a bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TtlFilter {
    /// No expiry set (`PERSIST`ed / never-expiring keys).
    Permanent,
    /// Expiring in 3 minutes or less — "ending soon".
    EndingSoon,
    /// A finite TTL under one hour.
    UnderHour,
    /// A finite TTL under one day.
    UnderDay,
    /// A finite TTL under one week.
    UnderWeek,
    /// A finite TTL of a week or more.
    OverWeek,
}

impl TtlFilter {
    /// The dropdown entries after the leading "Any TTL", in menu order.
    pub(crate) const ALL: [TtlFilter; 6] = [
        TtlFilter::Permanent,
        TtlFilter::EndingSoon,
        TtlFilter::UnderHour,
        TtlFilter::UnderDay,
        TtlFilter::UnderWeek,
        TtlFilter::OverWeek,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            TtlFilter::Permanent => "Permanent",
            TtlFilter::EndingSoon => "Ending ≤ 3 min",
            TtlFilter::UnderHour => "TTL < 1 hour",
            TtlFilter::UnderDay => "TTL < 1 day",
            TtlFilter::UnderWeek => "TTL < 1 week",
            TtlFilter::OverWeek => "TTL ≥ 1 week",
        }
    }

    /// Whether a row with this remaining TTL (`None` = no expiry) passes.
    fn matches(self, ttl: Option<Duration>) -> bool {
        const HOUR: Duration = Duration::from_secs(3_600);
        const DAY: Duration = Duration::from_secs(86_400);
        const WEEK: Duration = Duration::from_secs(604_800);
        const SOON: Duration = Duration::from_secs(180);
        match (self, ttl) {
            (TtlFilter::Permanent, ttl) => ttl.is_none(),
            // Every bucket below is about a *finite* expiry, so a permanent key
            // (no TTL) never matches them.
            (_, None) => false,
            (TtlFilter::EndingSoon, Some(t)) => t <= SOON,
            (TtlFilter::UnderHour, Some(t)) => t < HOUR,
            (TtlFilter::UnderDay, Some(t)) => t < DAY,
            (TtlFilter::UnderWeek, Some(t)) => t < WEEK,
            (TtlFilter::OverWeek, Some(t)) => t >= WEEK,
        }
    }
}

/// The kind of a Redis tab: what the `+` new-tab picker offers and what a
/// [`RedisTabState`] holds. Unlike the SQL side (every tab is a homogeneous
/// query editor), Redis tabs are heterogeneous, so the kind is an explicit
/// discriminant used for the picker labels and default titles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KvPanel {
    Browse,
    Console,
    PubSub,
    Monitor,
    Analysis,
    Keyspace,
}

impl KvPanel {
    /// The picker label + default tab title for this kind.
    pub(crate) fn label(self) -> &'static str {
        match self {
            KvPanel::Browse => "Browse",
            KvPanel::Console => "Console",
            KvPanel::PubSub => "Pub/Sub",
            KvPanel::Monitor => "Monitor",
            KvPanel::Analysis => "Analysis",
            KvPanel::Keyspace => "Keyspace",
        }
    }
}

/// The blank-tab chooser's panels, in display order: the shared source of truth
/// for the cards' layout and their `1`–`6` number shortcuts (see
/// `render_kv_new_tab` and [`AppState::kv_new_tab_key`]).
pub(crate) const KV_NEW_TAB_CHOICES: [(KvPanel, &str); 6] = [
    (KvPanel::Browse, "Scan and inspect keys"),
    (KvPanel::Console, "Run raw commands (redis-cli)"),
    (KvPanel::PubSub, "Watch published messages"),
    (KvPanel::Monitor, "Slow log · live MONITOR · clients"),
    (KvPanel::Analysis, "Keyspace memory/TTL report"),
    (KvPanel::Keyspace, "Live keyspace notifications"),
];

/// A `redis-cli --bigkeys`-style sample (see docs/plans/redis.md's "beyond
/// R4" list): an extended, bounded keyspace walk collecting metadata for
/// every key it passes (reusing the same pipelined `scan_keys` the live
/// browse uses), sorted by `approx_bytes` once the sample completes. Opt-in
/// and separate from the live browse grid, since it implies a full-ish
/// keyspace walk rather than the live browse's "just enough to fill the
/// viewport" pull.
pub(crate) struct BigKeysState {
    /// A dedicated epoch, distinct from the browse's own: this is a
    /// different scan run entirely, not a continuation of the live browse,
    /// and must not have its pages misfiled into `RedisBrowse::rows`.
    pub(crate) epoch: red_service::Epoch,
    pub(crate) cursor: ScanCursor,
    pub(crate) sampled: usize,
    pub(crate) running: bool,
    pub(crate) started: std::time::Instant,
    /// Every key seen so far this sample, unsorted until it completes, then
    /// sorted descending by `approx_bytes` and truncated to the top N.
    pub(crate) results: Vec<KeyMeta>,
}

/// Sample budget: stop once either bound is hit, whichever first. Generous
/// but bounded, since this is an explicit opt-in action the user asked for,
/// not something that runs by default.
const BIG_KEYS_SAMPLE_CAP: usize = 50_000;
const BIG_KEYS_SAMPLE_MS: u64 = 5_000;
/// How many of the biggest keys to keep and show once the sample completes.
const BIG_KEYS_TOP_N: usize = 200;

/// The analysis sample's own budget, more generous than the biggest-keys
/// sampler's since a diagnostic report wants breadth: a bigger, slightly
/// longer walk, still bounded so it can't run away on a huge keyspace. The
/// report records whether it hit a bound (`truncated`) so the UI can say so.
const ANALYSIS_SAMPLE_CAP: usize = 200_000;
const ANALYSIS_SAMPLE_MS: u64 = 12_000;

/// One connection's keyspace-analysis run + the report it's showing (see
/// docs/plans/redis.md's "persistent database analysis report" gap). The scan
/// reuses the biggest-keys sampler's chained `KvFetchScan` loop, but rolls the
/// collected metadata up into a persisted `RedisAnalysis` instead of a
/// biggest-keys list. `report` is `None` until either a run finishes or a saved
/// report is loaded for the connection.
pub(crate) struct AnalysisState {
    /// A dedicated scan epoch, distinct from the browse/big-keys epochs.
    pub(crate) epoch: red_service::Epoch,
    pub(crate) cursor: ScanCursor,
    pub(crate) running: bool,
    pub(crate) started: std::time::Instant,
    /// Keys collected so far this run (rolled up once it finishes; not kept
    /// after, to avoid holding a whole sample resident indefinitely).
    pub(crate) collected: Vec<KeyMeta>,
    /// The report on screen: the just-finished run's, or the one restored from
    /// disk when the panel opened. Persisted across restarts (see
    /// `redis_analysis.rs`).
    pub(crate) report: Option<red_core::kv::RedisAnalysis>,
    /// Set once a persisted report has been looked up for this connection, so
    /// reopening the panel doesn't re-read the store every time.
    pub(crate) loaded: bool,
}

/// One keyspace-browse tab's state (a `RedisTabState::Browse`). Everything
/// here is per-tab: two Browse tabs each have their own scan run, filter,
/// and inspector. Connection-level state (like `DBSIZE`) lives on
/// [`RedisView`] instead.
pub(crate) struct BrowseState {
    /// Identifies the current scan run (bumped on restart — a new filter
    /// pattern — exactly like a SQL result's epoch bumps on re-sort). A
    /// `KvScanPage` reply whose epoch doesn't match is from a superseded
    /// scan and is dropped. Also the stable per-tab identity backend events
    /// route by (see [`RedisView::browse_by_scan_epoch_mut`]).
    pub(crate) epoch: red_service::Epoch,
    /// The pattern the current scan run applies (`None` = unfiltered `*`).
    pub(crate) pattern: Option<String>,
    /// The Redis type the current scan run restricts to (`None` = all types),
    /// pushed down to `SCAN ... TYPE` server-side (see the type-filter
    /// dropdown in `render_kv_browse`). Composes with `pattern` and with the
    /// client-side `fuzzy` filter. Only ever one of the six concrete
    /// [`KvType`] variants — the dropdown never offers `Other`.
    pub(crate) type_filter: Option<KvType>,
    /// Whether the type-filter dropdown is showing its option list. Per-tab,
    /// owned here because Flint's `Select` is stateless (the caller holds the
    /// open flag).
    pub(crate) type_filter_open: bool,
    /// A client-side remaining-TTL filter over the loaded rows (`None` = any
    /// TTL). Unlike `type_filter`/`pattern` it is *not* pushed down to `SCAN`
    /// (Redis can't filter by expiry), so it narrows the resident window at
    /// render time — see [`BrowseState::visible_rows`] and the `TTL ▾` dropdown
    /// in `render_kv_browse`. Composes with every other filter.
    pub(crate) ttl_filter: Option<TtlFilter>,
    /// Whether the TTL-filter dropdown is showing its option list (Flint's
    /// `Select` is stateless, so the open flag lives here, like `type_filter_open`).
    pub(crate) ttl_filter_open: bool,
    /// Show only favourited (starred) keys. Client-side over the loaded window,
    /// like `ttl_filter`; membership comes from [`crate::key_meta::KeyMetaStore`]
    /// (on `AppState`, not reachable here), so it's snapshot into `fav_set`.
    pub(crate) fav_only: bool,
    /// Restrict to keys carrying this tag (`None` = any tag). Client-side; the
    /// matching keys are snapshot into `tag_set`.
    pub(crate) tag_filter: Option<String>,
    /// Whether the tag-filter dropdown is showing its option list.
    pub(crate) tag_filter_open: bool,
    /// Snapshot of the connection's favourite keys while `fav_only` is on (empty
    /// otherwise). Refreshed by [`AppState::kv_refresh_meta_snapshots`] when the
    /// filter or the annotation store changes; `meta_gen` bumps alongside so
    /// `visible_rows`' cache invalidates.
    fav_set: Rc<HashSet<String>>,
    /// Snapshot of the keys matching `tag_filter` (empty when no tag filter).
    tag_set: Rc<HashSet<String>>,
    /// Bumped whenever `fav_set`/`tag_set` change, so the `visible_rows` cache
    /// (keyed partly on this) recomputes.
    meta_gen: u64,
    /// Rows accumulated this run, forward-only, oldest-evicted past the cap.
    /// Held behind an `Rc` so the per-frame render (and keyboard nav) share the
    /// buffer by a refcount bump instead of deep-cloning up to `MAX_RESIDENT_ROWS`
    /// `KeyMeta` every frame; mutate only via [`BrowseState::rows_mut`].
    pub(crate) rows: Rc<Vec<KeyMeta>>,
    /// Bumped on every mutation of `rows` (via [`BrowseState::rows_mut`]) so the
    /// fuzzy `visible_rows` cache can tell when its scored subset is stale.
    rows_gen: u64,
    /// Memoized result of the fuzzy `visible_rows` computation, valid while the
    /// query and `rows_gen` are unchanged — so an unrelated re-render doesn't
    /// re-score and re-sort every loaded key.
    visible_cache: RefCell<Option<VisibleRowsCache>>,
    pub(crate) cursor: ScanCursor,
    pub(crate) exhausted: bool,
    pub(crate) loading: bool,
    pub(crate) scroll: UniformListScrollHandle,
    /// Focus for the key table, so ↑/↓/PageUp-Down/Home-End/Enter drive the
    /// keyboard cursor (see [`AppState::kv_browse_nav`]). Focused when a row is
    /// clicked; the table installs its own key handler while focused.
    pub(crate) list_focus: FocusHandle,
    /// The keyboard cursor: an index into the *currently visible* rows (see
    /// [`BrowseState::visible_rows`]), or `None` before any keyboard/click
    /// interaction, in which case the highlight falls back to the inspector's
    /// key. Distinct from `cursor` (the `SCAN` cursor) despite the name.
    pub(crate) nav_row: Option<usize>,
    pub(crate) filter: Entity<TextInput>,
    /// Bumped on every `Change`; a debounce timer captures the value live at
    /// the time it was scheduled and only restarts the scan if it's still
    /// current when the timer fires, so rapid typing coalesces into one
    /// backend round trip (see `AppState::kv_debounce_filter`).
    pub(crate) filter_gen: u64,
    /// How the filter box's text is interpreted (the query-mode dropdown at the
    /// head of the filter bar). `Glob`/`Prefix` push down to `SCAN … MATCH`;
    /// `Exact` probes a single key; `Fuzzy` filters loaded rows client-side and
    /// auto-grows the pool (`kv_maybe_grow_pool`); `Value` reads scanned
    /// string values. Switching mode re-applies the box text under the new
    /// meaning (see `kv_set_query_mode`).
    pub(crate) mode: QueryMode,
    /// Whether the query-mode dropdown is showing its option list (Flint's
    /// `Select` is stateless, so the open flag lives here, like `type_filter_open`).
    pub(crate) mode_open: bool,
    /// The value inspector opened by selecting a row, if any.
    pub(crate) inspector: Option<KvInspector>,
    /// `Some` while a "find biggest keys" sample is running or showing its
    /// last result; `None` is the normal live-browse state.
    pub(crate) big_keys: Option<BigKeysState>,
    /// `Some` while the "New key" popover is open (see `kv_open_create_key`).
    pub(crate) create_key: Option<CreateKeyState>,
    /// Set once the resident-row cap (`MAX_RESIDENT_ROWS`) has evicted the
    /// oldest scanned keys, so the header can say the view is windowed rather
    /// than silently dropping keys off the top.
    pub(crate) evicted: bool,
    /// `true`: render the loaded keys as a collapsible namespace tree (grouped
    /// by the `:` delimiter, Redis's near-universal key hierarchy) instead of
    /// the flat grid. A per-tab view toggle; keyboard nav is grid-only, so the
    /// tree relies on click/scroll.
    pub(crate) tree_mode: bool,
    /// The namespace prefixes currently expanded in tree mode (full `a:b:c`
    /// paths). A prefix absent here is collapsed. Kept across a mode toggle so
    /// switching to grid and back preserves the open branches.
    pub(crate) expanded: HashSet<String>,
    /// Bumped on every expand/collapse so the flattened-tree cache
    /// ([`BrowseState::tree_rows`]) knows to rebuild.
    expand_gen: u64,
    /// Memoized flattened tree, valid while the row buffer and `expand_gen` are
    /// unchanged — building the trie over up to `MAX_RESIDENT_ROWS` keys every
    /// frame would be wasteful (mirrors the fuzzy `visible_cache`).
    tree_cache: RefCell<Option<TreeCache>>,
    /// The active value-search needle (only ever set in [`QueryMode::Value`]),
    /// threaded into every `KvFetchScan` of the current run so pagination keeps
    /// filtering. `None` = no value search.
    pub(crate) value_needle: Option<String>,
    /// Auto-refresh interval: while `Some`, a background timer re-runs this tab's
    /// scan every interval (see `kv_arm_auto_refresh`). `None` = off. Seeded from
    /// the `redis.auto_refresh_secs` setting when the tab opens, and changed via
    /// the browse toolbar's actions menu.
    pub(crate) auto_refresh: Option<Duration>,
    /// Bumped whenever the auto-refresh interval changes, so an in-flight timer
    /// tick from a superseded interval no-ops instead of firing (the debounce
    /// generation-check shape, per `kv_debounce_filter`).
    auto_refresh_gen: u64,
}

/// The inputs a browse relaunch dispatches, produced by
/// [`BrowseState::begin_relaunch`] and consumed by
/// [`AppState::kv_dispatch_relaunch`].
struct RelaunchPlan {
    old_epoch: red_service::Epoch,
    new_epoch: red_service::Epoch,
    pattern: Option<String>,
    type_filter: Option<KvType>,
    value_needle: Option<String>,
}

impl BrowseState {
    /// Whether the filter box is in client-side fuzzy mode.
    pub(crate) fn is_fuzzy(&self) -> bool {
        self.mode == QueryMode::Fuzzy
    }

    /// Reset this browse to a fresh, empty run under a new epoch and return the
    /// [`RelaunchPlan`] to re-dispatch its scan with the current filters. Split
    /// from the command send so the caller can drop the `&mut` borrow first.
    fn begin_relaunch(&mut self) -> RelaunchPlan {
        let old_epoch = self.epoch;
        let new_epoch = crate::result::next_kv_epoch();
        self.epoch = new_epoch;
        self.rows_mut().clear();
        self.evicted = false;
        self.cursor = ScanCursor::START;
        self.exhausted = false;
        self.loading = true;
        RelaunchPlan {
            old_epoch,
            new_epoch,
            pattern: self.pattern.clone(),
            type_filter: self.type_filter.clone(),
            value_needle: self.value_needle.clone(),
        }
    }
}

/// The "New key" modal's state: a name, the chosen type, and the per-type seed
/// inputs the form shows/hides as the type changes (a hash/stream field, a zset
/// score, a string TTL, a list push end). A new key is created by its first
/// write — `SET`/`HSET`/`RPUSH`/`SADD`/`ZADD`/`XADD` — so this reuses the same
/// [`KvEdit`](red_core::kv::KvEdit) path as element editing.
pub(crate) struct CreateKeyState {
    pub(crate) name: Entity<TextInput>,
    /// Hash/stream field name (shown when `kv_type` is `Hash` or `Stream`).
    pub(crate) field: Entity<TextInput>,
    /// The value, set member, list element, or zset member.
    pub(crate) value: Entity<TextInput>,
    /// ZSet score (shown when `kv_type` is `ZSet`).
    pub(crate) score: Entity<TextInput>,
    /// Optional expiry in seconds for a new string (shown when `kv_type` is
    /// `String`); blank = no TTL.
    pub(crate) ttl: Entity<TextInput>,
    pub(crate) kv_type: KvType,
    /// List push end: `true` = `LPUSH` (head), `false` = `RPUSH` (tail). Only
    /// meaningful when `kv_type` is `List`.
    pub(crate) list_head: bool,
    pub(crate) error: Option<String>,
}

/// The key types the "New key" popover can create, in menu order. A Stream is
/// created by its first `XADD` (see [`KvEdit::StreamAdd`](red_core::kv::KvEdit));
/// `Other` (module types) still can't be created here.
fn kv_creatable_types() -> [KvType; 6] {
    [
        KvType::String,
        KvType::Hash,
        KvType::List,
        KvType::Set,
        KvType::ZSet,
        KvType::Stream,
    ]
}

/// One entry in a connection's "recently viewed keys" list — browser-history
/// for the keyspace (see docs/plans/redis-workflow-parity.md Part 2). In-memory,
/// newest-first, capped; recorded whenever the inspector opens on a key.
pub(crate) struct RecentKey {
    pub(crate) key: String,
    pub(crate) kv_type: KvType,
    pub(crate) ttl: Option<Duration>,
    pub(crate) viewed_unix: u64,
}

impl RecentKey {
    /// The serde-friendly persisted form (see `recent_keys.rs`): type as its
    /// label, TTL as whole seconds.
    fn to_rec(&self) -> crate::recent_keys::RecentKeyRec {
        crate::recent_keys::RecentKeyRec {
            key: self.key.clone(),
            kv_type: self.kv_type.label().to_string(),
            ttl_secs: self.ttl.map(|d| d.as_secs()),
            viewed_unix: self.viewed_unix,
        }
    }

    /// Rebuild from the persisted form; an unknown type label round-trips as
    /// `KvType::Other` rather than being dropped.
    fn from_rec(rec: &crate::recent_keys::RecentKeyRec) -> Self {
        RecentKey {
            key: rec.key.clone(),
            kv_type: KvType::parse(&rec.kv_type)
                .unwrap_or_else(|| KvType::Other(rec.kv_type.clone())),
            ttl: rec.ttl_secs.map(Duration::from_secs),
            viewed_unix: rec.viewed_unix,
        }
    }
}

/// How many recently-viewed keys to retain per connection.
const MAX_RECENT_KEYS: usize = 50;

/// The per-kind state a Redis tab holds. Heterogeneous, unlike the SQL side's
/// homogeneous `QueryTab` — a Browse tab and a Monitor tab are structurally
/// different, so the tab wraps this enum (see docs/plans/redis-workflow-parity.md).
pub(crate) enum RedisTabState {
    /// A blank tab awaiting a kind choice: its body shows the type chooser
    /// (mirrors the SQL side's blank query tab). Picking a kind converts it in
    /// place via [`AppState::kv_set_tab_kind`].
    Empty,
    // Boxed: `BrowseState` (grid rows + inspector + editors) dwarfs the other
    // variants, so an unboxed enum would size every tab to the biggest.
    Browse(Box<BrowseState>),
    Console(crate::kvconsole::KvConsole),
    PubSub(crate::kvpubsub::KvPubSub),
    Monitor(crate::kvmonitor::KvMonitor),
    Keyspace(crate::kvkeyspace::KvKeyspace),
    Analysis(AnalysisState),
}

impl RedisTabState {
    /// The panel kind, or `None` for a not-yet-chosen [`RedisTabState::Empty`].
    pub(crate) fn kind(&self) -> Option<KvPanel> {
        match self {
            RedisTabState::Empty => None,
            RedisTabState::Browse(_) => Some(KvPanel::Browse),
            RedisTabState::Console(_) => Some(KvPanel::Console),
            RedisTabState::PubSub(_) => Some(KvPanel::PubSub),
            RedisTabState::Monitor(_) => Some(KvPanel::Monitor),
            RedisTabState::Keyspace(_) => Some(KvPanel::Keyspace),
            RedisTabState::Analysis(_) => Some(KvPanel::Analysis),
        }
    }
}

/// One tab in the Redis shell: a title, a stable id, and its per-kind state.
pub(crate) struct RedisTab {
    /// Stable identity, never reused, assigned from [`RedisView::tab_seq`].
    /// Used to address a tab across closes/reorders (an index would shift).
    pub(crate) id: u64,
    pub(crate) title: String,
    pub(crate) state: RedisTabState,
    /// Which split half this tab belongs to (mirrors the SQL side). Always
    /// `Primary` in the single-pane layout.
    pub(crate) pane: SplitHalf,
    /// Pinned tabs sort ahead of the rest in their half's strip.
    pub(crate) pinned: bool,
}

/// One Redis connection's whole view: a dynamic, spawnable/closeable set of
/// tabs (mirrors the SQL side's `Vec<QueryTab>` on `ActiveConn`). Lives on
/// `ActiveConn` for a Redis session only (`None` for a SQL one).
pub(crate) struct RedisView {
    pub(crate) tabs: Vec<RedisTab>,
    /// Index into `tabs` of the visible tab. Kept in range by every close.
    pub(crate) active_tab: usize,
    /// Monotonic id source for `RedisTab::id`.
    pub(crate) tab_seq: u64,
    /// `DBSIZE`, fetched once at connect (connection-level, shared by every
    /// Browse tab — see docs/plans/redis.md on why there's no cheap filtered
    /// count).
    pub(crate) db_size: Option<u64>,
    /// Recently-viewed keys, newest-first (browser-history for the keyspace),
    /// shown in the History dock's Keys section.
    pub(crate) recent_keys: Vec<RecentKey>,
    /// Horizontal scroll for the tab strip (mirrors the SQL `ActiveConn::tab_scroll`).
    pub(crate) tab_scroll: ScrollHandle,
    /// The gap a dragged tab would land in during a reorder, or `None`.
    pub(crate) tab_drop_target: Option<usize>,
    /// The side-by-side split (reuses the SQL side's [`SplitState`]); `None` is
    /// the ordinary single-pane layout. `active_tab` is the Primary half's
    /// active tab; `split.secondary` the Secondary half's. See
    /// docs/plans/redis-workflow-parity.md Part 3 Phase 2.
    pub(crate) split: Option<SplitState>,
    /// The tab whose right-click context menu is open, as `(id, position)`.
    pub(crate) tab_menu: Option<(u64, gpui::Point<gpui::Pixels>)>,
    /// The key whose right-click context menu is open (from either the live
    /// browse list or the biggest-keys sample), anchored at the click position.
    pub(crate) key_menu: Option<KeyMenu>,
    /// The "Note & tags" annotation editor, when open (see
    /// [`AppState::kv_open_annotations`]). Connection-level like `key_menu`.
    pub(crate) annotate: Option<AnnotateState>,
    /// The browse toolbar's actions dropdown (Refresh · Expand/Collapse all ·
    /// Auto-refresh), anchored at the trigger's position while open. `None` =
    /// closed. Mirrors the `tab_menu`/`key_menu` positioned-menu pattern.
    pub(crate) actions_menu: Option<gpui::Point<gpui::Pixels>>,
    /// The browse toolbar's auto-refresh interval popover (Off · 2/5/10/30s),
    /// anchored at the disclosure caret while open. `None` = closed. Mirrors the
    /// `actions_menu` positioned-menu pattern; the interval lives per-tab on
    /// [`BrowseState::auto_refresh`].
    pub(crate) auto_menu: Option<gpui::Point<gpui::Pixels>>,
    /// The "Import keys" modal, when open (see [`AppState::kv_open_import`]).
    /// Connection-level (imports into the current DB), like `annotate`.
    pub(crate) import: Option<ImportState>,
    /// Focus + highlighted choice for the blank-tab panel chooser, so it's
    /// keyboard-drivable (1–6 / arrows / Enter). One handle is enough: only the
    /// focused half's chooser binds it (see `render_kv_new_tab`).
    pub(crate) new_tab_focus: FocusHandle,
    pub(crate) new_tab_sel: usize,
}

/// The open key context menu: which key it targets (with the type/TTL captured
/// at right-click time so the menu can label itself and open the inspector
/// without a re-lookup) and where to anchor it. Mirrors the `tab_menu`
/// `(id, pos)` pattern one level richer.
pub(crate) struct KeyMenu {
    pub(crate) key: String,
    pub(crate) kv_type: KvType,
    pub(crate) ttl: Option<Duration>,
    pub(crate) pos: gpui::Point<gpui::Pixels>,
}

/// Which inline inspector editor a key-menu item drives (see
/// [`AppState::kv_key_menu_edit`]). Delete is not here — it goes through the
/// confirmation modal ([`AppState::kv_request_delete_key`]) instead.
#[derive(Clone, Copy)]
pub(crate) enum KeyMenuEdit {
    Rename,
    Ttl,
}

/// The "Import keys" modal's state (see [`AppState::kv_open_import`]): the chosen
/// file and its parsed commands, or a parse/read error to show inline. Commands
/// are tokenized up front so the modal can show a count and the Import button
/// stays disabled until there's something to run.
pub(crate) struct ImportState {
    /// The chosen file's display path, or `None` before one is picked.
    pub(crate) path: Option<String>,
    /// The tokenized commands to run (blank/`#`-comment lines dropped).
    pub(crate) commands: Vec<Vec<String>>,
    /// A read/parse problem to surface inline (e.g. unreadable file, no commands).
    pub(crate) error: Option<String>,
    /// True once the import is in flight (buttons disabled until `KvImportDone`).
    pub(crate) running: bool,
}

/// The open "Note & tags" annotation editor (see
/// [`AppState::kv_open_annotations`]): the key it targets and two inputs — a
/// free-text note and a comma-separated tag list — seeded from the saved
/// annotation and written back to [`crate::key_meta::KeyMetaStore`] on save.
pub(crate) struct AnnotateState {
    pub(crate) key: String,
    pub(crate) note: Entity<TextInput>,
    pub(crate) tags: Entity<TextInput>,
}

/// Quote a Redis key for a redis-cli command line: bare when it is a simple
/// token, otherwise double-quoted with `"` and `\` escaped (redis-cli's own
/// quoting rules). Only used to seed the Console, which the user still reviews
/// before running.
fn quote_redis_arg(arg: &str) -> String {
    let simple = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-' | b'.' | b'/'));
    if simple {
        arg.to_string()
    } else {
        format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// The natural "read the whole value" command to pre-fill the Console with for a
/// key of the given type — a safe probe the user still runs manually.
fn kv_read_command(kv_type: &KvType, key: &str) -> String {
    let q = quote_redis_arg(key);
    match kv_type {
        KvType::String => format!("GET {q}"),
        KvType::Hash => format!("HGETALL {q}"),
        KvType::List => format!("LRANGE {q} 0 -1"),
        KvType::Set => format!("SMEMBERS {q}"),
        KvType::ZSet => format!("ZRANGE {q} 0 -1 WITHSCORES"),
        KvType::Stream => format!("XRANGE {q} - +"),
        KvType::Other(_) => format!("TYPE {q}"),
    }
}

/// The value inspector for one selected key: its value (or just a big
/// collection's length, per `KvValue`/`KvCollection`), and, for a big
/// collection, the paged sub-grid state (see docs/plans/redis.md's "big
/// collections inside a single key"). Replaces `Value::Capped`'s byte-length
/// triage with an element-count triage one level down, same idea.
pub(crate) struct KvInspector {
    pub(crate) key: String,
    pub(crate) kv_type: KvType,
    pub(crate) ttl: Option<Duration>,
    /// `None` while the initial `KvReadValue` is in flight.
    pub(crate) value: Option<KvValue>,
    /// The big-collection sub-grid's accumulated rows, only populated once
    /// `value` reports a `KvCollection::Large`. A list's elements reuse
    /// `KvElement::Member` (no separate variant; a list has no field/score,
    /// same shape as a set member for rendering purposes) and are fetched
    /// once as a static head window, not paged (see `LIST_PREVIEW_COUNT`).
    /// Behind an `Rc` so the per-frame grid render shares the buffer by a
    /// refcount bump instead of deep-cloning every paged element each frame;
    /// mutate via `Rc::make_mut`.
    pub(crate) collection_rows: Rc<Vec<KvElement>>,
    pub(crate) collection_cursor: u64,
    pub(crate) collection_exhausted: bool,
    /// True when `collection_rows` is a one-shot *head window* (a big list's
    /// preview) rather than the whole collection. Distinct from
    /// `collection_exhausted`, which a head window also sets: a tail append
    /// lands off-window, so the optimistic patch must not push it into the
    /// preview as if it were the next element.
    pub(crate) collection_head_only: bool,
    pub(crate) collection_loading: bool,
    pub(crate) collection_scroll: UniformListScrollHandle,

    // --- big-stream paging (only populated once `value` reports a
    // `KvValue::Stream(KvCollection::Large)`; see docs/plans/redis.md's R4).
    // Streams page by entry-ID range rather than the `*SCAN` cursor the other
    // collections use, so they get their own accumulator instead of reusing
    // `collection_rows`. Entries accumulate newest-first, oldest-continued.
    /// Behind an `Rc` for the same reason as `collection_rows`.
    pub(crate) stream_rows: Rc<Vec<StreamEntry>>,
    /// The oldest entry ID loaded so far, fed back as the next page's
    /// exclusive upper bound; `None` before the first page or once exhausted.
    pub(crate) stream_before: Option<String>,
    pub(crate) stream_exhausted: bool,
    pub(crate) stream_loading: bool,
    pub(crate) stream_scroll: UniformListScrollHandle,

    /// Consumer-group management state for a stream key (see
    /// docs/plans/redis.md's "stream consumer-group management" gap). Only
    /// meaningful when `kv_type` is `Stream`; its `view` toggles the stream
    /// body between the entries grid and the groups view. Loaded lazily the
    /// first time the user switches to the Groups tab.
    pub(crate) stream_groups: StreamGroupsState,

    // --- editing (see docs/plans/redis.md's editing phase) ---
    // Each editable field gets one persistent `TextInput`, created once when
    // the inspector opens rather than lazily, so a click just flips a
    // visibility flag instead of constructing a fresh entity mid-render
    // (render only has shared `&Context`, not the `&mut Context` entity
    // construction needs).
    /// The value editor is a multiline [`CodeEditor`] (not a single-line
    /// `TextInput`), mirroring the SQL cell inspector so a multi-line string
    /// (pretty JSON, a blob's text) is edited full-height in place. ⌘↵ saves,
    /// Esc cancels (see the subscription in `kv_open_inspector`).
    pub(crate) value_editor: Entity<CodeEditor>,
    pub(crate) editing_value: bool,
    /// The read-only, *selectable* preview of the string value (drag /
    /// double-click a word / ⌘C a portion), mirroring the SQL cell inspector.
    /// Rebuilt only when the value or lens changes (see
    /// `kv_rebuild_str_preview`) so an in-progress selection and scroll survive
    /// across frames; `None` while editing (the editor owns the body then) or
    /// for a non-string value.
    pub(crate) str_preview: Option<KvStrPreview>,
    pub(crate) ttl_editor: Entity<TextInput>,
    pub(crate) editing_ttl: bool,
    pub(crate) rename_editor: Entity<TextInput>,
    pub(crate) editing_key: bool,
    pub(crate) confirm_delete: bool,
    /// The lens the string value is rendered through (Auto/Raw/JSON/Hex +
    /// binary decoders), reusing the SQL inspector's `ValueFormat` (see
    /// docs/plans/redis.md's "binary value decoders" gap). Only meaningful for
    /// a `KvValue::Str`.
    pub(crate) str_format: crate::inspector::ValueFormat,
    /// True while a "Load full value" `KvReadStringFull` is in flight (the
    /// string was `read_value`-capped and the user asked for the whole thing);
    /// drives the button's "Loading…" state. Cleared when the value lands.
    pub(crate) loading_full_value: bool,
    /// Set when the user hit Edit on a still-capped string: the editor can't
    /// open until the full value lands (editing the truncated head would save
    /// it back over the key), so we fetch it first and open the editor in
    /// `on_kv_value_ready` once it arrives.
    pub(crate) edit_after_load: bool,

    // --- collection-element editing (hash field / set member / zset member /
    // list element add/edit/delete; see docs/plans/redis.md's editing phase).
    /// The two shared inputs the element popover uses: `elem_name` is the
    /// hash field / set member / list value; `elem_value` is the hash value or
    /// zset score. Persistent like the other inspector editors.
    pub(crate) elem_name_editor: Entity<TextInput>,
    pub(crate) elem_value_editor: Entity<TextInput>,
    /// The open collection-element edit popover, if any (one at a time).
    pub(crate) collection_edit: Option<CollectionEditKind>,
    /// An inline validation message for the element popover (e.g. a
    /// non-numeric zset score); cleared when the popover reopens.
    pub(crate) elem_error: Option<String>,

    /// True once a `KvReadValue` reply has landed (even a `None`): lets the
    /// value area distinguish "still loading" from "loaded, but the key is
    /// gone", so a vanished key no longer shows a permanent spinner.
    pub(crate) value_loaded: bool,
    /// An error from reading the value (a transport / `WRONGTYPE` failure),
    /// shown in the value area instead of a stuck "Loading…".
    pub(crate) value_error: Option<String>,
    /// Inline validation message for the expiry popover (a non-numeric input),
    /// shown instead of silently ignoring the value.
    pub(crate) ttl_error: Option<String>,
}

/// Which collection-element edit popover the inspector is showing: adding or
/// editing one hash field / set member / zset member / list element. The
/// per-type `Edit*` variants carry enough to identify the element being
/// changed; the new content comes from the inspector's shared element editors.
#[derive(Clone)]
pub(crate) enum CollectionEditKind {
    AddHashField,
    EditHashField { field: String },
    AddSetMember,
    EditSetMember { old: String },
    AddZSetMember,
    EditZSetScore { member: String },
    AddListHead,
    AddListTail,
    EditListIndex { index: i64 },
}

/// A read-only [`CodeEditor`] hosting the *displayed* string value so the user
/// can select and copy part of it, mirroring the SQL cell inspector's
/// `PreviewView`. Built by [`AppState::kv_rebuild_str_preview`].
pub(crate) struct KvStrPreview {
    pub(crate) editor: Entity<CodeEditor>,
    /// Kept alive for the editor's Escape → close-inspector subscription.
    #[allow(dead_code)]
    sub: Subscription,
}

/// Which stream sub-view the inspector shows: the entries grid (the default)
/// or the consumer-group management panel.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum StreamView {
    #[default]
    Entries,
    Groups,
}

/// The inspector's consumer-group panel for a stream: the list of groups, the
/// selected group's consumers + pending entries, and the inline claim form.
/// Fetched lazily (nothing loads until the user opens the Groups tab), then
/// refreshed after each `XACK`/`XCLAIM` so counts stay live.
pub(crate) struct StreamGroupsState {
    pub(crate) view: StreamView,
    /// Set once the first `XINFO GROUPS` reply lands, so switching to the tab
    /// again doesn't re-fetch on every toggle (an explicit refresh still does).
    pub(crate) loaded: bool,
    pub(crate) loading: bool,
    pub(crate) groups: Vec<StreamGroup>,
    /// The group whose consumers/pending are shown, if any.
    pub(crate) selected: Option<String>,
    pub(crate) consumers: Vec<StreamConsumer>,
    pub(crate) pending: Vec<PendingEntry>,
    pub(crate) detail_loading: bool,
    /// The pending entry ID whose inline "claim to consumer" form is open, if
    /// any (only one at a time).
    pub(crate) claiming: Option<String>,
    /// The target-consumer input for the claim form. Persistent (built once
    /// when the inspector opens), like the other inspector editors.
    pub(crate) claim_editor: Entity<TextInput>,
}

impl BrowseState {
    pub(crate) fn new(session: SessionId, cx: &mut Context<AppState>) -> Self {
        // `bare()` so the box has no border/background of its own: it sits inside
        // the combined `[mode ▾ │ input]` search field, which owns the chrome
        // (see the toolbar in `render_kv_browse`).
        let filter = cx.new(|cx| {
            TextInput::new(cx)
                .bare()
                .with_placeholder(QueryMode::Glob.placeholder())
        });
        cx.subscribe(&filter, move |this, input, event: &TextInputEvent, cx| {
            // Only the active (visible, focused) tab can receive input events
            // in the no-split shell, so routing to the active Browse tab is
            // unambiguous here (see docs/plans/redis-workflow-parity.md).
            let mode = this
                .conn_mut(Some(session))
                .and_then(|a| a.kv_view.as_ref())
                .and_then(|v| v.active_browse())
                .map(|b| b.mode)
                .unwrap_or(QueryMode::Glob);
            let text = input.read(cx).content().to_string();
            match event {
                // Enter applies the box text under the current mode immediately;
                // Fuzzy has no server round trip to fire early (it filters live at
                // render time), so Enter is a no-op there.
                TextInputEvent::Submit => match mode {
                    QueryMode::Glob | QueryMode::Prefix => {
                        this.kv_restart_scan(session, mode.scan_pattern(&text), cx)
                    }
                    // Exact is a single `probe_key`, not a scan.
                    QueryMode::Exact => this.kv_probe_exact(session, non_empty(text), cx),
                    // Value search is a costly per-key value read, so it runs only
                    // on Enter (never live on every keystroke).
                    QueryMode::Value => this.kv_set_value_needle(session, non_empty(text), cx),
                    QueryMode::Fuzzy => {}
                },
                TextInputEvent::Change => match mode {
                    // Every server-backed mode applies on a debounce (Enter is an
                    // accelerator, not a requirement): Glob/Prefix restart the
                    // scan, Exact probes the typed key, Value re-scans reading
                    // values. `kv_debounce_filter` re-reads the mode at fire time.
                    QueryMode::Glob | QueryMode::Prefix | QueryMode::Exact | QueryMode::Value => {
                        this.kv_debounce_filter(session, text, cx)
                    }
                    // Fuzzy filtering reads the input live at render time (see
                    // `render_kv_browse`); this just repaints and keeps the
                    // candidate pool growing if the new query is under-matched.
                    QueryMode::Fuzzy => {
                        this.kv_maybe_grow_pool(session, cx);
                        cx.notify();
                    }
                },
                _ => {}
            }
        })
        .detach();
        Self {
            epoch: crate::result::next_kv_epoch(),
            pattern: None,
            type_filter: None,
            type_filter_open: false,
            ttl_filter: None,
            ttl_filter_open: false,
            fav_only: false,
            tag_filter: None,
            tag_filter_open: false,
            fav_set: Rc::new(HashSet::new()),
            tag_set: Rc::new(HashSet::new()),
            meta_gen: 0,
            rows: Rc::new(Vec::new()),
            rows_gen: 0,
            visible_cache: RefCell::new(None),
            cursor: ScanCursor::START,
            exhausted: false,
            loading: false,
            scroll: UniformListScrollHandle::new(),
            list_focus: cx.focus_handle(),
            nav_row: None,
            filter,
            filter_gen: 0,
            mode: QueryMode::Glob,
            mode_open: false,
            inspector: None,
            big_keys: None,
            create_key: None,
            evicted: false,
            tree_mode: false,
            expanded: HashSet::new(),
            expand_gen: 0,
            tree_cache: RefCell::new(None),
            value_needle: None,
            // Seeded from the `redis.auto_refresh_secs` setting once the tab is
            // wired to a session (see `kv_apply_auto_refresh_default`); off here.
            auto_refresh: None,
            auto_refresh_gen: 0,
        }
    }

    /// The flattened namespace tree over `rows` (grouped by `:`), honoring the
    /// expanded set. Memoized on `(rows identity, expand_gen)` so it rebuilds
    /// only when the loaded keys or the open/closed branches change.
    fn tree_rows(&self, rows: &Rc<Vec<KeyMeta>>) -> Rc<Vec<DispRow>> {
        let ptr = Rc::as_ptr(rows);
        if let Some(cached) = self.tree_cache.borrow().as_ref()
            && cached.rows_ptr == ptr
            && cached.expand_gen == self.expand_gen
        {
            return Rc::clone(&cached.disp);
        }
        let disp = Rc::new(build_tree(rows, &self.expanded));
        *self.tree_cache.borrow_mut() = Some(TreeCache {
            rows_ptr: ptr,
            expand_gen: self.expand_gen,
            disp: Rc::clone(&disp),
        });
        disp
    }

    /// Mutable access to the loaded rows. Bumps the generation counter so the
    /// fuzzy `visible_rows` cache recomputes, and `Rc::make_mut` gives a unique
    /// buffer — which is in-place (no clone) whenever the previous frame's
    /// shared `Rc` has already been dropped, as it has by the next mutation.
    fn rows_mut(&mut self) -> &mut Vec<KeyMeta> {
        self.rows_gen = self.rows_gen.wrapping_add(1);
        Rc::make_mut(&mut self.rows)
    }

    /// The rows as currently shown in the grid: the raw scan rows, or, in fuzzy
    /// mode with a non-empty query, the fuzzy-scored subset in best-match order.
    /// Shared by render and keyboard nav so both agree on order and indices.
    /// Returns a shared `Rc` (a refcount bump), never a deep clone; the fuzzy
    /// subset is memoized on `(query, rows_gen)` so an unrelated re-render
    /// doesn't re-score and re-sort every loaded key.
    pub(crate) fn visible_rows(&self, cx: &App) -> Rc<Vec<KeyMeta>> {
        let fuzzy = self.is_fuzzy();
        let query = if fuzzy {
            self.filter.read(cx).content().to_string()
        } else {
            String::new()
        };
        let fuzzy_active = fuzzy && !query.is_empty();
        let ttl = self.ttl_filter;
        let meta_active = self.fav_only || self.tag_filter.is_some();
        // Fast path: nothing narrows the raw scan buffer, so share it by refcount.
        if !fuzzy_active && ttl.is_none() && !meta_active {
            return Rc::clone(&self.rows);
        }
        if let Some(cached) = self.visible_cache.borrow().as_ref()
            && cached.query == query
            && cached.ttl == ttl
            && cached.r#gen == self.rows_gen
            && cached.meta_gen == self.meta_gen
        {
            return Rc::clone(&cached.rows);
        }
        // The TTL + favourite/tag predicates prune first; a fuzzy query then
        // scores and best-match-orders what's left, otherwise the surviving rows
        // keep scan order. `fav_set`/`tag_set` are `AppState`-supplied snapshots
        // (see `kv_refresh_meta_snapshots`).
        let passes = |r: &KeyMeta| {
            ttl.is_none_or(|t| t.matches(r.ttl))
                && (!self.fav_only || self.fav_set.contains(&r.key))
                && (self.tag_filter.is_none() || self.tag_set.contains(&r.key))
        };
        let result = if fuzzy_active {
            let mut scored: Vec<(i32, &KeyMeta)> = self
                .rows
                .iter()
                .filter(|r| passes(r))
                .filter_map(|r| fuzzy_score(&query, &r.key).map(|s| (s, r)))
                .collect();
            scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            Rc::new(
                scored
                    .into_iter()
                    .map(|(_, r)| r.clone())
                    .collect::<Vec<_>>(),
            )
        } else {
            Rc::new(
                self.rows
                    .iter()
                    .filter(|r| passes(r))
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        *self.visible_cache.borrow_mut() = Some(VisibleRowsCache {
            query,
            ttl,
            r#gen: self.rows_gen,
            meta_gen: self.meta_gen,
            rows: Rc::clone(&result),
        });
        result
    }
}

/// Memoized [`BrowseState::visible_rows`] result (see that field). Valid while
/// the fuzzy query, the TTL filter, the favourite/tag snapshot generation, and
/// the row generation are all unchanged.
struct VisibleRowsCache {
    query: String,
    ttl: Option<TtlFilter>,
    r#gen: u64,
    meta_gen: u64,
    rows: Rc<Vec<KeyMeta>>,
}

/// One rendered line in the key list. In grid mode it's always a `Key`; in tree
/// mode the list is a depth-first flattening of the namespace trie into `Folder`
/// (a `:`-delimited prefix, expandable) and `Leaf` (a concrete key) lines.
#[derive(Clone)]
pub(crate) enum DispRow {
    /// A concrete key row: an index into the browse's visible-rows buffer, plus
    /// the display label (the last path segment in tree mode, the full key in
    /// grid mode) and its indent depth.
    Key {
        row: usize,
        label: String,
        depth: usize,
    },
    /// A namespace node: the full prefix path (`a:b`), the segment label to show,
    /// how many keys live under it, whether it's expanded, and its indent depth.
    Folder {
        prefix: String,
        label: String,
        count: usize,
        expanded: bool,
        depth: usize,
    },
}

/// Memoized flattened namespace tree (see [`BrowseState::tree_rows`]). Keyed by
/// the row buffer's pointer identity (stable while the `Rc` isn't re-`make_mut`ed)
/// and the expand generation.
struct TreeCache {
    rows_ptr: *const Vec<KeyMeta>,
    expand_gen: u64,
    disp: Rc<Vec<DispRow>>,
}

/// A node in the namespace trie built from the loaded keys. Children are a
/// `BTreeMap` so siblings render in stable, sorted order; `leaf` marks a node
/// that is itself a concrete key (a prefix can be both a key and a namespace).
#[derive(Default)]
struct TrieNode {
    children: std::collections::BTreeMap<String, TrieNode>,
    /// Index into the rows buffer if a concrete key terminates exactly here.
    leaf: Option<usize>,
    /// Number of concrete keys anywhere in this subtree (the folder count).
    count: usize,
}

/// Build the flattened namespace tree from `rows`, splitting each key on `:`
/// and honoring `expanded` (a collapsed folder hides its subtree). Grid indent
/// is expressed as a per-row `depth`.
fn build_tree(rows: &[KeyMeta], expanded: &HashSet<String>) -> Vec<DispRow> {
    let mut root = TrieNode::default();
    for (idx, meta) in rows.iter().enumerate() {
        let mut node = &mut root;
        node.count += 1;
        for seg in meta.key.split(':') {
            node = node.children.entry(seg.to_string()).or_default();
            node.count += 1;
        }
        node.leaf = Some(idx);
    }
    let mut out = Vec::with_capacity(rows.len());
    flatten_trie(&root, String::new(), 0, expanded, &mut out);
    out
}

/// Depth-first flatten of the trie into [`DispRow`]s, recursing into a folder
/// only when it's in `expanded`.
fn flatten_trie(
    node: &TrieNode,
    path: String,
    depth: usize,
    expanded: &HashSet<String>,
    out: &mut Vec<DispRow>,
) {
    for (seg, child) in &node.children {
        let full = if path.is_empty() {
            seg.clone()
        } else {
            format!("{path}:{seg}")
        };
        if child.children.is_empty() {
            // Pure leaf: a node with no children is always a concrete key.
            if let Some(row) = child.leaf {
                out.push(DispRow::Key {
                    row,
                    label: seg.clone(),
                    depth,
                });
            }
        } else {
            let is_expanded = expanded.contains(&full);
            out.push(DispRow::Folder {
                prefix: full.clone(),
                label: seg.clone(),
                count: child.count,
                expanded: is_expanded,
                depth,
            });
            if is_expanded {
                // A prefix that is *also* an exact key shows that key first.
                if let Some(row) = child.leaf {
                    out.push(DispRow::Key {
                        row,
                        label: format!("{seg} (self)"),
                        depth: depth + 1,
                    });
                }
                flatten_trie(child, full, depth + 1, expanded, out);
            }
        }
    }
}

/// Every namespace-folder prefix in `rows`: each strict `:`-delimited ancestor
/// of a key (so `a:b:c` contributes `a` and `a:b`, and a key with no `:`
/// contributes none). The set the tree view's "Expand all" opens — it mirrors
/// exactly the folders [`build_tree`] would create.
fn all_tree_prefixes(rows: &[KeyMeta]) -> HashSet<String> {
    let mut out = HashSet::new();
    for meta in rows {
        let segs: Vec<&str> = meta.key.split(':').collect();
        let mut acc = String::new();
        for seg in &segs[..segs.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push(':');
            }
            acc.push_str(seg);
            out.insert(acc.clone());
        }
    }
    out
}

impl AnalysisState {
    pub(crate) fn new() -> Self {
        Self {
            epoch: crate::result::next_kv_epoch(),
            cursor: ScanCursor::START,
            running: false,
            started: std::time::Instant::now(),
            collected: Vec::new(),
            report: None,
            loaded: false,
        }
    }
}

impl RedisTabState {
    /// Build a fresh tab body of the given kind. Needs `cx` because several
    /// panels create persistent `TextInput` entities + subscriptions up front.
    pub(crate) fn new(kind: KvPanel, session: SessionId, cx: &mut Context<AppState>) -> Self {
        match kind {
            KvPanel::Browse => RedisTabState::Browse(Box::new(BrowseState::new(session, cx))),
            KvPanel::Console => {
                RedisTabState::Console(crate::kvconsole::KvConsole::new(session, cx))
            }
            KvPanel::PubSub => RedisTabState::PubSub(crate::kvpubsub::KvPubSub::new(cx)),
            KvPanel::Monitor => RedisTabState::Monitor(crate::kvmonitor::KvMonitor::new()),
            KvPanel::Keyspace => RedisTabState::Keyspace(crate::kvkeyspace::KvKeyspace::new()),
            KvPanel::Analysis => RedisTabState::Analysis(AnalysisState::new()),
        }
    }
}

impl WorkspaceTab for RedisTab {
    fn pane(&self) -> SplitHalf {
        self.pane
    }
    fn set_pane(&mut self, half: SplitHalf) {
        self.pane = half;
    }
    fn pinned(&self) -> bool {
        self.pinned
    }
}

impl TabWorkspace for RedisView {
    type Tab = RedisTab;
    fn ws_tabs(&self) -> &[RedisTab] {
        &self.tabs
    }
    fn ws_tabs_mut(&mut self) -> &mut Vec<RedisTab> {
        &mut self.tabs
    }
    fn ws_active(&self) -> usize {
        self.active_tab
    }
    fn ws_set_active(&mut self, i: usize) {
        self.active_tab = i;
    }
    fn ws_split(&self) -> Option<&SplitState> {
        self.split.as_ref()
    }
    fn ws_split_mut(&mut self) -> &mut Option<SplitState> {
        &mut self.split
    }
    /// Redis has no separate pinned strip section, so pinned tabs sort ahead
    /// within their pane's strip.
    fn pins_sort_first(&self) -> bool {
        true
    }
}

impl RedisView {
    pub(crate) fn new(session: SessionId, cx: &mut Context<AppState>) -> Self {
        let browse = RedisTabState::Browse(Box::new(BrowseState::new(session, cx)));
        Self {
            tabs: vec![RedisTab {
                id: 0,
                title: KvPanel::Browse.label().to_string(),
                state: browse,
                pane: SplitHalf::Primary,
                pinned: false,
            }],
            active_tab: 0,
            tab_seq: 1,
            db_size: None,
            recent_keys: Vec::new(),
            tab_scroll: ScrollHandle::new(),
            tab_drop_target: None,
            split: None,
            tab_menu: None,
            key_menu: None,
            annotate: None,
            actions_menu: None,
            auto_menu: None,
            import: None,
            new_tab_focus: cx.focus_handle(),
            new_tab_sel: 0,
        }
    }

    /// The Browse tab with the given id, regardless of which tab is focused —
    /// so an auto-refresh timer keeps ticking a specific tab even in split view
    /// or after focus moves elsewhere (see `kv_arm_auto_refresh`).
    pub(crate) fn browse_by_tab_id_mut(&mut self, id: u64) -> Option<&mut BrowseState> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::Browse(b) if t.id == id => Some(&mut **b),
            _ => None,
        })
    }

    // --- split panes: the pane-routing + split-invariant logic is shared with
    // the SQL side via the `TabWorkspace` trait (see `crate::app`); this view
    // supplies only the field accessors, below. ---

    fn tab_index_by_id(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    // --- render-time per-tab-index accessors (each split half displays its
    // own tab, which may not be the focused one) ---

    pub(crate) fn browse_at(&self, idx: usize) -> Option<&BrowseState> {
        match self.tabs.get(idx).map(|t| &t.state)? {
            RedisTabState::Browse(b) => Some(b),
            _ => None,
        }
    }
    pub(crate) fn console_at(&self, idx: usize) -> Option<&crate::kvconsole::KvConsole> {
        match self.tabs.get(idx).map(|t| &t.state)? {
            RedisTabState::Console(c) => Some(c),
            _ => None,
        }
    }
    pub(crate) fn pubsub_at(&self, idx: usize) -> Option<&crate::kvpubsub::KvPubSub> {
        match self.tabs.get(idx).map(|t| &t.state)? {
            RedisTabState::PubSub(p) => Some(p),
            _ => None,
        }
    }
    pub(crate) fn monitor_at(&self, idx: usize) -> Option<&crate::kvmonitor::KvMonitor> {
        match self.tabs.get(idx).map(|t| &t.state)? {
            RedisTabState::Monitor(m) => Some(m),
            _ => None,
        }
    }
    pub(crate) fn keyspace_at(&self, idx: usize) -> Option<&crate::kvkeyspace::KvKeyspace> {
        match self.tabs.get(idx).map(|t| &t.state)? {
            RedisTabState::Keyspace(k) => Some(k),
            _ => None,
        }
    }
    pub(crate) fn analysis_at(&self, idx: usize) -> Option<&AnalysisState> {
        match self.tabs.get(idx).map(|t| &t.state)? {
            RedisTabState::Analysis(a) => Some(a),
            _ => None,
        }
    }

    // --- active-tab accessors (UI actions target the visible tab) ---

    pub(crate) fn active_state(&self) -> Option<&RedisTabState> {
        self.tabs.get(self.focused_tab_index()).map(|t| &t.state)
    }
    pub(crate) fn active_state_mut(&mut self) -> Option<&mut RedisTabState> {
        let i = self.focused_tab_index();
        self.tabs.get_mut(i).map(|t| &mut t.state)
    }
    pub(crate) fn active_browse(&self) -> Option<&BrowseState> {
        match self.active_state()? {
            RedisTabState::Browse(b) => Some(b),
            _ => None,
        }
    }
    pub(crate) fn active_browse_mut(&mut self) -> Option<&mut BrowseState> {
        match self.active_state_mut()? {
            RedisTabState::Browse(b) => Some(b),
            _ => None,
        }
    }
    pub(crate) fn active_console(&self) -> Option<&crate::kvconsole::KvConsole> {
        match self.active_state()? {
            RedisTabState::Console(c) => Some(c),
            _ => None,
        }
    }
    pub(crate) fn active_console_mut(&mut self) -> Option<&mut crate::kvconsole::KvConsole> {
        match self.active_state_mut()? {
            RedisTabState::Console(c) => Some(c),
            _ => None,
        }
    }
    pub(crate) fn active_pubsub_mut(&mut self) -> Option<&mut crate::kvpubsub::KvPubSub> {
        match self.active_state_mut()? {
            RedisTabState::PubSub(p) => Some(p),
            _ => None,
        }
    }
    pub(crate) fn active_monitor_mut(&mut self) -> Option<&mut crate::kvmonitor::KvMonitor> {
        match self.active_state_mut()? {
            RedisTabState::Monitor(m) => Some(m),
            _ => None,
        }
    }
    pub(crate) fn active_keyspace_mut(&mut self) -> Option<&mut crate::kvkeyspace::KvKeyspace> {
        match self.active_state_mut()? {
            RedisTabState::Keyspace(k) => Some(k),
            _ => None,
        }
    }
    pub(crate) fn active_analysis_mut(&mut self) -> Option<&mut AnalysisState> {
        match self.active_state_mut()? {
            RedisTabState::Analysis(a) => Some(a),
            _ => None,
        }
    }

    // --- epoch routing (backend events may target a background tab) ---

    /// The Browse tab whose live-scan run owns `epoch` (not its big-keys or
    /// analysis epoch — those have their own lookups).
    pub(crate) fn browse_by_scan_epoch_mut(
        &mut self,
        epoch: red_service::Epoch,
    ) -> Option<&mut BrowseState> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::Browse(b) if b.epoch == epoch => Some(&mut **b),
            _ => None,
        })
    }
    /// The Browse tab whose open inspector is on `key`, regardless of which tab
    /// is focused. Inspector replies (value/collection/stream/edit) route here
    /// rather than through `active_browse_mut`, so in split view a reply still
    /// lands on the tab that asked even if focus moved to the other half while
    /// the read was in flight (which would otherwise drop it and strand the
    /// inspector on "Loading…", or apply it to the wrong tab).
    pub(crate) fn browse_by_inspector_key_mut(&mut self, key: &str) -> Option<&mut BrowseState> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::Browse(b) if b.inspector.as_ref().is_some_and(|i| i.key == key) => {
                Some(&mut **b)
            }
            _ => None,
        })
    }
    /// The Browse tab whose in-flight biggest-keys sample owns `epoch`.
    pub(crate) fn browse_by_big_keys_epoch_mut(
        &mut self,
        epoch: red_service::Epoch,
    ) -> Option<&mut BrowseState> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::Browse(b) if b.big_keys.as_ref().is_some_and(|bk| bk.epoch == epoch) => {
                Some(&mut **b)
            }
            _ => None,
        })
    }
    pub(crate) fn analysis_by_epoch_mut(
        &mut self,
        epoch: red_service::Epoch,
    ) -> Option<&mut AnalysisState> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::Analysis(a) if a.epoch == epoch => Some(a),
            _ => None,
        })
    }
    pub(crate) fn console_by_epoch_mut(
        &mut self,
        epoch: red_service::Epoch,
    ) -> Option<&mut crate::kvconsole::KvConsole> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::Console(c) if c.epoch == epoch => Some(c),
            _ => None,
        })
    }
    pub(crate) fn monitor_by_epoch_mut(
        &mut self,
        epoch: red_service::Epoch,
    ) -> Option<&mut crate::kvmonitor::KvMonitor> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::Monitor(m) if m.epoch == epoch => Some(m),
            _ => None,
        })
    }
    pub(crate) fn pubsub_by_epoch_mut(
        &mut self,
        epoch: red_service::Epoch,
    ) -> Option<&mut crate::kvpubsub::KvPubSub> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::PubSub(p) if p.epoch == epoch => Some(p),
            _ => None,
        })
    }
    pub(crate) fn keyspace_by_epoch_mut(
        &mut self,
        epoch: red_service::Epoch,
    ) -> Option<&mut crate::kvkeyspace::KvKeyspace> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::Keyspace(k) if k.epoch == epoch => Some(k),
            _ => None,
        })
    }
}

impl AppState {
    /// Kick off the very first scan + the one-time `DBSIZE` header stat, right
    /// after a Redis session connects.
    pub(crate) fn kv_start_browse(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        browse.loading = true;
        let epoch = browse.epoch;
        self.service.send_to(session, Command::KvDbSize { epoch });
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern: None,
                type_filter: None,
                value_needle: None,
                cursor: ScanCursor::START,
                budget: scan_budget(),
            },
        );
        cx.notify();
        // Seed the connection's first Browse tab with the auto-refresh default.
        self.kv_apply_auto_refresh_default(session, cx);
    }

    /// The filter box changed via typing (not Enter): wait `FILTER_DEBOUNCE_MS`
    /// of no further typing before applying the box text under the current mode,
    /// so a fast typist doesn't fire one backend round trip per keystroke. Every
    /// mode debounces through here (Fuzzy filters loaded rows live and never
    /// reaches this); Enter is just an accelerator that applies the same thing
    /// immediately. Mirrors `connect.rs`'s `connect_gen` generation-check shape:
    /// bump `filter_gen` now, capture it, and only act in the timer callback if
    /// it's still current; any later `Change` (or an intervening `Submit`, which
    /// applies directly and leaves this generation stale) makes this a no-op.
    pub(crate) fn kv_debounce_filter(
        &mut self,
        session: SessionId,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        browse.filter_gen += 1;
        let generation = browse.filter_gen;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor()
                .timer(Duration::from_millis(FILTER_DEBOUNCE_MS))
                .await;
            this.update(cx, |this, cx| {
                // Re-read the mode at fire time so the action honors it even if
                // the user switched modes mid-debounce.
                let current = this
                    .conn_mut(Some(session))
                    .and_then(|a| a.kv_view.as_ref())
                    .and_then(|v| v.active_browse())
                    .filter(|b| b.filter_gen == generation)
                    .map(|b| b.mode);
                match current {
                    Some(mode @ (QueryMode::Glob | QueryMode::Prefix)) => {
                        this.kv_restart_scan(session, mode.scan_pattern(&text), cx)
                    }
                    // A single-key metadata probe — cheap enough to run live.
                    Some(QueryMode::Exact) => this.kv_probe_exact(session, non_empty(text), cx),
                    // A value search reads each scanned key's value, so it's the
                    // heaviest filter; debounce keeps it to one scan per typing
                    // pause rather than per keystroke.
                    Some(QueryMode::Value) => {
                        this.kv_set_value_needle(session, non_empty(text), cx)
                    }
                    Some(QueryMode::Fuzzy) | None => {}
                }
            })
            .ok();
        })
        .detach();
    }

    /// The filter pattern changed (Enter, or the debounce timer firing):
    /// restart the scan under the new `MATCH` pattern, keeping whatever type
    /// filter is active.
    pub(crate) fn kv_restart_scan(
        &mut self,
        session: SessionId,
        pattern: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        else {
            return;
        };
        if browse.pattern == pattern {
            return; // same filter re-submitted: nothing to restart
        }
        browse.pattern = pattern;
        self.kv_relaunch_browse(session, cx);
    }

    /// The type-filter dropdown picked a type (`None` = all types): restart the
    /// scan under the new `SCAN ... TYPE`, keeping whatever `MATCH` pattern is
    /// active. Always closes the dropdown.
    pub(crate) fn kv_set_type_filter(
        &mut self,
        session: SessionId,
        type_filter: Option<KvType>,
        cx: &mut Context<Self>,
    ) {
        let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        else {
            return;
        };
        browse.type_filter_open = false;
        if browse.type_filter == type_filter {
            cx.notify(); // same type re-picked: just dismiss the dropdown
            return;
        }
        browse.type_filter = type_filter;
        self.kv_relaunch_browse(session, cx);
    }

    /// Set (or clear) the client-side TTL filter on the active Browse tab. Unlike
    /// the type filter this needs no re-scan — it prunes the already-loaded rows
    /// at render time (see [`BrowseState::visible_rows`]) — but a newly-applied
    /// bucket can hide most of the resident window, so it kicks the pool-grow
    /// loop to keep the grid filling (mirrors fuzzy).
    pub(crate) fn kv_set_ttl_filter(
        &mut self,
        session: SessionId,
        ttl_filter: Option<TtlFilter>,
        cx: &mut Context<Self>,
    ) {
        let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        else {
            return;
        };
        browse.ttl_filter_open = false;
        if browse.ttl_filter == ttl_filter {
            cx.notify(); // same bucket re-picked: just dismiss the dropdown
            return;
        }
        browse.ttl_filter = ttl_filter;
        // The visible-row indices shift under a new client-side filter, so the
        // keyboard cursor (an index into the visible rows) is no longer valid.
        browse.nav_row = None;
        cx.notify();
        self.kv_maybe_grow_pool(session, cx);
    }

    /// Toggle the active Browse tab between the flat grid and the collapsible
    /// namespace tree. Purely a view change — no re-scan; the same loaded rows
    /// render either way. Clears the grid keyboard cursor (meaningless in tree
    /// mode).
    pub(crate) fn kv_toggle_tree_mode(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.tree_mode = !browse.tree_mode;
            browse.nav_row = None;
        }
        cx.notify();
    }

    /// Expand or collapse one namespace folder in the tree view (`prefix` is the
    /// full `a:b:c` path). Bumps the tree cache generation so the flattened list
    /// rebuilds.
    pub(crate) fn kv_toggle_tree_node(
        &mut self,
        session: SessionId,
        prefix: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            if !browse.expanded.remove(&prefix) {
                browse.expanded.insert(prefix);
            }
            browse.expand_gen = browse.expand_gen.wrapping_add(1);
        }
        cx.notify();
    }

    /// Drill an Analysis type row into a new Browse tab filtered to that type.
    pub(crate) fn kv_drill_type(
        &mut self,
        session: SessionId,
        type_label: String,
        cx: &mut Context<Self>,
    ) {
        let kv_type = KvType::parse(&type_label).unwrap_or(KvType::Other(type_label));
        self.kv_open_filtered_browse(session, None, Some(kv_type), cx);
    }

    /// Drill an Analysis namespace row into a new Browse tab matching
    /// `prefix:*`.
    pub(crate) fn kv_drill_namespace(
        &mut self,
        session: SessionId,
        prefix: String,
        cx: &mut Context<Self>,
    ) {
        self.kv_open_filtered_browse(session, Some(format!("{prefix}:*")), None, cx);
    }

    /// Spawn a fresh Browse tab and apply a `MATCH` pattern and/or `TYPE`
    /// filter to it — the shared engine behind the Analysis drill-downs. Keeps
    /// the Analysis tab open (opens a *new* Browse tab rather than reusing one).
    fn kv_open_filtered_browse(
        &mut self,
        session: SessionId,
        pattern: Option<String>,
        kv_type: Option<KvType>,
        cx: &mut Context<Self>,
    ) {
        self.kv_new_empty_tab(session, cx);
        let id = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.tabs.get(v.focused_tab_index()).map(|t| t.id));
        let Some(id) = id else {
            return;
        };
        // Converts the blank tab to Browse and fires an initial (unfiltered)
        // scan the filters below then supersede.
        self.kv_set_tab_kind(session, id, KvPanel::Browse, cx);
        if let Some(p) = pattern.clone()
            && let Some(browse) = self
                .conn_mut(Some(session))
                .and_then(|a| a.kv_view.as_mut())
                .and_then(|v| v.active_browse_mut())
        {
            browse.filter.update(cx, |ti, cx| ti.set_content(&p, cx));
        }
        if let Some(t) = kv_type {
            self.kv_set_type_filter(session, Some(t), cx);
        }
        if pattern.is_some() {
            self.kv_restart_scan(session, pattern, cx);
        }
    }

    /// Open or dismiss the type-filter dropdown's option list.
    pub(crate) fn kv_toggle_type_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.type_filter_open = !browse.type_filter_open;
            cx.notify();
        }
    }

    /// Open or dismiss the TTL-filter dropdown's option list.
    pub(crate) fn kv_toggle_ttl_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.ttl_filter_open = !browse.ttl_filter_open;
            cx.notify();
        }
    }

    /// Rebuild the active Browse tab's favourite/tag key snapshots from the
    /// annotation store to match its current `fav_only`/`tag_filter`, bump
    /// `meta_gen` (so `visible_rows`' cache recomputes) and clear the keyboard
    /// cursor (the visible indices shift). Call after the filter *or* the store
    /// changes (a star toggle, a tag edit). The store read and the `browse`
    /// write are sequenced so their borrows of `self` don't overlap.
    pub(crate) fn kv_refresh_meta_snapshots(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some((conn_id, fav_only, tag)) = self.conn_mut(Some(session)).and_then(|a| {
            let conn_id = a.conn_id.clone();
            let b = a.kv_view.as_ref()?.active_browse()?;
            Some((conn_id, b.fav_only, b.tag_filter.clone()))
        }) else {
            return;
        };
        let fav_set = if fav_only {
            Rc::new(self.redis_key_meta.favorites(&conn_id))
        } else {
            Rc::new(HashSet::new())
        };
        let tag_set = match &tag {
            Some(t) => Rc::new(self.redis_key_meta.keys_with_tag(&conn_id, t)),
            None => Rc::new(HashSet::new()),
        };
        if let Some(b) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            b.fav_set = fav_set;
            b.tag_set = tag_set;
            b.meta_gen = b.meta_gen.wrapping_add(1);
            b.nav_row = None;
        }
        cx.notify();
    }

    /// Toggle the "favourites only" browse filter (the toolbar star button), then
    /// refresh the snapshot and chase more pages if under-matched.
    pub(crate) fn kv_toggle_fav_only(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.fav_only = !browse.fav_only;
        } else {
            return;
        }
        self.kv_refresh_meta_snapshots(session, cx);
        self.kv_maybe_grow_pool(session, cx);
    }

    /// Set (or clear) the tag browse filter (the toolbar tag dropdown), then
    /// refresh the snapshot and chase more pages if under-matched.
    pub(crate) fn kv_set_tag_filter(
        &mut self,
        session: SessionId,
        tag: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        else {
            return;
        };
        browse.tag_filter_open = false;
        if browse.tag_filter == tag {
            cx.notify(); // same tag re-picked: just dismiss the dropdown
            return;
        }
        browse.tag_filter = tag;
        self.kv_refresh_meta_snapshots(session, cx);
        self.kv_maybe_grow_pool(session, cx);
    }

    /// Open or dismiss the tag-filter dropdown's option list.
    pub(crate) fn kv_toggle_tag_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.tag_filter_open = !browse.tag_filter_open;
            cx.notify();
        }
    }

    /// Open the browse toolbar's actions dropdown at `pos` (Refresh · Expand /
    /// Collapse all · Auto-refresh). Closes the other positioned menus first.
    pub(crate) fn kv_open_actions_menu(
        &mut self,
        session: SessionId,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.tab_menu = None;
            view.key_menu = None;
            view.actions_menu = Some(pos);
            cx.notify();
        }
    }

    pub(crate) fn kv_close_actions_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            && view.actions_menu.take().is_some()
        {
            cx.notify();
        }
    }

    /// Toggle the active Browse tab's auto-refresh on/off from the toolbar's
    /// dedicated auto-refresh button (its primary click). Turning it on uses the
    /// `redis.auto_refresh_secs` setting's interval, falling back to 5s when the
    /// setting is "off"; the exact interval is still pickable from the button's
    /// disclosure caret (see `render_kv_auto_menu`).
    pub(crate) fn kv_toggle_auto_refresh(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let on = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .map(|b| b.auto_refresh.is_some())
            .unwrap_or(false);
        let next = if on {
            None
        } else {
            Some(
                self.settings
                    .redis
                    .auto_refresh_interval()
                    .unwrap_or(Duration::from_secs(5)),
            )
        };
        self.kv_set_auto_refresh(session, next, cx);
    }

    /// Open the auto-refresh interval popover (Off · 2/5/10/30s) at `pos`, closing
    /// the other positioned menus first. Mirrors [`Self::kv_open_actions_menu`].
    pub(crate) fn kv_open_auto_menu(
        &mut self,
        session: SessionId,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.tab_menu = None;
            view.key_menu = None;
            view.actions_menu = None;
            view.auto_menu = Some(pos);
            cx.notify();
        }
    }

    pub(crate) fn kv_close_auto_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            && view.auto_menu.take().is_some()
        {
            cx.notify();
        }
    }

    /// Open the "Import keys" modal (actions menu). The file is chosen inside it.
    pub(crate) fn kv_open_import(&mut self, session: SessionId, cx: &mut Context<Self>) {
        self.kv_close_actions_menu(session, cx);
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.import = Some(ImportState {
                path: None,
                commands: Vec::new(),
                error: None,
                running: false,
            });
        }
        cx.notify();
    }

    pub(crate) fn kv_cancel_import(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let closed = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .map(|v| v.import.take().is_some())
            .unwrap_or(false);
        if closed {
            self.refocus_root = true;
            cx.notify();
        }
    }

    /// Native file picker for the import modal: choose a file of Redis commands
    /// (one per line, `#` comments and blanks skipped), read + tokenize it off
    /// the UI thread, and store the parsed commands (or a read/parse error) on
    /// the open `ImportState`.
    pub(crate) fn kv_import_choose_file(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose import file".into()),
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            // Read + tokenize on the background executor: a large command file
            // shouldn't stall the frame.
            let parsed = cx
                .background_executor()
                .spawn(async move {
                    std::fs::read_to_string(&path)
                        .map(|text| {
                            let commands: Vec<Vec<String>> = text
                                .lines()
                                .map(str::trim)
                                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                                .map(red_core::kv::tokenize_command)
                                .filter(|argv| !argv.is_empty())
                                .collect();
                            (path.display().to_string(), commands)
                        })
                        .map_err(|e| e.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                if let Some(imp) = this
                    .conn_mut(Some(session))
                    .and_then(|a| a.kv_view.as_mut())
                    .and_then(|v| v.import.as_mut())
                {
                    match parsed {
                        Ok((path, commands)) => {
                            imp.error = commands
                                .is_empty()
                                .then(|| "No commands found in this file".to_string());
                            imp.path = Some(path);
                            imp.commands = commands;
                        }
                        Err(e) => {
                            imp.error = Some(format!("Couldn't read the file: {e}"));
                            imp.path = None;
                            imp.commands.clear();
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Run the chosen import: send the parsed commands to the backend as one
    /// `KvImport` batch, marking the modal in-flight until `KvImportDone`.
    pub(crate) fn kv_run_import(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        let epoch = view
            .active_browse()
            .map(|b| b.epoch)
            .unwrap_or(red_service::Epoch::ZERO);
        let Some(imp) = view.import.as_mut() else {
            return;
        };
        if imp.running || imp.commands.is_empty() {
            return;
        }
        imp.running = true;
        let commands = imp.commands.clone();
        self.service
            .send_to(session, Command::KvImport { epoch, commands });
        cx.notify();
    }

    /// `Event::KvImportDone`: close the modal, toast a summary, and refresh the
    /// key list so the imported keys show.
    pub(crate) fn on_kv_import_done(
        &mut self,
        session: Option<SessionId>,
        ok: usize,
        failed: usize,
        first_error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.conn_mut(session).and_then(|a| a.kv_view.as_mut()) {
            view.import = None;
        }
        self.refocus_root = true;
        let (variant, msg) = if failed == 0 {
            (ToastVariant::Success, format!("Imported {ok} command(s)"))
        } else {
            let detail = first_error
                .map(|e| format!(" (first error — {e})"))
                .unwrap_or_default();
            (
                ToastVariant::Warning,
                format!("Imported {ok}, {failed} failed{detail}"),
            )
        };
        self.notify(variant, msg, cx);
        if let Some(session) = session {
            self.kv_relaunch_browse(session, cx);
        }
    }

    /// Manually refresh the active Browse tab's key list (actions menu / ⌘R).
    /// Re-runs whatever the current query mode does: a probe in Exact mode, a
    /// full scan relaunch otherwise (keeping the pattern/type/value filters).
    pub(crate) fn kv_refresh_keys(&mut self, session: SessionId, cx: &mut Context<Self>) {
        self.kv_close_actions_menu(session, cx);
        let info = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .map(|b| (b.mode, b.filter.read(cx).content().to_string()));
        match info {
            Some((QueryMode::Exact, text)) => self.kv_probe_exact(session, non_empty(text), cx),
            Some(_) => self.kv_relaunch_browse(session, cx),
            None => {}
        }
    }

    /// Expand every namespace folder in the active Browse tab's tree view.
    pub(crate) fn kv_expand_all(&mut self, session: SessionId, cx: &mut Context<Self>) {
        self.kv_close_actions_menu(session, cx);
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.expanded = all_tree_prefixes(&browse.rows);
            browse.expand_gen = browse.expand_gen.wrapping_add(1);
        }
        cx.notify();
    }

    /// Collapse every namespace folder in the active Browse tab's tree view.
    pub(crate) fn kv_collapse_all(&mut self, session: SessionId, cx: &mut Context<Self>) {
        self.kv_close_actions_menu(session, cx);
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            && !browse.expanded.is_empty()
        {
            browse.expanded.clear();
            browse.expand_gen = browse.expand_gen.wrapping_add(1);
        }
        cx.notify();
    }

    /// Set the active Browse tab's auto-refresh interval (`None` = off) and,
    /// when on, (re-)arm its timer. Driven by the actions-menu Auto-refresh
    /// options.
    pub(crate) fn kv_set_auto_refresh(
        &mut self,
        session: SessionId,
        interval: Option<Duration>,
        cx: &mut Context<Self>,
    ) {
        self.kv_close_actions_menu(session, cx);
        self.kv_close_auto_menu(session, cx);
        let tab_id = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.tabs.get(v.focused_tab_index()).map(|t| t.id));
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.auto_refresh = interval;
            // Bump the generation so any timer armed under the old interval stops.
            browse.auto_refresh_gen = browse.auto_refresh_gen.wrapping_add(1);
        }
        cx.notify();
        if interval.is_some()
            && let Some(id) = tab_id
        {
            self.kv_arm_auto_refresh(session, id, cx);
        }
    }

    /// Apply the `redis.auto_refresh_secs` default to the just-opened active
    /// Browse tab (called from the tab-creation paths). No-op when the setting
    /// is off.
    pub(crate) fn kv_apply_auto_refresh_default(
        &mut self,
        session: SessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(interval) = self.settings.redis.auto_refresh_interval() {
            self.kv_set_auto_refresh(session, Some(interval), cx);
        }
    }

    /// Arm one auto-refresh tick for the Browse tab `tab_id`: after the tab's
    /// interval elapses, relaunch its scan and re-arm — unless the interval was
    /// changed/turned off (generation check) or the tab closed. Routed by tab id
    /// (not the focused tab) so it survives tab switches and split view. An
    /// Exact-mode tab skips the scan (a single pinned key), but stays armed.
    fn kv_arm_auto_refresh(&mut self, session: SessionId, tab_id: u64, cx: &mut Context<Self>) {
        let armed = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.browse_by_tab_id_mut(tab_id))
            .and_then(|b| b.auto_refresh.map(|i| (i, b.auto_refresh_gen)));
        let Some((interval, generation)) = armed else {
            return;
        };
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(interval).await;
            this.update(cx, |this, cx| {
                let state = this
                    .conn_mut(Some(session))
                    .and_then(|a| a.kv_view.as_ref())
                    .and_then(|v| v.tabs.iter().find(|t| t.id == tab_id))
                    .and_then(|t| match &t.state {
                        RedisTabState::Browse(b) => {
                            Some((b.auto_refresh, b.auto_refresh_gen, b.mode))
                        }
                        _ => None,
                    });
                // Superseded (interval changed/off) or the tab is gone: stop.
                let Some((cur, cur_gen, mode)) = state else {
                    return;
                };
                if cur != Some(interval) || cur_gen != generation {
                    return;
                }
                if mode != QueryMode::Exact {
                    this.kv_relaunch_tab(session, tab_id, cx);
                }
                this.kv_arm_auto_refresh(session, tab_id, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Re-dispatch the browse scan from scratch under a fresh epoch with the
    /// browse's current `pattern` + `type_filter`. Shared by the pattern and
    /// type-filter changes: both mutate one field of the filter state, then
    /// call this to close the superseded scan (which cancels its in-flight
    /// fetch at the engine too, see `Command::CloseResult`'s doc comment),
    /// mint a fresh epoch, and start over from `cursor: 0`.
    fn kv_relaunch_browse(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        else {
            return;
        };
        let plan = browse.begin_relaunch();
        self.kv_dispatch_relaunch(session, plan, cx);
    }

    /// Like [`Self::kv_relaunch_browse`] but for a specific tab id rather than
    /// the focused one — the auto-refresh timer's path (see
    /// [`Self::kv_arm_auto_refresh`]), so a background tab keeps refreshing.
    fn kv_relaunch_tab(&mut self, session: SessionId, tab_id: u64, cx: &mut Context<Self>) {
        let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.browse_by_tab_id_mut(tab_id))
        else {
            return;
        };
        let plan = browse.begin_relaunch();
        self.kv_dispatch_relaunch(session, plan, cx);
    }

    /// Close the superseded scan and dispatch the fresh one described by a
    /// [`RelaunchPlan`] (from [`BrowseState::begin_relaunch`]). Shared tail of
    /// both relaunch paths, split out so the `self.service` calls don't overlap
    /// the `&mut BrowseState` borrow.
    fn kv_dispatch_relaunch(
        &mut self,
        session: SessionId,
        plan: RelaunchPlan,
        cx: &mut Context<Self>,
    ) {
        self.service.send_to(
            session,
            Command::CloseResult {
                epoch: plan.old_epoch,
            },
        );
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch: plan.new_epoch,
                pattern: plan.pattern,
                type_filter: plan.type_filter,
                value_needle: plan.value_needle,
                cursor: ScanCursor::START,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }

    /// The keyspace table's `on_visible_range` hook: load the next page once
    /// the visible range nears the end of what's loaded.
    pub(crate) fn kv_maybe_load_more(
        &mut self,
        session: SessionId,
        visible_end: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        if browse.loading || browse.exhausted {
            return;
        }
        // `visible_end` indexes the *visible* rows (the fuzzy-filtered subset in
        // fuzzy mode), so compare it against that same list's length — not
        // `browse.rows.len()` (the full unfiltered scan). Mixing the two made the
        // guard think there were always rows ahead in fuzzy mode, so this
        // scroll-triggered load never fired.
        let visible_count = browse.visible_rows(cx).len();
        if visible_end + LOAD_AHEAD_ROWS < visible_count {
            return; // plenty of loaded rows still ahead of the viewport
        }
        browse.loading = true;
        let epoch = browse.epoch;
        let pattern = browse.pattern.clone();
        let type_filter = browse.type_filter.clone();
        let value_needle = browse.value_needle.clone();
        let cursor = browse.cursor;
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern,
                type_filter,
                value_needle,
                cursor,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }

    /// Keyboard cursor movement in the key table (arrows / Home-End /
    /// PageUp-Down / ⌘arrows), driven by Flint's [`TableNav`]. Moving only
    /// shifts the cursor highlight (and lazily loads more rows as it nears the
    /// tail); the value inspector opens only on Enter/F2 activation
    /// ([`AppState::kv_activate_cursor`]), so holding an arrow through a huge
    /// keyspace doesn't fire a `KvReadValue` per row.
    pub(crate) fn kv_browse_nav(
        &mut self,
        session: SessionId,
        nav: TableNav,
        cx: &mut Context<Self>,
    ) {
        // Left/Right have no meaning in a single logical column.
        if matches!(nav, TableNav::Left | TableNav::Right) {
            return;
        }
        let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
        else {
            return;
        };
        // Tree mode is click-driven: its list indices are folder/leaf display
        // rows, not `visible_rows`, so keyboard nav (which moves `nav_row` over
        // `visible_rows`) doesn't apply.
        if browse.tree_mode {
            return;
        }
        let rows = browse.visible_rows(cx);
        if rows.is_empty() {
            return;
        }
        let last = rows.len() - 1;
        let cur = browse.nav_row.unwrap_or(0).min(last);

        const PAGE: usize = 12;
        let next = match nav {
            TableNav::Up => cur.saturating_sub(1),
            TableNav::Down => (cur + 1).min(last),
            TableNav::PageUp => cur.saturating_sub(PAGE),
            TableNav::PageDown => (cur + PAGE).min(last),
            TableNav::First | TableNav::RowStart => 0,
            TableNav::Last | TableNav::RowEnd => last,
            // Left/Right handled above.
            _ => cur,
        };
        if let Some(b) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            b.nav_row = Some(next);
            b.scroll.scroll_to_item(next, gpui::ScrollStrategy::Nearest);
        }
        // Keep the loaded window ahead of a cursor walking toward the tail.
        self.kv_maybe_load_more(session, next + 1, cx);
        cx.notify();
    }

    /// Enter/F2 activation on the key table (the `BeginEdit` action, shared with
    /// the SQL grid): open the value inspector on the keyboard cursor's row.
    /// Returns `true` when it handled the key — i.e. a Redis browse list is the
    /// focused table — so the caller falls through to `begin_grid_edit` for the
    /// SQL grid otherwise.
    pub(crate) fn kv_activate_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Phase::Connected(active) = &self.phase else {
            return false;
        };
        let session = active.session;
        let Some(browse) = active.kv_view.as_ref().and_then(|v| v.active_browse()) else {
            return false;
        };
        // Only when the key table actually holds focus, so Enter elsewhere in a
        // Redis session (e.g. the filter box) isn't hijacked.
        if !browse.list_focus.is_focused(window) {
            return false;
        }
        let rows = browse.visible_rows(cx);
        if rows.is_empty() {
            return true;
        }
        let cur = browse.nav_row.unwrap_or(0).min(rows.len() - 1);
        let row = rows[cur].clone();
        self.kv_open_inspector(session, row.key, row.ttl, row.kv_type, cx);
        true
    }

    /// ⌘F in a Redis session: jump focus to the active browse tab's filter box
    /// (which *is* the keyspace search field) instead of opening the SQL find
    /// bar. Returns `true` when it handled it — i.e. the foreground connection is
    /// Redis and its active tab is a browse — so the caller falls through to the
    /// SQL find bar otherwise.
    pub(crate) fn kv_focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Phase::Connected(active) = &self.phase else {
            return false;
        };
        let Some(browse) = active.kv_view.as_ref().and_then(|v| v.active_browse()) else {
            return false;
        };
        let handle = browse.filter.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
        true
    }

    /// `Event::KvScanPage`: append the page, or drop it if a filter restart
    /// has already superseded this scan run.
    pub(crate) fn on_kv_scan_page(
        &mut self,
        session: Option<SessionId>,
        epoch: red_service::Epoch,
        page: red_core::kv::KvScanPage,
        cx: &mut Context<Self>,
    ) {
        // The scan-page event is shared by three scan runs that each carry
        // their own epoch: a live browse, a biggest-keys sample, or a
        // keyspace-analysis run. Route to whichever tab owns this epoch.
        let is_big_keys = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.browse_by_big_keys_epoch_mut(epoch))
            .is_some();
        if is_big_keys {
            self.on_big_keys_page(session, epoch, page, cx);
            return;
        }
        let is_analysis = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.analysis_by_epoch_mut(epoch))
            .is_some_and(|a| a.running);
        if is_analysis {
            self.on_analysis_page(session, epoch, page, cx);
            return;
        }
        let Some(browse) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.browse_by_scan_epoch_mut(epoch))
        else {
            return; // superseded scan run, or no tab owns this epoch
        };
        browse.rows_mut().extend(page.keys);
        if browse.rows.len() > MAX_RESIDENT_ROWS {
            let drop = browse.rows.len() - MAX_RESIDENT_ROWS;
            browse.rows_mut().drain(0..drop);
            browse.evicted = true;
            // Front eviction shifts every row index down by `drop`, so move the
            // keyboard cursor with it to keep it on the same key. Only in the
            // plain scan view, where `visible_rows == rows`; fuzzy mode indexes a
            // re-scored subset, so a rows-delta wouldn't apply there.
            if !browse.is_fuzzy()
                && let Some(n) = browse.nav_row.as_mut()
            {
                *n = n.saturating_sub(drop);
            }
        }
        browse.cursor = page.next_cursor;
        browse.exhausted = page.exhausted;
        browse.loading = false;
        cx.notify();
        // Outside the `browse` borrow: if a fuzzy search is under-matched,
        // this page landing is what chains the next one (see
        // `kv_maybe_grow_pool`'s doc comment for the full loop shape).
        if let Some(session) = session {
            self.kv_maybe_grow_pool(session, cx);
        }
    }

    /// Open or dismiss the query-mode dropdown's option list.
    pub(crate) fn kv_toggle_mode_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.mode_open = !browse.mode_open;
            cx.notify();
        }
    }

    /// Switch the filter box's query mode (the dropdown at the head of the
    /// filter bar) and re-apply the current box text under the new meaning:
    /// `Glob`/`Prefix` restart the scan under a `MATCH` pattern, `Exact` probes a
    /// single key, and `Fuzzy`/`Value` restart unfiltered (fuzzy filters the
    /// loaded pool client-side; value search waits for Enter). Always dismisses
    /// the dropdown.
    pub(crate) fn kv_set_query_mode(
        &mut self,
        session: SessionId,
        mode: QueryMode,
        cx: &mut Context<Self>,
    ) {
        let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        else {
            return;
        };
        browse.mode_open = false;
        if browse.mode == mode {
            cx.notify(); // same mode re-picked: just dismiss the dropdown
            return;
        }
        browse.mode = mode;
        // A mode switch invalidates every mode-specific filter; the arm below
        // re-derives whichever ones the new mode uses from the box text.
        browse.value_needle = None;
        browse.pattern = None;
        browse.nav_row = None;
        let text = browse.filter.read(cx).content().to_string();
        browse
            .filter
            .update(cx, |ti, cx| ti.set_placeholder(mode.placeholder(), cx));
        match mode {
            QueryMode::Glob | QueryMode::Prefix => {
                browse.pattern = mode.scan_pattern(&text);
                self.kv_relaunch_browse(session, cx);
            }
            // Exact resolves a single key directly; no scan.
            QueryMode::Exact => self.kv_probe_exact(session, non_empty(text), cx),
            // Fuzzy filters the loaded pool; Value waits for Enter. Either way the
            // scan itself is unfiltered so the whole keyspace is in play.
            QueryMode::Fuzzy | QueryMode::Value => self.kv_relaunch_browse(session, cx),
        }
    }

    /// Resolve one exact key by name via `probe_key` (bypassing `SCAN`) and show
    /// it as the sole browse row, or an empty list when it doesn't exist. `None`
    /// (an empty box) reverts to an unfiltered browse. Used by [`QueryMode::Exact`].
    pub(crate) fn kv_probe_exact(
        &mut self,
        session: SessionId,
        key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = key else {
            // Empty box in Exact mode: fall back to a plain unfiltered browse.
            self.kv_relaunch_browse(session, cx);
            return;
        };
        let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        else {
            return;
        };
        // Reset to a fresh, empty run under a new epoch so a superseded scan (or
        // a previous probe) can't land into this one, then await the probe reply.
        let old_epoch = browse.epoch;
        let new_epoch = crate::result::next_kv_epoch();
        browse.epoch = new_epoch;
        browse.rows_mut().clear();
        browse.evicted = false;
        browse.cursor = ScanCursor::START;
        browse.exhausted = true; // a single probe never paginates
        browse.loading = true;
        browse.nav_row = None;
        self.service
            .send_to(session, Command::CloseResult { epoch: old_epoch });
        self.service.send_to(
            session,
            Command::KvProbeKey {
                epoch: new_epoch,
                key,
            },
        );
        cx.notify();
    }

    /// `Event::KvKeyProbed`: place the probed key's metadata as the sole row of
    /// the browse tab that owns `epoch` (or leave it empty when the key doesn't
    /// exist). Only [`QueryMode::Exact`] issues a probe, so a stray reply for a
    /// superseded epoch is simply dropped.
    pub(crate) fn on_kv_key_probed(
        &mut self,
        session: Option<SessionId>,
        epoch: red_service::Epoch,
        meta: Option<KeyMeta>,
        cx: &mut Context<Self>,
    ) {
        let Some(browse) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.browse_by_scan_epoch_mut(epoch))
        else {
            return; // superseded probe, or no tab owns this epoch
        };
        let rows = browse.rows_mut();
        rows.clear();
        if let Some(meta) = meta {
            rows.push(meta);
        }
        browse.exhausted = true;
        browse.loading = false;
        cx.notify();
    }

    /// Apply (or clear) the value-search needle and relaunch the scan. `None`
    /// (an empty box) reverts to an unfiltered browse.
    pub(crate) fn kv_set_value_needle(
        &mut self,
        session: SessionId,
        needle: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.value_needle = needle;
            browse.pattern = None;
        }
        self.kv_relaunch_browse(session, cx);
    }

    /// While a fuzzy search is active and under-matched, keep requesting
    /// more scan pages in the background (reusing the ordinary
    /// `KvFetchScan`/`on_kv_scan_page` round trip, budgeted exactly like a
    /// scroll-triggered load-more) until either `FUZZY_MATCH_TARGET` matches
    /// are found or the keyspace is exhausted. This is what makes fuzzy
    /// search feel like it covers "the whole keyspace" for a query with
    /// reasonably few true matches, without ever doing a synchronous,
    /// unbounded full walk: each step is the same bounded round trip the
    /// live browse already uses, chained by `on_kv_scan_page` as pages land
    /// and re-armed here on every keystroke.
    pub(crate) fn kv_maybe_grow_pool(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        if browse.loading || browse.exhausted {
            return;
        }
        // The client-side filters (fuzzy query, TTL bucket, favourite/tag) narrow
        // the resident window rather than the scan, so a short match set can hide
        // matches that live deeper in the keyspace. While any is active and
        // under-matched, chase more pages so the grid keeps filling.
        let query = browse.filter.read(cx).content().to_string();
        let fuzzy_active = browse.is_fuzzy() && !query.is_empty();
        let ttl = browse.ttl_filter;
        let fav_only = browse.fav_only;
        let tag_active = browse.tag_filter.is_some();
        if !fuzzy_active && ttl.is_none() && !fav_only && !tag_active {
            return;
        }
        let matches = browse
            .rows
            .iter()
            .filter(|r| ttl.is_none_or(|t| t.matches(r.ttl)))
            .filter(|r| !fuzzy_active || fuzzy_score(&query, &r.key).is_some())
            .filter(|r| !fav_only || browse.fav_set.contains(&r.key))
            .filter(|r| !tag_active || browse.tag_set.contains(&r.key))
            .count();
        if matches >= FUZZY_MATCH_TARGET {
            return;
        }
        browse.loading = true;
        let epoch = browse.epoch;
        let type_filter = browse.type_filter.clone();
        let cursor = browse.cursor;
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern: None,
                type_filter,
                value_needle: None,
                cursor,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }
}

/// The type column's short label + tint, mirroring `connect.rs`'s
/// `engine_tint`/`label_color` per-kind lookup style.
/// The accent colour for a Redis type, shared by the browse [`type_pill`] and
/// the New-key modal's segmented type picker so a type reads the same everywhere.
fn kv_type_color(kv_type: &KvType, theme: &Theme) -> gpui::Hsla {
    match kv_type {
        KvType::String => theme.blue,
        KvType::Hash => theme.orange,
        KvType::List => theme.green,
        KvType::Set => theme.purple,
        KvType::ZSet => theme.yellow,
        KvType::Stream => theme.cyan,
        KvType::Other(_) => theme.text_muted,
    }
}

/// A one-line description of what creating this type seeds, shown under the type
/// picker in the New-key modal.
fn kv_create_hint(kv_type: &KvType) -> &'static str {
    match kv_type {
        KvType::String => "A single string value (SET), with an optional expiry.",
        KvType::Hash => "A hash seeded with one field → value pair (HSET).",
        KvType::List => "A list seeded with one element (LPUSH / RPUSH).",
        KvType::Set => "A set seeded with one member (SADD).",
        KvType::ZSet => "A sorted set seeded with one scored member (ZADD).",
        KvType::Stream => "A stream seeded with one entry's field → value (XADD).",
        KvType::Other(_) => "",
    }
}

/// The caption for the primary value input, which means something different per
/// type (a value, a set member, a list element).
fn kv_value_label(kv_type: &KvType) -> &'static str {
    match kv_type {
        KvType::List => "Element",
        KvType::Set | KvType::ZSet => "Member",
        _ => "Value",
    }
}

fn type_pill(kv_type: &KvType, theme: &Theme) -> impl IntoElement + use<> {
    let color = kv_type_color(kv_type, theme);
    div()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(4.))
        .bg(color.opacity(0.12))
        .text_color(color)
        .text_size(theme.scale(10.))
        .child(kv_type.label().to_string())
}

/// How many fuzzy-matched keys is "enough" before the auto-continue scan
/// (see `AppState::kv_maybe_grow_pool`) stops chasing more pages.
/// Keeps a fuzzy search from silently walking the entire keyspace just to
/// find a handful of matches, while still finding more than the first
/// page's worth for a query that's genuinely common.
const FUZZY_MATCH_TARGET: usize = 40;

/// A fast, dependency-free subsequence fuzzy match + score (fzf-ish, not a
/// byte-for-byte reimplementation): every character of `query` must appear
/// in `target` in order, not necessarily contiguously. `None` when `query`
/// isn't a subsequence of `target` at all. Higher score wins ties by
/// rewarding consecutive runs, an early match position, and a tighter
/// (shorter) overall target — the usual "closer to what you typed" signals.
/// Case-insensitive. O(len(target)) per candidate: cheap enough to run over
/// every loaded row on every keystroke without debouncing (see
/// `render_kv_browse`, where this replaces the server-side `MATCH` filter
/// in fuzzy mode rather than running alongside it).
fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    let target_lower: Vec<char> = target.to_lowercase().chars().collect();
    let mut score: i32 = 0;
    let mut qi = 0;
    let mut consecutive: i32 = 0;
    for (ti, tc) in target_lower.iter().enumerate() {
        if qi < query_lower.len() && *tc == query_lower[qi] {
            score += 10 + consecutive * 5;
            if ti == 0 || qi == 0 {
                score += 15;
            }
            consecutive += 1;
            qi += 1;
        } else {
            consecutive = 0;
        }
    }
    if qi < query_lower.len() {
        return None; // not every query character was found, in order
    }
    score -= (target_lower.len() as i32) / 4; // mild bonus for tighter targets
    Some(score)
}

/// `"no expiry"` for `None` (Redis `PTTL -1`), else a coarse "expires in Xm"
/// countdown — a static snapshot at fetch time, not a live tick (see
/// docs/plans/redis.md's deferred-polish list). Mirrors `connect.rs::fmt_ago`'s
/// bucket shape, inverted (time remaining, not elapsed).
fn fmt_ttl(ttl: Option<Duration>) -> String {
    let Some(ttl) = ttl else {
        return "no expiry".into();
    };
    let secs = ttl.as_secs();
    match secs {
        0..=59 => "expires <1m".into(),
        60..=3599 => format!("expires in {}m", secs / 60),
        3600..=86_399 => format!("expires in {}h", secs / 3600),
        _ => format!("expires in {}d", secs / 86_400),
    }
}

/// Human-readable byte count (`MEMORY USAGE`'s sampled estimate), coarse on
/// purpose (it's an estimate, not an exact size).
fn fmt_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

/// A coarse "N ago" from an already-computed seconds delta, for the analysis
/// report's "as of …" line (mirrors the slow-log's relative time).
fn fmt_ago_secs(d: i64) -> String {
    let d = d.max(0);
    if d < 60 {
        "just now".to_string()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

/// One labelled proportion bar for the analysis report: `label` on the left,
/// `right` note on the right, and a fill sized to `value/max` behind them.
/// Shared by the type, namespace, and expiry sections.
fn bar_row(
    label: &str,
    right: &str,
    value: u64,
    max: u64,
    fill: Hsla,
    theme: &Theme,
) -> gpui::AnyElement {
    let frac = if max == 0 {
        0.0
    } else {
        (value as f64 / max as f64).clamp(0.0, 1.0) as f32
    };
    div()
        .px_2()
        .py_1()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(theme.scale(11.))
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(theme.scale(10.))
                        .text_color(theme.text_muted)
                        .child(right.to_string()),
                ),
        )
        .child(
            div()
                .mt_0p5()
                .h(px(4.))
                .w_full()
                .rounded(px(2.))
                .bg(theme.border.opacity(0.4))
                .child(div().h_full().w(relative(frac)).rounded(px(2.)).bg(fill)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_meta(key: &str) -> KeyMeta {
        KeyMeta {
            key: key.to_string(),
            kv_type: KvType::String,
            ttl: None,
            encoding: String::new(),
            approx_bytes: 0,
        }
    }

    #[test]
    fn ttl_filter_buckets_match_expected_ranges() {
        use std::time::Duration;
        let secs = Duration::from_secs;
        // Permanent matches only keys with no expiry, never a finite TTL.
        assert!(TtlFilter::Permanent.matches(None));
        assert!(!TtlFilter::Permanent.matches(Some(secs(10))));
        // A permanent key never falls into any finite bucket.
        for f in TtlFilter::ALL {
            if f != TtlFilter::Permanent {
                assert!(!f.matches(None), "{f:?} should reject a permanent key");
            }
        }
        // Ending soon: 3 minutes inclusive.
        assert!(TtlFilter::EndingSoon.matches(Some(secs(180))));
        assert!(!TtlFilter::EndingSoon.matches(Some(secs(181))));
        // Bound edges are exclusive on the "under" buckets, inclusive on OverWeek.
        assert!(TtlFilter::UnderHour.matches(Some(secs(3599))));
        assert!(!TtlFilter::UnderHour.matches(Some(secs(3600))));
        assert!(TtlFilter::UnderDay.matches(Some(secs(86_399))));
        assert!(!TtlFilter::UnderDay.matches(Some(secs(86_400))));
        assert!(TtlFilter::UnderWeek.matches(Some(secs(604_799))));
        assert!(!TtlFilter::UnderWeek.matches(Some(secs(604_800))));
        assert!(TtlFilter::OverWeek.matches(Some(secs(604_800))));
        assert!(!TtlFilter::OverWeek.matches(Some(secs(604_799))));
    }

    #[test]
    fn glob_escape_neutralizes_scan_metacharacters() {
        // A literal prefix containing glob metachars must match them literally.
        assert_eq!(glob_escape("user:*"), "user:\\*");
        assert_eq!(glob_escape("a?b[c]\\d"), "a\\?b\\[c\\]\\\\d");
        // Plain text is untouched.
        assert_eq!(glob_escape("user:1"), "user:1");
    }

    #[test]
    fn query_mode_scan_pattern_per_mode() {
        // Glob passes the box text through verbatim (the user writes the glob).
        assert_eq!(
            QueryMode::Glob.scan_pattern("user:*"),
            Some("user:*".to_string())
        );
        // Prefix appends `*` and escapes any metachars in the literal prefix.
        assert_eq!(
            QueryMode::Prefix.scan_pattern("user:"),
            Some("user:*".to_string())
        );
        assert_eq!(
            QueryMode::Prefix.scan_pattern("a*b"),
            Some("a\\*b*".to_string())
        );
        // Exact/Fuzzy/Value don't scan by pattern.
        assert_eq!(QueryMode::Exact.scan_pattern("user:1"), None);
        assert_eq!(QueryMode::Fuzzy.scan_pattern("usr"), None);
        assert_eq!(QueryMode::Value.scan_pattern("hello"), None);
        // An empty box never yields a pattern, in any mode.
        assert_eq!(QueryMode::Glob.scan_pattern(""), None);
        assert_eq!(QueryMode::Prefix.scan_pattern(""), None);
    }

    #[test]
    fn build_tree_groups_by_colon_and_honors_expansion() {
        let rows = vec![
            key_meta("user:1:name"),
            key_meta("user:2:name"),
            key_meta("session:abc"),
            key_meta("flat"),
        ];
        // All collapsed: only the top-level folders + the flat leaf show, in
        // BTreeMap (alphabetical) order: flat (leaf), session, user.
        let collapsed = build_tree(&rows, &HashSet::new());
        assert_eq!(collapsed.len(), 3);
        assert!(matches!(&collapsed[0], DispRow::Key { label, .. } if label == "flat"));
        assert!(
            matches!(&collapsed[1], DispRow::Folder { prefix, count, .. } if prefix == "session" && *count == 1)
        );
        assert!(
            matches!(&collapsed[2], DispRow::Folder { prefix, count, .. } if prefix == "user" && *count == 2)
        );

        // Expand `user`: its two `:name`-terminated branches surface as sub-folders.
        let mut expanded = HashSet::new();
        expanded.insert("user".to_string());
        let opened = build_tree(&rows, &expanded);
        // session(folder), user(folder), user:1(folder), user:2(folder), flat(leaf)
        assert_eq!(opened.len(), 5);
        assert!(
            opened
                .iter()
                .any(|r| matches!(r, DispRow::Folder { prefix, .. } if prefix == "user:1"))
        );
    }

    #[test]
    fn all_tree_prefixes_are_every_strict_ancestor() {
        let rows = vec![
            key_meta("user:1:name"),
            key_meta("user:2:name"),
            key_meta("session:abc"),
            key_meta("flat"),
        ];
        let prefixes = all_tree_prefixes(&rows);
        // Strict ancestors only: `user`, `user:1`, `user:2`, `session`. A key with
        // no `:` ("flat") and the full keys themselves are never folders.
        let mut got: Vec<String> = prefixes.into_iter().collect();
        got.sort();
        assert_eq!(got, ["session", "user", "user:1", "user:2"]);
        // Expanding all these opens every folder `build_tree` would create.
        let mut expanded = HashSet::new();
        expanded.extend(all_tree_prefixes(&rows));
        assert!(
            !build_tree(&rows, &expanded)
                .iter()
                .any(|r| matches!(r, DispRow::Folder { expanded, .. } if !*expanded))
        );
    }

    #[test]
    fn build_tree_expands_leaves_fully() {
        let rows = vec![key_meta("a:b:c")];
        let mut expanded = HashSet::new();
        expanded.insert("a".to_string());
        expanded.insert("a:b".to_string());
        let disp = build_tree(&rows, &expanded);
        // a(folder) → a:b(folder) → c(leaf), indented by depth.
        assert_eq!(disp.len(), 3);
        assert!(
            matches!(&disp[2], DispRow::Key { label, depth, .. } if label == "c" && *depth == 2)
        );
    }

    #[test]
    fn fuzzy_score_requires_an_in_order_subsequence() {
        assert!(fuzzy_score("usr1", "user:1:profile").is_some());
        assert!(fuzzy_score("1ru", "user:1:profile").is_none()); // out of order
        assert!(fuzzy_score("xyz", "user:1:profile").is_none()); // not present
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn fuzzy_score_is_case_insensitive() {
        assert!(fuzzy_score("USR", "user:1").is_some());
        assert_eq!(fuzzy_score("usr", "user:1"), fuzzy_score("USR", "USER:1"));
    }

    #[test]
    fn fuzzy_score_prefers_consecutive_and_earlier_matches() {
        // "user" is a contiguous, leading match in the first key; only a
        // scattered subsequence in the second. The contiguous one must win.
        let contiguous = fuzzy_score("user", "user:session:1").unwrap();
        let scattered = fuzzy_score("user", "u_n_s_e_e_r").unwrap();
        assert!(
            contiguous > scattered,
            "{contiguous} should beat {scattered}"
        );
    }

    #[test]
    fn fuzzy_score_prefers_tighter_targets_on_equal_match_quality() {
        let short = fuzzy_score("abc", "abc").unwrap();
        let long = fuzzy_score("abc", "abc-followed-by-a-lot-of-other-text").unwrap();
        assert!(short > long);
    }
}
