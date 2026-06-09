// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared domain types for RED. No UI, no async runtime, no driver knowledge —
//! the protocol/service/UI layers all speak in these types. Mirrors the role of
//! `nyx-core` in the Nyx codebase.

use std::fmt;

/// Which database engine a connection targets. Drives driver selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

/// What a schema object is. SQLite has tables and views; Postgres (M7) maps onto
/// the same two for the explorer's purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Table,
    View,
}

/// A namespace of objects — the top level of the schema tree. For SQLite this is
/// a database from `PRAGMA database_list` (`main` / `temp` / an attached DB); for
/// Postgres (M7) it's a real schema. One level so both engines fit the same tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaMeta {
    pub name: String,
    /// Names + kinds only — the cheap tree skeleton, loaded on connect. Column
    /// detail is pulled per table via [`TableDetail`] on expand.
    pub objects: Vec<ObjectMeta>,
}

/// One table or view in a [`SchemaMeta`] — just enough to draw the tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub name: String,
    pub kind: ObjectKind,
}

/// On-demand detail for one table/view: its columns, foreign keys, and indexes.
/// Loaded lazily when the user expands the object, never as part of the skeleton.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableDetail {
    pub columns: Vec<ColumnMeta>,
    pub foreign_keys: Vec<ForeignKeyMeta>,
    pub indexes: Vec<IndexMeta>,
}

/// One column of a table/view. Richer than result-set [`Column`]: it carries the
/// schema facts the tree shows (nullability, primary-key membership, default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    /// Declared type, best-effort (`None` for an untyped/computed column).
    pub type_name: Option<String>,
    pub not_null: bool,
    pub primary_key: bool,
    pub default: Option<String>,
}

/// A foreign-key edge from a local column to a referenced table/column. The tree
/// derives a column's FK badge by matching `column` against the table's columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyMeta {
    pub column: String,
    pub ref_table: String,
    pub ref_column: String,
}

/// An index over one or more columns of a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMeta {
    pub name: String,
    pub unique: bool,
    pub columns: Vec<String>,
}

/// One bounded window of rows pulled from a streaming cursor. The streaming path
/// never materializes a whole result — it yields these fixed-size windows.
#[derive(Debug, Clone, Default)]
pub struct RowWindow {
    pub rows: Vec<Vec<Value>>,
    /// `true` once this window reaches the end of the result — no more fetches.
    pub exhausted: bool,
}

/// One random-access page of a result, fetched by `(offset, limit)`. Backs the
/// result grid's load-on-scroll: a bounded window buffer requests the pages
/// around the viewport and evicts the rest, so memory stays flat over a
/// multi-million-row result. Columns ride along (stable, but cheap to repeat).
#[derive(Debug, Clone, Default)]
pub struct ResultPage {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

/// A streamed-export target format. The driver writes rows straight to disk
/// row-by-row, never materializing the whole result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
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
