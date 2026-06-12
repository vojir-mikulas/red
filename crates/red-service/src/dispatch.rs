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
    CancelToken, DatabaseDriver, MysqlDriver, PageCap, PostgresDriver, QueryCursor, SqliteDriver,
};
use tokio::sync::mpsc::UnboundedReceiver as CmdReceiver;
use tokio::sync::Semaphore;

use crate::{Command, Event, RunFetch};

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

/// A sparse `ordinal → key` index over an open keyset result: one entry every
/// [`CHECKPOINT_STRIDE`] rows, built by a single background ordered traversal.
/// Lets an exact jump to row N seek to the nearest checkpoint and skip `< stride`
/// rows — O(stride), not O(N). Shared via `Arc<Mutex<…>>` so the build task fills
/// it incrementally while fetches read it.
#[derive(Debug, Default)]
struct CheckpointIndex {
    /// `(ordinal, key)` pairs, ascending by ordinal. `points[0]` is `(0, first_key)`.
    points: Vec<(usize, Value)>,
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
    /// Position of the key column within a result row — the checkpoint build
    /// reads each checkpoint's key out of the row at that index.
    key_col: Option<usize>,
    bounds: Option<(i64, i64)>,
    total: Option<usize>,
    checkpoints: Arc<Mutex<CheckpointIndex>>,
}

/// The open-result map, shared with the spawned open/fetch tasks (they fill in
/// bounds/total after the fact and read specs without round-tripping commands).
type ResultMap = Arc<Mutex<HashMap<u64, OpenSpec>>>;

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

pub(crate) async fn dispatch(mut commands: CmdReceiver<Command>, events: UnboundedSender<Event>) {
    let mut session: Option<Arc<dyn DatabaseDriver>> = None;
    let mut active: Option<ActiveQuery> = None;
    // The spec backing each open result, keyed by epoch and re-fetched per
    // `FetchPage`/`FetchRun`. One entry per live query tab (plus a transient
    // extra while a re-sort swaps a tab to a new epoch). Fetches run as detached
    // tasks (so a slow `count`/deep page never stalls the dispatch loop); a
    // fetch for an epoch absent from the map is stale — the tab closed or moved
    // on — and is dropped. UI epochs start at 1, so an empty map means "no live
    // result".
    let results: ResultMap = Arc::new(Mutex::new(HashMap::new()));
    // Bounds how many page fetches hit the server concurrently (see the const).
    let page_fetch_limit = Arc::new(Semaphore::new(MAX_CONCURRENT_PAGE_FETCHES));
    // In-flight exports: `id → cancel flag`. An export runs as a detached task
    // (so the loop stays responsive while it streams); `CancelExport` flips its
    // flag and the task removes its own entry on completion. Shared so the task
    // can self-clean without round-tripping a command.
    let exports: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>> = Arc::new(Mutex::new(HashMap::new()));

    while let Some(command) = commands.recv().await {
        match command {
            Command::Connect(config) => {
                active = None; // a new connection abandons any in-flight cursor
                lock(&results).clear();
                match attempt_connect(&config).await {
                    Ok(driver) => {
                        let version = driver.server_version();
                        session = Some(driver);
                        emit(&events, Event::Connected { version });
                    }
                    Err(message) => emit(&events, Event::Error(message)),
                }
            }

            Command::TestConnection(config) => {
                // A throwaway probe: connect, report, and let the driver drop. The
                // active session is untouched.
                match attempt_connect(&config).await {
                    Ok(driver) => emit(
                        &events,
                        Event::TestSucceeded {
                            version: driver.server_version(),
                        },
                    ),
                    Err(message) => emit(&events, Event::TestFailed { message }),
                }
            }

            Command::Disconnect => {
                active = None;
                lock(&results).clear();
                session = None;
                emit(&events, Event::Disconnected);
            }

            Command::Query { sql, opts } => {
                active = None; // a new query supersedes the previous cursor
                let Some(driver) = session.clone() else {
                    emit(&events, Event::Error("not connected".into()));
                    continue;
                };
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
                            Event::QueryStarted {
                                columns: aq.cursor.columns().to_vec(),
                            },
                        );
                        if drive_fetch(aq, opts.window, &mut commands, &events, &mut active).await {
                            break; // shutdown requested mid-fetch
                        }
                    }
                    Err(err) => emit(&events, Event::Error(err.to_string())),
                }
            }

            Command::FetchMore { max } => match active.take() {
                Some(aq) => {
                    if drive_fetch(aq, max, &mut commands, &events, &mut active).await {
                        break;
                    }
                }
                None => emit(&events, Event::Error("no active query".into())),
            },

            Command::LoadObjects => {
                let Some(driver) = session.clone() else {
                    emit(&events, Event::Error("not connected".into()));
                    continue;
                };
                match driver.list_objects().await {
                    Ok(schemas) => emit(&events, Event::ObjectsLoaded { schemas }),
                    Err(e) => emit(&events, Event::Error(e.to_string())),
                }
            }

            Command::DescribeTable { schema, table } => {
                let Some(driver) = session.clone() else {
                    emit(&events, Event::Error("not connected".into()));
                    continue;
                };
                match driver.describe_table(&schema, &table).await {
                    Ok(detail) => emit(
                        &events,
                        Event::TableDescribed {
                            schema,
                            table,
                            detail,
                        },
                    ),
                    Err(e) => emit(&events, Event::Error(e.to_string())),
                }
            }

            Command::OpenResult { sql, epoch, table } => {
                let Some(driver) = session.clone() else {
                    emit(&events, Event::Error("not connected".into()));
                    continue;
                };
                // Registered before the (slow) open task so an early fetch for
                // this epoch isn't mistaken for a stale one.
                lock(&results).insert(
                    epoch,
                    OpenSpec {
                        sql: sql.clone(),
                        key: None,
                        key_col: None,
                        bounds: None,
                        total: None,
                        checkpoints: Arc::new(Mutex::new(CheckpointIndex::default())),
                    },
                );
                // Count + column metadata can be slow (a full `COUNT(*)` over a
                // large table); run them off the dispatch loop so switching to
                // another table stays instant.
                let events = events.clone();
                let results = results.clone();
                tokio::spawn(async move {
                    // A table browse resolves its seek key from the table's
                    // introspected detail; a resolution failure just means the
                    // `OFFSET` fallback (never an error).
                    let key = match &table {
                        Some((schema, table)) => match driver.describe_table(schema, table).await {
                            Ok(detail) => {
                                let key = KeySpec::from_detail(&detail);
                                match &key {
                                    Some(k) => tracing::info!(
                                        %schema, %table, column = %k.column,
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
                    // `LIMIT 0` reads column metadata without stepping rows;
                    // counting and the key-bounds probe run concurrently with it.
                    let bounds = async {
                        match &key {
                            Some(k) if k.kind == KeyKind::Int => {
                                driver.key_bounds(&sql, k).await.ok().flatten()
                            }
                            _ => None,
                        }
                    };
                    let (total, columns, bounds) = tokio::join!(
                        driver.count(&sql),
                        driver.fetch_page(&sql, 0, 0, PageCap::Full),
                        bounds
                    );
                    match (total, columns) {
                        (Ok(total), Ok(page)) => {
                            let total = total.max(0) as usize;
                            // Fill the spec in only if the result is still open.
                            // `key_col` locates the key within a row so the
                            // checkpoint build can read each checkpoint's key.
                            let key_col = key
                                .as_ref()
                                .and_then(|k| page.columns.iter().position(|c| c.name == k.column));
                            if let Some(spec) = lock(&results).get_mut(&epoch) {
                                spec.key = key.clone();
                                spec.key_col = key_col;
                                spec.bounds = bounds;
                                spec.total = Some(total);
                            }
                            emit(
                                &events,
                                Event::ResultReady {
                                    columns: page.columns,
                                    total,
                                    epoch,
                                    key,
                                },
                            );
                        }
                        (Err(e), _) | (_, Err(e)) => emit(&events, Event::Error(e.to_string())),
                    }
                });
            }

            Command::FetchPage {
                offset,
                limit,
                epoch,
            } => {
                let Some(driver) = session.clone() else {
                    emit(&events, Event::Error("not connected".into()));
                    continue;
                };
                // The tab closed or re-sorted (its epoch is gone); skip the stale
                // request rather than running an expensive query whose result
                // would be discarded.
                let Some(sql) = lock(&results).get(&epoch).map(|s| s.sql.clone()) else {
                    continue;
                };
                // Pages fetch concurrently (the driver pools connections) and off
                // the dispatch loop, so a deep-`OFFSET` page never blocks the next
                // command or another page — but no more than `page_fetch_limit` at
                // once, so a burst can't saturate the server.
                let events = events.clone();
                let limit_src = page_fetch_limit.clone();
                tokio::spawn(async move {
                    let _permit = limit_src.acquire_owned().await;
                    // Offset-mode display page — cap fat cells; no seek key to exempt.
                    match driver
                        .fetch_page(&sql, offset, limit, PageCap::Display { key: None })
                        .await
                    {
                        Ok(page) => emit(
                            &events,
                            Event::ResultPageLoaded {
                                offset,
                                rows: page.rows,
                                epoch,
                            },
                        ),
                        Err(e) => emit(&events, Event::Error(e.to_string())),
                    }
                });
            }

            Command::FetchRun {
                epoch,
                fetch,
                limit,
                seq,
            } => {
                let Some(driver) = session.clone() else {
                    emit(&events, Event::Error("not connected".into()));
                    continue;
                };
                // Stale epoch (tab closed / re-sorted) — drop, like `FetchPage`.
                let Some(spec) = lock(&results).get(&epoch).cloned() else {
                    continue;
                };
                let Some(key) = spec.key.clone() else {
                    continue; // a keyless result never gets `FetchRun`s
                };
                // A deep exact jump kicks off the checkpoint index (once) so the
                // *next* deep jump is O(stride). This one still serves via OFFSET.
                if let RunFetch::Jump {
                    ordinal,
                    exact: true,
                } = &fetch
                {
                    if claim_build(&spec, *ordinal) {
                        tokio::spawn(build_checkpoints(
                            driver.clone(),
                            spec.clone(),
                            results.clone(),
                            epoch,
                        ));
                    }
                }
                let events = events.clone();
                let limit_src = page_fetch_limit.clone();
                tokio::spawn(async move {
                    let _permit = limit_src.acquire_owned().await;
                    match run_fetch(&*driver, &spec, &key, &fetch, limit).await {
                        Ok((rows, estimated)) => emit(
                            &events,
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
                            emit(&events, Event::ResultRunFailed { epoch, seq });
                            emit(&events, Event::Error(e.to_string()));
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
                let Some(driver) = session.clone() else {
                    emit(&events, Event::Error("not connected".into()));
                    continue;
                };
                // Stale epoch (tab closed / re-sorted) — drop, like `FetchPage`.
                let Some(sql) = lock(&results).get(&epoch).map(|s| s.sql.clone()) else {
                    continue;
                };
                // Same windowed read as a page fetch, but `Full` so the rows carry the
                // real values (the grid's display cap is bypassed) for the clipboard.
                let events = events.clone();
                let limit_src = page_fetch_limit.clone();
                tokio::spawn(async move {
                    let _permit = limit_src.acquire_owned().await;
                    match driver.fetch_page(&sql, offset, limit, PageCap::Full).await {
                        Ok(page) => emit(
                            &events,
                            Event::CopyRowsLoaded {
                                id,
                                rows: page.rows,
                            },
                        ),
                        Err(e) => emit(&events, Event::Error(e.to_string())),
                    }
                });
            }

            Command::CloseResult { epoch } => {
                lock(&results).remove(&epoch);
            }

            Command::Execute { sql } => {
                let Some(driver) = session.clone() else {
                    emit(&events, Event::Error("not connected".into()));
                    continue;
                };
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
                            Event::Executed {
                                affected: affected as usize,
                            },
                        );
                    }
                    Err(e) => emit(&events, Event::Error(e.to_string())),
                }
            }

            Command::Export {
                format,
                path,
                epoch,
                id,
            } => {
                let Some(driver) = session.clone() else {
                    emit(&events, Event::Error("not connected".into()));
                    continue;
                };
                let Some(sql) = lock(&results).get(&epoch).map(|s| s.sql.clone()) else {
                    emit(&events, Event::Error("no open result to export".into()));
                    continue;
                };
                // Register the cancel flag before the task starts, so a fast
                // `CancelExport` can't race ahead of it.
                let cancel = Arc::new(AtomicBool::new(false));
                lock(&exports).insert(id, cancel.clone());

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
                let exports = exports.clone();
                tokio::spawn(async move {
                    let path_str = path.to_string_lossy().into_owned();
                    let result = driver
                        .export(&sql, &path, format, cancel, progress_tx)
                        .await;
                    lock(&exports).remove(&id);
                    match result {
                        Ok(rows) => emit(
                            &events,
                            Event::ExportFinished {
                                id,
                                path: path_str,
                                rows: rows as usize,
                            },
                        ),
                        Err(RedError::Interrupted) => emit(&events, Event::ExportCancelled { id }),
                        Err(e) => emit(&events, Event::Error(e.to_string())),
                    }
                });
            }

            Command::CancelExport { id } => {
                // Flip the flag; the export's per-row check picks it up, removes
                // the partial file, and replies `ExportCancelled`.
                if let Some(cancel) = lock(&exports).get(&id) {
                    cancel.store(true, Ordering::Relaxed);
                }
            }

            Command::Cancel => {
                // No fetch is in flight here (pull protocol), so cancelling just
                // drops the cursor; the in-flight case is handled inside
                // `drive_fetch`.
                if let Some(aq) = active.take() {
                    aq.cancel.cancel();
                    emit(&events, Event::QueryCancelled);
                }
            }

            Command::Shutdown => break,
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
    commands: &mut CmdReceiver<Command>,
    events: &UnboundedSender<Event>,
    active: &mut Option<ActiveQuery>,
) -> bool {
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
                    Some(Command::Cancel) => { cancelled = true; aq.cancel.cancel(); }
                    Some(Command::Shutdown) | None => { shutdown = true; aq.cancel.cancel(); }
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
                emit(events, Event::QueryCancelled);
            } else if timed_out {
                emit(events, Event::Error(RedError::Timeout.to_string()));
            } else {
                aq.streamed += window.rows.len();
                let done = window.exhausted;
                emit(events, Event::QueryRows(window));
                if done {
                    emit(
                        events,
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
                emit(events, Event::Error(RedError::Timeout.to_string()));
            } else {
                emit(events, Event::QueryCancelled);
            }
        }
        Err(e) => emit(events, Event::Error(e.to_string())),
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
) -> red_core::Result<(Vec<Vec<Value>>, bool)> {
    match fetch {
        RunFetch::Forward { after } => {
            let page = driver
                .fetch_seek(&spec.sql, key, after.as_ref(), false, limit)
                .await?;
            Ok((page.rows, false))
        }
        RunFetch::Backward { before } => {
            let page = driver
                .fetch_seek(&spec.sql, key, Some(before), true, limit)
                .await?;
            Ok((page.rows, false))
        }
        RunFetch::Jump { ordinal, exact } => {
            // Key-space interpolation: land near `ordinal / total` of the key
            // range in one indexed seek. Approximate (exact only for dense,
            // uniform keys) — the grid renders the run's ordinals with a `≈`.
            // Skipped for an exact "go to row N": that wants the precise row, so
            // it falls straight through to the exact `OFFSET` page below.
            if !exact && key.kind == KeyKind::Int {
                if let (Some((min, max)), Some(total)) = (spec.bounds, spec.total) {
                    if total > 1 && max > min {
                        let fraction = (*ordinal as f64 / (total - 1) as f64).clamp(0.0, 1.0);
                        let target = (min as f64 + (max as f64 - min as f64) * fraction)
                            .clamp(min as f64, max as f64)
                            as i64;
                        let page = driver
                            .fetch_seek(
                                &spec.sql,
                                key,
                                // `>=` via a strict `>` on the predecessor.
                                Some(&Value::Integer(target.saturating_sub(1))),
                                false,
                                limit,
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
                            .fetch_seek_skip(&spec.sql, key, Some(&cp_key), skip, limit)
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
    if spec.key.is_none() || spec.key_col.is_none() || ordinal <= BUILD_TRIGGER_DEPTH {
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

/// The greatest checkpoint `(ordinal, key)` at or before `target`, if the index
/// has reached that far. Points are ascending, so the last one `<= target` wins.
fn nearest_checkpoint(spec: &OpenSpec, target: usize) -> Option<(usize, Value)> {
    let idx = lock(&spec.checkpoints);
    idx.points.iter().rev().find(|(o, _)| *o <= target).cloned()
}

/// Build `spec`'s checkpoint index: walk the result in `CHECKPOINT_STRIDE`-sized
/// strides via an indexed seek + bounded skip, recording one `(ordinal, key)` per
/// stride. One row transfers per checkpoint (just its key), so it's a background
/// O(total)-server-work scan with flat memory. Bails if the result closes.
async fn build_checkpoints(
    driver: Arc<dyn DatabaseDriver>,
    spec: OpenSpec,
    results: ResultMap,
    epoch: u64,
) {
    let (Some(key), Some(key_col)) = (spec.key.clone(), spec.key_col) else {
        lock(&spec.checkpoints).status = BuildStatus::Idle;
        return;
    };
    let total = spec.total.unwrap_or(0);

    // First checkpoint: ordinal 0, seeking from the result's start. Each later
    // step seeks from the previous checkpoint key (inclusive) and skips a stride.
    let mut ordinal = 0usize;
    let mut from: Option<Value> = None;
    let mut skip = 0usize;

    loop {
        // The tab closed or re-sorted — abandon the scan.
        if !lock(&results).contains_key(&epoch) {
            return;
        }
        let page = match driver
            .fetch_seek_skip(&spec.sql, &key, from.as_ref(), skip, 1)
            .await
        {
            Ok(page) => page,
            Err(e) => {
                tracing::warn!(%epoch, "checkpoint build failed: {e}");
                lock(&spec.checkpoints).status = BuildStatus::Idle; // allow a later retry
                return;
            }
        };
        let Some(cp_key) = page.rows.first().and_then(|row| row.get(key_col).cloned()) else {
            break; // walked past the last row
        };
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

/// A timeout future that never fires when no timeout is set, so the `select!`
/// branch can be a stable shape.
async fn sleep_for(timeout: Option<Duration>) {
    match timeout {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
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
/// expected shutdown path, not an error.
fn emit(events: &UnboundedSender<Event>, event: Event) {
    let _ = events.unbounded_send(event);
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
            key: Some(KeySpec {
                column: "x".into(),
                kind: KeyKind::Int,
            }),
            key_col: Some(0),
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

        build_checkpoints(driver.clone(), spec.clone(), results, 1).await;

        // Checkpoints every 10k rows: ids are 1-based, so ordinal N → id N+1.
        // Scoped so the guard is dropped before the `await` below.
        {
            let idx = lock(&checkpoints);
            assert_eq!(idx.status, BuildStatus::Done);
            assert_eq!(
                idx.points,
                vec![
                    (0, Value::Integer(1)),
                    (10_000, Value::Integer(10_001)),
                    (20_000, Value::Integer(20_001)),
                ]
            );
        }

        // The nearest checkpoint at/under a target, and a bounded-skip serve.
        assert_eq!(
            nearest_checkpoint(&spec, 20_500),
            Some((20_000, Value::Integer(20_001)))
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
