// SPDX-License-Identifier: GPL-3.0-or-later

//! SQLite driver. `rusqlite` is synchronous and its `Connection`/`Statement`/
//! `Rows` form a `!Send`, self-referential stack that can't cross an `.await` or
//! move between threads. So a live cursor lives entirely on one dedicated
//! blocking-pool thread that owns that stack for the cursor's lifetime and serves
//! bounded row windows over channels; the async side holds only a thin handle.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use red_core::{
    Column, ColumnMeta, ForeignKeyMeta, IndexMeta, ObjectKind, ObjectMeta, QueryOptions, RedError,
    Result, RowWindow, SchemaMeta, TableDetail, Value,
};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, OpenFlags};
use tokio::sync::{mpsc, oneshot};

use crate::{driver_err, CancelToken, DatabaseDriver, QueryCursor};

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

        let (req_tx, req_rx) = mpsc::unbounded_channel::<FetchReq>();
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
    mut req_rx: mpsc::UnboundedReceiver<FetchReq>,
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
    let mut out = Vec::with_capacity(max);
    for _ in 0..max {
        match rows.next() {
            Ok(Some(row)) => out.push(extract_row(row, column_count)?),
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

fn extract_row(row: &rusqlite::Row<'_>, column_count: usize) -> Result<Vec<Value>> {
    let mut cells = Vec::with_capacity(column_count);
    for i in 0..column_count {
        let value = match row.get_ref(i).map_err(driver_err)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => Value::Integer(n),
            ValueRef::Real(x) => Value::Real(x),
            ValueRef::Text(s) => Value::Text(String::from_utf8_lossy(s).into_owned()),
            ValueRef::Blob(b) => Value::Blob(b.to_vec()),
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
    req_tx: mpsc::UnboundedSender<FetchReq>,
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

    /// Generate `n` rows (1..=n) without needing a fixture table; SQLite streams
    /// the recursive CTE incrementally, which is exactly what we want to test.
    fn counting_sql(n: i64) -> String {
        format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < {n}) SELECT x FROM c"
        )
    }

    #[tokio::test]
    async fn streams_in_bounded_windows() {
        let driver = SqliteDriver::new(":memory:", true);
        let cursor = driver
            .open_cursor(&counting_sql(100_000), QueryOptions::default())
            .await
            .unwrap();
        assert_eq!(cursor.columns().len(), 1);
        assert_eq!(cursor.columns()[0].name, "x");

        let mut total = 0usize;
        loop {
            let window = cursor.next_window(1000).await.unwrap();
            assert!(window.rows.len() <= 1000, "windows stay bounded");
            total += window.rows.len();
            if window.exhausted {
                break;
            }
        }
        assert_eq!(total, 100_000);
    }

    #[tokio::test]
    async fn cancel_aborts_in_flight_fetch() {
        let driver = SqliteDriver::new(":memory:", true);
        // Huge bound so the first step runs long enough to interrupt.
        let cursor = driver
            .open_cursor(&counting_sql(1_000_000_000), QueryOptions::default())
            .await
            .unwrap();
        let cancel = cursor.cancel_token();

        let fetch = tokio::spawn(async move { cursor.next_window(1_000_000_000).await });
        // `sqlite3_interrupt` is a no-op if no step is running yet, so let the
        // first step get well underway before interrupting out-of-band.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();

        match fetch.await.unwrap() {
            Err(RedError::Interrupted) => {}
            other => panic!("expected Interrupted, got {other:?}"),
        }
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

        let schemas = driver.list_objects().await.unwrap();
        let main = schemas.iter().find(|s| s.name == "main").unwrap();
        let objects: Vec<_> = main
            .objects
            .iter()
            .map(|o| (o.name.as_str(), o.kind))
            .collect();
        assert!(objects.contains(&("authors", ObjectKind::Table)));
        assert!(objects.contains(&("books", ObjectKind::Table)));
        assert!(objects.contains(&("recent_books", ObjectKind::View)));

        let books = driver.describe_table("main", "books").await.unwrap();
        let col = |n: &str| books.columns.iter().find(|c| c.name == n).unwrap();
        assert!(col("id").primary_key);
        assert!(col("title").not_null);
        assert_eq!(col("title").default.as_deref(), Some("'untitled'"));
        assert_eq!(col("author_id").type_name.as_deref(), Some("INTEGER"));

        assert_eq!(books.foreign_keys.len(), 1);
        let fk = &books.foreign_keys[0];
        assert_eq!(
            (
                fk.column.as_str(),
                fk.ref_table.as_str(),
                fk.ref_column.as_str()
            ),
            ("author_id", "authors", "id")
        );

        assert!(books
            .indexes
            .iter()
            .any(|i| i.columns == vec!["author_id".to_string()]));

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn read_only_rejects_writes() {
        let driver = SqliteDriver::new(":memory:", true);
        // The write is rejected when the statement steps, which (for a cheap
        // `open_cursor`) is the first `next_window` — accept rejection at either.
        let outcome = match driver
            .open_cursor("CREATE TABLE t(x)", QueryOptions::default())
            .await
        {
            Err(_) => return,
            Ok(cursor) => cursor.next_window(1).await,
        };
        assert!(outcome.is_err(), "read-only connection must reject DDL/DML");
    }
}
