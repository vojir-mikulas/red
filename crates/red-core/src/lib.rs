// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared domain types for RED. No UI, no async runtime, no driver knowledge —
//! the protocol/service/UI layers all speak in these types. Mirrors the role of
//! `nyx-core` in the Nyx codebase.

use std::fmt;

/// Which database engine a connection targets. Drives driver selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    Sqlite,
    Postgres,
}

impl fmt::Display for DbKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbKind::Sqlite => write!(f, "SQLite"),
            DbKind::Postgres => write!(f, "PostgreSQL"),
        }
    }
}

/// A saved connection target. `dsn` is the SQLite file path or the Postgres URL.
/// `read_only` reflects RED's read-mostly safety posture (enforced by the driver).
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub name: String,
    pub kind: DbKind,
    pub dsn: String,
    pub read_only: bool,
}

/// One cell value in a result set. A deliberately small, render-friendly tagged
/// union — drivers map their native types onto this; the UI formats per variant.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Integer(n) => write!(f, "{n}"),
            Value::Real(x) => write!(f, "{x}"),
            Value::Text(s) => write!(f, "{s}"),
            Value::Blob(b) => write!(f, "<{} bytes>", b.len()),
        }
    }
}

/// A materialized query result. NOTE: this is the eager shape for the scaffold;
/// the streaming/windowed cursor (the real performance story) replaces this for
/// large result sets — see docs/plans in the Nyx repo (red-db-explorer.md).
#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl QueryResult {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}

/// The error type that crosses every RED layer boundary.
#[derive(Debug, thiserror::Error)]
pub enum RedError {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("query failed: {0}")]
    Query(String),
    #[error("driver error: {0}")]
    Driver(String),
}

pub type Result<T> = std::result::Result<T, RedError>;
