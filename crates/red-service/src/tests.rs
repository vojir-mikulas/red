use super::*;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use std::time::{Duration, Instant};

use red_core::{ConnectionConfig, DbKind, QueryOptions, Value};

/// The session every single-session test drives. Real multi-session routing is
/// exercised by [`keeps_two_sessions_warm`].
const S: SessionId = SessionId(1);

/// Fire a command at the default test session (the channel now routes by id).
fn send(handle: &ServiceHandle, command: Command) {
    handle.send_to(S, command);
}

/// Pull the next event, dropping the session tag the single-session tests ignore.
async fn next(events: &mut UnboundedReceiver<(Option<SessionId>, Event)>) -> Option<Event> {
    events.next().await.map(|(_, e)| e)
}

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
    send(&handle, Command::Connect(sqlite(":memory:", true)));
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    send(
        &handle,
        Command::Query {
            sql: counting_sql(25_000),
            opts: QueryOptions {
                window: 1000,
                timeout: None,
            },
        },
    );

    match next(&mut events).await {
        Some(Event::QueryStarted { columns }) => assert_eq!(columns[0].name, "x"),
        other => panic!("expected QueryStarted, got {other:?}"),
    }

    let mut total = 0usize;
    loop {
        match next(&mut events).await {
            Some(Event::QueryRows(window)) => {
                assert!(window.rows.len() <= 1000);
                total += window.rows.len();
                if !window.exhausted {
                    send(&handle, Command::FetchMore { max: 1000 });
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
    send(&handle, Command::Shutdown);
}

/// Cancel a long-running query mid-flight and get a prompt `QueryCancelled`.
#[tokio::test]
async fn cancels_query_mid_flight() {
    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");
    send(&handle, Command::Connect(sqlite(":memory:", true)));
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    send(
        &handle,
        Command::Query {
            sql: counting_sql(1_000_000_000),
            opts: QueryOptions {
                window: 1_000_000_000,
                timeout: None,
            },
        },
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::QueryStarted { .. })
    ));

    // Let the first step get underway, then cancel out-of-band.
    tokio::time::sleep(Duration::from_millis(100)).await;
    send(&handle, Command::Cancel);

    assert!(matches!(
        next(&mut events).await,
        Some(Event::QueryCancelled)
    ));
    send(&handle, Command::Shutdown);
}

/// A tiny timeout aborts a runaway first step and surfaces an error.
#[tokio::test]
async fn query_times_out() {
    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");
    send(&handle, Command::Connect(sqlite(":memory:", true)));
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    send(
        &handle,
        Command::Query {
            sql: counting_sql(1_000_000_000),
            opts: QueryOptions {
                window: 1_000_000_000,
                timeout: Some(Duration::from_millis(50)),
            },
        },
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::QueryStarted { .. })
    ));

    match next(&mut events).await {
        Some(Event::Error(msg)) => assert!(msg.contains("timed out"), "got: {msg}"),
        other => panic!("expected timeout error, got {other:?}"),
    }
    send(&handle, Command::Shutdown);
}

#[tokio::test]
async fn disconnect_drops_session() {
    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");
    send(&handle, Command::Connect(sqlite(":memory:", true)));
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    send(&handle, Command::Disconnect);
    assert!(matches!(next(&mut events).await, Some(Event::Disconnected)));

    // A query after disconnect must report "not connected", not crash.
    send(
        &handle,
        Command::Query {
            sql: "SELECT 1".into(),
            opts: QueryOptions::default(),
        },
    );
    match next(&mut events).await {
        Some(Event::Error(msg)) => assert!(msg.contains("not connected")),
        other => panic!("expected not-connected error, got {other:?}"),
    }
    send(&handle, Command::Shutdown);
}

/// Connect, load the skeleton, then describe a table — the schema-explorer
/// round-trip the sidebar drives.
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
    send(
        &handle,
        Command::Connect(sqlite(path.to_str().unwrap(), true)),
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    send(&handle, Command::LoadObjects);
    match next(&mut events).await {
        Some(Event::ObjectsLoaded { schemas }) => {
            let main = schemas.iter().find(|s| s.name == "main").unwrap();
            assert!(main.objects.iter().any(|o| o.name == "t"));
            assert!(main.objects.iter().any(|o| o.name == "v"));
        }
        other => panic!("expected ObjectsLoaded, got {other:?}"),
    }

    send(
        &handle,
        Command::DescribeTable {
            schema: "main".into(),
            table: "t".into(),
        },
    );
    match next(&mut events).await {
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

    send(&handle, Command::Shutdown);
    std::fs::remove_file(&path).ok();
}

/// Explain a query: the `Explain` command replies with a `PlanReady` carrying a
/// non-empty plan and echoing the request epoch (Track B4). A bad statement comes
/// back as a pane-local `PlanFailed`, not a global `Error`.
#[tokio::test]
async fn explains_a_query() {
    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");
    send(&handle, Command::Connect(sqlite(":memory:", true)));
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    send(
        &handle,
        Command::Explain {
            sql: counting_sql(10),
            analyze: false,
            epoch: 7,
        },
    );
    match next(&mut events).await {
        Some(Event::PlanReady { epoch, plan }) => {
            assert_eq!(epoch, 7, "the request epoch is echoed");
            assert!(!plan.nodes.is_empty(), "a plan node was parsed");
            assert!(!plan.raw.is_empty(), "raw EXPLAIN text is present");
            assert!(!plan.analyzed);
        }
        other => panic!("expected PlanReady, got {other:?}"),
    }

    // A syntactically broken statement fails *in the plan pane*.
    send(
        &handle,
        Command::Explain {
            sql: "SELECT FROM WHERE".into(),
            analyze: false,
            epoch: 8,
        },
    );
    match next(&mut events).await {
        Some(Event::PlanFailed { epoch, message }) => {
            assert_eq!(epoch, 8);
            assert!(!message.is_empty());
        }
        other => panic!("expected PlanFailed, got {other:?}"),
    }
}

/// Apply a guarded data edit (Track B5): `ApplyEdit` on a writable session replies
/// `EditApplied` echoing the result epoch, and an edit that matches no row comes
/// back as a pane-local `EditFailed`, not a global `Error`.
#[tokio::test]
async fn applies_a_data_edit() {
    use red_core::{ColumnValue, EditOp, TableRef};
    let path = std::env::temp_dir().join(format!("red_svc_edit_{}.db", std::process::id()));
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO t VALUES (1, 'one');",
        )
        .unwrap();
    }

    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");
    send(
        &handle,
        Command::Connect(sqlite(path.to_str().unwrap(), false)),
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    let edit = |id: i64| EditOp::Update {
        table: TableRef {
            schema: Some("main".into()),
            name: "t".into(),
        },
        key: ColumnValue {
            column: "id".into(),
            value: Value::Integer(id),
        },
        set: vec![ColumnValue {
            column: "name".into(),
            value: Value::Text("two".into()),
        }],
    };

    send(
        &handle,
        Command::ApplyEdit {
            epoch: 4,
            op: edit(1),
        },
    );
    match next(&mut events).await {
        Some(Event::EditApplied { epoch, affected }) => {
            assert_eq!(epoch, 4, "the result epoch is echoed");
            assert_eq!(affected, 1);
        }
        other => panic!("expected EditApplied, got {other:?}"),
    }

    // An edit whose key matches no row fails *in the pane*, rolled back.
    send(
        &handle,
        Command::ApplyEdit {
            epoch: 5,
            op: edit(9999),
        },
    );
    match next(&mut events).await {
        Some(Event::EditFailed { epoch, message }) => {
            assert_eq!(epoch, 5);
            assert!(!message.is_empty());
        }
        other => panic!("expected EditFailed, got {other:?}"),
    }

    send(&handle, Command::Shutdown);
    std::fs::remove_file(&path).ok();
}

/// Open a result and page through it — the grid's load-on-scroll path.
#[tokio::test]
async fn opens_and_pages_result() {
    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");
    send(&handle, Command::Connect(sqlite(":memory:", true)));
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    send(
        &handle,
        Command::OpenResult {
            sql: counting_sql(1000),
            epoch: 1,
            table: None,
            sort: None,
            filter: None,
        },
    );
    match next(&mut events).await {
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

    send(
        &handle,
        Command::FetchPage {
            offset: 998,
            limit: 100,
            epoch: 1,
        },
    );
    match next(&mut events).await {
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
    send(&handle, Command::Shutdown);
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
    send(
        &handle,
        Command::Connect(sqlite(path.to_str().unwrap(), true)),
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    send(
        &handle,
        Command::OpenResult {
            sql: "SELECT * FROM t".into(),
            epoch: 7,
            table: Some(("main".into(), "t".into())),
            sort: None,
            filter: None,
        },
    );
    match next(&mut events).await {
        Some(Event::ResultReady { total, key, .. }) => {
            assert_eq!(total, 1000);
            let key = key.expect("table browse resolves its PK");
            assert_eq!(key.column, "id");
            assert_eq!(key.kind, KeyKind::Int);
        }
        other => panic!("expected ResultReady, got {other:?}"),
    }

    // Forward from the start, then forward from a boundary key.
    send(
        &handle,
        Command::FetchRun {
            epoch: 7,
            fetch: RunFetch::Forward { after: None },
            limit: 3,
            seq: 1,
        },
    );
    match next(&mut events).await {
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
    send(
        &handle,
        Command::FetchRun {
            epoch: 7,
            fetch: RunFetch::Forward {
                after: Some(vec![Value::Integer(3)]),
            },
            limit: 3,
            seq: 2,
        },
    );
    match next(&mut events).await {
        Some(Event::ResultRunLoaded { rows, .. }) => {
            assert_eq!(rows[0][0], Value::Integer(4));
        }
        other => panic!("expected ResultRunLoaded, got {other:?}"),
    }

    // Backward: rows strictly before the bound, delivered descending.
    send(
        &handle,
        Command::FetchRun {
            epoch: 7,
            fetch: RunFetch::Backward {
                before: vec![Value::Integer(4)],
            },
            limit: 5,
            seq: 3,
        },
    );
    match next(&mut events).await {
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
    send(
        &handle,
        Command::FetchRun {
            epoch: 7,
            fetch: RunFetch::Jump {
                ordinal: 499,
                exact: false,
            },
            limit: 3,
            seq: 4,
        },
    );
    match next(&mut events).await {
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
    send(
        &handle,
        Command::FetchRun {
            epoch: 7,
            fetch: RunFetch::Jump {
                ordinal: 0,
                exact: false,
            },
            limit: 3,
            seq: 5,
        },
    );
    match next(&mut events).await {
        Some(Event::ResultRunLoaded {
            rows, estimated, ..
        }) => {
            assert!(!estimated);
            assert_eq!(rows[0][0], Value::Integer(1));
        }
        other => panic!("expected ResultRunLoaded, got {other:?}"),
    }

    send(&handle, Command::Shutdown);
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
    send(&handle, Command::Connect(config));
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    // Seed 1M rows (MariaDB's sequence engine; the test targets MariaDB).
    send(
        &handle,
        Command::Execute {
            sql: format!("CREATE TABLE `{table}`(id BIGINT PRIMARY KEY, name VARCHAR(64))"),
        },
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Executed { .. })
    ));
    send(
        &handle,
        Command::Execute {
            sql: format!(
                "INSERT INTO `{table}` SELECT seq, CONCAT('row ', seq) FROM seq_1_to_1000000"
            ),
        },
    );
    match next(&mut events).await {
        Some(Event::Executed { affected }) => assert_eq!(affected, 1_000_000),
        other => panic!("seeding failed: {other:?}"),
    }

    send(
        &handle,
        Command::OpenResult {
            sql: format!("SELECT * FROM `{}`.`{table}`", p.database),
            epoch: 1,
            table: Some((p.database.clone(), table.clone())),
            sort: None,
            filter: None,
        },
    );
    match next(&mut events).await {
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
    send(
        &handle,
        Command::FetchRun {
            epoch: 1,
            fetch: RunFetch::Forward {
                after: Some(vec![Value::Integer(999_000)]),
            },
            limit: 200,
            seq: 1,
        },
    );
    match next(&mut events).await {
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

    // Interpolated jump to ~50% — fast but approximate, flagged estimated.
    send(
        &handle,
        Command::FetchRun {
            epoch: 1,
            fetch: RunFetch::Jump {
                ordinal: 500_000,
                exact: false,
            },
            limit: 200,
            seq: 2,
        },
    );
    match next(&mut events).await {
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

    // Exact jump to row 500_000 ("go to row N"): interpolation is skipped, so
    // ordinals are exact (not estimated) and the row is precisely id 500_001
    // (ids are 1-based; ordinal 500_000 is the 500_001st row).
    send(
        &handle,
        Command::FetchRun {
            epoch: 1,
            fetch: RunFetch::Jump {
                ordinal: 500_000,
                exact: true,
            },
            limit: 200,
            seq: 3,
        },
    );
    match next(&mut events).await {
        Some(Event::ResultRunLoaded {
            rows, estimated, ..
        }) => {
            assert!(!estimated, "an exact jump reports exact ordinals");
            assert_eq!(rows[0][0], Value::Integer(500_001));
        }
        other => panic!("expected ResultRunLoaded, got {other:?}"),
    }

    // Backward from the middle (scrolling up).
    send(
        &handle,
        Command::FetchRun {
            epoch: 1,
            fetch: RunFetch::Backward {
                before: vec![Value::Integer(500_000)],
            },
            limit: 200,
            seq: 4,
        },
    );
    match next(&mut events).await {
        Some(Event::ResultRunLoaded { rows, .. }) => {
            assert_eq!(rows[0][0], Value::Integer(499_999));
            assert_eq!(rows.len(), 200);
        }
        other => panic!("expected ResultRunLoaded, got {other:?}"),
    }

    send(
        &handle,
        Command::Execute {
            sql: format!("DROP TABLE `{table}`"),
        },
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Executed { .. })
    ));
    send(&handle, Command::Shutdown);
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
    send(
        &handle,
        Command::Connect(sqlite(path.to_str().unwrap(), true)),
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    send(
        &handle,
        Command::OpenResult {
            sql: "SELECT * FROM t".into(),
            epoch: 9,
            table: Some(("main".into(), "t".into())),
            sort: None,
            filter: None,
        },
    );
    match next(&mut events).await {
        Some(Event::ResultReady { key, .. }) => {
            assert_eq!(key.map(|k| k.kind), Some(KeyKind::Other));
        }
        other => panic!("expected ResultReady, got {other:?}"),
    }

    send(
        &handle,
        Command::FetchRun {
            epoch: 9,
            fetch: RunFetch::Jump {
                ordinal: 50,
                exact: false,
            },
            limit: 5,
            seq: 1,
        },
    );
    match next(&mut events).await {
        Some(Event::ResultRunLoaded {
            rows, estimated, ..
        }) => {
            assert!(!estimated, "OFFSET fallback is exact");
            assert_eq!(rows.len(), 5);
        }
        other => panic!("expected ResultRunLoaded, got {other:?}"),
    }

    send(&handle, Command::Shutdown);
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn connect_and_query_roundtrip() {
    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");
    send(&handle, Command::Connect(sqlite(":memory:", false)));
    send(
        &handle,
        Command::Query {
            sql: "SELECT 42 AS answer".into(),
            opts: QueryOptions::default(),
        },
    );

    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));
    assert!(matches!(
        next(&mut events).await,
        Some(Event::QueryStarted { .. })
    ));
    match next(&mut events).await {
        Some(Event::QueryRows(window)) => {
            assert_eq!(window.rows[0][0], Value::Integer(42));
            assert!(window.exhausted);
        }
        other => panic!("expected QueryRows, got {other:?}"),
    }
    assert!(matches!(
        next(&mut events).await,
        Some(Event::QueryFinished { .. })
    ));
    send(&handle, Command::Shutdown);
}

/// Two connections stay warm at once, each owning its own driver and per-session
/// epoch space; dropping one leaves the other serving (no reconnect), and a
/// command to the dropped session reports "not connected" rather than crashing.
#[tokio::test]
async fn keeps_two_sessions_warm() {
    let a = SessionId(1);
    let b = SessionId(2);
    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");

    handle.send_to(a, Command::Connect(sqlite(":memory:", true)));
    assert!(matches!(
        events.next().await,
        Some((Some(s), Event::Connected { .. })) if s == a
    ));
    handle.send_to(b, Command::Connect(sqlite(":memory:", true)));
    assert!(matches!(
        events.next().await,
        Some((Some(s), Event::Connected { .. })) if s == b
    ));

    // Same epoch in each session — epochs are per-session, not global.
    handle.send_to(
        a,
        Command::OpenResult {
            sql: counting_sql(100),
            epoch: 1,
            table: None,
            sort: None,
            filter: None,
        },
    );
    handle.send_to(
        b,
        Command::OpenResult {
            sql: counting_sql(200),
            epoch: 1,
            table: None,
            sort: None,
            filter: None,
        },
    );

    // The two opens probe off the loop, so their replies can interleave — route
    // each by its session tag and assert it landed on the right one.
    let mut totals = std::collections::HashMap::new();
    while totals.len() < 2 {
        match events.next().await {
            Some((Some(s), Event::ResultReady { total, epoch, .. })) => {
                assert_eq!(epoch, 1);
                totals.insert(s, total);
            }
            other => panic!("expected ResultReady, got {other:?}"),
        }
    }
    assert_eq!(totals[&a], 100);
    assert_eq!(totals[&b], 200);

    // Dropping session A leaves B warm and serving — no reconnect.
    handle.send_to(a, Command::Disconnect);
    assert!(matches!(
        events.next().await,
        Some((Some(s), Event::Disconnected)) if s == a
    ));
    handle.send_to(
        b,
        Command::FetchPage {
            offset: 0,
            limit: 5,
            epoch: 1,
        },
    );
    match events.next().await {
        Some((Some(s), Event::ResultPageLoaded { rows, .. })) => {
            assert_eq!(s, b);
            assert_eq!(rows[0][0], Value::Integer(1));
        }
        other => panic!("expected ResultPageLoaded from B, got {other:?}"),
    }

    // A command to the dropped session A reports not-connected, not a crash.
    handle.send_to(a, Command::LoadObjects);
    match events.next().await {
        Some((Some(s), Event::Error(msg))) => {
            assert_eq!(s, a);
            assert!(msg.contains("not connected"), "got: {msg}");
        }
        other => panic!("expected not-connected error from A, got {other:?}"),
    }

    handle.send_to(b, Command::Shutdown);
}

#[test]
fn panic_message_recovers_common_payloads() {
    // `&str` and `String` panic payloads round-trip; anything else degrades to a
    // placeholder rather than losing the crash report entirely.
    let from_str = std::panic::catch_unwind(|| panic!("boom")).unwrap_err();
    assert_eq!(panic_message(from_str.as_ref()), "boom");

    let owned = String::from("owned boom");
    let from_string = std::panic::catch_unwind(move || panic!("{owned}")).unwrap_err();
    assert_eq!(panic_message(from_string.as_ref()), "owned boom");

    let from_other = std::panic::catch_unwind(|| std::panic::panic_any(42u8)).unwrap_err();
    assert_eq!(panic_message(from_other.as_ref()), "unknown panic");
}

#[test]
fn lock_recovers_from_a_poisoned_mutex() {
    use crate::dispatch::lock;
    use std::sync::{Arc, Mutex};

    let m = Arc::new(Mutex::new(0u32));
    let poisoner = Arc::clone(&m);
    // Poison the mutex by panicking while holding the guard.
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.lock().unwrap();
        panic!("poison");
    })
    .join();
    assert!(m.lock().is_err(), "mutex should be poisoned");

    // The helper still hands back a usable guard despite the poison.
    *lock(&m) += 1;
    assert_eq!(*lock(&m), 1);
}
