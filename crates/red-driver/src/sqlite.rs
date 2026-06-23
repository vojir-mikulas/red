//! SQLite driver. `rusqlite` is synchronous and its `Connection`/`Statement`/
//! `Rows` form a `!Send`, self-referential stack that can't cross an `.await` or
//! move between threads. So a live cursor lives entirely on one dedicated
//! blocking-pool thread that owns that stack for the cursor's lifetime and serves
//! bounded row windows over channels; the async side holds only a thin handle.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use red_core::{
    Column, ColumnMeta, EditOp, ExportFormat, ForeignKeyMeta, IndexMeta, KeySpec, ObjectKind,
    ObjectMeta, QueryOptions, QueryPlan, RedError, Result, ResultPage, RowWindow, SchemaMeta,
    TableDetail, Value,
};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, OpenFlags};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{mpsc, oneshot};

use crate::format::{strip_trailing, ExportWriter, ProgressThrottle};
use crate::{driver_err, AbortSignal, CancelToken, CellCap, DatabaseDriver, PageCap, QueryCursor};

/// A SQLite connection target: a file path (or `:memory:`) plus the read-only
/// posture. Cheap to clone — it holds no live handle.
#[derive(Debug, Clone)]
pub struct SqliteDriver {
    path: PathBuf,
    read_only: bool,
}

impl SqliteDriver {
    pub fn new(path: impl Into<PathBuf>, read_only: bool) -> Self {
        Self {
            path: path.into(),
            read_only,
        }
    }

    fn open(path: &Path, read_only: bool) -> Result<Connection> {
        let flags = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
        } else {
            OpenFlags::default()
        };
        Connection::open_with_flags(path, flags).map_err(driver_err)
    }
}

/// A request from the async handle to the cursor's blocking thread: pull up to
/// `max` rows and reply on the oneshot.
struct FetchReq {
    max: usize,
    reply: oneshot::Sender<Result<RowWindow>>,
}

/// What the blocking thread sends back once the statement is prepared.
struct CursorMeta {
    columns: Vec<Column>,
    interrupt: rusqlite::InterruptHandle,
}

#[async_trait]
impl DatabaseDriver for SqliteDriver {
    async fn ping(&self) -> Result<()> {
        let path = self.path.clone();
        let read_only = self.read_only;
        tokio::task::spawn_blocking(move || Self::open(&path, read_only).map(|_| ()))
            .await
            .map_err(driver_err)?
    }

    fn server_version(&self) -> String {
        rusqlite::version().to_string()
    }

    async fn open_cursor(&self, sql: &str, _opts: QueryOptions) -> Result<Box<dyn QueryCursor>> {
        let path = self.path.clone();
        let read_only = self.read_only;
        let sql = sql.to_string();

        // Capacity 1: the handle sends one `FetchReq` then awaits its reply
        // before sending the next, so a single slot is all that's ever in flight —
        // a bound, not a buffer. Keeps the channel honest about the "one window at
        // a time" contract rather than letting requests pile up unboundedly.
        let (req_tx, req_rx) = mpsc::channel::<FetchReq>(1);
        let (meta_tx, meta_rx) = oneshot::channel::<Result<CursorMeta>>();

        // One blocking-pool thread owns the connection + statement + rows for the
        // whole cursor lifetime. RED runs a single session with one active query,
        // so holding one pool thread per cursor is fine.
        tokio::task::spawn_blocking(move || cursor_thread(&path, &sql, read_only, meta_tx, req_rx));

        let meta = meta_rx
            .await
            .map_err(|_| RedError::Driver("cursor thread exited before init".into()))??;

        let interrupt = meta.interrupt;
        let cancel = CancelToken::new(move || interrupt.interrupt());

        Ok(Box::new(SqliteCursor {
            columns: meta.columns,
            req_tx,
            cancel,
        }))
    }

    async fn list_objects(&self) -> Result<Vec<SchemaMeta>> {
        let path = self.path.clone();
        let read_only = self.read_only;
        tokio::task::spawn_blocking(move || list_objects_blocking(&path, read_only))
            .await
            .map_err(driver_err)?
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableDetail> {
        let path = self.path.clone();
        let read_only = self.read_only;
        let schema = schema.to_string();
        let table = table.to_string();
        tokio::task::spawn_blocking(move || {
            describe_table_blocking(&path, read_only, &schema, &table)
        })
        .await
        .map_err(driver_err)?
    }

    fn contains_predicate(&self, columns: &[ColumnMeta], term: &str) -> Option<String> {
        // SQLite treats `\` literally in string literals — no extra escaping.
        crate::contains_clause(
            columns,
            term,
            quote_ident,
            |c| format!("CAST({c} AS TEXT)"),
            "LIKE",
            false,
            true,
        )
    }

    async fn count(&self, sql: &str, abort: &AbortSignal) -> Result<i64> {
        let path = self.path.clone();
        let read_only = self.read_only;
        let abort = abort.clone();
        let sql = format!("SELECT count(*) FROM ({})", strip_trailing(sql));
        tokio::task::spawn_blocking(move || {
            let conn = SqliteDriver::open(&path, read_only)?;
            let _guard = arm_interrupt(&conn, &abort);
            if abort.is_aborted() {
                return Err(RedError::Interrupted);
            }
            conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
                .map_err(map_step_err)
        })
        .await
        .map_err(driver_err)?
    }

    async fn fetch_page(
        &self,
        sql: &str,
        offset: usize,
        limit: usize,
        cap: PageCap,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let path = self.path.clone();
        let read_only = self.read_only;
        let abort = abort.clone();
        // `limit`/`offset` are `usize`, so inlining them can't inject.
        let sql = format!(
            "SELECT * FROM ({}) LIMIT {limit} OFFSET {offset}",
            strip_trailing(sql)
        );
        tokio::task::spawn_blocking(move || {
            fetch_page_blocking(&path, read_only, &sql, Vec::new(), cap, &abort)
        })
        .await
        .map_err(driver_err)?
    }

    async fn fetch_seek(
        &self,
        sql: &str,
        key: &KeySpec,
        bound: Option<&[Value]>,
        descending: bool,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let path = self.path.clone();
        let read_only = self.read_only;
        let base = strip_trailing(sql);
        let bound_len = bound.map_or(0, <[Value]>::len);
        let (where_clause, order_by) =
            crate::seek_clauses(key, bound_len, descending, false, quote_ident, |_| {
                "?".into()
            });
        let sql = format!("SELECT * FROM ({base}) {where_clause}ORDER BY {order_by} LIMIT {limit}");
        let params: Vec<rusqlite::types::Value> =
            bound.into_iter().flatten().map(to_sqlite).collect();
        let cap = PageCap::Display {
            key: Some(key.clone()),
        };
        let abort = abort.clone();
        tokio::task::spawn_blocking(move || {
            fetch_page_blocking(&path, read_only, &sql, params, cap, &abort)
        })
        .await
        .map_err(driver_err)?
    }

    async fn fetch_seek_skip(
        &self,
        sql: &str,
        key: &KeySpec,
        from: Option<&[Value]>,
        skip: usize,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let path = self.path.clone();
        let read_only = self.read_only;
        let base = strip_trailing(sql);
        let bound_len = from.map_or(0, <[Value]>::len);
        // The lower bound walks forward in sort order (inclusive); `skip`/`limit`
        // are `usize`, so inlining them can't inject.
        let (where_clause, order_by) =
            crate::seek_clauses(key, bound_len, false, true, quote_ident, |_| "?".into());
        let sql = format!(
            "SELECT * FROM ({base}) {where_clause}ORDER BY {order_by} LIMIT {limit} OFFSET {skip}"
        );
        let params: Vec<rusqlite::types::Value> =
            from.into_iter().flatten().map(to_sqlite).collect();
        let cap = PageCap::Display {
            key: Some(key.clone()),
        };
        let abort = abort.clone();
        tokio::task::spawn_blocking(move || {
            fetch_page_blocking(&path, read_only, &sql, params, cap, &abort)
        })
        .await
        .map_err(driver_err)?
    }

    async fn key_bounds(
        &self,
        sql: &str,
        key: &KeySpec,
        abort: &AbortSignal,
    ) -> Result<Option<(i64, i64)>> {
        let path = self.path.clone();
        let read_only = self.read_only;
        let abort = abort.clone();
        let col = quote_ident(&key.column);
        let sql = format!(
            "SELECT min({col}), max({col}) FROM ({})",
            strip_trailing(sql)
        );
        tokio::task::spawn_blocking(move || {
            let conn = SqliteDriver::open(&path, read_only)?;
            let _guard = arm_interrupt(&conn, &abort);
            if abort.is_aborted() {
                return Err(RedError::Interrupted);
            }
            conn.query_row(&sql, [], |row| {
                Ok(match (row.get_ref(0)?, row.get_ref(1)?) {
                    (ValueRef::Integer(min), ValueRef::Integer(max)) => Some((min, max)),
                    _ => None,
                })
            })
            .map_err(map_step_err)
        })
        .await
        .map_err(driver_err)?
    }

    async fn execute(&self, sql: &str) -> Result<u64> {
        let path = self.path.clone();
        let read_only = self.read_only;
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || execute_blocking(&path, read_only, &sql))
            .await
            .map_err(driver_err)?
    }

    async fn apply_edits(&self, ops: &[EditOp]) -> Result<u64> {
        let path = self.path.clone();
        let read_only = self.read_only;
        let ops = ops.to_vec();
        tokio::task::spawn_blocking(move || apply_edits_blocking(&path, read_only, &ops))
            .await
            .map_err(driver_err)?
    }

    async fn explain(&self, sql: &str, _analyze: bool) -> Result<QueryPlan> {
        // `EXPLAIN QUERY PLAN` is the readable plan (the bytecode `EXPLAIN` is
        // not); it never steps the statement, so it's safe regardless of the
        // (ignored — SQLite has no ANALYZE) `analyze` flag. Columns: id, parent,
        // notused, detail.
        let path = self.path.clone();
        let read_only = self.read_only;
        let sql = format!("EXPLAIN QUERY PLAN {}", strip_trailing(sql));
        tokio::task::spawn_blocking(move || {
            let conn = SqliteDriver::open(&path, read_only)?;
            let mut stmt = conn.prepare(&sql).map_err(driver_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(driver_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(map_step_err)?;
            Ok(crate::plan::from_sqlite_rows(rows))
        })
        .await
        .map_err(driver_err)?
    }

    async fn export(
        &self,
        sql: &str,
        path: &Path,
        format: ExportFormat,
        cancel: Arc<AtomicBool>,
        progress: UnboundedSender<u64>,
    ) -> Result<u64> {
        let db_path = self.path.clone();
        let read_only = self.read_only;
        let out_path = path.to_path_buf();
        let sql = format!("SELECT * FROM ({})", strip_trailing(sql));
        tokio::task::spawn_blocking(move || {
            export_blocking(
                &db_path, read_only, &sql, &out_path, format, &cancel, progress,
            )
        })
        .await
        .map_err(driver_err)?
    }
}

/// Run one statement inside a transaction: commit on success, roll back on error.
fn execute_blocking(path: &Path, read_only: bool, sql: &str) -> Result<u64> {
    let conn = SqliteDriver::open(path, read_only)?;
    conn.execute_batch("BEGIN").map_err(driver_err)?;
    match conn.execute(sql, []) {
        Ok(affected) => {
            conn.execute_batch("COMMIT").map_err(driver_err)?;
            Ok(affected as u64)
        }
        Err(e) => {
            crate::warn_rollback(conn.execute_batch("ROLLBACK"), "execute");
            Err(map_step_err(e))
        }
    }
}

/// Render and run a batch of [`EditOp`]s in one transaction, asserting each touches
/// exactly one row (rolling the whole batch back otherwise). Values are bound (`?`
/// placeholders); a read-only open rejects the write at the engine. An empty batch
/// is a no-op (no transaction opened).
fn apply_edits_blocking(path: &Path, read_only: bool, ops: &[EditOp]) -> Result<u64> {
    if ops.is_empty() {
        return Ok(0);
    }
    let conn = SqliteDriver::open(path, read_only)?;
    conn.execute_batch("BEGIN").map_err(driver_err)?;
    let mut total = 0u64;
    for op in ops {
        let (sql, params) = crate::edit_sql(op, quote_ident, |_, _| "?".to_string());
        let bound: Vec<rusqlite::types::Value> = params.iter().map(|v| to_sqlite(v)).collect();
        match conn.execute(&sql, rusqlite::params_from_iter(bound)) {
            Ok(affected) => {
                let affected = affected as u64;
                if affected != 1 {
                    crate::warn_rollback(conn.execute_batch("ROLLBACK"), "apply_edits");
                    return Err(crate::edit_count_err(op, affected));
                }
                total += affected;
            }
            Err(e) => {
                crate::warn_rollback(conn.execute_batch("ROLLBACK"), "apply_edits");
                return Err(map_step_err(e));
            }
        }
    }
    conn.execute_batch("COMMIT").map_err(driver_err)?;
    Ok(total)
}

/// Stream the result of `sql` to `path`, one row at a time — never collecting the
/// whole result in memory. Checks `cancel` per row (bailing with the partial file
/// removed) and reports the running row count through `progress` (throttled).
#[allow(clippy::too_many_arguments)]
fn export_blocking(
    path: &Path,
    read_only: bool,
    sql: &str,
    out_path: &Path,
    format: ExportFormat,
    cancel: &AtomicBool,
    progress: UnboundedSender<u64>,
) -> Result<u64> {
    let conn = SqliteDriver::open(path, read_only)?;
    let mut stmt = conn.prepare(sql).map_err(driver_err)?;
    let column_count = stmt.column_count();
    let names: Vec<String> = stmt
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();

    let file = File::create(out_path).map_err(driver_err)?;
    let out = BufWriter::new(file);
    let mut rows_iter = stmt.query([]).map_err(driver_err)?;
    let mut writer = ExportWriter::begin(out, format, names).map_err(driver_err)?;
    let mut throttle = ProgressThrottle::new(progress);

    // Bail on cancel: drop the writer, remove the partial file, and report
    // interruption — never leave a truncated CSV/JSON behind.
    macro_rules! bail_if_cancelled {
        () => {
            if cancel.load(Ordering::Relaxed) {
                drop(writer);
                let _ = std::fs::remove_file(out_path);
                return Err(RedError::Interrupted);
            }
        };
    }

    while let Some(row) = rows_iter.next().map_err(map_step_err)? {
        bail_if_cancelled!();
        let cells = extract_row(row, column_count, None)?;
        writer.write_row(&cells).map_err(driver_err)?;
        throttle.tick(writer.written());
    }
    writer.finish().map_err(driver_err)
}

/// A cell value as a bindable SQLite parameter (for seek bounds). A seek bound is
/// read from the key column, which is never capped, so `Capped` is unreachable here.
fn to_sqlite(value: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sq;
    match value {
        Value::Null => Sq::Null,
        Value::Integer(n) => Sq::Integer(*n),
        Value::Real(x) => Sq::Real(*x),
        Value::Text(s) => Sq::Text(s.clone()),
        Value::Blob(b) => Sq::Blob(b.clone()),
        Value::Capped(_) => Sq::Null,
    }
}

/// Arm `abort` with this connection's interrupt handle for the duration of the
/// returned guard. The handle is taken before any step so a cancel can abort even
/// the first one, and `sqlite3_interrupt` is safe to call from the dispatch thread
/// while the step runs on this blocking thread.
fn arm_interrupt(conn: &Connection, abort: &AbortSignal) -> crate::ArmGuard {
    let interrupt = conn.get_interrupt_handle();
    abort.arm(CancelToken::new(move || interrupt.interrupt()))
}

fn fetch_page_blocking(
    path: &Path,
    read_only: bool,
    sql: &str,
    params: Vec<rusqlite::types::Value>,
    cap: PageCap,
    abort: &AbortSignal,
) -> Result<ResultPage> {
    let conn = SqliteDriver::open(path, read_only)?;
    let _guard = arm_interrupt(&conn, abort);
    if abort.is_aborted() {
        return Err(RedError::Interrupted);
    }
    let mut stmt = conn.prepare(sql).map_err(driver_err)?;
    let column_count = stmt.column_count();
    let columns: Vec<Column> = stmt
        .columns()
        .iter()
        .map(|c| Column {
            name: c.name().to_string(),
            decl_type: c.decl_type().map(|t| t.to_string()),
        })
        .collect();
    let cap = CellCap::resolve(&cap, &columns);
    let mut rows_iter = stmt
        .query(rusqlite::params_from_iter(params))
        .map_err(driver_err)?;
    let mut rows = Vec::new();
    while let Some(row) = rows_iter.next().map_err(map_step_err)? {
        rows.push(extract_row(row, column_count, cap)?);
    }
    Ok(ResultPage { columns, rows })
}

/// Quote a SQLite identifier (schema/table) for safe interpolation into a PRAGMA
/// or `sqlite_master` query — wrap in double quotes, doubling any embedded quote.
/// PRAGMA arguments can't be bound parameters, so quoting is the injection guard.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// The tree skeleton: each database in `PRAGMA database_list` with its table/view
/// names. Names + kinds only — cheap by contract (no `COUNT(*)`, no column walk).
/// Empty namespaces are dropped except `main`, so a fresh DB still shows its root.
fn list_objects_blocking(path: &Path, read_only: bool) -> Result<Vec<SchemaMeta>> {
    let conn = SqliteDriver::open(path, read_only)?;

    let schema_names: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA database_list").map_err(driver_err)?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(driver_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(driver_err)?;
        names
    };

    let mut schemas = Vec::with_capacity(schema_names.len());
    for schema in schema_names {
        let sql = format!(
            "SELECT name, type FROM {}.sqlite_master \
             WHERE type IN ('table','view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             ORDER BY name",
            quote_ident(&schema)
        );
        let mut stmt = conn.prepare(&sql).map_err(driver_err)?;
        let objects = stmt
            .query_map([], |row| {
                let name: String = row.get(0)?;
                let kind: String = row.get(1)?;
                Ok(ObjectMeta {
                    name,
                    kind: if kind == "view" {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    },
                })
            })
            .map_err(driver_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(driver_err)?;

        if !objects.is_empty() || schema == "main" {
            schemas.push(SchemaMeta {
                name: schema,
                objects,
            });
        }
    }
    Ok(schemas)
}

/// One table's columns + foreign keys + indexes via schema-qualified PRAGMAs.
/// Reads are by positional column index so reserved PRAGMA column names
/// (`notnull`, `from`, `to`, `unique`) need no quoting.
fn describe_table_blocking(
    path: &Path,
    read_only: bool,
    schema: &str,
    table: &str,
) -> Result<TableDetail> {
    let conn = SqliteDriver::open(path, read_only)?;
    let (sq, tq) = (quote_ident(schema), quote_ident(table));

    // table_info: cid, name, type, notnull, dflt_value, pk
    let columns = {
        let mut stmt = conn
            .prepare(&format!("PRAGMA {sq}.table_info({tq})"))
            .map_err(driver_err)?;
        let rows = stmt
            .query_map([], |row| {
                let type_name: Option<String> = row.get(2)?;
                let not_null: i64 = row.get(3)?;
                let pk: i64 = row.get(5)?;
                Ok(ColumnMeta {
                    name: row.get(1)?,
                    type_name: type_name.filter(|t| !t.is_empty()),
                    not_null: not_null != 0,
                    primary_key: pk != 0,
                    default: row.get(4)?,
                })
            })
            .map_err(driver_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(driver_err)?;
        rows
    };

    // foreign_key_list: id, seq, table, from, to, on_update, on_delete, match
    let foreign_keys = {
        let mut stmt = conn
            .prepare(&format!("PRAGMA {sq}.foreign_key_list({tq})"))
            .map_err(driver_err)?;
        let rows = stmt
            .query_map([], |row| {
                let ref_column: Option<String> = row.get(4)?;
                Ok(ForeignKeyMeta {
                    column: row.get(3)?,
                    ref_table: row.get(2)?,
                    ref_column: ref_column.unwrap_or_default(),
                })
            })
            .map_err(driver_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(driver_err)?;
        rows
    };

    // index_list: seq, name, unique, origin, partial
    let index_list: Vec<(String, bool)> = {
        let mut stmt = conn
            .prepare(&format!("PRAGMA {sq}.index_list({tq})"))
            .map_err(driver_err)?;
        let rows = stmt
            .query_map([], |row| {
                let unique: i64 = row.get(2)?;
                Ok((row.get::<_, String>(1)?, unique != 0))
            })
            .map_err(driver_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(driver_err)?;
        rows
    };

    // index_info: seqno, cid, name (name is NULL for an expression column).
    let mut indexes = Vec::with_capacity(index_list.len());
    for (name, unique) in index_list {
        let mut stmt = conn
            .prepare(&format!("PRAGMA {sq}.index_info({})", quote_ident(&name)))
            .map_err(driver_err)?;
        let columns = stmt
            .query_map([], |row| row.get::<_, Option<String>>(2))
            .map_err(driver_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(driver_err)?
            .into_iter()
            .flatten()
            .collect();
        indexes.push(IndexMeta {
            name,
            unique,
            columns,
        });
    }

    Ok(TableDetail {
        columns,
        foreign_keys,
        indexes,
    })
}

/// Runs on a dedicated blocking thread. Opens the connection, prepares the
/// statement (without stepping — `open_cursor` stays cheap), reports column
/// metadata, then serves fetch requests until exhausted, errored, or the handle
/// is dropped (channel closed), at which point the statement is finalized.
fn cursor_thread(
    path: &Path,
    sql: &str,
    read_only: bool,
    meta_tx: oneshot::Sender<Result<CursorMeta>>,
    mut req_rx: mpsc::Receiver<FetchReq>,
) {
    let conn = match SqliteDriver::open(path, read_only) {
        Ok(conn) => conn,
        Err(e) => {
            let _ = meta_tx.send(Err(e));
            return;
        }
    };
    // Taken before preparing so a cancel can interrupt even the first step.
    let interrupt = conn.get_interrupt_handle();

    let mut stmt = match conn.prepare(sql) {
        Ok(stmt) => stmt,
        Err(e) => {
            let _ = meta_tx.send(Err(driver_err(e)));
            return;
        }
    };

    let column_count = stmt.column_count();
    let columns: Vec<Column> = stmt
        .columns()
        .iter()
        .map(|c| Column {
            name: c.name().to_string(),
            decl_type: c.decl_type().map(|t| t.to_string()),
        })
        .collect();

    let mut rows = match stmt.query([]) {
        Ok(rows) => rows,
        Err(e) => {
            let _ = meta_tx.send(Err(driver_err(e)));
            return;
        }
    };

    if meta_tx.send(Ok(CursorMeta { columns, interrupt })).is_err() {
        return; // handle dropped during init
    }

    while let Some(FetchReq { max, reply }) = req_rx.blocking_recv() {
        let result = fetch_window(&mut rows, column_count, max);
        let stop = result.as_ref().map(|w| w.exhausted).unwrap_or(true);
        let _ = reply.send(result);
        if stop {
            break;
        }
    }
    // conn / stmt / rows drop here → the statement is finalized.
}

/// Step up to `max` rows off the live iterator into one window.
fn fetch_window(
    rows: &mut rusqlite::Rows<'_>,
    column_count: usize,
    max: usize,
) -> Result<RowWindow> {
    // The cursor backs the editor-run / initial-window stream — offset-mode
    // display, so cap every cell (no seek key resolved here to exempt).
    let cap = CellCap::display([None, None]);
    let mut out = Vec::with_capacity(crate::window_prealloc(max));
    for _ in 0..max {
        match rows.next() {
            Ok(Some(row)) => out.push(extract_row(row, column_count, cap)?),
            Ok(None) => {
                return Ok(RowWindow {
                    rows: out,
                    exhausted: true,
                })
            }
            Err(e) => return Err(map_step_err(e)),
        }
    }
    Ok(RowWindow {
        rows: out,
        exhausted: false,
    })
}

/// Map one row's cells to [`Value`]s. With a display `cap`, a non-key over-cap
/// text cell keeps only its prefix and a non-key blob only its length — the bytes
/// past the cap are never copied (`ValueRef` borrows the step buffer, so the cap
/// is read off the slice). `cap = None` (export / clipboard re-fetch) is full
/// fidelity, and the exempt key column rides through whole either way.
fn extract_row(
    row: &rusqlite::Row<'_>,
    column_count: usize,
    cap: Option<CellCap>,
) -> Result<Vec<Value>> {
    let mut cells = Vec::with_capacity(column_count);
    for i in 0..column_count {
        let max = CellCap::caps(cap, i);
        let value = match row.get_ref(i).map_err(driver_err)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => Value::Integer(n),
            ValueRef::Real(x) => Value::Real(x),
            ValueRef::Text(s) => match max {
                Some(max) => Value::capped_text(&String::from_utf8_lossy(s), max),
                None => Value::Text(String::from_utf8_lossy(s).into_owned()),
            },
            ValueRef::Blob(b) => match max {
                Some(_) => Value::capped_blob(b.len()),
                None => Value::Blob(b.to_vec()),
            },
        };
        cells.push(value);
    }
    Ok(cells)
}

/// A `sqlite3_interrupt` during a step surfaces as `OperationInterrupted` — map
/// it to the distinct `Interrupted` variant so the service can tell a cancel from
/// a genuine query failure.
fn map_step_err(e: rusqlite::Error) -> RedError {
    if let rusqlite::Error::SqliteFailure(ffi, _) = &e {
        if ffi.code == ErrorCode::OperationInterrupted {
            return RedError::Interrupted;
        }
    }
    driver_err(e)
}

/// The async-side cursor handle: column metadata + a request channel to the
/// blocking thread + a cancel token. `Send + 'static`; dropping it closes the
/// channel and tears down the blocking cursor.
struct SqliteCursor {
    columns: Vec<Column>,
    req_tx: mpsc::Sender<FetchReq>,
    cancel: CancelToken,
}

#[async_trait]
impl QueryCursor for SqliteCursor {
    fn columns(&self) -> &[Column] {
        &self.columns
    }

    async fn next_window(&self, max: usize) -> Result<RowWindow> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.req_tx
            .send(FetchReq {
                max,
                reply: reply_tx,
            })
            .await
            .map_err(|_| RedError::Driver("cursor closed".into()))?;
        reply_rx
            .await
            .map_err(|_| RedError::Driver("cursor closed".into()))?
    }

    fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance as battery;
    use red_core::KeyKind;

    /// Generate `n` rows (1..=n) without needing a fixture table; SQLite streams
    /// the recursive CTE incrementally, which is exactly what we want to test.
    fn counting_sql(n: i64) -> String {
        format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < {n}) SELECT x FROM c"
        )
    }

    /// A unique temp-file path so introspection runs against a real on-disk DB —
    /// `:memory:` can't be used because each `open` would see a fresh empty DB.
    fn temp_db_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("red_{tag}_{}_{n}.db", std::process::id()))
    }

    #[tokio::test]
    async fn streams_in_bounded_windows() {
        let driver = SqliteDriver::new(":memory:", true);
        battery::streams_in_bounded_windows(&driver, &counting_sql(100_000), 100_000).await;
    }

    #[tokio::test]
    async fn cancel_aborts_in_flight_fetch() {
        let driver = SqliteDriver::new(":memory:", true);
        // `sqlite3_interrupt` is a no-op if no step is running yet, so the battery
        // lets the first step get under way before interrupting; the huge bound
        // keeps it busy until then.
        battery::cancel_aborts_in_flight_fetch(
            &driver,
            &counting_sql(1_000_000_000),
            std::time::Duration::from_millis(100),
        )
        .await;
    }

    #[tokio::test]
    async fn superseded_one_shot_fetch_is_cancelled() {
        let driver = SqliteDriver::new(":memory:", true);
        battery::superseded_fetch_is_cancelled(
            &driver,
            &counting_sql(1_000_000_000),
            std::time::Duration::from_millis(100),
        )
        .await;
        battery::pre_aborted_fetch_returns_immediately(&driver, &counting_sql(1_000_000_000)).await;
        battery::abort_after_completion_is_noop(&driver, &counting_sql(10)).await;
    }

    #[tokio::test]
    async fn introspects_tables_columns_fks_and_indexes() {
        let path = temp_db_path("introspect");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE authors(id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                 CREATE TABLE books(
                     id INTEGER PRIMARY KEY,
                     title TEXT NOT NULL DEFAULT 'untitled',
                     author_id INTEGER REFERENCES authors(id)
                 );
                 CREATE INDEX idx_books_author ON books(author_id);
                 CREATE VIEW recent_books AS SELECT * FROM books;",
            )
            .unwrap();
        }
        let driver = SqliteDriver::new(&path, true);

        battery::introspects_tables_columns_fks_and_indexes(
            &driver,
            "main",
            "authors",
            "books",
            "recent_books",
        )
        .await;

        // SQLite-specific extras the shared battery doesn't assert: the column
        // default and the declared type round-trip through introspection.
        let books = driver.describe_table("main", "books").await.unwrap();
        let col = |n: &str| books.columns.iter().find(|c| c.name == n).unwrap();
        assert_eq!(col("title").default.as_deref(), Some("'untitled'"));
        assert_eq!(col("author_id").type_name.as_deref(), Some("INTEGER"));

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn executes_in_transaction_and_exports() {
        let path = temp_db_path("exec");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE t(id INTEGER, name TEXT);")
                .unwrap();
        }
        let driver = SqliteDriver::new(&path, false);

        let affected = driver
            .execute("INSERT INTO t VALUES (1, 'a,b'), (2, NULL)")
            .await
            .unwrap();
        assert_eq!(affected, 2, "execute reports rows affected");

        battery::exports_csv_and_json(&driver, "SELECT * FROM t ORDER BY id", "sqlite_exec").await;

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn applies_edits_and_read_only_rejects() {
        let path = temp_db_path("edit");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);
                 INSERT INTO t VALUES (1, 'one');",
            )
            .unwrap();
        }
        let driver = SqliteDriver::new(&path, false);
        battery::applies_edits(&driver, "main", "t").await;

        // A read-only open of the same file rejects the edit at the engine.
        let ro = SqliteDriver::new(&path, true);
        battery::read_only_rejects_edit(&ro, "main", "t").await;

        // Atomic batch editing (B6) on a fresh seed table.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tb(id INTEGER PRIMARY KEY, name TEXT);
                 INSERT INTO tb VALUES (1, 'one');",
            )
            .unwrap();
        }
        battery::applies_batch_atomic(&driver, "main", "tb").await;
        battery::read_only_rejects_batch(&ro, "main", "tb").await;

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn seeks_forward_backward_and_reads_bounds() {
        let path = temp_db_path("seek");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);
                 WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 1000)
                 INSERT INTO t SELECT x, 'row ' || x FROM c;",
            )
            .unwrap();
        }
        let driver = SqliteDriver::new(&path, true);
        let key = KeySpec::single("id", KeyKind::Int);
        let sql = "SELECT * FROM t";

        battery::seeks_forward_backward_and_reads_bounds(&driver, sql, &key).await;

        // SQLite-specific extra: a non-integer key has no interpolable bounds.
        let text_key = KeySpec::single("name", KeyKind::Other);
        assert_eq!(
            driver
                .key_bounds(sql, &text_key, &AbortSignal::new())
                .await
                .unwrap(),
            None
        );

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn filters_contains() {
        let path = temp_db_path("contains");
        {
            let conn = Connection::open(&path).unwrap();
            // Rows 1–2 carry a blob whose bytes spell "apple" (`x'6170706c65'`) so
            // a blob that leaked into the cast would lift the 'apple' count to 3.
            conn.execute_batch(
                "CREATE TABLE f(id INTEGER PRIMARY KEY, name TEXT, note TEXT, data BLOB);
                 INSERT INTO f VALUES
                   (1,'apple','red fruit',x'6170706c65'),
                   (2,'banana','yellow',x'6170706c65'),
                   (3,'apple pie','dessert',x'00'),
                   (4,'100% juice','on sale',x'00'),
                   (5,'O''Brien','name',x'00');",
            )
            .unwrap();
        }
        let driver = SqliteDriver::new(&path, true);
        battery::filters_contains(&driver, "main", "f", "SELECT * FROM f").await;
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn seeks_composite_sorted_key() {
        let path = temp_db_path("seek_composite");
        {
            let conn = Connection::open(&path).unwrap();
            // `grp = id % 3` repeats heavily (each value ~10 rows), so equal-`grp`
            // ties straddle the battery's small page boundary.
            conn.execute_batch(
                "CREATE TABLE g(id INTEGER PRIMARY KEY NOT NULL, grp INTEGER NOT NULL);
                 WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 30)
                 INSERT INTO g SELECT x, x % 3 FROM c;",
            )
            .unwrap();
        }
        let driver = SqliteDriver::new(&path, true);
        let key_asc = KeySpec {
            column: "grp".into(),
            kind: KeyKind::Int,
            tiebreak: Some("id".into()),
            descending: false,
        };
        let key_desc = KeySpec {
            descending: true,
            ..key_asc.clone()
        };
        battery::seeks_composite_sorted(&driver, "SELECT * FROM g", &key_asc, &key_desc, 30).await;
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn caps_display_keeps_key_and_export() {
        let path = temp_db_path("cap");
        {
            let conn = Connection::open(&path).unwrap();
            // `hex(zeroblob(2500))` is 5000 ASCII '0' chars; `zeroblob(5000)` is a
            // 5000-byte blob — both far over the display cap.
            conn.execute_batch(
                "CREATE TABLE big(id INTEGER PRIMARY KEY, t TEXT, b BLOB);
                 INSERT INTO big VALUES (1, hex(zeroblob(2500)), zeroblob(5000));",
            )
            .unwrap();
        }
        let driver = SqliteDriver::new(&path, true);
        let key = KeySpec::single("id", KeyKind::Int);
        battery::caps_display_keeps_key_and_export(
            &driver,
            "SELECT id, t, b FROM big",
            &key,
            b'0',
            5000,
            5000,
            "sqlite_cap",
        )
        .await;
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn read_only_rejects_writes() {
        let driver = SqliteDriver::new(":memory:", true);
        battery::read_only_rejects_write(&driver, "CREATE TABLE t(x)").await;
    }

    #[tokio::test]
    async fn explains_a_query() {
        let path = temp_db_path("explain");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE widgets(id INTEGER PRIMARY KEY, name TEXT);")
                .unwrap();
        }
        let driver = SqliteDriver::new(&path, true);
        battery::explains_query(&driver, "SELECT * FROM widgets WHERE name = 'x'", "widgets").await;

        // The plan tree carries SQLite's QUERY PLAN detail text.
        let plan = driver
            .explain("SELECT * FROM widgets", false)
            .await
            .unwrap();
        assert!(!plan.nodes.is_empty());
        assert!(plan.nodes[0].label.to_uppercase().contains("WIDGETS"));

        std::fs::remove_file(&path).ok();
    }

    /// A flagged cancel bails the export and removes the partial file, so no
    /// truncated CSV is ever left behind.
    #[tokio::test]
    async fn export_cancel_removes_partial_file() {
        let driver = SqliteDriver::new(":memory:", true);
        let out = temp_db_path("export_cancel").with_extension("csv");
        let cancel = Arc::new(AtomicBool::new(true)); // already cancelled
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = driver
            .export(&counting_sql(100_000), &out, ExportFormat::Csv, cancel, tx)
            .await
            .unwrap_err();
        assert!(matches!(err, RedError::Interrupted), "cancel → Interrupted");
        assert!(!out.exists(), "partial file removed on cancel");
    }

    /// A large export reports its running row count through the progress channel
    /// and finishes with the full file intact.
    #[tokio::test]
    async fn export_reports_progress() {
        let driver = SqliteDriver::new(":memory:", true);
        let out = temp_db_path("export_progress").with_extension("csv");
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rows = driver
            .export(&counting_sql(5_000), &out, ExportFormat::Csv, cancel, tx)
            .await
            .unwrap();
        assert_eq!(rows, 5_000);
        let mut updates = Vec::new();
        while let Ok(n) = rx.try_recv() {
            updates.push(n);
        }
        assert!(!updates.is_empty(), "progress was reported");
        assert!(
            updates.iter().all(|&n| n <= 5_000),
            "progress never exceeds the total: {updates:?}"
        );
        std::fs::remove_file(&out).ok();
    }
}
