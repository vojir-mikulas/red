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

/// Column metadata for a result set. `name` is always present; `decl_type` is the
/// engine's declared type (best-effort — `None` for computed expressions) and
/// feeds type-aware cell rendering later (M5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub decl_type: Option<String>,
}

/// One bounded window of rows pulled from a streaming cursor. The streaming path
/// never materializes a whole result — it yields these fixed-size windows.
#[derive(Debug, Clone, Default)]
pub struct RowWindow {
    pub rows: Vec<Vec<Value>>,
    /// `true` once this window reaches the end of the result — no more fetches.
    pub exhausted: bool,
}

/// Per-query knobs carried UI → service → driver.
#[derive(Debug, Clone)]
pub struct QueryOptions {
    /// Max rows per fetched window.
    pub window: usize,
    /// Abort a single fetch that stalls longer than this — guards a runaway
    /// query that computes a huge intermediate before yielding row 1.
    /// `None` = no cap.
    pub timeout: Option<std::time::Duration>,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            window: 1000,
            timeout: None,
        }
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
    /// A fetch was aborted out-of-band (user cancel). Distinct from a failure so
    /// the service can emit a clean "cancelled" rather than an error.
    #[error("query cancelled")]
    Interrupted,
    /// A fetch exceeded its configured timeout.
    #[error("query timed out")]
    Timeout,
}

pub type Result<T> = std::result::Result<T, RedError>;
