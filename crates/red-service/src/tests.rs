use super::*;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use std::time::{Duration, Instant};

use red_core::{ConnectionConfig, DbKind, FkJoin, QueryOptions, Value};

/// The session every single-session test drives. Real multi-session routing is
/// exercised by [`keeps_two_sessions_warm`].
const S: SessionId = SessionId::new(1);

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

/// Connect, run a windowed query, and drain it, proving the start → rows →
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
                full_fidelity: false,
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
                full_fidelity: false,
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
                full_fidelity: false,
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

/// The headless `red mcp` transport (`AiToolList` + `AiToolCall`): list the tool
/// catalog and run a guarded `run_select`, the same round-trip the CLI stdio pump
/// wraps. Asserts the catalog withholds writes and GUI-only tools, and that a
/// select runs against the driver.
#[tokio::test]
async fn mcp_tool_list_and_call_round_trip() {
    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");
    send(&handle, Command::Connect(sqlite(":memory:", true)));
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    // tools/list → catalog of headless-safe read tools only.
    send(&handle, Command::AiToolList { call_id: 1 });
    let tools_json = match next(&mut events).await {
        Some(Event::AiToolCatalog {
            call_id,
            tools_json,
        }) => {
            assert_eq!(call_id, 1);
            tools_json
        }
        other => panic!("expected AiToolCatalog, got {other:?}"),
    };
    let tools: serde_json::Value = serde_json::from_str(&tools_json).unwrap();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(names.contains(&"run_select"), "reads exposed: {names:?}");
    assert!(names.contains(&"list_schema"));
    // Writes and GUI-only tools are withheld headless.
    assert!(!names.contains(&"propose_write"));
    assert!(!names.contains(&"open_query"));
    assert!(!names.contains(&"save_query"));
    assert!(!names.contains(&"generate_report"));

    // tools/call run_select → a non-error result carrying the value.
    send(
        &handle,
        Command::AiToolCall {
            call_id: 2,
            name: "run_select".into(),
            input: r#"{"sql":"SELECT 1 AS one"}"#.into(),
        },
    );
    match next(&mut events).await {
        Some(Event::AiToolResult {
            call_id,
            text,
            is_error,
        }) => {
            assert_eq!(call_id, 2);
            assert!(!is_error, "run_select errored: {text}");
            assert!(text.contains('1'), "result missing value: {text}");
        }
        other => panic!("expected AiToolResult, got {other:?}"),
    }

    // A withheld write tool is refused in-band (a recoverable tool error), never
    // reaching the driver.
    send(
        &handle,
        Command::AiToolCall {
            call_id: 3,
            name: "propose_write".into(),
            input: "{}".into(),
        },
    );
    match next(&mut events).await {
        Some(Event::AiToolResult {
            call_id, is_error, ..
        }) => {
            assert_eq!(call_id, 3);
            assert!(is_error, "a write tool must be refused headless");
        }
        other => panic!("expected AiToolResult, got {other:?}"),
    }
    send(&handle, Command::Shutdown);
}

/// Connect, load the skeleton, then describe a table: the schema-explorer
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
            assert!(
                detail
                    .columns
                    .iter()
                    .any(|c| c.name == "id" && c.primary_key)
            );
            assert!(
                detail
                    .columns
                    .iter()
                    .any(|c| c.name == "name" && c.not_null)
            );
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
            epoch: crate::Epoch::new(7),
        },
    );
    match next(&mut events).await {
        Some(Event::PlanReady { epoch, plan }) => {
            assert_eq!(epoch.get(), 7, "the request epoch is echoed");
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
            epoch: crate::Epoch::new(8),
        },
    );
    match next(&mut events).await {
        Some(Event::PlanFailed { epoch, message }) => {
            assert_eq!(epoch.get(), 8);
            assert!(!message.is_empty());
        }
        other => panic!("expected PlanFailed, got {other:?}"),
    }
}

/// Apply a guarded edit batch (Track B6): `ApplyBatch` on a writable session replies
/// `BatchApplied` echoing the result epoch, and a batch whose op matches no row comes
/// back as a pane-local `BatchFailed`, not a global `Error`.
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
            decl_type: None,
        },
        set: vec![ColumnValue {
            column: "name".into(),
            value: Value::Text("two".into()),
            decl_type: None,
        }],
    };

    send(
        &handle,
        Command::ApplyBatch {
            epoch: crate::Epoch::new(4),
            ops: vec![edit(1)],
        },
    );
    match next(&mut events).await {
        Some(Event::BatchApplied { epoch, applied }) => {
            assert_eq!(epoch.get(), 4, "the result epoch is echoed");
            assert_eq!(applied, 1);
        }
        other => panic!("expected BatchApplied, got {other:?}"),
    }

    // A batch whose op matches no row fails *in the pane*, rolled back.
    send(
        &handle,
        Command::ApplyBatch {
            epoch: crate::Epoch::new(5),
            ops: vec![edit(9999)],
        },
    );
    match next(&mut events).await {
        Some(Event::BatchFailed { epoch, message, .. }) => {
            assert_eq!(epoch.get(), 5);
            assert!(!message.is_empty());
        }
        other => panic!("expected BatchFailed, got {other:?}"),
    }

    send(&handle, Command::Shutdown);
    std::fs::remove_file(&path).ok();
}

/// Open a result and page through it: the grid's load-on-scroll path.
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
            epoch: crate::Epoch::new(1),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
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
            assert_eq!(epoch.get(), 1);
            assert_eq!(key, None, "editor SQL resolves no seek key");
        }
        other => panic!("expected ResultReady, got {other:?}"),
    }

    send(
        &handle,
        Command::FetchPage {
            offset: 998,
            limit: 100,
            epoch: crate::Epoch::new(1),
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
            assert_eq!(epoch.get(), 1);
        }
        other => panic!("expected ResultPageLoaded, got {other:?}"),
    }
    send(&handle, Command::Shutdown);
}

/// The column-stats bar end-to-end: `ColumnStats` over an open result returns a
/// pushed-down aggregate summary keyed to the right epoch/column.
#[tokio::test]
async fn computes_column_stats_for_open_result() {
    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");
    send(&handle, Command::Connect(sqlite(":memory:", true)));
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    // `counting_sql(1000)` yields one int column `x` = 1..=1000.
    send(
        &handle,
        Command::OpenResult {
            sql: counting_sql(1000),
            epoch: crate::Epoch::new(1),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::ResultReady { epoch, .. }) if epoch == crate::Epoch::new(1)
    ));

    send(
        &handle,
        Command::ColumnStats {
            epoch: crate::Epoch::new(1),
            column: "x".into(),
            flags: red_core::StatsFlags {
                numeric: true,
                distinct: true,
            },
        },
    );
    match next(&mut events).await {
        Some(Event::ColumnStatsReady {
            epoch,
            column,
            stats,
        }) => {
            assert_eq!(epoch.get(), 1);
            assert_eq!(column, "x");
            assert_eq!(stats.total, 1000);
            assert_eq!(stats.non_null, 1000);
            assert_eq!(stats.distinct, Some(1000));
            assert_eq!(stats.min, Value::Integer(1));
            assert_eq!(stats.max, Value::Integer(1000));
            // sum(1..=1000) = 500500; present because the column is numeric.
            assert_eq!(stats.sum, Some(Value::Integer(500500)));
            assert!(stats.avg.is_some(), "numeric column reports avg");
        }
        other => panic!("expected ColumnStatsReady, got {other:?}"),
    }
    send(&handle, Command::Shutdown);
}

/// Inline FK expansion (Track B7): an `OpenResult` carrying a `LEFT JOIN` spec
/// decorates a table browse with the referenced table's columns, reported *inline,
/// right after the FK column they expand from*, without changing the row count (the
/// orphan-FK row survives with NULL joined cells) or the keyset key. `channel`'s FK
/// sits in the middle (`id, tier_id, name`), so the joined column landing at index 2
/// proves the interleaving (not an append-at-end).
#[tokio::test]
async fn fk_join_expands_referenced_columns_inline() {
    let path = std::env::temp_dir().join(format!("red_svc_fkjoin_{}.db", std::process::id()));
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE tier(id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO tier VALUES (1, 'Tier A'), (2, 'Tier B');
             CREATE TABLE channel(id INTEGER PRIMARY KEY,
                 tier_id INTEGER REFERENCES tier(id), name TEXT);
             INSERT INTO channel VALUES (1, 1, 'ch1'), (2, 2, 'ch2'), (3, NULL, 'ch3');",
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

    // Browse `channel`, expanding its `tier_id` FK into `tier.name` inline.
    send(
        &handle,
        Command::OpenResult {
            sql: "SELECT * FROM channel".into(),
            epoch: crate::Epoch::new(1),
            table: Some(("main".into(), "channel".into())),
            sort: None,
            filter: None,
            joins: vec![FkJoin {
                alias: "_red_j0".into(),
                parent_alias: "_red_base".into(),
                on: vec![("tier_id".into(), "id".into())],
                to_schema: Some("main".into()),
                to_table: "tier".into(),
                select: vec![("name".into(), "tier_id.name".into())],
            }],
        },
    );
    match next(&mut events).await {
        Some(Event::ResultReady {
            columns,
            total,
            key,
            ..
        }) => {
            // The joined column is interleaved right after its FK column (`tier_id`),
            // not appended at the end; `name` stays last.
            assert_eq!(
                columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                vec!["id", "tier_id", "tier_id.name", "name"]
            );
            // The unique-target LEFT JOIN preserves cardinality: count is unchanged.
            assert_eq!(total, 3);
            // The base PK is still the seek key (joins don't disturb it).
            assert_eq!(key.expect("PK key").column, "id");
        }
        other => panic!("expected ResultReady, got {other:?}"),
    }

    // Read the rows in key order; the joined value rides in the column right after
    // `tier_id` (index 2), NULL for the orphan-FK row.
    send(
        &handle,
        Command::FetchRun {
            epoch: crate::Epoch::new(1),
            fetch: RunFetch::Forward { after: None },
            limit: 10,
            seq: 1,
        },
    );
    match next(&mut events).await {
        Some(Event::ResultRunLoaded { rows, .. }) => {
            assert_eq!(rows.len(), 3);
            assert_eq!(
                rows[0],
                vec![
                    Value::Integer(1),
                    Value::Integer(1),
                    Value::Text("Tier A".into()),
                    Value::Text("ch1".into()),
                ]
            );
            assert_eq!(rows[1][2], Value::Text("Tier B".into()));
            // Orphan FK (tier_id NULL) → LEFT JOIN keeps the row, joined cell NULL.
            assert_eq!(rows[2][1], Value::Null);
            assert_eq!(rows[2][2], Value::Null);
        }
        other => panic!("expected ResultRunLoaded, got {other:?}"),
    }

    std::fs::remove_file(&path).ok();
}

/// A `WHERE` filter on an inline-expanded FK column (Track B7): the join runs
/// *before* the filter, so a predicate can reference the joined dotted-alias column
/// (`"tier_id.name"`), and the total / rows narrow to the matching referenced rows
/// while the base PK stays the seek key.
#[tokio::test]
async fn where_filters_on_expanded_fk_column() {
    use red_core::ResultFilter;
    let path = std::env::temp_dir().join(format!("red_svc_fkfilter_{}.db", std::process::id()));
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE tier(id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO tier VALUES (1, 'Tier A'), (2, 'Tier B');
             CREATE TABLE channel(id INTEGER PRIMARY KEY,
                 tier_id INTEGER REFERENCES tier(id), name TEXT);
             INSERT INTO channel VALUES (1, 1, 'ch1'), (2, 2, 'ch2'), (3, 2, 'ch3');",
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

    // Browse `channel` expanding `tier_id → tier.name`, filtered to only the rows
    // whose *referenced* tier is 'Tier B' (channels 2 and 3).
    send(
        &handle,
        Command::OpenResult {
            sql: "SELECT * FROM channel".into(),
            epoch: crate::Epoch::new(1),
            table: Some(("main".into(), "channel".into())),
            sort: None,
            filter: Some(ResultFilter::Where("\"tier_id.name\" = 'Tier B'".into())),
            joins: vec![FkJoin {
                alias: "_red_j0".into(),
                parent_alias: "_red_base".into(),
                on: vec![("tier_id".into(), "id".into())],
                to_schema: Some("main".into()),
                to_table: "tier".into(),
                select: vec![("name".into(), "tier_id.name".into())],
            }],
        },
    );
    match next(&mut events).await {
        Some(Event::ResultReady { total, key, .. }) => {
            assert_eq!(
                total, 2,
                "only the two 'Tier B' channels survive the filter"
            );
            assert_eq!(key.expect("PK key").column, "id");
        }
        other => panic!("expected ResultReady, got {other:?}"),
    }

    send(
        &handle,
        Command::FetchRun {
            epoch: crate::Epoch::new(1),
            fetch: RunFetch::Forward { after: None },
            limit: 10,
            seq: 1,
        },
    );
    match next(&mut events).await {
        Some(Event::ResultRunLoaded { rows, .. }) => {
            assert_eq!(rows.len(), 2);
            // Both rows carry the filtered joined value, ids 2 and 3.
            assert_eq!(rows[0][0], Value::Integer(2));
            assert_eq!(rows[1][0], Value::Integer(3));
            assert_eq!(rows[0][2], Value::Text("Tier B".into()));
        }
        other => panic!("expected ResultRunLoaded, got {other:?}"),
    }

    std::fs::remove_file(&path).ok();
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
            epoch: crate::Epoch::new(7),
            table: Some(("main".into(), "t".into())),
            sort: None,
            filter: None,
            joins: Vec::new(),
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
            epoch: crate::Epoch::new(7),
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
            epoch: crate::Epoch::new(7),
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
            epoch: crate::Epoch::new(7),
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
            epoch: crate::Epoch::new(7),
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

    // A jump to ordinal 0 seeks from the true start (exact, not estimated).
    send(
        &handle,
        Command::FetchRun {
            epoch: crate::Epoch::new(7),
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
/// seek key, then deep-seek and jump; each fetch must come back fast
/// (indexed), proving the derived-table wrapper merges on the server.
#[tokio::test]
async fn mariadb_keyset_end_to_end() {
    use red_core::KeyKind;
    let Ok(url) = std::env::var("RED_TEST_MYSQL_URL") else {
        // Visible skip (with `--nocapture`): never a silent pass. CI sets the URL.
        eprintln!(
            "SKIP {}::mariadb_keyset_end_to_end: RED_TEST_MYSQL_URL not set",
            module_path!()
        );
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
            epoch: crate::Epoch::new(1),
            table: Some((p.database.clone(), table.clone())),
            sort: None,
            filter: None,
            joins: Vec::new(),
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

    // Deep forward seek near the bottom; it must be indexed-fast.
    let started = Instant::now();
    send(
        &handle,
        Command::FetchRun {
            epoch: crate::Epoch::new(1),
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
        "deep seek took {elapsed:?}; the derived-table wrapper isn't merging"
    );

    // Interpolated jump to ~50%: fast but approximate, flagged estimated.
    send(
        &handle,
        Command::FetchRun {
            epoch: crate::Epoch::new(1),
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
            epoch: crate::Epoch::new(1),
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
            epoch: crate::Epoch::new(1),
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
            epoch: crate::Epoch::new(9),
            table: Some(("main".into(), "t".into())),
            sort: None,
            filter: None,
            joins: Vec::new(),
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
            epoch: crate::Epoch::new(9),
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
    // Connect dials off the dispatch loop, so a session-bound command is only
    // valid once `Connected` lands, which is exactly how the UI sequences it.
    send(&handle, Command::Connect(sqlite(":memory:", false)));
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));
    send(
        &handle,
        Command::Query {
            sql: "SELECT 42 AS answer".into(),
            opts: QueryOptions::default(),
        },
    );
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
    let a = SessionId::new(1);
    let b = SessionId::new(2);
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

    // Same epoch in each session: epochs are per-session, not global.
    handle.send_to(
        a,
        Command::OpenResult {
            sql: counting_sql(100),
            epoch: crate::Epoch::new(1),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    handle.send_to(
        b,
        Command::OpenResult {
            sql: counting_sql(200),
            epoch: crate::Epoch::new(1),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );

    // The two opens probe off the loop, so their replies can interleave; route
    // each by its session tag and assert it landed on the right one.
    let mut totals = std::collections::HashMap::new();
    while totals.len() < 2 {
        match events.next().await {
            Some((Some(s), Event::ResultReady { total, epoch, .. })) => {
                assert_eq!(epoch.get(), 1);
                totals.insert(s, total);
            }
            other => panic!("expected ResultReady, got {other:?}"),
        }
    }
    assert_eq!(totals[&a], 100);
    assert_eq!(totals[&b], 200);

    // Dropping session A leaves B warm and serving (no reconnect).
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
            epoch: crate::Epoch::new(1),
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

/// Drain copy events until the terminal one, asserting only `CopyProgress` arrives
/// in between. Returns the terminal event (`CopyFinished`/`CopyFailed`/...). Copy
/// events route globally (`None` session), so the tag is ignored.
async fn drain_copy(
    events: &mut UnboundedReceiver<(Option<SessionId>, Event)>,
    copy_id: u64,
) -> Event {
    loop {
        match events.next().await.map(|(_, e)| e) {
            Some(Event::CopyProgress { id, .. }) => assert_eq!(id.get(), copy_id),
            Some(
                e @ (Event::CopyFinished { .. }
                | Event::CopyFailed { .. }
                | Event::CopyCancelled { .. }),
            ) => return e,
            other => panic!("unexpected event during copy: {other:?}"),
        }
    }
}

/// Seed a writable scratch DB with `src` (3 rows) and an empty `dst`, returning its
/// path. The caller connects writable and removes the file at the end.
fn seed_copy_db(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("red_svc_copy_{tag}_{}.db", std::process::id()));
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE src(id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO src VALUES (1,'one'),(2,'two'),(3,'three');
         CREATE TABLE dst(id INTEGER PRIMARY KEY, name TEXT);",
    )
    .unwrap();
    path
}

/// The headline round-trip: open a result, stream it into another table in the same
/// connection (Append), and verify the rows landed verbatim.
#[tokio::test]
async fn copies_result_into_table() {
    use red_core::{ColumnMap, CopyMode, TableRef};
    let path = seed_copy_db("rt");
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

    // Open the source so the copy can reference its epoch (filter included for free).
    send(
        &handle,
        Command::OpenResult {
            sql: "SELECT id, name FROM src".into(),
            epoch: crate::Epoch::new(1),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::ResultReady {
            epoch,
            total: 3,
            ..
        }) if epoch == crate::Epoch::new(1)
    ));

    send(
        &handle,
        Command::CopyToTable {
            id: OpId::new(7),
            source_epoch: crate::Epoch::new(1),
            target: TableRef {
                schema: Some("main".into()),
                name: "dst".into(),
            },
            target_session: S,
            mapping: vec![
                ColumnMap {
                    source: 0,
                    column: "id".into(),
                    decl_type: None,
                },
                ColumnMap {
                    source: 1,
                    column: "name".into(),
                    decl_type: None,
                },
            ],
            mode: CopyMode::Append,
            create: None,
        },
    );
    match drain_copy(&mut events, 7).await {
        Event::CopyFinished { id, rows } => {
            assert_eq!(id.get(), 7);
            assert_eq!(rows, 3, "all three rows copied");
        }
        other => panic!("expected CopyFinished, got {other:?}"),
    }

    // The rows landed in dst.
    send(
        &handle,
        Command::OpenResult {
            sql: "SELECT id, name FROM dst ORDER BY id".into(),
            epoch: crate::Epoch::new(2),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::ResultReady { total: 3, .. })
    ));
    send(
        &handle,
        Command::FetchPage {
            offset: 0,
            limit: 10,
            epoch: crate::Epoch::new(2),
        },
    );
    match next(&mut events).await {
        Some(Event::ResultPageLoaded { rows, .. }) => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[2][1], Value::Text("three".into()));
        }
        other => panic!("expected ResultPageLoaded, got {other:?}"),
    }

    send(&handle, Command::Shutdown);
    std::fs::remove_file(&path).ok();
}

/// "Copy into a *new* table": with `create: Some(columns)` the target is created from
/// the source's column shape (types spelled into the target dialect via
/// `red_core::typemap`) before the rows stream in, the keystone of database
/// migration. The `created` table does not exist beforehand.
#[tokio::test]
async fn copies_result_into_a_new_table() {
    use red_core::{ColumnMap, ColumnMeta, CopyMode, TableRef};
    let path = seed_copy_db("create");
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

    send(
        &handle,
        Command::OpenResult {
            sql: "SELECT id, name FROM src".into(),
            epoch: crate::Epoch::new(1),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::ResultReady {
            epoch,
            total: 3,
            ..
        }) if epoch == crate::Epoch::new(1)
    ));

    // Target `created` does not exist; `create` carries the column shape to build it.
    send(
        &handle,
        Command::CopyToTable {
            id: OpId::new(9),
            source_epoch: crate::Epoch::new(1),
            target: TableRef {
                schema: Some("main".into()),
                name: "created".into(),
            },
            target_session: S,
            mapping: vec![
                ColumnMap {
                    source: 0,
                    column: "id".into(),
                    decl_type: Some("INTEGER".into()),
                },
                ColumnMap {
                    source: 1,
                    column: "name".into(),
                    decl_type: Some("TEXT".into()),
                },
            ],
            mode: CopyMode::Append,
            create: Some(vec![
                ColumnMeta {
                    name: "id".into(),
                    type_name: Some("INTEGER".into()),
                    not_null: true,
                    primary_key: true,
                    default: None,
                    auto_increment: false,
                },
                ColumnMeta {
                    name: "name".into(),
                    type_name: Some("TEXT".into()),
                    not_null: false,
                    primary_key: false,
                    default: None,
                    auto_increment: false,
                },
            ]),
        },
    );
    match drain_copy(&mut events, 9).await {
        Event::CopyFinished { id, rows } => {
            assert_eq!(id.get(), 9);
            assert_eq!(
                rows, 3,
                "all three rows copied into the freshly-created table"
            );
        }
        other => panic!("expected CopyFinished, got {other:?}"),
    }

    // The table was created (the SELECT would error otherwise) and the rows landed.
    send(
        &handle,
        Command::OpenResult {
            sql: "SELECT id, name FROM created ORDER BY id".into(),
            epoch: crate::Epoch::new(2),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::ResultReady { total: 3, .. })
    ));
    send(
        &handle,
        Command::FetchPage {
            offset: 0,
            limit: 10,
            epoch: crate::Epoch::new(2),
        },
    );
    match next(&mut events).await {
        Some(Event::ResultPageLoaded { rows, .. }) => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Integer(1));
            assert_eq!(rows[2][1], Value::Text("three".into()));
        }
        other => panic!("expected ResultPageLoaded, got {other:?}"),
    }

    send(&handle, Command::Shutdown);
    std::fs::remove_file(&path).ok();
}

/// `TruncateInsert` clears the target first (via `clear_table`) so the copy is a
/// refresh, not an append: a pre-existing target row is gone afterward.
#[tokio::test]
async fn copy_truncate_insert_refreshes_target() {
    use red_core::{ColumnMap, CopyMode, TableRef};
    let path = seed_copy_db("trunc");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("INSERT INTO dst VALUES (99,'stale');")
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
    send(
        &handle,
        Command::OpenResult {
            sql: "SELECT id, name FROM src".into(),
            epoch: crate::Epoch::new(1),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::ResultReady { epoch, .. }) if epoch == crate::Epoch::new(1)
    ));
    send(
        &handle,
        Command::CopyToTable {
            id: OpId::new(3),
            source_epoch: crate::Epoch::new(1),
            target: TableRef {
                schema: Some("main".into()),
                name: "dst".into(),
            },
            target_session: S,
            mapping: vec![
                ColumnMap {
                    source: 0,
                    column: "id".into(),
                    decl_type: None,
                },
                ColumnMap {
                    source: 1,
                    column: "name".into(),
                    decl_type: None,
                },
            ],
            mode: CopyMode::TruncateInsert,
            create: None,
        },
    );
    match drain_copy(&mut events, 3).await {
        Event::CopyFinished { rows, .. } => assert_eq!(rows, 3),
        other => panic!("expected CopyFinished, got {other:?}"),
    }

    // Exactly the 3 source rows remain; the stale row (id 99) was truncated.
    send(
        &handle,
        Command::OpenResult {
            sql: "SELECT id FROM dst WHERE id = 99".into(),
            epoch: crate::Epoch::new(2),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    match next(&mut events).await {
        Some(Event::ResultReady { total, .. }) => assert_eq!(total, 0, "id 99 was truncated"),
        other => panic!("expected ResultReady, got {other:?}"),
    }
    send(&handle, Command::Shutdown);
    std::fs::remove_file(&path).ok();
}

/// Gap 2 (correctness invariant): a copy reads at **full fidelity**, never the
/// display fat-cell cap: a long `TEXT` value copies byte-exact, not truncated.
#[tokio::test]
async fn copy_is_byte_exact_for_long_values() {
    use red_core::{ColumnMap, CopyMode, TableRef};
    let path = std::env::temp_dir().join(format!("red_svc_copy_big_{}.db", std::process::id()));
    let big = "x".repeat(5000); // far over any display cap
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE src(id INTEGER PRIMARY KEY, big TEXT);
             CREATE TABLE dst(id INTEGER PRIMARY KEY, big TEXT);",
        )
        .unwrap();
        conn.execute("INSERT INTO src VALUES (1, ?1)", [&big])
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
    send(
        &handle,
        Command::OpenResult {
            sql: "SELECT id, big FROM src".into(),
            epoch: crate::Epoch::new(1),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::ResultReady { epoch, .. }) if epoch == crate::Epoch::new(1)
    ));
    send(
        &handle,
        Command::CopyToTable {
            id: OpId::new(1),
            source_epoch: crate::Epoch::new(1),
            target: TableRef {
                schema: Some("main".into()),
                name: "dst".into(),
            },
            target_session: S,
            mapping: vec![
                ColumnMap {
                    source: 0,
                    column: "id".into(),
                    decl_type: None,
                },
                ColumnMap {
                    source: 1,
                    column: "big".into(),
                    decl_type: Some("TEXT".into()),
                },
            ],
            mode: CopyMode::Append,
            create: None,
        },
    );
    assert!(matches!(
        drain_copy(&mut events, 1).await,
        Event::CopyFinished { rows: 1, .. }
    ));

    // Read the *length* back (an integer dodges the read-side display cap): the full
    // 5000 bytes landed, so the copy never saw `Value::Capped`.
    send(
        &handle,
        Command::OpenResult {
            sql: "SELECT length(big) FROM dst".into(),
            epoch: crate::Epoch::new(2),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        next(&mut events).await,
        Some(Event::ResultReady { total: 1, .. })
    ));
    send(
        &handle,
        Command::FetchPage {
            offset: 0,
            limit: 1,
            epoch: crate::Epoch::new(2),
        },
    );
    match next(&mut events).await {
        Some(Event::ResultPageLoaded { rows, .. }) => {
            assert_eq!(
                rows[0][0],
                Value::Integer(5000),
                "long TEXT copied byte-exact"
            );
        }
        other => panic!("expected ResultPageLoaded, got {other:?}"),
    }
    send(&handle, Command::Shutdown);
    std::fs::remove_file(&path).ok();
}

/// Cross-connection copy (Phase 2): source in session A, target in session B; the
/// backend bridges A's cursor to B's `insert_rows` with both ends pinned. Two
/// separate DBs prove the rows really cross the connection boundary.
#[tokio::test]
async fn copies_across_connections() {
    use red_core::{ColumnMap, CopyMode, TableRef};
    let a = SessionId::new(1);
    let b = SessionId::new(2);
    let src_path = std::env::temp_dir().join(format!("red_svc_copy_a_{}.db", std::process::id()));
    let dst_path = std::env::temp_dir().join(format!("red_svc_copy_b_{}.db", std::process::id()));
    {
        let conn = rusqlite::Connection::open(&src_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE src(id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO src VALUES (1,'one'),(2,'two');",
        )
        .unwrap();
        let conn = rusqlite::Connection::open(&dst_path).unwrap();
        conn.execute_batch("CREATE TABLE dst(id INTEGER PRIMARY KEY, name TEXT);")
            .unwrap();
    }
    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");
    handle.send_to(
        a,
        Command::Connect(sqlite(src_path.to_str().unwrap(), false)),
    );
    assert!(matches!(
        events.next().await,
        Some((Some(s), Event::Connected { .. })) if s == a
    ));
    handle.send_to(
        b,
        Command::Connect(sqlite(dst_path.to_str().unwrap(), false)),
    );
    assert!(matches!(
        events.next().await,
        Some((Some(s), Event::Connected { .. })) if s == b
    ));

    // Source result on A; copy it into B's `dst`.
    handle.send_to(
        a,
        Command::OpenResult {
            sql: "SELECT id, name FROM src".into(),
            epoch: crate::Epoch::new(1),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        events.next().await,
        Some((Some(s), Event::ResultReady { epoch, .. })) if s == a && epoch == crate::Epoch::new(1)
    ));
    handle.send_to(
        a,
        Command::CopyToTable {
            id: OpId::new(5),
            source_epoch: crate::Epoch::new(1),
            target: TableRef {
                schema: Some("main".into()),
                name: "dst".into(),
            },
            target_session: b,
            mapping: vec![
                ColumnMap {
                    source: 0,
                    column: "id".into(),
                    decl_type: None,
                },
                ColumnMap {
                    source: 1,
                    column: "name".into(),
                    decl_type: None,
                },
            ],
            mode: CopyMode::Append,
            create: None,
        },
    );
    match drain_copy(&mut events, 5).await {
        Event::CopyFinished { rows, .. } => assert_eq!(rows, 2),
        other => panic!("expected CopyFinished, got {other:?}"),
    }

    // The rows are now in B's database, not A's.
    handle.send_to(
        b,
        Command::OpenResult {
            sql: "SELECT id, name FROM dst ORDER BY id".into(),
            epoch: crate::Epoch::new(1),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        events.next().await,
        Some((Some(s), Event::ResultReady { total: 2, .. })) if s == b
    ));
    handle.send_to(b, Command::Shutdown);
    std::fs::remove_file(&src_path).ok();
    std::fs::remove_file(&dst_path).ok();
}

/// Phase 2: migrate *many* tables from one connection into another (empty) one in a
/// single job: each table is created on the target from the source's shape and its
/// rows streamed in. The source is listed `child` before `parent`, but the FK
/// `child → parent` orders `parent` first.
#[tokio::test]
async fn migrates_all_tables_into_another_connection() {
    let a = SessionId::new(1);
    let b = SessionId::new(2);
    let src_path =
        std::env::temp_dir().join(format!("red_svc_migrate_a_{}.db", std::process::id()));
    let dst_path =
        std::env::temp_dir().join(format!("red_svc_migrate_b_{}.db", std::process::id()));
    {
        let conn = rusqlite::Connection::open(&src_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE parent(id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO parent VALUES (1,'a'),(2,'b');
             CREATE TABLE child(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id), tag TEXT);
             INSERT INTO child VALUES (10,1,'x'),(20,1,'y'),(30,2,'z');
             CREATE INDEX ix_child_parent ON child(parent_id);",
        )
        .unwrap();
        // dst: an empty database (no tables); migrate creates them.
        rusqlite::Connection::open(&dst_path).unwrap();
    }
    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");
    handle.send_to(
        a,
        Command::Connect(sqlite(src_path.to_str().unwrap(), false)),
    );
    assert!(matches!(
        events.next().await,
        Some((Some(s), Event::Connected { .. })) if s == a
    ));
    handle.send_to(
        b,
        Command::Connect(sqlite(dst_path.to_str().unwrap(), false)),
    );
    assert!(matches!(
        events.next().await,
        Some((Some(s), Event::Connected { .. })) if s == b
    ));

    // Migrate both tables (listed child-first to exercise FK ordering) from A into B.
    handle.send_to(
        a,
        Command::MigrateTables {
            id: OpId::new(11),
            source_schema: Some("main".into()),
            tables: vec!["child".into(), "parent".into()],
            target_session: b,
            target_schema: Some("main".into()),
        },
    );
    match drain_copy(&mut events, 11).await {
        Event::CopyFinished { rows, .. } => {
            assert_eq!(rows, 5, "2 parent + 3 child rows migrated")
        }
        other => panic!("expected CopyFinished, got {other:?}"),
    }

    // Both tables now exist on B (a missing table would error, not ResultReady) with
    // their rows.
    handle.send_to(
        b,
        Command::OpenResult {
            sql: "SELECT count(*) FROM parent".into(),
            epoch: crate::Epoch::new(1),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        events.next().await,
        Some((Some(s), Event::ResultReady { total: 1, .. })) if s == b
    ));
    handle.send_to(
        b,
        Command::OpenResult {
            sql: "SELECT id, parent_id, tag FROM child ORDER BY id".into(),
            epoch: crate::Epoch::new(2),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        events.next().await,
        Some((Some(s), Event::ResultReady { total: 3, .. })) if s == b
    ));
    handle.send_to(
        b,
        Command::FetchPage {
            offset: 0,
            limit: 10,
            epoch: crate::Epoch::new(2),
        },
    );
    match events.next().await.map(|(_, e)| e) {
        Some(Event::ResultPageLoaded { rows, .. }) => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Integer(10));
            assert_eq!(rows[0][2], Value::Text("x".into()));
        }
        other => panic!("expected ResultPageLoaded, got {other:?}"),
    }

    // The deferred index pass recreated `ix_child_parent` on the target.
    handle.send_to(
        b,
        Command::OpenResult {
            sql: "SELECT count(*) FROM sqlite_master \
                  WHERE type='index' AND name='ix_child_parent'"
                .into(),
            epoch: crate::Epoch::new(3),
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    assert!(matches!(
        events.next().await,
        Some((Some(s), Event::ResultReady { total: 1, .. })) if s == b
    ));
    handle.send_to(
        b,
        Command::FetchPage {
            offset: 0,
            limit: 1,
            epoch: crate::Epoch::new(3),
        },
    );
    match events.next().await.map(|(_, e)| e) {
        Some(Event::ResultPageLoaded { rows, .. }) => {
            assert_eq!(rows[0][0], Value::Integer(1), "index recreated on target");
        }
        other => panic!("expected ResultPageLoaded, got {other:?}"),
    }

    handle.send_to(b, Command::Shutdown);
    std::fs::remove_file(&src_path).ok();
    std::fs::remove_file(&dst_path).ok();
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

/// The agent registry resolves a turn's `agent` id: an unknown id (e.g. a saved
/// chat bound to a since-removed agent) fails with a clear error naming it, and a
/// configured API agent with no key reports "add an API key", both *before*
/// anything spawns, so neither launches an agent process.
#[tokio::test]
async fn ai_turn_resolves_agent_id_with_clear_errors() {
    let mut handle = spawn();
    let mut events = handle.take_events().expect("event stream");
    send(&handle, Command::Connect(sqlite(":memory:", true)));
    assert!(matches!(
        next(&mut events).await,
        Some(Event::Connected { .. })
    ));

    // One ACP agent (needs no key) and one keyless API agent.
    send(
        &handle,
        Command::ConfigureAi(AiConfig {
            agents: vec![
                AiAgentProfile {
                    id: "sub".into(),
                    name: "Sub".into(),
                    kind: AiAgentKind::Acp,
                    command: "true".into(),
                    base_url: String::new(),
                    model: String::new(),
                    api_key: String::new(),
                },
                AiAgentProfile {
                    id: "api".into(),
                    name: "API".into(),
                    kind: AiAgentKind::Api,
                    command: String::new(),
                    base_url: String::new(),
                    model: String::new(),
                    api_key: String::new(),
                },
            ],
            default_agent: "sub".into(),
            show_thinking: false,
            enabled: true,
            tier: AiTier::Read,
            limits: AiLimits::default(),
        }),
    );

    // Unknown id → a clear error that names the missing agent.
    send(
        &handle,
        Command::AiTurn {
            conversation_id: ConversationId::new(1),
            agent: "ghost".into(),
            message: "hi".into(),
            context: AiContext::default(),
        },
    );
    match next(&mut events).await {
        Some(Event::AiError { message, .. }) => {
            assert!(message.contains("ghost"), "got: {message}")
        }
        other => panic!("expected AiError, got {other:?}"),
    }

    // Keyless API agent → "add an API key", never a failed network call.
    send(
        &handle,
        Command::AiTurn {
            conversation_id: ConversationId::new(2),
            agent: "api".into(),
            message: "hi".into(),
            context: AiContext::default(),
        },
    );
    match next(&mut events).await {
        Some(Event::AiError { message, .. }) => {
            assert!(message.contains("add an API key"), "got: {message}")
        }
        other => panic!("expected AiError, got {other:?}"),
    }

    send(&handle, Command::Shutdown);
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
