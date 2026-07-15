//! Long-running background jobs the dispatch loop hands off: CSV/JSON import,
//! streamed table copy, and whole-schema migration (with its FK-ordered table
//! sequencing). Extracted from `dispatch/mod.rs` (guidelines D) as a pure move;
//! the loop's arms call these unchanged. Shared loop helpers (`emit`, `lock`,
//! `StreamRate`, the concurrency/window consts) live on the parent module and are
//! pulled in via `use super::*`.

use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use red_core::{ColumnMap, CopyMode, FkEdge, ImportFormat, TableRef};
use red_driver::{DatabaseDriver, ImportReader};

use crate::Event;

use super::*;

/// Stream `path` (CSV/JSONL) into `target`, coercing each source cell to a typed
/// `Value` per its mapped target column ([`coerce_edit_value`]) and inserting in
/// chunks of `chunk_size` rows. Runs on a blocking thread (file IO); each chunk's
/// async [`insert_rows`](DatabaseDriver::insert_rows) is driven with
/// `handle.block_on`. Holds at most one chunk in memory, never the whole file.
///
/// Inserts **commit per chunk** (v1), so the returned committed count is meaningful
/// even on error/cancel: a mid-file failure leaves earlier chunks committed (atomic
/// whole-file import is a future option; see `docs/plans/data-import.md`). `cancel`
/// is checked between rows. Returns `(rows committed, error-or-None)`.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_import_blocking(
    driver: Arc<dyn DatabaseDriver>,
    path: std::path::PathBuf,
    format: ImportFormat,
    target: TableRef,
    mapping: Vec<ColumnMap>,
    chunk_size: usize,
    cancel: Arc<AtomicBool>,
    progress: tokio::sync::mpsc::UnboundedSender<u64>,
    handle: tokio::runtime::Handle,
) -> (u64, Option<RedError>) {
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            return (
                0,
                Some(RedError::Driver(format!(
                    "cannot open {}: {e}",
                    path.display()
                ))),
            );
        }
    };
    let (_src_cols, mut reader) = match ImportReader::begin(BufReader::new(file), format) {
        Ok(r) => r,
        Err(e) => return (0, Some(RedError::Query(format!("read error: {e}")))),
    };
    let columns: Vec<Column> = mapping
        .iter()
        .map(|m| Column {
            name: m.column.clone(),
            decl_type: m.decl_type.clone(),
        })
        .collect();
    let chunk_size = chunk_size.max(1);
    let mut chunk: Vec<Vec<Value>> = Vec::with_capacity(chunk_size);
    let mut committed = 0u64;
    let mut row_no = 0usize;

    // Insert (and commit) the buffered chunk, reporting progress. Returns early from
    // the enclosing fn with the committed count on engine error.
    macro_rules! flush {
        () => {{
            if !chunk.is_empty() {
                match handle.block_on(driver.insert_rows(&target, &columns, &chunk)) {
                    Ok(n) => {
                        committed += n;
                        chunk.clear();
                        let _ = progress.send(committed);
                    }
                    Err(e) => return (committed, Some(e)),
                }
            }
        }};
    }

    loop {
        if cancel.load(Ordering::Relaxed) {
            return (committed, Some(RedError::Interrupted));
        }
        match reader.next_row() {
            Ok(None) => break,
            Ok(Some(cells)) => {
                row_no += 1;
                let mut values = Vec::with_capacity(columns.len());
                for m in &mapping {
                    let raw = cells.get(m.source).map(String::as_str).unwrap_or("");
                    match coerce_edit_value(raw, m.decl_type.as_deref()) {
                        Ok(v) => values.push(v),
                        Err(reason) => {
                            return (
                                committed,
                                Some(RedError::Query(format!("row {row_no}: {reason}"))),
                            );
                        }
                    }
                }
                chunk.push(values);
                if chunk.len() >= chunk_size {
                    flush!();
                }
            }
            Err(e) => {
                return (
                    committed,
                    Some(RedError::Query(format!("row {}: {e}", row_no + 1))),
                );
            }
        }
    }
    flush!();
    (committed, None)
}

/// Stream an open result (`source_sql`, already filtered/sorted/wrapped) from `src`
/// straight into `target` on `dst`: the table-copy job. Reuses the read seam
/// (`open_cursor`/`next_window`, **full fidelity** so a long TEXT/blob copies
/// byte-exact, never the display cap; `data-import.md`'s Gap 2) and the write seam
/// (`insert_rows`); `src` and `dst` may be the same driver (same-connection copy) or
/// two different engines (cross-connection). One window is resident at a time, so
/// memory is bounded by [`COPY_CHUNK_ROWS`], not row count.
///
/// `mapping` projects each source row into target-column order by the source column
/// **index** it carries; each value rides as a typed [`Value`] and `insert_rows`
/// binds it under the **target** column's `decl_type` (so a cross-engine
/// `uuid`/`json`/… text round-trips into its target column). For `TruncateInsert`
/// the target is cleared first. Inserts **commit per chunk** (like import), so the
/// returned committed count is meaningful on error/cancel. `cancel` is checked
/// between chunks. Returns `(rows committed, error-or-None)`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn copy_job(
    src: Arc<dyn DatabaseDriver>,
    dst: Arc<dyn DatabaseDriver>,
    source_sql: String,
    target: TableRef,
    mapping: Vec<ColumnMap>,
    mode: CopyMode,
    create: Option<Vec<ColumnMeta>>,
    cancel: Arc<AtomicBool>,
    events: Events,
    id: OpId,
) -> (u64, Option<RedError>) {
    if mapping.is_empty() {
        return (
            0,
            Some(RedError::Query("no columns map onto the target".into())),
        );
    }
    // The target columns, in insert order (name + declared type for the bind cast).
    let target_columns: Vec<Column> = mapping
        .iter()
        .map(|m| Column {
            name: m.column.clone(),
            decl_type: m.decl_type.clone(),
        })
        .collect();

    // "Copy into a *new* table" / migration: create the target from the source's
    // column shape (types mapped into the target dialect) before any read. `IF NOT
    // EXISTS`, so a pre-existing target is a no-op. Done before the truncate so a
    // Truncate+insert into a freshly-created table can't fail on a missing table.
    if let Some(columns) = &create {
        if cancel.load(Ordering::Relaxed) {
            return (0, Some(RedError::Interrupted));
        }
        if let Err(e) = dst.create_table(&target, columns).await {
            return (0, Some(e));
        }
    }

    // Truncate+insert clears the target first (behind the UI's destructive confirm).
    if matches!(mode, CopyMode::TruncateInsert) {
        if cancel.load(Ordering::Relaxed) {
            return (0, Some(RedError::Interrupted));
        }
        if let Err(e) = dst.clear_table(&target).await {
            return (0, Some(e));
        }
    }

    // Stream the source rows in, projecting per `mapping`, emitting `CopyProgress`
    // (one tick per committed chunk) so the caller's terminal event can never be
    // overtaken by a trailing progress from a separate forwarder task.
    stream_into(
        &src,
        &dst,
        &source_sql,
        &target,
        &mapping,
        &target_columns,
        &cancel,
        0,
        |total| {
            emit(
                &events,
                None,
                Event::CopyProgress {
                    id,
                    rows: total as usize,
                },
            )
        },
    )
    .await
}

/// Stream `source_sql` from `src` into `target` on `dst`: open a full-fidelity forward
/// cursor (each source row seen exactly once, never `Value::Capped`), project each row
/// into target-column order by the source index `mapping` carries, and `insert_rows` in
/// chunks, committing per chunk so the returned count is meaningful on error/cancel.
/// `on_progress(total)` is called after each committed chunk with `base` plus the rows
/// committed so far, so a single copy reports its own running count and a multi-table
/// migrate reports a *cumulative* count across tables. `cancel` is checked between
/// chunks. Returns `(rows committed by this call, error-or-None)`. Memory is bounded by
/// [`COPY_CHUNK_ROWS`], not row count. Shared by [`copy_job`] and [`migrate_job`].
#[allow(clippy::too_many_arguments)]
pub(super) async fn stream_into(
    src: &Arc<dyn DatabaseDriver>,
    dst: &Arc<dyn DatabaseDriver>,
    source_sql: &str,
    target: &TableRef,
    mapping: &[ColumnMap],
    target_columns: &[Column],
    cancel: &Arc<AtomicBool>,
    base: u64,
    mut on_progress: impl FnMut(u64),
) -> (u64, Option<RedError>) {
    let opts = QueryOptions {
        window: COPY_CHUNK_ROWS,
        timeout: None,
        full_fidelity: true,
    };
    let cursor = match src.open_cursor(source_sql, opts).await {
        Ok(c) => c,
        Err(e) => return (0, Some(e)),
    };
    let mut committed = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return (committed, Some(RedError::Interrupted));
        }
        let window = match cursor.next_window(COPY_CHUNK_ROWS).await {
            Ok(w) => w,
            Err(e) => return (committed, Some(e)),
        };
        if !window.rows.is_empty() {
            let chunk: Vec<Vec<Value>> = window
                .rows
                .iter()
                .map(|row| {
                    mapping
                        .iter()
                        .map(|m| row.get(m.source).cloned().unwrap_or(Value::Null))
                        .collect()
                })
                .collect();
            match dst.insert_rows(target, target_columns, &chunk).await {
                Ok(n) => {
                    committed += n;
                    on_progress(base + committed);
                }
                Err(e) => return (committed, Some(e)),
            }
        }
        if window.exhausted {
            break;
        }
    }
    (committed, None)
}

/// Order `tables` so a table's foreign-key parents come **before** it (children last):
/// Kahn's algorithm over the FK edges restricted to the migrated set, ties broken by
/// input order, cycles broken by emitting the next remaining table. Only edges whose
/// *both* endpoints are in `tables` (and, when `schema` is given, in that namespace)
/// constrain the order; self-references are ignored. With v1 not yet recreating FKs the
/// order is cosmetic (the fresh tables carry no constraints), but it lands parent rows
/// first and makes the Phase-3 deferred-FK pass a drop-in.
pub(super) fn order_by_fk(tables: &[String], schema: Option<&str>, fks: &[FkEdge]) -> Vec<String> {
    use std::collections::{HashMap, HashSet};
    // Unique lowercased keys in input order, and the original display name per key.
    let mut order: Vec<String> = Vec::new();
    let mut orig: HashMap<String, String> = HashMap::new();
    for t in tables {
        let k = t.to_ascii_lowercase();
        if orig.insert(k.clone(), t.clone()).is_none() {
            order.push(k);
        }
    }
    let in_set = |t: &str| orig.contains_key(&t.to_ascii_lowercase());
    let in_scope = |s: &Option<String>| {
        schema.is_none_or(|sc| s.as_deref().is_none_or(|x| x.eq_ignore_ascii_case(sc)))
    };
    // deps[child] = parents (lowercased) it must follow.
    let mut deps: HashMap<String, HashSet<String>> =
        order.iter().map(|k| (k.clone(), HashSet::new())).collect();
    for fk in fks {
        let child = fk.from_table.to_ascii_lowercase();
        let parent = fk.to_table.to_ascii_lowercase();
        if child != parent
            && in_set(&fk.from_table)
            && in_set(&fk.to_table)
            && in_scope(&fk.from_schema)
            && in_scope(&fk.to_schema)
        {
            // `deps` was seeded with an entry for every key in `order`, and the
            // `in_set` guard proves `child` is one of them.
            #[allow(
                clippy::unwrap_used,
                reason = "deps has an entry for every in_set child"
            )]
            deps.get_mut(&child).unwrap().insert(parent);
        }
    }
    let mut done: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::with_capacity(order.len());
    while out.len() < order.len() {
        let mut progressed = false;
        for k in &order {
            if done.contains(k) {
                continue;
            }
            if deps[k].iter().all(|p| done.contains(p)) {
                out.push(orig[k].clone());
                done.insert(k.clone());
                progressed = true;
            }
        }
        if !progressed {
            // A cycle among the remaining tables: emit the next one in input order.
            match order.iter().find(|k| !done.contains(*k)) {
                Some(k) => {
                    out.push(orig[k].clone());
                    done.insert(k.clone());
                }
                None => break,
            }
        }
    }
    out
}

/// Migrate many tables from `src` into `dst` in one job: the whole-database move.
/// Orders the tables FK-parents-first ([`order_by_fk`]), skips any that already exist
/// on the target (migrate populates a *fresh* database, never appends into an existing
/// table), and for each: `describe_table` → `create_table` (column shape mapped into
/// the target dialect) → stream the rows via [`stream_into`]. Reuses the `Copy*` events
/// with a cumulative `CopyProgress`. Both ends are pinned by the caller; `cancel` is
/// checked between tables and between chunks. Returns `(total rows committed, err)`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn migrate_job(
    src: Arc<dyn DatabaseDriver>,
    dst: Arc<dyn DatabaseDriver>,
    source_schema: Option<String>,
    tables: Vec<String>,
    target_schema: Option<String>,
    cancel: Arc<AtomicBool>,
    events: Events,
    id: OpId,
) -> (u64, Option<RedError>) {
    // FK graph for ordering (best-effort: a failure just falls back to listed order).
    let fks = src.foreign_keys().await.unwrap_or_default();
    let ordered = order_by_fk(&tables, source_schema.as_deref(), &fks);

    // Tables already present on the target → skipped (never appended into).
    let existing: std::collections::HashSet<String> = match dst.list_objects().await {
        Ok(schemas) => schemas
            .iter()
            .filter(|s| {
                target_schema
                    .as_deref()
                    .is_none_or(|t| s.name.eq_ignore_ascii_case(t))
            })
            .flat_map(|s| s.objects.iter().map(|o| o.name.to_ascii_lowercase()))
            .collect(),
        Err(_) => std::collections::HashSet::new(),
    };

    let mut committed = 0u64;
    // Tables actually migrated (name + their source detail), retained for the deferred
    // index/FK passes after all data lands.
    let mut migrated: Vec<(String, red_core::TableDetail)> = Vec::new();
    for table in ordered {
        if cancel.load(Ordering::Relaxed) {
            return (committed, Some(RedError::Interrupted));
        }
        if existing.contains(&table.to_ascii_lowercase()) {
            continue;
        }
        let detail = match src
            .describe_table(source_schema.as_deref().unwrap_or(""), &table)
            .await
        {
            Ok(d) => d,
            Err(e) => return (committed, Some(e)),
        };
        if detail.columns.is_empty() {
            continue; // nothing to shape a CREATE from (e.g. a 0-column view)
        }
        let target = TableRef {
            schema: target_schema.clone(),
            name: table.clone(),
        };
        if let Err(e) = dst.create_table(&target, &detail.columns).await {
            return (committed, Some(e));
        }
        // Identity mapping + target columns from the source's columns.
        let mapping: Vec<ColumnMap> = detail
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| ColumnMap {
                source: i,
                column: c.name.clone(),
                decl_type: c.type_name.clone(),
            })
            .collect();
        let target_columns: Vec<Column> = detail
            .columns
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                decl_type: c.type_name.clone(),
            })
            .collect();
        let source_ref = TableRef {
            schema: source_schema.clone(),
            name: table.clone(),
        };
        let source_sql = format!("SELECT * FROM {}", src.quote_table(&source_ref));
        let (delta, err) = stream_into(
            &src,
            &dst,
            &source_sql,
            &target,
            &mapping,
            &target_columns,
            &cancel,
            committed,
            |total| {
                emit(
                    &events,
                    None,
                    Event::CopyProgress {
                        id,
                        rows: total as usize,
                    },
                )
            },
        )
        .await;
        committed += delta;
        if let Some(e) = err {
            return (committed, Some(e));
        }
        migrated.push((table, detail));
    }

    // Deferred index pass: recreate secondary indexes after the data loads, skipping
    // the primary-key-backing / engine-auto index (already created with the table).
    // Best-effort; a failed index is logged, not fatal (the data is already in).
    for (table, detail) in &migrated {
        if cancel.load(Ordering::Relaxed) {
            return (committed, Some(RedError::Interrupted));
        }
        let pk: std::collections::HashSet<String> = detail
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.to_ascii_lowercase())
            .collect();
        let target = TableRef {
            schema: target_schema.clone(),
            name: table.clone(),
        };
        for idx in &detail.indexes {
            let cols: std::collections::HashSet<String> =
                idx.columns.iter().map(|c| c.to_ascii_lowercase()).collect();
            let lname = idx.name.to_ascii_lowercase();
            let backs_pk = !pk.is_empty() && cols == pk;
            let pk_named = lname == "primary"
                || lname.starts_with("sqlite_autoindex")
                || lname.ends_with("_pkey");
            if idx.columns.is_empty() || backs_pk || pk_named {
                continue;
            }
            if let Err(e) = dst
                .create_index(&target, &idx.name, idx.unique, &idx.columns)
                .await
            {
                tracing::warn!(table = %table, index = %idx.name, error = %e, "migrate: index recreation skipped");
            }
        }
    }

    // Deferred FK pass: recreate foreign keys among the migrated set now that every
    // table exists + is filled (so dependency order can't block). Best-effort: logged,
    // not fatal, and a no-op on engines that can't `ALTER … ADD a foreign key (SQLite).
    let migrated_set: std::collections::HashSet<String> = migrated
        .iter()
        .map(|(t, _)| t.to_ascii_lowercase())
        .collect();
    let in_scope = |s: &Option<String>| {
        source_schema
            .as_deref()
            .is_none_or(|sc| s.as_deref().is_none_or(|x| x.eq_ignore_ascii_case(sc)))
    };
    for fk in &fks {
        if cancel.load(Ordering::Relaxed) {
            return (committed, Some(RedError::Interrupted));
        }
        // Only FKs whose both endpoints were migrated (and, when scoped, in the source
        // schema), mirroring `order_by_fk`'s in-scope rule.
        if !migrated_set.contains(&fk.from_table.to_ascii_lowercase())
            || !migrated_set.contains(&fk.to_table.to_ascii_lowercase())
            || !in_scope(&fk.from_schema)
            || !in_scope(&fk.to_schema)
        {
            continue;
        }
        let child = TableRef {
            schema: target_schema.clone(),
            name: fk.from_table.clone(),
        };
        let parent = TableRef {
            schema: target_schema.clone(),
            name: fk.to_table.clone(),
        };
        let cols: Vec<String> = fk.columns.iter().map(|(f, _)| f.clone()).collect();
        let refs: Vec<String> = fk.columns.iter().map(|(_, t)| t.clone()).collect();
        if let Err(e) = dst.add_foreign_key(&child, &cols, &parent, &refs).await {
            tracing::warn!(child = %fk.from_table, parent = %fk.to_table, error = %e, "migrate: foreign key skipped");
        }
    }

    (committed, None)
}

/// Rows scanned (summed across both sides) between `DiffProgress` emissions.
const DIFF_PROGRESS_STEP: usize = 20_000;
/// Max diff rows retained in memory (added + removed + changed); further diffs are
/// counted in the summary but not stored (the UI shows a truncation marker). Only
/// the *diffs* are held — unchanged rows are counted, never kept — so a mostly-equal
/// diff of huge tables stays cheap.
const DIFF_ROW_CAP: usize = 20_000;

/// Compare two tables by a shared key: read both sides key-ordered at full fidelity
/// and merge-walk them, classifying each row as added / removed / changed (see
/// docs/plans/todo/data-diff.md). The structural counterpart to [`copy_job`] — same
/// two-cursor read seam and bounded windows, but instead of writing the source into
/// the target it aligns the two streams and records their differences.
///
/// Columns are aligned by name (case-insensitive): the compared set is those present
/// on both sides (in the left table's order); columns on only one side are reported
/// but never compared. `key` must be a compared column. Both cursors open full
/// fidelity so a long TEXT/blob compares byte-exact (a `Value::Capped` would never
/// equal the whole cell it was truncated from, reading every long cell as "changed").
/// One window per side is resident, so memory is bounded by the window + the diff-row
/// cap, never by table size. `cancel` is checked between windows.
#[allow(clippy::too_many_arguments)]
pub(super) async fn diff_job(
    left_driver: Arc<dyn DatabaseDriver>,
    left: TableRef,
    right_driver: Arc<dyn DatabaseDriver>,
    right: TableRef,
    key: String,
    cancel: Arc<AtomicBool>,
    events: Events,
    id: OpId,
) -> Result<
    (
        red_core::diff::DiffColumnPlan,
        red_core::diff::DiffAccumulator,
    ),
    RedError,
> {
    use red_core::diff::{DiffAccumulator, DiffColumnPlan};

    // An empty `key` means "align on the left table's primary key": describe it and
    // use its single PK column. This lets the UI trigger a diff from two table names
    // alone, without resolving the key itself. A composite / absent PK is an error
    // the caller surfaces (the user must name a key column).
    let key = if key.is_empty() {
        let detail = left_driver
            .describe_table(left.schema.as_deref().unwrap_or(""), &left.name)
            .await?;
        let pk: Vec<&str> = detail
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.as_str())
            .collect();
        match pk.as_slice() {
            [only] => (*only).to_string(),
            [] => {
                return Err(RedError::Query(format!(
                    "'{}' has no primary key; pick a key column to compare by",
                    left.name
                )));
            }
            _ => {
                return Err(RedError::Query(format!(
                    "'{}' has a composite primary key; pick a single key column to compare by",
                    left.name
                )));
            }
        }
    } else {
        key
    };

    // Key-ordered, full-fidelity reads of both whole tables. The key is quoted by
    // the engine's own helper; the table by `quote_table` — no raw UI text.
    let opts = QueryOptions {
        window: COPY_CHUNK_ROWS,
        timeout: None,
        full_fidelity: true,
    };
    let left_sql = format!(
        "SELECT * FROM {} ORDER BY {}",
        left_driver.quote_table(&left),
        left_driver.quote_ident(&key),
    );
    let right_sql = format!(
        "SELECT * FROM {} ORDER BY {}",
        right_driver.quote_table(&right),
        right_driver.quote_ident(&key),
    );
    let left_cursor = left_driver.open_cursor(&left_sql, opts.clone()).await?;
    let right_cursor = right_driver.open_cursor(&right_sql, opts).await?;

    // Column names in each side's own order (from the cursor, no extra describe).
    let left_cols: Vec<String> = left_cursor
        .columns()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    let right_cols: Vec<String> = right_cursor
        .columns()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    let find = |cols: &[String], name: &str| cols.iter().position(|c| c.eq_ignore_ascii_case(name));

    // Align columns by name: compared = present on both (left's order); the rest
    // are one-side-only. Each compared column carries its (left_idx, right_idx) so a
    // row is projected into the shared compared order regardless of the raw layout.
    let mut columns: Vec<String> = Vec::new();
    let mut proj: Vec<(usize, usize)> = Vec::new();
    let mut left_only: Vec<String> = Vec::new();
    for (li, name) in left_cols.iter().enumerate() {
        match find(&right_cols, name) {
            Some(ri) => {
                columns.push(name.clone());
                proj.push((li, ri));
            }
            None => left_only.push(name.clone()),
        }
    }
    let right_only: Vec<String> = right_cols
        .iter()
        .filter(|name| find(&left_cols, name).is_none())
        .cloned()
        .collect();

    let Some(key_index) = columns.iter().position(|c| c.eq_ignore_ascii_case(&key)) else {
        return Err(RedError::Query(format!(
            "key column '{key}' is not present on both tables"
        )));
    };

    let plan = DiffColumnPlan {
        key: columns[key_index].clone(),
        columns,
        left_only,
        right_only,
    };

    // Project a raw row into the shared compared-column order.
    let project = |row: &[Value], side_left: bool| -> Vec<Value> {
        proj.iter()
            .map(|(li, ri)| {
                let idx = if side_left { *li } else { *ri };
                row.get(idx).cloned().unwrap_or(Value::Null)
            })
            .collect()
    };

    let mut acc = DiffAccumulator::new(key_index, DIFF_ROW_CAP);
    let mut left_buf: std::collections::VecDeque<Vec<Value>> = std::collections::VecDeque::new();
    let mut right_buf: std::collections::VecDeque<Vec<Value>> = std::collections::VecDeque::new();
    let (mut left_done, mut right_done) = (false, false);
    let mut scanned = 0usize;
    let mut since_tick = 0usize;

    // Refill a side's projected-row buffer from the next window when it drains.
    macro_rules! refill {
        ($buf:ident, $cursor:ident, $done:ident, $left:expr) => {
            if $buf.is_empty() && !$done {
                let window = $cursor.next_window(COPY_CHUNK_ROWS).await?;
                for row in &window.rows {
                    $buf.push_back(project(row, $left));
                }
                scanned += window.rows.len();
                since_tick += window.rows.len();
                if window.exhausted {
                    $done = true;
                }
            }
        };
    }

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(RedError::Interrupted);
        }
        refill!(left_buf, left_cursor, left_done, true);
        refill!(right_buf, right_cursor, right_done, false);
        if since_tick >= DIFF_PROGRESS_STEP {
            since_tick = 0;
            emit(&events, None, Event::DiffProgress { id, scanned });
        }
        let advance = acc.step(
            left_buf.front().map(Vec::as_slice),
            right_buf.front().map(Vec::as_slice),
        );
        match advance {
            red_core::diff::Advance::Done => break,
            red_core::diff::Advance::Left => {
                left_buf.pop_front();
            }
            red_core::diff::Advance::Right => {
                right_buf.pop_front();
            }
            red_core::diff::Advance::Both => {
                left_buf.pop_front();
                right_buf.pop_front();
            }
        }
    }

    Ok((plan, acc))
}

#[cfg(test)]
mod order_tests {
    use super::*;

    fn fk(from: &str, to: &str) -> FkEdge {
        FkEdge {
            from_schema: None,
            from_table: from.into(),
            to_schema: None,
            to_table: to.into(),
            columns: vec![],
        }
    }

    #[test]
    fn orders_fk_parents_before_children() {
        let tables = vec!["child".to_string(), "parent".to_string()];
        // child → parent, so parent must be created/filled first.
        let out = order_by_fk(&tables, None, &[fk("child", "parent")]);
        assert_eq!(out, vec!["parent".to_string(), "child".to_string()]);
    }

    #[test]
    fn falls_back_to_input_order_without_fks() {
        let tables = vec!["b".to_string(), "a".to_string()];
        assert_eq!(order_by_fk(&tables, None, &[]), tables);
    }

    #[test]
    fn ignores_edges_to_tables_outside_the_migrated_set() {
        // `child → outsider` doesn't constrain order (outsider isn't migrated).
        let tables = vec!["child".to_string(), "parent".to_string()];
        let out = order_by_fk(&tables, None, &[fk("child", "outsider")]);
        assert_eq!(out, tables);
    }

    #[test]
    fn tolerates_cycles_and_self_refs() {
        let tables = vec!["x".to_string(), "y".to_string()];
        // x↔y is a cycle and x→x a self-ref; every table is still emitted exactly once.
        let out = order_by_fk(&tables, None, &[fk("x", "y"), fk("y", "x"), fk("x", "x")]);
        assert_eq!(out.len(), 2);
        assert!(out.contains(&"x".to_string()) && out.contains(&"y".to_string()));
    }
}
