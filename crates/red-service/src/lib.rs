// SPDX-License-Identifier: GPL-3.0-or-later

//! The backend thread. Mirrors `nyx-service`: a dedicated OS thread runs its own
//! Tokio runtime, owns the active database session, and communicates with the
//! GPUI UI over two channels — `Command` in (UI → service, a Tokio mpsc usable
//! from any thread) and `Event` out (service → UI, a `futures` mpsc the GPUI
//! foreground executor can `await` as a `Stream`). The UI never blocks on the
//! backend.
//!
//! Querying is **pull-based and windowed**: `Query` opens a streaming cursor and
//! delivers the first window; each `FetchMore` pulls the next. This gives true
//! end-to-end backpressure (the backend never races ahead of the consumer) and
//! is the seam the result grid's lazy load-on-scroll plugs into. A fetch is
//! raced against incoming commands so a `Cancel` — or a `timeout` — can abort an
//! in-flight query out-of-band rather than dropping a future.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use red_core::{
    Column, ConnectionConfig, DbKind, ExportFormat, KeyKind, KeySpec, QueryOptions, RedError,
    RowWindow, SchemaMeta, TableDetail, Value,
};
use red_driver::{
    CancelToken, DatabaseDriver, MysqlDriver, PostgresDriver, QueryCursor, SqliteDriver,
};
use tokio::sync::mpsc::{
    unbounded_channel, UnboundedReceiver as CmdReceiver, UnboundedSender as CmdSender,
};
use tokio::sync::Semaphore;

/// Cap on page fetches running at once. The grid can request a burst of pages
/// (several tabs, or a viewport spanning page boundaries); without a cap a flung
/// scrollbar could otherwise fan out dozens of simultaneous deep-`OFFSET` scans
/// and saturate the server. The UI also throttles requests (see `FLING_ROWS`);
/// this is the backstop.
const MAX_CONCURRENT_PAGE_FETCHES: usize = 6;

/// UI → service. One active session at a time, driven across many commands.
#[derive(Debug)]
pub enum Command {
    Connect(ConnectionConfig),
    /// Open a throwaway session to validate a config, then drop it. Reports back
    /// via `TestSucceeded`/`TestFailed` without disturbing the active session.
    TestConnection(ConnectionConfig),
    /// Open a cursor for `sql` and stream the first window.
    Query {
        sql: String,
        opts: QueryOptions,
    },
    /// Pull the next window from the active cursor.
    FetchMore {
        max: usize,
    },
    /// Load the schema-tree skeleton (namespaces + object names) for the sidebar.
    LoadObjects,
    /// Describe one object's columns / FKs / indexes — sent lazily on tree expand.
    DescribeTable {
        schema: String,
        table: String,
    },
    /// Open `sql` as a grid result: count its rows and report column metadata +
    /// the total. The result is then browsed page-by-page via `FetchPage`, or —
    /// when a seek key resolves — run-by-run via `FetchRun`.
    ///
    /// `epoch` identifies this open result. Several results can be open at once
    /// (one per query tab), each keyed by its epoch; a page or export names the
    /// epoch it wants. `CloseResult` drops one when its tab closes.
    ///
    /// `table` names the `(schema, table)` when `sql` is a plain table browse:
    /// the backend introspects it for a keyset seek key (single-column PK or
    /// unique not-null index) and echoes the resolved [`KeySpec`] in
    /// `ResultReady`. `None` (editor SQL, sorted re-opens) pages by `OFFSET`.
    OpenResult {
        sql: String,
        epoch: u64,
        table: Option<(String, String)>,
    },
    /// Fetch one random-access page of an open result (grid load-on-scroll).
    /// `epoch` selects which open result; an unknown epoch is ignored (the tab
    /// closed or re-sorted).
    FetchPage {
        offset: usize,
        limit: usize,
        epoch: u64,
    },
    /// Fetch one run window of a keyset-keyed open result (M10): extend the
    /// grid's resident run from a boundary key, or jump to an ordinal. Replied
    /// with `ResultRunLoaded`, echoing `fetch`/`seq` so the grid can drop a
    /// reply its buffer has moved past.
    FetchRun {
        epoch: u64,
        fetch: RunFetch,
        limit: usize,
        seq: u64,
    },
    /// Drop an open result (its query tab closed, or it was re-sorted into a new
    /// epoch). Unknown epochs are a no-op.
    CloseResult {
        epoch: u64,
    },
    /// Run a non-row-returning statement (write/DDL) in a transaction.
    Execute {
        sql: String,
    },
    /// Stream an open result to `path` in `format`, row-by-row. `epoch` selects
    /// which open result (the active tab's grid).
    Export {
        format: ExportFormat,
        path: PathBuf,
        epoch: u64,
    },
    /// Abort the active query / drop its cursor.
    Cancel,
    /// Drop the active session and any cursor; return to a disconnected state.
    Disconnect,
    Shutdown,
}

/// service → UI. Streamed into the UI's async loop.
#[derive(Debug)]
pub enum Event {
    /// A session opened. `version` is the engine version for the status bar.
    Connected {
        version: String,
    },
    /// The session was dropped (in response to `Disconnect`).
    Disconnected,
    /// A `TestConnection` probe opened a session successfully; `version` is the
    /// engine version it reported.
    TestSucceeded {
        version: String,
    },
    /// A `TestConnection` probe failed; `message` is the driver error.
    TestFailed {
        message: String,
    },
    /// A query opened; column metadata is known before any rows arrive.
    QueryStarted {
        columns: Vec<Column>,
    },
    /// One bounded window of rows. `RowWindow::exhausted` marks the last one.
    QueryRows(RowWindow),
    /// The cursor reached the end of the result.
    QueryFinished {
        rows_streamed: usize,
        elapsed: Duration,
    },
    /// The active query was cancelled (user `Cancel`).
    QueryCancelled,
    /// The schema-tree skeleton, in response to `LoadObjects`.
    ObjectsLoaded {
        schemas: Vec<SchemaMeta>,
    },
    /// One object's detail, in response to `DescribeTable`. Echoes `schema`/`table`
    /// so the async UI routes the detail to the right node regardless of order.
    TableDescribed {
        schema: String,
        table: String,
        detail: TableDetail,
    },
    /// A result opened: its columns and total row count (for `OpenResult`).
    /// Echoes the open `epoch` so the grid can ignore a late reply for a result
    /// it has already replaced. `key` is the seek key the backend resolved for
    /// a table browse — present, the grid pages by keyset runs (`FetchRun`)
    /// instead of `OFFSET`.
    ResultReady {
        columns: Vec<Column>,
        total: usize,
        epoch: u64,
        key: Option<KeySpec>,
    },
    /// One page of the open result. Echoes `offset` so the grid drops it into the
    /// right slot of its window buffer regardless of arrival order, and `epoch`
    /// so a page for a superseded result is discarded.
    ResultPageLoaded {
        offset: usize,
        rows: Vec<Vec<red_core::Value>>,
        epoch: u64,
    },
    /// One run window of a keyset result, in response to `FetchRun`. Echoes the
    /// request (`fetch`, `seq`) so the grid can match it against its in-flight
    /// state. `estimated` is `true` when a `Jump` landed by key-space
    /// interpolation — its ordinals are approximate until the run touches a
    /// true end of the result.
    ResultRunLoaded {
        epoch: u64,
        fetch: RunFetch,
        rows: Vec<Vec<red_core::Value>>,
        estimated: bool,
        seq: u64,
    },
    /// A `FetchRun` failed (the error itself is also surfaced via `Error`).
    /// Echoed so the grid can free its in-flight slot — without this a single
    /// failed seek would wedge the run buffer and freeze all further fetching.
    ResultRunFailed {
        epoch: u64,
        seq: u64,
    },
    /// A write/DDL statement committed; `affected` rows changed.
    Executed {
        affected: usize,
    },
    /// A streamed export finished: `rows` rows written to `path`.
    ExportFinished {
        path: String,
        rows: usize,
    },
    Error(String),
}

/// One `FetchRun` shape: how to extend or relocate the grid's resident run of
/// a keyset-keyed result.
#[derive(Debug, Clone, PartialEq)]
pub enum RunFetch {
    /// Rows strictly after `after`, ascending. `None` starts from the result's
    /// first row.
    Forward { after: Option<Value> },
    /// Rows strictly before `before`, delivered descending (the grid prepends
    /// them in arrival order, which restores ascending).
    Backward { before: Value },
    /// Replace the run near row `ordinal`: a key-space interpolated seek when
    /// the key is an integer with known bounds (`estimated` reply), else one
    /// `OFFSET` page (exact, but O(ordinal) — the one-off fallback).
    Jump { ordinal: usize },
}

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

/// The active query's cursor plus the bits needed to drive and abort it.
struct ActiveQuery {
    cursor: Box<dyn QueryCursor>,
    cancel: CancelToken,
    timeout: Option<Duration>,
    streamed: usize,
    started: Instant,
}

/// A cloneable handle that can only *send* commands. Handed to the result grid so
/// its load-on-scroll callback can request pages mid-render without touching the
/// (non-cloneable) `ServiceHandle` or the UI entity.
#[derive(Clone)]
pub struct CommandSender(CmdSender<Command>);

impl CommandSender {
    pub fn send(&self, command: Command) {
        let _ = self.0.send(command);
    }
}

/// The UI's handle on the backend: send commands, take the event stream once.
pub struct ServiceHandle {
    commands: CmdSender<Command>,
    events: Option<UnboundedReceiver<Event>>,
}

impl ServiceHandle {
    /// Fire a command at the backend. Infallible from the caller's view — if the
    /// backend is gone the command is dropped.
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// A cloneable send-only handle (for the result grid's page requests).
    pub fn command_sender(&self) -> CommandSender {
        CommandSender(self.commands.clone())
    }

    /// Take the event stream. Call once; it moves into the UI's async loop.
    pub fn take_events(&mut self) -> Option<UnboundedReceiver<Event>> {
        self.events.take()
    }
}

/// Spawn the backend thread and return its handle. The thread owns a
/// current-thread Tokio runtime; the blocking SQLite work runs on its blocking
/// pool, so the dispatch loop never stalls.
pub fn spawn() -> ServiceHandle {
    let (cmd_tx, cmd_rx) = unbounded_channel::<Command>();
    let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded::<Event>();

    std::thread::Builder::new()
        .name("red-service".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                // I/O enabled too: the Postgres driver's network connection needs it.
                .enable_all()
                .build()
                .expect("build red-service tokio runtime");
            rt.block_on(dispatch(cmd_rx, evt_tx));
        })
        .expect("spawn red-service thread");

    ServiceHandle {
        commands: cmd_tx,
        events: Some(evt_rx),
    }
}

async fn dispatch(mut commands: CmdReceiver<Command>, events: UnboundedSender<Event>) {
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
                results.lock().unwrap().clear();
                match connect(&config).await {
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
                match connect(&config).await {
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
                results.lock().unwrap().clear();
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
                results.lock().unwrap().insert(
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
                            if let Some(spec) = results.lock().unwrap().get_mut(&epoch) {
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
                let Some(sql) = results.lock().unwrap().get(&epoch).map(|s| s.sql.clone()) else {
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
                let Some(spec) = results.lock().unwrap().get(&epoch).cloned() else {
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
                results.lock().unwrap().remove(&epoch);
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
                let Some(sql) = results.lock().unwrap().get(&epoch).map(|s| s.sql.clone()) else {
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
        RunFetch::Jump { ordinal } => {
            // Key-space interpolation: land near `ordinal / total` of the key
            // range in one indexed seek. Approximate (exact only for dense,
            // uniform keys) — the grid renders the run's ordinals with a `≈`.
            if key.kind == KeyKind::Int {
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
mod tests {
    use super::*;
    use futures::StreamExt;
    use red_core::Value;

    fn sqlite(dsn: &str, read_only: bool) -> ConnectionConfig {
        ConnectionConfig {
            name: "scratch".into(),
            kind: DbKind::Sqlite,
            database: dsn.into(),
            read_only,
            ..Default::default()
        }
    }

    fn counting_sql(n: i64) -> String {
        format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < {n}) SELECT x FROM c"
        )
    }

    /// Connect, run a windowed query, and drain it — proving the start → rows →
    /// finished lifecycle and that windows stay bounded (memory flat: only one
    /// window is ever resident).
    #[tokio::test]
    async fn streams_query_in_bounded_windows() {
        let mut handle = spawn();
        let mut events = handle.take_events().expect("event stream");
        handle.send(Command::Connect(sqlite(":memory:", true)));
        assert!(matches!(events.next().await, Some(Event::Connected { .. })));

        handle.send(Command::Query {
            sql: counting_sql(25_000),
            opts: QueryOptions {
                window: 1000,
                timeout: None,
            },
        });

        match events.next().await {
            Some(Event::QueryStarted { columns }) => assert_eq!(columns[0].name, "x"),
            other => panic!("expected QueryStarted, got {other:?}"),
        }

        let mut total = 0usize;
        loop {
            match events.next().await {
                Some(Event::QueryRows(window)) => {
                    assert!(window.rows.len() <= 1000);
                    total += window.rows.len();
                    if !window.exhausted {
                        handle.send(Command::FetchMore { max: 1000 });
                    }
                }
                Some(Event::QueryFinished { rows_streamed, .. }) => {
                    assert_eq!(rows_streamed, 25_000);
                    break;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(total, 25_000);
        handle.send(Command::Shutdown);
    }

    /// Cancel a long-running query mid-flight and get a prompt `QueryCancelled`.
    #[tokio::test]
    async fn cancels_query_mid_flight() {
        let mut handle = spawn();
        let mut events = handle.take_events().expect("event stream");
        handle.send(Command::Connect(sqlite(":memory:", true)));
        assert!(matches!(events.next().await, Some(Event::Connected { .. })));

        handle.send(Command::Query {
            sql: counting_sql(1_000_000_000),
            opts: QueryOptions {
                window: 1_000_000_000,
                timeout: None,
            },
        });
        assert!(matches!(
            events.next().await,
            Some(Event::QueryStarted { .. })
        ));

        // Let the first step get underway, then cancel out-of-band.
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.send(Command::Cancel);

        assert!(matches!(events.next().await, Some(Event::QueryCancelled)));
        handle.send(Command::Shutdown);
    }

    /// A tiny timeout aborts a runaway first step and surfaces an error.
    #[tokio::test]
    async fn query_times_out() {
        let mut handle = spawn();
        let mut events = handle.take_events().expect("event stream");
        handle.send(Command::Connect(sqlite(":memory:", true)));
        assert!(matches!(events.next().await, Some(Event::Connected { .. })));

        handle.send(Command::Query {
            sql: counting_sql(1_000_000_000),
            opts: QueryOptions {
                window: 1_000_000_000,
                timeout: Some(Duration::from_millis(50)),
            },
        });
        assert!(matches!(
            events.next().await,
            Some(Event::QueryStarted { .. })
        ));

        match events.next().await {
            Some(Event::Error(msg)) => assert!(msg.contains("timed out"), "got: {msg}"),
            other => panic!("expected timeout error, got {other:?}"),
        }
        handle.send(Command::Shutdown);
    }

    #[tokio::test]
    async fn disconnect_drops_session() {
        let mut handle = spawn();
        let mut events = handle.take_events().expect("event stream");
        handle.send(Command::Connect(sqlite(":memory:", true)));
        assert!(matches!(events.next().await, Some(Event::Connected { .. })));

        handle.send(Command::Disconnect);
        assert!(matches!(events.next().await, Some(Event::Disconnected)));

        // A query after disconnect must report "not connected", not crash.
        handle.send(Command::Query {
            sql: "SELECT 1".into(),
            opts: QueryOptions::default(),
        });
        match events.next().await {
            Some(Event::Error(msg)) => assert!(msg.contains("not connected")),
            other => panic!("expected not-connected error, got {other:?}"),
        }
        handle.send(Command::Shutdown);
    }

    /// Connect, load the skeleton, then describe a table — the schema-explorer
    /// round-trip the sidebar drives (M3).
    #[tokio::test]
    async fn loads_and_describes_schema() {
        // A unique temp file so the service's own connections see a populated DB.
        let path = std::env::temp_dir().join(format!("red_svc_{}.db", std::process::id()));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                 CREATE VIEW v AS SELECT id FROM t;",
            )
            .unwrap();
        }

        let mut handle = spawn();
        let mut events = handle.take_events().expect("event stream");
        handle.send(Command::Connect(sqlite(path.to_str().unwrap(), true)));
        assert!(matches!(events.next().await, Some(Event::Connected { .. })));

        handle.send(Command::LoadObjects);
        match events.next().await {
            Some(Event::ObjectsLoaded { schemas }) => {
                let main = schemas.iter().find(|s| s.name == "main").unwrap();
                assert!(main.objects.iter().any(|o| o.name == "t"));
                assert!(main.objects.iter().any(|o| o.name == "v"));
            }
            other => panic!("expected ObjectsLoaded, got {other:?}"),
        }

        handle.send(Command::DescribeTable {
            schema: "main".into(),
            table: "t".into(),
        });
        match events.next().await {
            Some(Event::TableDescribed { table, detail, .. }) => {
                assert_eq!(table, "t");
                assert!(detail
                    .columns
                    .iter()
                    .any(|c| c.name == "id" && c.primary_key));
                assert!(detail
                    .columns
                    .iter()
                    .any(|c| c.name == "name" && c.not_null));
            }
            other => panic!("expected TableDescribed, got {other:?}"),
        }

        handle.send(Command::Shutdown);
        std::fs::remove_file(&path).ok();
    }

    /// Open a result and page through it — the grid's load-on-scroll path.
    #[tokio::test]
    async fn opens_and_pages_result() {
        let mut handle = spawn();
        let mut events = handle.take_events().expect("event stream");
        handle.send(Command::Connect(sqlite(":memory:", true)));
        assert!(matches!(events.next().await, Some(Event::Connected { .. })));

        handle.send(Command::OpenResult {
            sql: counting_sql(1000),
            epoch: 1,
            table: None,
        });
        match events.next().await {
            Some(Event::ResultReady {
                columns,
                total,
                epoch,
                key,
            }) => {
                assert_eq!(columns[0].name, "x");
                assert_eq!(total, 1000);
                assert_eq!(epoch, 1);
                assert_eq!(key, None, "editor SQL resolves no seek key");
            }
            other => panic!("expected ResultReady, got {other:?}"),
        }

        handle.send(Command::FetchPage {
            offset: 998,
            limit: 100,
            epoch: 1,
        });
        match events.next().await {
            Some(Event::ResultPageLoaded {
                offset,
                rows,
                epoch,
            }) => {
                assert_eq!(offset, 998);
                assert_eq!(rows.len(), 2); // only rows 999 and 1000 remain
                assert_eq!(rows[0][0], Value::Integer(999));
                assert_eq!(epoch, 1);
            }
            other => panic!("expected ResultPageLoaded, got {other:?}"),
        }
        handle.send(Command::Shutdown);
    }

    /// The keyset path end-to-end: a table browse resolves its PK as the seek
    /// key, contiguous runs extend from boundary keys, and a far jump lands by
    /// key-space interpolation with estimated ordinals.
    #[tokio::test]
    async fn resolves_key_and_serves_runs() {
        use red_core::KeyKind;
        let path = std::env::temp_dir().join(format!("red_svc_runs_{}.db", std::process::id()));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);
                 WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 1000)
                 INSERT INTO t SELECT x, 'row ' || x FROM c;",
            )
            .unwrap();
        }

        let mut handle = spawn();
        let mut events = handle.take_events().expect("event stream");
        handle.send(Command::Connect(sqlite(path.to_str().unwrap(), true)));
        assert!(matches!(events.next().await, Some(Event::Connected { .. })));

        handle.send(Command::OpenResult {
            sql: "SELECT * FROM t".into(),
            epoch: 7,
            table: Some(("main".into(), "t".into())),
        });
        match events.next().await {
            Some(Event::ResultReady { total, key, .. }) => {
                assert_eq!(total, 1000);
                let key = key.expect("table browse resolves its PK");
                assert_eq!(key.column, "id");
                assert_eq!(key.kind, KeyKind::Int);
            }
            other => panic!("expected ResultReady, got {other:?}"),
        }

        // Forward from the start, then forward from a boundary key.
        handle.send(Command::FetchRun {
            epoch: 7,
            fetch: RunFetch::Forward { after: None },
            limit: 3,
            seq: 1,
        });
        match events.next().await {
            Some(Event::ResultRunLoaded {
                rows,
                estimated,
                seq,
                ..
            }) => {
                assert_eq!(seq, 1);
                assert!(!estimated);
                assert_eq!(rows[0][0], Value::Integer(1));
                assert_eq!(rows.len(), 3);
            }
            other => panic!("expected ResultRunLoaded, got {other:?}"),
        }
        handle.send(Command::FetchRun {
            epoch: 7,
            fetch: RunFetch::Forward {
                after: Some(Value::Integer(3)),
            },
            limit: 3,
            seq: 2,
        });
        match events.next().await {
            Some(Event::ResultRunLoaded { rows, .. }) => {
                assert_eq!(rows[0][0], Value::Integer(4));
            }
            other => panic!("expected ResultRunLoaded, got {other:?}"),
        }

        // Backward: rows strictly before the bound, delivered descending.
        handle.send(Command::FetchRun {
            epoch: 7,
            fetch: RunFetch::Backward {
                before: Value::Integer(4),
            },
            limit: 5,
            seq: 3,
        });
        match events.next().await {
            Some(Event::ResultRunLoaded { rows, .. }) => {
                assert_eq!(
                    rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
                    vec![Value::Integer(3), Value::Integer(2), Value::Integer(1)]
                );
            }
            other => panic!("expected ResultRunLoaded, got {other:?}"),
        }

        // A far jump interpolates the key space: ~halfway lands near id 500,
        // flagged estimated.
        handle.send(Command::FetchRun {
            epoch: 7,
            fetch: RunFetch::Jump { ordinal: 499 },
            limit: 3,
            seq: 4,
        });
        match events.next().await {
            Some(Event::ResultRunLoaded {
                rows, estimated, ..
            }) => {
                assert!(estimated, "interpolated jump reports estimated ordinals");
                match &rows[0][0] {
                    Value::Integer(id) => {
                        assert!((495..=505).contains(id), "landed at id {id}")
                    }
                    other => panic!("expected an integer id, got {other:?}"),
                }
            }
            other => panic!("expected ResultRunLoaded, got {other:?}"),
        }

        // A jump to ordinal 0 seeks from the true start — exact, not estimated.
        handle.send(Command::FetchRun {
            epoch: 7,
            fetch: RunFetch::Jump { ordinal: 0 },
            limit: 3,
            seq: 5,
        });
        match events.next().await {
            Some(Event::ResultRunLoaded {
                rows, estimated, ..
            }) => {
                assert!(!estimated);
                assert_eq!(rows[0][0], Value::Integer(1));
            }
            other => panic!("expected ResultRunLoaded, got {other:?}"),
        }

        handle.send(Command::Shutdown);
        std::fs::remove_file(&path).ok();
    }

    /// The full app flow against a live MariaDB/MySQL (`RED_TEST_MYSQL_URL`,
    /// skipped without one): open a 1M-row table browse, resolve the PK as the
    /// seek key, then deep-seek and jump — each fetch must come back fast
    /// (indexed), proving the derived-table wrapper merges on the server.
    #[tokio::test]
    async fn mariadb_keyset_end_to_end() {
        use red_core::KeyKind;
        let Ok(url) = std::env::var("RED_TEST_MYSQL_URL") else {
            return;
        };
        let p = ConnectionConfig::parse_conn_str(&url).expect("parsable test url");
        let config = ConnectionConfig {
            name: "maria-test".into(),
            kind: DbKind::Mysql,
            host: p.host,
            port: p.port,
            user: p.user,
            password: p.password,
            database: p.database.clone(),
            ..Default::default()
        };
        let table = format!("red_keyset_{}", std::process::id());

        let mut handle = spawn();
        let mut events = handle.take_events().expect("event stream");
        handle.send(Command::Connect(config));
        assert!(matches!(events.next().await, Some(Event::Connected { .. })));

        // Seed 1M rows (MariaDB's sequence engine; the test targets MariaDB).
        handle.send(Command::Execute {
            sql: format!("CREATE TABLE `{table}`(id BIGINT PRIMARY KEY, name VARCHAR(64))"),
        });
        assert!(matches!(events.next().await, Some(Event::Executed { .. })));
        handle.send(Command::Execute {
            sql: format!(
                "INSERT INTO `{table}` SELECT seq, CONCAT('row ', seq) FROM seq_1_to_1000000"
            ),
        });
        match events.next().await {
            Some(Event::Executed { affected }) => assert_eq!(affected, 1_000_000),
            other => panic!("seeding failed: {other:?}"),
        }

        handle.send(Command::OpenResult {
            sql: format!("SELECT * FROM `{}`.`{table}`", p.database),
            epoch: 1,
            table: Some((p.database.clone(), table.clone())),
        });
        match events.next().await {
            Some(Event::ResultReady { total, key, .. }) => {
                assert_eq!(total, 1_000_000);
                let key = key.expect("PK resolves as the seek key");
                assert_eq!(key.column, "id");
                assert_eq!(key.kind, KeyKind::Int);
            }
            other => panic!("expected ResultReady, got {other:?}"),
        }

        // Deep forward seek near the bottom — must be indexed-fast.
        let started = Instant::now();
        handle.send(Command::FetchRun {
            epoch: 1,
            fetch: RunFetch::Forward {
                after: Some(Value::Integer(999_000)),
            },
            limit: 200,
            seq: 1,
        });
        match events.next().await {
            Some(Event::ResultRunLoaded { rows, .. }) => {
                assert_eq!(rows.len(), 200);
                assert_eq!(rows[0][0], Value::Integer(999_001));
            }
            other => panic!("expected ResultRunLoaded, got {other:?}"),
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "deep seek took {elapsed:?} — the derived-table wrapper isn't merging"
        );

        // Interpolated jump to ~50%.
        handle.send(Command::FetchRun {
            epoch: 1,
            fetch: RunFetch::Jump { ordinal: 500_000 },
            limit: 200,
            seq: 2,
        });
        match events.next().await {
            Some(Event::ResultRunLoaded {
                rows, estimated, ..
            }) => {
                assert!(estimated);
                match &rows[0][0] {
                    Value::Integer(id) => {
                        assert!((499_000..=501_000).contains(id), "jump landed at id {id}")
                    }
                    other => panic!("expected integer id, got {other:?}"),
                }
            }
            other => panic!("expected ResultRunLoaded, got {other:?}"),
        }

        // Backward from the middle (scrolling up).
        handle.send(Command::FetchRun {
            epoch: 1,
            fetch: RunFetch::Backward {
                before: Value::Integer(500_000),
            },
            limit: 200,
            seq: 3,
        });
        match events.next().await {
            Some(Event::ResultRunLoaded { rows, .. }) => {
                assert_eq!(rows[0][0], Value::Integer(499_999));
                assert_eq!(rows.len(), 200);
            }
            other => panic!("expected ResultRunLoaded, got {other:?}"),
        }

        handle.send(Command::Execute {
            sql: format!("DROP TABLE `{table}`"),
        });
        assert!(matches!(events.next().await, Some(Event::Executed { .. })));
        handle.send(Command::Shutdown);
    }

    /// A non-interpolable (text) key still gets keyset scroll, and its jumps
    /// fall back to one exact `OFFSET` page.
    #[tokio::test]
    async fn text_key_jump_falls_back_to_offset() {
        use red_core::KeyKind;
        let path = std::env::temp_dir().join(format!("red_svc_text_{}.db", std::process::id()));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE t(code TEXT PRIMARY KEY NOT NULL);
                 WITH RECURSIVE c(x) AS (SELECT 100 UNION ALL SELECT x+1 FROM c WHERE x < 199)
                 INSERT INTO t SELECT 'c' || x FROM c;",
            )
            .unwrap();
        }

        let mut handle = spawn();
        let mut events = handle.take_events().expect("event stream");
        handle.send(Command::Connect(sqlite(path.to_str().unwrap(), true)));
        assert!(matches!(events.next().await, Some(Event::Connected { .. })));

        handle.send(Command::OpenResult {
            sql: "SELECT * FROM t".into(),
            epoch: 9,
            table: Some(("main".into(), "t".into())),
        });
        match events.next().await {
            Some(Event::ResultReady { key, .. }) => {
                assert_eq!(key.map(|k| k.kind), Some(KeyKind::Other));
            }
            other => panic!("expected ResultReady, got {other:?}"),
        }

        handle.send(Command::FetchRun {
            epoch: 9,
            fetch: RunFetch::Jump { ordinal: 50 },
            limit: 5,
            seq: 1,
        });
        match events.next().await {
            Some(Event::ResultRunLoaded {
                rows, estimated, ..
            }) => {
                assert!(!estimated, "OFFSET fallback is exact");
                assert_eq!(rows.len(), 5);
            }
            other => panic!("expected ResultRunLoaded, got {other:?}"),
        }

        handle.send(Command::Shutdown);
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn connect_and_query_roundtrip() {
        let mut handle = spawn();
        let mut events = handle.take_events().expect("event stream");
        handle.send(Command::Connect(sqlite(":memory:", false)));
        handle.send(Command::Query {
            sql: "SELECT 42 AS answer".into(),
            opts: QueryOptions::default(),
        });

        assert!(matches!(events.next().await, Some(Event::Connected { .. })));
        assert!(matches!(
            events.next().await,
            Some(Event::QueryStarted { .. })
        ));
        match events.next().await {
            Some(Event::QueryRows(window)) => {
                assert_eq!(window.rows[0][0], Value::Integer(42));
                assert!(window.exhausted);
            }
            other => panic!("expected QueryRows, got {other:?}"),
        }
        assert!(matches!(
            events.next().await,
            Some(Event::QueryFinished { .. })
        ));
        handle.send(Command::Shutdown);
    }
}
