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
    div, point, prelude::*, px, relative, App, AsyncApp, Context, Entity, FocusHandle, Focusable,
    Hsla, MouseButton, ScrollHandle, SharedString, Subscription, UniformListScrollHandle,
    WeakEntity, Window,
};
use red_core::kv::{
    CollectionKind, KeyMeta, KvCollection, KvCollectionPage, KvElement, KvStreamActionReq,
    KvStreamPage, KvType, KvValue, PendingEntry, RecycledKey, ScanBudget, ScanCursor, StreamAction,
    StreamConsumer, StreamEntry, StreamGroup,
};
use red_service::{Command, SessionId};

use crate::app::{
    ActiveConn, AppState, Notification, NotificationAction, Phase, RecycleBatch, SplitHalf,
    SplitState, TabWorkspace, WorkspaceTab, RECYCLE_BIN_CAP,
};

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
    if s.is_empty() {
        None
    } else {
        Some(s)
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
    pub(crate) epoch: u64,
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
/// collected metadata up into a persisted [`RedisAnalysis`] instead of a
/// biggest-keys list. `report` is `None` until either a run finishes or a saved
/// report is loaded for the connection.
pub(crate) struct AnalysisState {
    /// A dedicated scan epoch, distinct from the browse/big-keys epochs.
    pub(crate) epoch: u64,
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
    pub(crate) epoch: u64,
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
    /// `true`: the filter box is a client-side fuzzy query over already-
    /// loaded rows (see `fuzzy_score`), auto-continuing the scan in the
    /// background while under-matched (`kv_maybe_grow_fuzzy_pool`) instead
    /// of the server-side `SCAN ... MATCH` glob filter. Toggled by the
    /// filter box's "fuzzy" button; switching it on drops any active `MATCH`
    /// pattern and restarts unfiltered, since a glob-filtered pool would
    /// silently hide keys the fuzzy query could otherwise have matched.
    pub(crate) fuzzy: bool,
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
}

/// The "New key" popover's state: a name, the chosen type, and the per-type
/// seed inputs (a hash needs a field, a zset a score; everything else just a
/// value/member). A new key is created by its first write — `SET`/`HSET`/
/// `RPUSH`/`SADD`/`ZADD` — so this reuses the same [`KvEdit`](red_core::kv::KvEdit)
/// path as element editing; stream creation (no `XADD` edit) isn't offered.
pub(crate) struct CreateKeyState {
    pub(crate) name: Entity<TextInput>,
    /// Hash field name (only shown/used when `kv_type` is `Hash`).
    pub(crate) field: Entity<TextInput>,
    /// The value, set member, list element, or zset member.
    pub(crate) value: Entity<TextInput>,
    /// ZSet score (only shown/used when `kv_type` is `ZSet`).
    pub(crate) score: Entity<TextInput>,
    pub(crate) kv_type: KvType,
    pub(crate) type_open: bool,
    pub(crate) error: Option<String>,
}

/// The key types the "New key" popover can create, in menu order. Stream is
/// omitted (no `XADD` edit exists), matching the element-edit path's coverage.
fn kv_creatable_types() -> [KvType; 5] {
    [
        KvType::String,
        KvType::Hash,
        KvType::List,
        KvType::Set,
        KvType::ZSet,
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
/// [`ActiveConn`] for a Redis session only (`None` for a SQL one).
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
        let filter = cx.new(|cx| TextInput::new(cx).with_placeholder("Filter (MATCH pattern)…"));
        cx.subscribe(&filter, move |this, input, event: &TextInputEvent, cx| {
            // Only the active (visible, focused) tab can receive input events
            // in the no-split shell, so routing to the active Browse tab is
            // unambiguous here (see docs/plans/redis-workflow-parity.md).
            let fuzzy = this
                .conn_mut(Some(session))
                .and_then(|a| a.kv_view.as_ref())
                .and_then(|v| v.active_browse())
                .map(|b| b.fuzzy)
                .unwrap_or(false);
            match event {
                // Enter restarts immediately, bypassing the debounce wait.
                // No-op in fuzzy mode: there's no server round trip to fire
                // early, filtering already happens live at render time.
                TextInputEvent::Submit if !fuzzy => {
                    let pattern = input.read(cx).content().to_string();
                    this.kv_restart_scan(session, non_empty(pattern), cx);
                }
                TextInputEvent::Change if !fuzzy => {
                    let pattern = input.read(cx).content().to_string();
                    this.kv_debounce_filter(session, pattern, cx);
                }
                TextInputEvent::Change => {
                    // Fuzzy filtering itself reads the input live at render
                    // time (see `render_kv_browse`); this just needs to (a)
                    // repaint and (b) keep the candidate pool growing if the
                    // new query is under-matched.
                    this.kv_maybe_grow_fuzzy_pool(session, cx);
                    cx.notify();
                }
                _ => {}
            }
        })
        .detach();
        Self {
            epoch: crate::result::next_kv_epoch(),
            pattern: None,
            type_filter: None,
            type_filter_open: false,
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
            fuzzy: false,
            inspector: None,
            big_keys: None,
            create_key: None,
            evicted: false,
            tree_mode: false,
            expanded: HashSet::new(),
            expand_gen: 0,
            tree_cache: RefCell::new(None),
        }
    }

    /// The flattened namespace tree over `rows` (grouped by `:`), honoring the
    /// expanded set. Memoized on `(rows identity, expand_gen)` so it rebuilds
    /// only when the loaded keys or the open/closed branches change.
    fn tree_rows(&self, rows: &Rc<Vec<KeyMeta>>) -> Rc<Vec<DispRow>> {
        let ptr = Rc::as_ptr(rows);
        if let Some(cached) = self.tree_cache.borrow().as_ref() {
            if cached.rows_ptr == ptr && cached.expand_gen == self.expand_gen {
                return Rc::clone(&cached.disp);
            }
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
        if !self.fuzzy {
            return Rc::clone(&self.rows);
        }
        let query = self.filter.read(cx).content().to_string();
        if query.is_empty() {
            return Rc::clone(&self.rows);
        }
        if let Some(cached) = self.visible_cache.borrow().as_ref() {
            if cached.query == query && cached.gen == self.rows_gen {
                return Rc::clone(&cached.rows);
            }
        }
        let mut scored: Vec<(i32, &KeyMeta)> = self
            .rows
            .iter()
            .filter_map(|r| fuzzy_score(&query, &r.key).map(|s| (s, r)))
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        let result = Rc::new(
            scored
                .into_iter()
                .map(|(_, r)| r.clone())
                .collect::<Vec<_>>(),
        );
        *self.visible_cache.borrow_mut() = Some(VisibleRowsCache {
            query,
            gen: self.rows_gen,
            rows: Rc::clone(&result),
        });
        result
    }
}

/// Memoized fuzzy [`BrowseState::visible_rows`] result (see that field).
struct VisibleRowsCache {
    query: String,
    gen: u64,
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
            new_tab_focus: cx.focus_handle(),
            new_tab_sel: 0,
        }
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
    pub(crate) fn browse_by_scan_epoch_mut(&mut self, epoch: u64) -> Option<&mut BrowseState> {
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
    pub(crate) fn browse_by_big_keys_epoch_mut(&mut self, epoch: u64) -> Option<&mut BrowseState> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::Browse(b) if b.big_keys.as_ref().is_some_and(|bk| bk.epoch == epoch) => {
                Some(&mut **b)
            }
            _ => None,
        })
    }
    pub(crate) fn analysis_by_epoch_mut(&mut self, epoch: u64) -> Option<&mut AnalysisState> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::Analysis(a) if a.epoch == epoch => Some(a),
            _ => None,
        })
    }
    pub(crate) fn console_by_epoch_mut(
        &mut self,
        epoch: u64,
    ) -> Option<&mut crate::kvconsole::KvConsole> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::Console(c) if c.epoch == epoch => Some(c),
            _ => None,
        })
    }
    pub(crate) fn monitor_by_epoch_mut(
        &mut self,
        epoch: u64,
    ) -> Option<&mut crate::kvmonitor::KvMonitor> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::Monitor(m) if m.epoch == epoch => Some(m),
            _ => None,
        })
    }
    pub(crate) fn pubsub_by_epoch_mut(
        &mut self,
        epoch: u64,
    ) -> Option<&mut crate::kvpubsub::KvPubSub> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            RedisTabState::PubSub(p) if p.epoch == epoch => Some(p),
            _ => None,
        })
    }
    pub(crate) fn keyspace_by_epoch_mut(
        &mut self,
        epoch: u64,
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
                cursor: ScanCursor::START,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }

    /// The filter box changed via typing (not Enter): wait `FILTER_DEBOUNCE_MS`
    /// of no further typing before restarting the scan, so a fast typist
    /// doesn't fire one `KvFetchScan` per keystroke. Mirrors `connect.rs`'s
    /// `connect_gen` generation-check shape: bump `filter_gen` now, capture
    /// it, and only act in the timer callback if it's still current; any
    /// later `Change` (or an intervening `Submit`, which restarts directly
    /// and leaves this generation stale) makes this callback a no-op.
    pub(crate) fn kv_debounce_filter(
        &mut self,
        session: SessionId,
        pattern: String,
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
                let still_current = this
                    .conn_mut(Some(session))
                    .and_then(|a| a.kv_view.as_ref())
                    .and_then(|v| v.active_browse())
                    .is_some_and(|b| b.filter_gen == generation);
                if still_current {
                    this.kv_restart_scan(session, non_empty(pattern), cx);
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
        if let Some(p) = pattern.clone() {
            if let Some(browse) = self
                .conn_mut(Some(session))
                .and_then(|a| a.kv_view.as_mut())
                .and_then(|v| v.active_browse_mut())
            {
                browse.filter.update(cx, |ti, cx| ti.set_content(&p, cx));
            }
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
        let old_epoch = browse.epoch;
        let new_epoch = crate::result::next_kv_epoch();
        browse.epoch = new_epoch;
        browse.rows_mut().clear();
        browse.evicted = false;
        browse.cursor = ScanCursor::START;
        browse.exhausted = false;
        browse.loading = true;
        let pattern = browse.pattern.clone();
        let type_filter = browse.type_filter.as_ref().map(|t| t.label().to_string());
        self.service
            .send_to(session, Command::CloseResult { epoch: old_epoch });
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch: new_epoch,
                pattern,
                type_filter,
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
        let type_filter = browse.type_filter.as_ref().map(|t| t.label().to_string());
        let cursor = browse.cursor;
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern,
                type_filter,
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
        epoch: u64,
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
            if !browse.fuzzy {
                if let Some(n) = browse.nav_row.as_mut() {
                    *n = n.saturating_sub(drop);
                }
            }
        }
        browse.cursor = page.next_cursor;
        browse.exhausted = page.exhausted;
        browse.loading = false;
        cx.notify();
        // Outside the `browse` borrow: if a fuzzy search is under-matched,
        // this page landing is what chains the next one (see
        // `kv_maybe_grow_fuzzy_pool`'s doc comment for the full loop shape).
        if let Some(session) = session {
            self.kv_maybe_grow_fuzzy_pool(session, cx);
        }
    }

    /// Toggle between the server-side `MATCH` glob filter and client-side
    /// fuzzy filtering. Turning fuzzy on drops any active glob pattern and
    /// restarts unfiltered: a glob-filtered pool would silently exclude keys
    /// the fuzzy query could otherwise have matched.
    pub(crate) fn kv_toggle_fuzzy(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        browse.fuzzy = !browse.fuzzy;
        let had_pattern = browse.pattern.is_some();
        let now_fuzzy = browse.fuzzy;
        cx.notify();
        if now_fuzzy && had_pattern {
            self.kv_restart_scan(session, None, cx);
        }
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
    pub(crate) fn kv_maybe_grow_fuzzy_pool(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        if !browse.fuzzy || browse.loading || browse.exhausted {
            return;
        }
        let query = browse.filter.read(cx).content().to_string();
        if query.is_empty() {
            return;
        }
        let matches = browse
            .rows
            .iter()
            .filter(|r| fuzzy_score(&query, &r.key).is_some())
            .count();
        if matches >= FUZZY_MATCH_TARGET {
            return;
        }
        browse.loading = true;
        let epoch = browse.epoch;
        let type_filter = browse.type_filter.as_ref().map(|t| t.label().to_string());
        let cursor = browse.cursor;
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern: None,
                type_filter,
                cursor,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }

    /// Kick off a "find biggest keys" sample (see `BigKeysState`'s doc
    /// comment): a fresh, dedicated scan epoch that keeps requesting pages
    /// until it's exhausted the keyspace or hit the sample's own bounds.
    pub(crate) fn kv_start_big_keys_sample(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = crate::result::next_kv_epoch();
        browse.big_keys = Some(BigKeysState {
            epoch,
            cursor: ScanCursor::START,
            sampled: 0,
            running: true,
            started: std::time::Instant::now(),
            results: Vec::new(),
        });
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern: None,
                type_filter: None,
                cursor: ScanCursor::START,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }

    fn on_big_keys_page(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        page: red_core::kv::KvScanPage,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(bk) = &mut browse.big_keys else {
            return;
        };
        if bk.epoch != epoch {
            return;
        }
        bk.sampled += page.keys.len();
        bk.results.extend(page.keys);
        bk.cursor = page.next_cursor;
        let over_budget = bk.sampled >= BIG_KEYS_SAMPLE_CAP
            || bk.started.elapsed() >= Duration::from_millis(BIG_KEYS_SAMPLE_MS);
        if page.exhausted || over_budget {
            bk.running = false;
            bk.results
                .sort_by_key(|k| std::cmp::Reverse(k.approx_bytes));
            bk.results.truncate(BIG_KEYS_TOP_N);
            cx.notify();
            return;
        }
        let cursor = bk.cursor;
        let Some(session) = session else { return };
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern: None,
                type_filter: None,
                cursor,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }

    /// Dismiss the big-keys sample (running or finished) and return to the
    /// live browse.
    pub(crate) fn kv_close_big_keys(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(bk) = browse.big_keys.take() else {
            return;
        };
        self.service
            .send_to(session, Command::CloseResult { epoch: bk.epoch });
        cx.notify();
    }

    /// Load the persisted analysis report for this connection into the panel,
    /// the first time it's opened this session (see `redis_analysis.rs`). A
    /// no-op if a run has already produced a fresher report.
    pub(crate) fn kv_load_saved_analysis(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let conn_id = self
            .conn_mut(Some(session))
            .map(|a| a.conn_id.clone())
            .unwrap_or_default();
        let saved = self.redis_analysis.get(&conn_id).cloned();
        let Some(analysis) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_analysis_mut())
        else {
            return;
        };
        if analysis.loaded {
            return;
        }
        analysis.loaded = true;
        if analysis.report.is_none() {
            analysis.report = saved;
        }
        cx.notify();
    }

    /// Start a fresh keyspace-analysis run: a dedicated scan epoch that chains
    /// pages (like the biggest-keys sampler) until the keyspace is exhausted or
    /// the analysis budget is hit, then rolls the sample up and persists it.
    pub(crate) fn kv_run_analysis(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(analysis) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_analysis_mut())
        else {
            return;
        };
        if analysis.running {
            return;
        }
        let epoch = crate::result::next_kv_epoch();
        analysis.epoch = epoch;
        analysis.cursor = ScanCursor::START;
        analysis.running = true;
        analysis.started = std::time::Instant::now();
        analysis.collected.clear();
        analysis.loaded = true;
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern: None,
                type_filter: None,
                cursor: ScanCursor::START,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }

    fn on_analysis_page(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        page: red_core::kv::KvScanPage,
        cx: &mut Context<Self>,
    ) {
        // First mutate the run state under the browse borrow; decide whether it
        // finished, and if so compute the report (needs `db_size` too).
        let (finished, report, conn_id) = {
            let Some(active) = self.conn_mut(session) else {
                return;
            };
            let conn_id = active.conn_id.clone();
            let Some(view) = &mut active.kv_view else {
                return;
            };
            // `DBSIZE` is connection-level; read it before borrowing the tab.
            let total_keys = view.db_size.unwrap_or(0);
            let Some(analysis) = view.analysis_by_epoch_mut(epoch) else {
                return;
            };
            if !analysis.running {
                return;
            }
            analysis.collected.extend(page.keys);
            analysis.cursor = page.next_cursor;
            let over_budget = analysis.collected.len() >= ANALYSIS_SAMPLE_CAP
                || analysis.started.elapsed() >= Duration::from_millis(ANALYSIS_SAMPLE_MS);
            if page.exhausted || over_budget {
                analysis.running = false;
                let truncated = !page.exhausted;
                let report = red_core::kv::analyze_keyspace(
                    &analysis.collected,
                    total_keys,
                    truncated,
                    crate::conversations::now_unix() as i64,
                );
                analysis.report = Some(report.clone());
                // Drop the raw sample now that it's rolled up.
                analysis.collected = Vec::new();
                (true, Some(report), conn_id)
            } else {
                (false, None, conn_id)
            }
        };

        if finished {
            if let Some(report) = report {
                // Persist the fresh report so it survives a restart.
                self.redis_analysis.set(&conn_id, report);
            }
            cx.notify();
            return;
        }

        // Not finished: chain the next page (outside the browse borrow).
        let Some(session) = session else { return };
        let cursor = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.analysis_by_epoch_mut(epoch))
            .map(|a| a.cursor);
        if let Some(cursor) = cursor {
            self.service.send_to(
                session,
                Command::KvFetchScan {
                    epoch,
                    pattern: None,
                    type_filter: None,
                    cursor,
                    budget: scan_budget(),
                },
            );
        }
        cx.notify();
    }

    /// Stop an in-progress analysis run (leaves any already-shown report).
    pub(crate) fn kv_cancel_analysis(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(analysis) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_analysis_mut())
        else {
            return;
        };
        if !analysis.running {
            return;
        }
        analysis.running = false;
        analysis.collected = Vec::new();
        let epoch = analysis.epoch;
        self.service
            .send_to(session, Command::CloseResult { epoch });
        cx.notify();
    }

    /// Open a new blank tab in the focused half (the ＋ / ⌘T action). Its body
    /// shows the type chooser; picking a kind converts it in place via
    /// [`kv_set_tab_kind`]. Mirrors the SQL side's `new_query`.
    pub(crate) fn kv_new_empty_tab(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        let half = view.focused_half();
        let id = view.tab_seq;
        view.tab_seq += 1;
        view.tabs.push(RedisTab {
            id,
            title: "New tab".to_string(),
            state: RedisTabState::Empty,
            pane: half,
            pinned: false,
        });
        let new_idx = view.tabs.len() - 1;
        view.set_pane_active(half, new_idx);
        cx.notify();
    }

    /// Convert the (blank) tab with `id` to `kind`, retitle it, and fire its
    /// lazy first load — the empty-tab chooser's action.
    pub(crate) fn kv_set_tab_kind(
        &mut self,
        session: SessionId,
        id: u64,
        kind: KvPanel,
        cx: &mut Context<Self>,
    ) {
        let state = RedisTabState::new(kind, session, cx);
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        let Some(idx) = view.tab_index_by_id(id) else {
            return;
        };
        view.tabs[idx].state = state;
        view.tabs[idx].title = kind.label().to_string();
        let half = view.tabs[idx].pane;
        view.set_pane_active(half, idx);
        // Fire the chosen kind's lazy first load, the same way the old
        // single-panel shell did on first switch.
        match kind {
            KvPanel::Browse => self.kv_start_browse(session, cx),
            KvPanel::Monitor => self.kv_load_slowlog(session, cx),
            KvPanel::Analysis => self.kv_load_saved_analysis(session, cx),
            KvPanel::Keyspace => self.kv_keyspace_load_config(session, cx),
            KvPanel::Console | KvPanel::PubSub => {}
        }
        cx.notify();
    }

    /// Keyboard driving of the blank-tab chooser (see `render_kv_new_tab`), for
    /// the empty tab with stable id `id`: digits `1`–`6` pick a panel outright,
    /// ←/↑ and →/↓ move the highlight (wrapping), and Enter/Space commit the
    /// highlighted one. Returns `true` when it consumed the key.
    pub(crate) fn kv_new_tab_key(
        &mut self,
        session: SessionId,
        id: u64,
        key: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let n = KV_NEW_TAB_CHOICES.len();
        // A direct digit pick (`1`–`6`).
        if let Ok(d) = key.parse::<usize>() {
            if (1..=n).contains(&d) {
                self.kv_set_tab_kind(session, id, KV_NEW_TAB_CHOICES[d - 1].0, cx);
                return true;
            }
        }
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return false;
        };
        match key {
            "left" | "up" => {
                view.new_tab_sel = (view.new_tab_sel + n - 1) % n;
                cx.notify();
                true
            }
            "right" | "down" => {
                view.new_tab_sel = (view.new_tab_sel + 1) % n;
                cx.notify();
                true
            }
            "enter" | "space" => {
                let sel = view.new_tab_sel.min(n - 1);
                self.kv_set_tab_kind(session, id, KV_NEW_TAB_CHOICES[sel].0, cx);
                true
            }
            _ => false,
        }
    }

    /// Step the focused half's active tab one slot forward/back, wrapping (the
    /// ctrl-tab / ctrl-shift-tab bindings). Shares the wrap math with the SQL
    /// side via [`crate::app::tabs::cycle_tab_index`].
    pub(crate) fn kv_step_tab(
        &mut self,
        session: SessionId,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        let half = view.focused_half();
        let pane_tabs = view.pane_tab_indices(half);
        let cur = view.focused_tab_index();
        let Some(next) = crate::app::tabs::cycle_tab_index(&pane_tabs, cur, forward) else {
            return;
        };
        view.set_pane_active(half, next);
        view.tab_scroll.scroll_to_item(next);
        view.tab_menu = None;
        cx.notify();
    }

    /// Activate the tab at `index`: make it its half's active tab and focus
    /// that half (each strip shows only its own tabs, so a click never crosses).
    pub(crate) fn kv_activate_tab(
        &mut self,
        session: SessionId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        let Some(half) = view.tabs.get(index).map(|t| t.pane) else {
            return;
        };
        view.set_pane_active(half, index);
        if let Some(s) = &mut view.split {
            s.focus = half;
        }
        view.tab_menu = None;
        cx.notify();
    }

    /// Close the tab at `index`: tear down its backend subscription (MONITOR /
    /// Pub-Sub / keyspace watcher ride an epoch that must be released), drop
    /// it, and restore the pane invariants. The last tab can't be closed — the
    /// shell always shows something (mirrors the SQL invariant).
    pub(crate) fn kv_close_tab(
        &mut self,
        session: SessionId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        if index >= view.tabs.len() {
            return;
        }
        // Release any backend epoch this tab owned: a live subscription
        // (MONITOR / Pub-Sub / keyspace watcher) or an in-flight scan run
        // (browse cursor + biggest-keys sample, a running analysis walk).
        // `CloseResult` cancels the in-flight fetch at the engine too.
        let close_epochs: Vec<u64> = match &view.tabs[index].state {
            RedisTabState::Monitor(m) => vec![m.epoch],
            RedisTabState::PubSub(p) => vec![p.epoch],
            RedisTabState::Keyspace(k) => vec![k.epoch],
            RedisTabState::Browse(b) => {
                let mut v = vec![b.epoch];
                if let Some(bk) = &b.big_keys {
                    v.push(bk.epoch);
                }
                v
            }
            RedisTabState::Analysis(a) if a.running => vec![a.epoch],
            RedisTabState::Empty | RedisTabState::Analysis(_) | RedisTabState::Console(_) => {
                Vec::new()
            }
        };
        if view.tabs.len() <= 1 {
            // The view must always show a tab, so we can't remove the only one —
            // but we must still release its epoch, or a lone MONITOR/Pub-Sub/
            // keyspace tab would leak its firehose connection forever. Reset it
            // to the blank chooser in place and release below.
            view.tabs[index].state = RedisTabState::Empty;
            view.tabs[index].title = "New tab".to_string();
            view.tab_menu = None;
            for epoch in close_epochs {
                self.service
                    .send_to(session, Command::CloseResult { epoch });
            }
            cx.notify();
            return;
        }
        view.tabs.remove(index);
        // Shift the two panes' stored active indices past the removed slot,
        // then let `normalize_panes` collapse an emptied half + clamp.
        if view.active_tab > index {
            view.active_tab -= 1;
        }
        if let Some(s) = &mut view.split {
            if s.secondary > index {
                s.secondary -= 1;
            }
        }
        view.tab_menu = None;
        view.normalize_panes();
        for epoch in close_epochs {
            self.service
                .send_to(session, Command::CloseResult { epoch });
        }
        cx.notify();
    }

    // --- drag reorder (mirrors the SQL `drop_tab` / drop-target helpers) ---

    /// Move the dragged tab (`from`) into `half` and reorder it to the current
    /// drop-target gap. Clears the gap indicator.
    pub(crate) fn kv_drop_tab(
        &mut self,
        session: SessionId,
        from: usize,
        half: SplitHalf,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        if from >= view.tabs.len() {
            return;
        }
        let gap = view.tab_drop_target.take().unwrap_or(from);
        view.tabs[from].pane = half;
        // Remove then reinsert at the gap (adjusting for the removal shift).
        let tab = view.tabs.remove(from);
        let dest = if gap > from { gap - 1 } else { gap };
        let dest = dest.min(view.tabs.len());
        view.tabs.insert(dest, tab);
        view.set_pane_active(half, dest);
        if let Some(s) = &mut view.split {
            s.focus = half;
        }
        view.normalize_panes();
        cx.notify();
    }

    pub(crate) fn kv_set_tab_drop_target(
        &mut self,
        session: SessionId,
        gap: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            if view.tab_drop_target != Some(gap) {
                view.tab_drop_target = Some(gap);
                cx.notify();
            }
        }
    }

    pub(crate) fn kv_clear_tab_drop_target(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            if view.tab_drop_target.take().is_some() {
                cx.notify();
            }
        }
    }

    // --- split panes ---

    const KV_SPLIT_DEFAULT_WIDTH: f32 = 520.;

    /// Toggle the side-by-side split (the ⌘\ action, routed here for a Redis
    /// connection): open a second pane, or collapse it when already split.
    pub(crate) fn kv_toggle_split(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let split = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .is_some_and(|v| v.split.is_some());
        if split {
            self.kv_unsplit(session, cx);
        } else {
            self.kv_split_right(session, cx);
        }
    }

    /// Open the split: a fresh blank tab in a second, focused pane on the right
    /// (its body shows the type chooser). The left pane keeps its tabs; move a
    /// tab across with the tab context menu or by dragging.
    pub(crate) fn kv_split_right(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        if view.split.is_some() {
            return; // already split
        }
        let id = view.tab_seq;
        view.tab_seq += 1;
        view.tabs.push(RedisTab {
            id,
            title: "New tab".to_string(),
            state: RedisTabState::Empty,
            pane: SplitHalf::Secondary,
            pinned: false,
        });
        let secondary = view.tabs.len() - 1;
        view.split = Some(SplitState {
            secondary,
            focus: SplitHalf::Secondary,
            width: px(Self::KV_SPLIT_DEFAULT_WIDTH),
            drag: None,
        });
        view.normalize_panes();
        cx.notify();
    }

    /// Collapse the split: every tab folds into the single strip, keeping the
    /// focused half's tab on screen.
    pub(crate) fn kv_unsplit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        let Some(s) = view.split.take() else {
            return;
        };
        let keep = if s.focus == SplitHalf::Secondary {
            s.secondary
        } else {
            view.active_tab
        };
        for t in &mut view.tabs {
            t.pane = SplitHalf::Primary;
        }
        view.active_tab = keep.min(view.tabs.len().saturating_sub(1));
        cx.notify();
    }

    /// Set the focused half (a per-half mouse-down picks this, so actions target
    /// the half the user just touched). No-op when not split or unchanged.
    pub(crate) fn kv_set_split_focus(
        &mut self,
        session: SessionId,
        half: SplitHalf,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            if let Some(s) = &mut view.split {
                if s.focus != half {
                    s.focus = half;
                    cx.notify();
                }
            }
        }
    }

    /// Move focus to the other half (the ⌥⌘\ action). No-op when not split.
    pub(crate) fn kv_focus_other_half(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            if let Some(s) = &mut view.split {
                s.focus = if s.focus == SplitHalf::Primary {
                    SplitHalf::Secondary
                } else {
                    SplitHalf::Primary
                };
                cx.notify();
            }
        }
    }

    /// Move the tab with `id` to the other split half (tab context menu). If not
    /// split, opens the split first so there's a half to move to.
    pub(crate) fn kv_move_tab_to_other_half(
        &mut self,
        session: SessionId,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        let is_split = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .is_some_and(|v| v.split.is_some());
        if !is_split {
            self.kv_split_right(session, cx);
            // Then move the requested tab into the (now Secondary) pane below.
        }
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        let Some(idx) = view.tab_index_by_id(id) else {
            return;
        };
        let target = if view.tabs[idx].pane == SplitHalf::Primary {
            SplitHalf::Secondary
        } else {
            SplitHalf::Primary
        };
        view.tabs[idx].pane = target;
        view.set_pane_active(target, idx);
        if let Some(s) = &mut view.split {
            s.focus = target;
        }
        view.tab_menu = None;
        view.normalize_panes();
        cx.notify();
    }

    /// Pin/unpin the tab with `id` (pinned tabs sort ahead in their strip).
    pub(crate) fn kv_toggle_tab_pin(
        &mut self,
        session: SessionId,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            if let Some(idx) = view.tab_index_by_id(id) {
                view.tabs[idx].pinned = !view.tabs[idx].pinned;
                view.tab_menu = None;
                cx.notify();
            }
        }
    }

    /// Open / close the tab right-click context menu.
    pub(crate) fn kv_open_tab_menu(
        &mut self,
        session: SessionId,
        id: u64,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.tab_menu = Some((id, pos));
            cx.notify();
        }
    }

    pub(crate) fn kv_close_tab_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            if view.tab_menu.take().is_some() {
                cx.notify();
            }
        }
    }

    /// Open the right-click context menu for a key row (from either the live
    /// browse list or the biggest-keys sample). The type/TTL are captured now so
    /// the menu labels itself and its actions target the exact key, independent
    /// of what the inspector currently shows.
    pub(crate) fn kv_open_key_menu(
        &mut self,
        session: SessionId,
        key: String,
        kv_type: KvType,
        ttl: Option<Duration>,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.tab_menu = None;
            view.key_menu = Some(KeyMenu {
                key,
                kv_type,
                ttl,
                pos,
            });
            cx.notify();
        }
    }

    pub(crate) fn kv_close_key_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            if view.key_menu.take().is_some() {
                cx.notify();
            }
        }
    }

    /// Menu action: put `key` on the clipboard.
    pub(crate) fn kv_copy_key_name(
        &mut self,
        session: SessionId,
        key: String,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(key));
        self.kv_close_key_menu(session, cx);
    }

    /// Menu action: open the inspector on `key`, then enter one of the inline
    /// editors (rename / TTL) or raise the delete-confirm bar — reusing the
    /// inspector's existing edit flows so the menu is a shortcut, not a second
    /// implementation. `action` selects which one.
    pub(crate) fn kv_key_menu_edit(
        &mut self,
        session: SessionId,
        key: String,
        kv_type: KvType,
        ttl: Option<Duration>,
        action: KeyMenuEdit,
        cx: &mut Context<Self>,
    ) {
        self.kv_close_key_menu(session, cx);
        self.kv_open_inspector(session, key, ttl, kv_type, cx);
        match action {
            KeyMenuEdit::Rename => self.kv_start_editing_key(session, cx),
            KeyMenuEdit::Ttl => self.kv_start_editing_ttl(session, cx),
        }
    }

    /// Menu action: ask to delete `key`. Unlike the inspector (which has its own
    /// inline confirm bar), a delete straight from the list opens a proper modal
    /// — the destructive action deserves an unmissable "are you sure?".
    pub(crate) fn kv_request_delete_key(
        &mut self,
        session: SessionId,
        key: String,
        cx: &mut Context<Self>,
    ) {
        self.kv_close_key_menu(session, cx);
        self.confirm_kv_delete = Some((session, key));
        self.focus_modal = true;
        cx.notify();
    }

    pub(crate) fn kv_cancel_delete_key(&mut self, cx: &mut Context<Self>) {
        if self.confirm_kv_delete.take().is_some() {
            self.refocus_root = true;
            cx.notify();
        }
    }

    /// The modal's Delete button: commit the `DEL` against the active browse's
    /// epoch (so [`Self::on_kv_edit_applied`] patches the right list) and close.
    pub(crate) fn kv_confirm_delete_key(&mut self, cx: &mut Context<Self>) {
        let Some((session, key)) = self.confirm_kv_delete.take() else {
            return;
        };
        let epoch = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .map(|b| b.epoch);
        if let Some(epoch) = epoch {
            let edit = red_core::kv::KvEdit::Delete { keys: vec![key] };
            self.service
                .send_to(session, Command::KvApplyEdit { epoch, edit });
        }
        self.refocus_root = true;
        cx.notify();
    }

    /// Menu action: seed the Console with the natural read-all command for
    /// `key`'s type (never auto-run — the user reviews and presses Enter),
    /// reusing [`Self::kv_seed_console`].
    pub(crate) fn kv_key_menu_open_console(
        &mut self,
        session: SessionId,
        kv_type: KvType,
        key: String,
        cx: &mut Context<Self>,
    ) {
        let cmd = kv_read_command(&kv_type, &key);
        self.kv_close_key_menu(session, cx);
        self.kv_seed_console(session, cmd, cx);
    }

    /// Close the tab with `id` (the context menu's Close item; resolves the id
    /// to a current index first, since positions shift).
    pub(crate) fn kv_close_tab_by_id(
        &mut self,
        session: SessionId,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        let idx = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.tab_index_by_id(id));
        if let Some(idx) = idx {
            self.kv_close_tab(session, idx, cx);
        }
    }

    /// Bulk close from the tab context menu: Close Others / Close Left / Close
    /// Right / Close All, resolved against `id`'s own pane and skipping pinned
    /// tabs (mirrors the SQL side's [`AppState::close_tab_group`]). Targets are
    /// collected as stable ids first, then closed one by one so shifting indices
    /// stay valid; `kv_close_tab`'s "keep at least one tab" guard is respected.
    pub(crate) fn kv_close_tab_group(
        &mut self,
        session: SessionId,
        id: u64,
        scope: crate::app::TabCloseScope,
        cx: &mut Context<Self>,
    ) {
        use crate::app::TabCloseScope;
        if scope == TabCloseScope::One {
            self.kv_close_tab_by_id(session, id, cx);
            return;
        }
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
        else {
            return;
        };
        let Some(idx) = view.tab_index_by_id(id) else {
            return;
        };
        let pane = view.tabs[idx].pane;
        let siblings = view.pane_tab_indices(pane);
        let Some(pos) = siblings.iter().position(|&i| i == idx) else {
            return;
        };
        let target_indices: Vec<usize> = match scope {
            TabCloseScope::One => return,
            TabCloseScope::All => siblings.clone(),
            TabCloseScope::Others => siblings.iter().copied().filter(|&i| i != idx).collect(),
            TabCloseScope::Left => siblings[..pos].to_vec(),
            TabCloseScope::Right => siblings[pos + 1..].to_vec(),
        };
        // Resolve to stable ids now (indices shift as we close), skipping pinned
        // tabs — those close only via the explicit "Close" item.
        let target_ids: Vec<u64> = target_indices
            .into_iter()
            .filter(|&i| !view.tabs[i].pinned)
            .map(|i| view.tabs[i].id)
            .collect();
        for target in target_ids {
            self.kv_close_tab_by_id(session, target, cx);
        }
    }

    pub(crate) fn on_kv_db_size(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        count: u64,
        cx: &mut Context<Self>,
    ) {
        // `DBSIZE` is connection-level: store it on the view (shared by every
        // Browse tab), matched against the browse tab that requested it.
        let Some(view) = self.conn_mut(session).and_then(|a| a.kv_view.as_mut()) else {
            return;
        };
        if view.browse_by_scan_epoch_mut(epoch).is_none() {
            return;
        }
        view.db_size = Some(count);
        cx.notify();
    }

    /// A keyspace row was selected: open the inspector on it and kick off
    /// `KvReadValue`. Replaces whatever the inspector was showing before.
    /// Open the inspector on `key` (called with the resolved `KeyMeta`
    /// fields rather than a row index, so both the live browse table and
    /// the biggest-keys sample's table — two different backing `Vec`s — can
    /// open the same inspector without this needing to know which list a
    /// selection came from).
    pub(crate) fn kv_open_inspector(
        &mut self,
        session: SessionId,
        key: String,
        ttl: Option<Duration>,
        kv_type: KvType,
        cx: &mut Context<Self>,
    ) {
        let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
        else {
            return;
        };
        let epoch = browse.epoch;

        // Record this key in the connection's recently-viewed list (newest-first,
        // deduped, capped) — the History dock's Keys section reads it.
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.recent_keys.retain(|r| r.key != key);
            view.recent_keys.insert(
                0,
                RecentKey {
                    key: key.clone(),
                    kv_type: kv_type.clone(),
                    ttl,
                    viewed_unix: crate::conversations::now_unix(),
                },
            );
            view.recent_keys.truncate(MAX_RECENT_KEYS);
        }
        self.kv_persist_recent_keys(session);

        // A multiline surface (no gutter, no frame of its own) so it reads as
        // the value body becoming editable in place, exactly like the SQL cell
        // inspector's inline editor. ⌘↵ (Run) saves; Esc cancels. Enter inserts
        // a newline, so multi-line JSON stays editable.
        let value_editor = cx.new(|cx| {
            CodeEditor::new(cx)
                .gutter(false)
                .resting_border(false)
                .corner_radius(px(0.))
                .soft_wrap(true)
                .a11y_label("Key value editor")
        });
        cx.subscribe(
            &value_editor,
            move |this, _, event: &CodeEditorEvent, cx| match event {
                CodeEditorEvent::Run => this.kv_submit_value_edit(session, cx),
                CodeEditorEvent::Escape => this.kv_cancel_editing_value(session, cx),
                _ => {}
            },
        )
        .detach();
        let ttl_editor =
            cx.new(|cx| TextInput::new(cx).with_placeholder("seconds, blank = no expiry"));
        cx.subscribe(&ttl_editor, move |this, _, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submit) {
                this.kv_submit_ttl_edit(session, cx);
            }
        })
        .detach();
        let rename_editor = cx.new(TextInput::new);
        cx.subscribe(
            &rename_editor,
            move |this, _, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Submit) {
                    this.kv_submit_rename(session, cx);
                }
            },
        )
        .detach();
        let claim_editor = cx.new(|cx| TextInput::new(cx).with_placeholder("claim to consumer…"));
        cx.subscribe(&claim_editor, move |this, _, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submit) {
                this.kv_submit_claim(session, cx);
            }
        })
        .detach();
        let elem_name_editor = cx.new(TextInput::new);
        cx.subscribe(
            &elem_name_editor,
            move |this, _, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Submit) {
                    this.kv_submit_collection_edit(session, cx);
                }
            },
        )
        .detach();
        let elem_value_editor = cx.new(TextInput::new);
        cx.subscribe(
            &elem_value_editor,
            move |this, _, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Submit) {
                    this.kv_submit_collection_edit(session, cx);
                }
            },
        )
        .detach();

        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        browse.inspector = Some(KvInspector {
            key: key.clone(),
            kv_type,
            ttl,
            value: None,
            collection_rows: Rc::new(Vec::new()),
            collection_cursor: 0,
            collection_exhausted: false,
            collection_head_only: false,
            collection_loading: false,
            collection_scroll: UniformListScrollHandle::new(),
            stream_rows: Rc::new(Vec::new()),
            stream_before: None,
            stream_exhausted: false,
            stream_loading: false,
            stream_scroll: UniformListScrollHandle::new(),
            stream_groups: StreamGroupsState {
                view: StreamView::Entries,
                loaded: false,
                loading: false,
                groups: Vec::new(),
                selected: None,
                consumers: Vec::new(),
                pending: Vec::new(),
                detail_loading: false,
                claiming: None,
                claim_editor,
            },
            value_editor,
            editing_value: false,
            str_preview: None,
            ttl_editor,
            editing_ttl: false,
            rename_editor,
            editing_key: false,
            confirm_delete: false,
            str_format: crate::inspector::ValueFormat::Auto,
            loading_full_value: false,
            edit_after_load: false,
            elem_name_editor,
            elem_value_editor,
            collection_edit: None,
            elem_error: None,
            value_loaded: false,
            value_error: None,
            ttl_error: None,
        });
        self.service
            .send_to(session, Command::KvReadValue { epoch, key });
        cx.notify();
    }

    /// Open a recently-viewed key (the History dock's Keys section): make sure
    /// the focused half shows a Browse tab, then open the inspector on it.
    pub(crate) fn kv_open_recent_key(
        &mut self,
        session: SessionId,
        key: String,
        kv_type: KvType,
        ttl: Option<Duration>,
        cx: &mut Context<Self>,
    ) {
        let is_browse = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .is_some_and(|v| matches!(v.active_state(), Some(RedisTabState::Browse(_))));
        if !is_browse {
            self.kv_new_empty_tab(session, cx);
            let id = self
                .conn_mut(Some(session))
                .and_then(|a| a.kv_view.as_ref())
                .and_then(|v| v.tabs.get(v.focused_tab_index()))
                .map(|t| t.id);
            if let Some(id) = id {
                self.kv_set_tab_kind(session, id, KvPanel::Browse, cx);
            }
        }
        self.kv_open_inspector(session, key, ttl, kv_type, cx);
    }

    /// Clear the connection's recently-viewed keys (the History dock's trash).
    pub(crate) fn kv_clear_recent_keys(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            if !view.recent_keys.is_empty() {
                view.recent_keys.clear();
                self.kv_persist_recent_keys(session);
                cx.notify();
            }
        }
    }

    /// Seed a freshly-connected Redis view's recently-viewed list from the
    /// persisted store, so browsing history survives a restart.
    pub(crate) fn kv_seed_recent_keys(&mut self, session: SessionId, conn_id: &str) {
        let seeded: Vec<RecentKey> = self
            .redis_recent_keys
            .get(conn_id)
            .map(|recs| recs.iter().map(RecentKey::from_rec).collect())
            .unwrap_or_default();
        if seeded.is_empty() {
            return;
        }
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.recent_keys = seeded;
        }
    }

    /// Write the connection's current recently-viewed list to the persisted
    /// store (called after any change: record / clear / remove).
    fn kv_persist_recent_keys(&mut self, session: SessionId) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let conn_id = active.conn_id.clone();
        if conn_id.is_empty() {
            return;
        }
        let recs: Vec<crate::recent_keys::RecentKeyRec> = active
            .kv_view
            .as_ref()
            .map(|v| v.recent_keys.iter().map(RecentKey::to_rec).collect())
            .unwrap_or_default();
        self.redis_recent_keys.set(&conn_id, recs);
    }

    /// Drop a single recently-viewed key from the History dock's Keys section
    /// (the per-row remove button), leaving the rest of the list intact.
    pub(crate) fn kv_remove_recent_key(
        &mut self,
        session: SessionId,
        key: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            let before = view.recent_keys.len();
            view.recent_keys.retain(|r| r.key != key);
            if view.recent_keys.len() != before {
                self.kv_persist_recent_keys(session);
                cx.notify();
            }
        }
    }

    /// Change the string inspector's display lens (Auto/Raw/JSON/Hex or a
    /// binary decoder).
    pub(crate) fn kv_set_str_format(
        &mut self,
        session: SessionId,
        fmt: crate::inspector::ValueFormat,
        cx: &mut Context<Self>,
    ) {
        let Some(inspector) = self.kv_inspector_mut(session) else {
            return;
        };
        inspector.str_format = fmt;
        // The preview renders through the lens, so rebuild it under the new one.
        self.kv_rebuild_str_preview(session, cx);
        cx.notify();
    }

    /// "Load full value": re-fetch the inspector's string key in full (a plain
    /// `GET`, no cap), for a value `read_value` returned as a `Value::Capped`.
    /// The reply comes back on `Event::KvValueReady` and replaces the capped
    /// body in place, mirroring the SQL cell inspector's load-full flow.
    /// Copy the inspected string value to the clipboard. Copies whatever's
    /// resident — the full text, or a capped value's loaded head (use "Load
    /// full value" first to copy the whole thing).
    pub(crate) fn kv_copy_string_value(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let text = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .and_then(|b| b.inspector.as_ref())
            .and_then(|i| match i.value.as_ref()? {
                KvValue::Str(red_core::Value::Text(s)) => Some(s.to_string()),
                KvValue::Str(red_core::Value::Capped(c)) => Some(c.head.clone()),
                KvValue::Str(other) => Some(format!("{other:?}")),
                _ => None,
            });
        if let Some(text) = text {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    pub(crate) fn kv_load_full_value(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        if inspector.loading_full_value {
            return;
        }
        inspector.loading_full_value = true;
        let key = inspector.key.clone();
        self.service
            .send_to(session, Command::KvReadStringFull { epoch, key });
        cx.notify();
    }

    /// Build (or drop) the read-only, selectable preview editor for the
    /// inspector's string value, keyed off the current value + lens. Called
    /// whenever either changes — not per frame — so an in-progress selection
    /// and scroll survive. A non-string value, a still-loading value, or an
    /// open edit leaves `str_preview` empty (those render their own body).
    fn kv_rebuild_str_preview(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(inspector) = self.kv_inspector_mut(session) else {
            return;
        };
        let build = match (&inspector.value, inspector.editing_value) {
            (Some(KvValue::Str(v)), false) => Some((v.clone(), inspector.str_format)),
            _ => None,
        };
        let Some((value, fmt)) = build else {
            inspector.str_preview = None;
            return;
        };
        let (body, _summary, wrap) = crate::inspector::format_value_body(&value, fmt);
        let editor = cx.new(|cx| {
            let mut e = CodeEditor::new(cx)
                .gutter(false)
                .resting_border(false)
                .corner_radius(px(0.))
                .soft_wrap(wrap)
                .a11y_label("Key value")
                .with_content(body);
            e.set_read_only(true, cx);
            e
        });
        // Esc from the focused preview closes the inspector, matching Esc from
        // the keyspace grid (the editor swallows the key otherwise).
        let sub = cx.subscribe(&editor, move |this, _, event: &CodeEditorEvent, cx| {
            if matches!(event, CodeEditorEvent::Escape) {
                this.kv_close_inspector(session, cx);
            }
        });
        let Some(inspector) = self.kv_inspector_mut(session) else {
            return;
        };
        inspector.str_preview = Some(KvStrPreview { editor, sub });
    }

    pub(crate) fn kv_close_inspector(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        browse.inspector = None;
        cx.notify();
    }

    // --- editing (see docs/plans/redis.md's editing phase) ---

    pub(crate) fn kv_start_editing_value(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        // A `read_value`-capped string only holds its head; editing must run on
        // the whole value or a save would truncate the key. Fetch the full
        // string first and defer opening the editor to `on_kv_value_ready`.
        if matches!(
            &inspector.value,
            Some(KvValue::Str(red_core::Value::Capped(_)))
        ) {
            inspector.edit_after_load = true;
            self.kv_load_full_value(session, cx);
            return;
        }
        let seed = match &inspector.value {
            Some(KvValue::Str(v)) => render_string_preview(v),
            _ => String::new(),
        };
        inspector
            .value_editor
            .update(cx, |ed, cx| ed.set_content(seed, cx));
        inspector.editing_value = true;
        // The editor owns the body while editing, so drop the read-only preview.
        inspector.str_preview = None;
        cx.notify();
    }

    pub(crate) fn kv_cancel_editing_value(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.editing_value = false;
        // Restore the selectable read-only preview now the editor is gone.
        self.kv_rebuild_str_preview(session, cx);
        cx.notify();
    }

    pub(crate) fn kv_submit_value_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &browse.inspector else {
            return;
        };
        let key = inspector.key.clone();
        // Preserve the key's existing TTL: `KEEPTTL` retains the server's actual
        // expiry exactly, so editing the value neither clears nor resets the
        // countdown (a re-applied `EX` snapshot would do both).
        let value = inspector.value_editor.read(cx).content();
        let edit = red_core::kv::KvEdit::SetString {
            key,
            value,
            ttl: red_core::kv::StringTtl::Keep,
        };
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
    }

    pub(crate) fn kv_start_editing_ttl(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        let seed = inspector
            .ttl
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        inspector
            .ttl_editor
            .update(cx, |ti, cx| ti.set_content(seed, cx));
        inspector.ttl_error = None;
        inspector.editing_ttl = true;
        cx.notify();
    }

    pub(crate) fn kv_cancel_editing_ttl(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.editing_ttl = false;
        cx.notify();
    }

    /// Blank input persists the key (no expiry); otherwise parses as whole
    /// seconds. An unparseable, non-blank input reports inline in the popover
    /// (`ttl_error`) rather than silently doing nothing.
    pub(crate) fn kv_submit_ttl_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some((epoch, key, text)) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| {
                let epoch = b.epoch;
                let inspector = b.inspector.as_ref()?;
                Some((
                    epoch,
                    inspector.key.clone(),
                    inspector.ttl_editor.read(cx).content().to_string(),
                ))
            })
        else {
            return;
        };
        let ttl = if text.trim().is_empty() {
            None
        } else {
            match text.trim().parse::<u64>() {
                Ok(secs) => Some(Duration::from_secs(secs)),
                Err(_) => {
                    if let Some(inspector) = self
                        .conn_mut(Some(session))
                        .and_then(|a| a.kv_view.as_mut())
                        .and_then(|v| v.active_browse_mut())
                        .and_then(|b| b.inspector.as_mut())
                    {
                        inspector.ttl_error =
                            Some("Enter whole seconds, or leave blank to persist".into());
                    }
                    cx.notify();
                    return;
                }
            }
        };
        if let Some(inspector) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.inspector.as_mut())
        {
            inspector.ttl_error = None;
        }
        let edit = red_core::kv::KvEdit::SetTtl { key, ttl };
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
    }

    pub(crate) fn kv_start_editing_key(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        let seed = inspector.key.clone();
        inspector
            .rename_editor
            .update(cx, |ti, cx| ti.set_content(seed, cx));
        inspector.editing_key = true;
        cx.notify();
    }

    pub(crate) fn kv_cancel_editing_key(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.editing_key = false;
        cx.notify();
    }

    pub(crate) fn kv_submit_rename(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &browse.inspector else {
            return;
        };
        let from = inspector.key.clone();
        let to = inspector.rename_editor.read(cx).content().to_string();
        if to.is_empty() || to == from {
            return;
        }
        let edit = red_core::kv::KvEdit::Rename { from, to };
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
    }

    pub(crate) fn kv_request_delete(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.confirm_delete = true;
        cx.notify();
    }

    pub(crate) fn kv_cancel_delete(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.confirm_delete = false;
        cx.notify();
    }

    pub(crate) fn kv_confirm_delete(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        // Hide the confirm bar right away: the action is already committed.
        // If it somehow fails, the global error toast still fires; there's
        // just no stale confirm banner left sitting on screen.
        inspector.confirm_delete = false;
        let edit = red_core::kv::KvEdit::Delete {
            keys: vec![inspector.key.clone()],
        };
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
        cx.notify();
    }

    /// Open the collection-element popover (add or edit a hash field / set
    /// member / zset member / list element), seeding the shared element editors.
    /// `seed_name` fills the field/member/list-value input; `seed_value` fills
    /// the hash-value/zset-score input (either is empty for an add).
    pub(crate) fn kv_open_collection_edit(
        &mut self,
        session: SessionId,
        kind: CollectionEditKind,
        seed_name: String,
        seed_value: String,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        // Only one popover open at a time.
        inspector.editing_key = false;
        inspector.editing_ttl = false;
        inspector.elem_error = None;
        inspector
            .elem_name_editor
            .update(cx, |ti, cx| ti.set_content(seed_name, cx));
        inspector
            .elem_value_editor
            .update(cx, |ti, cx| ti.set_content(seed_value, cx));
        inspector.collection_edit = Some(kind);
        cx.notify();
    }

    pub(crate) fn kv_cancel_collection_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.collection_edit = None;
        inspector.elem_error = None;
        cx.notify();
    }

    fn kv_set_elem_error(&mut self, session: SessionId, msg: String, cx: &mut Context<Self>) {
        if let Some(inspector) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.inspector.as_mut())
        {
            inspector.elem_error = Some(msg);
        }
        cx.notify();
    }

    /// Read the open element popover, build the matching [`KvEdit`], and send
    /// it. A blank name / unparseable score surfaces inline in the popover
    /// (`elem_error`) rather than silently no-op'ing.
    pub(crate) fn kv_submit_collection_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        use red_core::kv::KvEdit;
        let Some((epoch, key, kind, name, value)) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| {
                let epoch = b.epoch;
                let inspector = b.inspector.as_ref()?;
                let kind = inspector.collection_edit.clone()?;
                Some((
                    epoch,
                    inspector.key.clone(),
                    kind,
                    inspector.elem_name_editor.read(cx).content().to_string(),
                    inspector.elem_value_editor.read(cx).content().to_string(),
                ))
            })
        else {
            return;
        };

        // Parse a zset score up front so a bad value reports inline.
        let parse_score = |raw: &str| raw.trim().parse::<f64>();
        let edit = match kind {
            CollectionEditKind::AddHashField => {
                if name.is_empty() {
                    return self.kv_set_elem_error(session, "Field name is required".into(), cx);
                }
                KvEdit::SetField {
                    key,
                    field: name,
                    value,
                }
            }
            CollectionEditKind::EditHashField { field } => KvEdit::SetField { key, field, value },
            CollectionEditKind::AddSetMember => {
                if name.is_empty() {
                    return self.kv_set_elem_error(session, "Member is required".into(), cx);
                }
                KvEdit::SetAdd {
                    key,
                    members: vec![name],
                }
            }
            CollectionEditKind::EditSetMember { old } => {
                if name.is_empty() {
                    return self.kv_set_elem_error(session, "Member is required".into(), cx);
                }
                if name == old {
                    return self.kv_cancel_collection_edit(session, cx);
                }
                KvEdit::SetReplace {
                    key,
                    old,
                    new: name,
                }
            }
            CollectionEditKind::AddZSetMember => {
                if name.is_empty() {
                    return self.kv_set_elem_error(session, "Member is required".into(), cx);
                }
                let Ok(score) = parse_score(&value) else {
                    return self.kv_set_elem_error(session, "Score must be a number".into(), cx);
                };
                KvEdit::ZSetAdd {
                    key,
                    member: name,
                    score,
                }
            }
            CollectionEditKind::EditZSetScore { member } => {
                let Ok(score) = parse_score(&value) else {
                    return self.kv_set_elem_error(session, "Score must be a number".into(), cx);
                };
                KvEdit::ZSetAdd { key, member, score }
            }
            CollectionEditKind::AddListHead => KvEdit::ListPush {
                key,
                value: name,
                head: true,
            },
            CollectionEditKind::AddListTail => KvEdit::ListPush {
                key,
                value: name,
                head: false,
            },
            CollectionEditKind::EditListIndex { index } => KvEdit::ListSet {
                key,
                index,
                value: name,
            },
        };

        if let Some(inspector) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.inspector.as_mut())
        {
            inspector.collection_edit = None;
            inspector.elem_error = None;
        }
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
        cx.notify();
    }

    /// Send a collection-element edit built from the current inspector key
    /// (the row-level Delete/replace helpers). No-op if the inspector closed.
    fn kv_send_element_edit(
        &mut self,
        session: SessionId,
        make: impl FnOnce(String) -> red_core::kv::KvEdit,
        cx: &mut Context<Self>,
    ) {
        let Some((epoch, key)) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .and_then(|b| Some((b.epoch, b.inspector.as_ref()?.key.clone())))
        else {
            return;
        };
        let edit = make(key);
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
        cx.notify();
    }

    /// Open the "New key" popover, building its inputs (each submits the form).
    pub(crate) fn kv_open_create_key(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let sub = |editor: &Entity<TextInput>, cx: &mut Context<Self>| {
            cx.subscribe(editor, move |this, _, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Submit) {
                    this.kv_submit_create_key(session, cx);
                }
            })
            .detach();
        };
        let name = cx.new(|cx| TextInput::new(cx).with_placeholder("key name…"));
        sub(&name, cx);
        let field = cx.new(|cx| TextInput::new(cx).with_placeholder("field…"));
        sub(&field, cx);
        let value = cx.new(|cx| TextInput::new(cx).with_placeholder("value…"));
        sub(&value, cx);
        let score = cx.new(|cx| TextInput::new(cx).with_placeholder("score (e.g. 1.0)"));
        sub(&score, cx);

        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.create_key = Some(CreateKeyState {
                name,
                field,
                value,
                score,
                kv_type: KvType::String,
                type_open: false,
                error: None,
            });
        }
        cx.notify();
    }

    pub(crate) fn kv_cancel_create_key(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.create_key = None;
        }
        cx.notify();
    }

    pub(crate) fn kv_toggle_create_type_menu(
        &mut self,
        session: SessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(ck) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.create_key.as_mut())
        {
            ck.type_open = !ck.type_open;
        }
        cx.notify();
    }

    pub(crate) fn kv_set_create_type(
        &mut self,
        session: SessionId,
        kv_type: KvType,
        cx: &mut Context<Self>,
    ) {
        if let Some(ck) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.create_key.as_mut())
        {
            ck.kv_type = kv_type;
            ck.type_open = false;
            ck.error = None;
        }
        cx.notify();
    }

    fn kv_set_create_error(&mut self, session: SessionId, msg: String, cx: &mut Context<Self>) {
        if let Some(ck) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.create_key.as_mut())
        {
            ck.error = Some(msg);
        }
        cx.notify();
    }

    /// Validate the "New key" form, send the key's first write, then open the
    /// inspector on it so its value shows straight away. Blank required fields
    /// / a bad score report inline (`create_key.error`).
    pub(crate) fn kv_submit_create_key(&mut self, session: SessionId, cx: &mut Context<Self>) {
        use red_core::kv::KvEdit;
        let Some((epoch, name, kv_type, field, value, score)) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| {
                let epoch = b.epoch;
                let ck = b.create_key.as_ref()?;
                Some((
                    epoch,
                    ck.name.read(cx).content().trim().to_string(),
                    ck.kv_type.clone(),
                    ck.field.read(cx).content().to_string(),
                    ck.value.read(cx).content().to_string(),
                    ck.score.read(cx).content().to_string(),
                ))
            })
        else {
            return;
        };
        if name.is_empty() {
            return self.kv_set_create_error(session, "Key name is required".into(), cx);
        }
        let key = name.clone();
        let edit = match &kv_type {
            KvType::String => KvEdit::SetString {
                key,
                value,
                ttl: red_core::kv::StringTtl::Clear,
            },
            KvType::Hash => {
                if field.is_empty() {
                    return self.kv_set_create_error(session, "Field name is required".into(), cx);
                }
                KvEdit::SetField { key, field, value }
            }
            KvType::List => KvEdit::ListPush {
                key,
                value,
                head: false,
            },
            KvType::Set => {
                if value.is_empty() {
                    return self.kv_set_create_error(session, "Member is required".into(), cx);
                }
                KvEdit::SetAdd {
                    key,
                    members: vec![value],
                }
            }
            KvType::ZSet => {
                if value.is_empty() {
                    return self.kv_set_create_error(session, "Member is required".into(), cx);
                }
                let Ok(score) = score.trim().parse::<f64>() else {
                    return self.kv_set_create_error(session, "Score must be a number".into(), cx);
                };
                KvEdit::ZSetAdd {
                    key,
                    member: value,
                    score,
                }
            }
            KvType::Stream | KvType::Other(_) => return,
        };

        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.create_key = None;
        }
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
        // Show the freshly created key immediately.
        self.kv_open_inspector(session, name, None, kv_type, cx);
        cx.notify();
    }

    /// `Event::KvEditApplied`: patch local state so the UI reflects the edit
    /// without a full re-fetch. Drops the reply if the browse it targets has
    /// since been superseded (a filter restart bumped the epoch).
    pub(crate) fn on_kv_edit_applied(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        edit: red_core::kv::KvEdit,
        cx: &mut Context<Self>,
    ) {
        use red_core::kv::KvEdit;
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        // Route by the owning scan epoch across all tabs, not the focused tab: in
        // split view the edit must patch the tab it was issued from even if focus
        // moved. A superseded epoch (filter restart) still finds nothing and drops.
        let Some(browse) = active
            .kv_view
            .as_mut()
            .and_then(|v| v.browse_by_scan_epoch_mut(epoch))
        else {
            return;
        };
        match edit {
            KvEdit::SetString { key, value, ttl } => {
                if let Some(inspector) = &mut browse.inspector {
                    if inspector.key == key {
                        inspector.value = Some(KvValue::Str(red_core::Value::Text(value.into())));
                        inspector.editing_value = false;
                    }
                }
                if let Some(row) = browse.rows_mut().iter_mut().find(|r| r.key == key) {
                    // Mirror the write's TTL intent onto the optimistic row.
                    match ttl {
                        red_core::kv::StringTtl::Keep => {}
                        red_core::kv::StringTtl::Clear => row.ttl = None,
                        red_core::kv::StringTtl::Set(d) => row.ttl = Some(d),
                    }
                }
            }
            KvEdit::SetField { key, field, value } => {
                if let Some(inspector) = &mut browse.inspector {
                    if inspector.key == key {
                        if let Some(KvValue::Hash(KvCollection::Loaded(pairs))) =
                            &mut inspector.value
                        {
                            match pairs.iter_mut().find(|(f, _)| *f == field) {
                                Some((_, v)) => *v = value,
                                None => pairs.push((field, value)),
                            }
                        }
                    }
                }
            }
            KvEdit::HashDelete { key, fields } => {
                if let Some(insp) = &mut browse.inspector {
                    if insp.key == key {
                        if let Some(KvValue::Hash(KvCollection::Loaded(pairs))) = &mut insp.value {
                            pairs.retain(|(f, _)| !fields.contains(f));
                        }
                        Rc::make_mut(&mut insp.collection_rows).retain(|e| match e {
                            KvElement::Field(f, _) => !fields.contains(f),
                            _ => true,
                        });
                    }
                }
            }
            KvEdit::SetAdd { key, members } => {
                if let Some(insp) = &mut browse.inspector {
                    if insp.key == key {
                        if let Some(KvValue::Set(KvCollection::Loaded(items))) = &mut insp.value {
                            for m in &members {
                                if !items.contains(m) {
                                    items.push(m.clone());
                                }
                            }
                        }
                        for m in &members {
                            let present = insp
                                .collection_rows
                                .iter()
                                .any(|e| matches!(e, KvElement::Member(x) if x == m));
                            if !present {
                                Rc::make_mut(&mut insp.collection_rows)
                                    .push(KvElement::Member(m.clone()));
                            }
                        }
                    }
                }
            }
            KvEdit::SetRemove { key, members } => {
                if let Some(insp) = &mut browse.inspector {
                    if insp.key == key {
                        if let Some(KvValue::Set(KvCollection::Loaded(items))) = &mut insp.value {
                            items.retain(|m| !members.contains(m));
                        }
                        Rc::make_mut(&mut insp.collection_rows).retain(|e| match e {
                            KvElement::Member(x) => !members.contains(x),
                            _ => true,
                        });
                    }
                }
            }
            KvEdit::SetReplace { key, old, new } => {
                if let Some(insp) = &mut browse.inspector {
                    if insp.key == key {
                        if let Some(KvValue::Set(KvCollection::Loaded(items))) = &mut insp.value {
                            if let Some(slot) = items.iter_mut().find(|m| **m == old) {
                                *slot = new.clone();
                            }
                        }
                        for e in Rc::make_mut(&mut insp.collection_rows).iter_mut() {
                            if let KvElement::Member(x) = e {
                                if *x == old {
                                    *x = new.clone();
                                }
                            }
                        }
                    }
                }
            }
            KvEdit::ZSetAdd { key, member, score } => {
                if let Some(insp) = &mut browse.inspector {
                    if insp.key == key {
                        if let Some(KvValue::ZSet(KvCollection::Loaded(items))) = &mut insp.value {
                            match items.iter().position(|(m, _)| *m == member) {
                                Some(pos) => items[pos].1 = score,
                                None => items.push((member.clone(), score)),
                            }
                        }
                        match insp
                            .collection_rows
                            .iter()
                            .position(|e| matches!(e, KvElement::Scored(m, _) if *m == member))
                        {
                            Some(pos) => {
                                if let KvElement::Scored(_, s) =
                                    &mut Rc::make_mut(&mut insp.collection_rows)[pos]
                                {
                                    *s = score;
                                }
                            }
                            None => Rc::make_mut(&mut insp.collection_rows)
                                .push(KvElement::Scored(member.clone(), score)),
                        }
                    }
                }
            }
            KvEdit::ZSetRemove { key, members } => {
                if let Some(insp) = &mut browse.inspector {
                    if insp.key == key {
                        if let Some(KvValue::ZSet(KvCollection::Loaded(items))) = &mut insp.value {
                            items.retain(|(m, _)| !members.contains(m));
                        }
                        Rc::make_mut(&mut insp.collection_rows).retain(|e| match e {
                            KvElement::Scored(m, _) => !members.contains(m),
                            _ => true,
                        });
                    }
                }
            }
            KvEdit::ListSet { key, index, value } => {
                if let Some(insp) = &mut browse.inspector {
                    if insp.key == key && index >= 0 {
                        let idx = index as usize;
                        if let Some(KvValue::List(KvCollection::Loaded(items))) = &mut insp.value {
                            if let Some(slot) = items.get_mut(idx) {
                                *slot = value.clone();
                            }
                        }
                        if let Some(KvElement::Member(x)) =
                            Rc::make_mut(&mut insp.collection_rows).get_mut(idx)
                        {
                            *x = value.clone();
                        }
                    }
                }
            }
            KvEdit::ListPush { key, value, head } => {
                if let Some(insp) = &mut browse.inspector {
                    if insp.key == key {
                        if let Some(KvValue::List(KvCollection::Loaded(items))) = &mut insp.value {
                            if head {
                                items.insert(0, value.clone());
                            } else {
                                items.push(value.clone());
                            }
                        }
                        // For a large list, `collection_rows` is a head window:
                        // a head push shows immediately; a tail push only when
                        // the whole list is loaded (else it lands off-window).
                        if head {
                            Rc::make_mut(&mut insp.collection_rows)
                                .insert(0, KvElement::Member(value.clone()));
                        } else if insp.collection_exhausted && !insp.collection_head_only {
                            Rc::make_mut(&mut insp.collection_rows)
                                .push(KvElement::Member(value.clone()));
                        }
                    }
                }
            }
            KvEdit::ListRemove { key, value, .. } => {
                if let Some(insp) = &mut browse.inspector {
                    if insp.key == key {
                        if let Some(KvValue::List(KvCollection::Loaded(items))) = &mut insp.value {
                            if let Some(pos) = items.iter().position(|v| *v == value) {
                                items.remove(pos);
                            }
                        }
                        if let Some(pos) = insp
                            .collection_rows
                            .iter()
                            .position(|e| matches!(e, KvElement::Member(x) if *x == value))
                        {
                            Rc::make_mut(&mut insp.collection_rows).remove(pos);
                        }
                    }
                }
            }
            KvEdit::ListRemoveAt { key, index } => {
                if let Some(insp) = &mut browse.inspector {
                    if insp.key == key && index >= 0 {
                        let idx = index as usize;
                        if let Some(KvValue::List(KvCollection::Loaded(items))) = &mut insp.value {
                            if idx < items.len() {
                                items.remove(idx);
                            }
                        }
                        let rows = Rc::make_mut(&mut insp.collection_rows);
                        if idx < rows.len() {
                            rows.remove(idx);
                        }
                    }
                }
            }
            KvEdit::SetTtl { key, ttl } => {
                if let Some(inspector) = &mut browse.inspector {
                    if inspector.key == key {
                        inspector.ttl = ttl;
                        inspector.editing_ttl = false;
                    }
                }
                if let Some(row) = browse.rows_mut().iter_mut().find(|r| r.key == key) {
                    row.ttl = ttl;
                }
            }
            KvEdit::Rename { from, to } => {
                if let Some(inspector) = &mut browse.inspector {
                    if inspector.key == from {
                        inspector.key = to.clone();
                        inspector.editing_key = false;
                    }
                }
                if let Some(row) = browse.rows_mut().iter_mut().find(|r| r.key == from) {
                    row.key = to;
                }
            }
            KvEdit::Delete { keys } => {
                if let Some(inspector) = &browse.inspector {
                    if keys.contains(&inspector.key) {
                        browse.inspector = None;
                    }
                }
                browse.rows_mut().retain(|r| !keys.contains(&r.key));
            }
        }
        // A `SetString` replaced the string body (and cleared the edit); refresh
        // the selectable preview so it shows the just-written value.
        if let Some(session) = session {
            self.kv_rebuild_str_preview(session, cx);
        }
        cx.notify();
    }

    /// `Event::KvKeysRecycled`: a delete captured these keys' `DUMP` snapshots.
    /// Hold them in the recycle bin and raise an "Undo" toast that restores them
    /// (see [`Self::kv_undo_delete`]). Emitted just before the matching
    /// `KvEditApplied` that removes the rows.
    pub(crate) fn on_kv_keys_recycled(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        keys: Vec<RecycledKey>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = session else { return };
        if keys.is_empty() {
            return;
        }
        let n = keys.len();
        let id = self.next_recycle_id;
        self.next_recycle_id += 1;
        self.recycle_bin.push(RecycleBatch {
            id,
            session,
            epoch,
            keys,
        });
        // Evict the oldest batches beyond the cap (a session-scoped, bounded bin).
        if self.recycle_bin.len() > RECYCLE_BIN_CAP {
            let overflow = self.recycle_bin.len() - RECYCLE_BIN_CAP;
            self.recycle_bin.drain(0..overflow);
        }
        let msg = if n == 1 {
            "Deleted 1 key".to_string()
        } else {
            format!("Deleted {n} keys")
        };
        // A persistent (no auto-dismiss) toast: the restore button is the undo
        // affordance, so it must stay until the user acts or closes it.
        self.push_notification(
            Notification {
                id: 0,
                variant: ToastVariant::Info,
                message: msg.into(),
                detail: Some("Restore to undo".into()),
                detail_label: None,
                auto_dismiss: None,
                export: None,
                expanded: false,
                hovered: false,
                dismiss_gen: 0,
                action: Some(NotificationAction::UndoDelete(id)),
            },
            cx,
        );
    }

    /// `Event::KvKeysRestored`: an undo finished. Confirm it and re-scan the
    /// active browse so the restored keys reappear.
    pub(crate) fn on_kv_keys_restored(
        &mut self,
        session: Option<SessionId>,
        _epoch: u64,
        count: u64,
        cx: &mut Context<Self>,
    ) {
        let msg = if count == 1 {
            "Restored 1 key".to_string()
        } else {
            format!("Restored {count} keys")
        };
        self.notify(ToastVariant::Success, msg, cx);
        if let Some(session) = session {
            self.kv_relaunch_browse(session, cx);
        }
    }

    /// The "Undo" toast action: `RESTORE` a recycle batch's keys on the server
    /// (see `Command::KvRestoreKeys`), consuming the batch.
    pub(crate) fn kv_undo_delete(&mut self, batch_id: u64, cx: &mut Context<Self>) {
        let Some(pos) = self.recycle_bin.iter().position(|b| b.id == batch_id) else {
            return; // already restored, evicted, or on another window
        };
        let batch = self.recycle_bin.remove(pos);
        self.service.send_to(
            batch.session,
            Command::KvRestoreKeys {
                epoch: batch.epoch,
                keys: batch.keys,
            },
        );
        cx.notify();
    }

    /// `Event::KvValueReady`: apply it if the inspector is still open on this
    /// key (a `key` comparison, not the browse's epoch, since the inspector
    /// can outlive a filter-triggered scan restart). A `Large` collection
    /// auto-loads its first page/window right away, same one-click-in flow
    /// as opening the inspector itself.
    pub(crate) fn on_kv_value_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        value: Option<KvValue>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        // Route by the inspector's key across all tabs, not the focused tab: in
        // split view the reply must reach the tab that asked even if focus moved.
        let Some(browse) = active
            .kv_view
            .as_mut()
            .and_then(|v| v.browse_by_inspector_key_mut(&key))
        else {
            return; // no open inspector on this key: a newer selection superseded it
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.value = value.clone();
        inspector.value_loaded = true;
        inspector.value_error = None;
        // Whether this is the initial capped read or a "Load full value" reply,
        // the string body is now settled — drop the loading state either way.
        inspector.loading_full_value = false;
        // A pending "Edit" that was waiting on the full value can now open, but
        // only if the loaded body is editable text (a binary `Blob` is not).
        let start_edit = inspector.edit_after_load
            && matches!(&value, Some(KvValue::Str(red_core::Value::Text(_))));
        inspector.edit_after_load = false;
        cx.notify();
        let Some(session) = session else { return };
        if start_edit {
            self.kv_start_editing_value(session, cx);
            return;
        }
        // Build the selectable read-only preview for a freshly-loaded string.
        self.kv_rebuild_str_preview(session, cx);
        match value {
            Some(KvValue::Hash(KvCollection::Large { .. })) => {
                self.kv_load_collection_page(session, CollectionKind::Hash, cx);
            }
            Some(KvValue::Set(KvCollection::Large { .. })) => {
                self.kv_load_collection_page(session, CollectionKind::Set, cx);
            }
            Some(KvValue::ZSet(KvCollection::Large { .. })) => {
                self.kv_load_collection_page(session, CollectionKind::ZSet, cx);
            }
            Some(KvValue::List(KvCollection::Large { .. })) => {
                self.kv_load_list_preview(session, cx);
            }
            Some(KvValue::Stream(KvCollection::Large { .. })) => {
                self.kv_load_stream_page(session, cx);
            }
            _ => {}
        }
    }

    /// `Event::KvValueError`: a value read failed. Settle the inspector's
    /// value area on the error (for the matching key) instead of leaving it on
    /// a permanent "Loading…".
    pub(crate) fn on_kv_value_error(
        &mut self,
        session: Option<SessionId>,
        key: String,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(inspector) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.browse_by_inspector_key_mut(&key))
            .and_then(|b| b.inspector.as_mut())
        {
            inspector.value_loaded = true;
            inspector.value_error = Some(message);
            inspector.loading_full_value = false;
            cx.notify();
        }
    }

    /// Fetch the next page of the inspector's big hash/set/zset, or the
    /// first page if none has loaded yet. The keyspace table's
    /// `on_visible_range` calls this too, once the sub-grid's own visible
    /// range nears the end of what's loaded (see `render_kv_inspector`).
    pub(crate) fn kv_load_collection_page(
        &mut self,
        session: SessionId,
        kind: CollectionKind,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        if inspector.collection_loading || inspector.collection_exhausted {
            return;
        }
        inspector.collection_loading = true;
        let key = inspector.key.clone();
        let cursor = inspector.collection_cursor;
        self.service.send_to(
            session,
            Command::KvReadCollectionPage {
                epoch,
                key,
                kind,
                cursor,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }

    fn kv_load_list_preview(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.collection_loading = true;
        let key = inspector.key.clone();
        self.service.send_to(
            session,
            Command::KvReadListWindow {
                epoch,
                key,
                from_head: true,
                count: LIST_PREVIEW_COUNT,
            },
        );
        cx.notify();
    }

    /// The inspector sub-grid's `on_visible_range` hook, mirroring
    /// `kv_maybe_load_more` for the top-level keyspace table.
    pub(crate) fn kv_inspector_maybe_load_more(
        &mut self,
        session: SessionId,
        kind: CollectionKind,
        visible_end: usize,
        cx: &mut Context<Self>,
    ) {
        let loaded = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .and_then(|b| b.inspector.as_ref())
            .map(|i| i.collection_rows.len());
        let Some(loaded) = loaded else {
            return;
        };
        if visible_end + LOAD_AHEAD_ROWS < loaded {
            return;
        }
        self.kv_load_collection_page(session, kind, cx);
    }

    pub(crate) fn on_kv_collection_page_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        page: KvCollectionPage,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(browse) = active
            .kv_view
            .as_mut()
            .and_then(|v| v.browse_by_inspector_key_mut(&key))
        else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        Rc::make_mut(&mut inspector.collection_rows).extend(page.elements);
        inspector.collection_cursor = page.next_cursor;
        inspector.collection_exhausted = page.exhausted;
        inspector.collection_loading = false;
        cx.notify();
    }

    pub(crate) fn on_kv_list_window_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        values: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(browse) = active
            .kv_view
            .as_mut()
            .and_then(|v| v.browse_by_inspector_key_mut(&key))
        else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.collection_rows = Rc::new(values.into_iter().map(KvElement::Member).collect());
        // A list's head-window preview is a one-shot fetch, not paged: mark it
        // exhausted (no more pages) but also head-only, so a tail append isn't
        // optimistically shown inside the head window (it lands off-window).
        inspector.collection_exhausted = true;
        inspector.collection_head_only = true;
        inspector.collection_loading = false;
        cx.notify();
    }

    /// Fetch the next (older) page of the inspector's big stream, or the first
    /// (newest) page if none has loaded yet. Mirrors `kv_load_collection_page`
    /// but continues by entry ID (`stream_before`) rather than a `*SCAN`
    /// cursor.
    pub(crate) fn kv_load_stream_page(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        if inspector.stream_loading || inspector.stream_exhausted {
            return;
        }
        inspector.stream_loading = true;
        let key = inspector.key.clone();
        let before = inspector.stream_before.clone();
        self.service.send_to(
            session,
            Command::KvReadStreamPage {
                epoch,
                key,
                before,
                count: STREAM_PAGE_COUNT,
            },
        );
        cx.notify();
    }

    /// The stream sub-grid's `on_visible_range` hook, mirroring
    /// `kv_inspector_maybe_load_more` for a big hash/set/zset.
    pub(crate) fn kv_inspector_maybe_load_more_stream(
        &mut self,
        session: SessionId,
        visible_end: usize,
        cx: &mut Context<Self>,
    ) {
        let loaded = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .and_then(|b| b.inspector.as_ref())
            .map(|i| i.stream_rows.len());
        let Some(loaded) = loaded else {
            return;
        };
        if visible_end + LOAD_AHEAD_ROWS < loaded {
            return;
        }
        self.kv_load_stream_page(session, cx);
    }

    pub(crate) fn on_kv_stream_page_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        page: KvStreamPage,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(browse) = active
            .kv_view
            .as_mut()
            .and_then(|v| v.browse_by_inspector_key_mut(&key))
        else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        Rc::make_mut(&mut inspector.stream_rows).extend(page.entries);
        inspector.stream_before = page.next_before;
        inspector.stream_exhausted = page.exhausted;
        inspector.stream_loading = false;
        cx.notify();
    }

    // --- stream consumer groups (see docs/plans/redis.md's "stream
    // consumer-group management" gap) ---

    /// Switch the stream inspector between its entries grid and its
    /// consumer-group view. Opening the Groups tab for the first time kicks
    /// off the lazy `XINFO GROUPS` load.
    pub(crate) fn kv_set_stream_view(
        &mut self,
        session: SessionId,
        view: StreamView,
        cx: &mut Context<Self>,
    ) {
        let need_load = {
            let Some(inspector) = self.kv_inspector_mut(session) else {
                return;
            };
            inspector.stream_groups.view = view;
            view == StreamView::Groups && !inspector.stream_groups.loaded
        };
        if need_load {
            self.kv_load_stream_groups(session, cx);
        }
        cx.notify();
    }

    /// Fetch (or refresh) the stream's consumer groups.
    pub(crate) fn kv_load_stream_groups(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.stream_groups.loading = true;
        let key = inspector.key.clone();
        self.service
            .send_to(session, Command::KvStreamGroups { epoch, key });
        cx.notify();
    }

    pub(crate) fn on_kv_stream_groups_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        groups: Vec<StreamGroup>,
        cx: &mut Context<Self>,
    ) {
        let Some(inspector) = self.kv_inspector_for(session) else {
            return;
        };
        if inspector.key != key {
            return;
        }
        inspector.stream_groups.loaded = true;
        inspector.stream_groups.loading = false;
        // Keep a valid selection: default to the first group, and if the
        // previously-selected group is gone (dropped meanwhile), fall back.
        let still_present = inspector
            .stream_groups
            .selected
            .as_ref()
            .is_some_and(|s| groups.iter().any(|g| &g.name == s));
        let auto_select = (!still_present).then(|| groups.first().map(|g| g.name.clone()));
        inspector.stream_groups.groups = groups;
        cx.notify();
        if let Some(Some(first)) = auto_select {
            if let Some(session) = session {
                self.kv_select_stream_group(session, first, cx);
            }
        }
    }

    /// Select a group and load its consumers + pending entries.
    pub(crate) fn kv_select_stream_group(
        &mut self,
        session: SessionId,
        group: String,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        let key = inspector.key.clone();
        inspector.stream_groups.selected = Some(group.clone());
        inspector.stream_groups.consumers.clear();
        inspector.stream_groups.pending.clear();
        inspector.stream_groups.claiming = None;
        inspector.stream_groups.detail_loading = true;
        self.service.send_to(
            session,
            Command::KvStreamConsumers {
                epoch,
                key: key.clone(),
                group: group.clone(),
            },
        );
        self.service.send_to(
            session,
            Command::KvStreamPending {
                epoch,
                key,
                group,
                count: STREAM_PENDING_COUNT,
            },
        );
        cx.notify();
    }

    pub(crate) fn on_kv_stream_consumers_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        group: String,
        consumers: Vec<StreamConsumer>,
        cx: &mut Context<Self>,
    ) {
        let Some(inspector) = self.kv_inspector_for(session) else {
            return;
        };
        // Drop a reply for a key/group the inspector has since moved off.
        if inspector.key != key || inspector.stream_groups.selected.as_deref() != Some(&group) {
            return;
        }
        inspector.stream_groups.consumers = consumers;
        inspector.stream_groups.detail_loading = false;
        cx.notify();
    }

    pub(crate) fn on_kv_stream_pending_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        group: String,
        pending: Vec<PendingEntry>,
        cx: &mut Context<Self>,
    ) {
        let Some(inspector) = self.kv_inspector_for(session) else {
            return;
        };
        if inspector.key != key || inspector.stream_groups.selected.as_deref() != Some(&group) {
            return;
        }
        inspector.stream_groups.pending = pending;
        inspector.stream_groups.detail_loading = false;
        cx.notify();
    }

    /// Acknowledge one pending entry (`XACK`), dropping it from the group's PEL.
    pub(crate) fn kv_stream_ack(&mut self, session: SessionId, id: String, cx: &mut Context<Self>) {
        self.kv_send_stream_action(session, KvStreamActionReq::Ack { ids: vec![id] }, cx);
    }

    /// Open the inline "claim to consumer" form for one pending entry.
    pub(crate) fn kv_start_claim(
        &mut self,
        session: SessionId,
        id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(inspector) = self.kv_inspector_mut(session) else {
            return;
        };
        inspector
            .stream_groups
            .claim_editor
            .update(cx, |ti, cx| ti.set_content(String::new(), cx));
        inspector.stream_groups.claiming = Some(id);
        cx.notify();
    }

    pub(crate) fn kv_cancel_claim(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(inspector) = self.kv_inspector_mut(session) else {
            return;
        };
        inspector.stream_groups.claiming = None;
        cx.notify();
    }

    /// Submit the open claim form: reassign the pending entry to the typed
    /// consumer (`XCLAIM`, `min-idle 0` since the operator is deliberately
    /// reclaiming it now). A blank consumer name is a no-op.
    pub(crate) fn kv_submit_claim(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(inspector) = self.kv_inspector_mut(session) else {
            return;
        };
        let Some(id) = inspector.stream_groups.claiming.clone() else {
            return;
        };
        let consumer = inspector
            .stream_groups
            .claim_editor
            .read(cx)
            .content()
            .trim()
            .to_string();
        if consumer.is_empty() {
            return;
        }
        inspector.stream_groups.claiming = None;
        self.kv_send_stream_action(
            session,
            KvStreamActionReq::Claim {
                consumer,
                min_idle_ms: 0,
                ids: vec![id],
            },
            cx,
        );
    }

    /// Shared send path for `XACK`/`XCLAIM`: needs the selected group, which
    /// both actions target.
    fn kv_send_stream_action(
        &mut self,
        session: SessionId,
        action: KvStreamActionReq,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        let Some(group) = inspector.stream_groups.selected.clone() else {
            return;
        };
        let key = inspector.key.clone();
        self.service.send_to(
            session,
            Command::KvStreamAction {
                epoch,
                key,
                group,
                action,
            },
        );
        cx.notify();
    }

    pub(crate) fn on_kv_stream_action_done(
        &mut self,
        session: Option<SessionId>,
        key: String,
        group: String,
        action: StreamAction,
        count: u64,
        cx: &mut Context<Self>,
    ) {
        let verb = match action {
            StreamAction::Ack => "Acknowledged",
            StreamAction::Claim => "Claimed",
        };
        let plural = if count == 1 { "entry" } else { "entries" };
        self.notify(
            ToastVariant::Success,
            format!("{verb} {count} pending {plural} in \"{group}\""),
            cx,
        );
        let Some(session) = session else { return };
        // Refresh the affected group's detail and the group list (pending /
        // consumer counts just changed), matching the current inspector.
        let matches = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .and_then(|b| b.inspector.as_ref())
            .is_some_and(|i| i.key == key && i.stream_groups.selected.as_deref() == Some(&group));
        if matches {
            self.kv_select_stream_group(session, group, cx);
            self.kv_load_stream_groups(session, cx);
        }
    }

    /// The current inspector for `session` if the browse is live, borrowed
    /// mutably — the shared preamble every group handler needs.
    fn kv_inspector_mut(&mut self, session: SessionId) -> Option<&mut KvInspector> {
        self.conn_mut(Some(session))?
            .kv_view
            .as_mut()?
            .active_browse_mut()?
            .inspector
            .as_mut()
    }

    /// Like [`kv_inspector_mut`](Self::kv_inspector_mut) but resolving the
    /// session `Option` an event carries (events are delivered with the
    /// originating `SessionId`, or `None` for the foreground).
    fn kv_inspector_for(&mut self, session: Option<SessionId>) -> Option<&mut KvInspector> {
        self.conn_mut(session)?
            .kv_view
            .as_mut()?
            .active_browse_mut()?
            .inspector
            .as_mut()
    }
}

/// The type column's short label + tint, mirroring `connect.rs`'s
/// `engine_tint`/`label_color` per-kind lookup style.
fn type_pill(kv_type: &KvType, theme: &Theme) -> impl IntoElement {
    let color = match kv_type {
        KvType::String => theme.blue,
        KvType::Hash => theme.orange,
        KvType::List => theme.green,
        KvType::Set => theme.purple,
        KvType::ZSet => theme.yellow,
        KvType::Stream => theme.cyan,
        KvType::Other(_) => theme.text_muted,
    };
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
/// (see `AppState::kv_maybe_grow_fuzzy_pool`) stops chasing more pages.
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

impl AppState {
    /// The keyspace browser's body: filter box + header stat + the
    /// virtualized key list. Called from `render_redis_shell`.
    pub(crate) fn render_kv_browse(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let session = active.session;
        let Some(view_ref) = active.kv_view.as_ref() else {
            return div().flex_1();
        };
        let db_size = view_ref.db_size;
        let Some(browse) = view_ref.browse_at(tab_idx) else {
            return div().flex_1();
        };

        let writable = !active.config.read_only;
        let fuzzy_query = browse.filter.read(cx).content().to_string();
        let rows: Rc<Vec<KeyMeta>> = browse.visible_rows(cx);

        // Appended to the header stat once the resident-row cap has kicked in.
        let evicted_note = if browse.evicted {
            " · showing the most recent 20k, narrow the filter for older keys"
        } else {
            ""
        };

        let header = if browse.fuzzy {
            if fuzzy_query.is_empty() {
                format!("{} keys loaded so far", browse.rows.len())
            } else {
                format!(
                    "{} fuzzy match(es) of {} loaded{}",
                    rows.len(),
                    browse.rows.len(),
                    if browse.exhausted {
                        ""
                    } else {
                        ", still scanning…"
                    }
                )
            }
        } else {
            let type_label = browse.type_filter.as_ref().map(|t| t.label());
            match (&browse.pattern, type_label, db_size) {
                // No filter at all: the cheap `DBSIZE` header stat.
                (None, None, Some(n)) => {
                    format!("~{} keys in db0", crate::result::group_digits(n as usize))
                }
                (None, None, None) => "counting keys…".into(),
                // A pattern and/or type filter is active: there's no cheap
                // filtered count, so report what's loaded so far.
                (pattern, ty, _) => {
                    let kind = ty.map(|t| format!("{t} ")).unwrap_or_default();
                    match pattern {
                        Some(p) => {
                            format!("{} {kind}key(s) matching \"{p}\" so far", browse.rows.len())
                        }
                        None => format!("{} {kind}key(s) so far", browse.rows.len()),
                    }
                }
            }
        };
        let header = format!("{header}{evicted_note}");

        // In tree mode the list is a flattened namespace trie (folders + keys);
        // in grid mode it's the raw key rows (no per-row allocation — the flat
        // grid's hot path). `disp` is `Some` only in tree mode.
        let tree_mode = browse.tree_mode;
        let disp: Option<Rc<Vec<DispRow>>> = tree_mode.then(|| browse.tree_rows(&rows));
        let row_count = match &disp {
            Some(d) => d.len(),
            None => rows.len(),
        };

        let rows_render = rows.clone();
        let rows_select = rows.clone();
        let rows_menu = rows.clone();
        let disp_render = disp.clone();
        let disp_select = disp.clone();
        let disp_menu = disp.clone();
        let visible_range_view = view.clone();
        let select_view = view.clone();
        let menu_view = view.clone();
        let nav_view = view.clone();
        let list_focus = browse.list_focus.clone();

        // The keyboard cursor drives the grid highlight once the list has been
        // touched; before that (and always in tree mode, where nav is disabled)
        // it falls back to the inspected key's row.
        let selected_ix = match &disp {
            Some(d) => browse.inspector.as_ref().and_then(|insp| {
                d.iter().position(
                    |r| matches!(r, DispRow::Key { row, .. } if rows.get(*row).is_some_and(|m| m.key == insp.key)),
                )
            }),
            None => browse.nav_row.filter(|&i| i < row_count).or_else(|| {
                browse
                    .inspector
                    .as_ref()
                    .and_then(|i| rows.iter().position(|r| r.key == i.key))
            }),
        };

        let columns = vec![
            Column::new("Key").flex(),
            Column::new("Type").width(px(72.)),
            Column::new("TTL").width(px(110.)).align_end(),
            Column::new("Size").width(px(80.)).align_end(),
            Column::new("Encoding").width(px(110.)),
        ];

        let key_color = theme.text;
        let dim_color = theme.text_muted;
        let cell_size = theme.scale(12.);
        let row_theme = theme.clone();
        // Per-depth indent; leaves align one chevron-width past their folder.
        let indent = |depth: usize| px(depth as f32 * 14.);

        let table = Table::<()>::new("kv-browse", columns)
            .row_count(row_count)
            .grid_lines(true)
            .text_size(cell_size)
            .track_scroll(&browse.scroll)
            .selected(selected_ix)
            .focus_handle(list_focus)
            .on_nav(move |nav, _extend, _window, cx| {
                // No-op in tree mode (see `kv_browse_nav`): the tree is click-driven.
                nav_view
                    .update(cx, |this, cx| this.kv_browse_nav(session, nav, cx))
                    .ok();
            })
            .on_select(move |ix, _click, window, cx| {
                // A folder toggles; a key (tree leaf or grid row) opens the inspector.
                let key_row = match &disp_select {
                    Some(d) => match d.get(ix) {
                        Some(DispRow::Folder { prefix, .. }) => {
                            let prefix = prefix.clone();
                            select_view
                                .update(cx, |this, cx| {
                                    this.kv_toggle_tree_node(session, prefix, cx)
                                })
                                .ok();
                            return;
                        }
                        Some(DispRow::Key { row, .. }) => rows_select.get(*row),
                        None => return,
                    },
                    None => rows_select.get(ix),
                };
                let Some(row) = key_row else { return };
                let (key, ttl, kv_type) = (row.key.clone(), row.ttl, row.kv_type.clone());
                let grid_ix = disp_select.is_none().then_some(ix);
                select_view
                    .update(cx, |this, cx| {
                        // A grid click plants the keyboard cursor and focuses the
                        // list so arrows continue from it; the tree has no cursor.
                        if let Some(gi) = grid_ix {
                            if let Some(b) = this
                                .conn_mut(Some(session))
                                .and_then(|a| a.kv_view.as_mut())
                                .and_then(|v| v.active_browse_mut())
                            {
                                b.nav_row = Some(gi);
                                window.focus(&b.list_focus, cx);
                            }
                        }
                        this.kv_open_inspector(session, key, ttl, kv_type, cx)
                    })
                    .ok();
            })
            .on_secondary(move |ix, pos, _window, cx| {
                // Folders carry no context menu; keys do (rename / TTL / delete / …).
                let key_row = match &disp_menu {
                    Some(d) => match d.get(ix) {
                        Some(DispRow::Key { row, .. }) => rows_menu.get(*row),
                        _ => None,
                    },
                    None => rows_menu.get(ix),
                };
                let Some(row) = key_row else { return };
                let (key, kv_type, ttl) = (row.key.clone(), row.kv_type.clone(), row.ttl);
                menu_view
                    .update(cx, |this, cx| {
                        this.kv_open_key_menu(session, key, kv_type, ttl, pos, cx)
                    })
                    .ok();
            })
            .render_row(move |ix, _window, _cx| {
                // Tree mode: render a folder or an indented leaf.
                if let Some(d) = &disp_render {
                    return match d.get(ix) {
                        Some(DispRow::Folder {
                            label,
                            count,
                            expanded,
                            depth,
                            ..
                        }) => {
                            let chevron = if *expanded { "chevron-down" } else { "chevron" };
                            vec![
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .pl(indent(*depth))
                                    .child(crate::icons::icon(chevron, cell_size, dim_color))
                                    .child(div().text_color(key_color).child(label.clone()))
                                    .into_any_element(),
                                div().into_any_element(),
                                div().into_any_element(),
                                div()
                                    .text_color(dim_color)
                                    .child(format!("{count}"))
                                    .into_any_element(),
                                div().into_any_element(),
                            ]
                        }
                        Some(DispRow::Key { row, label, depth }) => {
                            let Some(meta) = rows_render.get(*row) else {
                                return Vec::new();
                            };
                            vec![
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(key_color)
                                    .pl(px(*depth as f32 * 14. + 16.))
                                    .child(label.clone())
                                    .into_any_element(),
                                type_pill(&meta.kv_type, &row_theme).into_any_element(),
                                div()
                                    .text_color(dim_color)
                                    .child(fmt_ttl(meta.ttl))
                                    .into_any_element(),
                                div()
                                    .text_color(dim_color)
                                    .child(fmt_bytes(meta.approx_bytes))
                                    .into_any_element(),
                                div()
                                    .text_color(dim_color)
                                    .truncate()
                                    .child(meta.encoding.clone())
                                    .into_any_element(),
                            ]
                        }
                        None => Vec::new(),
                    };
                }
                // Grid mode: the raw key row (unchanged hot path).
                let Some(row) = rows_render.get(ix) else {
                    return Vec::new();
                };
                vec![
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(key_color)
                        .child(row.key.clone())
                        .into_any_element(),
                    type_pill(&row.kv_type, &row_theme).into_any_element(),
                    div()
                        .text_color(dim_color)
                        .child(fmt_ttl(row.ttl))
                        .into_any_element(),
                    div()
                        .text_color(dim_color)
                        .child(fmt_bytes(row.approx_bytes))
                        .into_any_element(),
                    div()
                        .text_color(dim_color)
                        .truncate()
                        .child(row.encoding.clone())
                        .into_any_element(),
                ]
            })
            .on_visible_range(move |range, _window, cx| {
                visible_range_view
                    .update(cx, |this, cx| {
                        this.kv_maybe_load_more(session, range.end, cx)
                    })
                    .ok();
            });

        let big_keys_view = view.clone();
        let big_keys_button = Button::new("kv-find-big-keys", "Find biggest keys")
            .size(ButtonSize::Sm)
            .variant(ButtonVariant::Secondary)
            .on_click(move |_, _, cx| {
                big_keys_view
                    .update(cx, |this, cx| this.kv_start_big_keys_sample(session, cx))
                    .ok();
            });

        let new_key_button = writable.then(|| {
            let new_view = view.clone();
            Button::new("kv-new-key", "+ New key")
                .size(ButtonSize::Sm)
                .variant(ButtonVariant::Secondary)
                .on_click(move |_, _, cx| {
                    new_view
                        .update(cx, |this, cx| this.kv_open_create_key(session, cx))
                        .ok();
                })
        });

        let create_popover = browse
            .create_key
            .as_ref()
            .map(|ck| self.render_kv_create_popover(session, ck, &theme, cx));

        // An empty key list gets a settled message instead of a blank grid, so
        // "no matches" reads as deliberate (mirrors the other panels' empties).
        let has_filter = browse.pattern.is_some()
            || browse.type_filter.is_some()
            || (browse.fuzzy && !fuzzy_query.is_empty());
        let empty_msg = if browse.loading {
            "Scanning…"
        } else if has_filter {
            "No keys match this filter"
        } else {
            "No keys in this database"
        };
        let key_area = if row_count == 0 {
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme.scale(12.))
                .text_color(theme.text_muted)
                .child(empty_msg)
        } else {
            div().flex_1().min_w(px(0.)).child(table)
        };

        let main = match &browse.big_keys {
            None => div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .child(key_area)
                .when_some(browse.inspector.as_ref(), |el, inspector| {
                    el.child(self.render_kv_inspector(
                        session,
                        inspector,
                        !active.config.read_only,
                        &theme,
                        cx,
                    ))
                })
                .into_any_element(),
            Some(bk) => self
                .render_big_keys(
                    session,
                    bk,
                    browse.inspector.as_ref(),
                    !active.config.read_only,
                    &theme,
                    cx,
                )
                .into_any_element(),
        };

        div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .child(div().flex_1().min_w(px(120.)).child(browse.filter.clone()))
                    .child({
                        let fuzzy_view = view.clone();
                        let fuzzy = browse.fuzzy;
                        IconButton::new(
                            "kv-fuzzy-toggle",
                            crate::icons::icon(
                                "sparkles",
                                theme.scale(14.),
                                if fuzzy { theme.accent } else { theme.text_muted },
                            ),
                        )
                        .size(IconButtonSize::Sm)
                        .tooltip(if fuzzy {
                            "Fuzzy search (on): searches loaded keys and keeps scanning until enough match"
                        } else {
                            "Switch to fuzzy search"
                        })
                        .a11y_label("Toggle fuzzy search")
                        .on_click(move |_, _, cx| {
                            fuzzy_view
                                .update(cx, |this, cx| this.kv_toggle_fuzzy(session, cx))
                                .ok();
                        })
                    })
                    .child({
                        // Namespace tree ↔ flat grid view toggle. Groups keys by
                        // their `:` hierarchy without a re-scan.
                        let tree_view = view.clone();
                        IconButton::new(
                            "kv-tree-toggle",
                            crate::icons::icon(
                                "schema",
                                theme.scale(14.),
                                if tree_mode { theme.accent } else { theme.text_muted },
                            ),
                        )
                        .size(IconButtonSize::Sm)
                        .tooltip(if tree_mode {
                            "Namespace tree (on): keys grouped by their : hierarchy"
                        } else {
                            "Group keys into a namespace tree"
                        })
                        .a11y_label("Toggle namespace tree")
                        .on_click(move |_, _, cx| {
                            tree_view
                                .update(cx, |this, cx| this.kv_toggle_tree_mode(session, cx))
                                .ok();
                        })
                    })
                    .child({
                        // Server-side type filter (`SCAN ... TYPE`): index 0 is
                        // "All types", 1..=6 the concrete types in menu order.
                        // Composes with both the MATCH/fuzzy filter above.
                        let types = kv_filter_types();
                        let selected_ix = match &browse.type_filter {
                            None => 0,
                            Some(t) => types
                                .iter()
                                .position(|x| x == t)
                                .map(|i| i + 1)
                                .unwrap_or(0),
                        };
                        let toggle_view = view.clone();
                        let select_view = view.clone();
                        let mut select = Select::new("kv-type-filter")
                            .accent(false)
                            .option("All types");
                        for t in types.iter() {
                            select = select.option(t.label().to_string());
                        }
                        select
                            .selected(selected_ix)
                            .open(browse.type_filter_open)
                            .on_toggle(move |_, cx| {
                                toggle_view
                                    .update(cx, |this, cx| this.kv_toggle_type_menu(session, cx))
                                    .ok();
                            })
                            .on_select(move |ix, _, cx| {
                                let choice = ix
                                    .checked_sub(1)
                                    .and_then(|i| kv_filter_types().into_iter().nth(i));
                                select_view
                                    .update(cx, |this, cx| {
                                        this.kv_set_type_filter(session, choice, cx)
                                    })
                                    .ok();
                            })
                    })
                    .child(
                        // Yields width to the filter input when the pane is narrow
                        // (e.g. the History dock is open) instead of squeezing it.
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.scale(11.))
                            .text_color(theme.text_muted)
                            .child(header),
                    )
                    .children(new_key_button)
                    .child(big_keys_button),
            )
            .child(main)
            .children(create_popover)
    }

    /// The "New key" popover card: name, a type dropdown, and the per-type seed
    /// inputs (a hash field, a zset score), over a panel-wide backdrop that
    /// dismisses on an outside click — the browse-level twin of
    /// [`kv_edit_popover`].
    fn render_kv_create_popover(
        &self,
        session: SessionId,
        ck: &CreateKeyState,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let view = cx.entity().downgrade();
        let (save_view, cancel_view, backdrop_view) = (view.clone(), view.clone(), view.clone());
        let (toggle_view, select_view) = (view.clone(), view.clone());

        let types = kv_creatable_types();
        let selected_ix = types.iter().position(|t| *t == ck.kv_type).unwrap_or(0);
        let mut type_select = Select::new("kv-create-type").accent(false);
        for t in types.iter() {
            type_select = type_select.option(t.label().to_string());
        }
        let type_select = type_select
            .selected(selected_ix)
            .open(ck.type_open)
            .on_toggle(move |_, cx| {
                toggle_view
                    .update(cx, |this, cx| this.kv_toggle_create_type_menu(session, cx))
                    .ok();
            })
            .on_select(move |ix, _, cx| {
                let choice = kv_creatable_types()
                    .into_iter()
                    .nth(ix)
                    .unwrap_or(KvType::String);
                select_view
                    .update(cx, |this, cx| this.kv_set_create_type(session, choice, cx))
                    .ok();
            });

        let show_field = matches!(ck.kv_type, KvType::Hash);
        let show_score = matches!(ck.kv_type, KvType::ZSet);

        let card = div()
            .occlude()
            .w(px(300.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_lg()
            .child(
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child("New key"),
            )
            .child(ck.name.clone())
            .child(type_select)
            .children(show_field.then(|| ck.field.clone()))
            .child(ck.value.clone())
            .children(show_score.then(|| ck.score.clone()))
            .children(ck.error.clone().map(|e| {
                div()
                    .text_size(theme.scale(10.))
                    .text_color(theme.red)
                    .child(e)
            }))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("kv-create-save", "Create")
                            .variant(ButtonVariant::Primary)
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                save_view
                                    .update(cx, |this, cx| this.kv_submit_create_key(session, cx))
                                    .ok();
                            }),
                    )
                    .child(
                        Button::new("kv-create-cancel", "Cancel")
                            .variant(ButtonVariant::Secondary)
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                cancel_view
                                    .update(cx, |this, cx| this.kv_cancel_create_key(session, cx))
                                    .ok();
                            }),
                    ),
            );

        div()
            .absolute()
            .inset_0()
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                backdrop_view
                    .update(cx, |this, cx| this.kv_cancel_create_key(session, cx))
                    .ok();
            })
            .child(floating(card).offset(point(px(8.), px(40.))))
            .into_any_element()
    }

    /// The "find biggest keys" sample's own table (see `BigKeysState`),
    /// replacing the live browse's table+inspector while it's active.
    fn render_big_keys(
        &self,
        session: SessionId,
        bk: &BigKeysState,
        inspector: Option<&KvInspector>,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let close_view = view.clone();
        let select_view = view.clone();
        let menu_view = view.clone();
        let rows = std::rc::Rc::new(bk.results.clone());
        let rows_render = rows.clone();
        let rows_select = rows.clone();
        let rows_menu = rows.clone();
        let row_count = rows.len();
        let selected_ix = inspector.and_then(|i| rows.iter().position(|r| r.key == i.key));

        let status = if bk.running {
            format!("sampling… {} keys scanned so far", bk.sampled)
        } else {
            format!(
                "sampled {} keys; showing the {} biggest",
                bk.sampled,
                rows.len()
            )
        };

        let columns = vec![
            Column::new("Key").flex(),
            Column::new("Type").width(px(72.)),
            Column::new("Size").width(px(90.)).align_end(),
            Column::new("Encoding").width(px(110.)),
        ];
        let key_color = theme.text;
        let dim_color = theme.text_muted;
        let row_theme = theme.clone();
        let cell_size = theme.scale(12.);

        let table = Table::<()>::new("kv-big-keys", columns)
            .row_count(row_count)
            .grid_lines(true)
            .text_size(cell_size)
            .selected(selected_ix)
            .on_select(move |ix, _click, _window, cx| {
                let Some(row) = rows_select.get(ix) else {
                    return;
                };
                let (key, ttl, kv_type) = (row.key.clone(), row.ttl, row.kv_type.clone());
                select_view
                    .update(cx, |this, cx| {
                        this.kv_open_inspector(session, key, ttl, kv_type, cx)
                    })
                    .ok();
            })
            .on_secondary(move |ix, pos, _window, cx| {
                let Some(row) = rows_menu.get(ix) else {
                    return;
                };
                let (key, kv_type, ttl) = (row.key.clone(), row.kv_type.clone(), row.ttl);
                menu_view
                    .update(cx, |this, cx| {
                        this.kv_open_key_menu(session, key, kv_type, ttl, pos, cx)
                    })
                    .ok();
            })
            .render_row(move |ix, _window, _cx| {
                let Some(row) = rows_render.get(ix) else {
                    return Vec::new();
                };
                vec![
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(key_color)
                        .child(row.key.clone())
                        .into_any_element(),
                    type_pill(&row.kv_type, &row_theme).into_any_element(),
                    div()
                        .text_color(dim_color)
                        .child(fmt_bytes(row.approx_bytes))
                        .into_any_element(),
                    div()
                        .text_color(dim_color)
                        .truncate()
                        .child(row.encoding.clone())
                        .into_any_element(),
                ]
            });

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme.scale(10.5))
                            .text_color(theme.text_muted)
                            .child(status),
                    )
                    .child(
                        Button::new(
                            "kv-big-keys-close",
                            if bk.running {
                                "Stop sampling"
                            } else {
                                "Back to live browse"
                            },
                        )
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            close_view
                                .update(cx, |this, cx| this.kv_close_big_keys(session, cx))
                                .ok();
                        }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(div().flex_1().min_w(px(0.)).child(table))
                    .when_some(inspector, |el, inspector| {
                        el.child(self.render_kv_inspector(session, inspector, writable, theme, cx))
                    }),
            )
    }

    /// The keyspace-analysis panel: a persisted, point-in-time report (type
    /// distribution, top namespaces by memory, expiry summary) with a
    /// Run/Re-run control (see docs/plans/redis.md's "persistent database
    /// analysis report" gap).
    pub(crate) fn render_kv_analysis(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let session = active.session;
        let Some(st) = active.kv_view.as_ref().and_then(|v| v.analysis_at(tab_idx)) else {
            return div().flex_1();
        };

        let run_view = cx.entity().downgrade();
        let cancel_view = cx.entity().downgrade();
        let run_label = if st.report.is_some() {
            "Re-analyze"
        } else {
            "Analyze keyspace"
        };
        let status = if st.running {
            format!("Scanning… {} keys sampled", st.collected.len())
        } else if let Some(r) = &st.report {
            let scope = if r.truncated {
                format!("sampled {} of {}", r.sampled, r.total_keys.max(r.sampled))
            } else {
                format!("{} keys (full scan)", r.sampled)
            };
            format!(
                "As of {} — {scope}, {} total",
                fmt_ago_secs(crate::conversations::now_unix() as i64 - r.generated_at),
                fmt_bytes(r.total_bytes)
            )
        } else {
            "No analysis yet. Run one to break down types, namespaces and expiry.".to_string()
        };

        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(status),
            )
            .when(st.running, |d| {
                d.child(
                    Button::new("kv-analysis-cancel", "Stop")
                        .variant(ButtonVariant::Secondary)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            cancel_view
                                .update(cx, |this, cx| this.kv_cancel_analysis(session, cx))
                                .ok();
                        }),
                )
            })
            .when(!st.running, |d| {
                d.child(
                    Button::new("kv-analysis-run", run_label)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            run_view
                                .update(cx, |this, cx| this.kv_run_analysis(session, cx))
                                .ok();
                        }),
                )
            });

        let body = match &st.report {
            Some(report) => self
                .render_analysis_report(report, session, &theme, cx)
                .into_any_element(),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_size(theme.scale(11.5))
                .text_color(theme.text_muted)
                .child(if st.running {
                    "Analyzing the keyspace…"
                } else {
                    "Run an analysis to see the report here."
                })
                .into_any_element(),
        };

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(header)
            .child(body)
    }

    /// The report body: type distribution, top namespaces, expiry summary,
    /// each a section of proportion bars. Read-only, scrollable.
    fn render_analysis_report(
        &self,
        report: &red_core::kv::RedisAnalysis,
        session: SessionId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let section_label = |s: &str| {
            div()
                .flex_shrink_0()
                .px_2()
                .pt_2()
                .pb_1()
                .text_size(theme.scale(9.5))
                .text_color(theme.text_muted)
                .child(s.to_string().to_uppercase())
        };

        // A labelled proportion bar: `label` left, `right` note, a fill sized
        // to `value/max` behind it. Reused across the type and namespace lists.
        // Clicking a type or namespace row drills into a filtered Browse tab
        // (see `kv_drill_type`/`kv_drill_namespace`).
        let max_type = report.types.iter().map(|t| t.bytes).max().unwrap_or(0);
        let type_rows: Vec<_> = report
            .types
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let row = bar_row(
                    &t.kv_type,
                    &format!("{} · {}", t.count, fmt_bytes(t.bytes)),
                    t.bytes,
                    max_type,
                    theme.blue,
                    theme,
                );
                let drill_view = cx.entity().downgrade();
                let label = t.kv_type.clone();
                div()
                    .id(("kv-analysis-type", i))
                    .cursor_pointer()
                    .child(row)
                    .on_click(move |_, _, cx| {
                        let label = label.clone();
                        drill_view
                            .update(cx, |this, cx| this.kv_drill_type(session, label, cx))
                            .ok();
                    })
                    .into_any_element()
            })
            .collect();

        let max_ns = report.namespaces.iter().map(|n| n.bytes).max().unwrap_or(0);
        let ns_rows: Vec<_> = report
            .namespaces
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let row = bar_row(
                    &n.prefix,
                    &format!("{} · {}", n.count, fmt_bytes(n.bytes)),
                    n.bytes,
                    max_ns,
                    theme.purple,
                    theme,
                );
                // The "(no prefix)" bucket has no glob to drill into.
                if n.prefix == red_core::kv::NO_PREFIX_LABEL {
                    return row;
                }
                let drill_view = cx.entity().downgrade();
                let prefix = n.prefix.clone();
                div()
                    .id(("kv-analysis-ns", i))
                    .cursor_pointer()
                    .child(row)
                    .on_click(move |_, _, cx| {
                        let prefix = prefix.clone();
                        drill_view
                            .update(cx, |this, cx| this.kv_drill_namespace(session, prefix, cx))
                            .ok();
                    })
                    .into_any_element()
            })
            .collect();

        // Expiry summary: persistent vs. bucketed by how soon.
        let ttl = &report.ttl;
        let ttl_total = ttl.persistent + ttl.with_ttl();
        let ttl_rows: Vec<_> = [
            ("No expiry", ttl.persistent, theme.text_muted),
            ("< 1 hour", ttl.under_hour, theme.red),
            ("< 1 day", ttl.under_day, theme.orange),
            ("< 1 week", ttl.under_week, theme.yellow),
            ("> 1 week", ttl.over_week, theme.green),
        ]
        .into_iter()
        .filter(|(_, n, _)| *n > 0)
        .map(|(label, n, color)| bar_row(label, &n.to_string(), n, ttl_total, color, theme))
        .collect();

        div()
            .id("kv-analysis-report")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(section_label("Types by memory"))
            .children(type_rows)
            .child(section_label(&format!(
                "Top {} namespaces by memory",
                report.namespaces.len()
            )))
            .children(ns_rows)
            .child(section_label("Expiry"))
            .children(ttl_rows)
            .into_any_element()
    }

    /// The value inspector panel: key/type/TTL header, then the value
    /// rendered per type, docked to the right of the keyspace table.
    fn render_kv_inspector(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity().downgrade();

        // --- key name; the rename affordance opens a popover (see the
        // `rename_popover` overlay on the panel root below) rather than swapping
        // the header inline. ---
        let key_row = {
            let rename_view = view.clone();
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(theme.scale(12.))
                        .child(inspector.key.clone()),
                )
                .when(writable, |d| {
                    d.child(
                        IconButton::new(
                            "kv-rename-start",
                            crate::icons::icon("edit", theme.scale(12.), theme.text_muted),
                        )
                        .size(IconButtonSize::Sm)
                        .tooltip("Rename key")
                        .a11y_label("Rename key")
                        .active(inspector.editing_key)
                        .on_click(move |_, _, cx| {
                            rename_view
                                .update(cx, |this, cx| this.kv_start_editing_key(session, cx))
                                .ok();
                        }),
                    )
                })
                .into_any_element()
        };

        // --- TTL; clicking opens the expiry popover (see `ttl_popover` below). ---
        let ttl_row = {
            let ttl_view = view.clone();
            let label = div()
                .text_size(theme.scale(10.5))
                .text_color(theme.text_muted)
                .child(fmt_ttl(inspector.ttl));
            if writable {
                div()
                    .id("kv-ttl-start")
                    .cursor_pointer()
                    .child(label)
                    .on_click(move |_, _, cx| {
                        ttl_view
                            .update(cx, |this, cx| this.kv_start_editing_ttl(session, cx))
                            .ok();
                    })
                    .into_any_element()
            } else {
                label.into_any_element()
            }
        };

        let delete_button = writable.then(|| {
            let delete_view = view.clone();
            IconButton::new(
                "kv-inspector-delete",
                crate::icons::icon("trash", theme.scale(13.), theme.red),
            )
            .size(IconButtonSize::Sm)
            .tooltip("Delete key")
            .a11y_label("Delete key")
            .on_click(move |_, _, cx| {
                delete_view
                    .update(cx, |this, cx| this.kv_request_delete(session, cx))
                    .ok();
            })
        });

        let close_view = view.clone();
        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .child(key_row)
            .child(type_pill(&inspector.kv_type, theme))
            .child(ttl_row)
            .children(delete_button)
            .child(
                IconButton::new(
                    "kv-inspector-close",
                    crate::icons::icon("x", theme.scale(13.), theme.text_muted),
                )
                .size(IconButtonSize::Sm)
                .tooltip("Close")
                .a11y_label("Close inspector")
                .on_click(move |_, _, cx| {
                    close_view
                        .update(cx, |this, cx| this.kv_close_inspector(session, cx))
                        .ok();
                }),
            );

        let confirm_delete = inspector.confirm_delete.then(|| {
            let (confirm_view, cancel_view) = (view.clone(), view.clone());
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1p5()
                .bg(theme.red.opacity(0.1))
                .border_b_1()
                .border_color(theme.red)
                .child(
                    div()
                        .flex_1()
                        .text_size(theme.scale(11.))
                        .text_color(theme.text)
                        .child(format!(
                            "Delete \"{}\"? This can't be undone.",
                            inspector.key
                        )),
                )
                .child(
                    Button::new("kv-inspector-delete-confirm", "Delete")
                        .variant(ButtonVariant::Danger)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            confirm_view
                                .update(cx, |this, cx| this.kv_confirm_delete(session, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new("kv-inspector-delete-cancel", "Cancel")
                        .variant(ButtonVariant::Secondary)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            cancel_view
                                .update(cx, |this, cx| this.kv_cancel_delete(session, cx))
                                .ok();
                        }),
                )
        });

        // Rename / expiry each open as a small popover card anchored under the
        // header, dismissed by an outside click on the panel-wide backdrop, the
        // Cancel button (secondary), or Escape/Submit in the field. Replaces the
        // old inline header-swap so the header stays legible while editing.
        let rename_popover = inspector.editing_key.then(|| {
            self.kv_edit_popover(
                session,
                "kv-rename",
                "Rename key",
                None,
                inspector.rename_editor.clone().into_any_element(),
                "Rename",
                theme,
                cx,
                |this, session, cx| this.kv_submit_rename(session, cx),
                |this, session, cx| this.kv_cancel_editing_key(session, cx),
            )
        });
        let ttl_popover = inspector.editing_ttl.then(|| {
            let ttl_field = div()
                .child(inspector.ttl_editor.clone())
                .children(inspector.ttl_error.clone().map(|e| {
                    div()
                        .text_size(theme.scale(10.))
                        .text_color(theme.red)
                        .child(e)
                }))
                .into_any_element();
            self.kv_edit_popover(
                session,
                "kv-ttl",
                "Set expiry",
                Some("Seconds from now. Blank clears the expiry (persist).".into()),
                ttl_field,
                "Set",
                theme,
                cx,
                |this, session, cx| this.kv_submit_ttl_edit(session, cx),
                |this, session, cx| this.kv_cancel_editing_ttl(session, cx),
            )
        });

        let collection_popover = inspector
            .collection_edit
            .clone()
            .map(|kind| self.render_kv_collection_popover(session, &kind, inspector, theme, cx));

        let body = self.render_kv_value(session, inspector, writable, theme, cx);

        div()
            .relative()
            .flex_shrink_0()
            .w(px(380.))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.bg_panel)
            .child(header)
            .children(confirm_delete)
            .child(body)
            .children(rename_popover)
            .children(ttl_popover)
            .children(collection_popover)
    }

    /// The add/edit-element popover for a collection key. Shows one or two
    /// inputs (member/field + value/score) plus any inline validation error,
    /// reusing [`kv_edit_popover`]'s card + backdrop.
    fn render_kv_collection_popover(
        &self,
        session: SessionId,
        kind: &CollectionEditKind,
        inspector: &KvInspector,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let name = inspector.elem_name_editor.clone();
        let value = inspector.elem_value_editor.clone();
        // (title, save label, two_input, name_placeholder)
        let (title, save_label, two_input): (&'static str, &'static str, bool) = match kind {
            CollectionEditKind::AddHashField => ("Add field", "Add", true),
            CollectionEditKind::EditHashField { .. } => ("Edit value", "Save", false),
            CollectionEditKind::AddSetMember => ("Add member", "Add", false),
            CollectionEditKind::EditSetMember { .. } => ("Edit member", "Save", false),
            CollectionEditKind::AddZSetMember => ("Add member", "Add", true),
            CollectionEditKind::EditZSetScore { .. } => ("Edit score", "Save", false),
            CollectionEditKind::AddListHead => ("Prepend item", "Add", false),
            CollectionEditKind::AddListTail => ("Append item", "Add", false),
            CollectionEditKind::EditListIndex { .. } => ("Edit item", "Save", false),
        };
        // The single-input variants that edit `elem_value` (hash value / zset
        // score) rather than `elem_name` (member / list value).
        let single_is_value = matches!(
            kind,
            CollectionEditKind::EditHashField { .. } | CollectionEditKind::EditZSetScore { .. }
        );

        let field = if two_input {
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(name.into_any_element())
                .child(value.into_any_element())
        } else if single_is_value {
            div().child(value.into_any_element())
        } else {
            div().child(name.into_any_element())
        };
        let field = field
            .children(inspector.elem_error.clone().map(|e| {
                div()
                    .text_size(theme.scale(10.))
                    .text_color(theme.red)
                    .child(e)
            }))
            .into_any_element();

        let hint: Option<SharedString> = matches!(kind, CollectionEditKind::AddZSetMember)
            .then(|| "Score is a number (e.g. 1.0).".into());

        self.kv_edit_popover(
            session,
            "kv-elem",
            title,
            hint,
            field,
            save_label,
            theme,
            cx,
            |this, session, cx| this.kv_submit_collection_edit(session, cx),
            |this, session, cx| this.kv_cancel_collection_edit(session, cx),
        )
    }

    /// A small popover card anchored just under the inspector header, for
    /// editing the key name or its expiry. Its own panel-wide backdrop dismisses
    /// on an outside click; the card carries a title, the caller's editor field,
    /// an optional hint, and Save (primary) / Cancel (secondary) buttons.
    #[allow(clippy::too_many_arguments)]
    fn kv_edit_popover(
        &self,
        session: SessionId,
        id: &'static str,
        title: impl Into<SharedString>,
        hint: Option<SharedString>,
        field: gpui::AnyElement,
        save_label: &'static str,
        theme: &Theme,
        cx: &mut Context<Self>,
        on_save: impl Fn(&mut Self, SessionId, &mut Context<Self>) + 'static,
        on_cancel: impl Fn(&mut Self, SessionId, &mut Context<Self>) + Clone + 'static,
    ) -> gpui::AnyElement {
        let view = cx.entity().downgrade();
        let (save_view, cancel_view, backdrop_view) = (view.clone(), view.clone(), view.clone());
        let on_cancel_backdrop = on_cancel.clone();
        let title = title.into();
        let card = div()
            .occlude()
            .w(px(300.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_lg()
            .child(
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(title),
            )
            .child(field)
            .children(hint.map(|h| {
                div()
                    .text_size(theme.scale(10.))
                    .text_color(theme.text_muted)
                    .child(h)
            }))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("{id}-save")), save_label)
                            .variant(ButtonVariant::Primary)
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                save_view
                                    .update(cx, |this, cx| on_save(this, session, cx))
                                    .ok();
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{id}-cancel")), "Cancel")
                            .variant(ButtonVariant::Secondary)
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                cancel_view
                                    .update(cx, |this, cx| on_cancel(this, session, cx))
                                    .ok();
                            }),
                    ),
            );
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                backdrop_view
                    .update(cx, |this, cx| on_cancel_backdrop(this, session, cx))
                    .ok();
            })
            .child(floating(card).offset(point(px(8.), px(38.))))
            .into_any_element()
    }

    /// The string inspector's lens toolbar (Auto/Raw/JSON/Hex + the binary
    /// decoders), reusing the SQL inspector's `ValueFormat`. Lets a Redis
    /// string holding msgpack/protobuf/pickle be decoded in place, the same way
    /// a SQL blob cell can (see docs/plans/redis.md's "binary value decoders").
    fn render_kv_str_lens(
        &self,
        session: SessionId,
        current: crate::inspector::ValueFormat,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::inspector::ValueFormat;
        let opt = |id: &'static str, label: &'static str, fmt: ValueFormat| {
            let view = cx.entity().downgrade();
            Button::new(id, label)
                .variant(if current == fmt {
                    ButtonVariant::Secondary
                } else {
                    ButtonVariant::Ghost
                })
                .size(ButtonSize::Sm)
                .on_click(move |_, _, cx| {
                    view.update(cx, |this, cx| this.kv_set_str_format(session, fmt, cx))
                        .ok();
                })
        };
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(opt("kv-fmt-auto", "Auto", ValueFormat::Auto))
            .child(opt("kv-fmt-raw", "Raw", ValueFormat::Raw))
            .child(opt("kv-fmt-json", "JSON", ValueFormat::Json))
            .child(opt("kv-fmt-hex", "Hex", ValueFormat::Hex))
            .child(opt("kv-fmt-msgpack", "MsgPack", ValueFormat::MsgPack))
            .child(opt("kv-fmt-protobuf", "Protobuf", ValueFormat::Protobuf))
            .child(opt("kv-fmt-pickle", "Pickle", ValueFormat::Pickle))
            .child(opt("kv-fmt-timestamp", "Time", ValueFormat::Timestamp))
            .child(opt("kv-fmt-decompress", "Gzip", ValueFormat::Decompress))
            .into_any_element()
    }

    /// The inspector's value area: a per-type renderer for a loaded value, a
    /// paged sub-grid for a big collection, or a loading/unsupported note.
    fn render_kv_value(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let text_size = theme.scale(11.5);
        let dim = theme.text_muted;
        let mono = theme.mono_family.clone();
        let view = cx.entity().downgrade();

        // An error reading the value, or a key that vanished between listing
        // and read, gets a settled message rather than a permanent "Loading…".
        if let Some(err) = &inspector.value_error {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_3()
                .text_size(text_size)
                .text_color(theme.red)
                .child(format!("Couldn't read value: {err}"))
                .into_any_element();
        }
        let Some(value) = &inspector.value else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(text_size)
                .text_color(dim)
                .child(if inspector.value_loaded {
                    "This key no longer exists"
                } else {
                    "Loading…"
                })
                .into_any_element();
        };

        match value {
            KvValue::Str(_) if inspector.editing_value => {
                let (save_view, cancel_view) = (view.clone(), view.clone());
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .child(
                        // Full-height editor (it scrolls itself): the value body
                        // becomes editable in place, inheriting the mono
                        // typography just like the read-only preview.
                        div()
                            .id("kv-inspector-string-edit")
                            .flex_1()
                            .min_h(px(0.))
                            .font_family(mono.clone())
                            .text_size(text_size)
                            .line_height(text_size * 1.5)
                            .child(inspector.value_editor.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(
                                Button::new("kv-string-save", "Save")
                                    .variant(ButtonVariant::Primary)
                                    .size(ButtonSize::Sm)
                                    .on_click(move |_, _, cx| {
                                        save_view
                                            .update(cx, |this, cx| {
                                                this.kv_submit_value_edit(session, cx)
                                            })
                                            .ok();
                                    }),
                            )
                            .child(
                                Button::new("kv-string-cancel", "Cancel")
                                    .variant(ButtonVariant::Secondary)
                                    .size(ButtonSize::Sm)
                                    .on_click(move |_, _, cx| {
                                        cancel_view
                                            .update(cx, |this, cx| {
                                                this.kv_cancel_editing_value(session, cx)
                                            })
                                            .ok();
                                    }),
                            ),
                    )
                    .into_any_element()
            }
            KvValue::Str(v) => {
                // A `read_value`-capped string shows only its head; editing that
                // would save the truncated head back over the whole key, so a
                // capped value is view-only until "Load full value" pulls the
                // rest (see `kv_load_full_value`).
                let capped = matches!(v, red_core::Value::Capped(_));
                // Editing makes sense for any textual value; a binary value (a
                // `Value::Blob`, see `cap_string_value`) is view-only. A capped
                // text is editable too — Edit fetches the full value first (see
                // `kv_start_editing_value`) so a save never truncates the key.
                let editable = matches!(v, red_core::Value::Text(_))
                    || matches!(v, red_core::Value::Capped(c) if !c.blob);
                let loading_full = inspector.loading_full_value;
                // Prefer the selectable read-only preview editor (mirrors the
                // SQL cell inspector); fall back to plain, non-selectable text
                // for the frame or two before `kv_rebuild_str_preview` runs.
                let body_el = match &inspector.str_preview {
                    Some(p) => div()
                        .flex_1()
                        .min_h(px(0.))
                        .font_family(mono.clone())
                        .text_size(text_size)
                        .line_height(text_size * 1.5)
                        .child(p.editor.clone())
                        .into_any_element(),
                    None => {
                        let (body, _summary, _wrap) =
                            crate::inspector::format_value_body(v, inspector.str_format);
                        div()
                            .id("kv-inspector-string")
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .p_2()
                            .child(
                                div()
                                    .font_family(mono.clone())
                                    .text_size(text_size)
                                    .child(body),
                            )
                            .into_any_element()
                    }
                };
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .child(self.render_kv_str_lens(session, inspector.str_format, theme, cx))
                    .child(body_el)
                    .child({
                        let (edit_view, load_view, copy_view) =
                            (view.clone(), view.clone(), view.clone());
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(
                                Button::new("kv-string-copy", "Copy")
                                    .variant(ButtonVariant::Secondary)
                                    .size(ButtonSize::Sm)
                                    .on_click(move |_, _, cx| {
                                        copy_view
                                            .update(cx, |this, cx| {
                                                this.kv_copy_string_value(session, cx)
                                            })
                                            .ok();
                                    }),
                            )
                            .when(capped, |d| {
                                d.child(
                                    Button::new(
                                        "kv-string-load-full",
                                        if loading_full {
                                            "Loading…"
                                        } else {
                                            "Load full value"
                                        },
                                    )
                                    .variant(ButtonVariant::Primary)
                                    .size(ButtonSize::Sm)
                                    .disabled(loading_full)
                                    .on_click(
                                        move |_, _, cx| {
                                            load_view
                                                .update(cx, |this, cx| {
                                                    this.kv_load_full_value(session, cx)
                                                })
                                                .ok();
                                        },
                                    ),
                                )
                                // The whole value is bigger than the preview:
                                // say so, so the truncation reads as deliberate.
                                .child(
                                    div()
                                        .text_size(theme.scale(11.))
                                        .text_color(dim)
                                        .child("Preview truncated"),
                                )
                            })
                            .when(writable && editable, |d| {
                                d.child(
                                    Button::new("kv-string-edit", "Edit")
                                        .size(ButtonSize::Sm)
                                        .disabled(loading_full)
                                        .on_click(move |_, _, cx| {
                                            edit_view
                                                .update(cx, |this, cx| {
                                                    this.kv_start_editing_value(session, cx)
                                                })
                                                .ok();
                                        }),
                                )
                            })
                    })
                    .into_any_element()
            }
            KvValue::Stream(_) => self.render_kv_stream(session, inspector, writable, theme, cx),
            KvValue::Unsupported(kind) => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_2()
                .text_size(text_size)
                .text_color(dim)
                .child(format!(
                    "Preview not available for {} keys yet",
                    kind.label()
                ))
                .into_any_element(),
            KvValue::Hash(KvCollection::Loaded(pairs)) => {
                let rows: Rc<Vec<KvElement>> = Rc::new(
                    pairs
                        .iter()
                        .map(|(f, v)| KvElement::Field(f.clone(), v.clone()))
                        .collect(),
                );
                let len = rows.len() as u64;
                self.render_kv_collection_grid(
                    session,
                    CollectionKind::Hash,
                    rows,
                    len,
                    true,
                    writable,
                    inspector,
                    theme,
                    cx,
                )
            }
            KvValue::Set(KvCollection::Loaded(members)) => {
                let rows: Rc<Vec<KvElement>> = Rc::new(
                    members
                        .iter()
                        .map(|m| KvElement::Member(m.clone()))
                        .collect(),
                );
                let len = rows.len() as u64;
                self.render_kv_collection_grid(
                    session,
                    CollectionKind::Set,
                    rows,
                    len,
                    true,
                    writable,
                    inspector,
                    theme,
                    cx,
                )
            }
            KvValue::ZSet(KvCollection::Loaded(pairs)) => {
                let rows: Rc<Vec<KvElement>> = Rc::new(
                    pairs
                        .iter()
                        .map(|(m, s)| KvElement::Scored(m.clone(), *s))
                        .collect(),
                );
                let len = rows.len() as u64;
                self.render_kv_collection_grid(
                    session,
                    CollectionKind::ZSet,
                    rows,
                    len,
                    true,
                    writable,
                    inspector,
                    theme,
                    cx,
                )
            }
            KvValue::List(KvCollection::Loaded(items)) => {
                let rows = items.clone();
                let len = rows.len() as u64;
                self.render_kv_list(session, rows, len, false, writable, theme, cx)
            }
            KvValue::Hash(KvCollection::Large { len }) => self.render_kv_collection_grid(
                session,
                CollectionKind::Hash,
                inspector.collection_rows.clone(),
                *len,
                inspector.collection_exhausted,
                writable,
                inspector,
                theme,
                cx,
            ),
            KvValue::Set(KvCollection::Large { len }) => self.render_kv_collection_grid(
                session,
                CollectionKind::Set,
                inspector.collection_rows.clone(),
                *len,
                inspector.collection_exhausted,
                writable,
                inspector,
                theme,
                cx,
            ),
            KvValue::ZSet(KvCollection::Large { len }) => self.render_kv_collection_grid(
                session,
                CollectionKind::ZSet,
                inspector.collection_rows.clone(),
                *len,
                inspector.collection_exhausted,
                writable,
                inspector,
                theme,
                cx,
            ),
            KvValue::List(KvCollection::Large { len }) => {
                let rows: Vec<String> = inspector
                    .collection_rows
                    .iter()
                    .filter_map(|e| match e {
                        KvElement::Member(v) => Some(v.clone()),
                        _ => None,
                    })
                    .collect();
                self.render_kv_list(session, rows, *len, true, writable, theme, cx)
            }
        }
    }

    /// The hash/set/zset element grid: the same `Table` + `on_visible_range`
    /// load-more shape as the keyspace browser, scoped to one key's elements,
    /// now serving both the small (fully `Loaded`) and big (`Large`, paged)
    /// cases. On a writable connection each row carries inline edit/delete
    /// buttons and the toolbar an Add button (see the element popover).
    #[allow(clippy::too_many_arguments)]
    fn render_kv_collection_grid(
        &self,
        session: SessionId,
        kind: CollectionKind,
        rows: Rc<Vec<KvElement>>,
        total_len: u64,
        complete: bool,
        writable: bool,
        inspector: &KvInspector,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let dim = theme.text_muted;
        let cell_size = theme.scale(11.5);
        let edit_color = theme.text_muted;
        let del_color = theme.red;
        let icon_sz = theme.scale(12.);
        let rows_render = rows.clone();
        let row_count = rows.len();

        let mut columns = match kind {
            CollectionKind::Hash => vec![
                Column::new("Field").width(px(150.)),
                Column::new("Value").flex(),
            ],
            CollectionKind::Set => vec![Column::new("Member").flex()],
            CollectionKind::ZSet => vec![
                Column::new("Member").flex(),
                Column::new("Score").width(px(90.)).align_end(),
            ],
        };
        if writable {
            columns.push(Column::new("").width(px(60.)).align_end());
        }

        let action_view = cx.entity().downgrade();
        let mut table = Table::<()>::new("kv-inspector-grid", columns)
            .row_count(row_count)
            .grid_lines(true)
            .text_size(cell_size)
            .track_scroll(&inspector.collection_scroll)
            .render_row(move |ix, _window, _cx| {
                let Some(el) = rows_render.get(ix) else {
                    return Vec::new();
                };
                let mut cells = match el {
                    KvElement::Field(f, v) => vec![
                        div()
                            .min_w_0()
                            .truncate()
                            .child(f.clone())
                            .into_any_element(),
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(dim)
                            .child(v.clone())
                            .into_any_element(),
                    ],
                    KvElement::Scored(m, s) => vec![
                        div()
                            .min_w_0()
                            .truncate()
                            .child(m.clone())
                            .into_any_element(),
                        div()
                            .text_color(dim)
                            .child(format!("{s}"))
                            .into_any_element(),
                    ],
                    KvElement::Member(m) => vec![div()
                        .min_w_0()
                        .truncate()
                        .child(m.clone())
                        .into_any_element()],
                };
                if writable {
                    cells.push(collection_row_actions(
                        &action_view,
                        session,
                        kind,
                        ix,
                        el,
                        edit_color,
                        del_color,
                        icon_sz,
                    ));
                }
                cells
            });
        // Only page a `Large` collection; a fully loaded one has nothing more
        // to fetch (and firing load-more would issue a needless `*SCAN`).
        if !complete {
            let load_view = cx.entity().downgrade();
            table = table.on_visible_range(move |range, _window, cx| {
                load_view
                    .update(cx, |this, cx| {
                        this.kv_inspector_maybe_load_more(session, kind, range.end, cx)
                    })
                    .ok();
            });
        }

        let note = if complete {
            format!("{total_len} elements")
        } else {
            format!("{total_len} elements, paging as you scroll")
        };
        let (add_kind, add_label) = match kind {
            CollectionKind::Hash => (CollectionEditKind::AddHashField, "+ Field"),
            CollectionKind::Set => (CollectionEditKind::AddSetMember, "+ Member"),
            CollectionKind::ZSet => (CollectionEditKind::AddZSetMember, "+ Member"),
        };
        let add_view = cx.entity().downgrade();
        let toolbar = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .child(
                div()
                    .flex_1()
                    .text_size(theme.scale(10.5))
                    .text_color(dim)
                    .child(note),
            )
            .when(writable, |d| {
                d.child(
                    Button::new("kv-coll-add", add_label)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            let k = add_kind.clone();
                            add_view
                                .update(cx, |this, cx| {
                                    this.kv_open_collection_edit(
                                        session,
                                        k,
                                        String::new(),
                                        String::new(),
                                        cx,
                                    )
                                })
                                .ok();
                        }),
                )
            });

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(toolbar)
            .child(div().flex_1().min_h(px(0.)).child(table))
            .into_any_element()
    }

    /// A list key's element view (small fully loaded, or a big list's head
    /// window). Each row carries inline edit (`LSET` by index) / delete
    /// (`LREM`) buttons on a writable connection, plus Prepend/Append in the
    /// toolbar. A big list stays head-only (see `LIST_PREVIEW_COUNT`), so
    /// editing reaches only the head window it shows.
    #[allow(clippy::too_many_arguments)]
    fn render_kv_list(
        &self,
        session: SessionId,
        rows: Vec<String>,
        total_len: u64,
        head_only: bool,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let shown = rows.len();
        let note = if head_only {
            format!("showing the first {shown} of {total_len} items (head only)")
        } else {
            format!("{total_len} items")
        };
        let edit_color = theme.text_muted;
        let del_color = theme.red;
        let icon_sz = theme.scale(12.);

        let items: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let row = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .child(
                        div()
                            .w(px(36.))
                            .flex_shrink_0()
                            .text_color(theme.text_faint)
                            .child(i.to_string()),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(v.clone()));
                if !writable {
                    return row.into_any_element();
                }
                let edit_view = cx.entity().downgrade();
                let del_view = cx.entity().downgrade();
                let value_edit = v.clone();
                let idx = i as i64;
                row.child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .gap_0p5()
                        .child(
                            IconButton::new(
                                SharedString::from(format!("kv-li-edit-{i}")),
                                crate::icons::icon("edit", icon_sz, edit_color),
                            )
                            .size(IconButtonSize::Sm)
                            .tooltip("Edit")
                            .a11y_label("Edit item")
                            .on_click(move |_, _, cx| {
                                let v = value_edit.clone();
                                edit_view
                                    .update(cx, |this, cx| {
                                        this.kv_open_collection_edit(
                                            session,
                                            CollectionEditKind::EditListIndex { index: idx },
                                            v,
                                            String::new(),
                                            cx,
                                        )
                                    })
                                    .ok();
                            }),
                        )
                        .child(
                            IconButton::new(
                                SharedString::from(format!("kv-li-del-{i}")),
                                crate::icons::icon("trash", icon_sz, del_color),
                            )
                            .size(IconButtonSize::Sm)
                            .tooltip("Delete")
                            .a11y_label("Delete item")
                            .on_click(move |_, _, cx| {
                                del_view
                                    .update(cx, |this, cx| {
                                        this.kv_send_element_edit(
                                            session,
                                            move |key| red_core::kv::KvEdit::ListRemoveAt {
                                                key,
                                                index: idx,
                                            },
                                            cx,
                                        )
                                    })
                                    .ok();
                            }),
                        ),
                )
                .into_any_element()
            })
            .collect();

        let head_view = cx.entity().downgrade();
        let tail_view = cx.entity().downgrade();
        let toolbar = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .child(
                div()
                    .flex_1()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(note),
            )
            .when(writable, |d| {
                d.child(
                    Button::new("kv-list-add-head", "+ Head")
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            head_view
                                .update(cx, |this, cx| {
                                    this.kv_open_collection_edit(
                                        session,
                                        CollectionEditKind::AddListHead,
                                        String::new(),
                                        String::new(),
                                        cx,
                                    )
                                })
                                .ok();
                        }),
                )
                .child(
                    Button::new("kv-list-add-tail", "+ Tail")
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            tail_view
                                .update(cx, |this, cx| {
                                    this.kv_open_collection_edit(
                                        session,
                                        CollectionEditKind::AddListTail,
                                        String::new(),
                                        String::new(),
                                        cx,
                                    )
                                })
                                .ok();
                        }),
                )
            });

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(toolbar)
            .child(
                div()
                    .id("kv-inspector-list")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .text_size(theme.scale(11.5))
                    .children(items),
            )
            .into_any_element()
    }

    /// The stream inspector body: a segmented `Entries | Groups` toggle over
    /// either the entries view (loaded list or paged sub-grid) or the
    /// consumer-group management panel (see docs/plans/redis.md's "stream
    /// consumer-group management" gap).
    fn render_kv_stream(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let view = inspector.stream_groups.view;
        let tab = |label: &'static str, this_view: StreamView| {
            let active = view == this_view;
            let tab_view = cx.entity().downgrade();
            div()
                .id(label)
                .px_2()
                .py_0p5()
                .cursor_pointer()
                .text_size(theme.scale(11.))
                .text_color(if active { theme.text } else { theme.text_muted })
                .border_b_2()
                .border_color(if active {
                    theme.accent
                } else {
                    theme.border.opacity(0.)
                })
                .child(label)
                .on_click(move |_, _, cx| {
                    tab_view
                        .update(cx, |this, cx| {
                            this.kv_set_stream_view(session, this_view, cx)
                        })
                        .ok();
                })
        };

        let toggle = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(tab("Entries", StreamView::Entries))
            .child(tab("Groups", StreamView::Groups));

        let body = match view {
            StreamView::Entries => match &inspector.value {
                Some(KvValue::Stream(KvCollection::Loaded(entries))) => {
                    render_loaded_stream(entries, theme)
                }
                Some(KvValue::Stream(KvCollection::Large { len })) => {
                    self.render_kv_stream_grid(session, *len, inspector, theme, cx)
                }
                _ => div().flex_1().into_any_element(),
            },
            StreamView::Groups => {
                self.render_kv_stream_groups(session, inspector, writable, theme, cx)
            }
        };

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(toggle)
            .child(body)
            .into_any_element()
    }

    /// The consumer-group management panel: the stream's groups, and the
    /// selected group's consumers + pending entries with per-entry
    /// `XACK`/`XCLAIM` actions when the connection is writable.
    fn render_kv_stream_groups(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let st = &inspector.stream_groups;
        let dim = theme.text_muted;
        let text_size = theme.scale(11.);

        let note = |msg: &str| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_2()
                .text_size(text_size)
                .text_color(dim)
                .child(msg.to_string())
                .into_any_element()
        };

        if st.groups.is_empty() {
            return if st.loading || !st.loaded {
                note("Loading groups…")
            } else {
                note("No consumer groups on this stream.")
            };
        }

        // The groups list: one clickable row each, the selected one tinted.
        let group_rows: Vec<_> = st
            .groups
            .iter()
            .map(|g| {
                let selected = st.selected.as_deref() == Some(&g.name);
                let select_view = cx.entity().downgrade();
                let name = g.name.clone();
                let lag = g.lag.map(|l| format!(" · lag {l}")).unwrap_or_default();
                div()
                    .id(SharedString::from(format!("kv-group-{}", g.name)))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.accent.opacity(0.12)))
                    .hover(|d| d.bg(theme.bg_hover))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(text_size)
                            .text_color(if selected {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .child(g.name.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(theme.scale(10.))
                            .text_color(dim)
                            .child(format!("{}c · {}p{lag}", g.consumers, g.pending)),
                    )
                    .on_click(move |_, _, cx| {
                        select_view
                            .update(cx, |this, cx| {
                                this.kv_select_stream_group(session, name.clone(), cx)
                            })
                            .ok();
                    })
                    .into_any_element()
            })
            .collect();

        let groups_list = div()
            .id("kv-groups-list")
            .flex_shrink_0()
            .max_h(px(120.))
            .overflow_y_scroll()
            .border_b_1()
            .border_color(theme.border)
            .children(group_rows);

        let detail = st
            .selected
            .as_ref()
            .map(|_| self.render_kv_group_detail(session, inspector, writable, theme, cx));

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(groups_list)
            .children(detail)
            .into_any_element()
    }

    /// The selected group's detail: its consumers, then its pending entries,
    /// each with `Ack`/`Claim` affordances when writable.
    fn render_kv_group_detail(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let st = &inspector.stream_groups;
        let dim = theme.text_muted;
        let text_size = theme.scale(11.);
        let section_label = |s: &str| {
            div()
                .flex_shrink_0()
                .px_2()
                .py_0p5()
                .text_size(theme.scale(9.5))
                .text_color(dim)
                .child(s.to_string().to_uppercase())
        };

        // Consumers.
        let consumer_rows: Vec<_> = st
            .consumers
            .iter()
            .map(|c| {
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .text_size(text_size)
                    .child(div().min_w_0().truncate().child(c.name.clone()))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(theme.scale(10.))
                            .text_color(dim)
                            .child(format!("{}p · idle {}", c.pending, fmt_idle(c.idle))),
                    )
                    .into_any_element()
            })
            .collect();

        let consumers_empty = st.consumers.is_empty();

        // Pending entries.
        let pending_rows: Vec<_> = st
            .pending
            .iter()
            .map(|p| self.render_pending_row(session, inspector, p, writable, theme, cx))
            .collect();
        let pending_empty = st.pending.is_empty();
        let pending_header = format!(
            "Pending ({}{})",
            st.pending.len(),
            if st.pending.len() >= STREAM_PENDING_COUNT {
                "+"
            } else {
                ""
            }
        );

        div()
            .id("kv-group-detail")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(section_label("Consumers"))
            .when(consumers_empty, |d| {
                d.child(
                    div()
                        .px_2()
                        .py_0p5()
                        .text_size(text_size)
                        .text_color(dim)
                        .child("No consumers."),
                )
            })
            .children(consumer_rows)
            .child(section_label(&pending_header))
            .when(pending_empty && !st.detail_loading, |d| {
                d.child(
                    div()
                        .px_2()
                        .py_0p5()
                        .text_size(text_size)
                        .text_color(dim)
                        .child("Nothing pending — all delivered entries are acknowledged."),
                )
            })
            .children(pending_rows)
            .into_any_element()
    }

    /// One pending entry row: id, consumer, idle, delivery-count, plus an
    /// `Ack`/`Claim` action pair (writable only). The row expands to an inline
    /// claim form while this entry is the one being claimed.
    fn render_pending_row(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        entry: &PendingEntry,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let dim = theme.text_muted;
        let text_size = theme.scale(11.);
        let claiming = inspector.stream_groups.claiming.as_deref() == Some(&entry.id);

        let meta = div().flex().items_center().justify_between().gap_2().child(
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(theme.mono_family.clone())
                        .text_size(theme.scale(10.5))
                        .child(entry.id.clone()),
                )
                .child(
                    div()
                        .text_size(theme.scale(9.5))
                        .text_color(dim)
                        .child(format!(
                            "{} · idle {} · delivered {}×",
                            entry.consumer,
                            fmt_idle(entry.idle),
                            entry.delivery_count
                        )),
                ),
        );

        let actions = writable.then(|| {
            let id_ack = entry.id.clone();
            let ack_view = cx.entity().downgrade();
            let id_claim = entry.id.clone();
            let claim_view = cx.entity().downgrade();
            div()
                .flex_shrink_0()
                .flex()
                .gap_1()
                .child(
                    Button::new(SharedString::from(format!("kv-ack-{}", entry.id)), "Ack")
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            ack_view
                                .update(cx, |this, cx| {
                                    this.kv_stream_ack(session, id_ack.clone(), cx)
                                })
                                .ok();
                        }),
                )
                .child(
                    Button::new(
                        SharedString::from(format!("kv-claim-{}", entry.id)),
                        "Claim",
                    )
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        claim_view
                            .update(cx, |this, cx| {
                                this.kv_start_claim(session, id_claim.clone(), cx)
                            })
                            .ok();
                    }),
                )
        });

        let claim_form = claiming.then(|| {
            let (submit_view, cancel_view) = (cx.entity().downgrade(), cx.entity().downgrade());
            div()
                .flex()
                .items_center()
                .gap_1()
                .pt_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(inspector.stream_groups.claim_editor.clone()),
                )
                .child(
                    Button::new("kv-claim-submit", "Claim")
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            submit_view
                                .update(cx, |this, cx| this.kv_submit_claim(session, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new("kv-claim-cancel", "Cancel")
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            cancel_view
                                .update(cx, |this, cx| this.kv_cancel_claim(session, cx))
                                .ok();
                        }),
                )
        });

        div()
            .flex()
            .flex_col()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border.opacity(0.5))
            .text_size(text_size)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(meta)
                    .children(actions),
            )
            .children(claim_form)
            .into_any_element()
    }

    /// The big-stream sub-grid: newest-first entries in a virtualized `Table`
    /// (ID + fields), paging older on scroll via `kv_load_stream_page`. Mirrors
    /// `render_kv_collection_grid`, but keyed off `stream_rows` and continuing
    /// by entry ID rather than a `*SCAN` cursor.
    fn render_kv_stream_grid(
        &self,
        session: SessionId,
        len: u64,
        inspector: &KvInspector,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let view = cx.entity().downgrade();
        let rows = inspector.stream_rows.clone();
        let rows_render = rows.clone();
        let row_count = rows.len();

        let columns = vec![
            Column::new("ID").width(px(160.)),
            Column::new("Fields").flex(),
        ];
        let dim = theme.text_muted;
        let cell_size = theme.scale(11.5);

        let table = Table::<()>::new("kv-inspector-stream", columns)
            .row_count(row_count)
            .grid_lines(true)
            .text_size(cell_size)
            .track_scroll(&inspector.stream_scroll)
            .render_row(move |ix, _window, _cx| match rows_render.get(ix) {
                Some(entry) => vec![
                    div()
                        .min_w_0()
                        .truncate()
                        .child(entry.id.clone())
                        .into_any_element(),
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(dim)
                        .child(fmt_stream_fields(&entry.fields))
                        .into_any_element(),
                ],
                None => Vec::new(),
            })
            .on_visible_range(move |range, _window, cx| {
                view.update(cx, |this, cx| {
                    this.kv_inspector_maybe_load_more_stream(session, range.end, cx)
                })
                .ok();
            });

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .text_size(theme.scale(10.5))
                    .text_color(dim)
                    .child(format!(
                        "{len} entries, newest first — paging as you scroll"
                    )),
            )
            .child(div().flex_1().min_h(px(0.)).child(table))
            .into_any_element()
    }
}

/// A compact human idle-time for the consumer-group view (`XINFO`/`XPENDING`
/// idle is in ms): `"820ms"`, `"3.4s"`, `"5m"`, `"2h"`, `"1d"`. Coarse on
/// purpose — the operator wants "how stuck is this", not millisecond precision.
fn fmt_idle(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 3_600_000 {
        format!("{}m", ms / 60_000)
    } else if ms < 86_400_000 {
        format!("{}h", ms / 3_600_000)
    } else {
        format!("{}d", ms / 86_400_000)
    }
}

/// Flatten a stream entry's field/value pairs into a compact one-line
/// preview (`field=value  field=value`) for the grid's Fields column.
fn fmt_stream_fields(fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(f, v)| format!("{f}={v}"))
        .collect::<Vec<_>>()
        .join("  ")
}

/// A small (< threshold) stream rendered as a plain scrollable list of
/// `ID → fields` rows, newest-first, capped by `SMALL_COLLECTION_THRESHOLD` so
/// it needs no virtualization.
fn render_loaded_stream(entries: &[StreamEntry], theme: &Theme) -> gpui::AnyElement {
    let dim = theme.text_muted;
    let items: Vec<_> = entries
        .iter()
        .map(|e| {
            div()
                .flex()
                .gap_2()
                .px_2()
                .py_0p5()
                .child(
                    div()
                        .w(px(150.))
                        .min_w_0()
                        .truncate()
                        .text_color(dim)
                        .child(e.id.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(fmt_stream_fields(&e.fields)),
                )
                .into_any_element()
        })
        .collect();
    div()
        .id("kv-inspector-loaded-stream")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .text_size(theme.scale(11.5))
        .children(items)
        .into_any_element()
}

/// The inline edit + delete buttons for one hash/set/zset element row (the
/// grid's trailing actions cell). Edit opens the element popover seeded with
/// the row's current content; Delete sends the type's remove edit immediately
/// (a single element is trivially re-addable, so it skips a confirm).
#[allow(clippy::too_many_arguments)]
fn collection_row_actions(
    view: &WeakEntity<AppState>,
    session: SessionId,
    kind: CollectionKind,
    ix: usize,
    el: &KvElement,
    edit_color: Hsla,
    del_color: Hsla,
    icon_sz: gpui::Pixels,
) -> gpui::AnyElement {
    use red_core::kv::KvEdit;
    // Edit: map the element to its popover kind + seeds.
    let (edit_kind, seed_name, seed_value) = match el {
        KvElement::Field(f, v) => (
            CollectionEditKind::EditHashField { field: f.clone() },
            f.clone(),
            v.clone(),
        ),
        KvElement::Scored(m, s) => (
            CollectionEditKind::EditZSetScore { member: m.clone() },
            m.clone(),
            format!("{s}"),
        ),
        KvElement::Member(m) => (
            CollectionEditKind::EditSetMember { old: m.clone() },
            m.clone(),
            String::new(),
        ),
    };
    let ident = match el {
        KvElement::Field(f, _) => f.clone(),
        KvElement::Scored(m, _) => m.clone(),
        KvElement::Member(m) => m.clone(),
    };
    let (edit_view, del_view) = (view.clone(), view.clone());
    div()
        .flex()
        .gap_0p5()
        .justify_end()
        .child(
            IconButton::new(
                SharedString::from(format!("kv-el-edit-{ix}")),
                crate::icons::icon("edit", icon_sz, edit_color),
            )
            .size(IconButtonSize::Sm)
            .tooltip("Edit")
            .a11y_label("Edit element")
            .on_click(move |_, _, cx| {
                let (k, n, v) = (edit_kind.clone(), seed_name.clone(), seed_value.clone());
                edit_view
                    .update(cx, |this, cx| {
                        this.kv_open_collection_edit(session, k, n, v, cx)
                    })
                    .ok();
            }),
        )
        .child(
            IconButton::new(
                SharedString::from(format!("kv-el-del-{ix}")),
                crate::icons::icon("trash", icon_sz, del_color),
            )
            .size(IconButtonSize::Sm)
            .tooltip("Delete")
            .a11y_label("Delete element")
            .on_click(move |_, _, cx| {
                let ident = ident.clone();
                del_view
                    .update(cx, |this, cx| {
                        this.kv_send_element_edit(
                            session,
                            move |key| match kind {
                                CollectionKind::Hash => KvEdit::HashDelete {
                                    key,
                                    fields: vec![ident],
                                },
                                CollectionKind::Set => KvEdit::SetRemove {
                                    key,
                                    members: vec![ident],
                                },
                                CollectionKind::ZSet => KvEdit::ZSetRemove {
                                    key,
                                    members: vec![ident],
                                },
                            },
                            cx,
                        )
                    })
                    .ok();
            }),
        )
        .into_any_element()
}

/// A string value's preview body: pretty-printed if it parses as JSON
/// (a common Redis string payload shape), else the raw text; a capped value
/// shows its prefix plus a "… (N bytes total)" note.
fn render_string_preview(value: &red_core::Value) -> String {
    match value {
        red_core::Value::Text(s) => s.to_string(),
        red_core::Value::Capped(cell) => {
            format!("{}\n\n… ({} bytes total, truncated)", cell.head, cell.len)
        }
        other => format!("{other:?}"),
    }
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
        assert!(opened
            .iter()
            .any(|r| matches!(r, DispRow::Folder { prefix, .. } if prefix == "user:1")));
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
