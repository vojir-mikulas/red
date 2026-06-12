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
    /// A cell the *display* fetch path capped: only a bounded prefix was read
    /// (over-cap text) or only the length (blob), so the full value was never
    /// materialized. Produced solely by capped fetch paths — never by `export` or
    /// a write path, where every cell is whole.
    Capped(CappedCell),
}

/// The payload of a [`Value::Capped`] cell: what the grid shows plus the true byte
/// length of the source value, so a detail view can re-fetch the whole thing.
#[derive(Debug, Clone, PartialEq)]
pub struct CappedCell {
    /// The shown text: a char-boundary-safe prefix of over-cap text, or empty for
    /// a blob (rendered as its `<len bytes>` summary). No ellipsis — that's added
    /// at render time, so a copy path can tell the real head from the marker.
    pub head: String,
    /// True byte length of the source value (full text length, or blob size).
    pub len: usize,
    /// Blob vs text — drives the `<N bytes>` summary and the grid's faint styling.
    pub blob: bool,
}

impl Value {
    /// Build a display value from full text `s`, capping to `max_bytes`. Under the
    /// cap it's a whole [`Value::Text`]; over it, a [`Value::Capped`] holding only a
    /// char-boundary-safe prefix plus the true length — the bytes past the cap are
    /// never copied into the value, which is the point on the display fetch path.
    pub fn capped_text(s: &str, max_bytes: usize) -> Value {
        if s.len() <= max_bytes {
            return Value::Text(s.to_owned());
        }
        let mut end = max_bytes;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        Value::Capped(CappedCell {
            head: s[..end].to_owned(),
            len: s.len(),
            blob: false,
        })
    }

    /// A blob reduced to its length for display — the bytes are never read. The
    /// grid only ever paints a blob as its `<N bytes>` summary; a copy/inspector
    /// re-fetches the real bytes on demand.
    pub fn capped_blob(len: usize) -> Value {
        Value::Capped(CappedCell {
            head: String::new(),
            len,
            blob: true,
        })
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Integer(n) => write!(f, "{n}"),
            Value::Real(x) => write!(f, "{x}"),
            Value::Text(s) => write!(f, "{s}"),
            Value::Blob(b) => write!(f, "<{} bytes>", b.len()),
            Value::Capped(c) if c.blob => write!(f, "<{} bytes>", c.len),
            Value::Capped(c) => write!(f, "{}…", c.head),
        }
    }
}

/// Column metadata for a result set. `name` is always present; `decl_type` is the
/// engine's declared type (best-effort — `None` for computed expressions) and
/// feeds type-aware cell rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub decl_type: Option<String>,
}

/// A result-narrowing filter pushed into the query (Track B2). The service wraps
/// the open's base SQL in `SELECT * FROM (base) WHERE <predicate>` *before* the
/// count / key-bounds probe, so the whole result — count, keyset seek, sort,
/// export — operates on the filtered set without ever materializing it. Because
/// the wrap preserves `SELECT *`, the key column survives and keyset paging is
/// unaffected.
///
/// The UI stays driver-independent by sending this *semantic* filter, not SQL: the
/// backend renders [`Contains`](Self::Contains) to a portable, escaped predicate
/// per engine (see `DatabaseDriver::contains_predicate`) and wraps
/// [`Where`](Self::Where) — power-user SQL, same trust level as the editor —
/// verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultFilter {
    /// "Any text-representable column contains this term" — the portable quick
    /// filter. Rendered per engine to a case-insensitive `LIKE`/`ILIKE` OR-chain
    /// over the non-blob columns, with the term escaped to match literally.
    Contains(String),
    /// A raw boolean SQL expression, wrapped verbatim into the `WHERE`. For users
    /// who want a precise predicate; trusted like editor SQL.
    Where(String),
}

/// What a schema object is. SQLite has tables and views; Postgres maps onto
/// the same two for the explorer's purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Table,
    View,
}

/// A namespace of objects — the top level of the schema tree. For SQLite this is
/// a database from `PRAGMA database_list` (`main` / `temp` / an attached DB); for
/// Postgres it's a real schema. One level so both engines fit the same tree.
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

/// How a result's rows are keyed for seek (keyset) pagination: an ordered,
/// effectively-unique tuple of columns. A plain table browse is a single key
/// column (the PK / unique index — see [`KeySpec::from_detail`]); a header-click
/// sorted browse is `(sort_col, pk)` with the PK as tiebreaker (see
/// [`KeySpec::sorted`]). Arbitrary editor SQL has no key and pages by `OFFSET`.
///
/// The seek is always a single row-value comparison `(c1, …) </> (…)`, so every
/// column in the tuple shares one [`descending`](Self::descending) direction. RED
/// only offers a single-column header sort, so the tiebreaker inherits the lead's
/// direction and a mixed-direction tuple never arises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySpec {
    /// The lead key column's name, as it appears in the result set — the sort
    /// column for a sorted browse, or the PK for a plain browse.
    pub column: String,
    /// The lead column's kind. Drives key-space interpolation (only the lead
    /// matters): an `Int` lead supports fraction jumps, `Other` degrades to
    /// `OFFSET` for far jumps.
    pub kind: KeyKind,
    /// The PK tiebreaker, appended after [`column`](Self::column) when sorting by
    /// a non-PK column so rows sharing a `column` value order deterministically.
    /// `None` for a plain browse (the lead column is itself the unique key).
    pub tiebreak: Option<String>,
    /// The sort direction of the lead column (header click). `false` (ascending)
    /// for a plain browse. The tiebreaker shares this direction.
    pub descending: bool,
}

/// Whether the key is numerically interpolable. `Int` keys support key-space
/// seek (jump to a fraction via `min + f·(max − min)`); `Other` keys still get
/// keyset scroll but fall back to `OFFSET` for far jumps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Int,
    Other,
}

impl KeySpec {
    /// A single-column, ascending key (a plain table browse — also the building
    /// block the conformance battery seeds with).
    pub fn single(column: impl Into<String>, kind: KeyKind) -> KeySpec {
        KeySpec {
            column: column.into(),
            kind,
            tiebreak: None,
            descending: false,
        }
    }

    /// The seek columns in order — the lead column then the tiebreaker, if any.
    /// Drivers quote these for the `ORDER BY` and the row-value comparison.
    pub fn column_names(&self) -> Vec<&str> {
        let mut cols = vec![self.column.as_str()];
        if let Some(t) = &self.tiebreak {
            cols.push(t.as_str());
        }
        cols
    }

    /// Resolve a table's seek key from its introspected detail: a single-column
    /// primary key, or failing that a single-column unique not-null index.
    /// `None` (composite or nullable key, or no key at all) means the result
    /// isn't keyset-eligible and pages by `OFFSET`.
    pub fn from_detail(detail: &TableDetail) -> Option<KeySpec> {
        let column = resolve_key_column(detail)?;
        // SQLite reports `INTEGER PRIMARY KEY` (the rowid alias, never null) as
        // nullable, so an integer key passes without `not_null`; any other
        // nullable key disqualifies keyset (NULLs don't order reliably).
        let kind = key_kind(column);
        (column.not_null || kind == KeyKind::Int)
            .then(|| KeySpec::single(column.name.clone(), kind))
    }

    /// Resolve the composite seek key for a header-click sort by `sort_col`: the
    /// sort column led, with the table's PK appended as a tiebreaker so equal
    /// `sort_col` rows page deterministically. `None` (→ `OFFSET` fallback) when
    /// the table has no usable PK, when `sort_col` isn't a real column of the
    /// table (an expression/alias), or when it's nullable (NULLs don't order
    /// reliably across engines). Sorting by the PK itself collapses to the plain
    /// single-column key, just carrying the direction.
    pub fn sorted(detail: &TableDetail, sort_col: &str, descending: bool) -> Option<KeySpec> {
        let pk = resolve_key_column(detail)?;
        let lead = detail.columns.iter().find(|c| c.name == sort_col)?;
        if lead.name == pk.name {
            return Some(KeySpec {
                column: pk.name.clone(),
                kind: key_kind(pk),
                tiebreak: None,
                descending,
            });
        }
        // A nullable non-PK lead disqualifies keyset — same posture `from_detail`
        // takes for nullable keys.
        let kind = key_kind(lead);
        (lead.not_null || kind == KeyKind::Int).then(|| KeySpec {
            column: lead.name.clone(),
            kind,
            tiebreak: Some(pk.name.clone()),
            descending,
        })
    }
}

/// The PK / unique-not-null-index column a table's keyset key is built from, if
/// any. Shared by [`KeySpec::from_detail`] (the key itself) and
/// [`KeySpec::sorted`] (the tiebreaker).
fn resolve_key_column(detail: &TableDetail) -> Option<&ColumnMeta> {
    let pks: Vec<&ColumnMeta> = detail.columns.iter().filter(|c| c.primary_key).collect();
    match pks.as_slice() {
        [single] => Some(*single),
        [] => detail
            .indexes
            .iter()
            .filter(|i| i.unique && i.columns.len() == 1)
            .find_map(|i| {
                detail
                    .columns
                    .iter()
                    .find(|c| c.name == i.columns[0] && c.not_null)
            }),
        _ => None,
    }
}

/// Whether a column is numerically interpolable (drives key-space fraction jumps).
fn key_kind(column: &ColumnMeta) -> KeyKind {
    if column.type_name.as_deref().is_some_and(is_int_type) {
        KeyKind::Int
    } else {
        KeyKind::Other
    }
}

/// Whether a declared type names an integer column across the three engines
/// (`INTEGER`, `bigint`, `int(11)`, `serial`, …). Deliberately a whitelist of
/// the base token — `interval`/`point` must not match.
fn is_int_type(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    matches!(
        base,
        "int"
            | "integer"
            | "int2"
            | "int4"
            | "int8"
            | "tinyint"
            | "smallint"
            | "mediumint"
            | "bigint"
            | "serial"
            | "smallserial"
            | "bigserial"
    )
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
mod key_tests {
    use super::*;

    fn col(name: &str, type_name: &str, not_null: bool, primary_key: bool) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: Some(type_name.into()),
            not_null,
            primary_key,
            default: None,
        }
    }

    #[test]
    fn single_int_pk_is_the_key() {
        // SQLite-style: INTEGER PRIMARY KEY reports notnull = 0 but still counts.
        let detail = TableDetail {
            columns: vec![
                col("id", "INTEGER", false, true),
                col("name", "TEXT", true, false),
            ],
            ..Default::default()
        };
        let key = KeySpec::from_detail(&detail).unwrap();
        assert_eq!(key.column, "id");
        assert_eq!(key.kind, KeyKind::Int);
    }

    #[test]
    fn text_pk_needs_not_null() {
        let nullable = TableDetail {
            columns: vec![col("id", "TEXT", false, true)],
            ..Default::default()
        };
        assert!(KeySpec::from_detail(&nullable).is_none());

        let not_null = TableDetail {
            columns: vec![col("id", "TEXT", true, true)],
            ..Default::default()
        };
        let key = KeySpec::from_detail(&not_null).unwrap();
        assert_eq!(key.kind, KeyKind::Other);
    }

    #[test]
    fn composite_pk_is_ineligible() {
        let detail = TableDetail {
            columns: vec![
                col("a", "INTEGER", true, true),
                col("b", "INTEGER", true, true),
            ],
            ..Default::default()
        };
        assert!(KeySpec::from_detail(&detail).is_none());
    }

    #[test]
    fn unique_not_null_index_substitutes_for_a_pk() {
        let detail = TableDetail {
            columns: vec![
                col("email", "varchar", true, false),
                col("name", "text", false, false),
            ],
            indexes: vec![IndexMeta {
                name: "uq_email".into(),
                unique: true,
                columns: vec!["email".into()],
            }],
            ..Default::default()
        };
        let key = KeySpec::from_detail(&detail).unwrap();
        assert_eq!(key.column, "email");
        assert_eq!(key.kind, KeyKind::Other);
    }

    #[test]
    fn no_key_at_all_is_ineligible() {
        let detail = TableDetail {
            columns: vec![col("x", "INTEGER", true, false)],
            ..Default::default()
        };
        assert!(KeySpec::from_detail(&detail).is_none());
    }

    #[test]
    fn int_detection_is_a_whitelist() {
        assert!(is_int_type("INTEGER"));
        assert!(is_int_type("bigint"));
        assert!(is_int_type("int(11) unsigned"));
        assert!(is_int_type("serial"));
        assert!(!is_int_type("interval"));
        assert!(!is_int_type("point"));
        assert!(!is_int_type("text"));
    }
}

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
