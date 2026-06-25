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
use std::io::BufWriter;
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
    Column, ColumnMeta, ColumnValue, EditOp, ExportFormat, FkEdge, ForeignKeyMeta, IndexMeta,
    KeySpec, ObjectKind, ObjectMeta, QueryOptions, QueryPlan, RedError, Result, ResultPage,
    RowWindow, SchemaMeta, TableDetail, TableRef, Value,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{mpsc, Mutex};

use crate::format::{strip_trailing, ExportWriter, ProgressThrottle};
use crate::{
    driver_err, AbortSignal, ArmGuard, CancelToken, CellCap, DatabaseDriver, PageCap, QueryCursor,
};

/// A live MySQL/MariaDB session. Holds a connection `Pool`: cursors take a
/// dedicated connection for the duration of their stream, and the out-of-band
/// `KILL QUERY` cancel borrows a fresh connection from the same pool.
pub struct MysqlDriver {
    pool: Pool,
    version: String,
    /// Read-only posture. Every write transaction is opened with `START
    /// TRANSACTION READ ONLY` (not a bare `BEGIN`) so the engine rejects writes
    /// per-transaction — the session-level `SET SESSION TRANSACTION READ ONLY`
    /// from `init` does not survive a pooled connection being reset on reclaim.
    read_only: bool,
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
        // `CLIENT_FOUND_ROWS`: report *matched* rows from `affected_rows`, like
        // Postgres and SQLite. Without it a no-op `UPDATE` (PK matched, value
        // unchanged) reports 0 and `apply_edit`'s `affected != 1` check would roll
        // an otherwise-valid edit back — a cross-driver behaviour difference.
        let mut builder = OptsBuilder::from_opts(opts).client_found_rows(true);
        if read_only {
            // Runs on each new pooled connection, so writes are rejected at the
            // engine even in autocommit (each statement is its own read-only txn).
            builder = builder.init(vec!["SET SESSION TRANSACTION READ ONLY"]);
        }
        let pool = Pool::new(builder);

        let mut conn = pool.get_conn().await.map_err(map_connect_err)?;
        let version: Option<String> = conn
            .query_first("SELECT VERSION()")
            .await
            .map_err(driver_err)?;
        drop(conn);

        Ok(Self {
            pool,
            version: version.unwrap_or_default(),
            read_only,
            scope: None,
        })
    }

    /// The statement that opens a write transaction. On a read-only connection it
    /// is `START TRANSACTION READ ONLY` so the engine rejects any write in the
    /// batch per-transaction — robust even when a pooled connection's session-level
    /// read-only posture was wiped by a reset on reclaim. See the `read_only` field.
    fn begin_stmt(&self) -> &'static str {
        if self.read_only {
            "START TRANSACTION READ ONLY"
        } else {
            "BEGIN"
        }
    }

    /// Restrict the schema tree to a single database. An empty name clears the
    /// scope (browse all databases). See the `scope` field.
    pub fn with_scope(mut self, database: Option<String>) -> Self {
        self.scope = database.filter(|d| !d.is_empty());
        self
    }

    /// An out-of-band cancel that `KILL QUERY <conn_id>`s on a *separate* pooled
    /// connection — MySQL has no in-band cancel-request protocol. Shared by the
    /// streaming cursor and the one-shot fetches.
    ///
    /// `alive` guards the connection-reuse race: MySQL thread ids are recycled, so
    /// a `KILL` spawned by an `abort` that fired just as the query finished could
    /// otherwise land on the *next* fetch handed the same id. The fetch flips
    /// `alive` to `false` the instant it's done with its connection (before the
    /// connection returns to the pool); the spawned `KILL` re-checks `alive` right
    /// before firing, so a recycled id is left alone. (A genuine in-flight abort
    /// still finds `alive == true` and kills the right query.)
    fn kill_token(&self, conn_id: u32, alive: Arc<AtomicBool>) -> CancelToken {
        let pool = self.pool.clone();
        CancelToken::new(move || {
            let pool = pool.clone();
            let alive = alive.clone();
            tokio::spawn(async move {
                // Cheap pre-check, then a second check once we hold the kill
                // connection — narrowing the window between "is our query still
                // running?" and issuing the KILL to acquiring that connection.
                if !alive.load(Ordering::SeqCst) {
                    return;
                }
                if let Ok(mut c) = pool.get_conn().await {
                    if alive.load(Ordering::SeqCst) {
                        let _ = c.query_drop(format!("KILL QUERY {conn_id}")).await;
                    }
                }
            });
        })
    }

    /// Arm `abort` with a `KILL QUERY` for `conn_id`, returning a guard that — on
    /// drop at fetch completion — both disarms the signal *and* marks the fetch
    /// finished (so a concurrently-spawned `KILL` skips the now-recycled id). The
    /// guard is declared after the `Conn` at every call site, so it drops (and
    /// flips `alive`) before the connection returns to the pool. See [`kill_token`].
    fn arm_kill(&self, abort: &AbortSignal, conn_id: u32) -> KillGuard {
        let alive = Arc::new(AtomicBool::new(true));
        let arm = abort.arm(self.kill_token(conn_id, alive.clone()));
        KillGuard { _arm: arm, alive }
    }

    /// Run a prepared display-capped seek query on a dedicated pooled connection,
    /// armed for cancellation. Shared by `fetch_seek` / `fetch_seek_skip` (they
    /// differ only in the SQL and its bound parameters).
    async fn exec_seek(
        &self,
        sql: &str,
        params: Vec<MyValue>,
        key: &KeySpec,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        let _guard = self.arm_kill(abort, conn.id());
        if abort.is_aborted() {
            return Err(RedError::Interrupted);
        }
        let stmt = conn.prep(sql).await.map_err(map_my_err)?;
        let columns: Vec<Column> = stmt.columns().iter().map(col_meta).collect();
        let rows: Vec<Row> = if params.is_empty() {
            conn.exec(&stmt, ()).await.map_err(map_my_err)?
        } else {
            conn.exec(&stmt, params).await.map_err(map_my_err)?
        };
        let cap = CellCap::display(crate::key_positions(key, &columns));
        Ok(ResultPage {
            rows: rows.iter().map(|r| my_row(r, cap)).collect(),
            columns,
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
        // the thread running this cursor's query. `alive` guards the same
        // connection-reuse race as the one-shot fetches (see `kill_token`): the
        // pump flips it false before its connection returns to the pool.
        let alive = Arc::new(AtomicBool::new(true));
        let cancel = self.kill_token(conn_id, alive.clone());

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
            let cap = CellCap::display([None, None]);
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
            // The pump is done with the connection: mark it released *before*
            // `result`/`conn` drop it back to the pool, so a late `KILL` spawned by
            // a concurrent cancel skips the now-recyclable id (see `kill_token`).
            alive.store(false, Ordering::SeqCst);
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
            escape_ident(schema),
            escape_ident(table)
        );
        let idx_rows: Vec<Row> = conn.query(idx_sql).await.map_err(driver_err)?;
        let indexes = group_indexes(&idx_rows);

        Ok(TableDetail {
            columns,
            foreign_keys,
            indexes,
        })
    }

    async fn foreign_keys(&self) -> Result<Vec<FkEdge>> {
        // `key_column_usage` carries the referenced endpoint per FK column with a
        // reliable `ordinal_position`, so composite keys group correctly. Scope to
        // the connected database (`DATABASE()`) to match `list_objects`.
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        let rows: Vec<(
            String,
            String,
            String,
            Option<String>,
            String,
            String,
            String,
        )> = conn
            .exec(
                "SELECT table_schema, table_name, column_name, \
                        referenced_table_schema, referenced_table_name, referenced_column_name, \
                        constraint_name \
                 FROM information_schema.key_column_usage \
                 WHERE referenced_table_name IS NOT NULL AND table_schema = DATABASE() \
                 ORDER BY table_schema, table_name, constraint_name, ordinal_position",
                (),
            )
            .await
            .map_err(driver_err)?;
        let edges = crate::group_fk_edges(rows.into_iter().map(
            |(from_schema, from_table, from_column, to_schema, to_table, to_column, constraint)| {
                crate::FkRow {
                    from_schema: Some(from_schema),
                    from_table,
                    from_column,
                    to_schema,
                    to_table,
                    to_column,
                    constraint,
                }
            },
        ));
        Ok(edges)
    }

    fn contains_predicate(&self, columns: &[ColumnMeta], term: &str) -> Option<String> {
        // MySQL/MariaDB treat `\` as a string-literal escape in the default mode,
        // so the pattern's backslashes need a second doubling — `true` here.
        crate::contains_clause(
            columns,
            term,
            |s| format!("`{}`", escape_ident(s)),
            |c| format!("CAST({c} AS CHAR)"),
            "LIKE",
            true,
            true,
        )
    }

    fn eq_predicate(&self, pairs: &[ColumnValue]) -> String {
        crate::eq_clause(pairs, |c| format!("`{}`", escape_ident(c)))
    }

    async fn count(&self, sql: &str, abort: &AbortSignal) -> Result<i64> {
        let sql = format!("SELECT count(*) FROM ({}) AS _red", strip_trailing(sql));
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        let _guard = self.arm_kill(abort, conn.id());
        if abort.is_aborted() {
            return Err(RedError::Interrupted);
        }
        let n: Option<i64> = conn.query_first(&sql).await.map_err(map_my_err)?;
        Ok(n.unwrap_or(0))
    }

    async fn column_stats(
        &self,
        sql: &str,
        column: &str,
        numeric: bool,
        distinct: bool,
        abort: &AbortSignal,
    ) -> Result<red_core::ColumnStats> {
        let sql = crate::stats_sql(sql, column, numeric, distinct, |c| {
            format!("`{}`", escape_ident(c))
        });
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        let _guard = self.arm_kill(abort, conn.id());
        if abort.is_aborted() {
            return Err(RedError::Interrupted);
        }
        let stmt = conn.prep(&sql).await.map_err(map_my_err)?;
        let rows: Vec<Row> = conn.exec(&stmt, ()).await.map_err(map_my_err)?;
        // One aggregate row, mapped full-fidelity then read positionally.
        let cells = rows.first().map(|r| my_row(r, None)).unwrap_or_default();
        Ok(crate::parse_stats(&cells, numeric, distinct))
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
        let _guard = self.arm_kill(abort, conn.id());
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
        bound: Option<&[Value]>,
        descending: bool,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let base = strip_trailing(sql);
        let bound_len = bound.map_or(0, <[Value]>::len);
        let (where_clause, order_by) = crate::seek_clauses(
            key,
            bound_len,
            descending,
            false,
            |c| format!("`{}`", escape_ident(c)),
            |_| "?".into(),
        );
        let sql = format!(
            "SELECT * FROM ({base}) AS _red {where_clause}ORDER BY {order_by} LIMIT {limit}"
        );
        let params: Vec<MyValue> = bound.into_iter().flatten().map(to_my).collect();
        self.exec_seek(&sql, params, key, abort).await
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
        let base = strip_trailing(sql);
        let bound_len = from.map_or(0, <[Value]>::len);
        let (where_clause, order_by) = crate::seek_clauses(
            key,
            bound_len,
            false,
            true,
            |c| format!("`{}`", escape_ident(c)),
            |_| "?".into(),
        );
        let sql = format!(
            "SELECT * FROM ({base}) AS _red {where_clause}\
             ORDER BY {order_by} LIMIT {limit} OFFSET {skip}"
        );
        let params: Vec<MyValue> = from.into_iter().flatten().map(to_my).collect();
        self.exec_seek(&sql, params, key, abort).await
    }

    async fn key_bounds(
        &self,
        sql: &str,
        key: &KeySpec,
        abort: &AbortSignal,
    ) -> Result<Option<(i64, i64)>> {
        let col = format!("`{}`", escape_ident(&key.column));
        let sql = format!(
            "SELECT min({col}), max({col}) FROM ({}) AS _red",
            strip_trailing(sql)
        );
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        let _guard = self.arm_kill(abort, conn.id());
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
        conn.query_drop(self.begin_stmt())
            .await
            .map_err(map_my_err)?;
        match conn.query_drop(sql).await {
            Ok(()) => {
                let affected = conn.affected_rows();
                conn.query_drop("COMMIT").await.map_err(map_my_err)?;
                Ok(affected)
            }
            Err(e) => {
                crate::warn_rollback(conn.query_drop("ROLLBACK").await, "execute");
                Err(map_my_err(e))
            }
        }
    }

    async fn apply_edits(&self, ops: &[EditOp]) -> Result<u64> {
        if ops.is_empty() {
            return Ok(0);
        }
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        conn.query_drop(self.begin_stmt())
            .await
            .map_err(map_my_err)?;
        let mut total = 0u64;
        for op in ops {
            // MySQL auto-coerces a bound string into a JSON column (it validates and
            // stores), so no per-type cast is needed — `decl_type` is ignored here.
            let (sql, params) = crate::edit_sql(
                op,
                |id| format!("`{}`", escape_ident(id)),
                |_, _| "?".to_string(),
            );
            let bound: Vec<MyValue> = params.iter().map(|v| to_my(v)).collect();
            // With `CLIENT_FOUND_ROWS` set at connect, `affected_rows` reports
            // *matched* rows (like Postgres/SQLite), so a PK-matched edit reports 1
            // even when the value is unchanged — the `affected != 1` guard stays
            // consistent.
            let result = if bound.is_empty() {
                conn.exec_drop(sql.as_str(), ()).await
            } else {
                conn.exec_drop(sql.as_str(), bound).await
            };
            match result {
                Ok(()) => {
                    let affected = conn.affected_rows();
                    if affected != 1 {
                        crate::warn_rollback(conn.query_drop("ROLLBACK").await, "apply_edits");
                        return Err(crate::edit_count_err(op, affected));
                    }
                    total += affected;
                }
                Err(e) => {
                    crate::warn_rollback(conn.query_drop("ROLLBACK").await, "apply_edits");
                    return Err(map_my_err(e));
                }
            }
        }
        conn.query_drop("COMMIT").await.map_err(map_my_err)?;
        Ok(total)
    }

    async fn insert_rows(
        &self,
        table: &TableRef,
        columns: &[Column],
        rows: &[Vec<Value>],
    ) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        conn.query_drop(self.begin_stmt())
            .await
            .map_err(map_my_err)?;
        let max = crate::insert_chunk_rows(columns.len(), MYSQL_PARAM_CAP);
        let mut total = 0u64;
        for chunk in rows.chunks(max) {
            // MySQL auto-coerces a bound string into the column type (incl. JSON),
            // so no per-type cast is needed — `decl_type` is ignored.
            let (sql, params) = crate::insert_sql(
                table,
                columns,
                chunk,
                |id| format!("`{}`", escape_ident(id)),
                |_, _, _| "?".to_string(),
            );
            let bound: Vec<MyValue> = params.iter().map(|v| to_my(v)).collect();
            let result = if bound.is_empty() {
                conn.exec_drop(sql.as_str(), ()).await
            } else {
                conn.exec_drop(sql.as_str(), bound).await
            };
            match result {
                Ok(()) => total += conn.affected_rows(),
                Err(e) => {
                    crate::warn_rollback(conn.query_drop("ROLLBACK").await, "insert_rows");
                    return Err(map_my_err(e));
                }
            }
        }
        conn.query_drop("COMMIT").await.map_err(map_my_err)?;
        Ok(total)
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<QueryPlan> {
        let base = strip_trailing(sql);
        let mut conn = self.pool.get_conn().await.map_err(driver_err)?;
        if analyze {
            // `EXPLAIN ANALYZE` (MySQL 8.0.18+) yields the tree format and runs
            // the statement. MariaDB's syntax differs; that surfaces as a normal
            // error the UI shows in the plan pane.
            let rows: Vec<String> = conn
                .query(format!("EXPLAIN ANALYZE {base}"))
                .await
                .map_err(map_my_err)?;
            return Ok(crate::plan::from_text_tree(&rows.join("\n"), true));
        }
        // Prefer the readable `FORMAT=TREE` (MySQL 8.0.16+); on the engines that
        // lack it (older MySQL, MariaDB) fall back to the tabular `EXPLAIN`, which
        // every version supports, rendered as a flat node list.
        let tree: std::result::Result<Vec<String>, MyError> =
            conn.query(format!("EXPLAIN FORMAT=TREE {base}")).await;
        match tree {
            Ok(rows) if !rows.is_empty() => {
                Ok(crate::plan::from_text_tree(&rows.join("\n"), false))
            }
            _ => {
                let rows: Vec<Row> = conn
                    .query(format!("EXPLAIN {base}"))
                    .await
                    .map_err(map_my_err)?;
                Ok(plan_from_table(&rows))
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
        let out = BufWriter::new(file);
        let mut writer = ExportWriter::begin(out, format, names).map_err(driver_err)?;
        let mut throttle = ProgressThrottle::new(progress);

        // Bail on cancel: drop the writer, remove the partial file, and report
        // interruption — never leave a truncated CSV/JSON behind.
        macro_rules! bail_if_cancelled {
            () => {
                if cancel.load(Ordering::Relaxed) {
                    drop(writer);
                    let _ = std::fs::remove_file(path);
                    return Err(RedError::Interrupted);
                }
            };
        }

        while let Some(row) = result.next().await.map_err(map_my_err)? {
            bail_if_cancelled!();
            let cells = my_row(&row, None);
            writer.write_row(&cells).map_err(driver_err)?;
            throttle.tick(writer.written());
        }
        writer.finish().map_err(driver_err)
    }
}

/// Holds a one-shot fetch's `KILL` arm for the fetch's lifetime. On drop (fetch
/// done) it marks the fetch finished — *before* the connection returns to the pool
/// — then the wrapped [`ArmGuard`] disarms the signal. See [`MysqlDriver::arm_kill`].
struct KillGuard {
    _arm: ArmGuard,
    alive: Arc<AtomicBool>,
}

impl Drop for KillGuard {
    fn drop(&mut self) {
        // Flip `alive` first so an already-spawned `KILL` sees the connection is
        // being released; the `_arm` field then disarms as it drops.
        self.alive.store(false, Ordering::SeqCst);
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
        let mut rows = Vec::with_capacity(crate::window_prealloc(max));
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

/// Map a row's cells to [`Value`]s, branching `Bytes` on the column charset. With a
/// display `cap`, over-cap non-key text/blob cells come back [`Value::Capped`] —
/// the bytes past the cap (and a blob's whole payload) never reach a `Value`.
fn my_row(row: &Row, cap: Option<CellCap>) -> Vec<Value> {
    let cols = row.columns_ref();
    (0..row.len())
        .map(|i| my_value(row.as_ref(i), &cols[i], CellCap::caps(cap, i)))
        .collect()
}

/// MySQL's cap on placeholders in a prepared statement (65535), with margin; a
/// multi-row insert sub-chunks below it.
const MYSQL_PARAM_CAP: usize = 60_000;

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

/// Map a tabular `EXPLAIN` result (the `FORMAT=TREE` fallback) to a flat plan:
/// column names from the result metadata, each row's cells as display strings.
fn plan_from_table(rows: &[Row]) -> QueryPlan {
    let columns: Vec<String> = rows
        .first()
        .map(|r| {
            r.columns_ref()
                .iter()
                .map(|c| c.name_str().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let table_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| (0..r.len()).map(|i| my_cell_string(r.as_ref(i))).collect())
        .collect();
    crate::plan::from_table(columns, table_rows)
}

/// One tabular-`EXPLAIN` cell as a plain display string (no capping — plan cells
/// are tiny). Binary/temporal cells fall back to their debug form; they don't
/// appear in `EXPLAIN` output in practice.
fn my_cell_string(value: Option<&MyValue>) -> String {
    match value {
        None | Some(MyValue::NULL) => String::new(),
        Some(MyValue::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        Some(MyValue::Int(n)) => n.to_string(),
        Some(MyValue::UInt(n)) => n.to_string(),
        Some(MyValue::Float(f)) => f.to_string(),
        Some(MyValue::Double(f)) => f.to_string(),
        Some(other) => format!("{other:?}"),
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

/// Escape an identifier's embedded backticks (doubling them); callers add the
/// surrounding backticks. Named to contrast with the other drivers' `quote_ident`/
/// `pg_quote`, which both wrap *and* escape.
fn escape_ident(s: &str) -> String {
    s.replace('`', "``")
}

/// Map a failed dial to a *fatal* [`RedError::Auth`] (a credential/target the
/// user must fix) or a transient [`RedError::Connect`]. 1045 = access denied,
/// 1044 = access denied to database, 1049 = unknown database — none retry away.
/// Anything else (refused/unreachable host) stays a retryable `Connect`.
fn map_connect_err(e: MyError) -> RedError {
    if let MyError::Server(ref se) = e {
        if matches!(se.code, 1045 | 1044 | 1049) {
            return RedError::Auth(se.message.clone());
        }
    }
    RedError::Connect(e.to_string())
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
                None => {
                    // Visible skip (with `--nocapture`): a missing URL must read as
                    // "not run", never a silent pass. CI sets the URL so it runs.
                    eprintln!("SKIP {}: RED_TEST_MYSQL_URL not set", module_path!());
                    return;
                }
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
        // Track B7: the connection-wide FK graph reports the same edge.
        battery::lists_foreign_key_graph(&driver, &schema, &authors, &books).await;

        // Seed rows for the column-stats summary: author_id is 1,1,2,NULL (NULLs +
        // duplicates), narrowable by `author_id = 1`.
        driver
            .execute(&format!(
                "INSERT INTO `{authors}`(id, name) VALUES (1, 'Ada'), (2, 'Grace')"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO `{books}`(id, title, author_id) \
                 VALUES (1, 'a', 1), (2, 'b', 1), (3, 'c', 2), (4, 'd', NULL)"
            ))
            .await
            .unwrap();
        battery::column_stats_summary(
            &driver,
            &format!("SELECT * FROM `{books}`"),
            "author_id",
            "title",
            "author_id = 1",
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
    async fn filters_contains() {
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, false).await.unwrap();
        let f = tag("f");
        let schema = current_schema(&driver).await;
        driver
            .execute(&format!(
                "CREATE TABLE `{f}` \
                 (id INT PRIMARY KEY, name VARCHAR(50), note VARCHAR(50), data BLOB)"
            ))
            .await
            .unwrap();
        // Rows 1–2 carry a blob whose bytes spell "apple" (`CAST(blob AS CHAR)` on
        // MySQL yields the text) so a leaked blob cast would lift the 'apple' count.
        driver
            .execute(&format!(
                "INSERT INTO `{f}` VALUES \
                 (1,'apple','red fruit',0x6170706c65), \
                 (2,'banana','yellow',0x6170706c65), \
                 (3,'apple pie','dessert',0x00), \
                 (4,'100% juice','on sale',0x00), \
                 (5,'O''Brien','name',0x00)"
            ))
            .await
            .unwrap();
        battery::filters_contains(&driver, &schema, &f, &format!("SELECT * FROM `{f}`")).await;
        driver.execute(&format!("DROP TABLE `{f}`")).await.unwrap();
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
    async fn applies_edits_and_read_only_rejects() {
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, false).await.unwrap();
        let t = tag("edit");
        driver
            .execute(&format!(
                "CREATE TABLE `{t}` (id INT PRIMARY KEY, name VARCHAR(64))"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!("INSERT INTO `{t}` VALUES (1, 'one')"))
            .await
            .unwrap();
        let schema = current_schema(&driver).await;
        battery::applies_edits(&driver, &schema, &t).await;

        let ro = MysqlDriver::connect(&url, true).await.unwrap();
        battery::read_only_rejects_edit(&ro, &schema, &t).await;

        // Atomic batch editing (B6) on a fresh seed table.
        let tb = tag("batch");
        driver
            .execute(&format!(
                "CREATE TABLE `{tb}` (id INT PRIMARY KEY, name VARCHAR(64))"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!("INSERT INTO `{tb}` VALUES (1, 'one')"))
            .await
            .unwrap();
        battery::applies_batch_atomic(&driver, &schema, &tb).await;
        battery::read_only_rejects_batch(&ro, &schema, &tb).await;

        // Bulk insert (data import / table copy) on a fresh empty table.
        let ti = tag("insert");
        driver
            .execute(&format!(
                "CREATE TABLE `{ti}` (id INT PRIMARY KEY, name VARCHAR(64))"
            ))
            .await
            .unwrap();
        battery::inserts_rows(&driver, &schema, &ti).await;
        battery::read_only_rejects_insert_rows(&ro, &schema, &ti).await;

        driver.execute(&format!("DROP TABLE `{t}`")).await.unwrap();
        driver.execute(&format!("DROP TABLE `{tb}`")).await.unwrap();
        driver.execute(&format!("DROP TABLE `{ti}`")).await.unwrap();
    }

    #[tokio::test]
    async fn explains_a_query() {
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, false).await.unwrap();
        let t = tag("explain");
        driver
            .execute(&format!(
                "CREATE TABLE `{t}` (id INT PRIMARY KEY, name VARCHAR(50))"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!("INSERT INTO `{t}` VALUES (1, 'a'), (2, 'b')"))
            .await
            .unwrap();

        // FORMAT=TREE on MySQL 8, the tabular fallback on MariaDB — either way the
        // plan parses to ≥1 node and names the table in `raw`.
        battery::explains_query(&driver, &format!("SELECT * FROM `{t}`"), &t).await;

        driver.execute(&format!("DROP TABLE `{t}`")).await.unwrap();
    }

    #[tokio::test]
    async fn seeks_composite_sorted_key() {
        use red_core::KeyKind;
        let url = url_or_skip!();
        let driver = MysqlDriver::connect(&url, false).await.unwrap();
        let g = tag("seekcomposite");
        driver
            .execute(&format!(
                "CREATE TABLE `{g}` (id INT PRIMARY KEY, grp INT NOT NULL)"
            ))
            .await
            .unwrap();
        // `grp = id % 3` repeats heavily so equal-`grp` ties straddle page bounds.
        driver
            .execute(&format!(
                "INSERT INTO `{g}` (id, grp) WITH RECURSIVE c(x) AS \
                 (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 30) \
                 SELECT x, x % 3 FROM c"
            ))
            .await
            .unwrap();
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
        battery::seeks_composite_sorted(
            &driver,
            &format!("SELECT * FROM `{g}`"),
            &key_asc,
            &key_desc,
            30,
        )
        .await;
        driver.execute(&format!("DROP TABLE `{g}`")).await.unwrap();
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
        let key = KeySpec::single("id", KeyKind::Int);
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
