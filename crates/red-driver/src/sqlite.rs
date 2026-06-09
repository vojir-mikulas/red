// SPDX-License-Identifier: GPL-3.0-or-later

//! SQLite driver. `rusqlite` is synchronous and its `Connection` is `!Send`, so
//! every call hops onto `spawn_blocking` and opens a fresh connection there —
//! never holding a connection across an `.await`. Good enough for the scaffold;
//! a pooled/streamed variant comes with the windowed-cursor work.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use red_core::{QueryResult, Result, Value};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};

use crate::{driver_err, DatabaseDriver};

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

    fn run_query(path: &Path, sql: &str, read_only: bool) -> Result<QueryResult> {
        let conn = Self::open(path, read_only)?;
        let mut stmt = conn.prepare(sql).map_err(driver_err)?;
        let column_count = stmt.column_count();
        let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();

        let mut out = Vec::new();
        let mut rows = stmt.query([]).map_err(driver_err)?;
        while let Some(row) = rows.next().map_err(driver_err)? {
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
            out.push(cells);
        }
        Ok(QueryResult { columns, rows: out })
    }
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

    async fn query(&self, sql: &str) -> Result<QueryResult> {
        let path = self.path.clone();
        let read_only = self.read_only;
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || Self::run_query(&path, &sql, read_only))
            .await
            .map_err(driver_err)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_a_scalar_query() {
        let driver = SqliteDriver::new(":memory:", false);
        let result = driver
            .query("SELECT 1 AS one, 'hi' AS greeting")
            .await
            .unwrap();
        assert_eq!(result.columns, vec!["one", "greeting"]);
        assert_eq!(result.row_count(), 1);
        assert_eq!(result.rows[0][0], Value::Integer(1));
        assert_eq!(result.rows[0][1], Value::Text("hi".into()));
    }
}
