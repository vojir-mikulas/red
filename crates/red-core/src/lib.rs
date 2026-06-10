// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared domain types for RED. No UI, no async runtime, no driver knowledge —
//! the protocol/service/UI layers all speak in these types. Mirrors the role of
//! `nyx-core` in the Nyx codebase.

use std::fmt;

/// Which database engine a connection targets. Drives driver selection and,
/// via [`DbKind::all`]/the metadata accessors, how the connection form renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DbKind {
    Postgres,
    #[default]
    Sqlite,
    Mysql,
}

impl DbKind {
    /// Every engine, in the order the connection form lists them. Adding a driver
    /// means adding a variant here and an arm to each accessor below — the form is
    /// data-driven off these, not a hand-maintained list.
    pub const fn all() -> &'static [DbKind] {
        &[DbKind::Postgres, DbKind::Sqlite, DbKind::Mysql]
    }

    /// File-based engines (SQLite) take a single path, not host/port/user/pass.
    pub const fn is_file(self) -> bool {
        matches!(self, DbKind::Sqlite)
    }

    /// The conventional default port, or `None` for file engines.
    pub const fn default_port(self) -> Option<u16> {
        match self {
            DbKind::Postgres => Some(5432),
            DbKind::Mysql => Some(3306),
            DbKind::Sqlite => None,
        }
    }

    /// The URL scheme used when composing a connection string for the driver.
    pub const fn url_scheme(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgres",
            DbKind::Mysql => "mysql",
            DbKind::Sqlite => "sqlite",
        }
    }

    /// Map a connection-string scheme (`postgresql`, `mariadb`, `file`, …) onto an
    /// engine, for parsing a pasted DSN.
    pub fn from_scheme(scheme: &str) -> Option<DbKind> {
        match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Some(DbKind::Postgres),
            "mysql" | "mariadb" => Some(DbKind::Mysql),
            "sqlite" | "sqlite3" | "file" => Some(DbKind::Sqlite),
            _ => None,
        }
    }
}

impl fmt::Display for DbKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbKind::Sqlite => write!(f, "SQLite"),
            DbKind::Postgres => write!(f, "PostgreSQL"),
            DbKind::Mysql => write!(f, "MySQL/MariaDB"),
        }
    }
}

/// A saved connection target. Stored as structured fields rather than one opaque
/// DSN so the form can offer both entry modes and so engines stay swappable; the
/// driver-facing connection string is composed on demand by [`Self::dsn`]. For a
/// file engine the path lives in `database`; `host`/`port`/`user`/`password` are
/// unused. `color` is a label-palette index (UI-defined). `read_only` reflects
/// RED's read-mostly safety posture (enforced by the driver).
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ConnectionConfig {
    pub name: String,
    pub kind: DbKind,
    #[cfg_attr(feature = "serde", serde(default))]
    pub host: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub port: Option<u16>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub user: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub password: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub database: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub color: u8,
    #[cfg_attr(feature = "serde", serde(default))]
    pub read_only: bool,
}

impl ConnectionConfig {
    /// The connection string handed to the driver. File engines yield the bare
    /// path; network engines compose `scheme://user:pass@host:port/database`, with
    /// the userinfo and database percent-encoded so credentials with reserved
    /// characters survive the round-trip.
    pub fn dsn(&self) -> String {
        if self.kind.is_file() {
            return self.database.clone();
        }
        let mut url = format!("{}://", self.kind.url_scheme());
        if !self.user.is_empty() {
            url.push_str(&encode(&self.user));
            if !self.password.is_empty() {
                url.push(':');
                url.push_str(&encode(&self.password));
            }
            url.push('@');
        }
        url.push_str(&self.host);
        if let Some(port) = self.port {
            url.push(':');
            url.push_str(&port.to_string());
        }
        url.push('/');
        url.push_str(&encode(&self.database));
        url
    }

    /// A short human label for the connection's target — the file path, or
    /// `user@host:port/database` — shown on cards and in the status bar.
    pub fn display_target(&self) -> String {
        if self.kind.is_file() {
            return self.database.clone();
        }
        let mut s = String::new();
        if !self.user.is_empty() {
            s.push_str(&self.user);
            s.push('@');
        }
        s.push_str(&self.host);
        if let Some(port) = self.port {
            s.push(':');
            s.push_str(&port.to_string());
        }
        if !self.database.is_empty() {
            s.push('/');
            s.push_str(&self.database);
        }
        s
    }

    /// Decompose a pasted connection string into engine + fields, best-effort.
    /// Returns `None` if there's no recognizable `scheme://…`. Query strings and
    /// fragments are ignored — RED keeps "roughly enough" of the URL to refill the
    /// form, not every libpq option.
    pub fn parse_conn_str(input: &str) -> Option<ParsedDsn> {
        let input = input.trim();
        let (scheme, rest) = input.split_once("://")?;
        let kind = DbKind::from_scheme(scheme).unwrap_or(DbKind::Postgres);
        // Drop any ?query / #fragment tail.
        let rest = rest
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .trim_start_matches('/');

        if kind.is_file() {
            // `sqlite:///abs/path` → authority empty, path is the file. Hand the
            // whole remainder back as the database/path.
            return Some(ParsedDsn {
                kind,
                database: format!("/{}", rest.trim_start_matches('/')),
                ..ParsedDsn::for_kind(kind)
            });
        }

        // authority[/database]
        let (authority, database) = match rest.split_once('/') {
            Some((a, db)) => (a, db.to_string()),
            None => (rest, String::new()),
        };
        // [user[:pass]@]host[:port]
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        let (user, password) = match userinfo {
            Some(u) => match u.split_once(':') {
                Some((user, pass)) => (decode(user), decode(pass)),
                None => (decode(u), String::new()),
            },
            None => (String::new(), String::new()),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()),
            None => (hostport.to_string(), None),
        };
        Some(ParsedDsn {
            kind,
            host,
            port: port.or_else(|| kind.default_port()),
            user,
            password,
            database: decode(&database),
        })
    }
}

/// The fields recovered from a pasted connection string by
/// [`ConnectionConfig::parse_conn_str`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDsn {
    pub kind: DbKind,
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl ParsedDsn {
    fn for_kind(kind: DbKind) -> Self {
        ParsedDsn {
            kind,
            host: String::new(),
            port: kind.default_port(),
            user: String::new(),
            password: String::new(),
            database: String::new(),
        }
    }
}

/// Percent-encode the characters that would otherwise be parsed as URL syntax
/// inside a userinfo/database component. Deliberately small — not a general
/// RFC 3986 encoder, just enough to keep credentials intact.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'@' | b':' | b'/' | b'?' | b'#' | b'[' | b']' | b'%' | b' ' => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
            _ => out.push(b as char),
        }
    }
    out
}

/// Reverse of [`encode`]: decode `%XX` escapes, leaving anything malformed as-is.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

#[cfg(test)]
mod conn_tests {
    use super::*;

    #[test]
    fn dsn_composes_network_url() {
        let cfg = ConnectionConfig {
            kind: DbKind::Postgres,
            host: "localhost".into(),
            port: Some(5432),
            user: "postgres".into(),
            password: "p@ss:word".into(),
            database: "analytics".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.dsn(),
            "postgres://postgres:p%40ss%3Aword@localhost:5432/analytics"
        );
    }

    #[test]
    fn dsn_for_file_is_the_path() {
        let cfg = ConnectionConfig {
            kind: DbKind::Sqlite,
            database: "/tmp/app.sqlite".into(),
            ..Default::default()
        };
        assert_eq!(cfg.dsn(), "/tmp/app.sqlite");
    }

    #[test]
    fn parse_network_dsn() {
        let p = ConnectionConfig::parse_conn_str("mysql://root:secret@db.host:3307/shop").unwrap();
        assert_eq!(p.kind, DbKind::Mysql);
        assert_eq!(p.host, "db.host");
        assert_eq!(p.port, Some(3307));
        assert_eq!(p.user, "root");
        assert_eq!(p.password, "secret");
        assert_eq!(p.database, "shop");
    }

    #[test]
    fn parse_dsn_defaults_port() {
        let p = ConnectionConfig::parse_conn_str("postgresql://localhost/mydb").unwrap();
        assert_eq!(p.kind, DbKind::Postgres);
        assert_eq!(p.host, "localhost");
        assert_eq!(p.port, Some(5432));
        assert!(p.user.is_empty());
        assert_eq!(p.database, "mydb");
    }

    #[test]
    fn parse_sqlite_dsn_keeps_path() {
        let p = ConnectionConfig::parse_conn_str("sqlite:///Users/me/data/app.db").unwrap();
        assert_eq!(p.kind, DbKind::Sqlite);
        assert_eq!(p.database, "/Users/me/data/app.db");
    }

    #[test]
    fn parse_rejects_non_url() {
        assert!(ConnectionConfig::parse_conn_str("not a url").is_none());
    }

    #[test]
    fn dsn_round_trips_through_parse() {
        let cfg = ConnectionConfig {
            kind: DbKind::Postgres,
            host: "h".into(),
            port: Some(5432),
            user: "u".into(),
            password: "pw".into(),
            database: "d".into(),
            ..Default::default()
        };
        let p = ConnectionConfig::parse_conn_str(&cfg.dsn()).unwrap();
        assert_eq!(p.host, "h");
        assert_eq!(p.user, "u");
        assert_eq!(p.password, "pw");
        assert_eq!(p.database, "d");
    }
}
