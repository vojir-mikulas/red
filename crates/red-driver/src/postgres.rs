//! PostgreSQL driver: the second source of `DatabaseDriver`, proving the
//! abstraction on a real network engine. Built on `tokio-postgres`: a live
//! `Client` (its connection driven by a background task), a streaming cursor over
//! `query_raw`, and **out-of-band cancel** via `tokio-postgres`'s `CancelToken`
//! (a separate cancel-request connection, not a dropped future).
//!
//! Caveats for v0.1: connections are `NoTls` (TLS is the next hardening step).
//! Value mapping covers the common scalar types, bool/int/float/text/bytea,
//! plus the richer ones a first-time visitor expects to *see* rather than as empty
//! NULLs: numeric, timestamp(tz), date, time(tz), uuid, and json(b) are rendered
//! from their binary wire form by [`crate::pg_text`] (dependency-free). Anything
//! else decodes through Postgres's string path, and a type that path rejects
//! (enum, inet, interval, array, …) falls back to its raw wire bytes as lossy UTF-8
//! rather than a silent NULL. Read-only sets `default_transaction_read_only`.

use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use red_core::{
    Column, ColumnMeta, ColumnValue, DbKind, EditOp, ExportFormat, FkEdge, FkJoin, ForeignKeyMeta,
    IndexMeta, KeySpec, ObjectKind, ObjectMeta, QueryOptions, QueryPlan, RedError, Result,
    ResultPage, RowWindow, SchemaMeta, TableDetail, TableRef, Value,
};
use std::fs::File;
use std::io::BufWriter;
use std::sync::Mutex as StdMutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, NoTls, Row, RowStream, Statement};

use crate::format::{strip_trailing, ExportWriter, ProgressThrottle};
use crate::pg_text;
use crate::{
    driver_err, AbortSignal, ArmGuard, CancelToken, CellCap, DatabaseDriver, PageCap, QueryCursor,
};

/// Warm fetch connections kept ready for the one-shot read paths. `tokio-postgres`
/// cancellation is *connection-scoped*, so running every page/seek/count on the one
/// shared `Client` would mean a superseded fetch's cancel could land on a sibling
/// fetch pipelined on the same connection. A small pool gives each cancellable
/// fetch its own connection, so its cancel hits exactly its own query. Grows
/// lazily (nothing opened until the first fetch) to respect the cold-start budget.
const FETCH_POOL_CAP: usize = 4;

/// A live PostgreSQL session. Holds the shared `Client` (cursor, introspection,
/// `execute`) plus a small lazily-grown pool of warm connections the cancellable
/// one-shot fetches borrow; see [`FETCH_POOL_CAP`].
pub struct PostgresDriver {
    client: Arc<Client>,
    version: String,
    dsn: String,
    read_only: bool,
    /// Idle fetch connections, returned after each one-shot fetch. A free list, not
    /// a semaphore: `acquire` opens a fresh connection when it's empty.
    pool: StdMutex<Vec<Arc<Client>>>,
}

/// No bind parameters; `query_raw` needs a typed iterator, so spell out the kind.
fn no_params() -> Vec<&'static (dyn ToSql + Sync)> {
    Vec::new()
}

/// Lock a mutex, tolerating poison (the free-list critical sections can't panic).
fn lock<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// An out-of-band cancel for `client`: a separate cancel request over a fresh
/// connection (not a dropped future). `client` is one connection, so this cancels
/// exactly that connection's in-flight query.
fn pg_cancel_token(client: &Client) -> CancelToken {
    let token = client.cancel_token();
    CancelToken::new(move || {
        let token = token.clone();
        tokio::spawn(async move {
            let _ = token.cancel_query(NoTls).await;
        });
    })
}

/// Prepare `sql` on `client` and read its column metadata (works for an empty result).
async fn prepare_columns(client: &Client, sql: &str) -> Result<(Statement, Vec<Column>)> {
    // Postgres validates SQL at prepare time, so a user's bad custom query
    // surfaces here; map through `map_pg_err` to keep the server's message
    // instead of the bare `"db error"` that `tokio_postgres::Error` renders.
    let stmt = client.prepare(sql).await.map_err(map_pg_err)?;
    let columns = stmt
        .columns()
        .iter()
        .map(|c| Column {
            name: c.name().to_string(),
            decl_type: Some(c.type_().name().to_string()),
        })
        .collect();
    Ok((stmt, columns))
}

impl PostgresDriver {
    /// Connect over the network, drive the connection in the background, apply the
    /// read-only posture, and read the server version.
    pub async fn connect(dsn: &str, read_only: bool) -> Result<Self> {
        // TLS for Postgres isn't wired yet (the driver dials `NoTls`; adding it
        // needs a rustls connector — tracked in `security-review-2026-07.md`).
        // Rather than silently connect in cleartext when TLS is requested, refuse
        // with an actionable message. Uses the same TLS detection as the DSN
        // parser (`sslmode=`/`ssl=true`/`tls=true`/`require_ssl=true`), not a
        // narrow `sslmode=require` substring match that a raw `?ssl=true` DSN
        // would slip past into a silent cleartext connection.
        if red_core::dsn_requests_tls(dsn) {
            return Err(RedError::Connect(
                "TLS for PostgreSQL isn't supported yet in this build — turn TLS off, \
                 or tunnel the connection over SSH instead."
                    .to_string(),
            ));
        }
        let (client, connection) = tokio_postgres::connect(dsn, NoTls)
            .await
            .map_err(map_connect_err)?;
        tokio::spawn(async move {
            // When the client drops, this resolves and the task ends.
            let _ = connection.await;
        });

        if read_only {
            client
                .batch_execute("SET default_transaction_read_only = on")
                .await
                .map_err(|e| RedError::Connect(e.to_string()))?;
        }

        let version: String = client
            .query_one("SHOW server_version", &[])
            .await
            .map_err(driver_err)?
            .get(0);

        Ok(Self {
            client: Arc::new(client),
            version,
            dsn: dsn.to_string(),
            read_only,
            pool: StdMutex::new(Vec::new()),
        })
    }

    /// Borrow a warm fetch connection: pop a live one off the free list, or open a
    /// fresh one. Dead connections (a dropped backend) are discarded, not reused.
    async fn acquire(&self) -> Result<Arc<Client>> {
        loop {
            let pooled = lock(&self.pool).pop();
            match pooled {
                Some(c) if !c.is_closed() => return Ok(c),
                Some(_) => continue, // closed: drop it and try the next
                None => break,
            }
        }
        self.open_fetch_conn().await
    }

    /// Return a fetch connection to the free list (dropping it if dead or the pool
    /// is at cap). Call only *after* disarming the fetch's cancel, so a late abort
    /// can't fire against a connection that's about to serve someone else.
    fn release(&self, client: Arc<Client>) {
        if client.is_closed() {
            return;
        }
        let mut pool = lock(&self.pool);
        if pool.len() < FETCH_POOL_CAP {
            pool.push(client);
        }
    }

    /// Open one fetch connection with the same read-only posture as the main client.
    async fn open_fetch_conn(&self) -> Result<Arc<Client>> {
        let (client, connection) = tokio_postgres::connect(&self.dsn, NoTls)
            .await
            .map_err(|e| RedError::Connect(e.to_string()))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        if self.read_only {
            client
                .batch_execute("SET default_transaction_read_only = on")
                .await
                .map_err(|e| RedError::Connect(e.to_string()))?;
        }
        Ok(Arc::new(client))
    }

    /// Run `f` on a borrowed fetch connection with `abort` armed to its cancel for
    /// the duration. Disarms *before* the connection returns to the pool, so a late
    /// `abort` never reaches a reused connection. A fetch superseded before it
    /// starts bails with `Interrupted` (a connection-scoped cancel is a no-op with
    /// nothing yet running).
    async fn with_fetch_conn<T, F, Fut>(&self, abort: &AbortSignal, f: F) -> Result<T>
    where
        F: FnOnce(Arc<Client>) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let client = self.acquire().await?;
        let guard = abort.arm(pg_cancel_token(&client));
        let result = if abort.is_aborted() {
            Err(RedError::Interrupted)
        } else {
            f(client.clone()).await
        };
        drop::<ArmGuard>(guard); // disarm before the connection is reusable
        self.release(client);
        result
    }
}

#[async_trait]
impl DatabaseDriver for PostgresDriver {
    async fn ping(&self) -> Result<()> {
        self.client
            .batch_execute("SELECT 1")
            .await
            .map_err(driver_err)
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    async fn open_cursor(&self, sql: &str, opts: QueryOptions) -> Result<Box<dyn QueryCursor>> {
        let (stmt, columns) = prepare_columns(&self.client, sql).await?;
        let stream = self
            .client
            .query_raw(&stmt, no_params())
            .await
            .map_err(driver_err)?;

        // Out-of-band cancel: a separate cancel request over a fresh connection.
        let cancel = pg_cancel_token(&self.client);

        Ok(Box::new(PgCursor {
            columns,
            stream: Mutex::new(Box::pin(stream)),
            cancel,
            full: opts.full_fidelity,
        }))
    }

    async fn list_objects(&self) -> Result<Vec<SchemaMeta>> {
        let schema_rows = self
            .client
            .query(
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name NOT IN ('pg_catalog', 'information_schema') \
                 AND schema_name NOT LIKE 'pg\\_%' ORDER BY schema_name",
                &[],
            )
            .await
            .map_err(driver_err)?;

        let mut schemas = Vec::with_capacity(schema_rows.len());
        for schema_row in schema_rows {
            let schema: String = schema_row.get(0);
            let object_rows = self
                .client
                .query(
                    "SELECT table_name, table_type FROM information_schema.tables \
                     WHERE table_schema = $1 ORDER BY table_name",
                    &[&schema],
                )
                .await
                .map_err(driver_err)?;
            let objects = object_rows
                .iter()
                .map(|row| {
                    let name: String = row.get(0);
                    let kind: String = row.get(1);
                    ObjectMeta {
                        name,
                        kind: if kind == "VIEW" {
                            ObjectKind::View
                        } else {
                            ObjectKind::Table
                        },
                    }
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
        // Primary-key columns.
        let pk_rows = self
            .client
            .query(
                "SELECT kcu.column_name FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON kcu.constraint_name = tc.constraint_name \
                  AND kcu.table_schema = tc.table_schema \
                 WHERE tc.constraint_type = 'PRIMARY KEY' \
                   AND tc.table_schema = $1 AND tc.table_name = $2",
                &[&schema, &table],
            )
            .await
            .map_err(driver_err)?;
        let pk: std::collections::HashSet<String> =
            pk_rows.iter().map(|r| r.get::<_, String>(0)).collect();

        // Columns.
        let column_rows = self
            .client
            .query(
                "SELECT column_name, data_type, is_nullable, column_default \
                 FROM information_schema.columns \
                 WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
                &[&schema, &table],
            )
            .await
            .map_err(driver_err)?;
        let columns = column_rows
            .iter()
            .map(|row| {
                let name: String = row.get(0);
                let type_name: String = row.get(1);
                let nullable: String = row.get(2);
                let default: Option<String> = row.get(3);
                // `serial`/`bigserial` columns default to `nextval('…_seq')`.
                let auto_increment = default.as_deref().is_some_and(|d| d.starts_with("nextval"));
                ColumnMeta {
                    primary_key: pk.contains(&name),
                    not_null: nullable == "NO",
                    type_name: Some(type_name),
                    default,
                    name,
                    auto_increment,
                }
            })
            .collect();

        // Foreign keys.
        let fk_rows = self
            .client
            .query(
                "SELECT kcu.column_name, ccu.table_name, ccu.column_name \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON kcu.constraint_name = tc.constraint_name \
                  AND kcu.table_schema = tc.table_schema \
                 JOIN information_schema.constraint_column_usage ccu \
                   ON ccu.constraint_name = tc.constraint_name \
                  AND ccu.table_schema = tc.table_schema \
                 WHERE tc.constraint_type = 'FOREIGN KEY' \
                   AND tc.table_schema = $1 AND tc.table_name = $2",
                &[&schema, &table],
            )
            .await
            .map_err(driver_err)?;
        let foreign_keys = fk_rows
            .iter()
            .map(|row| ForeignKeyMeta {
                column: row.get(0),
                ref_table: row.get(1),
                ref_column: row.get(2),
            })
            .collect();

        // Indexes (columns parsed out of the index definition).
        let index_rows = self
            .client
            .query(
                "SELECT indexname, indexdef FROM pg_indexes \
                 WHERE schemaname = $1 AND tablename = $2",
                &[&schema, &table],
            )
            .await
            .map_err(driver_err)?;
        let indexes = index_rows
            .iter()
            .map(|row| {
                let name: String = row.get(0);
                let def: String = row.get(1);
                IndexMeta {
                    unique: def.to_uppercase().contains("UNIQUE INDEX"),
                    columns: parse_index_columns(&def),
                    name,
                }
            })
            .collect();

        Ok(TableDetail {
            columns,
            foreign_keys,
            indexes,
        })
    }

    async fn enum_columns(
        &self,
        table: &TableRef,
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        // Each enum-typed column of the table, joined to its `pg_enum` labels in the
        // enum's own sort order. Non-enum columns simply don't join, so they're absent.
        let schema = table.schema.as_deref().unwrap_or("public");
        let rows = self
            .client
            .query(
                "SELECT a.attname, e.enumlabel \
                 FROM pg_attribute a \
                 JOIN pg_type t ON t.oid = a.atttypid \
                 JOIN pg_enum e ON e.enumtypid = t.oid \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relname = $2 AND n.nspname = $1 AND a.attnum > 0 \
                   AND NOT a.attisdropped \
                 ORDER BY a.attnum, e.enumsortorder",
                &[&schema, &table.name],
            )
            .await
            .map_err(driver_err)?;
        let mut out: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for row in &rows {
            let col: String = row.get(0);
            let label: String = row.get(1);
            out.entry(col).or_default().push(label);
        }
        Ok(out)
    }

    async fn foreign_keys(&self) -> Result<Vec<FkEdge>> {
        // One pass over the catalog: every FK column with both endpoints' schema +
        // table, ordered so a composite key's columns arrive together in key order.
        // System schemas are excluded to match `list_objects`'s visible namespaces.
        let rows = self
            .client
            .query(
                "SELECT tc.table_schema, tc.table_name, kcu.column_name, \
                        ccu.table_schema, ccu.table_name, ccu.column_name, tc.constraint_name \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON kcu.constraint_name = tc.constraint_name \
                  AND kcu.table_schema = tc.table_schema \
                 JOIN information_schema.constraint_column_usage ccu \
                   ON ccu.constraint_name = tc.constraint_name \
                  AND ccu.table_schema = tc.table_schema \
                 WHERE tc.constraint_type = 'FOREIGN KEY' \
                   AND tc.table_schema NOT IN ('pg_catalog', 'information_schema') \
                   AND tc.table_schema NOT LIKE 'pg\\_%' \
                 ORDER BY tc.table_schema, tc.table_name, tc.constraint_name, kcu.ordinal_position",
                &[],
            )
            .await
            .map_err(driver_err)?;
        let edges = crate::group_fk_edges(rows.iter().map(|r| crate::FkRow {
            from_schema: r.get(0),
            from_table: r.get(1),
            from_column: r.get(2),
            to_schema: r.get(3),
            to_table: r.get(4),
            to_column: r.get(5),
            constraint: r.get(6),
        }));
        Ok(edges)
    }

    fn contains_predicate(&self, columns: &[ColumnMeta], term: &str) -> Option<String> {
        // Postgres standard strings treat `\` literally, so no extra literal escaping.
        crate::contains_clause(
            columns,
            term,
            pg_quote,
            |c| format!("({c})::text"),
            "ILIKE",
            false,
            true,
        )
    }

    fn eq_predicate(&self, pairs: &[ColumnValue]) -> String {
        crate::eq_clause(pairs, pg_quote, false)
    }

    fn fk_join_wrap(&self, base: &str, base_cols: &[String], joins: &[FkJoin]) -> String {
        crate::join_wrap(base, base_cols, joins, pg_quote)
    }

    async fn count(&self, sql: &str, abort: &AbortSignal) -> Result<i64> {
        let sql = format!("SELECT count(*) FROM ({}) AS _red", strip_trailing(sql));
        self.with_fetch_conn(abort, |client| async move {
            let row = client.query_one(&sql, &[]).await.map_err(map_pg_err)?;
            Ok(row.get(0))
        })
        .await
    }

    async fn column_stats(
        &self,
        sql: &str,
        column: &str,
        numeric: bool,
        distinct: bool,
        abort: &AbortSignal,
    ) -> Result<red_core::ColumnStats> {
        let sql = crate::stats_sql(sql, column, numeric, distinct, pg_quote);
        self.with_fetch_conn(abort, |client| async move {
            let row = client.query_one(&sql, &[]).await.map_err(map_pg_err)?;
            // Read the one aggregate row full-fidelity, then map it positionally.
            let cells = pg_row(&row, None);
            Ok(crate::parse_stats(&cells, numeric, distinct))
        })
        .await
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
        self.with_fetch_conn(abort, |client| async move {
            let (stmt, columns) = prepare_columns(&client, &sql).await?;
            let rows = client.query(&stmt, &[]).await.map_err(map_pg_err)?;
            let cap = CellCap::resolve(&cap, &columns);
            Ok(ResultPage {
                rows: rows.iter().map(|r| pg_row(r, cap)).collect(),
                columns,
            })
        })
        .await
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
        // Each placeholder carries an explicit cast: the parameter's wire type is
        // fixed by the Rust value (i64 → int8), and without the cast Postgres
        // would infer the column's narrower type (int4) and reject the bind.
        // Cast each cursor bind back to its key column's type: a text-decoded
        // cursor (uuid/timestamp/numeric key) would otherwise bind as `text` and
        // `col > $1::text` has no operator (42883). Int/real values pin their own
        // wire type and ignore the column type.
        let key_types = key.column_types();
        let (where_clause, order_by) =
            crate::seek_clauses(key, bound_len, descending, false, pg_quote, |i| {
                format!("${}{}", i + 1, pg_cast(&bound.unwrap()[i], key_types[i]))
            });
        let sql = format!(
            "SELECT * FROM ({base}) AS _red {where_clause}ORDER BY {order_by} LIMIT {limit}"
        );
        let boxed = pg_params(bound)?;
        self.with_fetch_conn(abort, |client| async move {
            let (stmt, columns) = prepare_columns(&client, &sql).await?;
            let params: Vec<&(dyn ToSql + Sync)> = boxed
                .iter()
                .map(|b| -> &(dyn ToSql + Sync) { b.as_ref() })
                .collect();
            let rows = client.query(&stmt, &params).await.map_err(map_pg_err)?;
            let cap = CellCap::display(crate::key_positions(key, &columns));
            Ok(ResultPage {
                rows: rows.iter().map(|r| pg_row(r, cap)).collect(),
                columns,
            })
        })
        .await
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
        let key_types = key.column_types();
        let (where_clause, order_by) =
            crate::seek_clauses(key, bound_len, false, true, pg_quote, |i| {
                format!("${}{}", i + 1, pg_cast(&from.unwrap()[i], key_types[i]))
            });
        let sql = format!(
            "SELECT * FROM ({base}) AS _red {where_clause}\
             ORDER BY {order_by} LIMIT {limit} OFFSET {skip}"
        );
        let boxed = pg_params(from)?;
        self.with_fetch_conn(abort, |client| async move {
            let (stmt, columns) = prepare_columns(&client, &sql).await?;
            let params: Vec<&(dyn ToSql + Sync)> = boxed
                .iter()
                .map(|b| -> &(dyn ToSql + Sync) { b.as_ref() })
                .collect();
            let rows = client.query(&stmt, &params).await.map_err(map_pg_err)?;
            let cap = CellCap::display(crate::key_positions(key, &columns));
            Ok(ResultPage {
                rows: rows.iter().map(|r| pg_row(r, cap)).collect(),
                columns,
            })
        })
        .await
    }

    async fn key_bounds(
        &self,
        sql: &str,
        key: &KeySpec,
        abort: &AbortSignal,
    ) -> Result<Option<(i64, i64)>> {
        let col = pg_quote(&key.column);
        let sql = format!(
            "SELECT min({col}), max({col}) FROM ({}) AS _red",
            strip_trailing(sql)
        );
        self.with_fetch_conn(abort, |client| async move {
            let rows = client.query(&sql, &[]).await.map_err(map_pg_err)?;
            Ok(rows.first().map(|r| pg_row(r, None)).and_then(|cells| {
                match (cells.first(), cells.get(1)) {
                    (Some(Value::Integer(min)), Some(Value::Integer(max))) => Some((*min, *max)),
                    _ => None,
                }
            }))
        })
        .await
    }

    async fn execute(&self, sql: &str) -> Result<u64> {
        // Run the write on a borrowed pool connection, never the shared `client`
        // that backs the live cursor: a `BEGIN`/`COMMIT` pipelined onto the cursor's
        // connection can entangle an in-flight stream ("another command is already in
        // progress"). The pool connection carries the same read-only posture, so a
        // write on a read-only session is still rejected at the engine.
        let client = self.acquire().await?;
        let result = async {
            client.batch_execute("BEGIN").await.map_err(driver_err)?;
            match client.execute(sql, &[]).await {
                Ok(affected) => {
                    client.batch_execute("COMMIT").await.map_err(driver_err)?;
                    Ok(affected)
                }
                Err(e) => {
                    crate::warn_rollback(client.batch_execute("ROLLBACK").await, "execute");
                    Err(map_pg_err(e))
                }
            }
        }
        .await;
        self.release(client);
        result
    }

    async fn execute_batch(&self, statements: &[String]) -> Result<Vec<u64>> {
        if statements.is_empty() {
            return Ok(Vec::new());
        }
        let client = self.acquire().await?;
        let result = async {
            client.batch_execute("BEGIN").await.map_err(driver_err)?;
            let mut affected = Vec::with_capacity(statements.len());
            for sql in statements {
                match client.execute(sql.as_str(), &[]).await {
                    Ok(n) => affected.push(n),
                    Err(e) => {
                        crate::warn_rollback(
                            client.batch_execute("ROLLBACK").await,
                            "execute_batch",
                        );
                        return Err(map_pg_err(e));
                    }
                }
            }
            client.batch_execute("COMMIT").await.map_err(driver_err)?;
            Ok(affected)
        }
        .await;
        self.release(client);
        result
    }

    async fn apply_edits(&self, ops: &[EditOp]) -> Result<u64> {
        if ops.is_empty() {
            return Ok(0);
        }
        // Borrow a pool connection so the batch's transaction never shares the
        // cursor's connection; see `execute`.
        let client = self.acquire().await?;
        let result = async {
            client.batch_execute("BEGIN").await.map_err(driver_err)?;
            let mut total = 0u64;
            for op in ops {
                // Typed placeholders (`$n::int8`, …) like the seek path: the value's
                // wire type is fixed by the Rust value, the cast keeps Postgres from
                // re-inferring.
                let (sql, params) = crate::edit_sql(op, pg_quote, |i, cv| {
                    format!("${}{}", i + 1, pg_cast(&cv.value, cv.decl_type.as_deref()))
                });
                let owned: Vec<Value> = params.iter().map(|v| (*v).clone()).collect();
                let boxed = pg_params(Some(&owned))?;
                let refs: Vec<&(dyn ToSql + Sync)> = boxed
                    .iter()
                    .map(|b| -> &(dyn ToSql + Sync) { b.as_ref() })
                    .collect();
                match client.execute(&sql, &refs).await {
                    Ok(affected) => {
                        if affected != 1 {
                            crate::warn_rollback(
                                client.batch_execute("ROLLBACK").await,
                                "apply_edits",
                            );
                            return Err(crate::edit_count_err(op, affected));
                        }
                        total += affected;
                    }
                    Err(e) => {
                        crate::warn_rollback(client.batch_execute("ROLLBACK").await, "apply_edits");
                        return Err(map_pg_err(e));
                    }
                }
            }
            client.batch_execute("COMMIT").await.map_err(driver_err)?;
            Ok(total)
        }
        .await;
        self.release(client);
        result
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
        // Borrow a pool connection so the transaction never shares the cursor's
        // connection; see `execute`/`apply_edits`.
        let client = self.acquire().await?;
        let result = async {
            client.batch_execute("BEGIN").await.map_err(driver_err)?;
            let max = crate::insert_chunk_rows(columns.len(), PG_PARAM_CAP);
            let mut total = 0u64;
            for chunk in rows.chunks(max) {
                // Typed placeholders (`$n::int8`, `$n::text::"uuid"`, …) like the
                // edit path, so Postgres can't re-infer the parameter type.
                let (sql, params) =
                    crate::insert_sql(table, columns, chunk, pg_quote, |i, v, dt| {
                        format!("${}{}", i + 1, pg_cast(v, dt))
                    });
                let owned: Vec<Value> = params.iter().map(|v| (*v).clone()).collect();
                let boxed = pg_params(Some(&owned))?;
                let refs: Vec<&(dyn ToSql + Sync)> = boxed
                    .iter()
                    .map(|b| -> &(dyn ToSql + Sync) { b.as_ref() })
                    .collect();
                match client.execute(&sql, &refs).await {
                    Ok(affected) => total += affected,
                    Err(e) => {
                        crate::warn_rollback(client.batch_execute("ROLLBACK").await, "insert_rows");
                        return Err(map_pg_err(e));
                    }
                }
            }
            client.batch_execute("COMMIT").await.map_err(driver_err)?;
            Ok(total)
        }
        .await;
        self.release(client);
        result
    }

    async fn clear_table(&self, table: &TableRef) -> Result<u64> {
        let qualify = match &table.schema {
            Some(s) if !s.is_empty() => format!("{}.{}", pg_quote(s), pg_quote(&table.name)),
            _ => pg_quote(&table.name),
        };
        let client = self.acquire().await?;
        let result = async {
            client.batch_execute("BEGIN").await.map_err(driver_err)?;
            match client.execute(&format!("DELETE FROM {qualify}"), &[]).await {
                Ok(affected) => {
                    client.batch_execute("COMMIT").await.map_err(driver_err)?;
                    Ok(affected)
                }
                Err(e) => {
                    crate::warn_rollback(client.batch_execute("ROLLBACK").await, "clear_table");
                    Err(map_pg_err(e))
                }
            }
        }
        .await;
        self.release(client);
        result
    }

    async fn create_table(&self, table: &TableRef, columns: &[ColumnMeta]) -> Result<u64> {
        let sql = crate::create_table_sql(table, columns, DbKind::Postgres, pg_quote);
        self.execute(&sql).await
    }

    fn quote_table(&self, table: &TableRef) -> String {
        crate::qualify_table(table, pg_quote)
    }

    fn quote_ident(&self, ident: &str) -> String {
        pg_quote(ident)
    }

    async fn create_index(
        &self,
        table: &TableRef,
        name: &str,
        unique: bool,
        columns: &[String],
    ) -> Result<u64> {
        let sql = crate::create_index_sql(table, name, unique, columns, DbKind::Postgres, pg_quote);
        self.execute(&sql).await
    }

    async fn add_foreign_key(
        &self,
        child: &TableRef,
        columns: &[String],
        parent: &TableRef,
        ref_columns: &[String],
    ) -> Result<u64> {
        let sql = crate::add_fk_sql(child, columns, parent, ref_columns, pg_quote);
        self.execute(&sql).await
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<QueryPlan> {
        // Default `FORMAT TEXT`: the most stable parse target, and avoids the
        // JSON dependency. Plain `EXPLAIN` never executes the statement;
        // `EXPLAIN ANALYZE` does (the caller gates it to read queries, and a
        // read-only connection rejects an underlying write at the engine anyway).
        let verb = if analyze {
            "EXPLAIN ANALYZE "
        } else {
            "EXPLAIN "
        };
        let sql = format!("{verb}{}", strip_trailing(sql));
        let rows = self.client.query(&sql, &[]).await.map_err(map_pg_err)?;
        let text = rows
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(crate::plan::from_text_tree(&text, analyze))
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
        let (stmt, columns) = prepare_columns(&self.client, &sql).await?;
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();

        let stream = self
            .client
            .query_raw(&stmt, no_params())
            .await
            .map_err(driver_err)?;
        futures_util::pin_mut!(stream);

        let file = File::create(path).map_err(driver_err)?;
        let out = BufWriter::new(file);
        let table = crate::format::sql_table_name(path);
        let mut writer = ExportWriter::begin(out, format, names, table).map_err(driver_err)?;
        let mut throttle = ProgressThrottle::new(progress);

        // Bail on cancel: drop the writer, remove the partial file, and report
        // interruption; never leave a truncated CSV/JSON behind.
        macro_rules! bail_if_cancelled {
            () => {
                if cancel.load(Ordering::Relaxed) {
                    drop(writer);
                    let _ = std::fs::remove_file(path);
                    return Err(RedError::Interrupted);
                }
            };
        }

        while let Some(row) = stream.next().await {
            bail_if_cancelled!();
            let row = row.map_err(map_pg_err)?;
            let cells = pg_row(&row, None);
            writer.write_row(&cells).map_err(driver_err)?;
            throttle.tick(writer.written());
        }
        writer.finish().map_err(driver_err)
    }
}

/// The async-side cursor: column metadata + the live row stream behind a `Mutex`
/// (so `next_window(&self)` can pull) + the out-of-band cancel token.
struct PgCursor {
    columns: Vec<Column>,
    stream: Mutex<Pin<Box<RowStream>>>,
    cancel: CancelToken,
    /// Read cells at full fidelity (the table-copy read) rather than the display
    /// fat-cell cap; see [`QueryOptions::full_fidelity`](red_core::QueryOptions).
    full: bool,
}

#[async_trait]
impl QueryCursor for PgCursor {
    fn columns(&self) -> &[Column] {
        &self.columns
    }

    async fn next_window(&self, max: usize) -> Result<RowWindow> {
        // Offset-mode display stream (editor run): cap every cell, no key exempt.
        // A full-fidelity reader (the table copy) reads byte-exact instead.
        let cap = if self.full {
            None
        } else {
            CellCap::display([None, None])
        };
        let mut stream = self.stream.lock().await;
        let mut rows = Vec::with_capacity(crate::window_prealloc(max));
        for _ in 0..max {
            match stream.next().await {
                Some(Ok(row)) => rows.push(pg_row(&row, cap)),
                Some(Err(e)) => return Err(map_pg_err(e)),
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

/// Double-quote an identifier for interpolation (doubling embedded quotes).
fn pg_quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Postgres' wire-protocol cap on bound parameters per statement (`u16`, 65535),
/// with margin; a multi-row insert sub-chunks below it.
const PG_PARAM_CAP: usize = 60_000;

/// The explicit cast for a bound placeholder, pinning the inferred parameter type
/// to the wire form the value's Rust type encodes (so Postgres can't re-infer it):
/// `i64`→`int8`, `f64`→`float8`, `String`→`text`, `Vec<u8>`→`bytea`.
///
/// A [`Value::Text`] is special on **write**: it binds as `text` (the only form
/// `String` encodes), but the target column may be jsonb/json/timestamp/uuid/numeric
/// /an enum: types with no implicit (assignment) cast *from* text (the post-8.3
/// rule), so `SET jsonb_col = $1::text` is rejected with "column is of type jsonb
/// but expression is of type text". When `decl_type` names such a column we add a
/// second, *explicit* cast, `$1::text::"jsonb"`, which type-checks. Plain
/// text-family columns (and an unknown / absent type, e.g. a key bind) keep `::text`.
fn pg_cast(value: &Value, decl_type: Option<&str>) -> String {
    match value {
        Value::Integer(_) => "::int8".to_string(),
        Value::Real(_) => "::float8".to_string(),
        Value::Blob(_) => "::bytea".to_string(),
        Value::Null | Value::Capped(_) => String::new(),
        Value::Text(_) => match decl_type {
            Some(t) if !is_pg_text_type(t) => format!("::text::{}", pg_quote(t)),
            _ => "::text".to_string(),
        },
    }
}

/// Whether a Postgres column type (the `typname` we store as `decl_type`) is a
/// text-family type a `text` bind assigns to directly, so it needs no second cast
/// on write. Everything else (jsonb, timestamp, uuid, numeric, an enum, …) does.
fn is_pg_text_type(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "text" | "varchar" | "bpchar" | "char" | "name" | "citext" | "unknown"
    )
}

/// Box each seek-bound value as a typed `ToSql` parameter (one per leading key
/// column), for positional binding into the row-value comparison. Key columns are
/// never null/capped, so those variants are a query error rather than a `NULL`.
fn pg_params(bound: Option<&[Value]>) -> Result<Vec<Box<dyn ToSql + Sync + Send>>> {
    bound
        .unwrap_or(&[])
        .iter()
        .map(|v| -> Result<Box<dyn ToSql + Sync + Send>> {
            Ok(match v {
                Value::Integer(n) => Box::new(*n),
                Value::Real(x) => Box::new(*x),
                Value::Text(s) => Box::new(s.to_string()),
                Value::Blob(b) => Box::new(b.clone()),
                Value::Null | Value::Capped(_) => {
                    return Err(RedError::Query("null seek bound".into()))
                }
            })
        })
        .collect()
}

/// Map one row's cells to [`Value`]s by column type (text fallback for the rest).
/// With a display `cap`, over-cap non-key text/blob cells come back [`Value::Capped`]
/// (blob bytes are read as a borrowed slice for their length only, never owned).
fn pg_row(row: &Row, cap: Option<CellCap>) -> Vec<Value> {
    (0..row.len())
        .map(|i| pg_value(row, i, CellCap::caps(cap, i)))
        .collect()
}

fn pg_value(row: &Row, i: usize, max: Option<usize>) -> Value {
    match *row.columns()[i].type_() {
        Type::BOOL => scalar(row, i, max, |b: bool| Value::Integer(b as i64)),
        Type::INT2 => scalar(row, i, max, |n: i16| Value::Integer(i64::from(n))),
        Type::INT4 => scalar(row, i, max, |n: i32| Value::Integer(i64::from(n))),
        Type::INT8 => scalar(row, i, max, |n: i64| Value::Integer(n)),
        Type::FLOAT4 => scalar(row, i, max, |x: f32| Value::Real(x as f64)),
        Type::FLOAT8 => scalar(row, i, max, Value::Real),
        // Types `tokio-postgres` won't decode without an optional crate: render the
        // raw wire bytes to text ourselves (see `pg_text`) so they don't decode-fail
        // into a silent NULL. Each result is short except JSON, which honours `max`.
        Type::NUMERIC => decode_raw(row, i, max, pg_text::numeric_to_string),
        Type::TIMESTAMP => decode_raw(row, i, max, |b| be_i64(b).map(pg_text::timestamp_to_string)),
        Type::TIMESTAMPTZ => decode_raw(row, i, max, |b| {
            be_i64(b).map(pg_text::timestamptz_to_string)
        }),
        Type::DATE => decode_raw(row, i, max, |b| be_i32(b).map(pg_text::date_to_string)),
        Type::TIME => decode_raw(row, i, max, |b| be_i64(b).map(pg_text::time_to_string)),
        Type::TIMETZ => decode_raw(row, i, max, pg_text::timetz_to_string),
        Type::UUID => decode_raw(row, i, max, pg_text::uuid_to_string),
        // JSON is UTF-8 text on the wire; JSONB prefixes a 1-byte version header.
        Type::JSON => decode_raw(row, i, max, |b| {
            Some(String::from_utf8_lossy(b).into_owned())
        }),
        Type::JSONB => decode_raw(row, i, max, |b| {
            let text = b.split_first().map(|(_, rest)| rest).unwrap_or(b);
            Some(String::from_utf8_lossy(text).into_owned())
        }),
        // Capped: read the bytes as a borrowed slice for their length, never owning
        // them. Full fidelity: own the bytes (export / clipboard / key column).
        Type::BYTEA => match max {
            Some(_) => row
                .try_get::<_, Option<&[u8]>>(i)
                .ok()
                .flatten()
                .map(|b| Value::capped_blob(b.len()))
                .unwrap_or(Value::Null),
            None => row
                .try_get::<_, Option<Vec<u8>>>(i)
                .ok()
                .flatten()
                .map(Value::Blob)
                .unwrap_or(Value::Null),
        },
        // text / varchar / name / bpchar / unknown, and a best-effort for the rest.
        // `&str` and `String` accept the same types, so capping doesn't change which
        // columns decode (only how much of an over-cap one is kept).
        //
        // `try_get` returns `Ok(None)` for a SQL NULL and `Err` when the target type
        // *rejects* the column type (its `accepts` said no). The former is a genuine
        // `Null`; the latter is an unmapped type the string decode declined (enum,
        // inet, interval, array, …), and rather than collapse it to a silent NULL we
        // fall back to its raw wire bytes as lossy UTF-8: correct for the text-shaped
        // wire forms (enum labels, citext-likes) and a visible cell for the rest.
        _ => match row.try_get::<_, Option<&str>>(i) {
            Ok(None) => Value::Null,
            Ok(Some(s)) => match max {
                Some(max) => Value::capped_text(s, max),
                None => Value::Text(s.into()),
            },
            Err(_) => raw_text_fallback(row, i, max),
        },
    }
}

/// Decode a scalar cell of type `T` and map it with `f`. A SQL NULL is
/// [`Value::Null`]; a decode *error* (the column isn't the `T` we expected) falls
/// back to the raw wire bytes as text via [`raw_text_fallback`] rather than
/// collapsing to a silent NULL, the same safety the text `_` arm relies on.
fn scalar<'a, T>(row: &'a Row, i: usize, max: Option<usize>, f: impl FnOnce(T) -> Value) -> Value
where
    T: tokio_postgres::types::FromSql<'a>,
{
    match row.try_get::<_, Option<T>>(i) {
        Ok(Some(v)) => f(v),
        Ok(None) => Value::Null,
        Err(_) => raw_text_fallback(row, i, max),
    }
}

/// Captures a column's raw binary wire bytes verbatim, so the driver can render the
/// types `tokio-postgres` declines to decode itself (see [`crate::pg_text`]).
/// `accepts` is unconditional: it's only ever asked for via the explicit type
/// arms in [`pg_value`].
struct RawBytes(Vec<u8>);

impl<'a> tokio_postgres::types::FromSql<'a> for RawBytes {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(RawBytes(raw.to_vec()))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

/// Decode cell `i`'s raw wire bytes with `f`, wrapping the result as a display
/// [`Value`] (honouring `max`). A SQL NULL, a fetch error, or an `f` that can't
/// parse the buffer all collapse to [`Value::Null`].
fn decode_raw(
    row: &Row,
    i: usize,
    max: Option<usize>,
    f: impl FnOnce(&[u8]) -> Option<String>,
) -> Value {
    row.try_get::<_, Option<RawBytes>>(i)
        .ok()
        .flatten()
        .and_then(|b| f(&b.0))
        .map(|s| match max {
            Some(m) => Value::capped_text(&s, m),
            None => Value::Text(s.into()),
        })
        .unwrap_or(Value::Null)
}

/// Last-resort render for a column type the scalar/`pg_text` arms don't name and
/// that the string decode rejected (its `accepts` said no): take the raw wire bytes
/// and render them as lossy UTF-8. Correct for the text-shaped binary forms (enum
/// labels, `citext`-likes, domains over text) and at worst a visible cell for the
/// others: anything but the silent NULL the bare string decode would have produced.
/// A fetch error or genuine SQL NULL still collapses to [`Value::Null`].
fn raw_text_fallback(row: &Row, i: usize, max: Option<usize>) -> Value {
    decode_raw(row, i, max, |b| {
        Some(String::from_utf8_lossy(b).into_owned())
    })
}

fn be_i64(b: &[u8]) -> Option<i64> {
    b.try_into().ok().map(i64::from_be_bytes)
}

fn be_i32(b: &[u8]) -> Option<i32> {
    b.try_into().ok().map(i32::from_be_bytes)
}

/// Map a failed dial to a *fatal* [`RedError::Auth`] (the user must fix the
/// connection before a retry helps) or a transient [`RedError::Connect`]. Bad
/// credentials (28xxx) and a missing database (3D000) are user-correctable; a
/// refused/unreachable host has no server `DbError` and stays a retryable
/// `Connect`. The server's own message is surfaced (its `Display` is a bare
/// `"db error"`; the text lives only in the attached `DbError`).
fn map_connect_err(e: tokio_postgres::Error) -> RedError {
    if let Some(db) = e.as_db_error() {
        let class = &db.code().code()[..2];
        // SQLSTATE class 28 = invalid authorization; 3D000 = invalid catalog
        // (database does not exist). Both need a credential/target fix, not a wait.
        if class == "28" || db.code() == &tokio_postgres::error::SqlState::INVALID_CATALOG_NAME {
            return RedError::Auth(db.message().to_string());
        }
        return RedError::Connect(format!("{}: {}", db.code().code(), db.message()));
    }
    RedError::Connect(e.to_string())
}

/// Map a cancel (SQLSTATE 57014) to the distinct `Interrupted`, else a driver
/// error. A database-side failure (bad SQL, missing relation, type mismatch) is
/// the common case, and `tokio_postgres::Error`'s own `Display` renders it as a
/// bare `"db error"`; the useful text lives only in the attached `DbError`. So
/// surface the server's message (with SQLSTATE and any hint) rather than letting
/// the round-trip bounce back as the cryptic `"db error"`.
fn map_pg_err(e: tokio_postgres::Error) -> RedError {
    if let Some(db) = e.as_db_error() {
        if db.code() == &tokio_postgres::error::SqlState::QUERY_CANCELED {
            return RedError::Interrupted;
        }
        let mut msg = format!("{}: {}", db.code().code(), db.message());
        if let Some(hint) = db.hint() {
            msg.push_str(&format!(" (hint: {hint})"));
        }
        return RedError::Driver(msg);
    }
    driver_err(e)
}

/// The column list inside an index definition's parentheses.
fn parse_index_columns(def: &str) -> Vec<String> {
    let Some(open) = def.find('(') else {
        return Vec::new();
    };
    let Some(close) = def.rfind(')') else {
        return Vec::new();
    };
    if close <= open {
        return Vec::new();
    }
    def[open + 1..close]
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// Tests run against a live PostgreSQL provided via `RED_TEST_POSTGRES_URL`, so CI
// without a server skips cleanly. Spin one up with:
//
//   docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=red \
//     -e POSTGRES_DB=red_test --name red-pg postgres:16
//   export RED_TEST_POSTGRES_URL='host=127.0.0.1 user=postgres password=red dbname=red_test'
#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance as battery;
    use red_core::KeyKind;

    fn test_url() -> Option<String> {
        std::env::var("RED_TEST_POSTGRES_URL").ok()
    }

    /// A unique fixture-name suffix so concurrent tests don't collide on a shared
    /// server. Postgres lowercases unquoted identifiers, so keep it lowercase.
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
                    eprintln!("SKIP {}: RED_TEST_POSTGRES_URL not set", module_path!());
                    return;
                }
            }
        };
    }

    /// The write-side cast `pg_cast` emits per `(value, column type)`. A scalar pins
    /// the wire type from its Rust value; a text value bound into a non-text column
    /// (jsonb, timestamp, an enum) gets the second explicit cast that lets the
    /// assignment type-check, while a plain text column (or an unknown / key bind)
    /// stays a bare `::text`. No DB needed: pure string rendering.
    #[test]
    fn pg_cast_casts_text_into_non_text_columns() {
        // Scalars: cast follows the Rust value, column type is irrelevant.
        assert_eq!(pg_cast(&Value::Integer(1), Some("int4")), "::int8");
        assert_eq!(pg_cast(&Value::Real(1.0), Some("numeric")), "::float8");
        assert_eq!(pg_cast(&Value::Blob(vec![1]), None), "::bytea");
        // NULL / capped never bind, so they emit no cast.
        assert_eq!(pg_cast(&Value::Null, Some("jsonb")), "");

        let text = Value::Text("{\"a\":1}".into());
        // A jsonb / json / timestamp / uuid / enum column needs the explicit cast,
        // because Postgres won't assignment-cast text into them.
        assert_eq!(pg_cast(&text, Some("jsonb")), "::text::\"jsonb\"");
        assert_eq!(pg_cast(&text, Some("json")), "::text::\"json\"");
        assert_eq!(
            pg_cast(&text, Some("timestamptz")),
            "::text::\"timestamptz\""
        );
        assert_eq!(pg_cast(&text, Some("uuid")), "::text::\"uuid\"");
        assert_eq!(pg_cast(&text, Some("mood")), "::text::\"mood\"");
        // Plain text-family columns assign directly; no second cast.
        assert_eq!(pg_cast(&text, Some("text")), "::text");
        assert_eq!(pg_cast(&text, Some("VARCHAR")), "::text");
        assert_eq!(pg_cast(&text, Some("bpchar")), "::text");
        // Unknown / absent type (e.g. a key bind) is best-effort `::text`.
        assert_eq!(pg_cast(&text, None), "::text");
    }

    /// The connection's current schema: unqualified fixtures land here, so
    /// introspection filters to it. Read through the public API rather than the
    /// private client.
    async fn current_schema(driver: &PostgresDriver) -> String {
        let page = driver
            .fetch_page(
                "SELECT current_schema()",
                0,
                1,
                PageCap::Full,
                &AbortSignal::new(),
            )
            .await
            .unwrap();
        match &page.rows[0][0] {
            Value::Text(s) => s.to_string(),
            other => panic!("current_schema() returned {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_reports_version() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        assert!(!driver.server_version().is_empty());
        driver.ping().await.unwrap();
    }

    #[tokio::test]
    async fn tls_dsn_is_refused_not_silently_downgraded() {
        // Postgres TLS isn't wired yet; a `sslmode=require` DSN must error rather
        // than connect in cleartext. No server needed — it fails before dialing.
        match PostgresDriver::connect("postgres://h:5432/db?sslmode=require", true).await {
            Ok(_) => panic!("a TLS Postgres DSN should be refused, not connected"),
            Err(e) => assert!(
                e.to_string().to_lowercase().contains("tls"),
                "expected a TLS-not-supported error, got {e}"
            ),
        }
    }

    /// The non-scalar types `pg_value` renders from their binary wire form must
    /// come back as their text, never as a silent NULL (the regression `pg_text`
    /// fixes). Complements the wire-format unit tests in [`crate::pg_text`] with a
    /// live round-trip through the real `tokio-postgres` decode path.
    #[tokio::test]
    async fn rich_types_render_as_text_not_null() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        let sql = "SELECT \
            1234.567::numeric, \
            '2021-03-15 12:30:45'::timestamp, \
            '2021-03-15 12:30:45+00'::timestamptz, \
            '2021-03-15'::date, \
            '12:30:45'::time, \
            '12345678-1234-5678-1234-567812345678'::uuid, \
            '{\"a\":1}'::json, \
            '{\"b\": 2}'::jsonb";
        let page = driver
            .fetch_page(sql, 0, 1, PageCap::Full, &AbortSignal::new())
            .await
            .unwrap();
        let row = &page.rows[0];
        let text = |v: &Value| match v {
            Value::Text(s) => s.to_string(),
            other => panic!("expected text, got {other:?}"),
        };
        assert_eq!(text(&row[0]), "1234.567");
        assert_eq!(text(&row[1]), "2021-03-15 12:30:45");
        // timestamptz is UTC on the wire regardless of session zone.
        assert_eq!(text(&row[2]), "2021-03-15 12:30:45+00");
        assert_eq!(text(&row[3]), "2021-03-15");
        assert_eq!(text(&row[4]), "12:30:45");
        assert_eq!(text(&row[5]), "12345678-1234-5678-1234-567812345678");
        assert_eq!(text(&row[6]), "{\"a\":1}");
        // jsonb normalizes spacing/key order on the server.
        assert_eq!(text(&row[7]), "{\"b\": 2}");
    }

    /// Types neither the scalar arms nor `pg_text` name, and that the string decode
    /// *rejects* (its `accepts` says no): inet, interval, and an array all flow
    /// through the raw-bytes fallback. The contract under test is "visible text, not
    /// a silent NULL"; the exact bytes are server-version dependent, so assert only
    /// that each cell is non-empty text.
    #[tokio::test]
    async fn unmapped_types_fall_back_to_text_not_null() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        let sql = "SELECT \
            '192.168.0.1'::inet, \
            '1 day 02:03:04'::interval, \
            ARRAY[1, 2, 3]";
        let page = driver
            .fetch_page(sql, 0, 1, PageCap::Full, &AbortSignal::new())
            .await
            .unwrap();
        let row = &page.rows[0];
        for (i, cell) in row.iter().enumerate() {
            match cell {
                Value::Text(s) if !s.is_empty() => {}
                other => panic!("col {i} fell back to {other:?}, expected non-empty text"),
            }
        }
    }

    #[tokio::test]
    async fn streams_in_bounded_windows() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        // `generate_series` is a server-side streaming row source: no fixture, and
        // it never materializes server-side, mirroring the windowed read.
        battery::streams_in_bounded_windows(&driver, "SELECT generate_series(1, 100000)", 100_000)
            .await;
    }

    #[tokio::test]
    async fn cancel_aborts_in_flight_fetch() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        // A large cross join keeps the server streaming long enough to cancel
        // out-of-band; Postgres maps the cancel to `QUERY_CANCELED` → Interrupted.
        let sql = "SELECT a FROM generate_series(1, 100000) a \
                   CROSS JOIN generate_series(1, 100000) b";
        battery::cancel_aborts_in_flight_fetch(&driver, sql, std::time::Duration::from_millis(200))
            .await;
    }

    #[tokio::test]
    async fn superseded_one_shot_fetch_is_cancelled() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        // count(*) over a 10^10-row cross join keeps the backend busy to interrupt.
        let heavy = "SELECT a FROM generate_series(1, 100000) a \
                     CROSS JOIN generate_series(1, 100000) b";
        battery::superseded_fetch_is_cancelled(
            &driver,
            heavy,
            std::time::Duration::from_millis(200),
        )
        .await;
        battery::pre_aborted_fetch_returns_immediately(&driver, heavy).await;
        battery::abort_after_completion_is_noop(&driver, "SELECT 1").await;
    }

    /// The reason Postgres fetches use a pool: cancelling one fetch's abort signal
    /// must not disturb another fetch in flight on a *different* pooled connection.
    #[tokio::test]
    async fn superseding_one_fetch_spares_a_concurrent_one() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        let heavy = "SELECT a FROM generate_series(1, 100000) a \
                     CROSS JOIN generate_series(1, 100000) b";

        let doomed = AbortSignal::new();
        let kept = AbortSignal::new();
        // Abort only `doomed` once both are in flight on their own pooled conns.
        let trigger = doomed.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            trigger.abort();
        });

        let (a, b) = tokio::join!(
            driver.count(heavy, &doomed),
            // A finite-but-still-running count that must complete untouched.
            driver.count("SELECT generate_series(1, 5000000)", &kept),
        );
        assert!(
            matches!(a, Err(RedError::Interrupted)),
            "doomed fetch cancelled: {a:?}"
        );
        assert_eq!(
            b.unwrap(),
            5_000_000,
            "the concurrent fetch finished unharmed"
        );
    }

    #[tokio::test]
    async fn introspects_tables_columns_fks_and_indexes() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let authors = tag("authors");
        let books = tag("books");
        let recent = tag("recent");
        let idx = tag("idx");
        let schema = current_schema(&driver).await;

        // Postgres `execute` runs a single statement, so issue the DDL one at a time.
        driver
            .execute(&format!(
                "CREATE TABLE {authors} (id INT PRIMARY KEY, name TEXT NOT NULL)"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "CREATE TABLE {books} (\
                   id INT PRIMARY KEY, \
                   title TEXT NOT NULL DEFAULT 'untitled', \
                   author_id INT REFERENCES {authors}(id))"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!("CREATE INDEX {idx} ON {books}(author_id)"))
            .await
            .unwrap();
        driver
            .execute(&format!("CREATE VIEW {recent} AS SELECT * FROM {books}"))
            .await
            .unwrap();

        battery::introspects_tables_columns_fks_and_indexes(
            &driver, &schema, &authors, &books, &recent,
        )
        .await;
        // Track B7: the connection-wide FK graph reports the same edge.
        battery::lists_foreign_key_graph(&driver, &schema, &authors, &books).await;

        // Seed a few rows so the column-stats summary has data: author_id is
        // 1,1,2,NULL (NULLs + duplicates), narrowable by `author_id = 1`.
        driver
            .execute(&format!(
                "INSERT INTO {authors}(id, name) VALUES (1, 'Ada'), (2, 'Grace')"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {books}(id, title, author_id) \
                 VALUES (1, 'a', 1), (2, 'b', 1), (3, 'c', 2), (4, 'd', NULL)"
            ))
            .await
            .unwrap();
        battery::column_stats_summary(
            &driver,
            &format!("SELECT * FROM {books}"),
            "author_id",
            "title",
            "author_id = 1",
        )
        .await;

        for obj in [
            format!("VIEW {recent}"),
            format!("TABLE {books}"),
            format!("TABLE {authors}"),
        ] {
            driver.execute(&format!("DROP {obj}")).await.unwrap();
        }
    }

    #[tokio::test]
    async fn filters_contains() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let f = tag("f");
        let schema = current_schema(&driver).await;
        driver
            .execute(&format!(
                "CREATE TABLE {f} (id INT PRIMARY KEY, name TEXT, note TEXT, data BYTEA)"
            ))
            .await
            .unwrap();
        // Rows 1–2 carry a blob whose bytes spell "apple"; on Postgres `bytea::text`
        // is a hex string anyway, but the predicate must still skip the column.
        driver
            .execute(&format!(
                "INSERT INTO {f} VALUES \
                 (1,'apple','red fruit','\\x6170706c65'::bytea), \
                 (2,'banana','yellow','\\x6170706c65'::bytea), \
                 (3,'apple pie','dessert','\\x00'::bytea), \
                 (4,'100% juice','on sale','\\x00'::bytea), \
                 (5,'O''Brien','name','\\x00'::bytea)"
            ))
            .await
            .unwrap();
        battery::filters_contains(&driver, &schema, &f, &format!("SELECT * FROM {f}")).await;
        driver.execute(&format!("DROP TABLE {f}")).await.unwrap();
    }

    #[tokio::test]
    async fn executes_in_transaction_and_exports() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let t = tag("t");
        driver
            .execute(&format!("CREATE TABLE {t} (id INT, name TEXT)"))
            .await
            .unwrap();

        let affected = driver
            .execute(&format!("INSERT INTO {t} VALUES (1, 'a,b'), (2, NULL)"))
            .await
            .unwrap();
        assert_eq!(affected, 2, "execute reports rows affected");

        battery::exports_csv_and_json(&driver, &format!("SELECT * FROM {t} ORDER BY id"), &t).await;

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn seeks_forward_backward_and_reads_bounds() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let t = tag("seek");
        driver
            .execute(&format!("CREATE TABLE {t} (id INT PRIMARY KEY, name TEXT)"))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {t} SELECT g, 'row ' || g FROM generate_series(1, 1000) g"
            ))
            .await
            .unwrap();

        let key = KeySpec::single("id", KeyKind::Int);
        battery::seeks_forward_backward_and_reads_bounds(
            &driver,
            &format!("SELECT * FROM {t}"),
            &key,
        )
        .await;

        // Composite `(grp, id)` seek over a non-unique sort column.
        let g = tag("seekcomposite");
        driver
            .execute(&format!(
                "CREATE TABLE {g} (id INT PRIMARY KEY, grp INT NOT NULL)"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {g} SELECT s, s % 3 FROM generate_series(1, 30) s"
            ))
            .await
            .unwrap();
        let key_asc = KeySpec {
            column: "grp".into(),
            kind: KeyKind::Int,
            column_type: None,
            tiebreak: Some("id".into()),
            tiebreak_type: None,
            descending: false,
        };
        let key_desc = KeySpec {
            descending: true,
            ..key_asc.clone()
        };
        battery::seeks_composite_sorted(
            &driver,
            &format!("SELECT * FROM {g}"),
            &key_asc,
            &key_desc,
            30,
        )
        .await;
        driver.execute(&format!("DROP TABLE {g}")).await.unwrap();

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn read_only_rejects_writes() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        battery::read_only_rejects_write(&driver, "CREATE TABLE red_ro_should_fail (x INT)").await;
    }

    #[tokio::test]
    async fn applies_edits_and_read_only_rejects() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let t = tag("edit");
        driver
            .execute(&format!("CREATE TABLE {t} (id INT PRIMARY KEY, name TEXT)"))
            .await
            .unwrap();
        driver
            .execute(&format!("INSERT INTO {t} VALUES (1, 'one')"))
            .await
            .unwrap();
        let schema = current_schema(&driver).await;
        battery::applies_edits(&driver, &schema, &t).await;

        let ro = PostgresDriver::connect(&url, true).await.unwrap();
        battery::read_only_rejects_edit(&ro, &schema, &t).await;

        // Atomic batch editing (B6) on a fresh seed table.
        let tb = tag("batch");
        driver
            .execute(&format!(
                "CREATE TABLE {tb} (id INT PRIMARY KEY, name TEXT)"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!("INSERT INTO {tb} VALUES (1, 'one')"))
            .await
            .unwrap();
        battery::applies_batch_atomic(&driver, &schema, &tb).await;
        battery::read_only_rejects_batch(&ro, &schema, &tb).await;

        // Bulk insert (data import / table copy) on a fresh empty table.
        let ti = tag("insert");
        driver
            .execute(&format!(
                "CREATE TABLE {ti} (id INT PRIMARY KEY, name TEXT)"
            ))
            .await
            .unwrap();
        battery::inserts_rows(&driver, &schema, &ti).await;
        battery::read_only_rejects_insert_rows(&ro, &schema, &ti).await;

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
        driver.execute(&format!("DROP TABLE {tb}")).await.unwrap();
        driver.execute(&format!("DROP TABLE {ti}")).await.unwrap();
    }

    /// Editing a column whose value decodes to [`Value::Text`] but whose real type
    /// has no assignment cast *from* text (jsonb, timestamptz, uuid) must succeed:
    /// the write-side `::text::"type"` cast (driven by `ColumnValue::decl_type`) lets
    /// the bound text type-check into the column. A bare `::text` would be rejected
    /// ("column is of type jsonb but expression is of type text"); this is the
    /// regression test for that. The PK (int) and a plain text column ride along to
    /// show the typed columns don't disturb the ordinary path.
    #[tokio::test]
    async fn edits_jsonb_timestamp_and_uuid_columns() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let t = tag("typededit");
        driver
            .execute(&format!(
                "CREATE TABLE {t} (id INT PRIMARY KEY, doc JSONB, at TIMESTAMPTZ, ref UUID, name TEXT)"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {t} VALUES (1, '{{\"a\":1}}', '2000-01-01 00:00:00+00', \
                 '00000000-0000-0000-0000-000000000000', 'one')"
            ))
            .await
            .unwrap();
        let schema = current_schema(&driver).await;
        let tref = red_core::TableRef {
            schema: Some(schema),
            name: t.clone(),
        };
        // One UPDATE setting every typed column at once; each `set` carries the
        // column's `decl_type`, the key carries none (an int PK binds bare).
        let set = |column: &str, value: &str, decl: &str| red_core::ColumnValue {
            column: column.into(),
            value: Value::Text(value.into()),
            decl_type: Some(decl.into()),
        };
        let affected = driver
            .apply_edit(&EditOp::Update {
                table: tref,
                key: red_core::ColumnValue {
                    column: "id".into(),
                    value: Value::Integer(1),
                    decl_type: None,
                },
                set: vec![
                    set("doc", "{\"b\": [2, 3]}", "jsonb"),
                    set("at", "2021-06-15 12:30:00+00", "timestamptz"),
                    set("ref", "12345678-1234-5678-1234-567812345678", "uuid"),
                    set("name", "two", "text"),
                ],
            })
            .await
            .unwrap();
        assert_eq!(affected, 1, "the typed UPDATE matched exactly its row");

        let page = driver
            .fetch_page(
                &format!("SELECT doc, at, ref, name FROM {t} WHERE id = 1"),
                0,
                1,
                PageCap::Full,
                &AbortSignal::new(),
            )
            .await
            .unwrap();
        let text = |v: &Value| match v {
            Value::Text(s) => s.to_string(),
            other => panic!("expected text, got {other:?}"),
        };
        let row = &page.rows[0];
        // jsonb re-serializes with canonical spacing on the server.
        assert_eq!(text(&row[0]), "{\"b\": [2, 3]}", "jsonb landed");
        assert_eq!(
            text(&row[1]),
            "2021-06-15 12:30:00+00",
            "timestamptz landed"
        );
        assert_eq!(
            text(&row[2]),
            "12345678-1234-5678-1234-567812345678",
            "uuid landed"
        );
        assert_eq!(text(&row[3]), "two", "plain text column unaffected");

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn explains_a_query() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let t = tag("explain");
        driver
            .execute(&format!("CREATE TABLE {t} (id INT PRIMARY KEY, name TEXT)"))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {t} SELECT g, 'row ' || g FROM generate_series(1, 100) g"
            ))
            .await
            .unwrap();

        battery::explains_query(&driver, &format!("SELECT * FROM {t}"), &t).await;

        // EXPLAIN ANALYZE carries actual-time metrics and is flagged analyzed.
        let plan = driver
            .explain(&format!("SELECT count(*) FROM {t}"), true)
            .await
            .unwrap();
        assert!(plan.analyzed, "analyze flag set");
        let has_actual =
            |n: &red_core::PlanNode| n.metrics.iter().any(|(k, _)| k.starts_with("actual"));
        assert!(
            plan.nodes.iter().any(has_actual)
                || plan.nodes.iter().flat_map(|n| &n.children).any(has_actual),
            "ANALYZE plan carries actual metrics: {}",
            plan.raw
        );

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn caps_display_keeps_key_and_export() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let t = tag("cap");
        driver
            .execute(&format!(
                "CREATE TABLE {t} (id INT PRIMARY KEY, t TEXT, b BYTEA)"
            ))
            .await
            .unwrap();
        // One row whose text and blob both far exceed the display cap.
        driver
            .execute(&format!(
                "INSERT INTO {t} VALUES (1, repeat('a', 5000), decode(repeat('61', 5000), 'hex'))"
            ))
            .await
            .unwrap();
        let key = KeySpec::single("id", KeyKind::Int);
        battery::caps_display_keeps_key_and_export(
            &driver,
            &format!("SELECT id, t, b FROM {t}"),
            &key,
            b'a',
            5000,
            5000,
            &t,
        )
        .await;
        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }
}
