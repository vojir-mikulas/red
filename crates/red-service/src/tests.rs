use super::*;
use futures::StreamExt;
use std::time::{Duration, Instant};

use red_core::{ConnectionConfig, DbKind, QueryOptions, Value};

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
        sort: None,
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
        sort: None,
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
            after: Some(vec![Value::Integer(3)]),
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
            before: vec![Value::Integer(4)],
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
        fetch: RunFetch::Jump {
            ordinal: 499,
            exact: false,
        },
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
        fetch: RunFetch::Jump {
            ordinal: 0,
            exact: false,
        },
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
        sql: format!("INSERT INTO `{table}` SELECT seq, CONCAT('row ', seq) FROM seq_1_to_1000000"),
    });
    match events.next().await {
        Some(Event::Executed { affected }) => assert_eq!(affected, 1_000_000),
        other => panic!("seeding failed: {other:?}"),
    }

    handle.send(Command::OpenResult {
        sql: format!("SELECT * FROM `{}`.`{table}`", p.database),
        epoch: 1,
        table: Some((p.database.clone(), table.clone())),
        sort: None,
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
            after: Some(vec![Value::Integer(999_000)]),
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

    // Interpolated jump to ~50% — fast but approximate, flagged estimated.
    handle.send(Command::FetchRun {
        epoch: 1,
        fetch: RunFetch::Jump {
            ordinal: 500_000,
            exact: false,
        },
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

    // Exact jump to row 500_000 ("go to row N"): interpolation is skipped, so
    // ordinals are exact (not estimated) and the row is precisely id 500_001
    // (ids are 1-based; ordinal 500_000 is the 500_001st row).
    handle.send(Command::FetchRun {
        epoch: 1,
        fetch: RunFetch::Jump {
            ordinal: 500_000,
            exact: true,
        },
        limit: 200,
        seq: 3,
    });
    match events.next().await {
        Some(Event::ResultRunLoaded {
            rows, estimated, ..
        }) => {
            assert!(!estimated, "an exact jump reports exact ordinals");
            assert_eq!(rows[0][0], Value::Integer(500_001));
        }
        other => panic!("expected ResultRunLoaded, got {other:?}"),
    }

    // Backward from the middle (scrolling up).
    handle.send(Command::FetchRun {
        epoch: 1,
        fetch: RunFetch::Backward {
            before: vec![Value::Integer(500_000)],
        },
        limit: 200,
        seq: 4,
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
        sort: None,
    });
    match events.next().await {
        Some(Event::ResultReady { key, .. }) => {
            assert_eq!(key.map(|k| k.kind), Some(KeyKind::Other));
        }
        other => panic!("expected ResultReady, got {other:?}"),
    }

    handle.send(Command::FetchRun {
        epoch: 9,
        fetch: RunFetch::Jump {
            ordinal: 50,
            exact: false,
        },
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
