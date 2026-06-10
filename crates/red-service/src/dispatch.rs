//! The dispatch loop: the backend thread's command pump. Owns the active
//! session and cursor, the open-result map, and the page-fetch concurrency
//! limit; runs queries through a windowed cursor and races each fetch against
//! incoming commands so a cancel or timeout can abort one in flight.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;
use red_core::{ConnectionConfig, DbKind, KeyKind, KeySpec, RedError, Value};
use red_driver::{
    CancelToken, DatabaseDriver, MysqlDriver, PostgresDriver, QueryCursor, SqliteDriver,
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

/// What the backend remembers about one open result: the SQL it re-fetches per
/// page/run, the resolved seek key, and — for interpolated jumps — the key's
/// min/max and the result's total.
#[derive(Debug, Clone)]
struct OpenSpec {
    sql: String,
    key: Option<KeySpec>,
    bounds: Option<(i64, i64)>,
    total: Option<usize>,
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
                        bounds: None,
                        total: None,
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
                    let (total, columns, bounds) =
                        tokio::join!(driver.count(&sql), driver.fetch_page(&sql, 0, 0), bounds);
                    match (total, columns) {
                        (Ok(total), Ok(page)) => {
                            let total = total.max(0) as usize;
                            // Fill the spec in only if the result is still open.
                            if let Some(spec) = lock(&results).get_mut(&epoch) {
                                spec.key = key.clone();
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
                    match driver.fetch_page(&sql, offset, limit).await {
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

            Command::CloseResult { epoch } => {
                lock(&results).remove(&epoch);
            }

            Command::Execute { sql } => {
                let Some(driver) = session.clone() else {
                    emit(&events, Event::Error("not connected".into()));
                    continue;
                };
                match driver.execute(&sql).await {
                    Ok(affected) => emit(
                        &events,
                        Event::Executed {
                            affected: affected as usize,
                        },
                    ),
                    Err(e) => emit(&events, Event::Error(e.to_string())),
                }
            }

            Command::Export {
                format,
                path,
                epoch,
            } => {
                let Some(driver) = session.clone() else {
                    emit(&events, Event::Error("not connected".into()));
                    continue;
                };
                let Some(sql) = lock(&results).get(&epoch).map(|s| s.sql.clone()) else {
                    emit(&events, Event::Error("no open result to export".into()));
                    continue;
                };
                match driver.export(&sql, &path, format).await {
                    Ok(rows) => emit(
                        &events,
                        Event::ExportFinished {
                            path: path.to_string_lossy().into_owned(),
                            rows: rows as usize,
                        },
                    ),
                    Err(e) => emit(&events, Event::Error(e.to_string())),
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
            // Non-interpolable key (or unknown bounds): one `OFFSET` page —
            // O(ordinal), but a one-off; ordinals stay exact.
            let page = driver.fetch_page(&spec.sql, *ordinal, limit).await?;
            Ok((page.rows, false))
        }
    }
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
