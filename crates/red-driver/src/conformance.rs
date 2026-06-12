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

use crate::{AbortSignal, DatabaseDriver, PageCap, DEFAULT_DISPLAY_CELL_CAP};

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

/// A superseded one-shot fetch (`count` over `heavy_sql`) is aborted *at the
/// engine*, not just abandoned: after `settle` the [`AbortSignal`] fires and the
/// fetch returns `Interrupted` promptly — proving the engine stopped rather than
/// the future being dropped while the server kept scanning. `heavy_sql` must keep a
/// `count(*)` busy long enough to interrupt.
pub(crate) async fn superseded_fetch_is_cancelled(
    driver: &dyn DatabaseDriver,
    heavy_sql: &str,
    settle: Duration,
) {
    let abort = AbortSignal::new();
    let trigger = abort.clone();
    tokio::spawn(async move {
        tokio::time::sleep(settle).await;
        trigger.abort();
    });
    match driver.count(heavy_sql, &abort).await {
        Err(RedError::Interrupted) => {}
        other => panic!("expected Interrupted, got {other:?}"),
    }
}

/// A fetch on an *already*-aborted signal bails immediately with `Interrupted` —
/// the arm-after-abort path, so a fetch superseded while still queued behind the
/// concurrency limit never reaches the engine. `heavy_sql` would run a long time
/// if it *weren't* short-circuited, so a prompt return is the proof.
pub(crate) async fn pre_aborted_fetch_returns_immediately(
    driver: &dyn DatabaseDriver,
    heavy_sql: &str,
) {
    let abort = AbortSignal::new();
    abort.abort();
    match driver.count(heavy_sql, &abort).await {
        Err(RedError::Interrupted) => {}
        other => panic!("expected immediate Interrupted, got {other:?}"),
    }
}

/// A late abort, fired *after* its fetch completed, is a no-op — the driver
/// disarmed on completion, so it can't cancel a connection that's since returned to
/// a pool and serves the next fetch. `fast_sql` must be a cheap `count` source.
pub(crate) async fn abort_after_completion_is_noop(driver: &dyn DatabaseDriver, fast_sql: &str) {
    let abort = AbortSignal::new();
    let first = driver.count(fast_sql, &abort).await.unwrap();
    abort.abort(); // late — the fetch already disarmed it
                   // A follow-up fetch on the (possibly reused) connection still succeeds.
    let again = driver.count(fast_sql, &AbortSignal::new()).await.unwrap();
    assert_eq!(
        first, again,
        "the reused connection is healthy after a late abort"
    );
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

/// The contains-filter ([`red_core::ResultFilter::Contains`]) narrows a result to
/// rows whose text matches case-insensitively and **literally** (no `LIKE`
/// wildcard or quote can leak), and excludes blob columns. The caller seeds a
/// table `(id PK, name TEXT, note TEXT, data BLOB)` with these five rows and passes
/// its `(schema, table)` plus a `SELECT *` base over it:
/// ```text
///   1 'apple'      'red fruit'   data = bytes "apple"
///   2 'banana'     'yellow'      data = bytes "apple"
///   3 'apple pie'  'dessert'     data = 0x00
///   4 '100% juice' 'on sale'     data = 0x00
///   5 "O'Brien"    'name'        data = 0x00
/// ```
/// Rows 1–2 carry a `data` blob whose bytes spell `apple` so that *including* the
/// blob in the cast would inflate the `apple` count — proving it's excluded.
pub(crate) async fn filters_contains(
    driver: &dyn DatabaseDriver,
    schema: &str,
    table: &str,
    base_sql: &str,
) {
    let abort = AbortSignal::new();
    let detail = driver.describe_table(schema, table).await.unwrap();
    // The base narrowed by the contains predicate for `term` (panics if nothing is
    // searchable — these fixtures always have text columns).
    let filtered = |term: &str| {
        let pred = driver
            .contains_predicate(&detail.columns, term)
            .expect("a text column is searchable");
        format!("SELECT * FROM ({base_sql}) AS _red_f WHERE ({pred})")
    };
    let count = |sql: String| {
        let abort = &abort;
        async move { driver.count(&sql, abort).await.unwrap() }
    };

    // Plain substring across the text columns, case-insensitive: 'apple' + 'apple pie'.
    assert_eq!(count(filtered("apple")).await, 2, "matches across text columns");
    assert_eq!(count(filtered("APPLE")).await, 2, "case-insensitive");
    // `%` is a LIKE wildcard; escaped, it matches only the literal-percent row.
    // Unescaped the pattern would be `%%%` and match every row — the regression
    // this guards against.
    assert_eq!(count(filtered("%")).await, 1, "LIKE metacharacters match literally");
    // A single quote can't break out of the string literal (no injection / no error).
    assert_eq!(count(filtered("O'Brien")).await, 1, "embedded quote is escaped");
    // A non-match is empty, never an error.
    assert_eq!(count(filtered("zzznope")).await, 0, "no match → empty result");

    // The blob column is excluded from the cast (binary-to-text is engine-specific
    // noise) — its bytes spell `apple` but don't lift the count above.
    let pred = driver.contains_predicate(&detail.columns, "apple").unwrap();
    assert!(!pred.contains("data"), "blob column is not searched: {pred}");
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

    let abort = AbortSignal::new();

    // First page: no bound, ascending from the start.
    let first = driver
        .fetch_seek(sql, key, None, false, 5, &abort)
        .await
        .unwrap();
    assert_eq!(first.rows.len(), 5);
    assert_eq!(first.rows[0][0], Value::Integer(1));

    // Forward: strictly after a bound.
    let fwd = driver
        .fetch_seek(sql, key, Some(&[Value::Integer(997)]), false, 5, &abort)
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
        .fetch_seek(sql, key, Some(&[Value::Integer(4)]), true, 5, &abort)
        .await
        .unwrap();
    assert_eq!(
        ids(&back, 0),
        vec![Value::Integer(3), Value::Integer(2), Value::Integer(1)]
    );

    // Seek + bounded skip (the exact-jump / checkpoint primitive): an inclusive
    // `>=` lower bound, then OFFSET within the post-seek window.
    let from_start = driver
        .fetch_seek_skip(sql, key, None, 10, 3, &abort)
        .await
        .unwrap();
    assert_eq!(
        ids(&from_start, 0),
        vec![Value::Integer(11), Value::Integer(12), Value::Integer(13)]
    );
    // `>=` includes the bound itself (skip 0 lands on it).
    let inclusive = driver
        .fetch_seek_skip(sql, key, Some(&[Value::Integer(500)]), 0, 1, &abort)
        .await
        .unwrap();
    assert_eq!(ids(&inclusive, 0), vec![Value::Integer(500)]);
    // The bound is ordinal 0 of the window; skipping 10 lands on id 510.
    let skipped = driver
        .fetch_seek_skip(sql, key, Some(&[Value::Integer(500)]), 10, 1, &abort)
        .await
        .unwrap();
    assert_eq!(ids(&skipped, 0), vec![Value::Integer(510)]);

    assert_eq!(
        driver.key_bounds(sql, key, &abort).await.unwrap(),
        Some((1, 1000))
    );
}

/// Composite keyset seek for sorted results: paging by a `(sort_col,
/// pk)` tuple over a **non-unique** sort column covers every row exactly once, in
/// `(sort_col, pk)` order, for both sort directions — the tiebreaker is what stops
/// equal-`sort_col` rows from being skipped or duplicated across a page boundary.
/// `sql` must select a table of `n` rows whose `pk` is the contiguous range
/// `1..=n` and whose `sort_col` repeats (many rows share a value); `key_asc` and
/// `key_desc` are the same composite key ascending/descending. `lead`/`tie` name
/// the sort and pk columns; both must be integers for this check.
pub(crate) async fn seeks_composite_sorted(
    driver: &dyn DatabaseDriver,
    sql: &str,
    key_asc: &KeySpec,
    key_desc: &KeySpec,
    n: i64,
) {
    let abort = AbortSignal::new();
    // A deliberately small page so equal-`sort_col` ties straddle boundaries —
    // the exact case a scalar (sort_col-only) seek gets wrong.
    let page_size = 4;
    let int = |row: &[Value], col: usize| match row[col] {
        Value::Integer(v) => v,
        ref other => panic!("expected integer key, got {other:?}"),
    };

    for (key, descending) in [(key_asc, false), (key_desc, true)] {
        // Walk the whole result forward (in sort order) one page at a time,
        // re-seeking from each page's last key tuple.
        let mut all: Vec<Vec<Value>> = Vec::new();
        let mut bound: Option<Vec<Value>> = None;
        let (mut lead, mut tie) = (0usize, 0usize);
        loop {
            let page = driver
                .fetch_seek(sql, key, bound.as_deref(), false, page_size, &abort)
                .await
                .unwrap();
            let Some(last) = page.rows.last() else { break };
            lead = page
                .columns
                .iter()
                .position(|c| c.name == key.column)
                .unwrap();
            tie = page
                .columns
                .iter()
                .position(|c| Some(&c.name) == key.tiebreak.as_ref())
                .unwrap();
            bound = Some(vec![last[lead].clone(), last[tie].clone()]);
            all.extend(page.rows);
        }

        // Every row exactly once: the pk set is precisely `1..=n`.
        assert_eq!(
            all.len(),
            n as usize,
            "composite seek covered every row (descending={descending})"
        );
        let mut ids: Vec<i64> = all.iter().map(|r| int(r, tie)).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            (1..=n).collect::<Vec<_>>(),
            "no skipped or duplicated rows at tie boundaries (descending={descending})"
        );

        // Strictly monotonic in `(sort_col, pk)` — the tiebreaker orders rows that
        // share a `sort_col` value deterministically.
        let tuples: Vec<(i64, i64)> = all.iter().map(|r| (int(r, lead), int(r, tie))).collect();
        for w in tuples.windows(2) {
            if descending {
                assert!(w[0] > w[1], "descending (sort_col, pk) strictly decreasing");
            } else {
                assert!(w[0] < w[1], "ascending (sort_col, pk) strictly increasing");
            }
        }
    }
}

/// The driver-side display cap: a display fetch caps fat non-key cells
/// while the keyset key rides through verbatim and `export`/`Full` stay byte-exact.
/// `sql` must select exactly one row with columns `(key, big_text, big_blob)` where
/// `big_text` and `big_blob` each exceed [`DEFAULT_DISPLAY_CELL_CAP`] bytes and `big_text`
/// is `text_len` repeats of the ASCII byte `fill`; `key` names the integer key
/// column; `text_len`/`blob_len` are the full source byte lengths; `tag` keeps the
/// export temp file unique.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn caps_display_keeps_key_and_export(
    driver: &dyn DatabaseDriver,
    sql: &str,
    key: &KeySpec,
    fill: u8,
    text_len: usize,
    blob_len: usize,
    tag: &str,
) {
    // --- Display seek caps the fat non-key cells, key column verbatim. ---
    let abort = AbortSignal::new();
    let page = driver
        .fetch_seek(sql, key, None, false, 5, &abort)
        .await
        .unwrap();
    assert_eq!(page.rows.len(), 1, "fixture has exactly one row");
    let row = &page.rows[0];

    // Key column (0): NOT capped — its bytes must round-trip as a seek bound.
    assert!(
        matches!(row[0], Value::Integer(_)),
        "the key rides through whole, got {:?}",
        row[0]
    );

    // Text column (1): a Capped prefix carrying the true length, head within the cap.
    match &row[1] {
        Value::Capped(c) => {
            assert!(!c.blob, "text capped as text");
            assert_eq!(c.len, text_len, "the true text length is preserved");
            assert!(
                c.head.len() <= DEFAULT_DISPLAY_CELL_CAP,
                "head ({}) within the cap ({DEFAULT_DISPLAY_CELL_CAP})",
                c.head.len()
            );
        }
        other => panic!("expected capped text, got {other:?}"),
    }

    // Blob column (2): length only — the bytes were never materialized.
    match &row[2] {
        Value::Capped(c) => {
            assert!(c.blob, "blob capped as blob");
            assert_eq!(c.len, blob_len, "the true blob length is preserved");
        }
        other => panic!("expected capped blob, got {other:?}"),
    }

    // --- A Full page fetch keeps the same cells whole (the clipboard re-fetch). ---
    let full = driver
        .fetch_page(sql, 0, 5, PageCap::Full, &abort)
        .await
        .unwrap();
    match &full.rows[0][1] {
        Value::Text(s) => assert_eq!(s.len(), text_len, "Full keeps the whole text"),
        other => panic!("expected whole text under Full, got {other:?}"),
    }

    // --- Export stays byte-exact: the full text reaches the file uncapped. ---
    let dir = std::env::temp_dir();
    let csv_path = dir.join(format!("red_conf_cap_{tag}.csv"));
    let no_cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drain = tokio::sync::mpsc::unbounded_channel().0;
    driver
        .export(sql, &csv_path, ExportFormat::Csv, no_cancel, drain)
        .await
        .unwrap();
    let csv = std::fs::read_to_string(&csv_path).unwrap();
    let needle = String::from_utf8(vec![fill; text_len]).unwrap();
    assert!(
        csv.contains(&needle),
        "export carries the full {text_len}-byte text uncapped"
    );
    std::fs::remove_file(&csv_path).ok();
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
