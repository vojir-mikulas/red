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
    div, prelude::*, px, AsyncApp, Context, Entity, UniformListScrollHandle, WeakEntity, Window,
};
use red_core::kv::{
    CollectionKind, KeyMeta, KvCollection, KvCollectionPage, KvElement, KvType, KvValue, ScanBudget,
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
    pub(crate) cursor: u64,
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
    pub(crate) cursor: u64,
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

impl RedisBrowse {
    pub(crate) fn new(session: SessionId, cx: &mut Context<AppState>) -> Self {
        let filter = cx.new(|cx| TextInput::new(cx).with_placeholder("Filter (MATCH pattern)…"));
        cx.subscribe(&filter, move |this, input, event: &TextInputEvent, cx| {
            match event {
                // Enter restarts immediately, bypassing the debounce wait.
                TextInputEvent::Submit => {
                    let pattern = input.read(cx).content().to_string();
                    this.kv_restart_scan(session, non_empty(pattern), cx);
                }
                TextInputEvent::Change => {
                    let pattern = input.read(cx).content().to_string();
                    this.kv_debounce_filter(session, pattern, cx);
                }
                _ => {}
            }
        })
        .detach();
        Self {
            epoch: crate::result::next_kv_epoch(),
            pattern: None,
            rows: Vec::new(),
            cursor: 0,
            exhausted: false,
            loading: false,
            db_size: None,
            scroll: UniformListScrollHandle::new(),
            filter,
            filter_gen: 0,
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
                cursor: 0,
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
        browse.cursor = 0;
        browse.exhausted = false;
        browse.loading = true;
        self.service
            .send_to(session, Command::CloseResult { epoch: old_epoch });
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch: new_epoch,
                pattern,
                cursor: 0,
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
            cursor: 0,
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
                cursor: 0,
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
    pub(crate) fn kv_open_inspector(
        &mut self,
        session: SessionId,
        ix: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = &mut active.kv_browse else {
            return;
        };
        let Some(row) = browse.rows.get(ix) else {
            return;
        };
        let key = row.key.clone();
        let ttl = row.ttl;
        let kv_type = row.kv_type.clone();
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

        let header = match (&browse.pattern, browse.db_size) {
            (None, Some(n)) => format!("~{} keys in db0", crate::result::group_digits(n as usize)),
            (None, None) => "counting keys…".into(),
            (Some(p), _) => format!("{} matched \"{p}\" so far", browse.rows.len()),
        };

        let rows = std::rc::Rc::new(browse.rows.clone());
        let rows_render = rows.clone();
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
                select_view
                    .update(cx, |this, cx| this.kv_open_inspector(session, ix, cx))
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
                .render_big_keys(session, bk, &theme, cx)
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
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let rows = std::rc::Rc::new(bk.results.clone());
        let rows_render = rows.clone();
        let row_count = rows.len();

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
                                view.update(cx, |this, cx| this.kv_close_big_keys(session, cx))
                                    .ok();
                            }),
                    ),
            )
            .child(div().flex_1().min_h(px(0.)).child(table))
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
