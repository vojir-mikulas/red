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

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use red_core::{
    Column, ConnectionConfig, DbKind, QueryOptions, RedError, RowWindow, SchemaMeta, TableDetail,
};
use red_driver::{CancelToken, DatabaseDriver, QueryCursor, SqliteDriver};
use tokio::sync::mpsc::{
    unbounded_channel, UnboundedReceiver as CmdReceiver, UnboundedSender as CmdSender,
};

/// UI → service. One active session at a time, driven across many commands.
#[derive(Debug)]
pub enum Command {
    Connect(ConnectionConfig),
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
    Error(String),
}

/// The active query's cursor plus the bits needed to drive and abort it.
struct ActiveQuery {
    cursor: Box<dyn QueryCursor>,
    cancel: CancelToken,
    timeout: Option<Duration>,
    streamed: usize,
    started: Instant,
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
                .enable_time()
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

    while let Some(command) = commands.recv().await {
        match command {
            Command::Connect(config) => {
                active = None; // a new connection abandons any in-flight cursor
                match connect(&config).await {
                    Ok(driver) => {
                        let version = driver.server_version();
                        session = Some(driver);
                        emit(&events, Event::Connected { version });
                    }
                    Err(message) => emit(&events, Event::Error(message)),
                }
            }

            Command::Disconnect => {
                active = None;
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
            let driver = SqliteDriver::new(config.dsn.clone(), config.read_only);
            driver.ping().await.map_err(|e| e.to_string())?;
            Ok(Arc::new(driver))
        }
        DbKind::Postgres => Err("Postgres driver not yet implemented".into()),
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
            dsn: dsn.into(),
            read_only,
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
