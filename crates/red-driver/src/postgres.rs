//! PostgreSQL driver — the second source of `DatabaseDriver`, proving the
//! abstraction on a real network engine. Built on `tokio-postgres`: a live
//! `Client` (its connection driven by a background task), a streaming cursor over
//! `query_raw`, and **out-of-band cancel** via `tokio-postgres`'s `CancelToken`
//! (a separate cancel-request connection, not a dropped future).
//!
//! Caveats for v0.1: connections are `NoTls` (TLS is the next hardening step),
//! and value mapping covers the common scalar types — bool/int/float/text/bytea —
//! with a text fallback; richer types (numeric, timestamp, json, uuid) surface as
//! NULL until typed rendering lands. Read-only sets
//! `default_transaction_read_only`.

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use red_core::{
    Column, ColumnMeta, ExportFormat, ForeignKeyMeta, IndexMeta, KeySpec, ObjectKind, ObjectMeta,
    QueryOptions, RedError, Result, ResultPage, RowWindow, SchemaMeta, TableDetail, Value,
};
use std::fs::File;
use std::io::{BufWriter, Write};
use tokio::sync::Mutex;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, NoTls, Row, RowStream};

use crate::format::{csv_cell, csv_record, json_string, json_value, strip_trailing};
use crate::{driver_err, CancelToken, DatabaseDriver, QueryCursor};

/// A live PostgreSQL session. Holds the shared `Client`; the connection future
/// runs on a background task spawned at connect.
pub struct PostgresDriver {
    client: Arc<Client>,
    version: String,
}

/// No bind parameters — `query_raw` needs a typed iterator, so spell out the kind.
fn no_params() -> Vec<&'static (dyn ToSql + Sync)> {
    Vec::new()
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
        })
    }

    /// Prepare `sql` and read its column metadata (works even for an empty result).
    async fn prepare_columns(&self, sql: &str) -> Result<(tokio_postgres::Statement, Vec<Column>)> {
        let stmt = self.client.prepare(sql).await.map_err(driver_err)?;
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
        let (stmt, columns) = self.prepare_columns(sql).await?;
        let stream = self
            .client
            .query_raw(&stmt, no_params())
            .await
            .map_err(driver_err)?;

        // Out-of-band cancel: a separate cancel request over a fresh connection.
        let cancel_token = self.client.cancel_token();
        let cancel = CancelToken::new(move || {
            let token = cancel_token.clone();
            tokio::spawn(async move {
                let _ = token.cancel_query(NoTls).await;
            });
        });

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

    async fn count(&self, sql: &str) -> Result<i64> {
        let sql = format!("SELECT count(*) FROM ({}) AS _red", strip_trailing(sql));
        let row = self.client.query_one(&sql, &[]).await.map_err(driver_err)?;
        Ok(row.get(0))
    }

    async fn fetch_page(&self, sql: &str, offset: usize, limit: usize) -> Result<ResultPage> {
        let sql = format!(
            "SELECT * FROM ({}) AS _red LIMIT {limit} OFFSET {offset}",
            strip_trailing(sql)
        );
        let (stmt, columns) = self.prepare_columns(&sql).await?;
        let rows = self.client.query(&stmt, &[]).await.map_err(driver_err)?;
        Ok(ResultPage {
            columns,
            rows: rows.iter().map(pg_row).collect(),
        })
    }

    async fn fetch_seek(
        &self,
        sql: &str,
        key: &KeySpec,
        bound: Option<&Value>,
        descending: bool,
        limit: usize,
    ) -> Result<ResultPage> {
        let col = pg_quote(&key.column);
        let base = strip_trailing(sql);
        let (cmp, ord) = if descending {
            ("<", "DESC")
        } else {
            (">", "ASC")
        };
        // The placeholder carries an explicit cast: the parameter's wire type is
        // fixed by the Rust value (i64 → int8), and without the cast Postgres
        // would infer the column's narrower type (int4) and reject the bind.
        let sql = match bound {
            Some(value) => format!(
                "SELECT * FROM ({base}) AS _red WHERE {col} {cmp} $1{cast} \
                 ORDER BY {col} {ord} LIMIT {limit}",
                cast = pg_cast(value)
            ),
            None => format!("SELECT * FROM ({base}) AS _red ORDER BY {col} {ord} LIMIT {limit}"),
        };
        let (stmt, columns) = self.prepare_columns(&sql).await?;
        let rows = match bound {
            Some(Value::Integer(n)) => self.client.query(&stmt, &[n]).await,
            Some(Value::Real(x)) => self.client.query(&stmt, &[x]).await,
            Some(Value::Text(s)) => self.client.query(&stmt, &[s]).await,
            Some(Value::Blob(b)) => self.client.query(&stmt, &[b]).await,
            Some(Value::Null) => return Err(RedError::Query("null seek bound".into())),
            None => self.client.query(&stmt, &[]).await,
        }
        .map_err(map_pg_err)?;
        Ok(ResultPage {
            columns,
            rows: rows.iter().map(pg_row).collect(),
        })
    }

    async fn key_bounds(&self, sql: &str, key: &KeySpec) -> Result<Option<(i64, i64)>> {
        let col = pg_quote(&key.column);
        let sql = format!(
            "SELECT min({col}), max({col}) FROM ({}) AS _red",
            strip_trailing(sql)
        );
        let rows = self.client.query(&sql, &[]).await.map_err(driver_err)?;
        Ok(rows
            .first()
            .map(pg_row)
            .and_then(|cells| match (cells.first(), cells.get(1)) {
                (Some(Value::Integer(min)), Some(Value::Integer(max))) => Some((*min, *max)),
                _ => None,
            }))
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

    async fn export(&self, sql: &str, path: &Path, format: ExportFormat) -> Result<u64> {
        let sql = format!("SELECT * FROM ({}) AS _red", strip_trailing(sql));
        let (stmt, columns) = self.prepare_columns(&sql).await?;
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

        match format {
            ExportFormat::Csv => {
                writeln!(out, "{}", csv_record(names.iter().map(String::as_str)))
                    .map_err(driver_err)?;
                while let Some(row) = stream.next().await {
                    let row = row.map_err(map_pg_err)?;
                    let cells = pg_row(&row);
                    let fields: Vec<String> = cells.iter().map(csv_cell).collect();
                    writeln!(out, "{}", csv_record(fields.iter().map(String::as_str)))
                        .map_err(driver_err)?;
                    written += 1;
                }
            }
            ExportFormat::Json => {
                write!(out, "[").map_err(driver_err)?;
                while let Some(row) = stream.next().await {
                    let row = row.map_err(map_pg_err)?;
                    let cells = pg_row(&row);
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
        let mut stream = self.stream.lock().await;
        let mut rows = Vec::with_capacity(max);
        for _ in 0..max {
            match stream.next().await {
                Some(Ok(row)) => rows.push(pg_row(&row)),
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
/// type (see `fetch_seek`).
fn pg_cast(value: &Value) -> &'static str {
    match value {
        Value::Integer(_) => "::int8",
        Value::Real(_) => "::float8",
        Value::Text(_) => "::text",
        Value::Blob(_) => "::bytea",
        Value::Null => "",
    }
}

/// Map one row's cells to [`Value`]s by column type (text fallback for the rest).
fn pg_row(row: &Row) -> Vec<Value> {
    (0..row.len()).map(|i| pg_value(row, i)).collect()
}

fn pg_value(row: &Row, i: usize) -> Value {
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
        Type::BYTEA => row
            .try_get::<_, Option<Vec<u8>>>(i)
            .ok()
            .flatten()
            .map(Value::Blob)
            .unwrap_or(Value::Null),
        // text / varchar / name / bpchar / unknown — and a best-effort for the rest.
        _ => row
            .try_get::<_, Option<String>>(i)
            .ok()
            .flatten()
            .map(Value::Text)
            .unwrap_or(Value::Null),
    }
}

fn int_value(v: Option<i64>) -> Value {
    v.map(Value::Integer).unwrap_or(Value::Null)
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
            .fetch_page("SELECT current_schema()", 0, 1)
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

        let key = KeySpec {
            column: "id".into(),
            kind: KeyKind::Int,
        };
        battery::seeks_forward_backward_and_reads_bounds(
            &driver,
            &format!("SELECT * FROM {t}"),
            &key,
        )
        .await;

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn read_only_rejects_writes() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        battery::read_only_rejects_write(&driver, "CREATE TABLE red_ro_should_fail (x INT)").await;
    }
}
