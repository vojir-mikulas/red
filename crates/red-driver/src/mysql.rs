//! MySQL / MariaDB driver — the third source of `DatabaseDriver`, proving the
//! abstraction across a second network engine. Built on `mysql_async`: a pooled
//! async connection, a streaming cursor, and **out-of-band cancel** via
//! `KILL QUERY <conn_id>` issued on a separate pooled connection (MySQL has no
//! in-band cancel-request protocol like Postgres').
//!
//! Streaming note: `mysql_async`'s `QueryResult` borrows the `Conn`, so a
//! self-owned stream behind a `Mutex` (the `PgCursor` shape) fights lifetimes.
//! Instead a task owns the connection for the cursor's life and pumps rows over a
//! bounded channel — capacity provides natural backpressure, so memory stays flat
//! over a large result and `next_window(&self)` simply drains the receiver.
//!
//! Value mapping covers the common scalar types — int/float/text/blob — with
//! date/time/decimal/json/enum/set rendered as text. Read-only sets
//! `SESSION TRANSACTION READ ONLY` on every pooled connection.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use mysql_async::consts::ColumnType;
use mysql_async::prelude::Queryable;
use mysql_async::{
    Column as MyColumn, Error as MyError, Opts, OptsBuilder, Pool, Row, Value as MyValue,
};
use red_core::{
    Column, ColumnMeta, ExportFormat, ForeignKeyMeta, IndexMeta, KeySpec, ObjectKind, ObjectMeta,
    QueryOptions, RedError, Result, ResultPage, RowWindow, SchemaMeta, TableDetail, Value,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{mpsc, Mutex};

use crate::format::{
    csv_cell, csv_record, json_string, json_value, strip_trailing, ProgressThrottle,
};
use crate::{driver_err, AbortSignal, CancelToken, CellCap, DatabaseDriver, PageCap, QueryCursor};

/// A live MySQL/MariaDB session. Holds a connection `Pool`: cursors take a
/// dedicated connection for the duration of their stream, and the out-of-band
/// `KILL QUERY` cancel borrows a fresh connection from the same pool.
pub struct MysqlDriver {
    pool: Pool,
    version: String,
    /// When set, the schema tree is restricted to this one database — the
    /// connection's chosen `database`. `None` lists every non-system database on
    /// the server (a MySQL connection can see them all). See [`Self::with_scope`].
    scope: Option<String>,
}

impl MysqlDriver {
    /// Open the pool, apply the read-only posture (on every pooled connection),
    /// verify connectivity, and read the server version.
    pub async fn connect(dsn: &str, read_only: bool) -> Result<Self> {
        let opts = Opts::from_url(dsn).map_err(|e| RedError::Connect(e.to_string()))?;
        let mut builder = OptsBuilder::from_opts(opts);
        if read_only {
            // Runs on each new pooled connection, so writes are rejected at the
            // engine even in autocommit (each statement is its own read-only txn).
            builder = builder.init(vec!["SET SESSION TRANSACTION READ ONLY"]);
        }
        let pool = Pool::new(builder);

        let mut conn = pool
            .get_conn()
            .await
            .map_err(|e| RedError::Connect(e.to_string()))?;
        let version: Option<String> = conn
            .query_first("SELECT VERSION()")
            .await
            .map_err(driver_err)?;
        drop(conn);

        Ok(Self {
            pool,
            version: version.unwrap_or_default(),
            scope: None,
        })
    }

    /// Restrict the schema tree to a single database. An empty name clears the
    /// scope (browse all databases). See the `scope` field.
    pub fn with_scope(mut self, database: Option<String>) -> Self {
        self.scope = database.filter(|d| !d.is_empty());
        self
    }

    /// An out-of-band cancel that `KILL QUERY <conn_id>`s on a *separate* pooled
    /// connection — MySQL has no in-band cancel-request protocol. Idempotent: a
    /// `KILL` that lands after the query finished (or the connection went idle) is
    /// a harmless no-op. Shared by the streaming cursor and the one-shot fetches.
    fn kill_token(&self, conn_id: u32) -> CancelToken {
        let pool = self.pool.clone();
        CancelToken::new(move || {
            let pool = pool.clone();
            tokio::spawn(async move {
                if let Ok(mut c) = pool.get_conn().await {
                    let _ = c.query_drop(format!("KILL QUERY {conn_id}")).await;
                }
            });
        })
    }
}

#[async_trait]
impl DatabaseDriver for MysqlDriver {
    async fn ping(&self) -> Result<()> {
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        conn.query_drop("SELECT 1").await.map_err(driver_err)
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    async fn open_cursor(&self, sql: &str, opts: QueryOptions) -> Result<Box<dyn QueryCursor>> {
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        let conn_id = conn.id();
        // Prepare to read columns up front without stepping rows — cheap, as the
        // contract requires; the (possibly expensive) execute happens in the task.
        let stmt = conn.prep(sql).await.map_err(map_my_err)?;
        let columns: Vec<Column> = stmt.columns().iter().map(col_meta).collect();

        // Out-of-band cancel: `KILL QUERY` on a *separate* pooled connection aborts
        // the thread running this cursor's query.
        let cancel = self.kill_token(conn_id);

        let (tx, rx) = mpsc::channel::<Result<Vec<Value>>>(opts.window.max(1));
        tokio::spawn(async move {
            let mut conn = conn;
            let mut result = match conn.exec_iter(stmt, ()).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(map_my_err(e))).await;
                    return;
                }
            };
            // Offset-mode display stream (editor run) — cap every cell, no exempt key.
            let cap = CellCap::display(None);
            loop {
                match result.next().await {
                    Ok(Some(row)) => {
                        if tx.send(Ok(my_row(&row, cap))).await.is_err() {
                            break; // cursor dropped — stop pumping.
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send(Err(map_my_err(e))).await;
                        break;
                    }
                }
            }
            // `result` then `conn` drop here, returning the connection to the pool.
        });

        Ok(Box::new(MyCursor {
            columns,
            rows: Mutex::new(rx),
            cancel,
        }))
    }

    async fn list_objects(&self) -> Result<Vec<SchemaMeta>> {
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        // Scoped to one database → just that name (empty tree if it doesn't exist);
        // otherwise every non-system database the connection can see.
        let schema_names: Vec<String> = if let Some(scope) = &self.scope {
            conn.exec(
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name = ?",
                (scope.clone(),),
            )
            .await
            .map_err(driver_err)?
        } else {
            conn.query(
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name NOT IN \
                   ('information_schema', 'performance_schema', 'mysql', 'sys') \
                 ORDER BY schema_name",
            )
            .await
            .map_err(driver_err)?
        };

        let mut schemas = Vec::with_capacity(schema_names.len());
        for schema in schema_names {
            let object_rows: Vec<(String, String)> = conn
                .exec(
                    "SELECT table_name, table_type FROM information_schema.tables \
                     WHERE table_schema = ? ORDER BY table_name",
                    (schema.clone(),),
                )
                .await
                .map_err(driver_err)?;
            let objects = object_rows
                .into_iter()
                .map(|(name, table_type)| ObjectMeta {
                    kind: if table_type == "VIEW" {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    },
                    name,
                })
                .collect();
            schemas.push(SchemaMeta {
                name: schema,
                objects,
            });
        }
        Ok(schemas)
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableDetail> {
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;

        // Columns (`column_key = 'PRI'` ⇒ primary-key member).
        let column_rows: Vec<(String, String, String, Option<String>, String)> = conn
            .exec(
                "SELECT column_name, data_type, is_nullable, column_default, column_key \
                 FROM information_schema.columns \
                 WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position",
                (schema, table),
            )
            .await
            .map_err(driver_err)?;
        let columns = column_rows
            .into_iter()
            .map(
                |(name, data_type, is_nullable, default, column_key)| ColumnMeta {
                    primary_key: column_key == "PRI",
                    not_null: is_nullable == "NO",
                    type_name: Some(data_type),
                    default,
                    name,
                },
            )
            .collect();

        // Foreign keys.
        let fk_rows: Vec<(String, String, String)> = conn
            .exec(
                "SELECT column_name, referenced_table_name, referenced_column_name \
                 FROM information_schema.key_column_usage \
                 WHERE table_schema = ? AND table_name = ? \
                   AND referenced_table_name IS NOT NULL",
                (schema, table),
            )
            .await
            .map_err(driver_err)?;
        let foreign_keys = fk_rows
            .into_iter()
            .map(|(column, ref_table, ref_column)| ForeignKeyMeta {
                column,
                ref_table,
                ref_column,
            })
            .collect();

        // Indexes: `SHOW INDEX` yields one row per (index, column); group by name
        // and order columns by `Seq_in_index`. Identifiers are backtick-quoted (no
        // bind parameters for `SHOW INDEX`).
        let idx_sql = format!(
            "SHOW INDEX FROM `{}`.`{}`",
            quote_ident(schema),
            quote_ident(table)
        );
        let idx_rows: Vec<Row> = conn.query(idx_sql).await.map_err(driver_err)?;
        let indexes = group_indexes(&idx_rows);

        Ok(TableDetail {
            columns,
            foreign_keys,
            indexes,
        })
    }

    async fn count(&self, sql: &str, abort: &AbortSignal) -> Result<i64> {
        let sql = format!("SELECT count(*) FROM ({}) AS _red", strip_trailing(sql));
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        let _guard = abort.arm(self.kill_token(conn.id()));
        if abort.is_aborted() {
            return Err(RedError::Interrupted);
        }
        let n: Option<i64> = conn.query_first(&sql).await.map_err(map_my_err)?;
        Ok(n.unwrap_or(0))
    }

    async fn fetch_page(
        &self,
        sql: &str,
        offset: usize,
        limit: usize,
        cap: PageCap,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let sql = format!(
            "SELECT * FROM ({}) AS _red LIMIT {limit} OFFSET {offset}",
            strip_trailing(sql)
        );
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        let _guard = abort.arm(self.kill_token(conn.id()));
        if abort.is_aborted() {
            return Err(RedError::Interrupted);
        }
        let stmt = conn.prep(&sql).await.map_err(map_my_err)?;
        let columns: Vec<Column> = stmt.columns().iter().map(col_meta).collect();
        let rows: Vec<Row> = conn.exec(&stmt, ()).await.map_err(map_my_err)?;
        let cap = CellCap::resolve(&cap, &columns);
        Ok(ResultPage {
            rows: rows.iter().map(|r| my_row(r, cap)).collect(),
            columns,
        })
    }

    async fn fetch_seek(
        &self,
        sql: &str,
        key: &KeySpec,
        bound: Option<&Value>,
        descending: bool,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let col = format!("`{}`", quote_ident(&key.column));
        let base = strip_trailing(sql);
        let (cmp, ord) = if descending {
            ("<", "DESC")
        } else {
            (">", "ASC")
        };
        let sql = match bound {
            Some(_) => format!(
                "SELECT * FROM ({base}) AS _red WHERE {col} {cmp} ? ORDER BY {col} {ord} LIMIT {limit}"
            ),
            None => format!("SELECT * FROM ({base}) AS _red ORDER BY {col} {ord} LIMIT {limit}"),
        };
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        let _guard = abort.arm(self.kill_token(conn.id()));
        if abort.is_aborted() {
            return Err(RedError::Interrupted);
        }
        let stmt = conn.prep(&sql).await.map_err(map_my_err)?;
        let columns: Vec<Column> = stmt.columns().iter().map(col_meta).collect();
        let rows: Vec<Row> = match bound {
            Some(value) => conn
                .exec(&stmt, (to_my(value),))
                .await
                .map_err(map_my_err)?,
            None => conn.exec(&stmt, ()).await.map_err(map_my_err)?,
        };
        let cap = CellCap::display(key_col(&columns, key));
        Ok(ResultPage {
            rows: rows.iter().map(|r| my_row(r, cap)).collect(),
            columns,
        })
    }

    async fn fetch_seek_skip(
        &self,
        sql: &str,
        key: &KeySpec,
        from: Option<&Value>,
        skip: usize,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let col = format!("`{}`", quote_ident(&key.column));
        let base = strip_trailing(sql);
        let sql = match from {
            Some(_) => format!(
                "SELECT * FROM ({base}) AS _red WHERE {col} >= ? \
                 ORDER BY {col} ASC LIMIT {limit} OFFSET {skip}"
            ),
            None => format!(
                "SELECT * FROM ({base}) AS _red ORDER BY {col} ASC LIMIT {limit} OFFSET {skip}"
            ),
        };
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        let _guard = abort.arm(self.kill_token(conn.id()));
        if abort.is_aborted() {
            return Err(RedError::Interrupted);
        }
        let stmt = conn.prep(&sql).await.map_err(map_my_err)?;
        let columns: Vec<Column> = stmt.columns().iter().map(col_meta).collect();
        let rows: Vec<Row> = match from {
            Some(value) => conn
                .exec(&stmt, (to_my(value),))
                .await
                .map_err(map_my_err)?,
            None => conn.exec(&stmt, ()).await.map_err(map_my_err)?,
        };
        let cap = CellCap::display(key_col(&columns, key));
        Ok(ResultPage {
            rows: rows.iter().map(|r| my_row(r, cap)).collect(),
            columns,
        })
    }

    async fn key_bounds(
        &self,
        sql: &str,
        key: &KeySpec,
        abort: &AbortSignal,
    ) -> Result<Option<(i64, i64)>> {
        let col = format!("`{}`", quote_ident(&key.column));
        let sql = format!(
            "SELECT min({col}), max({col}) FROM ({}) AS _red",
            strip_trailing(sql)
        );
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        let _guard = abort.arm(self.kill_token(conn.id()));
        if abort.is_aborted() {
            return Err(RedError::Interrupted);
        }
        let stmt = conn.prep(&sql).await.map_err(map_my_err)?;
        let rows: Vec<Row> = conn.exec(&stmt, ()).await.map_err(map_my_err)?;
        Ok(rows.first().map(|r| my_row(r, None)).and_then(|cells| {
            match (cells.first(), cells.get(1)) {
                (Some(Value::Integer(min)), Some(Value::Integer(max))) => Some((*min, *max)),
                _ => None,
            }
        }))
    }

    async fn execute(&self, sql: &str) -> Result<u64> {
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        conn.query_drop("BEGIN").await.map_err(map_my_err)?;
        match conn.query_drop(sql).await {
            Ok(()) => {
                let affected = conn.affected_rows();
                conn.query_drop("COMMIT").await.map_err(map_my_err)?;
                Ok(affected)
            }
            Err(e) => {
                let _ = conn.query_drop("ROLLBACK").await;
                Err(map_my_err(e))
            }
        }
    }

    async fn export(
        &self,
        sql: &str,
        path: &Path,
        format: ExportFormat,
        cancel: Arc<AtomicBool>,
        progress: UnboundedSender<u64>,
    ) -> Result<u64> {
        let sql = format!("SELECT * FROM ({}) AS _red", strip_trailing(sql));
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        let stmt = conn.prep(&sql).await.map_err(map_my_err)?;
        let names: Vec<String> = stmt
            .columns()
            .iter()
            .map(|c| c.name_str().into_owned())
            .collect();
        let mut result = conn.exec_iter(&stmt, ()).await.map_err(map_my_err)?;

        let file = File::create(path).map_err(driver_err)?;
        let mut out = BufWriter::new(file);
        let mut written: u64 = 0;
        let mut throttle = ProgressThrottle::new(progress);

        // Bail on cancel: drop the writer, remove the partial file, and report
        // interruption — never leave a truncated CSV/JSON behind.
        macro_rules! bail_if_cancelled {
            () => {
                if cancel.load(Ordering::Relaxed) {
                    drop(out);
                    let _ = std::fs::remove_file(path);
                    return Err(RedError::Interrupted);
                }
            };
        }

        match format {
            ExportFormat::Csv => {
                writeln!(out, "{}", csv_record(names.iter().map(String::as_str)))
                    .map_err(driver_err)?;
                while let Some(row) = result.next().await.map_err(map_my_err)? {
                    bail_if_cancelled!();
                    let cells = my_row(&row, None);
                    let fields: Vec<String> = cells.iter().map(csv_cell).collect();
                    writeln!(out, "{}", csv_record(fields.iter().map(String::as_str)))
                        .map_err(driver_err)?;
                    written += 1;
                    throttle.tick(written);
                }
            }
            ExportFormat::Json => {
                write!(out, "[").map_err(driver_err)?;
                while let Some(row) = result.next().await.map_err(map_my_err)? {
                    bail_if_cancelled!();
                    let cells = my_row(&row, None);
                    if written > 0 {
                        write!(out, ",").map_err(driver_err)?;
                    }
                    write!(out, "\n  {{").map_err(driver_err)?;
                    for (i, value) in cells.iter().enumerate() {
                        if i > 0 {
                            write!(out, ",").map_err(driver_err)?;
                        }
                        write!(out, "{}:{}", json_string(&names[i]), json_value(value))
                            .map_err(driver_err)?;
                    }
                    write!(out, "}}").map_err(driver_err)?;
                    written += 1;
                    throttle.tick(written);
                }
                write!(out, "\n]\n").map_err(driver_err)?;
            }
        }
        out.flush().map_err(driver_err)?;
        Ok(written)
    }
}

/// The async-side cursor: column metadata + the receiving end of the row pump (a
/// dedicated task owns the connection and feeds it) + the out-of-band cancel token.
struct MyCursor {
    columns: Vec<Column>,
    rows: Mutex<mpsc::Receiver<Result<Vec<Value>>>>,
    cancel: CancelToken,
}

#[async_trait]
impl QueryCursor for MyCursor {
    fn columns(&self) -> &[Column] {
        &self.columns
    }

    async fn next_window(&self, max: usize) -> Result<RowWindow> {
        let mut rx = self.rows.lock().await;
        let mut rows = Vec::with_capacity(max);
        for _ in 0..max {
            match rx.recv().await {
                Some(Ok(row)) => rows.push(row),
                Some(Err(e)) => return Err(e),
                None => {
                    return Ok(RowWindow {
                        rows,
                        exhausted: true,
                    })
                }
            }
        }
        Ok(RowWindow {
            rows,
            exhausted: false,
        })
    }

    fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }
}

/// Map one prepared-statement / result column to result-set [`Column`] metadata.
fn col_meta(col: &MyColumn) -> Column {
    Column {
        name: col.name_str().into_owned(),
        decl_type: Some(col_type_name(col.column_type()).to_string()),
    }
}

/// The result-column index of `key`, used to exempt it from the display cap.
fn key_col(columns: &[Column], key: &KeySpec) -> Option<usize> {
    columns.iter().position(|c| c.name == key.column)
}

/// Map a row's cells to [`Value`]s, branching `Bytes` on the column charset. With a
/// display `cap`, over-cap non-key text/blob cells come back [`Value::Capped`] —
/// the bytes past the cap (and a blob's whole payload) never reach a `Value`.
fn my_row(row: &Row, cap: Option<CellCap>) -> Vec<Value> {
    let cols = row.columns_ref();
    (0..row.len())
        .map(|i| my_value(row.as_ref(i), &cols[i], CellCap::caps(cap, i)))
        .collect()
}

/// A cell value as a bindable MySQL parameter (for seek bounds). A bound comes from
/// the key column, never capped, so `Capped` is unreachable here.
fn to_my(value: &Value) -> MyValue {
    match value {
        Value::Null | Value::Capped(_) => MyValue::NULL,
        Value::Integer(n) => MyValue::Int(*n),
        Value::Real(x) => MyValue::Double(*x),
        Value::Text(s) => MyValue::Bytes(s.clone().into_bytes()),
        Value::Blob(b) => MyValue::Bytes(b.clone()),
    }
}

fn my_value(value: Option<&MyValue>, col: &MyColumn, max: Option<usize>) -> Value {
    match value {
        None | Some(MyValue::NULL) => Value::Null,
        Some(MyValue::Int(n)) => Value::Integer(*n),
        Some(MyValue::UInt(n)) => Value::Integer(*n as i64),
        Some(MyValue::Float(f)) => Value::Real(*f as f64),
        Some(MyValue::Double(f)) => Value::Real(*f),
        Some(MyValue::Bytes(bytes)) => bytes_value(bytes, col, max),
        Some(MyValue::Date(y, mo, d, h, mi, s, us)) => {
            Value::Text(fmt_datetime(*y, *mo, *d, *h, *mi, *s, *us))
        }
        Some(MyValue::Time(neg, days, h, mi, s, us)) => {
            Value::Text(fmt_time(*neg, *days, *h, *mi, *s, *us))
        }
    }
}

/// `TEXT` and `BLOB` share type codes and differ only by charset: the binary
/// charset (`63`) marks genuine binary. `JSON` reports the binary charset but is
/// UTF-8 text, so it's the one exception that stays `Text`. With a display cap
/// (`max`), a non-key blob keeps only its length and over-cap text its prefix —
/// `mysql_async` already owns the row's bytes, but capping keeps a *page* of fat
/// cells from accumulating (and skips the second copy into the `Value`).
fn bytes_value(bytes: &[u8], col: &MyColumn, max: Option<usize>) -> Value {
    let is_binary = col.character_set() == 63 && col.column_type() != ColumnType::MYSQL_TYPE_JSON;
    if is_binary {
        match max {
            Some(_) => Value::capped_blob(bytes.len()),
            None => Value::Blob(bytes.to_vec()),
        }
    } else {
        match max {
            Some(max) => Value::capped_text(&String::from_utf8_lossy(bytes), max),
            None => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        }
    }
}

fn fmt_datetime(y: u16, mo: u8, d: u8, h: u8, mi: u8, s: u8, us: u32) -> String {
    let date = format!("{y:04}-{mo:02}-{d:02}");
    match (h, mi, s, us) {
        (0, 0, 0, 0) => date,
        (_, _, _, 0) => format!("{date} {h:02}:{mi:02}:{s:02}"),
        _ => format!("{date} {h:02}:{mi:02}:{s:02}.{us:06}"),
    }
}

fn fmt_time(neg: bool, days: u32, h: u8, mi: u8, s: u8, us: u32) -> String {
    let sign = if neg { "-" } else { "" };
    let hours = days * 24 + h as u32;
    if us == 0 {
        format!("{sign}{hours:02}:{mi:02}:{s:02}")
    } else {
        format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}")
    }
}

/// A best-effort declared-type name for a result column (feeds type-aware
/// rendering). Text fallback for the internal/replication-only types.
fn col_type_name(ct: ColumnType) -> &'static str {
    use ColumnType::*;
    match ct {
        MYSQL_TYPE_TINY => "TINYINT",
        MYSQL_TYPE_SHORT => "SMALLINT",
        MYSQL_TYPE_LONG => "INT",
        MYSQL_TYPE_INT24 => "MEDIUMINT",
        MYSQL_TYPE_LONGLONG => "BIGINT",
        MYSQL_TYPE_YEAR => "YEAR",
        MYSQL_TYPE_BIT => "BIT",
        MYSQL_TYPE_FLOAT => "FLOAT",
        MYSQL_TYPE_DOUBLE => "DOUBLE",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "DECIMAL",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "DATE",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "TIME",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "DATETIME",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "TIMESTAMP",
        MYSQL_TYPE_JSON => "JSON",
        MYSQL_TYPE_ENUM => "ENUM",
        MYSQL_TYPE_SET => "SET",
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            "BLOB"
        }
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => "VARCHAR",
        MYSQL_TYPE_STRING => "CHAR",
        MYSQL_TYPE_GEOMETRY => "GEOMETRY",
        MYSQL_TYPE_NULL => "NULL",
        _ => "TEXT",
    }
}

/// Group `SHOW INDEX` rows into one [`IndexMeta`] per index, columns ordered by
/// `Seq_in_index`. `Non_unique = 0` ⇒ a unique index.
fn group_indexes(rows: &[Row]) -> Vec<IndexMeta> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: HashMap<String, (bool, Vec<(i64, String)>)> = HashMap::new();
    for row in rows {
        let name: String = row.get("Key_name").unwrap_or_default();
        let non_unique: i64 = row.get("Non_unique").unwrap_or(1);
        let seq: i64 = row.get("Seq_in_index").unwrap_or(0);
        let column: String = row.get("Column_name").unwrap_or_default();
        let entry = by_name.entry(name.clone()).or_insert_with(|| {
            order.push(name.clone());
            (non_unique == 0, Vec::new())
        });
        entry.1.push((seq, column));
    }
    order
        .into_iter()
        .map(|name| {
            let (unique, mut cols) = by_name
                .remove(&name)
                .expect("invariant: every name in `order` was inserted into `by_name`");
            cols.sort_by_key(|(seq, _)| *seq);
            IndexMeta {
                name,
                unique,
                columns: cols.into_iter().map(|(_, c)| c).collect(),
            }
        })
        .collect()
}

/// Backtick-quote an identifier for interpolation (doubling embedded backticks).
fn quote_ident(s: &str) -> String {
    s.replace('`', "``")
}

/// Map a killed query (`KILL QUERY` → error 1317, SQLSTATE `70100`) to the
/// distinct `Interrupted`; everything else is a driver error.
fn map_my_err(e: MyError) -> RedError {
    if let MyError::Server(ref se) = e {
        if se.code == 1317 || se.code == 1927 || se.state == "70100" {
            return RedError::Interrupted;
        }
    }
    driver_err(e)
}

// Tests run against a live MariaDB/MySQL provided via `RED_TEST_MYSQL_URL`, so CI
// without a server skips cleanly. Spin one up with:
//
//   docker run --rm -d -p 3306:3306 -e MARIADB_ROOT_PASSWORD=red \
//     -e MARIADB_DATABASE=red_test --name red-maria mariadb:11
//   export RED_TEST_MYSQL_URL='mysql://root:red@127.0.0.1:3306/red_test'
#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance as battery;

    fn test_url() -> Option<String> {
        std::env::var("RED_TEST_MYSQL_URL").ok()
    }

    /// A unique fixture-table suffix so concurrent tests don't collide on a shared
    /// server.
    fn tag(name: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        format!("red_{name}_{}_{n}", std::process::id())
    }

    macro_rules! url_or_skip {
        () => {
            match test_url() {
                Some(u) => u,
                None => return,
            }
        };
    }

    #[tokio::test]
    async fn connect_reports_version() {
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, true).await.unwrap();
        assert!(!driver.server_version().is_empty());
        driver.ping().await.unwrap();
    }

    #[tokio::test]
    async fn streams_in_bounded_windows() {
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, true).await.unwrap();
        // A cross join over information_schema gives a large, server-agnostic row
        // source without needing to seed a fixture; LIMIT pins the exact count.
        let sql = "SELECT a.ORDINAL_POSITION FROM information_schema.columns a \
                   CROSS JOIN information_schema.columns b LIMIT 100000";
        battery::streams_in_bounded_windows(&driver, sql, 100_000).await;
    }

    #[tokio::test]
    async fn cancel_aborts_in_flight_fetch() {
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, true).await.unwrap();
        // A heavy, unbounded cross join keeps the engine busy long enough to KILL
        // mid-flight. (SLEEP is unsuitable: KILL QUERY makes it return 1, not 1317.)
        let sql = "SELECT a.ORDINAL_POSITION FROM information_schema.columns a \
                   CROSS JOIN information_schema.columns b \
                   CROSS JOIN information_schema.columns c";
        battery::cancel_aborts_in_flight_fetch(&driver, sql, std::time::Duration::from_millis(200))
            .await;
    }

    #[tokio::test]
    async fn superseded_one_shot_fetch_is_cancelled() {
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, true).await.unwrap();
        // count(*) over a heavy triple cross join keeps the engine busy to KILL.
        let heavy = "SELECT a.ORDINAL_POSITION FROM information_schema.columns a \
                     CROSS JOIN information_schema.columns b \
                     CROSS JOIN information_schema.columns c";
        battery::superseded_fetch_is_cancelled(
            &driver,
            heavy,
            std::time::Duration::from_millis(200),
        )
        .await;
        battery::pre_aborted_fetch_returns_immediately(&driver, heavy).await;
        battery::abort_after_completion_is_noop(&driver, "SELECT 1").await;
    }

    #[tokio::test]
    async fn introspects_tables_columns_fks_and_indexes() {
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, false).await.unwrap();
        let authors = tag("authors");
        let books = tag("books");
        let recent = tag("recent");

        driver
            .execute(&format!(
                "CREATE TABLE `{authors}` (id INT PRIMARY KEY, name VARCHAR(100) NOT NULL)"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "CREATE TABLE `{books}` (\
                   id INT PRIMARY KEY, \
                   title VARCHAR(200) NOT NULL DEFAULT 'untitled', \
                   author_id INT, \
                   KEY idx_author (author_id), \
                   CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES `{authors}`(id))"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "CREATE VIEW `{recent}` AS SELECT * FROM `{books}`"
            ))
            .await
            .unwrap();

        let schema = current_schema(&driver).await;
        battery::introspects_tables_columns_fks_and_indexes(
            &driver, &schema, &authors, &books, &recent,
        )
        .await;

        for obj in [
            format!("VIEW `{recent}`"),
            format!("TABLE `{books}`"),
            format!("TABLE `{authors}`"),
        ] {
            driver.execute(&format!("DROP {obj}")).await.unwrap();
        }
    }

    #[tokio::test]
    async fn executes_in_transaction_and_exports() {
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, false).await.unwrap();
        let t = tag("t");
        driver
            .execute(&format!("CREATE TABLE `{t}` (id INT, name VARCHAR(50))"))
            .await
            .unwrap();

        let affected = driver
            .execute(&format!("INSERT INTO `{t}` VALUES (1, 'a,b'), (2, NULL)"))
            .await
            .unwrap();
        assert_eq!(affected, 2, "execute reports rows affected");

        battery::exports_csv_and_json(&driver, &format!("SELECT * FROM `{t}` ORDER BY id"), &t)
            .await;

        driver.execute(&format!("DROP TABLE `{t}`")).await.unwrap();
    }

    #[tokio::test]
    async fn read_only_rejects_writes() {
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, true).await.unwrap();
        battery::read_only_rejects_write(&driver, "CREATE TABLE red_ro_should_fail (x INT)").await;
    }

    #[tokio::test]
    async fn caps_display_keeps_key_and_export() {
        use red_core::KeyKind;
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, false).await.unwrap();
        let t = tag("cap");
        driver
            .execute(&format!(
                "CREATE TABLE `{t}` (id INT PRIMARY KEY, t TEXT, b BLOB)"
            ))
            .await
            .unwrap();
        // One row whose text and blob both far exceed the display cap.
        driver
            .execute(&format!(
                "INSERT INTO `{t}` VALUES (1, REPEAT('a', 5000), REPEAT('a', 5000))"
            ))
            .await
            .unwrap();
        let key = KeySpec {
            column: "id".into(),
            kind: KeyKind::Int,
        };
        battery::caps_display_keeps_key_and_export(
            &driver,
            &format!("SELECT id, t, b FROM `{t}`"),
            &key,
            b'a',
            5000,
            5000,
            &t,
        )
        .await;
        driver.execute(&format!("DROP TABLE `{t}`")).await.unwrap();
    }

    /// The connection's current database — fixtures live here, so introspection
    /// filters to it.
    async fn current_schema(driver: &MysqlDriver) -> String {
        let mut conn = driver.pool.get_conn().await.unwrap();
        let db: Option<String> = conn.query_first("SELECT DATABASE()").await.unwrap();
        db.expect("connection must target a database")
    }

    #[tokio::test]
    async fn scope_restricts_schema_tree() {
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, false).await.unwrap();
        let schema = current_schema(&driver).await;

        // Unscoped: the current database is one of several visible namespaces.
        let unscoped = driver.list_objects().await.unwrap();
        assert!(unscoped.iter().any(|s| s.name == schema));

        // Scoped to the current database: exactly that one namespace.
        let scoped = MysqlDriver::connect(&url, false)
            .await
            .unwrap()
            .with_scope(Some(schema.clone()));
        let names: Vec<String> = scoped
            .list_objects()
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec![schema.clone()]);

        // A scope that names no real database yields an empty tree, not an error.
        let missing = MysqlDriver::connect(&url, false)
            .await
            .unwrap()
            .with_scope(Some("red_no_such_db_xyz".into()));
        assert!(missing.list_objects().await.unwrap().is_empty());

        // An empty scope clears it — the current database is visible again.
        let cleared = MysqlDriver::connect(&url, false)
            .await
            .unwrap()
            .with_scope(Some(String::new()));
        assert!(cleared
            .list_objects()
            .await
            .unwrap()
            .iter()
            .any(|s| s.name == schema));
    }
}
