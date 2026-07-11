//! The Redis keyspace browser (R1, see docs/plans/redis.md): a forward-only
//! list of `SCAN`ned keys with their type/TTL/size/encoding. Deliberately
//! its own thing, not built on the SQL result grid's `GridBuffer`
//! (`crate::result::buffer`) — that's tied to offset/keyset paging an
//! unordered keyspace doesn't have (see the plan's "grid needs a third
//! buffer mode" section). Reuses Flint's `Table` (a generic, domain-free
//! virtualized list on `uniform_list`, the same primitive the SQL grid sits
//! on) directly instead: a plain growing `Vec` is all the "buffer" this
//! needs, no windowing/eviction/margin machinery.

use std::time::Duration;

use flint::prelude::*;
use gpui::{
    div, prelude::*, px, AsyncApp, Context, Entity, SharedString, UniformListScrollHandle,
    WeakEntity, Window,
};
use red_core::kv::{
    CollectionKind, KeyMeta, KvCollection, KvCollectionPage, KvElement, KvStreamActionReq,
    KvStreamPage, KvType, KvValue, PendingEntry, ScanBudget, ScanCursor, StreamAction,
    StreamConsumer, StreamEntry, StreamGroup,
};
use red_service::{Command, SessionId};

use crate::app::{ActiveConn, AppState};

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

/// Which of the Redis shell's panels is showing. Browse is the default and
/// the only one active session state doesn't need to preserve across
/// switches particularly carefully (console history and pubsub state live
/// on their own structs regardless of which panel is visible, so switching
/// away and back never loses anything).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KvPanel {
    Browse,
    Console,
    PubSub,
}

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

/// One connection's keyspace-browse state. Lives on [`ActiveConn`] for a
/// Redis session only (`None` for a SQL one).
pub(crate) struct RedisBrowse {
    /// Identifies the current scan run (bumped on restart — a new filter
    /// pattern — exactly like a SQL result's epoch bumps on re-sort). A
    /// `KvScanPage` reply whose epoch doesn't match is from a superseded
    /// scan and is dropped.
    pub(crate) epoch: u64,
    /// The pattern the current scan run applies (`None` = unfiltered `*`).
    pub(crate) pattern: Option<String>,
    /// Rows accumulated this run, forward-only, oldest-evicted past the cap.
    pub(crate) rows: Vec<KeyMeta>,
    pub(crate) cursor: ScanCursor,
    pub(crate) exhausted: bool,
    pub(crate) loading: bool,
    /// `DBSIZE`, fetched once at connect (unfiltered browses only show it —
    /// see docs/plans/redis.md on why there's no cheap filtered count).
    pub(crate) db_size: Option<u64>,
    pub(crate) scroll: UniformListScrollHandle,
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
    pub(crate) panel: KvPanel,
    pub(crate) console: crate::kvconsole::KvConsole,
    pub(crate) pubsub: crate::kvpubsub::KvPubSub,
    /// `Some` while a "find biggest keys" sample is running or showing its
    /// last result; `None` is the normal live-browse state.
    pub(crate) big_keys: Option<BigKeysState>,
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
    pub(crate) collection_rows: Vec<KvElement>,
    pub(crate) collection_cursor: u64,
    pub(crate) collection_exhausted: bool,
    pub(crate) collection_loading: bool,
    pub(crate) collection_scroll: UniformListScrollHandle,

    // --- big-stream paging (only populated once `value` reports a
    // `KvValue::Stream(KvCollection::Large)`; see docs/plans/redis.md's R4).
    // Streams page by entry-ID range rather than the `*SCAN` cursor the other
    // collections use, so they get their own accumulator instead of reusing
    // `collection_rows`. Entries accumulate newest-first, oldest-continued.
    pub(crate) stream_rows: Vec<StreamEntry>,
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
    pub(crate) value_editor: Entity<TextInput>,
    pub(crate) editing_value: bool,
    pub(crate) ttl_editor: Entity<TextInput>,
    pub(crate) editing_ttl: bool,
    pub(crate) rename_editor: Entity<TextInput>,
    pub(crate) editing_key: bool,
    pub(crate) confirm_delete: bool,
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

impl RedisBrowse {
    pub(crate) fn new(session: SessionId, cx: &mut Context<AppState>) -> Self {
        let filter = cx.new(|cx| TextInput::new(cx).with_placeholder("Filter (MATCH pattern)…"));
        cx.subscribe(&filter, move |this, input, event: &TextInputEvent, cx| {
            let fuzzy = this
                .conn_mut(Some(session))
                .and_then(|a| a.kv_browse.as_ref())
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
            rows: Vec::new(),
            cursor: ScanCursor::START,
            exhausted: false,
            loading: false,
            db_size: None,
            scroll: UniformListScrollHandle::new(),
            filter,
            filter_gen: 0,
            fuzzy: false,
            inspector: None,
            panel: KvPanel::Browse,
            console: crate::kvconsole::KvConsole::new(session, cx),
            pubsub: crate::kvpubsub::KvPubSub::new(cx),
            big_keys: None,
        }
    }
}

impl AppState {
    /// Kick off the very first scan + the one-time `DBSIZE` header stat, right
    /// after a Redis session connects.
    pub(crate) fn kv_start_browse(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
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
                    .and_then(|a| a.kv_browse.as_ref())
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
    /// close the superseded scan (cancels its in-flight fetch at the engine
    /// too, see `Command::CloseResult`'s doc comment), mint a fresh epoch,
    /// and start over from `cursor: 0` with the new pattern.
    pub(crate) fn kv_restart_scan(
        &mut self,
        session: SessionId,
        pattern: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        if browse.pattern == pattern {
            return; // same filter re-submitted: nothing to restart
        }
        let old_epoch = browse.epoch;
        let new_epoch = crate::result::next_kv_epoch();
        browse.epoch = new_epoch;
        browse.pattern = pattern.clone();
        browse.rows.clear();
        browse.cursor = ScanCursor::START;
        browse.exhausted = false;
        browse.loading = true;
        self.service
            .send_to(session, Command::CloseResult { epoch: old_epoch });
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch: new_epoch,
                pattern,
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
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        if browse.loading || browse.exhausted {
            return;
        }
        if visible_end + LOAD_AHEAD_ROWS < browse.rows.len() {
            return; // plenty of loaded rows still ahead of the viewport
        }
        browse.loading = true;
        let epoch = browse.epoch;
        let pattern = browse.pattern.clone();
        let cursor = browse.cursor;
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern,
                cursor,
                budget: scan_budget(),
            },
        );
        cx.notify();
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
        let is_big_keys = self
            .conn_mut(session)
            .and_then(|a| a.kv_browse.as_ref())
            .and_then(|b| b.big_keys.as_ref())
            .is_some_and(|bk| bk.epoch == epoch);
        if is_big_keys {
            self.on_big_keys_page(session, epoch, page, cx);
            return;
        }
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        if browse.epoch != epoch {
            return; // superseded scan run
        }
        browse.rows.extend(page.keys);
        if browse.rows.len() > MAX_RESIDENT_ROWS {
            let drop = browse.rows.len() - MAX_RESIDENT_ROWS;
            browse.rows.drain(0..drop);
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
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
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
        let cursor = browse.cursor;
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern: None,
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
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        let Some(bk) = browse.big_keys.take() else {
            return;
        };
        self.service
            .send_to(session, Command::CloseResult { epoch: bk.epoch });
        cx.notify();
    }

    pub(crate) fn kv_set_panel(
        &mut self,
        session: SessionId,
        panel: KvPanel,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        browse.panel = panel;
        cx.notify();
    }

    pub(crate) fn on_kv_db_size(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        count: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        if browse.epoch != epoch {
            return;
        }
        browse.db_size = Some(count);
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
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &active.kv_browse else {
            return;
        };
        let epoch = browse.epoch;

        let value_editor = cx.new(TextInput::new);
        cx.subscribe(&value_editor, move |this, _, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submit) {
                this.kv_submit_value_edit(session, cx);
            }
        })
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

        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        browse.inspector = Some(KvInspector {
            key: key.clone(),
            kv_type,
            ttl,
            value: None,
            collection_rows: Vec::new(),
            collection_cursor: 0,
            collection_exhausted: false,
            collection_loading: false,
            collection_scroll: UniformListScrollHandle::new(),
            stream_rows: Vec::new(),
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
            ttl_editor,
            editing_ttl: false,
            rename_editor,
            editing_key: false,
            confirm_delete: false,
        });
        self.service
            .send_to(session, Command::KvReadValue { epoch, key });
        cx.notify();
    }

    pub(crate) fn kv_close_inspector(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        let seed = match &inspector.value {
            Some(KvValue::Str(v)) => render_string_preview(v),
            _ => String::new(),
        };
        inspector
            .value_editor
            .update(cx, |ti, cx| ti.set_content(seed, cx));
        inspector.editing_value = true;
        cx.notify();
    }

    pub(crate) fn kv_cancel_editing_value(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.editing_value = false;
        cx.notify();
    }

    pub(crate) fn kv_submit_value_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &browse.inspector else {
            return;
        };
        let key = inspector.key.clone();
        // Preserve the key's existing TTL: a plain `SET` with no `EX`
        // clears any expiry, and editing the value isn't meant to also
        // reset the countdown.
        let ttl = inspector.ttl;
        let value = inspector.value_editor.read(cx).content().to_string();
        let edit = red_core::kv::KvEdit::SetString { key, value, ttl };
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
    }

    pub(crate) fn kv_start_editing_ttl(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
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
        inspector.editing_ttl = true;
        cx.notify();
    }

    pub(crate) fn kv_cancel_editing_ttl(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.editing_ttl = false;
        cx.notify();
    }

    /// Blank input persists the key (no expiry); otherwise parses as whole
    /// seconds. An unparseable, non-blank input is a silent no-op — a real
    /// input validation message is a nice-to-have this pass skips.
    pub(crate) fn kv_submit_ttl_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &browse.inspector else {
            return;
        };
        let key = inspector.key.clone();
        let text = inspector.ttl_editor.read(cx).content().to_string();
        let ttl = if text.trim().is_empty() {
            None
        } else {
            match text.trim().parse::<u64>() {
                Ok(secs) => Some(Duration::from_secs(secs)),
                Err(_) => return,
            }
        };
        let edit = red_core::kv::KvEdit::SetTtl { key, ttl };
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
    }

    pub(crate) fn kv_start_editing_key(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        if browse.epoch != epoch {
            return;
        }
        match edit {
            KvEdit::SetString { key, value, ttl } => {
                if let Some(inspector) = &mut browse.inspector {
                    if inspector.key == key {
                        inspector.value = Some(KvValue::Str(red_core::Value::Text(value)));
                        inspector.editing_value = false;
                    }
                }
                if let Some(row) = browse.rows.iter_mut().find(|r| r.key == key) {
                    row.ttl = ttl;
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
            KvEdit::SetTtl { key, ttl } => {
                if let Some(inspector) = &mut browse.inspector {
                    if inspector.key == key {
                        inspector.ttl = ttl;
                        inspector.editing_ttl = false;
                    }
                }
                if let Some(row) = browse.rows.iter_mut().find(|r| r.key == key) {
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
                if let Some(row) = browse.rows.iter_mut().find(|r| r.key == from) {
                    row.key = to;
                }
            }
            KvEdit::Delete { keys } => {
                if let Some(inspector) = &browse.inspector {
                    if keys.contains(&inspector.key) {
                        browse.inspector = None;
                    }
                }
                browse.rows.retain(|r| !keys.contains(&r.key));
            }
        }
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
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        if inspector.key != key {
            return; // a newer selection has already superseded this reply
        }
        inspector.value = value.clone();
        cx.notify();
        let Some(session) = session else { return };
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
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
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
            .and_then(|a| a.kv_browse.as_ref())
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
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        if inspector.key != key {
            return;
        }
        inspector.collection_rows.extend(page.elements);
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
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        if inspector.key != key {
            return;
        }
        inspector.collection_rows = values.into_iter().map(KvElement::Member).collect();
        // A list's head-window preview is a one-shot fetch, not paged.
        inspector.collection_exhausted = true;
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
        let Some(browse) = &mut active.kv_browse else {
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
            .and_then(|a| a.kv_browse.as_ref())
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
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        if inspector.key != key {
            return;
        }
        inspector.stream_rows.extend(page.entries);
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
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
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
        let Some(browse) = &mut active.kv_browse else {
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
            .and_then(|a| a.kv_browse.as_ref())
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
            .kv_browse
            .as_mut()?
            .inspector
            .as_mut()
    }

    /// Like [`kv_inspector_mut`](Self::kv_inspector_mut) but resolving the
    /// session `Option` an event carries (events are delivered with the
    /// originating `SessionId`, or `None` for the foreground).
    fn kv_inspector_for(&mut self, session: Option<SessionId>) -> Option<&mut KvInspector> {
        self.conn_mut(session)?
            .kv_browse
            .as_mut()?
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

impl AppState {
    /// The keyspace browser's body: filter box + header stat + the
    /// virtualized key list. Called from `render_redis_shell`.
    pub(crate) fn render_kv_browse(
        &self,
        active: &ActiveConn,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let session = active.session;
        let Some(browse) = &active.kv_browse else {
            return div().flex_1();
        };

        let fuzzy_query = browse.filter.read(cx).content().to_string();
        let rows: std::rc::Rc<Vec<KeyMeta>> = if browse.fuzzy && !fuzzy_query.is_empty() {
            let mut scored: Vec<(i32, &KeyMeta)> = browse
                .rows
                .iter()
                .filter_map(|r| fuzzy_score(&fuzzy_query, &r.key).map(|s| (s, r)))
                .collect();
            scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            std::rc::Rc::new(scored.into_iter().map(|(_, r)| r.clone()).collect())
        } else {
            std::rc::Rc::new(browse.rows.clone())
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
            match (&browse.pattern, browse.db_size) {
                (None, Some(n)) => {
                    format!("~{} keys in db0", crate::result::group_digits(n as usize))
                }
                (None, None) => "counting keys…".into(),
                (Some(p), _) => format!("{} matched \"{p}\" so far", browse.rows.len()),
            }
        };

        let rows_render = rows.clone();
        let rows_select = rows.clone();
        let row_count = rows.len();
        let visible_range_view = view.clone();
        let select_view = view.clone();

        let selected_ix = browse
            .inspector
            .as_ref()
            .and_then(|i| rows.iter().position(|r| r.key == i.key));

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

        let table = Table::<()>::new("kv-browse", columns)
            .row_count(row_count)
            .grid_lines(true)
            .text_size(cell_size)
            .track_scroll(&browse.scroll)
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

        let main = match &browse.big_keys {
            None => div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .child(div().flex_1().min_w(px(0.)).child(table))
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
                    .child(div().flex_1().child(browse.filter.clone()))
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
                    .child(
                        div()
                            .text_size(theme.scale(11.))
                            .text_color(theme.text_muted)
                            .child(header),
                    )
                    .child(big_keys_button),
            )
            .child(main)
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
        let rows = std::rc::Rc::new(bk.results.clone());
        let rows_render = rows.clone();
        let rows_select = rows.clone();
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
                        Button::new("kv-big-keys-close", "Back to live browse")
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

        // --- key name, with an inline rename affordance ---
        let key_row = if inspector.editing_key {
            let (save_view, cancel_view) = (view.clone(), view.clone());
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
                        .child(inspector.rename_editor.clone()),
                )
                .child(
                    IconButton::new(
                        "kv-rename-save",
                        crate::icons::icon("check", theme.scale(13.), theme.green),
                    )
                    .size(IconButtonSize::Sm)
                    .tooltip("Rename")
                    .a11y_label("Save rename")
                    .on_click(move |_, _, cx| {
                        save_view
                            .update(cx, |this, cx| this.kv_submit_rename(session, cx))
                            .ok();
                    }),
                )
                .child(
                    IconButton::new(
                        "kv-rename-cancel",
                        crate::icons::icon("x", theme.scale(13.), theme.text_muted),
                    )
                    .size(IconButtonSize::Sm)
                    .tooltip("Cancel")
                    .a11y_label("Cancel rename")
                    .on_click(move |_, _, cx| {
                        cancel_view
                            .update(cx, |this, cx| this.kv_cancel_editing_key(session, cx))
                            .ok();
                    }),
                )
                .into_any_element()
        } else {
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
                        .on_click(move |_, _, cx| {
                            rename_view
                                .update(cx, |this, cx| this.kv_start_editing_key(session, cx))
                                .ok();
                        }),
                    )
                })
                .into_any_element()
        };

        // --- TTL, with an inline edit affordance ---
        let ttl_row = if inspector.editing_ttl {
            let (save_view, cancel_view) = (view.clone(), view.clone());
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(div().w(px(110.)).child(inspector.ttl_editor.clone()))
                .child(
                    IconButton::new(
                        "kv-ttl-save",
                        crate::icons::icon("check", theme.scale(13.), theme.green),
                    )
                    .size(IconButtonSize::Sm)
                    .tooltip("Set TTL (blank = no expiry)")
                    .a11y_label("Save TTL")
                    .on_click(move |_, _, cx| {
                        save_view
                            .update(cx, |this, cx| this.kv_submit_ttl_edit(session, cx))
                            .ok();
                    }),
                )
                .child(
                    IconButton::new(
                        "kv-ttl-cancel",
                        crate::icons::icon("x", theme.scale(13.), theme.text_muted),
                    )
                    .size(IconButtonSize::Sm)
                    .tooltip("Cancel")
                    .a11y_label("Cancel TTL edit")
                    .on_click(move |_, _, cx| {
                        cancel_view
                            .update(cx, |this, cx| this.kv_cancel_editing_ttl(session, cx))
                            .ok();
                    }),
                )
                .into_any_element()
        } else {
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
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            cancel_view
                                .update(cx, |this, cx| this.kv_cancel_delete(session, cx))
                                .ok();
                        }),
                )
        });

        let body = self.render_kv_value(session, inspector, writable, theme, cx);

        div()
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

        let Some(value) = &inspector.value else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(text_size)
                .text_color(dim)
                .child("Loading…")
                .into_any_element();
        };

        match value {
            KvValue::Str(v) if inspector.editing_value => {
                let (save_view, cancel_view) = (view.clone(), view.clone());
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("kv-inspector-string-edit")
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .p_2()
                            .font_family(mono)
                            .text_size(text_size)
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
            KvValue::Str(v) => div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .child(
                    div()
                        .id("kv-inspector-string")
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .p_2()
                        .child(
                            div()
                                .font_family(mono)
                                .text_size(text_size)
                                .child(render_string_preview(v)),
                        ),
                )
                .when(writable, |d| {
                    let edit_view = view.clone();
                    d.child(
                        div()
                            .flex_shrink_0()
                            .px_2()
                            .py_1p5()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(
                                Button::new("kv-string-edit", "Edit")
                                    .size(ButtonSize::Sm)
                                    .on_click(move |_, _, cx| {
                                        edit_view
                                            .update(cx, |this, cx| {
                                                this.kv_start_editing_value(session, cx)
                                            })
                                            .ok();
                                    }),
                            ),
                    )
                })
                .into_any_element(),
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
                render_loaded_list(pairs.iter().map(|(f, v)| (f.clone(), v.clone())), theme)
            }
            KvValue::Set(KvCollection::Loaded(members)) => render_loaded_list(
                members
                    .iter()
                    .enumerate()
                    .map(|(i, m)| (i.to_string(), m.clone())),
                theme,
            ),
            KvValue::ZSet(KvCollection::Loaded(pairs)) => {
                render_loaded_list(pairs.iter().map(|(m, s)| (m.clone(), s.to_string())), theme)
            }
            KvValue::List(KvCollection::Loaded(items)) => render_loaded_list(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (i.to_string(), v.clone())),
                theme,
            ),
            KvValue::Hash(KvCollection::Large { len }) => self.render_kv_collection_grid(
                session,
                CollectionKind::Hash,
                *len,
                inspector,
                theme,
                cx,
            ),
            KvValue::Set(KvCollection::Large { len }) => self.render_kv_collection_grid(
                session,
                CollectionKind::Set,
                *len,
                inspector,
                theme,
                cx,
            ),
            KvValue::ZSet(KvCollection::Large { len }) => self.render_kv_collection_grid(
                session,
                CollectionKind::ZSet,
                *len,
                inspector,
                theme,
                cx,
            ),
            KvValue::List(KvCollection::Large { len }) => {
                self.render_kv_list_preview(*len, inspector, theme)
            }
        }
    }

    /// The big hash/set/zset sub-grid: same `Table` + `on_visible_range`
    /// load-more shape as the top-level keyspace browser, scoped to one
    /// key's elements.
    fn render_kv_collection_grid(
        &self,
        session: SessionId,
        kind: CollectionKind,
        len: u64,
        inspector: &KvInspector,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let view = cx.entity().downgrade();
        let rows = std::rc::Rc::new(inspector.collection_rows.clone());
        let rows_render = rows.clone();
        let row_count = rows.len();

        let columns = match kind {
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

        let dim = theme.text_muted;
        let cell_size = theme.scale(11.5);

        let table = Table::<()>::new("kv-inspector-grid", columns)
            .row_count(row_count)
            .grid_lines(true)
            .text_size(cell_size)
            .track_scroll(&inspector.collection_scroll)
            .render_row(move |ix, _window, _cx| match rows_render.get(ix) {
                Some(KvElement::Field(f, v)) => vec![
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
                Some(KvElement::Scored(m, s)) => vec![
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
                Some(KvElement::Member(m)) => {
                    vec![div()
                        .min_w_0()
                        .truncate()
                        .child(m.clone())
                        .into_any_element()]
                }
                None => Vec::new(),
            })
            .on_visible_range(move |range, _window, cx| {
                view.update(cx, |this, cx| {
                    this.kv_inspector_maybe_load_more(session, kind, range.end, cx)
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
                    .child(format!("{len} elements, paging as you scroll")),
            )
            .child(div().flex_1().min_h(px(0.)).child(table))
            .into_any_element()
    }

    /// A big list's static head-window preview (no infinite scroll; see
    /// `LIST_PREVIEW_COUNT`'s doc comment).
    fn render_kv_list_preview(
        &self,
        len: u64,
        inspector: &KvInspector,
        theme: &Theme,
    ) -> gpui::AnyElement {
        let shown = inspector.collection_rows.len();
        let note = format!("showing the first {shown} of {len} items (head only)");
        let items = inspector.collection_rows.iter().enumerate().map(|(i, el)| {
            let KvElement::Member(v) = el else {
                return div().into_any_element();
            };
            div()
                .flex()
                .gap_2()
                .px_2()
                .py_0p5()
                .child(
                    div()
                        .w(px(36.))
                        .text_color(theme.text_faint)
                        .child(i.to_string()),
                )
                .child(div().flex_1().min_w_0().truncate().child(v.clone()))
                .into_any_element()
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
                    .text_color(theme.text_muted)
                    .child(note),
            )
            .child(
                div()
                    .id("kv-inspector-list-preview")
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
        let rows = std::rc::Rc::new(inspector.stream_rows.clone());
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
/// `ID → fields` rows, newest-first — the stream counterpart of
/// [`render_loaded_list`], capped by `SMALL_COLLECTION_THRESHOLD` so it needs
/// no virtualization.
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

/// A small (< threshold) collection rendered as a plain scrollable list, not
/// the virtualized `Table` the big-collection path uses — capped at a few
/// hundred rows by construction (`SMALL_COLLECTION_THRESHOLD` on the driver
/// side), so no virtualization is needed.
fn render_loaded_list(
    pairs: impl Iterator<Item = (String, String)>,
    theme: &Theme,
) -> gpui::AnyElement {
    let dim = theme.text_muted;
    let items: Vec<_> = pairs
        .map(|(k, v)| {
            div()
                .flex()
                .gap_2()
                .px_2()
                .py_0p5()
                .child(
                    div()
                        .w(px(90.))
                        .min_w_0()
                        .truncate()
                        .text_color(dim)
                        .child(k),
                )
                .child(div().flex_1().min_w_0().truncate().child(v))
                .into_any_element()
        })
        .collect();
    div()
        .id("kv-inspector-loaded-list")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .text_size(theme.scale(11.5))
        .children(items)
        .into_any_element()
}

/// A string value's preview body: pretty-printed if it parses as JSON
/// (a common Redis string payload shape), else the raw text; a capped value
/// shows its prefix plus a "… (N bytes total)" note.
fn render_string_preview(value: &red_core::Value) -> String {
    match value {
        red_core::Value::Text(s) => s.clone(),
        red_core::Value::Capped(cell) => {
            format!("{}\n\n… ({} bytes total, truncated)", cell.head, cell.len)
        }
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
