// SPDX-License-Identifier: GPL-3.0-or-later

//! SQLite driver. `rusqlite` is synchronous and its `Connection`/`Statement`/
//! `Rows` form a `!Send`, self-referential stack that can't cross an `.await` or
//! move between threads. So a live cursor lives entirely on one dedicated
//! blocking-pool thread that owns that stack for the cursor's lifetime and serves
//! bounded row windows over channels; the async side holds only a thin handle.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use red_core::{Column, QueryOptions, RedError, Result, RowWindow, Value};
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
