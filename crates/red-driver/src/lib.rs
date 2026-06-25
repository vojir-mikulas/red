//! The database abstraction layer. `DatabaseDriver` is RED's analogue of Nyx's
//! `RemoteClient`: an object-safe trait the service holds as `Arc<dyn …>` and
//! drives across many commands, with one impl per engine.
//!
//! Nothing here materializes a whole result: queries run behind a windowed
//! [`QueryCursor`], paging is random-access (`fetch_page`) or indexed-seek
//! (`fetch_seek`), and `export` streams row-by-row. This keeps memory flat over
//! results of any size, the layer's central performance contract.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use red_core::{
    Column, ColumnMeta, ColumnValue, EditOp, ExportFormat, FkEdge, KeySpec, QueryOptions,
    QueryPlan, RedError, Result, ResultPage, RowWindow, SchemaMeta, TableDetail, TableRef, Value,
};
use tokio::sync::mpsc::UnboundedSender;

mod clickhouse;
#[cfg(test)]
mod conformance;
mod format;
mod mysql;
mod pg_text;
mod plan;
mod postgres;
mod sqlite;
pub use clickhouse::ClickhouseDriver;
pub use format::html_escape;
pub use mysql::MysqlDriver;
pub use postgres::PostgresDriver;
pub use sqlite::SqliteDriver;

/// Default bytes of a non-key cell's content a *display* fetch keeps; past it,
/// text is truncated to a [`Value::Capped`] prefix and a blob to its length only.
/// The resident-cell budget that keeps the grid's RAM flat over fat `TEXT`/`BLOB`
/// columns — the driver never materializes the over-cap bytes, so a page of huge
/// cells can't spike the channel. The live value is [`display_cell_cap`], driven
/// by `grid.max_cell_chars` via [`set_display_cell_cap`].
pub const DEFAULT_DISPLAY_CELL_CAP: usize = 4096;

/// The live display cap (see [`DEFAULT_DISPLAY_CELL_CAP`]). A process-global so a
/// settings change applies to every subsequent display fetch across all sessions
/// without threading the value through each `fetch_page`/`fetch_seek` call; the
/// service sets it from `grid.max_cell_chars` on launch and on every reload.
static DISPLAY_CELL_CAP: AtomicUsize = AtomicUsize::new(DEFAULT_DISPLAY_CELL_CAP);

/// Set the live display cap (bytes of a non-key cell a display fetch keeps). The
/// caller clamps to a sane range; export fetches ([`PageCap::Full`]) ignore it.
pub fn set_display_cell_cap(bytes: usize) {
    DISPLAY_CELL_CAP.store(bytes.max(1), Ordering::Relaxed);
}

/// The live display cap currently in effect.
pub(crate) fn display_cell_cap() -> usize {
    DISPLAY_CELL_CAP.load(Ordering::Relaxed)
}

/// Whether [`DatabaseDriver::fetch_page`] caps oversized cells. The display path
/// caps non-key cells to [`display_cell_cap`]; the clipboard re-fetch wants the
/// real values, so it asks for `Full`. (The seek paths are always display-capped
/// and learn their exempt key from their own `KeySpec` argument.)
#[derive(Clone)]
pub enum PageCap {
    /// Cap non-key cells; `key` (when the result is keyed) rides through verbatim
    /// so its bytes round-trip as a seek bound.
    Display { key: Option<KeySpec> },
    /// No cap — full-fidelity rows, for the clipboard re-fetch.
    Full,
}

/// The positional, per-extraction form of a display cap: the byte budget plus the
/// result-column indices of the key columns (exempt — each rides back verbatim as
/// a seek bound, so its value must round-trip exactly). A composite sort key has
/// two exempt columns (lead + tiebreaker); a plain browse has one.
/// `None` everywhere a fetch is full-fidelity (export / clipboard re-fetch).
#[derive(Clone, Copy)]
pub(crate) struct CellCap {
    pub(crate) max_bytes: usize,
    pub(crate) key_cols: [Option<usize>; 2],
}

impl CellCap {
    /// Resolve a [`PageCap`] against a result's columns into a positional cap.
    /// `Full` → `None` (nothing capped); `Display` → the key's column indices (the
    /// present ones) are the exempt columns.
    pub(crate) fn resolve(cap: &PageCap, columns: &[Column]) -> Option<CellCap> {
        match cap {
            PageCap::Full => None,
            PageCap::Display { key } => Some(CellCap {
                max_bytes: display_cell_cap(),
                key_cols: key
                    .as_ref()
                    .map(|k| key_positions(k, columns))
                    .unwrap_or([None, None]),
            }),
        }
    }

    /// The display cap a seek/cursor fetch applies: always on, exempting the key
    /// columns (see [`key_positions`]).
    pub(crate) fn display(key_cols: [Option<usize>; 2]) -> Option<CellCap> {
        Some(CellCap {
            max_bytes: display_cell_cap(),
            key_cols,
        })
    }

    /// Whether column `i` is capped under `cap` (`None` cap → nothing capped; a key
    /// column → never capped).
    pub(crate) fn caps(cap: Option<CellCap>, i: usize) -> Option<usize> {
        match cap {
            Some(c) if !c.key_cols.contains(&Some(i)) => Some(c.max_bytes),
            _ => None,
        }
    }
}

/// The result-column indices of `key`'s columns (lead, then tiebreaker), used to
/// exempt them from the display cap. A missing column resolves to `None`.
pub(crate) fn key_positions(key: &KeySpec, columns: &[Column]) -> [Option<usize>; 2] {
    let find = |name: &str| columns.iter().position(|c| c.name == name);
    [find(&key.column), key.tiebreak.as_deref().and_then(find)]
}

/// Build a seek's `WHERE (cols) cmp (ph…)` and `ORDER BY cols dir` clauses, shared
/// across the three drivers (only quoting and placeholder syntax differ). The seek
/// is a single row-value comparison over the leading `bound_len` key columns, so
/// every column shares one direction: the key's [`descending`](KeySpec::descending)
/// sort, XOR'd with `scroll_descending` (the up/down scroll direction).
/// `inclusive` picks `>=`/`<=` (the `fetch_seek_skip` lower bound) over `>`/`<`.
///
/// `quote` quotes one identifier; `placeholder(i)` renders the `i`-th (0-based)
/// bind slot (e.g. `?` or `$1::int8`). `bound_len == 0` yields an empty `WHERE`
/// (a first/last page). The returned `WHERE` clause carries a trailing space so it
/// drops cleanly before `ORDER BY` when present.
pub(crate) fn seek_clauses(
    key: &KeySpec,
    bound_len: usize,
    scroll_descending: bool,
    inclusive: bool,
    quote: impl Fn(&str) -> String,
    placeholder: impl Fn(usize) -> String,
) -> (String, String) {
    let cols: Vec<String> = key.column_names().iter().map(|c| quote(c)).collect();
    let descending = key.descending ^ scroll_descending;
    let (strict, dir) = if descending {
        ("<", "DESC")
    } else {
        (">", "ASC")
    };
    let cmp = if inclusive {
        format!("{strict}=")
    } else {
        strict.to_string()
    };
    let order_by = cols
        .iter()
        .map(|c| format!("{c} {dir}"))
        .collect::<Vec<_>>()
        .join(", ");
    let where_clause = if bound_len == 0 {
        String::new()
    } else {
        let lhs = cols[..bound_len].join(", ");
        let rhs = (0..bound_len)
            .map(&placeholder)
            .collect::<Vec<_>>()
            .join(", ");
        format!("WHERE ({lhs}) {cmp} ({rhs}) ")
    };
    (where_clause, order_by)
}

/// Render an [`EditOp`] into `(sql, ordered bind values)` for one engine — the
/// shared half of every driver's `apply_edit`, so only quoting and placeholder
/// syntax differ. `quote` quotes one identifier; `placeholder(i, cv)` renders the
/// `i`-th (0-based) bind slot for the column=value pair `cv` (e.g. `?` or
/// `$1::int8`, or — given `cv.decl_type` — a cast back to a non-text column type).
/// A [`Value::Null`] is emitted as the literal `NULL` keyword and **not** bound — so
/// the per-engine value binders never see a null (and a typeless null bind, which
/// Postgres can't infer, never arises). Identifiers are quoted, values are bound:
/// no part of an edit is string-interpolated.
pub(crate) fn edit_sql<'a>(
    op: &'a EditOp,
    quote: impl Fn(&str) -> String,
    placeholder: impl Fn(usize, &ColumnValue) -> String,
) -> (String, Vec<&'a Value>) {
    let qualify = |t: &TableRef| match &t.schema {
        Some(s) if !s.is_empty() => format!("{}.{}", quote(s), quote(&t.name)),
        _ => quote(&t.name),
    };
    // Render `cv` as a bound placeholder (pushing its value to `params`) or the
    // literal NULL. The placeholder sees the whole pair so it can cast by column type.
    let slot = |cv: &'a ColumnValue, params: &mut Vec<&'a Value>| -> String {
        if matches!(cv.value, Value::Null) {
            "NULL".to_string()
        } else {
            let s = placeholder(params.len(), cv);
            params.push(&cv.value);
            s
        }
    };

    let mut params: Vec<&Value> = Vec::new();
    let sql = match op {
        EditOp::Update { table, key, set } => {
            let mut assigns = Vec::with_capacity(set.len());
            for cv in set {
                assigns.push(format!("{} = {}", quote(&cv.column), slot(cv, &mut params)));
            }
            let where_clause = format!("{} = {}", quote(&key.column), slot(key, &mut params));
            format!(
                "UPDATE {} SET {} WHERE {}",
                qualify(table),
                assigns.join(", "),
                where_clause
            )
        }
        EditOp::Delete { table, key } => {
            let where_clause = format!("{} = {}", quote(&key.column), slot(key, &mut params));
            format!("DELETE FROM {} WHERE {}", qualify(table), where_clause)
        }
        EditOp::Insert { table, values } => {
            let cols = values
                .iter()
                .map(|cv| quote(&cv.column))
                .collect::<Vec<_>>()
                .join(", ");
            let mut vals = Vec::with_capacity(values.len());
            for cv in values {
                vals.push(slot(cv, &mut params));
            }
            format!(
                "INSERT INTO {} ({}) VALUES ({})",
                qualify(table),
                cols,
                vals.join(", ")
            )
        }
    };
    (sql, params)
}

/// The error for an edit whose row count wasn't the expected one — surfaced to the
/// user in the result pane (not as a silent success). `affected = 0` means the row
/// changed or vanished under us; `> 1` means the key was less unique than believed.
/// Either way the driver rolled the statement back.
pub(crate) fn edit_count_err(op: &EditOp, affected: u64) -> RedError {
    let what = match op {
        EditOp::Update { .. } => "update",
        EditOp::Delete { .. } => "delete",
        EditOp::Insert { .. } => "insert",
    };
    RedError::Query(format!(
        "{what} matched {affected} rows (expected 1) — nothing was changed"
    ))
}

/// Note a failed `ROLLBACK` after an edit/exec error. The original statement
/// error is the primary cause and is still returned by the caller; this only
/// surfaces the rare case where the rollback *itself* failed, which leaves the
/// transaction state ambiguous and is worth a log line. `context` names the call
/// site (e.g. `"execute"`, `"apply_edits"`).
pub(crate) fn warn_rollback<E: std::fmt::Display>(
    result: std::result::Result<(), E>,
    context: &str,
) {
    if let Err(e) = result {
        tracing::warn!(
            context,
            error = %e,
            "rollback failed; transaction state may be inconsistent"
        );
    }
}

/// Build a portable "any text-representable column contains `term`" predicate for
/// the result-search filter ([`red_core::ResultFilter::Contains`]): an OR-chain of
/// `<col-as-text> <like> '<escaped %term%>' ESCAPE '\'` over every non-blob column.
/// Shared across the three drivers — only the identifier quoting, the text cast,
/// and the match keyword differ. Returns `None` when nothing is searchable (all
/// columns blob, or an empty column set); the service then applies no filter.
///
/// Blob columns are skipped: casting binary to text is engine-specific noise
/// (Postgres hex, etc.) and never what a text search means. The `term` is escaped
/// to match **literally** — the `LIKE` metacharacters `\` `%` `_` are backslash-
/// escaped and embedded quotes doubled — so it can never inject SQL or leak a
/// wildcard.
///
/// `quote` quotes one identifier; `as_text(quoted)` wraps a quoted column in the
/// engine's text cast (`(c)::text`, `CAST(c AS TEXT)`, `CAST(c AS CHAR)`); `like_op`
/// is the case-insensitive match keyword (`ILIKE` on Postgres, `LIKE` elsewhere —
/// SQLite/MySQL `LIKE` is ASCII-case-insensitive by default).
/// `backslash_escapes` must be `true` for engines that treat `\` as a string-
/// literal escape (MySQL/MariaDB in the default mode, and ClickHouse), `false`
/// where `\` is a plain literal byte (SQLite, and Postgres with
/// `standard_conforming_strings`). It controls a second escaping layer so the
/// backslashes the `LIKE` pattern uses survive the engine's *string-literal*
/// parser intact — without it, a search for a literal `%`, `_`, or `\` silently
/// misbehaves on MySQL.
///
/// `escape_clause` controls the trailing `ESCAPE '…'`: the SQL-standard engines
/// (Postgres/MySQL/SQLite) accept it and rely on it to name `\` as the pattern's
/// escape char, but ClickHouse's `LIKE`/`ILIKE` has no `ESCAPE` clause (its escape
/// char is always `\`), so it passes `false` to omit it — the `\`-escaped pattern
/// still matches literally against ClickHouse's built-in backslash escaping.
pub(crate) fn contains_clause(
    columns: &[ColumnMeta],
    term: &str,
    quote: impl Fn(&str) -> String,
    as_text: impl Fn(&str) -> String,
    like_op: &str,
    backslash_escapes: bool,
    escape_clause: bool,
) -> Option<String> {
    let pattern = like_pattern(term, backslash_escapes);
    // The escape char inside the literal is one backslash; on a backslash-escaping
    // engine that backslash must itself be doubled in the literal to reach `LIKE`.
    let escape = if !escape_clause {
        String::new()
    } else if backslash_escapes {
        r" ESCAPE '\\'".to_string()
    } else {
        r" ESCAPE '\'".to_string()
    };
    let preds: Vec<String> = columns
        .iter()
        .filter(|c| !c.type_name.as_deref().is_some_and(is_blob_type))
        .map(|c| format!("{} {like_op} {pattern}{escape}", as_text(&quote(&c.name))))
        .collect();
    (!preds.is_empty()).then(|| format!("({})", preds.join(" OR ")))
}

/// Render the AND-chain of `column = value` equalities behind every driver's
/// [`eq_predicate`](DatabaseDriver::eq_predicate) (Track B7 FK follow): only the
/// identifier `quote` differs per engine. Each value becomes a SQL *literal* (see
/// [`sql_literal`]) compared with `=` — no column cast, so the comparison stays
/// index-usable and the engine coerces the untyped literal to the column's type.
/// `pairs` is non-empty by contract (a follow always has at least one column).
pub(crate) fn eq_clause(pairs: &[ColumnValue], quote: impl Fn(&str) -> String) -> String {
    pairs
        .iter()
        .map(|cv| format!("{} = {}", quote(&cv.column), sql_literal(&cv.value)))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// A [`Value`] as a SQL literal for an FK-follow equality (see [`eq_clause`]): an
/// integer/real bare, text single-quoted with embedded quotes doubled. A NULL or a
/// kind that can never be an FK key (blob / a display-capped cell) renders as the
/// literal `NULL`, so `col = NULL` matches nothing — a safe no-op rather than an
/// error, though the UI gates FK follow to non-null int/text values so it shouldn't
/// arise. The text quoting is the injection guard; no value is interpolated raw.
fn sql_literal(v: &Value) -> String {
    match v {
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Null | Value::Blob(_) | Value::Capped(_) => "NULL".to_string(),
    }
}

/// One flat foreign-key *column* row from an `information_schema` scan (Postgres /
/// MySQL), before composite keys are grouped. `constraint` + the `from` endpoint
/// identify the edge a row belongs to; [`group_fk_edges`] folds consecutive rows of
/// one constraint into a single [`FkEdge`].
pub(crate) struct FkRow {
    pub from_schema: Option<String>,
    pub from_table: String,
    pub from_column: String,
    pub to_schema: Option<String>,
    pub to_table: String,
    pub to_column: String,
    pub constraint: String,
}

/// Fold flat catalog FK rows into one [`FkEdge`] per constraint, preserving the
/// column order the query yields. The catalog queries order by
/// `… constraint_name, ordinal_position`, so a constraint's columns arrive
/// consecutively: a change in `(from_schema, from_table, constraint)` starts a new
/// edge, and same-key rows extend the current edge's `columns` (composite key).
pub(crate) fn group_fk_edges(rows: impl Iterator<Item = FkRow>) -> Vec<FkEdge> {
    let mut edges: Vec<FkEdge> = Vec::new();
    let mut current: Option<(String, String, String)> = None;
    for r in rows {
        let key = (
            r.from_schema.clone().unwrap_or_default(),
            r.from_table.clone(),
            r.constraint.clone(),
        );
        if current.as_ref() == Some(&key) {
            if let Some(edge) = edges.last_mut() {
                edge.columns.push((r.from_column, r.to_column));
            }
        } else {
            current = Some(key);
            edges.push(FkEdge {
                from_schema: r.from_schema,
                from_table: r.from_table,
                to_schema: r.to_schema,
                to_table: r.to_table,
                columns: vec![(r.from_column, r.to_column)],
            });
        }
    }
    edges
}

/// The SQL string literal `'%term%'` for a `LIKE` contains-match: backslash-escape
/// the `LIKE` metacharacters (`\` `%` `_`) so the term matches literally, wrap in
/// `%…%`, then single-quote with embedded quotes doubled. When `backslash_escapes`
/// is set, every backslash is *also* doubled for the engine's string-literal layer
/// (see [`contains_clause`]); pair with the matching `ESCAPE` clause there.
fn like_pattern(term: &str, backslash_escapes: bool) -> String {
    let mut esc = String::with_capacity(term.len() + 2);
    for ch in term.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            esc.push('\\');
        }
        esc.push(ch);
    }
    let mut lit = format!("%{esc}%").replace('\'', "''");
    if backslash_escapes {
        lit = lit.replace('\\', "\\\\");
    }
    format!("'{lit}'")
}

/// Whether a declared type names a binary/blob column across the three engines —
/// skipped by the contains-search cast (binary-to-text is engine-specific noise).
fn is_blob_type(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    matches!(
        base,
        "blob" | "bytea" | "binary" | "varbinary" | "tinyblob" | "mediumblob" | "longblob" | "bit"
    )
}

/// One open database session. Object-safe so the service can hold
/// `Arc<dyn DatabaseDriver>` and swap engines behind it.
#[async_trait]
pub trait DatabaseDriver: Send + Sync {
    /// Cheap liveness probe — opens/touches the underlying connection.
    async fn ping(&self) -> Result<()>;

    /// Engine version string (e.g. `"3.46.0"`), for the status bar. Cheap and
    /// synchronous — drivers report a compiled-in or already-known value.
    fn server_version(&self) -> String;

    /// Prepare `sql`, read column metadata, and return a live cursor. Cheap by
    /// design: this does NOT step rows — the first (potentially expensive) step
    /// happens on the first `next_window`, which is the cancellable path.
    async fn open_cursor(&self, sql: &str, opts: QueryOptions) -> Result<Box<dyn QueryCursor>>;

    /// The schema-tree skeleton: every namespace with its table/view names. Cheap
    /// by contract — names + kinds only, no per-table `COUNT(*)` and no column
    /// walk (that's `describe_table`, pulled lazily on expand).
    async fn list_objects(&self) -> Result<Vec<SchemaMeta>>;

    /// One object's columns, foreign keys, and indexes. Loaded on demand when the
    /// user expands a table, so the initial tree load stays light.
    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableDetail>;

    /// The connection-wide foreign-key graph: every declared FK edge across the
    /// visible namespaces (Track B7), for click-through and the relation tree. One
    /// catalog pass where the engine allows (Postgres/MySQL `information_schema`);
    /// SQLite loops `PRAGMA foreign_key_list` over its tables. Read-only and cached
    /// by the caller (loaded once per connection). An engine without relational FKs
    /// (ClickHouse) returns an empty graph, so the feature degrades to absent.
    async fn foreign_keys(&self) -> Result<Vec<FkEdge>>;

    /// Render a conjunction of `column = value` equalities as an escaped *literal*
    /// predicate for [`red_core::ResultFilter::Eq`] — the FK-follow filter. Each
    /// value is escaped to match literally with **no column cast** (unlike
    /// [`contains_predicate`](Self::contains_predicate)'s `(col)::text`, so an index
    /// on the column stays usable, and comparison context coerces the literal to the
    /// column type). Synchronous string building; identifiers are quoted, values are
    /// rendered as literals — never raw UI SQL. NULL values are excluded by the
    /// caller (a null FK isn't followable); `pairs` is non-empty. Each impl delegates
    /// to [`eq_clause`] with its own identifier quoting.
    fn eq_predicate(&self, pairs: &[ColumnValue]) -> String;

    /// Render a portable, case-insensitive "contains `term`" predicate over the
    /// searchable columns of a result, for [`red_core::ResultFilter::Contains`].
    /// The service wraps `SELECT * FROM (base) WHERE <predicate>`; `columns` are the
    /// result's own (a browse passes the table's columns, an editor result its
    /// probed columns). `None` when nothing is searchable (all-blob / empty) — the
    /// service then applies no filter. `term` is escaped to match literally, never
    /// interpolated raw. Synchronous: pure string building, no engine round-trip.
    /// Each impl delegates to [`contains_clause`] with its own quoting/cast/keyword.
    fn contains_predicate(&self, columns: &[ColumnMeta], term: &str) -> Option<String>;

    /// Total row count of `sql`'s result — one pass, no row materialization. Lets
    /// the grid show a real scrollbar without holding every row. `abort` cancels
    /// the (potentially full-table) scan out-of-band when the result is superseded.
    async fn count(&self, sql: &str, abort: &AbortSignal) -> Result<i64>;

    /// A random-access `(offset, limit)` page of `sql`'s result. Backs the grid's
    /// load-on-scroll so memory stays flat: only the pages around the viewport are
    /// ever resident. `cap` chooses display capping (the common scroll path) or
    /// full fidelity (the clipboard re-fetch) — see [`PageCap`]. `abort` cancels a
    /// superseded fetch (a flung scrollbar, a closed tab) at the engine.
    async fn fetch_page(
        &self,
        sql: &str,
        offset: usize,
        limit: usize,
        cap: PageCap,
        abort: &AbortSignal,
    ) -> Result<ResultPage>;

    /// One keyset (seek) page of `sql`'s result, ordered by `key`'s column tuple:
    /// the rows strictly after (`descending = false`) or strictly before
    /// (`descending = true`, returned in reverse order — the caller flips)
    /// `bound`; `None` starts from the result's first/last row. An indexed seek,
    /// so it costs the same at row 200 or 46,000,000 — unlike `fetch_page`'s
    /// O(offset).
    ///
    /// `descending` is the *scroll* direction; it composes (XOR) with the key's
    /// own [`descending`](KeySpec::descending) sort direction. `bound` carries one
    /// value per leading key column — the full tuple for a contiguous seek, or
    /// just the lead value for a key-space interpolation jump (a prefix
    /// comparison). Each value is bound as a real parameter, never interpolated.
    async fn fetch_seek(
        &self,
        sql: &str,
        key: &KeySpec,
        bound: Option<&[Value]>,
        descending: bool,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage>;

    /// One keyset page from an inclusive lower bound with a bounded `skip`:
    /// rows at or after `from` in the key's sort order, skipping `skip` then
    /// taking `limit`. `from = None` starts at the result's first row. The lower
    /// bound is an *indexed* seek, so the `OFFSET skip` only walks within the
    /// post-seek window — O(skip), not O(offset-from-row-0). Backs exact "go to
    /// row N" jumps and the checkpoint-index build, which both seek to a known
    /// key and then step a bounded number of rows. `from` carries one value per
    /// leading key column, each bound as a real parameter.
    async fn fetch_seek_skip(
        &self,
        sql: &str,
        key: &KeySpec,
        from: Option<&[Value]>,
        skip: usize,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage>;

    /// `MIN`/`MAX` of `key` over `sql`'s result — one indexed probe, backing
    /// key-space interpolation for fraction jumps. `None` when the result is
    /// empty or the key isn't an integer (not interpolable). `abort` cancels the
    /// probe out-of-band when the open it belongs to is superseded.
    async fn key_bounds(
        &self,
        sql: &str,
        key: &KeySpec,
        abort: &AbortSignal,
    ) -> Result<Option<(i64, i64)>>;

    /// Run a non-row-returning statement wrapped in a transaction, returning the
    /// number of rows affected. A read-only driver rejects the write at the engine.
    async fn execute(&self, sql: &str) -> Result<u64>;

    /// Apply a batch of guarded, PK-keyed data edits (Track B6) **atomically** in a
    /// single transaction: render each `op` to dialect SQL with every value **bound**
    /// (see [`edit_sql`]), run them in order, and assert each touches exactly one row
    /// — rolling the *whole* batch back and returning [`edit_count_err`] (or the
    /// engine error) the moment any op fails or matches ≠1 row, so a multi-edit
    /// submit is all-or-nothing and a stale/non-unique key can't half-apply. A
    /// read-only driver rejects the writes at the engine (defense in depth behind the
    /// UI's opt-in gate). Returns the total affected count (`== ops.len()` on
    /// success). An empty batch is a no-op returning 0 without opening a transaction.
    async fn apply_edits(&self, ops: &[EditOp]) -> Result<u64>;

    /// Apply one guarded data edit — the single-row common case, delegating to the
    /// transactional [`apply_edits`](DatabaseDriver::apply_edits) batch path.
    async fn apply_edit(&self, op: &EditOp) -> Result<u64> {
        self.apply_edits(std::slice::from_ref(op)).await
    }

    /// Run the engine's `EXPLAIN` for `sql` and return a normalized [`QueryPlan`]
    /// (Track B4). Plain `explain` (`analyze = false`) never executes the
    /// statement — it's read-only-safe for any SQL. `analyze = true` runs
    /// `EXPLAIN ANALYZE`, which *executes* the statement to gather actuals; the
    /// caller gates that to read queries (SQLite has no ANALYZE and ignores the
    /// flag). Each driver reads its native textual/tabular plan and maps it — no
    /// `FORMAT JSON`, so no JSON parser enters the layer.
    async fn explain(&self, sql: &str, analyze: bool) -> Result<QueryPlan>;

    /// Stream `sql`'s result straight to `path` in `format`, row-by-row — never
    /// materializing the whole result. Returns the number of data rows written.
    ///
    /// `cancel` is checked per row: when it flips true the export bails early,
    /// removes the partial file, and returns [`RedError::Interrupted`]. `progress`
    /// receives the running row count, throttled (every N rows / ~50ms) so the
    /// channel isn't flooded — the caller maps it to a progress event.
    async fn export(
        &self,
        sql: &str,
        path: &Path,
        format: ExportFormat,
        cancel: Arc<AtomicBool>,
        progress: UnboundedSender<u64>,
    ) -> Result<u64>;
}

/// A live, windowed result cursor. Object-safe; the service holds it as
/// `Box<dyn QueryCursor>`. `next_window` takes `&self` — all mutable cursor state
/// lives on the driver's blocking thread — so the returned future is
/// `Send + 'static` and can be raced against incoming commands for cancellation.
#[async_trait]
pub trait QueryCursor: Send {
    /// Column metadata, known up front (read at `open_cursor` without stepping).
    fn columns(&self) -> &[Column];

    /// Fetch up to `max` more rows. `RowWindow::exhausted` marks the end of the
    /// result; once `true`, no further `next_window` calls should be made.
    async fn next_window(&self, max: usize) -> Result<RowWindow>;

    /// A clone-able, thread-safe handle that aborts an in-flight fetch
    /// out-of-band (user cancel / timeout).
    fn cancel_token(&self) -> CancelToken;
}

/// Initial capacity to reserve for a `next_window(max)` row buffer.
///
/// `max` is caller-supplied and can be enormous — the cancel-mid-fetch path asks
/// for a billion rows precisely so the fetch is still running when the abort
/// fires. Reserving `max` up front would try to allocate ~24 GB before a single
/// row is read and abort the process. Cap the reservation to one display page's
/// worth; the `Vec` still grows past it for the rare genuinely large window.
pub(crate) fn window_prealloc(max: usize) -> usize {
    max.min(4096)
}

/// Engine-agnostic cancel handle. SQLite wraps `rusqlite`'s `InterruptHandle`;
/// Postgres wraps its out-of-band cancel request. Cloning is cheap and the token
/// is safe to call from any thread.
#[derive(Clone)]
pub struct CancelToken(Arc<dyn Fn() + Send + Sync>);

impl CancelToken {
    pub(crate) fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// Signal the in-flight fetch to abort. Idempotent and non-blocking.
    pub fn cancel(&self) {
        (self.0)()
    }
}

/// A caller-created abort handle for one in-flight one-shot fetch (`count`,
/// `fetch_page`, `fetch_seek`, `fetch_seek_skip`, `key_bounds`). The service makes
/// one per cancellable fetch, keeps a clone, and calls [`abort`](Self::abort) when
/// that fetch is superseded — a flung scrollbar, a re-sort, a closed tab.
///
/// Where [`CancelToken`] is produced *by* the driver (the streaming cursor hands
/// one back), a one-shot `async fn` can't return a handle before it's awaited — so
/// this inverts it: the caller owns the handle and the driver [`arm`](Self::arm)s
/// it with an engine [`CancelToken`] for the fetch's lifetime. The arm is dropped
/// when the fetch returns ([`ArmGuard`]), so a late `abort` after completion — the
/// connection already back in a pool and reused — is a harmless no-op.
///
/// A single signal can be armed by several concurrent fetches (the open probe runs
/// `count` + `fetch_page` + `key_bounds` together under one signal); `abort` fires
/// every armed token.
#[derive(Clone, Default)]
pub struct AbortSignal(Arc<AbortState>);

#[derive(Default)]
struct AbortState {
    aborted: AtomicBool,
    next_id: AtomicU64,
    /// The engine cancels currently armed (one per in-flight fetch sharing this
    /// signal), each tagged with a unique id so its [`ArmGuard`] removes only its own.
    armed: Mutex<Vec<(u64, CancelToken)>>,
}

impl AbortSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Supersede every fetch armed on this signal: fire each armed engine cancel
    /// and latch the aborted state so a fetch that arms *after* this dies at once.
    /// Idempotent and non-blocking.
    pub fn abort(&self) {
        let armed = lock(&self.0.armed);
        self.0.aborted.store(true, Ordering::SeqCst);
        for (_, token) in armed.iter() {
            token.cancel();
        }
    }

    /// Whether [`abort`](Self::abort) has fired. Drivers check this right before the
    /// engine call so a fetch superseded *before* it starts bails immediately —
    /// some engines no-op an out-of-band cancel with nothing yet running.
    pub fn is_aborted(&self) -> bool {
        self.0.aborted.load(Ordering::SeqCst)
    }

    /// Driver side: install `token` as this fetch's engine cancel for the duration
    /// of the returned guard. If the signal already aborted, fire `token` now (the
    /// arm-after-abort race). Cancel/arm are serialized on the same lock so a
    /// concurrent `abort` can't slip between the check and the install.
    pub(crate) fn arm(&self, token: CancelToken) -> ArmGuard {
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let mut armed = lock(&self.0.armed);
        if self.0.aborted.load(Ordering::SeqCst) {
            token.cancel();
        }
        armed.push((id, token));
        ArmGuard {
            state: self.0.clone(),
            id,
        }
    }
}

/// Disarms its fetch's engine cancel on drop (fetch completion), so a later
/// `abort` can't reach a connection that's since been returned to a pool/reused.
pub(crate) struct ArmGuard {
    state: Arc<AbortState>,
    id: u64,
}

impl Drop for ArmGuard {
    fn drop(&mut self) {
        lock(&self.state.armed).retain(|(id, _)| *id != self.id);
    }
}

/// Lock a mutex, tolerating poison — the armed-list critical sections can't panic,
/// but recovering the guard keeps a stray panic elsewhere from wedging cancels.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Map any error carrying a message into a driver error. Small helper so impls
/// stay terse.
pub(crate) fn driver_err(e: impl std::fmt::Display) -> RedError {
    RedError::Driver(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, type_name: Option<&str>) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: type_name.map(Into::into),
            not_null: false,
            primary_key: false,
            default: None,
        }
    }

    #[test]
    fn like_pattern_escapes_metacharacters_and_quotes() {
        // Plain term: wrapped in `%…%`, nothing escaped.
        assert_eq!(like_pattern("foo", false), "'%foo%'");
        // LIKE metacharacters are backslash-escaped so they match literally.
        assert_eq!(like_pattern("50%_x", false), "'%50\\%\\_x%'");
        assert_eq!(like_pattern("a\\b", false), "'%a\\\\b%'");
        // Single quotes are doubled (can't break out of the literal).
        assert_eq!(like_pattern("O'Brien", false), "'%O''Brien%'");
    }

    #[test]
    fn like_pattern_doubles_backslashes_for_mysql_literals() {
        // On a backslash-escaping engine every backslash is doubled again so the
        // engine's string-literal parser hands `LIKE` the same pattern the
        // non-escaping engines see. `%` → `\%` (escape) → `\\%` (literal layer).
        assert_eq!(like_pattern("50%", true), "'%50\\\\%%'");
        // A literal backslash: `\` → `\\` (escape) → `\\\\` (literal layer).
        assert_eq!(like_pattern("a\\b", true), "'%a\\\\\\\\b%'");
        // Quote-doubling still applies and isn't affected by the backslash pass.
        assert_eq!(like_pattern("O'Brien", true), "'%O''Brien%'");
    }

    #[test]
    fn is_blob_type_matches_binary_families() {
        for t in ["BLOB", "bytea", "VARBINARY(16)", "longblob", "bit"] {
            assert!(is_blob_type(t), "{t} is a blob type");
        }
        for t in ["text", "varchar(20)", "integer", "json"] {
            assert!(!is_blob_type(t), "{t} is not a blob type");
        }
    }

    #[test]
    fn contains_clause_skips_blobs_and_ors_text_columns() {
        let columns = [
            col("id", Some("integer")),
            col("name", Some("text")),
            col("data", Some("blob")),
        ];
        let pred = contains_clause(
            &columns,
            "x",
            |c| format!("\"{c}\""),
            |c| format!("CAST({c} AS TEXT)"),
            "LIKE",
            false,
            true,
        )
        .expect("text/int columns are searchable");
        assert_eq!(
            pred,
            "(CAST(\"id\" AS TEXT) LIKE '%x%' ESCAPE '\\' OR \
             CAST(\"name\" AS TEXT) LIKE '%x%' ESCAPE '\\')"
        );
        assert!(!pred.contains("data"), "the blob column is excluded");
    }

    #[test]
    fn contains_clause_is_none_when_nothing_searchable() {
        // All-blob and empty column sets yield no predicate (→ no filter applied).
        let blobs = [col("a", Some("blob")), col("b", Some("bytea"))];
        assert!(
            contains_clause(&blobs, "x", |c| c.into(), |c| c.into(), "LIKE", false, true).is_none()
        );
        assert!(
            contains_clause(&[], "x", |c| c.into(), |c| c.into(), "LIKE", false, true).is_none()
        );
    }

    #[test]
    fn contains_clause_searches_untyped_columns() {
        // A computed/untyped column (`type_name: None`) is searched, not skipped.
        let columns = [col("expr", None)];
        assert!(contains_clause(
            &columns,
            "x",
            |c| c.into(),
            |c| c.into(),
            "LIKE",
            false,
            true
        )
        .is_some());
    }

    #[test]
    fn eq_clause_escapes_values_and_joins_with_and() {
        // Identifiers quoted by the engine's `quote`; values rendered as literals —
        // integer bare, text single-quoted with the embedded quote doubled. The
        // composite case AND-joins the equalities.
        let pairs = vec![
            ColumnValue {
                column: "a".into(),
                value: Value::Integer(7),
                decl_type: None,
            },
            ColumnValue {
                column: "name".into(),
                value: Value::Text("O'Brien".into()),
                decl_type: None,
            },
        ];
        let pred = eq_clause(&pairs, |c| format!("\"{c}\""));
        assert_eq!(pred, r#""a" = 7 AND "name" = 'O''Brien'"#);
    }

    #[test]
    fn sql_literal_maps_non_key_kinds_to_null() {
        // A null / blob / display-capped value can't be an FK key; it renders as the
        // literal NULL so `col = NULL` matches nothing instead of erroring.
        assert_eq!(sql_literal(&Value::Null), "NULL");
        assert_eq!(sql_literal(&Value::Blob(vec![1, 2, 3])), "NULL");
        assert_eq!(sql_literal(&Value::Integer(5)), "5");
        assert_eq!(sql_literal(&Value::Text("x".into())), "'x'");
    }
}
