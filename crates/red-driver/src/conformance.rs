//! The engine-agnostic driver conformance battery.
//!
//! Every `DatabaseDriver` impl upholds the same contract — windowed streaming,
//! out-of-band cancel, introspection shape, read-only enforcement, indexed seek,
//! and row-by-row export. These assertions encode that contract once; each
//! driver's test module supplies its own dialect (the SQL/DDL) and calls in. The
//! payoff: the engines are verified *identically*, and adding a fourth driver is
//! "make the battery pass" rather than "reinvent the assertions".
//!
//! SQLite runs the battery on every `cargo test` (embedded, no server). Postgres
//! and MySQL run it only when their `RED_TEST_*_URL` is set, so CI without a
//! server skips cleanly.

use std::time::Duration;

use red_core::{ExportFormat, KeySpec, ObjectKind, QueryOptions, RedError, Value};

use crate::DatabaseDriver;

/// `open_cursor` streams the result in windows that never exceed the requested
/// size, and the total is exact — memory stays flat regardless of result size.
/// `sql` must yield `expected` single-column rows.
pub(crate) async fn streams_in_bounded_windows(
    driver: &dyn DatabaseDriver,
    sql: &str,
    expected: usize,
) {
    let cursor = driver
        .open_cursor(sql, QueryOptions::default())
        .await
        .unwrap();
    assert_eq!(cursor.columns().len(), 1, "single projected column");

    let mut total = 0usize;
    loop {
        let window = cursor.next_window(1000).await.unwrap();
        assert!(window.rows.len() <= 1000, "windows stay bounded");
        total += window.rows.len();
        if window.exhausted {
            break;
        }
    }
    assert_eq!(total, expected, "every row is streamed exactly once");
}

/// A cancel token aborts an in-flight fetch out-of-band, surfacing as
/// `RedError::Interrupted` (never a generic failure). `heavy_sql` must keep the
/// engine busy long enough to interrupt; `settle` lets the first step get under
/// way before the cancel fires (some engines no-op a cancel with nothing
/// running).
pub(crate) async fn cancel_aborts_in_flight_fetch(
    driver: &dyn DatabaseDriver,
    heavy_sql: &str,
    settle: Duration,
) {
    let cursor = driver
        .open_cursor(heavy_sql, QueryOptions::default())
        .await
        .unwrap();
    let cancel = cursor.cancel_token();

    let fetch = tokio::spawn(async move { cursor.next_window(1_000_000_000).await });
    tokio::time::sleep(settle).await;
    cancel.cancel();

    match fetch.await.unwrap() {
        Err(RedError::Interrupted) => {}
        other => panic!("expected Interrupted, got {other:?}"),
    }
}

/// The introspection shape: `list_objects` surfaces the fixture tables and view
/// under `schema` with the right kinds, and `describe_table` reports the primary
/// key, a NOT NULL column, the foreign key, and the secondary index.
///
/// The caller creates the fixtures with its own dialect first; this asserts the
/// engine-agnostic result. Fixtures: a `books` table with an integer primary key
/// `id`, a NOT NULL `title`, an `author_id` foreign key to `authors(id)`, and an
/// index over `author_id`; plus a `recent` view.
pub(crate) async fn introspects_tables_columns_fks_and_indexes(
    driver: &dyn DatabaseDriver,
    schema: &str,
    authors: &str,
    books: &str,
    recent: &str,
) {
    let schemas = driver.list_objects().await.unwrap();
    let ns = schemas
        .iter()
        .find(|s| s.name == schema)
        .unwrap_or_else(|| panic!("schema {schema} present in the tree"));
    let objects: Vec<(&str, ObjectKind)> = ns
        .objects
        .iter()
        .map(|o| (o.name.as_str(), o.kind))
        .collect();
    assert!(objects.contains(&(authors, ObjectKind::Table)));
    assert!(objects.contains(&(books, ObjectKind::Table)));
    assert!(objects.contains(&(recent, ObjectKind::View)));

    let detail = driver.describe_table(schema, books).await.unwrap();
    let col = |n: &str| {
        detail
            .columns
            .iter()
            .find(|c| c.name == n)
            .unwrap_or_else(|| panic!("column {n} present on {books}"))
    };
    assert!(col("id").primary_key, "id is the primary key");
    assert!(col("title").not_null, "title is NOT NULL");

    assert_eq!(detail.foreign_keys.len(), 1, "one foreign key");
    let fk = &detail.foreign_keys[0];
    assert_eq!(fk.column, "author_id");
    assert_eq!(fk.ref_table, authors);
    assert_eq!(fk.ref_column, "id");

    assert!(
        detail
            .indexes
            .iter()
            .any(|i| i.columns == vec!["author_id".to_string()]),
        "an index over author_id is reported"
    );
}

/// `export` streams to CSV and JSON without materializing the result: a field
/// containing a comma is quoted, and a SQL NULL becomes JSON `null`. `select_sql`
/// must yield two columns `id, name` = `(1, 'a,b'), (2, NULL)` ordered by `id`.
/// `tag` makes the temp file names unique across concurrent callers.
pub(crate) async fn exports_csv_and_json(driver: &dyn DatabaseDriver, select_sql: &str, tag: &str) {
    let dir = std::env::temp_dir();
    let csv_path = dir.join(format!("red_conf_{tag}.csv"));
    let json_path = dir.join(format!("red_conf_{tag}.json"));

    // A never-cancelled flag and a throwaway progress channel — the export's
    // cancellation / progress plumbing is exercised separately.
    let no_cancel = || std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drain = || tokio::sync::mpsc::unbounded_channel().0;

    let rows = driver
        .export(
            select_sql,
            &csv_path,
            ExportFormat::Csv,
            no_cancel(),
            drain(),
        )
        .await
        .unwrap();
    assert_eq!(rows, 2, "two data rows written");
    let csv = std::fs::read_to_string(&csv_path).unwrap();
    assert!(csv.starts_with("id,name\n"), "header row first: {csv}");
    assert!(csv.contains("\"a,b\""), "comma field is quoted: {csv}");

    driver
        .export(
            select_sql,
            &json_path,
            ExportFormat::Json,
            no_cancel(),
            drain(),
        )
        .await
        .unwrap();
    let json = std::fs::read_to_string(&json_path).unwrap();
    assert!(
        json.contains("\"name\":null"),
        "NULL becomes json null: {json}"
    );

    std::fs::remove_file(&csv_path).ok();
    std::fs::remove_file(&json_path).ok();
}

/// Keyset (seek) paging is exact in both directions and reads key bounds. `sql`
/// must select a table with an integer key column whose values are the
/// contiguous range `1..=1000`; `key` names that column.
pub(crate) async fn seeks_forward_backward_and_reads_bounds(
    driver: &dyn DatabaseDriver,
    sql: &str,
    key: &KeySpec,
) {
    let ids = |page: &red_core::ResultPage, col: usize| {
        page.rows.iter().map(|r| r[col].clone()).collect::<Vec<_>>()
    };

    // First page: no bound, ascending from the start.
    let first = driver.fetch_seek(sql, key, None, false, 5).await.unwrap();
    assert_eq!(first.rows.len(), 5);
    assert_eq!(first.rows[0][0], Value::Integer(1));

    // Forward: strictly after a bound.
    let fwd = driver
        .fetch_seek(sql, key, Some(&Value::Integer(997)), false, 5)
        .await
        .unwrap();
    assert_eq!(
        ids(&fwd, 0),
        vec![
            Value::Integer(998),
            Value::Integer(999),
            Value::Integer(1000)
        ]
    );

    // Backward: strictly before a bound, returned descending (the caller flips).
    let back = driver
        .fetch_seek(sql, key, Some(&Value::Integer(4)), true, 5)
        .await
        .unwrap();
    assert_eq!(
        ids(&back, 0),
        vec![Value::Integer(3), Value::Integer(2), Value::Integer(1)]
    );

    // Seek + bounded skip (the exact-jump / checkpoint primitive): an inclusive
    // `>=` lower bound, then OFFSET within the post-seek window.
    let from_start = driver.fetch_seek_skip(sql, key, None, 10, 3).await.unwrap();
    assert_eq!(
        ids(&from_start, 0),
        vec![Value::Integer(11), Value::Integer(12), Value::Integer(13)]
    );
    // `>=` includes the bound itself (skip 0 lands on it).
    let inclusive = driver
        .fetch_seek_skip(sql, key, Some(&Value::Integer(500)), 0, 1)
        .await
        .unwrap();
    assert_eq!(ids(&inclusive, 0), vec![Value::Integer(500)]);
    // The bound is ordinal 0 of the window; skipping 10 lands on id 510.
    let skipped = driver
        .fetch_seek_skip(sql, key, Some(&Value::Integer(500)), 10, 1)
        .await
        .unwrap();
    assert_eq!(ids(&skipped, 0), vec![Value::Integer(510)]);

    assert_eq!(driver.key_bounds(sql, key).await.unwrap(), Some((1, 1000)));
}

/// A read-only connection rejects a write at the engine. `write_sql` is any
/// statement that mutates (DDL or DML); the driver must surface an error rather
/// than silently succeeding.
pub(crate) async fn read_only_rejects_write(driver: &dyn DatabaseDriver, write_sql: &str) {
    assert!(
        driver.execute(write_sql).await.is_err(),
        "read-only connection must reject a write"
    );
}
