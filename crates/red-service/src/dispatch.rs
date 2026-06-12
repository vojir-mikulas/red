//! The dispatch loop: the backend thread's command pump. Owns the active
//! session and cursor, the open-result map, and the page-fetch concurrency
//! limit; runs queries through a windowed cursor and races each fetch against
//! incoming commands so a cancel or timeout can abort one in flight.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;
use red_core::{ConnectionConfig, DbKind, KeyKind, KeySpec, RedError, Value};
use red_driver::{
    AbortSignal, CancelToken, DatabaseDriver, MysqlDriver, PageCap, PostgresDriver, QueryCursor,
    SqliteDriver,
};
use tokio::sync::mpsc::UnboundedReceiver as CmdReceiver;
use tokio::sync::Semaphore;

use crate::{Command, Envelope, Event, RunFetch, SessionId};

/// The event sender carries each event tagged with the session it belongs to
/// (`None` for the session-less probe replies).
type Events = UnboundedSender<(Option<SessionId>, Event)>;

/// Cap on page fetches running at once. The grid can request a burst of pages
/// (several tabs, or a viewport spanning page boundaries); without a cap a flung
/// scrollbar could otherwise fan out dozens of simultaneous deep-`OFFSET` scans
/// and saturate the server. The UI also throttles requests (see `FLING_ROWS`);
/// this is the backstop.
const MAX_CONCURRENT_PAGE_FETCHES: usize = 6;

/// Cap on how long one connect attempt may run before the backend gives up and
/// reports a timeout. Bounds a hung connect (a black-hole host) so the dispatch
/// loop frees up for the next command — the UI drives retry/backoff and cancel
/// on top of this, but those only work if the loop isn't wedged awaiting a dial.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Rows between checkpoints in the index (see [`CheckpointIndex`]). Serving an
/// exact jump seeks to the nearest checkpoint then skips at most this many rows,
/// so it stays O(stride) regardless of depth.
const CHECKPOINT_STRIDE: usize = 10_000;

/// An exact "go to row N" deeper than this kicks off a checkpoint-index build.
/// Shallower jumps are served by a plain `OFFSET` (already fast), so tables
/// nobody jumps deep into are never scanned.
const BUILD_TRIGGER_DEPTH: usize = 100_000;

/// How long a non-foreground session may sit untouched before it's evicted: its
/// driver is dropped (connection released) and any in-flight work aborted. The
/// foreground session (per `SetActiveSession`) is exempt — it must stay warm
/// however long the user studies a result without scrolling.
const IDLE_EVICT: Duration = Duration::from_secs(600);

/// How often the dispatch loop wakes (absent any command) to sweep idle sessions.
const EVICT_SWEEP: Duration = Duration::from_secs(30);

/// A sparse `ordinal → key` index over an open keyset result: one entry every
/// [`CHECKPOINT_STRIDE`] rows, built by a single background ordered traversal.
/// Lets an exact jump to row N seek to the nearest checkpoint and skip `< stride`
/// rows — O(stride), not O(N). Shared via `Arc<Mutex<…>>` so the build task fills
/// it incrementally while fetches read it.
#[derive(Debug, Default)]
struct CheckpointIndex {
    /// `(ordinal, key tuple)` pairs, ascending by ordinal. `points[0]` is
    /// `(0, first_key)`. The key is the full seek tuple (lead, then tiebreaker).
    points: Vec<(usize, Vec<Value>)>,
    status: BuildStatus,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
enum BuildStatus {
    /// Not yet built (or invalidated by a write) — eligible to start.
    #[default]
    Idle,
    /// A background build is in flight; don't start another.
    Building,
    /// Fully built to the end of the result.
    Done,
}

/// What the backend remembers about one open result: the SQL it re-fetches per
/// page/run, the resolved seek key (+ its column index, for the checkpoint
/// build), the key's min/max and total (for interpolated jumps), and the
/// checkpoint index for exact jumps.
#[derive(Debug, Clone)]
struct OpenSpec {
    sql: String,
    key: Option<KeySpec>,
    /// Positions of the key columns within a result row (lead, then tiebreaker) —
    /// the checkpoint build reads each checkpoint's key tuple out of the row at
    /// these indices. Empty when the result isn't keyset-keyed.
    key_cols: Vec<usize>,
    bounds: Option<(i64, i64)>,
    total: Option<usize>,
    checkpoints: Arc<Mutex<CheckpointIndex>>,
}

/// The open-result map, shared with the spawned open/fetch tasks (they fill in
/// bounds/total after the fact and read specs without round-tripping commands).
type ResultMap = Arc<Mutex<HashMap<u64, OpenSpec>>>;

/// The cancellable work in flight for one open result. Each detached fetch carries
/// an [`AbortSignal`]; when a newer one supersedes it (a flung scrollbar, a new
/// page, a closed tab) the old signal is [`abort`](AbortSignal::abort)ed so the
/// engine stops the doomed query instead of running it to completion. Held only on
/// the dispatch loop (single-threaded), so no extra lock — the spawned task keeps
/// its own clone and the driver disarms it on completion, making a late abort a
/// no-op.
#[derive(Default)]
struct InFlight {
    /// The `OpenResult` probe bundle (`count` + `fetch_page` + `key_bounds`).
    open: Option<AbortSignal>,
    /// The latest offset `FetchPage` for this epoch.
    page: Option<AbortSignal>,
    /// The latest `FetchRun`, tagged with its `seq` so a stale (lower-seq) run
    /// arriving late doesn't cancel a newer one.
    run: Option<(u64, AbortSignal)>,
    /// The background checkpoint-index build, if one is running.
    build: Option<AbortSignal>,
}

impl InFlight {
    /// Supersede every in-flight fetch for this result (tab closed / reconnected).
    fn abort_all(&self) {
        for sig in [self.open.as_ref(), self.page.as_ref(), self.build.as_ref()]
            .into_iter()
            .flatten()
        {
            sig.abort();
        }
        if let Some((_, sig)) = &self.run {
            sig.abort();
        }
    }
}

/// Lock a mutex, tolerating poison. A detached page-fetch task can panic while
/// holding `results`; recovering the guard keeps one bad task from bricking the
/// whole backend. The worst case is a half-written entry, which dispatch already
/// tolerates — a fetch for an epoch absent or stale in the map is dropped.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The active query's cursor plus the bits needed to drive and abort it.
struct ActiveQuery {
    cursor: Box<dyn QueryCursor>,
    cancel: CancelToken,
    timeout: Option<Duration>,
    streamed: usize,
    started: Instant,
}

/// Everything the backend owns for one keep-alive session: its driver, the
/// streaming cursor (the legacy `Query` path), the open-result map, the per-epoch
/// abort handles, the in-flight export flags, and when it was last touched (for
/// idle eviction). Several of these stay warm at once, keyed by [`SessionId`], so
/// switching between connections is instant — no reconnect, no schema reload.
struct SessionState {
    driver: Arc<dyn DatabaseDriver>,
    /// The streaming `Query`/`FetchMore` cursor. Single-active per session; this
    /// path is legacy/test-only (the UI drives results via `OpenResult`).
    active: Option<ActiveQuery>,
    results: ResultMap,
    inflight: HashMap<u64, InFlight>,
    exports: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    /// Bumped on every command routed here; idle eviction reads it.
    last_used: Instant,
}

impl SessionState {
    fn new(driver: Arc<dyn DatabaseDriver>) -> Self {
        Self {
            driver,
            active: None,
            results: Arc::new(Mutex::new(HashMap::new())),
            inflight: HashMap::new(),
            exports: Arc::new(Mutex::new(HashMap::new())),
            last_used: Instant::now(),
        }
    }

    /// Stop everything in flight at the engine and forget every open result —
    /// the session is being dropped (disconnect / close / eviction).
    fn teardown(&mut self) {
        abort_all_inflight(&mut self.inflight);
        lock(&self.results).clear();
    }
}

pub(crate) async fn dispatch(mut commands: CmdReceiver<Envelope>, events: Events) {
    // The warm sessions, keyed by `SessionId`. Several stay live at once so the UI
    // can switch between connections instantly (no reconnect); each owns its
    // driver, cursor, open-result map, in-flight handles, and exports. `Connect`
    // inserts, `Disconnect`/`CloseSession`/eviction remove. Per-epoch fetch state
    // lives inside each session — UI epochs start at 1, so an empty result map
    // means "no live result" for that session.
    let mut sessions: HashMap<SessionId, SessionState> = HashMap::new();
    // Which session the UI currently shows (`SetActiveSession`). Exempt from idle
    // eviction so an on-screen-but-unscrolled result stays warm.
    let mut foreground: Option<SessionId> = None;
    // The statement timeout (`query.statement_timeout`) applied to every open
    // probe and page/run fetch. `None` = no cap. Global, set by the UI at launch
    // and on each settings reload, captured into each spawned fetch task.
    let mut statement_timeout: Option<Duration> = None;
    // Bounds how many page fetches hit servers concurrently across *all* sessions
    // (see the const) — a shared backstop, so a flung scrollbar on one connection
    // can't fan out dozens of deep scans. A busy session can briefly delay
    // another's page fetches; acceptable for a backstop.
    let page_fetch_limit = Arc::new(Semaphore::new(MAX_CONCURRENT_PAGE_FETCHES));
    // Wakes the loop even when no command arrives, so idle sessions get swept.
    let mut sweep = tokio::time::interval(EVICT_SWEEP);

    loop {
        let (session_id, command) = tokio::select! {
            maybe = commands.recv() => match maybe {
                Some(envelope) => envelope,
                None => break, // UI dropped the sender — window closed
            },
            _ = sweep.tick() => {
                evict_idle(&mut sessions, foreground, &events);
                continue;
            }
        };
        // Any command routed to a session counts as activity, deferring eviction.
        if let Some(id) = session_id {
            if let Some(s) = sessions.get_mut(&id) {
                s.last_used = Instant::now();
            }
        }
        match command {
            Command::Connect(config) => {
                let Some(id) = session_id else { continue };
                // A re-connect on the same id (a retry, or replacing a dropped
                // session) tears down whatever was there first.
                if let Some(mut old) = sessions.remove(&id) {
                    old.teardown();
                }
                match attempt_connect(&config).await {
                    Ok(driver) => {
                        let version = driver.server_version();
                        sessions.insert(id, SessionState::new(driver));
                        emit(&events, Some(id), Event::Connected { version });
                    }
                    Err(message) => emit(&events, Some(id), Event::Error(message)),
                }
            }

            Command::SetActiveSession(id) => foreground = id,

            Command::SetStatementTimeout(timeout) => statement_timeout = timeout,

            Command::SetDisplayCellCap(bytes) => red_driver::set_display_cell_cap(bytes),

            Command::TestConnection(config) => {
                // A throwaway probe: connect, report, and let the driver drop. No
                // session is created or disturbed — it's session-less (`None`).
                match attempt_connect(&config).await {
                    Ok(driver) => emit(
                        &events,
                        None,
                        Event::TestSucceeded {
                            version: driver.server_version(),
                        },
                    ),
                    Err(message) => emit(&events, None, Event::TestFailed { message }),
                }
            }

            Command::Disconnect | Command::CloseSession => {
                let Some(id) = session_id else { continue };
                if let Some(mut state) = sessions.remove(&id) {
                    state.teardown();
                }
                if foreground == session_id {
                    foreground = None;
                }
                emit(&events, session_id, Event::Disconnected);
            }

            Command::Query { sql, opts } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                state.active = None; // a new query supersedes the previous cursor
                let driver = state.driver.clone();
                match driver.open_cursor(&sql, opts.clone()).await {
                    Ok(cursor) => {
                        let aq = ActiveQuery {
                            cancel: cursor.cancel_token(),
                            timeout: opts.timeout,
                            streamed: 0,
                            started: Instant::now(),
                            cursor,
                        };
                        emit(
                            &events,
                            session_id,
                            Event::QueryStarted {
                                columns: aq.cursor.columns().to_vec(),
                            },
                        );
                        // Re-borrow the session's cursor slot (it can't vanish
                        // mid-await on this single-threaded loop).
                        if let Some(active) = sessions.get_mut(&id).map(|s| &mut s.active) {
                            if drive_fetch(aq, opts.window, id, &mut commands, &events, active)
                                .await
                            {
                                break; // shutdown requested mid-fetch
                            }
                        }
                    }
                    Err(err) => emit(&events, session_id, Event::Error(err.to_string())),
                }
            }

            Command::FetchMore { max } => {
                let Some(id) = session_id else { continue };
                let aq = sessions.get_mut(&id).and_then(|s| s.active.take());
                match aq {
                    Some(aq) => {
                        if let Some(active) = sessions.get_mut(&id).map(|s| &mut s.active) {
                            if drive_fetch(aq, max, id, &mut commands, &events, active).await {
                                break;
                            }
                        }
                    }
                    None => emit(&events, session_id, Event::Error("no active query".into())),
                }
            }

            Command::LoadObjects => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let driver = state.driver.clone();
                match driver.list_objects().await {
                    Ok(schemas) => emit(&events, session_id, Event::ObjectsLoaded { schemas }),
                    Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                }
            }

            Command::DescribeTable { schema, table } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let driver = state.driver.clone();
                match driver.describe_table(&schema, &table).await {
                    Ok(detail) => emit(
                        &events,
                        session_id,
                        Event::TableDescribed {
                            schema,
                            table,
                            detail,
                        },
                    ),
                    Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                }
            }

            Command::OpenResult {
                sql,
                epoch,
                table,
                sort,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let driver = state.driver.clone();
                // A re-open on the same epoch supersedes any prior probe.
                if let Some(f) = state.inflight.remove(&epoch) {
                    f.abort_all();
                }
                // Registered before the (slow) open task so an early fetch for
                // this epoch isn't mistaken for a stale one.
                lock(&state.results).insert(
                    epoch,
                    OpenSpec {
                        sql: sql.clone(),
                        key: None,
                        key_cols: Vec::new(),
                        bounds: None,
                        total: None,
                        checkpoints: Arc::new(Mutex::new(CheckpointIndex::default())),
                    },
                );
                // One abort handle for the whole probe bundle: re-sort / close
                // cancels the (potentially full-table) `count` and column probe.
                let abort = AbortSignal::new();
                state.inflight.entry(epoch).or_default().open = Some(abort.clone());
                // Count + column metadata can be slow (a full `COUNT(*)` over a
                // large table); run them off the dispatch loop so switching to
                // another table stays instant.
                let events = events.clone();
                let results = state.results.clone();
                let timeout = statement_timeout;
                tokio::spawn(async move {
                    // A table browse resolves its seek key from the table's
                    // introspected detail: a sorted browse gets the composite
                    // `(sort_col, pk)` key, an unsorted one the plain PK. A
                    // resolution failure just means the `OFFSET` fallback (never
                    // an error).
                    let key = match &table {
                        Some((schema, table)) => match driver.describe_table(schema, table).await {
                            Ok(detail) => {
                                let key = match &sort {
                                    Some(s) => KeySpec::sorted(&detail, &s.column, s.descending),
                                    None => KeySpec::from_detail(&detail),
                                };
                                match &key {
                                    Some(k) => tracing::info!(
                                        %schema, %table, column = %k.column,
                                        tiebreak = ?k.tiebreak, descending = k.descending,
                                        "keyset key resolved"
                                    ),
                                    None => tracing::info!(
                                        %schema, %table,
                                        "no usable key (composite/nullable/no PK) — OFFSET paging"
                                    ),
                                }
                                key
                            }
                            Err(e) => {
                                tracing::warn!(%schema, %table, "keyset describe failed: {e}");
                                None
                            }
                        },
                        None => None,
                    };
                    // The SQL later page/run fetches re-run. Keyset orders itself
                    // (driver adds `ORDER BY (sort_col, pk)`), so it pages the
                    // *base* query; a sorted result that fell back to OFFSET must
                    // still be ordered, so wrap it by output position.
                    let effective_sql = match (&sort, &key) {
                        (Some(s), None) => wrap_sorted(&sql, s.position, s.descending),
                        _ => sql.clone(),
                    };
                    // `LIMIT 0` reads column metadata without stepping rows;
                    // counting and the key-bounds probe run concurrently with it.
                    // Probes run on the base `sql` — ordering doesn't change the
                    // count, the column set, or the lead column's min/max.
                    let bounds = async {
                        match &key {
                            Some(k) if k.kind == KeyKind::Int => {
                                driver.key_bounds(&sql, k, &abort).await.ok().flatten()
                            }
                            _ => None,
                        }
                    };
                    // Race the (potentially full-table `COUNT(*)`) probe against the
                    // statement timeout: on expiry, abort the bundle at the engine
                    // and report a timeout instead of leaving the result "running".
                    let probe = async {
                        tokio::join!(
                            driver.count(&sql, &abort),
                            driver.fetch_page(&sql, 0, 0, PageCap::Full, &abort),
                            bounds
                        )
                    };
                    let (total, columns, bounds) = tokio::select! {
                        out = probe => out,
                        _ = sleep_for(timeout), if timeout.is_some() => {
                            abort.abort();
                            emit(&events, session_id, Event::Error(RedError::Timeout.to_string()));
                            return;
                        }
                    };
                    match (total, columns) {
                        (Ok(total), Ok(page)) => {
                            let total = total.max(0) as usize;
                            // Fill the spec in only if the result is still open.
                            // `key_cols` locate the key columns within a row so the
                            // checkpoint build can read each checkpoint's key tuple.
                            let key_cols = key
                                .as_ref()
                                .map(|k| {
                                    k.column_names()
                                        .iter()
                                        .filter_map(|name| {
                                            page.columns.iter().position(|c| &c.name == name)
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            if let Some(spec) = lock(&results).get_mut(&epoch) {
                                spec.sql = effective_sql;
                                spec.key = key.clone();
                                spec.key_cols = key_cols;
                                spec.bounds = bounds;
                                spec.total = Some(total);
                            }
                            emit(
                                &events,
                                session_id,
                                Event::ResultReady {
                                    columns: page.columns,
                                    total,
                                    epoch,
                                    key,
                                },
                            );
                        }
                        (Err(e), _) | (_, Err(e)) => {
                            emit(&events, session_id, Event::Error(e.to_string()))
                        }
                    }
                });
            }

            Command::FetchPage {
                offset,
                limit,
                epoch,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let driver = state.driver.clone();
                // The tab closed or re-sorted (its epoch is gone); skip the stale
                // request rather than running an expensive query whose result
                // would be discarded.
                let Some(sql) = lock(&state.results).get(&epoch).map(|s| s.sql.clone()) else {
                    continue;
                };
                // A newer page for this epoch supersedes the last one (the viewport
                // moved); cancel its in-flight fetch so a flung scrollbar doesn't
                // back a pile of doomed deep-`OFFSET` scans up behind the semaphore.
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.page.take() {
                    prev.abort();
                }
                let abort = AbortSignal::new();
                entry.page = Some(abort.clone());
                // Pages fetch concurrently (the driver pools connections) and off
                // the dispatch loop, so a deep-`OFFSET` page never blocks the next
                // command or another page — but no more than `page_fetch_limit` at
                // once, so a burst can't saturate the server.
                let events = events.clone();
                let limit_src = page_fetch_limit.clone();
                let timeout = statement_timeout;
                tokio::spawn(async move {
                    let _permit = limit_src.acquire_owned().await;
                    // Offset-mode display page — cap fat cells; no seek key to exempt.
                    let fetch = driver.fetch_page(
                        &sql,
                        offset,
                        limit,
                        PageCap::Display { key: None },
                        &abort,
                    );
                    match with_timeout(timeout, &abort, fetch).await {
                        Ok(page) => emit(
                            &events,
                            session_id,
                            Event::ResultPageLoaded {
                                offset,
                                rows: page.rows,
                                epoch,
                            },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::FetchRun {
                epoch,
                fetch,
                limit,
                seq,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let driver = state.driver.clone();
                // Stale epoch (tab closed / re-sorted) — drop, like `FetchPage`.
                let Some(spec) = lock(&state.results).get(&epoch).cloned() else {
                    continue;
                };
                let Some(key) = spec.key.clone() else {
                    continue; // a keyless result never gets `FetchRun`s
                };
                // A newer run (higher `seq`) supersedes the last one — a scrollbar
                // fling emits a stream of runs and only the latest matters. Cancel
                // the previous in-flight run so its seek stops at the engine. `seq`
                // is monotonic over the ordered command stream, so the guard against
                // a lower-seq arrival is belt-and-suspenders.
                let entry = state.inflight.entry(epoch).or_default();
                match entry.run.take() {
                    Some((prev_seq, prev)) if prev_seq >= seq => {
                        entry.run = Some((prev_seq, prev));
                        continue;
                    }
                    Some((_, prev)) => prev.abort(),
                    None => {}
                }
                let abort = AbortSignal::new();
                entry.run = Some((seq, abort.clone()));
                // A deep exact jump kicks off the checkpoint index (once) so the
                // *next* deep jump is O(stride). This one still serves via OFFSET.
                if let RunFetch::Jump {
                    ordinal,
                    exact: true,
                } = &fetch
                {
                    if claim_build(&spec, *ordinal) {
                        let build_abort = AbortSignal::new();
                        state.inflight.entry(epoch).or_default().build = Some(build_abort.clone());
                        tokio::spawn(build_checkpoints(
                            driver.clone(),
                            spec.clone(),
                            state.results.clone(),
                            epoch,
                            build_abort,
                        ));
                    }
                }
                let events = events.clone();
                let limit_src = page_fetch_limit.clone();
                let timeout = statement_timeout;
                tokio::spawn(async move {
                    let _permit = limit_src.acquire_owned().await;
                    let run = run_fetch(&*driver, &spec, &key, &fetch, limit, &abort);
                    match with_timeout(timeout, &abort, run).await {
                        Ok((rows, estimated)) => emit(
                            &events,
                            session_id,
                            Event::ResultRunLoaded {
                                epoch,
                                fetch,
                                rows,
                                estimated,
                                seq,
                            },
                        ),
                        Err(e) => {
                            tracing::warn!(%epoch, ?fetch, "run fetch failed: {e}");
                            emit(&events, session_id, Event::ResultRunFailed { epoch, seq });
                            emit(&events, session_id, Event::Error(e.to_string()));
                        }
                    }
                });
            }

            Command::CopyRows {
                offset,
                limit,
                epoch,
                id,
            } => {
                let Some(sid) = session_id else { continue };
                let Some(state) = sessions.get(&sid) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let driver = state.driver.clone();
                // Stale epoch (tab closed / re-sorted) — drop, like `FetchPage`.
                let Some(sql) = lock(&state.results).get(&epoch).map(|s| s.sql.clone()) else {
                    continue;
                };
                // Same windowed read as a page fetch, but `Full` so the rows carry the
                // real values (the grid's display cap is bypassed) for the clipboard.
                let events = events.clone();
                let limit_src = page_fetch_limit.clone();
                tokio::spawn(async move {
                    let _permit = limit_src.acquire_owned().await;
                    // A deliberate clipboard re-fetch isn't superseded by scrolling,
                    // so it carries a throwaway signal that never aborts.
                    let abort = AbortSignal::new();
                    match driver
                        .fetch_page(&sql, offset, limit, PageCap::Full, &abort)
                        .await
                    {
                        Ok(page) => emit(
                            &events,
                            session_id,
                            Event::CopyRowsLoaded {
                                id,
                                rows: page.rows,
                            },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::CloseResult { epoch } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    continue;
                };
                // Stop every in-flight fetch for the tab at the engine, then forget it.
                if let Some(f) = state.inflight.remove(&epoch) {
                    f.abort_all();
                }
                lock(&state.results).remove(&epoch);
            }

            Command::Execute { sql } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let driver = state.driver.clone();
                let results = state.results.clone();
                match driver.execute(&sql).await {
                    Ok(affected) => {
                        // A write may have shifted rows under any open result, so
                        // drop the checkpoint indexes — they rebuild lazily on the
                        // next deep jump rather than serving from stale keys.
                        for spec in lock(&results).values() {
                            let mut idx = lock(&spec.checkpoints);
                            idx.points.clear();
                            idx.status = BuildStatus::Idle;
                        }
                        emit(
                            &events,
                            session_id,
                            Event::Executed {
                                affected: affected as usize,
                            },
                        );
                    }
                    Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                }
            }

            Command::Export {
                format,
                path,
                epoch,
                id,
            } => {
                let Some(sid) = session_id else { continue };
                let Some(state) = sessions.get(&sid) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let driver = state.driver.clone();
                let Some(sql) = lock(&state.results).get(&epoch).map(|s| s.sql.clone()) else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("no open result to export".into()),
                    );
                    continue;
                };
                // Register the cancel flag before the task starts, so a fast
                // `CancelExport` can't race ahead of it.
                let cancel = Arc::new(AtomicBool::new(false));
                lock(&state.exports).insert(id, cancel.clone());

                // Forward the driver's throttled row counts to the UI as progress
                // events; the channel closes (loop ends) when the export drops its
                // sender on completion.
                let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
                {
                    let events = events.clone();
                    tokio::spawn(async move {
                        while let Some(rows) = progress_rx.recv().await {
                            emit(
                                &events,
                                session_id,
                                Event::ExportProgress {
                                    id,
                                    rows: rows as usize,
                                },
                            );
                        }
                    });
                }

                // Run the export off the dispatch loop so the loop keeps pumping
                // (a `CancelExport` or any other command lands while it streams).
                let events = events.clone();
                let exports = state.exports.clone();
                tokio::spawn(async move {
                    let path_str = path.to_string_lossy().into_owned();
                    let result = driver
                        .export(&sql, &path, format, cancel, progress_tx)
                        .await;
                    lock(&exports).remove(&id);
                    match result {
                        Ok(rows) => emit(
                            &events,
                            session_id,
                            Event::ExportFinished {
                                id,
                                path: path_str,
                                rows: rows as usize,
                            },
                        ),
                        Err(RedError::Interrupted) => {
                            emit(&events, session_id, Event::ExportCancelled { id })
                        }
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::CancelExport { id } => {
                let Some(sid) = session_id else { continue };
                // Flip the flag; the export's per-row check picks it up, removes
                // the partial file, and replies `ExportCancelled`.
                if let Some(state) = sessions.get(&sid) {
                    if let Some(cancel) = lock(&state.exports).get(&id) {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            }

            Command::Cancel => {
                let Some(id) = session_id else { continue };
                // No fetch is in flight here (pull protocol), so cancelling just
                // drops the cursor; the in-flight case is handled inside
                // `drive_fetch`.
                if let Some(aq) = sessions.get_mut(&id).and_then(|s| s.active.take()) {
                    aq.cancel.cancel();
                    emit(&events, session_id, Event::QueryCancelled);
                }
            }

            Command::Shutdown => break,
        }
    }
}

/// Drop every session that's been idle past [`IDLE_EVICT`] and isn't the
/// foreground one: abort its in-flight work, release its driver, and tell the UI
/// (`Disconnected`) so it demotes that workspace to a plain recent.
fn evict_idle(
    sessions: &mut HashMap<SessionId, SessionState>,
    foreground: Option<SessionId>,
    events: &Events,
) {
    let now = Instant::now();
    let stale: Vec<SessionId> = sessions
        .iter()
        .filter(|(id, s)| Some(**id) != foreground && now.duration_since(s.last_used) >= IDLE_EVICT)
        .map(|(id, _)| *id)
        .collect();
    for id in stale {
        if let Some(mut state) = sessions.remove(&id) {
            state.teardown();
            tracing::info!(id = id.0, "evicted idle session");
            emit(events, Some(id), Event::Disconnected);
        }
    }
}

/// Run one window fetch while staying responsive to `Cancel` / `Shutdown` /
/// timeout. On a full window the cursor is parked back into `active` for the next
/// `FetchMore`; on the last window / cancel / error the cursor is dropped.
/// Returns `true` if a `Shutdown` arrived during the fetch.
async fn drive_fetch(
    aq: ActiveQuery,
    max: usize,
    session: SessionId,
    commands: &mut CmdReceiver<Envelope>,
    events: &Events,
    active: &mut Option<ActiveQuery>,
) -> bool {
    let session = Some(session);
    let mut aq = aq;
    let mut cancelled = false;
    let mut timed_out = false;
    let mut shutdown = false;

    let outcome = {
        let fetch = aq.cursor.next_window(max);
        tokio::pin!(fetch);
        loop {
            tokio::select! {
                res = &mut fetch => break res,
                cmd = commands.recv(), if !shutdown => match cmd {
                    // Legacy single-active streaming path: a `Cancel` (from any
                    // session) drops this cursor; `Shutdown` always stops the loop.
                    Some((_, Command::Cancel)) => { cancelled = true; aq.cancel.cancel(); }
                    Some((_, Command::Shutdown)) | None => { shutdown = true; aq.cancel.cancel(); }
                    // Pull protocol: the consumer awaits each window before
                    // sending the next command, so nothing else lands mid-fetch.
                    _ => {}
                },
                _ = sleep_for(aq.timeout), if !timed_out && aq.timeout.is_some() => {
                    timed_out = true;
                    aq.cancel.cancel();
                }
            }
        }
    };

    match outcome {
        Ok(window) => {
            // An interrupt can land between the last row and the channel reply,
            // so honor a pending cancel/timeout even on an Ok window.
            if shutdown || cancelled {
                emit(events, session, Event::QueryCancelled);
            } else if timed_out {
                emit(events, session, Event::Error(RedError::Timeout.to_string()));
            } else {
                aq.streamed += window.rows.len();
                let done = window.exhausted;
                emit(events, session, Event::QueryRows(window));
                if done {
                    emit(
                        events,
                        session,
                        Event::QueryFinished {
                            rows_streamed: aq.streamed,
                            elapsed: aq.started.elapsed(),
                        },
                    );
                } else {
                    *active = Some(aq);
                }
            }
        }
        Err(RedError::Interrupted) => {
            if timed_out {
                emit(events, session, Event::Error(RedError::Timeout.to_string()));
            } else {
                emit(events, session, Event::QueryCancelled);
            }
        }
        Err(e) => emit(events, session, Event::Error(e.to_string())),
    }

    shutdown
}

/// Serve one `FetchRun` (see [`RunFetch`]). Returns the rows plus whether
/// their ordinals are interpolated estimates (only an interpolated `Jump`).
async fn run_fetch(
    driver: &dyn DatabaseDriver,
    spec: &OpenSpec,
    key: &KeySpec,
    fetch: &RunFetch,
    limit: usize,
    abort: &AbortSignal,
) -> red_core::Result<(Vec<Vec<Value>>, bool)> {
    match fetch {
        RunFetch::Forward { after } => {
            let page = driver
                .fetch_seek(&spec.sql, key, after.as_deref(), false, limit, abort)
                .await?;
            Ok((page.rows, false))
        }
        RunFetch::Backward { before } => {
            let page = driver
                .fetch_seek(&spec.sql, key, Some(before.as_slice()), true, limit, abort)
                .await?;
            Ok((page.rows, false))
        }
        RunFetch::Jump { ordinal, exact } => {
            // Key-space interpolation: land near `ordinal / total` of the key
            // range in one indexed seek. Approximate (exact only for dense,
            // uniform keys) — the grid renders the run's ordinals with a `≈`.
            // Skipped for an exact "go to row N": that wants the precise row, so
            // it falls straight through to the exact `OFFSET` page below.
            // Interpolates on the *lead* column only (a one-element prefix bound),
            // so a composite sort key still gets fraction jumps when its lead is
            // an integer.
            if !exact && key.kind == KeyKind::Int {
                if let (Some((min, max)), Some(total)) = (spec.bounds, spec.total) {
                    if total > 1 && max > min {
                        let fraction = (*ordinal as f64 / (total - 1) as f64).clamp(0.0, 1.0);
                        let span = max as f64 - min as f64;
                        // Ordinal 0 is the result's first row in sort order: the
                        // smallest lead value for an ascending sort, the largest
                        // for a descending one. Seek forward (in sort order) from
                        // just past the target's neighbour so the bound row is
                        // included.
                        let bound = if key.descending {
                            let target =
                                (max as f64 - span * fraction).clamp(min as f64, max as f64);
                            (target as i64).saturating_add(1) // `< t+1` == `<= t`
                        } else {
                            let target =
                                (min as f64 + span * fraction).clamp(min as f64, max as f64);
                            (target as i64).saturating_sub(1) // `> t-1` == `>= t`
                        };
                        let page = driver
                            .fetch_seek(
                                &spec.sql,
                                key,
                                Some(&[Value::Integer(bound)]),
                                false,
                                limit,
                                abort,
                            )
                            .await?;
                        // Jumping to ordinal 0 seeks from the true start — exact.
                        if !page.rows.is_empty() {
                            return Ok((page.rows, *ordinal != 0));
                        }
                        // An empty interpolated window (data shrank underneath)
                        // falls through to the exact `OFFSET` page.
                    }
                }
            }
            // Exact jump: serve from the checkpoint index when it reaches this
            // depth — seek to the nearest checkpoint, then skip `< stride` rows
            // (O(stride), exact). The build is kicked off by the `FetchRun` arm.
            if *exact {
                if let Some((cp_ordinal, cp_key)) = nearest_checkpoint(spec, *ordinal) {
                    let skip = *ordinal - cp_ordinal;
                    if skip <= CHECKPOINT_STRIDE {
                        let page = driver
                            .fetch_seek_skip(&spec.sql, key, Some(&cp_key), skip, limit, abort)
                            .await?;
                        return Ok((page.rows, false));
                    }
                }
            }
            // No usable key/bounds/index: one `OFFSET` page — O(ordinal), but a
            // one-off; ordinals stay exact. Keyed mode, so the key column rides
            // through verbatim — these rows' keys seed the run's seek bounds.
            let page = driver
                .fetch_page(
                    &spec.sql,
                    *ordinal,
                    limit,
                    PageCap::Display {
                        key: Some(key.clone()),
                    },
                    abort,
                )
                .await?;
            Ok((page.rows, false))
        }
    }
}

/// Claim the right to build `spec`'s checkpoint index: true only for a keyset
/// result, a jump deep enough to be worth it, and an index not already built or
/// building. Flips the status to `Building` under the lock so two concurrent deep
/// jumps can't both spawn a build.
fn claim_build(spec: &OpenSpec, ordinal: usize) -> bool {
    if spec.key.is_none() || spec.key_cols.is_empty() || ordinal <= BUILD_TRIGGER_DEPTH {
        return false;
    }
    let mut idx = lock(&spec.checkpoints);
    if idx.status == BuildStatus::Idle {
        idx.status = BuildStatus::Building;
        true
    } else {
        false
    }
}

/// The greatest checkpoint `(ordinal, key tuple)` at or before `target`, if the
/// index has reached that far. Points are ascending, so the last one `<= target`
/// wins.
fn nearest_checkpoint(spec: &OpenSpec, target: usize) -> Option<(usize, Vec<Value>)> {
    let idx = lock(&spec.checkpoints);
    idx.points.iter().rev().find(|(o, _)| *o <= target).cloned()
}

/// Build `spec`'s checkpoint index: walk the result in `CHECKPOINT_STRIDE`-sized
/// strides via an indexed seek + bounded skip, recording one `(ordinal, key tuple)`
/// per stride. One row transfers per checkpoint (just its key columns), so it's a
/// background O(total)-server-work scan with flat memory. Bails if the result closes.
async fn build_checkpoints(
    driver: Arc<dyn DatabaseDriver>,
    spec: OpenSpec,
    results: ResultMap,
    epoch: u64,
    abort: AbortSignal,
) {
    let key = spec.key.clone();
    let (Some(key), false) = (key, spec.key_cols.is_empty()) else {
        lock(&spec.checkpoints).status = BuildStatus::Idle;
        return;
    };
    let key_cols = spec.key_cols.clone();
    let total = spec.total.unwrap_or(0);

    // One checkpoint per stride: reserve the whole index up front when the total
    // is known, so a deep walk doesn't repeatedly grow + copy the points Vec under
    // the lock as it fills (a 100M-row result is ~10k pushes otherwise).
    if total > 0 {
        lock(&spec.checkpoints)
            .points
            .reserve(total / CHECKPOINT_STRIDE + 1);
    }

    // First checkpoint: ordinal 0, seeking from the result's start. Each later
    // step seeks from the previous checkpoint key (inclusive) and skips a stride.
    let mut ordinal = 0usize;
    let mut from: Option<Vec<Value>> = None;
    let mut skip = 0usize;

    loop {
        // The tab closed or re-sorted — abandon the scan.
        if !lock(&results).contains_key(&epoch) {
            return;
        }
        let page = match driver
            .fetch_seek_skip(&spec.sql, &key, from.as_deref(), skip, 1, &abort)
            .await
        {
            Ok(page) => page,
            // A superseded build's in-flight stride comes back interrupted — a
            // clean stop, not a failure; leave the status so a later jump retries.
            Err(RedError::Interrupted) => {
                lock(&spec.checkpoints).status = BuildStatus::Idle;
                return;
            }
            Err(e) => {
                tracing::warn!(%epoch, "checkpoint build failed: {e}");
                lock(&spec.checkpoints).status = BuildStatus::Idle; // allow a later retry
                return;
            }
        };
        let Some(row) = page.rows.first() else {
            break; // walked past the last row
        };
        let cp_key: Vec<Value> = key_cols
            .iter()
            .map(|&c| row.get(c).cloned().unwrap_or(Value::Null))
            .collect();
        lock(&spec.checkpoints)
            .points
            .push((ordinal, cp_key.clone()));

        from = Some(cp_key);
        skip = CHECKPOINT_STRIDE;
        ordinal += CHECKPOINT_STRIDE;
        if total > 0 && ordinal >= total {
            break;
        }
        // Yield so a burst of interactive fetches isn't starved by the scan.
        tokio::task::yield_now().await;
    }
    lock(&spec.checkpoints).status = BuildStatus::Done;
}

/// Wrap a base query in `ORDER BY <position>` for the `OFFSET`-fallback sorted
/// path (a sorted browse with no resolvable PK). Ordering by output *position*
/// is engine-agnostic — it needs no identifier quoting — and the derived table is
/// aliased because MySQL and Postgres both reject an unaliased subquery in `FROM`.
fn wrap_sorted(base: &str, position: usize, descending: bool) -> String {
    let base = base.trim_end().trim_end_matches(';').trim_end();
    format!(
        "SELECT * FROM ({base}) AS _red_sort ORDER BY {position} {}",
        if descending { "DESC" } else { "ASC" }
    )
}

/// A timeout future that never fires when no timeout is set, so the `select!`
/// branch can be a stable shape.
async fn sleep_for(timeout: Option<Duration>) {
    match timeout {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
    }
}

/// Race a one-shot fetch against the statement timeout. On expiry, fire the
/// fetch's [`AbortSignal`] so the engine stops, then surface [`RedError::Timeout`].
/// A `None` timeout never fires; an *externally* aborted fetch (a superseded
/// page/run) keeps its [`RedError::Interrupted`], so the caller stays silent.
async fn with_timeout<T>(
    timeout: Option<Duration>,
    abort: &AbortSignal,
    fut: impl std::future::Future<Output = red_core::Result<T>>,
) -> red_core::Result<T> {
    tokio::pin!(fut);
    let mut timed_out = false;
    let out = loop {
        tokio::select! {
            res = &mut fut => break res,
            _ = sleep_for(timeout), if !timed_out && timeout.is_some() => {
                timed_out = true;
                abort.abort();
            }
        }
    };
    match out {
        Err(RedError::Interrupted) if timed_out => Err(RedError::Timeout),
        other => other,
    }
}

/// [`connect`] bounded by [`CONNECT_TIMEOUT`]. A timeout surfaces as a connect
/// error like any other failure, so the UI's retry/backoff path handles it.
async fn attempt_connect(config: &ConnectionConfig) -> Result<Arc<dyn DatabaseDriver>, String> {
    match tokio::time::timeout(CONNECT_TIMEOUT, connect(config)).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "connection timed out after {}s",
            CONNECT_TIMEOUT.as_secs()
        )),
    }
}

async fn connect(config: &ConnectionConfig) -> Result<Arc<dyn DatabaseDriver>, String> {
    match config.kind {
        DbKind::Sqlite => {
            let driver = SqliteDriver::new(config.dsn(), config.read_only);
            driver.ping().await.map_err(|e| e.to_string())?;
            Ok(Arc::new(driver))
        }
        DbKind::Postgres => {
            let driver = PostgresDriver::connect(&config.dsn(), config.read_only)
                .await
                .map_err(|e| e.to_string())?;
            Ok(Arc::new(driver))
        }
        DbKind::Mysql => {
            // A MySQL connection can see every database on the server; scope the
            // schema tree to the chosen one when the connection names a database.
            let driver = MysqlDriver::connect(&config.dsn(), config.read_only)
                .await
                .map_err(|e| e.to_string())?
                .with_scope(Some(config.database.clone()));
            Ok(Arc::new(driver))
        }
    }
}

/// The UI may have dropped its receiver (window closed) — a failed send is the
/// expected shutdown path, not an error. `session` tags the event so the UI
/// routes it to the right workspace (`None` for the session-less probe replies).
fn emit(events: &Events, session: Option<SessionId>, event: Event) {
    let _ = events.unbounded_send((session, event));
}

/// Abort every in-flight fetch across all open results and clear the registry —
/// the connection is being dropped or replaced, so all of it is doomed work.
fn abort_all_inflight(inflight: &mut HashMap<u64, InFlight>) {
    for (_, f) in inflight.drain() {
        f.abort_all();
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use red_core::KeyKind;
    use red_driver::SqliteDriver;

    /// Build an `id 1..=n` table in a fresh temp DB and return a driver over it.
    fn driver_with(n: i64, tag: &str) -> (std::path::PathBuf, Arc<dyn DatabaseDriver>) {
        let path = std::env::temp_dir().join(format!("red_ckpt_{tag}_{}.db", std::process::id()));
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE t(x INTEGER PRIMARY KEY NOT NULL);
             WITH RECURSIVE c(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM c WHERE v < {n})
             INSERT INTO t SELECT v FROM c;"
        ))
        .unwrap();
        let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::new(path.clone(), true));
        (path, driver)
    }

    fn spec_for(checkpoints: &Arc<Mutex<CheckpointIndex>>, total: usize) -> OpenSpec {
        OpenSpec {
            sql: "SELECT * FROM t".into(),
            key: Some(KeySpec::single("x", KeyKind::Int)),
            key_cols: vec![0],
            bounds: None,
            total: Some(total),
            checkpoints: checkpoints.clone(),
        }
    }

    /// The build walks the result in `CHECKPOINT_STRIDE` strides, recording the
    /// key at each — and an exact jump is then served from the nearest one.
    #[tokio::test]
    async fn builds_index_and_serves_exact_jump() {
        let (path, driver) = driver_with(25_000, "build");
        let checkpoints = Arc::new(Mutex::new(CheckpointIndex::default()));
        let spec = spec_for(&checkpoints, 25_000);

        // The build aborts unless the result is still open, so register it.
        let results: ResultMap = Arc::new(Mutex::new(HashMap::new()));
        lock(&results).insert(1, spec.clone());

        build_checkpoints(driver.clone(), spec.clone(), results, 1, AbortSignal::new()).await;

        // Checkpoints every 10k rows: ids are 1-based, so ordinal N → id N+1.
        // Scoped so the guard is dropped before the `await` below.
        {
            let idx = lock(&checkpoints);
            assert_eq!(idx.status, BuildStatus::Done);
            assert_eq!(
                idx.points,
                vec![
                    (0, vec![Value::Integer(1)]),
                    (10_000, vec![Value::Integer(10_001)]),
                    (20_000, vec![Value::Integer(20_001)]),
                ]
            );
        }

        // The nearest checkpoint at/under a target, and a bounded-skip serve.
        assert_eq!(
            nearest_checkpoint(&spec, 20_500),
            Some((20_000, vec![Value::Integer(20_001)]))
        );
        let (rows, estimated) = run_fetch(
            &*driver,
            &spec,
            spec.key.as_ref().unwrap(),
            &RunFetch::Jump {
                ordinal: 20_500,
                exact: true,
            },
            5,
            &AbortSignal::new(),
        )
        .await
        .unwrap();
        assert!(!estimated, "an exact jump reports exact ordinals");
        assert_eq!(rows[0][0], Value::Integer(20_501));

        std::fs::remove_file(&path).ok();
    }

    /// `claim_build` only fires for a deep jump on a keyed result, and only once.
    #[tokio::test]
    async fn claim_build_is_deep_and_one_shot() {
        let checkpoints = Arc::new(Mutex::new(CheckpointIndex::default()));
        let spec = spec_for(&checkpoints, 1_000_000);

        // Shallow jumps don't trigger a build (OFFSET is already fast there).
        assert!(!claim_build(&spec, 50));
        // A deep jump claims it once; a second claim is refused (build in flight).
        assert!(claim_build(&spec, 500_000));
        assert!(!claim_build(&spec, 600_000));
        assert_eq!(lock(&checkpoints).status, BuildStatus::Building);
    }
}
