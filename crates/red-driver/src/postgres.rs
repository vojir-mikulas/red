//! PostgreSQL driver — the second source of `DatabaseDriver`, proving the
//! abstraction on a real network engine. Built on `tokio-postgres`: a live
//! `Client` (its connection driven by a background task), a streaming cursor over
//! `query_raw`, and **out-of-band cancel** via `tokio-postgres`'s `CancelToken`
//! (a separate cancel-request connection, not a dropped future).
//!
//! Caveats for v0.1: connections are `NoTls` (TLS is the next hardening step).
//! Value mapping covers the common scalar types — bool/int/float/text/bytea —
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
    Column, ColumnMeta, ExportFormat, ForeignKeyMeta, IndexMeta, KeySpec, ObjectKind, ObjectMeta,
    QueryOptions, RedError, Result, ResultPage, RowWindow, SchemaMeta, TableDetail, Value,
};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Mutex as StdMutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, NoTls, Row, RowStream, Statement};

use crate::format::{
    csv_cell, csv_record, json_string, json_value, strip_trailing, ProgressThrottle,
};
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
/// one-shot fetches borrow — see [`FETCH_POOL_CAP`].
pub struct PostgresDriver {
    client: Arc<Client>,
    version: String,
    dsn: String,
    read_only: bool,
    /// Idle fetch connections, returned after each one-shot fetch. A free list, not
    /// a semaphore — `acquire` opens a fresh connection when it's empty.
    pool: StdMutex<Vec<Arc<Client>>>,
}

/// No bind parameters — `query_raw` needs a typed iterator, so spell out the kind.
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
    let stmt = client.prepare(sql).await.map_err(driver_err)?;
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
        let (client, connection) = tokio_postgres::connect(dsn, NoTls)
            .await
            .map_err(|e| RedError::Connect(e.to_string()))?;
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
                Some(_) => continue, // closed — drop it and try the next
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

    async fn open_cursor(&self, sql: &str, _opts: QueryOptions) -> Result<Box<dyn QueryCursor>> {
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
                ColumnMeta {
                    primary_key: pk.contains(&name),
                    not_null: nullable == "NO",
                    type_name: Some(type_name),
                    default,
                    name,
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

    async fn count(&self, sql: &str, abort: &AbortSignal) -> Result<i64> {
        let sql = format!("SELECT count(*) FROM ({}) AS _red", strip_trailing(sql));
        self.with_fetch_conn(abort, |client| async move {
            let row = client.query_one(&sql, &[]).await.map_err(map_pg_err)?;
            Ok(row.get(0))
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
        let (where_clause, order_by) =
            crate::seek_clauses(key, bound_len, descending, false, pg_quote, |i| {
                format!("${}{}", i + 1, pg_cast(&bound.unwrap()[i]))
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
        let (where_clause, order_by) =
            crate::seek_clauses(key, bound_len, false, true, pg_quote, |i| {
                format!("${}{}", i + 1, pg_cast(&from.unwrap()[i]))
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
        self.client
            .batch_execute("BEGIN")
            .await
            .map_err(driver_err)?;
        match self.client.execute(sql, &[]).await {
            Ok(affected) => {
                self.client
                    .batch_execute("COMMIT")
                    .await
                    .map_err(driver_err)?;
                Ok(affected)
            }
            Err(e) => {
                let _ = self.client.batch_execute("ROLLBACK").await;
                Err(map_pg_err(e))
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
        let (stmt, columns) = prepare_columns(&self.client, &sql).await?;
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();

        let stream = self
            .client
            .query_raw(&stmt, no_params())
            .await
            .map_err(driver_err)?;
        futures_util::pin_mut!(stream);

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
                while let Some(row) = stream.next().await {
                    bail_if_cancelled!();
                    let row = row.map_err(map_pg_err)?;
                    let cells = pg_row(&row, None);
                    let fields: Vec<String> = cells.iter().map(csv_cell).collect();
                    writeln!(out, "{}", csv_record(fields.iter().map(String::as_str)))
                        .map_err(driver_err)?;
                    written += 1;
                    throttle.tick(written);
                }
            }
            ExportFormat::Json => {
                write!(out, "[").map_err(driver_err)?;
                while let Some(row) = stream.next().await {
                    bail_if_cancelled!();
                    let row = row.map_err(map_pg_err)?;
                    let cells = pg_row(&row, None);
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

/// The async-side cursor: column metadata + the live row stream behind a `Mutex`
/// (so `next_window(&self)` can pull) + the out-of-band cancel token.
struct PgCursor {
    columns: Vec<Column>,
    stream: Mutex<Pin<Box<RowStream>>>,
    cancel: CancelToken,
}

#[async_trait]
impl QueryCursor for PgCursor {
    fn columns(&self) -> &[Column] {
        &self.columns
    }

    async fn next_window(&self, max: usize) -> Result<RowWindow> {
        // Offset-mode display stream (editor run) — cap every cell, no key exempt.
        let cap = CellCap::display([None, None]);
        let mut stream = self.stream.lock().await;
        let mut rows = Vec::with_capacity(max);
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

/// The explicit cast for a seek-bound placeholder, from the bound value's wire
/// type (see `fetch_seek`). A bound comes from the key column, never capped.
fn pg_cast(value: &Value) -> &'static str {
    match value {
        Value::Integer(_) => "::int8",
        Value::Real(_) => "::float8",
        Value::Text(_) => "::text",
        Value::Blob(_) => "::bytea",
        Value::Null | Value::Capped(_) => "",
    }
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
                Value::Text(s) => Box::new(s.clone()),
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
        Type::BOOL => row
            .try_get::<_, Option<bool>>(i)
            .ok()
            .flatten()
            .map(|b| Value::Integer(b as i64))
            .unwrap_or(Value::Null),
        Type::INT2 => int_value(
            row.try_get::<_, Option<i16>>(i)
                .ok()
                .flatten()
                .map(i64::from),
        ),
        Type::INT4 => int_value(
            row.try_get::<_, Option<i32>>(i)
                .ok()
                .flatten()
                .map(i64::from),
        ),
        Type::INT8 => int_value(row.try_get::<_, Option<i64>>(i).ok().flatten()),
        Type::FLOAT4 => row
            .try_get::<_, Option<f32>>(i)
            .ok()
            .flatten()
            .map(|x| Value::Real(x as f64))
            .unwrap_or(Value::Null),
        Type::FLOAT8 => row
            .try_get::<_, Option<f64>>(i)
            .ok()
            .flatten()
            .map(Value::Real)
            .unwrap_or(Value::Null),
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
        // text / varchar / name / bpchar / unknown — and a best-effort for the rest.
        // `&str` and `String` accept the same types, so capping doesn't change which
        // columns decode (only how much of an over-cap one is kept).
        //
        // `try_get` returns `Ok(None)` for a SQL NULL and `Err` when the target type
        // *rejects* the column type (its `accepts` said no). The former is a genuine
        // `Null`; the latter is an unmapped type the string decode declined (enum,
        // inet, interval, array, …), and rather than collapse it to a silent NULL we
        // fall back to its raw wire bytes as lossy UTF-8 — correct for the text-shaped
        // wire forms (enum labels, citext-likes) and a visible cell for the rest.
        _ => match row.try_get::<_, Option<&str>>(i) {
            Ok(None) => Value::Null,
            Ok(Some(s)) => match max {
                Some(max) => Value::capped_text(s, max),
                None => Value::Text(s.to_string()),
            },
            Err(_) => raw_text_fallback(row, i, max),
        },
    }
}

fn int_value(v: Option<i64>) -> Value {
    v.map(Value::Integer).unwrap_or(Value::Null)
}

/// Captures a column's raw binary wire bytes verbatim, so the driver can render the
/// types `tokio-postgres` declines to decode itself (see [`crate::pg_text`]).
/// `accepts` is unconditional — it's only ever asked for via the explicit type
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
            None => Value::Text(s),
        })
        .unwrap_or(Value::Null)
}

/// Last-resort render for a column type the scalar/`pg_text` arms don't name and
/// that the string decode rejected (its `accepts` said no): take the raw wire bytes
/// and render them as lossy UTF-8. Correct for the text-shaped binary forms (enum
/// labels, `citext`-likes, domains over text) and at worst a visible cell for the
/// others — anything but the silent NULL the bare string decode would have produced.
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

/// Map a cancel (SQLSTATE 57014) to the distinct `Interrupted`, else a driver error.
fn map_pg_err(e: tokio_postgres::Error) -> RedError {
    if let Some(db) = e.as_db_error() {
        if db.code() == &tokio_postgres::error::SqlState::QUERY_CANCELED {
            return RedError::Interrupted;
        }
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
                None => return,
            }
        };
    }

    /// The connection's current schema — unqualified fixtures land here, so
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
            Value::Text(s) => s.clone(),
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
            Value::Text(s) => s.clone(),
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
    /// a silent NULL" — the exact bytes are server-version dependent, so assert only
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
        // `generate_series` is a server-side streaming row source — no fixture, and
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

        for obj in [
            format!("VIEW {recent}"),
            format!("TABLE {books}"),
            format!("TABLE {authors}"),
        ] {
            driver.execute(&format!("DROP {obj}")).await.unwrap();
        }
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
            tiebreak: Some("id".into()),
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
