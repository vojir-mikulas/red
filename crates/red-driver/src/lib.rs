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
    Column, ColumnMeta, ColumnStats, ColumnValue, DbKind, EditOp, ExportFormat, FkEdge, FkJoin,
    KeySpec, QueryOptions, QueryPlan, RedError, Result, ResultPage, RowWindow, SchemaMeta,
    TableDetail, TableRef, Value, BASE_ALIAS,
};
use tokio::sync::mpsc::UnboundedSender;

mod clickhouse;
#[cfg(test)]
mod conformance;
mod format;
mod import;
mod kv;
mod mysql;
mod pg_text;
mod plan;
mod postgres;
mod redis_kv;
mod sqlite;
pub use clickhouse::ClickhouseDriver;
pub use format::html_escape;
pub use import::ImportReader;
pub use kv::{KvDriver, KvTopology};
pub use mysql::MysqlDriver;
pub use postgres::PostgresDriver;
pub use redis_kv::{sentinel_masters, RedisDriver, SentinelMaster};
pub use sqlite::SqliteDriver;

/// Default bytes of a non-key cell's content a *display* fetch keeps; past it,
/// text is truncated to a [`Value::Capped`] prefix and a blob to its length only.
/// The resident-cell budget that keeps the grid's RAM flat over fat `TEXT`/`BLOB`
/// columns: the driver never materializes the over-cap bytes, so a page of huge
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
    /// No cap: full-fidelity rows, for the clipboard re-fetch.
    Full,
}

/// The positional, per-extraction form of a display cap: the byte budget plus the
/// result-column indices of the key columns (exempt; each rides back verbatim as
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

/// Render an [`EditOp`] into `(sql, ordered bind values)` for one engine: the
/// shared half of every driver's `apply_edit`, so only quoting and placeholder
/// syntax differ. `quote` quotes one identifier; `placeholder(i, cv)` renders the
/// `i`-th (0-based) bind slot for the column=value pair `cv` (e.g. `?` or
/// `$1::int8`, or, given `cv.decl_type`, a cast back to a non-text column type).
/// A [`Value::Null`] is emitted as the literal `NULL` keyword and **not** bound, so
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

/// Render a multi-row `INSERT` into `(sql, ordered bind values)` for one engine:
/// the bulk sibling of [`edit_sql`], so only quoting and placeholder syntax differ.
/// `quote` quotes one identifier; `placeholder(i, value, decl_type)` renders the
/// `i`-th (0-based) **bind slot** for a non-null cell (e.g. `?`, or `$1::int8` /
/// `$1::text::"uuid"`, a cast to the target column's type). A [`Value::Null`] is
/// emitted as the literal `NULL` keyword and **not** bound, so the per-engine value
/// binders never see a null and the bind index only advances for bound cells. Every
/// row binds `columns` in order; `columns` carries each target column's name and
/// best-effort `decl_type`. Identifiers are quoted, values are bound; no part of an
/// insert is string-interpolated.
pub(crate) fn insert_sql<'a>(
    table: &TableRef,
    columns: &[Column],
    rows: &'a [Vec<Value>],
    quote: impl Fn(&str) -> String,
    placeholder: impl Fn(usize, &Value, Option<&str>) -> String,
) -> (String, Vec<&'a Value>) {
    let qualify = match &table.schema {
        Some(s) if !s.is_empty() => format!("{}.{}", quote(s), quote(&table.name)),
        _ => quote(&table.name),
    };
    let col_list = columns
        .iter()
        .map(|c| quote(&c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut params: Vec<&Value> = Vec::new();
    let mut tuples = Vec::with_capacity(rows.len());
    for row in rows {
        debug_assert_eq!(row.len(), columns.len(), "row width must match columns");
        let mut cells = Vec::with_capacity(columns.len());
        for (cell, col) in row.iter().zip(columns) {
            if matches!(cell, Value::Null) {
                cells.push("NULL".to_string());
            } else {
                cells.push(placeholder(params.len(), cell, col.decl_type.as_deref()));
                params.push(cell);
            }
        }
        tuples.push(format!("({})", cells.join(", ")));
    }
    let sql = format!(
        "INSERT INTO {qualify} ({col_list}) VALUES {}",
        tuples.join(", ")
    );
    (sql, params)
}

/// The largest number of rows to bind in one `INSERT` statement so the bound-
/// parameter count (`rows × columns`) stays under an engine's placeholder cap
/// (`param_cap`). At least 1: a single row wider than the cap is left for the
/// engine to reject rather than looping forever. Callers feed `rows.chunks(_)` so a
/// big insert splits into several statements inside one transaction.
pub(crate) fn insert_chunk_rows(columns: usize, param_cap: usize) -> usize {
    (param_cap / columns.max(1)).max(1)
}

/// Qualify and quote a table reference (`schema.name`, or just `name` when there's no
/// schema) using `quote`: the shared body of every driver's [`quote_table`] and the
/// `CREATE TABLE` builder. Identifiers are quoted, never interpolated raw.
pub(crate) fn qualify_table(table: &TableRef, quote: impl Fn(&str) -> String) -> String {
    match &table.schema {
        Some(s) if !s.is_empty() => format!("{}.{}", quote(s), quote(&table.name)),
        _ => quote(&table.name),
    }
}

/// Build a `CREATE TABLE IF NOT EXISTS` for `table` from `columns`, spelling each
/// column's declared type into `kind`'s dialect via [`red_core::typemap`], the
/// shared body of every driver's [`create_table`](DatabaseDriver::create_table). A
/// Postgres `int4`/`numeric(10,2)`/`jsonb`/`uuid` becomes a faithful column in the
/// target engine instead of invalid DDL; a type the lattice can't classify falls
/// through verbatim (the engine accepts or rejects it, like dbgate). `NOT NULL` is
/// emitted, primary-key columns are gathered into a trailing `PRIMARY KEY (…)`, and an
/// auto-increment column is re-spelled per dialect (SQLite `INTEGER PRIMARY KEY`,
/// Postgres `serial`/`bigserial`, MySQL `… AUTO_INCREMENT`) so the migrated table keeps
/// auto-numbering. Indexes and foreign keys are **not** emitted here; they ride a
/// deferred pass after the data loads (`docs/plans/database-migration.md` Phase 3).
/// Identifiers are quoted by `quote`; the only interpolated type text comes from the
/// fixed per-engine spelling table, never raw user input.
pub(crate) fn create_table_sql(
    table: &TableRef,
    columns: &[ColumnMeta],
    kind: DbKind,
    quote: impl Fn(&str) -> String,
) -> String {
    use red_core::typemap::{normalize, spell, NormType};
    let qualify = qualify_table(table, &quote);
    let pk_count = columns.iter().filter(|c| c.primary_key).count();
    // SQLite expresses a sole-INTEGER-PK auto-increment column *inline* as
    // `INTEGER PRIMARY KEY` (the rowid alias), which then must NOT also appear in a
    // trailing PRIMARY KEY clause.
    let sqlite_inline_pk = kind == DbKind::Sqlite
        && pk_count == 1
        && columns.iter().any(|c| c.primary_key && c.auto_increment);
    let mut defs: Vec<String> = columns
        .iter()
        .map(|c| {
            let nt = normalize(c.type_name.as_deref().unwrap_or(""));
            if c.auto_increment {
                match kind {
                    DbKind::Sqlite if sqlite_inline_pk && c.primary_key => {
                        format!("{} INTEGER PRIMARY KEY", quote(&c.name))
                    }
                    // A non-sole-PK auto-inc in SQLite can't be the rowid alias; emit a
                    // plain INTEGER (the values still carry across; future auto-numbering
                    // is the only loss).
                    DbKind::Sqlite => format!("{} INTEGER", quote(&c.name)),
                    DbKind::Postgres => {
                        let serial = if matches!(nt, NormType::BigInt) {
                            "bigserial"
                        } else {
                            "serial"
                        };
                        format!("{} {serial}", quote(&c.name))
                    }
                    DbKind::Mysql => {
                        format!("{} {} AUTO_INCREMENT", quote(&c.name), spell(kind, &nt))
                    }
                    DbKind::Clickhouse => format!("{} {}", quote(&c.name), spell(kind, &nt)),
                    // No column/DDL model, no `DatabaseDriver` impl, so this
                    // never sees `DbKind::Redis` (see `typemap::spell`).
                    DbKind::Redis => unreachable!("Redis has no column/DDL model"),
                }
            } else {
                let ty = spell(kind, &nt);
                let null = if c.not_null { " NOT NULL" } else { "" };
                format!("{} {ty}{null}", quote(&c.name))
            }
        })
        .collect();
    if pk_count > 0 && !sqlite_inline_pk {
        let pk = columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| quote(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        defs.push(format!("PRIMARY KEY ({pk})"));
    }
    format!("CREATE TABLE IF NOT EXISTS {qualify} ({})", defs.join(", "))
}

/// Build a `CREATE [UNIQUE] INDEX` for `table` over `columns` in `kind`'s dialect: the
/// shared body of every driver's [`create_index`](DatabaseDriver::create_index), run as
/// the migration's deferred index pass. `IF NOT EXISTS` is used where the engine
/// supports it (not MySQL). Identifiers are quoted by `quote`, never interpolated raw.
pub(crate) fn create_index_sql(
    table: &TableRef,
    name: &str,
    unique: bool,
    columns: &[String],
    kind: DbKind,
    quote: impl Fn(&str) -> String,
) -> String {
    let uniq = if unique { "UNIQUE " } else { "" };
    // MySQL has no `IF NOT EXISTS` for `CREATE INDEX`; SQLite and Postgres do.
    let guard = if matches!(kind, DbKind::Mysql) {
        ""
    } else {
        "IF NOT EXISTS "
    };
    let cols = columns
        .iter()
        .map(|c| quote(c))
        .collect::<Vec<_>>()
        .join(", ");
    match kind {
        // SQLite puts the schema on the *index name*; the table name in `CREATE INDEX`
        // is never schema-qualified (`CREATE INDEX main.ix ON child(...)`).
        DbKind::Sqlite => {
            let idx = match &table.schema {
                Some(s) if !s.is_empty() => format!("{}.{}", quote(s), quote(name)),
                _ => quote(name),
            };
            format!(
                "CREATE {uniq}INDEX {guard}{idx} ON {} ({cols})",
                quote(&table.name)
            )
        }
        // Postgres/MySQL: bare index name, schema-qualified table.
        _ => format!(
            "CREATE {uniq}INDEX {guard}{} ON {} ({cols})",
            quote(name),
            qualify_table(table, &quote)
        ),
    }
}

/// Build an `ALTER TABLE … ADD FOREIGN KEY (…) REFERENCES … (…)` in `kind`'s dialect,
/// the shared body of every (FK-capable) driver's [`add_foreign_key`]. No referential
/// actions are emitted (`FkEdge` doesn't carry them). Identifiers are quoted, never
/// interpolated raw. `kind` is currently unused (the syntax is the same on Postgres and
/// MySQL) but kept for symmetry with the other builders and future per-dialect needs.
pub(crate) fn add_fk_sql(
    child: &TableRef,
    columns: &[String],
    parent: &TableRef,
    ref_columns: &[String],
    quote: impl Fn(&str) -> String,
) -> String {
    let cols = columns
        .iter()
        .map(|c| quote(c))
        .collect::<Vec<_>>()
        .join(", ");
    let refs = ref_columns
        .iter()
        .map(|c| quote(c))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "ALTER TABLE {} ADD FOREIGN KEY ({cols}) REFERENCES {} ({refs})",
        qualify_table(child, &quote),
        qualify_table(parent, &quote)
    )
}

/// The error for an edit whose row count wasn't the expected one, surfaced to the
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
        "{what} matched {affected} rows (expected 1); nothing was changed"
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
/// Shared across the three drivers; only the identifier quoting, the text cast,
/// and the match keyword differ. Returns `None` when nothing is searchable (all
/// columns blob, or an empty column set); the service then applies no filter.
///
/// Blob columns are skipped: casting binary to text is engine-specific noise
/// (Postgres hex, etc.) and never what a text search means. The `term` is escaped
/// to match **literally** (the `LIKE` metacharacters `\` `%` `_` are backslash-
/// escaped and embedded quotes doubled), so it can never inject SQL or leak a
/// wildcard.
///
/// `quote` quotes one identifier; `as_text(quoted)` wraps a quoted column in the
/// engine's text cast (`(c)::text`, `CAST(c AS TEXT)`, `CAST(c AS CHAR)`); `like_op`
/// is the case-insensitive match keyword (`ILIKE` on Postgres, `LIKE` elsewhere;
/// SQLite/MySQL `LIKE` is ASCII-case-insensitive by default).
/// `backslash_escapes` must be `true` for engines that treat `\` as a string-
/// literal escape (MySQL/MariaDB in the default mode, and ClickHouse), `false`
/// where `\` is a plain literal byte (SQLite, and Postgres with
/// `standard_conforming_strings`). It controls a second escaping layer so the
/// backslashes the `LIKE` pattern uses survive the engine's *string-literal*
/// parser intact; without it, a search for a literal `%`, `_`, or `\` silently
/// misbehaves on MySQL.
///
/// `escape_clause` controls the trailing `ESCAPE '…'`: the SQL-standard engines
/// (Postgres/MySQL/SQLite) accept it and rely on it to name `\` as the pattern's
/// escape char, but ClickHouse's `LIKE`/`ILIKE` has no `ESCAPE` clause (its escape
/// char is always `\`), so it passes `false` to omit it; the `\`-escaped pattern
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
/// [`sql_literal`]) compared with `=`: no column cast, so the comparison stays
/// index-usable and the engine coerces the untyped literal to the column's type.
/// `pairs` is non-empty by contract (a follow always has at least one column).
///
/// `backslash_escapes` must match the engine's string-literal rules: MySQL (default
/// mode) and ClickHouse treat `\` as an escape inside `'…'`, so a value's backslashes
/// must be doubled too, not just its quotes (see [`sql_literal`]). Postgres (with the
/// default `standard_conforming_strings`) and SQLite treat `\` as a literal byte, so
/// they pass `false`. Getting this wrong is a SQL-injection hole: an unescaped
/// trailing `\` lets a value break out of the literal.
pub(crate) fn eq_clause(
    pairs: &[ColumnValue],
    quote: impl Fn(&str) -> String,
    backslash_escapes: bool,
) -> String {
    pairs
        .iter()
        .map(|cv| {
            format!(
                "{} = {}",
                quote(&cv.column),
                sql_literal(&cv.value, backslash_escapes)
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Wrap `base` so each [`FkJoin`]'s selected columns ride **inline, next to the
/// foreign-key column they expand from**: the shared half of every relational
/// driver's [`fk_join_wrap`](DatabaseDriver::fk_join_wrap) (inline FK expansion,
/// Track B7). Only the identifier `quote` differs per engine.
///
/// `base_cols` is the base result's columns in their natural order (from
/// `describe_table`). The projection emits each base column in turn and, right after
/// a foreign-key column, the chosen columns of the table(s) it references, in
/// depth-first order, so a nested chain (`tier → cascade → placement`) lands grouped
/// under its anchor (dbgate's column layout). Each join contributes a
/// `LEFT JOIN <ref> AS <alias>` plus its columns as `<alias>.<col> AS "<dotted path>"`;
/// the dotted output name's first segment is the base FK column it anchors to, and the
/// full path orders siblings (a parent FK sorts before its descendants).
///
/// A `LEFT JOIN` (never inner) keeps rows whose FK is NULL/orphaned, and the
/// unique-target gate (caller-enforced; the UI only offers unique-key FKs) keeps the
/// row count identical, so `count`, the keyset key, and paging over the wrapped result
/// are all unaffected. Downstream code addresses columns by *name*, so reordering them
/// is transparent to keyset/FK-accent/edit/stats.
///
/// `joins` is ordered outer→inner: each hop's [`parent_alias`](FkJoin::parent_alias)
/// is an earlier join's alias or [`BASE_ALIAS`]. Empty `joins` returns `base`
/// untouched; an empty `base_cols` (the base order is unknown) falls back to
/// `_red_base.*` then the joined columns appended (correct, just not interleaved). A
/// trailing `;`/whitespace is stripped so the subquery wrap stays well-formed.
pub(crate) fn join_wrap(
    base: &str,
    base_cols: &[String],
    joins: &[FkJoin],
    quote: impl Fn(&str) -> String,
) -> String {
    if joins.is_empty() {
        return base.to_string();
    }
    let qualify = |schema: &Option<String>, table: &str| match schema {
        Some(s) if !s.is_empty() => format!("{}.{}", quote(s), quote(table)),
        _ => quote(table),
    };
    // The `LEFT JOIN` chain (declaration order = outer→inner).
    let mut from = String::new();
    for j in joins {
        let on =
            j.on.iter()
                .map(|(p, c)| format!("{}.{} = {}.{}", j.parent_alias, quote(p), j.alias, quote(c)))
                .collect::<Vec<_>>()
                .join(" AND ");
        from.push_str(&format!(
            " LEFT JOIN {} AS {} ON {on}",
            qualify(&j.to_schema, &j.to_table),
            j.alias
        ));
    }
    // Every selected joined column, tagged with its dotted path (anchor = first
    // segment, the full path orders siblings depth-first) and its source alias.
    let mut joined: Vec<(Vec<&str>, &str, &str, &str)> = Vec::new();
    for j in joins {
        for (leaf, out) in &j.select {
            joined.push((out.split('.').collect(), j.alias.as_str(), leaf, out));
        }
    }
    let render =
        |g: &(Vec<&str>, &str, &str, &str)| format!("{}.{} AS {}", g.1, quote(g.2), quote(g.3));

    let mut selects: Vec<String> = Vec::new();
    if base_cols.is_empty() {
        // Base order unknown: keep base columns contiguous via `*`, joined appended.
        selects.push(format!("{BASE_ALIAS}.*"));
        joined.sort_by(|a, b| a.0.cmp(&b.0));
        selects.extend(joined.iter().map(&render));
    } else {
        for bc in base_cols {
            selects.push(format!("{BASE_ALIAS}.{}", quote(bc)));
            // The joined columns anchored at this FK column, depth-first.
            let mut group: Vec<_> = joined
                .iter()
                .filter(|g| g.0.first() == Some(&bc.as_str()))
                .collect();
            group.sort_by(|a, b| a.0.cmp(&b.0));
            selects.extend(group.into_iter().map(&render));
        }
        // Safety net: a joined column whose anchor isn't a base column (shouldn't
        // arise) is appended rather than silently dropped.
        let anchored: std::collections::HashSet<&str> =
            base_cols.iter().map(String::as_str).collect();
        let mut orphans: Vec<_> = joined
            .iter()
            .filter(|g| !g.0.first().is_some_and(|a| anchored.contains(a)))
            .collect();
        orphans.sort_by(|a, b| a.0.cmp(&b.0));
        selects.extend(orphans.into_iter().map(&render));
    }
    format!(
        "SELECT {} FROM ({}) AS {BASE_ALIAS}{from}",
        selects.join(", "),
        format::strip_trailing(base)
    )
}

/// A [`Value`] as a SQL literal for an FK-follow equality (see [`eq_clause`]): an
/// integer/real bare, text single-quoted with embedded quotes doubled. A NULL or a
/// kind that can never be an FK key (blob / a display-capped cell) renders as the
/// literal `NULL`, so `col = NULL` matches nothing: a safe no-op rather than an
/// error, though the UI gates FK follow to non-null int/text values so it shouldn't
/// arise. The text quoting is the injection guard; no value is interpolated raw.
///
/// On an engine where `\` escapes inside a string literal (`backslash_escapes`:
/// MySQL/ClickHouse), embedded backslashes are *also* doubled, exactly as
/// [`like_pattern`] does; without this a value such as `\' OR 1=1 -- ` escapes the
/// closing quote and injects SQL (the FK-follow value comes from a result cell, which
/// on a hostile/shared database is attacker-controlled). Postgres/SQLite treat `\` as
/// a literal byte, so they pass `false` and only the quote is doubled.
fn sql_literal(v: &Value, backslash_escapes: bool) -> String {
    match v {
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => {
            let mut lit = s.replace('\'', "''");
            if backslash_escapes {
                lit = lit.replace('\\', "\\\\");
            }
            format!("'{lit}'")
        }
        Value::Null | Value::Blob(_) | Value::Capped(_) => "NULL".to_string(),
    }
}

/// Build the aggregate-summary `SELECT` for [`DatabaseDriver::column_stats`]: a
/// pushdown that wraps the result's SQL in a subquery and aggregates `column`.
/// Shared across the engines; only the identifier `quote` differs. The select
/// list is assembled in a **fixed order** so [`parse_stats`] reads it positionally:
/// `count(*)`, `count(col)`, then `count(distinct col)` (iff `distinct`), then
/// `min(col)`, `max(col)`, then `sum(col)`, `avg(col)` (iff `numeric`). The column
/// identifier is quoted; the wrapped `sql` is the already-resolved (filtered) base,
/// passed through [`strip_trailing`](format::strip_trailing) so a trailing `;`/
/// whitespace doesn't break the subquery wrap.
pub(crate) fn stats_sql(
    sql: &str,
    column: &str,
    numeric: bool,
    distinct: bool,
    quote: impl Fn(&str) -> String,
) -> String {
    let col = quote(column);
    let mut items = vec!["count(*)".to_string(), format!("count({col})")];
    if distinct {
        items.push(format!("count(distinct {col})"));
    }
    items.push(format!("min({col})"));
    items.push(format!("max({col})"));
    if numeric {
        items.push(format!("sum({col})"));
        items.push(format!("avg({col})"));
    }
    format!(
        "SELECT {} FROM ({}) AS _red",
        items.join(", "),
        format::strip_trailing(sql)
    )
}

/// Map the one aggregate row [`stats_sql`] produced into a [`ColumnStats`], reading
/// the columns back in the order they were emitted. The counts are read as integers
/// (a NULL/absent cell → 0; a count returned as a numeric string is parsed);
/// `min`/`max`/`sum`/`avg` ride through as typed [`Value`]s so the UI formats them
/// like grid cells. A NULL `sum`/`avg` (every row null) collapses to `None` so the
/// bar omits it. `nulls` isn't stored; the UI derives `total - non_null`.
pub(crate) fn parse_stats(cells: &[Value], numeric: bool, distinct: bool) -> ColumnStats {
    let int_at = |i: usize| match cells.get(i) {
        Some(Value::Integer(n)) => *n,
        Some(Value::Real(x)) => *x as i64,
        Some(Value::Text(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    };
    let mut i = 0;
    let total = int_at(i);
    i += 1;
    let non_null = int_at(i);
    i += 1;
    let distinct = distinct.then(|| {
        let d = int_at(i);
        i += 1;
        d
    });
    let min = cells.get(i).cloned().unwrap_or(Value::Null);
    i += 1;
    let max = cells.get(i).cloned().unwrap_or(Value::Null);
    i += 1;
    // A NULL aggregate (empty/all-null column) → None, so the bar shows no sum/avg.
    let non_null_val = |v: Option<Value>| v.filter(|v| !matches!(v, Value::Null));
    let (sum, avg) = if numeric {
        (
            non_null_val(cells.get(i).cloned()),
            non_null_val(cells.get(i + 1).cloned()),
        )
    } else {
        (None, None)
    };
    ColumnStats {
        total,
        non_null,
        distinct,
        min,
        max,
        sum,
        avg,
    }
}

/// Build the `SELECT DISTINCT` for [`DatabaseDriver::fetch_lookup`]: a bounded list
/// of a referenced table's ids (and an optional label column) for the in-cell FK
/// picker. `table`/`id`/`label` are already quoted by the caller (engine identifier
/// quoting), so this only assembles the projection, orders by the id for a stable
/// list, and caps at `limit`. The label is dropped when it equals the id column, so
/// a labelless target selects just the id. No user *values* enter the SQL (only
/// quoted identifiers), so there is no injection surface — the picker filters the
/// fetched page client-side.
/// Parse a MySQL `enum('a','b','c')` (or `set(...)`) type string into its variant
/// list. MySQL stores the allowed values only in `information_schema.columns.
/// COLUMN_TYPE`; `DATA_TYPE` is just `enum`. Single quotes inside a variant are
/// doubled (`''`), matching how MySQL renders the definition. Returns an empty vec if
/// the string isn't a recognizable `enum(...)`/`set(...)` spec.
pub(crate) fn parse_mysql_enum(column_type: &str) -> Vec<String> {
    let t = column_type.trim();
    let inner = t
        .strip_prefix("enum(")
        .or_else(|| t.strip_prefix("set("))
        .or_else(|| t.strip_prefix("ENUM("))
        .or_else(|| t.strip_prefix("SET("))
        .and_then(|s| s.strip_suffix(')'));
    let Some(inner) = inner else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut chars = inner.chars().peekable();
    while chars.peek().is_some() {
        // Skip to the opening quote of the next variant.
        while matches!(chars.peek(), Some(c) if *c != '\'') {
            chars.next();
        }
        if chars.next().is_none() {
            break; // no opening quote left
        }
        let mut val = String::new();
        loop {
            match chars.next() {
                // A doubled '' is an escaped single quote inside the value.
                Some('\'') if chars.peek() == Some(&'\'') => {
                    chars.next();
                    val.push('\'');
                }
                Some('\'') | None => break, // closing quote (or malformed end)
                Some(c) => val.push(c),
            }
        }
        out.push(val);
    }
    out
}

pub(crate) fn lookup_sql(table: &str, id: &str, label: Option<&str>, limit: usize) -> String {
    let proj = match label {
        Some(l) if l != id => format!("{id}, {l}"),
        _ => id.to_string(),
    };
    format!("SELECT DISTINCT {proj} FROM {table} ORDER BY {id} LIMIT {limit}")
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

/// Whether a declared type names a binary/blob column across the three engines,
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
    /// Cheap liveness probe: opens/touches the underlying connection.
    async fn ping(&self) -> Result<()>;

    /// Engine version string (e.g. `"3.46.0"`), for the status bar. Cheap and
    /// synchronous; drivers report a compiled-in or already-known value.
    fn server_version(&self) -> String;

    /// Prepare `sql`, read column metadata, and return a live cursor. Cheap by
    /// design: this does NOT step rows; the first (potentially expensive) step
    /// happens on the first `next_window`, which is the cancellable path.
    async fn open_cursor(&self, sql: &str, opts: QueryOptions) -> Result<Box<dyn QueryCursor>>;

    /// The schema-tree skeleton: every namespace with its table/view names. Cheap
    /// by contract: names + kinds only, no per-table `COUNT(*)` and no column
    /// walk (that's `describe_table`, pulled lazily on expand).
    async fn list_objects(&self) -> Result<Vec<SchemaMeta>>;

    /// One object's columns, foreign keys, and indexes. Loaded on demand when the
    /// user expands a table, so the initial tree load stays light.
    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableDetail>;

    /// The enum-typed columns of `table` and their allowed values (Track B8: the
    /// in-cell enum picker), `{ column → [variant, …] }`. Used to offer a value
    /// dropdown when editing an enum cell. The default returns nothing (engines
    /// without an enum concept, or where it isn't editable); Postgres (`pg_enum`) and
    /// MySQL (`information_schema.columns.COLUMN_TYPE`) override it. Loaded lazily and
    /// cached by the caller, like [`foreign_keys`](Self::foreign_keys).
    async fn enum_columns(
        &self,
        table: &TableRef,
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        let _ = table;
        Ok(std::collections::HashMap::new())
    }

    /// The connection-wide foreign-key graph: every declared FK edge across the
    /// visible namespaces (Track B7), for click-through and the relation tree. One
    /// catalog pass where the engine allows (Postgres/MySQL `information_schema`);
    /// SQLite loops `PRAGMA foreign_key_list` over its tables. Read-only and cached
    /// by the caller (loaded once per connection). An engine without relational FKs
    /// (ClickHouse) returns an empty graph, so the feature degrades to absent.
    async fn foreign_keys(&self) -> Result<Vec<FkEdge>>;

    /// Render a conjunction of `column = value` equalities as an escaped *literal*
    /// predicate for [`red_core::ResultFilter::Eq`], the FK-follow filter. Each
    /// value is escaped to match literally with **no column cast** (unlike
    /// [`contains_predicate`](Self::contains_predicate)'s `(col)::text`, so an index
    /// on the column stays usable, and comparison context coerces the literal to the
    /// column type). Synchronous string building; identifiers are quoted, values are
    /// rendered as literals, never raw UI SQL. NULL values are excluded by the
    /// caller (a null FK isn't followable); `pairs` is non-empty. Each impl delegates
    /// to [`eq_clause`] with its own identifier quoting.
    fn eq_predicate(&self, pairs: &[ColumnValue]) -> String;

    /// Wrap `base` so each inline FK expansion (Track B7) `LEFT JOIN`s its referenced
    /// table and selects the chosen columns *inline, next to the FK column they expand
    /// from*, the inline sibling of [`eq_predicate`](Self::eq_predicate). `base_cols`
    /// is the base result's columns in order (so the projection can interleave). Returns
    /// `base` unchanged for an empty `joins` or an engine without relational FKs (the
    /// default impl, which ClickHouse keeps); the relational engines override it to
    /// delegate to [`join_wrap`] with their own identifier quoting. Synchronous string
    /// building; identifiers are quoted, never raw UI SQL. The unique-target gate is the
    /// caller's (the UI only offers unique-key FKs), so the join can't change the row
    /// count; `count`, the keyset key, and paging stay correct over the wrapped result.
    fn fk_join_wrap(&self, base: &str, base_cols: &[String], joins: &[FkJoin]) -> String {
        let _ = base_cols;
        debug_assert!(
            joins.is_empty(),
            "fk_join_wrap not supported by this engine"
        );
        base.to_string()
    }

    /// Render a portable, case-insensitive "contains `term`" predicate over the
    /// searchable columns of a result, for [`red_core::ResultFilter::Contains`].
    /// The service wraps `SELECT * FROM (base) WHERE <predicate>`; `columns` are the
    /// result's own (a browse passes the table's columns, an editor result its
    /// probed columns). `None` when nothing is searchable (all-blob / empty); the
    /// service then applies no filter. `term` is escaped to match literally, never
    /// interpolated raw. Synchronous: pure string building, no engine round-trip.
    /// Each impl delegates to [`contains_clause`] with its own quoting/cast/keyword.
    fn contains_predicate(&self, columns: &[ColumnMeta], term: &str) -> Option<String>;

    /// Total row count of `sql`'s result: one pass, no row materialization. Lets
    /// the grid show a real scrollbar without holding every row. `abort` cancels
    /// the (potentially full-table) scan out-of-band when the result is superseded.
    async fn count(&self, sql: &str, abort: &AbortSignal) -> Result<i64>;

    /// An aggregate summary of `column` over `sql` (the result's already-filtered
    /// SQL), pushed to the engine: builds `SELECT count(*), count(col),
    /// [count(distinct col)], min(col), max(col), [sum(col), avg(col)] FROM (sql)
    /// AS _red` and reads one row (see [`stats_sql`]/[`parse_stats`]), never
    /// scanning the materialized window. `numeric` toggles the `sum`/`avg`
    /// aggregates (decided UI-side from the column's declared type), `distinct`
    /// toggles the potentially-expensive `count(distinct col)`. Cancellable via
    /// `abort`, exactly like [`count`](Self::count); identifier quoting is the
    /// engine's own helper.
    async fn column_stats(
        &self,
        sql: &str,
        column: &str,
        numeric: bool,
        distinct: bool,
        abort: &AbortSignal,
    ) -> Result<ColumnStats>;

    /// A bounded list of a referenced table's existing ids (and an optional label
    /// column) for the in-cell foreign-key picker (Track B8): `SELECT DISTINCT
    /// <id>[, <label>] FROM <target> ORDER BY <id> LIMIT <limit>` (see [`lookup_sql`]),
    /// read back through the tested [`fetch_page`](Self::fetch_page) path so no engine
    /// needs its own body. `target`/`id_column`/`label_column` are quoted with the
    /// engine's [`quote_table`](Self::quote_table)/[`quote_ident`](Self::quote_ident);
    /// only identifiers enter the SQL, never user values, so the picker's search runs
    /// client-side over this page with no injection surface. Cancellable via `abort`.
    async fn fetch_lookup(
        &self,
        target: &TableRef,
        id_column: &str,
        label_column: Option<&str>,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<Vec<red_core::LookupRow>> {
        let table = self.quote_table(target);
        let id = self.quote_ident(id_column);
        let label = label_column.map(|l| self.quote_ident(l));
        let sql = lookup_sql(&table, &id, label.as_deref(), limit);
        let page = self
            .fetch_page(&sql, 0, limit, PageCap::Full, abort)
            .await?;
        Ok(page
            .rows
            .into_iter()
            .map(|r| red_core::LookupRow {
                id: r.first().cloned().unwrap_or(Value::Null),
                label: r.get(1).cloned(),
            })
            .collect())
    }

    /// A random-access `(offset, limit)` page of `sql`'s result. Backs the grid's
    /// load-on-scroll so memory stays flat: only the pages around the viewport are
    /// ever resident. `cap` chooses display capping (the common scroll path) or
    /// full fidelity (the clipboard re-fetch); see [`PageCap`]. `abort` cancels a
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
    /// (`descending = true`, returned in reverse order; the caller flips)
    /// `bound`; `None` starts from the result's first/last row. An indexed seek,
    /// so it costs the same at row 200 or 46,000,000, unlike `fetch_page`'s
    /// O(offset).
    ///
    /// `descending` is the *scroll* direction; it composes (XOR) with the key's
    /// own [`descending`](KeySpec::descending) sort direction. `bound` carries one
    /// value per leading key column: the full tuple for a contiguous seek, or
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
    /// post-seek window: O(skip), not O(offset-from-row-0). Backs exact "go to
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

    /// `MIN`/`MAX` of `key` over `sql`'s result: one indexed probe, backing
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

    /// Run several data-modifying statements as ONE atomic transaction: BEGIN, run
    /// each in order, COMMIT; on any error ROLLBACK the whole batch and return the
    /// error, so the set lands all-or-nothing. Returns the per-statement affected
    /// counts (same length/order as `statements`). Backs the assistant's approved
    /// multi-statement changeset (Feature B): the model proposes N statements, the
    /// user approves them as a unit, and they commit together or not at all. An empty
    /// batch is a no-op returning an empty vec without opening a transaction. A
    /// read-only driver rejects the writes at the engine. (An engine without
    /// multi-statement transactions runs them sequentially; it documents that.)
    async fn execute_batch(&self, statements: &[String]) -> Result<Vec<u64>>;

    /// Apply a batch of guarded, PK-keyed data edits (Track B6) **atomically** in a
    /// single transaction: render each `op` to dialect SQL with every value **bound**
    /// (see [`edit_sql`]), run them in order, and assert each touches exactly one row,
    /// rolling the *whole* batch back and returning [`edit_count_err`] (or the
    /// engine error) the moment any op fails or matches ≠1 row, so a multi-edit
    /// submit is all-or-nothing and a stale/non-unique key can't half-apply. A
    /// read-only driver rejects the writes at the engine (defense in depth behind the
    /// UI's opt-in gate). Returns the total affected count (`== ops.len()` on
    /// success). An empty batch is a no-op returning 0 without opening a transaction.
    async fn apply_edits(&self, ops: &[EditOp]) -> Result<u64>;

    /// Apply one guarded data edit: the single-row common case, delegating to the
    /// transactional [`apply_edits`](DatabaseDriver::apply_edits) batch path.
    async fn apply_edit(&self, op: &EditOp) -> Result<u64> {
        self.apply_edits(std::slice::from_ref(op)).await
    }

    /// Bulk-insert `rows` into `table` for `columns` (each a target column's name +
    /// best-effort `decl_type`), as multi-row, fully-bound `INSERT … VALUES
    /// (…),(…),…` inside **one** transaction, the streaming sibling of
    /// [`apply_edits`](DatabaseDriver::apply_edits) for data import / table copy.
    /// Unlike the edit path it makes **no** 1-row assertion (an insert affects
    /// `rows.len()`) and issues one statement per sub-chunk rather than one per row.
    /// Every value is bound via the same per-engine binder as an edit (a
    /// [`Value::Null`] becomes the literal `NULL`, never a typeless bind); the driver
    /// sub-chunks `rows` to keep the bound-parameter count under the engine cap, all
    /// in the one transaction, and rolls the whole call back on any error. A
    /// read-only driver rejects the write at the engine. Returns the rows inserted
    /// (`== rows.len()` on success); an empty `rows` is a no-op returning 0 without
    /// opening a transaction.
    async fn insert_rows(
        &self,
        table: &TableRef,
        columns: &[Column],
        rows: &[Vec<Value>],
    ) -> Result<u64>;

    /// Empty `table` in one transaction (`DELETE FROM <table>`), returning the rows
    /// removed. The `TruncateInsert` table-copy mode runs this before streaming the
    /// source in, so the target is refreshed rather than appended to. Identifier is
    /// quoted by the engine's own helper (never interpolated UI text); a read-only
    /// driver rejects it at the engine, like every other write seam. `DELETE` (not
    /// `TRUNCATE`) is used for cross-engine uniformity and transactional safety;
    /// MySQL's `TRUNCATE` auto-commits and resets auto-increment, which a refresh
    /// shouldn't silently do.
    async fn clear_table(&self, table: &TableRef) -> Result<u64>;

    /// Create `table` with `columns` **if it doesn't already exist**, in this engine's
    /// dialect: the keystone of "copy into a *new* table" / whole-database migration.
    /// Each column's declared type is mapped into this engine via
    /// [`red_core::typemap`] (so a cross-engine create produces faithful DDL, not a
    /// foreign type that fails at execute time), `NOT NULL` and a composite
    /// `PRIMARY KEY` are carried, and the `CREATE` runs through the same transaction
    /// wrapper as [`execute`](DatabaseDriver::execute) (shared body:
    /// [`create_table_sql`]). Defaults, indexes, foreign keys, and auto-increment are
    /// **not** emitted in v1 (see `docs/plans/database-migration.md`). Idempotent
    /// (`IF NOT EXISTS`). A read-only driver (ClickHouse) rejects it at the engine, so
    /// it is never a migration target. Returns the engine's affected-row count (0 for
    /// DDL on most engines).
    async fn create_table(&self, table: &TableRef, columns: &[ColumnMeta]) -> Result<u64>;

    /// Qualify + quote `table` for this engine (`"schema"."name"`, `` `schema`.`name` ``)
    /// so the migration job can build a `SELECT * FROM <table>` source query without
    /// interpolating raw identifiers. Pure string, no I/O (shared body:
    /// [`qualify_table`]). Implemented by every driver, including read-only ClickHouse,
    /// which can be a migration *source*.
    fn quote_table(&self, table: &TableRef) -> String;

    /// Quote a single identifier (a column name) for this engine (`"col"`, `` `col` ``),
    /// so seams like [`fetch_lookup`](Self::fetch_lookup) can build a `SELECT` without
    /// interpolating raw identifiers. Pure string, no I/O; each driver delegates to its
    /// own identifier-quoting helper (the same one `quote_table` uses per segment).
    fn quote_ident(&self, ident: &str) -> String;

    /// Create a secondary index on `table` over `columns` (optionally `unique`) in this
    /// engine's dialect: the migration's **deferred index pass**, run after the data
    /// loads (shared body: [`create_index_sql`]). Built from the source table's
    /// [`IndexMeta`](red_core::IndexMeta); the primary-key-backing index is filtered out
    /// by the caller. A read-only driver (ClickHouse) rejects it. The migrate job treats
    /// a failure as non-fatal (logs + continues; an index is decoration, the data is in).
    async fn create_index(
        &self,
        table: &TableRef,
        name: &str,
        unique: bool,
        columns: &[String],
    ) -> Result<u64>;

    /// Add a foreign key from `child(columns)` to `parent(ref_columns)` in this engine's
    /// dialect: the migration's **deferred FK pass**, run after all tables exist + are
    /// filled (so dependency order can't block) (shared body: [`add_fk_sql`]).
    /// **SQLite can't `ALTER TABLE ADD CONSTRAINT`**, so its impl returns an error the
    /// migrate job treats as a logged skip; ClickHouse (read-only/OLAP) likewise. The
    /// migrate job is best-effort: a failed FK is logged, not fatal.
    async fn add_foreign_key(
        &self,
        child: &TableRef,
        columns: &[String],
        parent: &TableRef,
        ref_columns: &[String],
    ) -> Result<u64>;

    /// Run the engine's `EXPLAIN` for `sql` and return a normalized [`QueryPlan`]
    /// (Track B4). Plain `explain` (`analyze = false`) never executes the
    /// statement; it's read-only-safe for any SQL. `analyze = true` runs
    /// `EXPLAIN ANALYZE`, which *executes* the statement to gather actuals; the
    /// caller gates that to read queries (SQLite has no ANALYZE and ignores the
    /// flag). Each driver reads its native textual/tabular plan and maps it; no
    /// `FORMAT JSON`, so no JSON parser enters the layer.
    async fn explain(&self, sql: &str, analyze: bool) -> Result<QueryPlan>;

    /// Stream `sql`'s result straight to `path` in `format`, row-by-row, never
    /// materializing the whole result. Returns the number of data rows written.
    ///
    /// `cancel` is checked per row: when it flips true the export bails early,
    /// removes the partial file, and returns [`RedError::Interrupted`]. `progress`
    /// receives the running row count, throttled (every N rows / ~50ms) so the
    /// channel isn't flooded; the caller maps it to a progress event.
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
/// `Box<dyn QueryCursor>`. `next_window` takes `&self` (all mutable cursor state
/// lives on the driver's blocking thread), so the returned future is
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
/// `max` is caller-supplied and can be enormous; the cancel-mid-fetch path asks
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
/// that fetch is superseded: a flung scrollbar, a re-sort, a closed tab.
///
/// Where [`CancelToken`] is produced *by* the driver (the streaming cursor hands
/// one back), a one-shot `async fn` can't return a handle before it's awaited, so
/// this inverts it: the caller owns the handle and the driver [`arm`](Self::arm)s
/// it with an engine [`CancelToken`] for the fetch's lifetime. The arm is dropped
/// when the fetch returns ([`ArmGuard`]), so a late `abort` after completion (the
/// connection already back in a pool and reused) is a harmless no-op.
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
    /// engine call so a fetch superseded *before* it starts bails immediately;
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

/// Lock a mutex, tolerating poison: the armed-list critical sections can't panic,
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
            auto_increment: false,
        }
    }

    #[test]
    fn create_table_sql_emits_auto_increment_per_dialect() {
        let cols = vec![
            ColumnMeta {
                name: "id".into(),
                type_name: Some("bigint".into()),
                not_null: true,
                primary_key: true,
                default: None,
                auto_increment: true,
            },
            ColumnMeta {
                name: "name".into(),
                type_name: Some("text".into()),
                not_null: false,
                primary_key: false,
                default: None,
                auto_increment: false,
            },
        ];
        let t = TableRef {
            schema: None,
            name: "t".into(),
        };
        // SQLite: a sole INTEGER PK auto-inc column is emitted inline as the rowid
        // alias, and must NOT also appear in a trailing PRIMARY KEY clause.
        let s = create_table_sql(&t, &cols, DbKind::Sqlite, |i| format!("\"{i}\""));
        assert!(s.contains("\"id\" INTEGER PRIMARY KEY"), "{s}");
        assert!(!s.contains("PRIMARY KEY (\"id\")"), "{s}");
        // Postgres: bigserial (Int → serial) + a trailing PK clause.
        let p = create_table_sql(&t, &cols, DbKind::Postgres, |i| format!("\"{i}\""));
        assert!(p.contains("\"id\" bigserial"), "{p}");
        assert!(p.contains("PRIMARY KEY (\"id\")"), "{p}");
        // MySQL: `<type> AUTO_INCREMENT` + a trailing PK clause.
        let m = create_table_sql(&t, &cols, DbKind::Mysql, |i| format!("`{i}`"));
        assert!(m.contains("`id` bigint AUTO_INCREMENT"), "{m}");
        assert!(m.contains("PRIMARY KEY (`id`)"), "{m}");
    }

    #[test]
    fn add_fk_and_create_index_sql_quote_identifiers() {
        let child = TableRef {
            schema: Some("public".into()),
            name: "child".into(),
        };
        let parent = TableRef {
            schema: Some("public".into()),
            name: "parent".into(),
        };
        let q = |i: &str| format!("\"{i}\"");
        assert_eq!(
            add_fk_sql(&child, &["parent_id".into()], &parent, &["id".into()], q),
            "ALTER TABLE \"public\".\"child\" ADD FOREIGN KEY (\"parent_id\") \
             REFERENCES \"public\".\"parent\" (\"id\")"
        );
        // Postgres: UNIQUE off, `IF NOT EXISTS` supported.
        assert_eq!(
            create_index_sql(
                &child,
                "ix_child_pid",
                false,
                &["parent_id".into()],
                DbKind::Postgres,
                q
            ),
            "CREATE INDEX IF NOT EXISTS \"ix_child_pid\" ON \"public\".\"child\" (\"parent_id\")"
        );
        // MySQL: UNIQUE on, no `IF NOT EXISTS`, composite columns.
        let myq = |i: &str| format!("`{i}`");
        assert_eq!(
            create_index_sql(
                &child,
                "ix",
                true,
                &["a".into(), "b".into()],
                DbKind::Mysql,
                myq
            ),
            "CREATE UNIQUE INDEX `ix` ON `public`.`child` (`a`, `b`)"
        );
        // SQLite: the schema rides on the *index name*, the table is bare.
        assert_eq!(
            create_index_sql(
                &child,
                "ix",
                false,
                &["parent_id".into()],
                DbKind::Sqlite,
                q
            ),
            "CREATE INDEX IF NOT EXISTS \"public\".\"ix\" ON \"child\" (\"parent_id\")"
        );
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
        // Identifiers quoted by the engine's `quote`; values rendered as literals:
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
        let pred = eq_clause(&pairs, |c| format!("\"{c}\""), false);
        assert_eq!(pred, r#""a" = 7 AND "name" = 'O''Brien'"#);
    }

    #[test]
    fn sql_literal_escapes_backslash_only_on_backslash_engines() {
        // A value ending in `\` before a quote is the FK-follow injection vector: on
        // MySQL/ClickHouse (`backslash_escapes = true`) the backslash is doubled so it
        // can't escape the closing quote; on Postgres/SQLite it's a literal byte.
        let v = Value::Text(r"\' OR 1=1 -- ".into());
        // Postgres/SQLite: only the quote is doubled (backslash is literal there).
        assert_eq!(sql_literal(&v, false), r"'\'' OR 1=1 -- '");
        // MySQL/ClickHouse: the backslash is doubled too, so the payload stays inside
        // the string literal; `'\\'' OR 1=1 -- '` reads back as the literal text.
        assert_eq!(sql_literal(&v, true), r"'\\'' OR 1=1 -- '");
        // The whole eq predicate carries the escaping through per-engine.
        let pairs = vec![ColumnValue {
            column: "c".into(),
            value: v,
            decl_type: None,
        }];
        assert_eq!(
            eq_clause(&pairs, |c| format!("`{c}`"), true),
            r"`c` = '\\'' OR 1=1 -- '"
        );
    }

    #[test]
    fn join_wrap_empty_is_noop() {
        // No expansions → the base SQL is returned untouched (no subquery wrap).
        assert_eq!(
            join_wrap("SELECT * FROM t", &["id".into()], &[], |c| format!(
                "\"{c}\""
            )),
            "SELECT * FROM t"
        );
    }

    #[test]
    fn join_wrap_interleaves_joined_column_next_to_its_fk() {
        // One FK expansion: the chosen ref column lands *right after* its FK column
        // (`tier_id`), not at the end; base columns before and after keep their slots.
        // The trailing `;` on the base is stripped before the subquery wrap.
        let base_cols = vec!["id".into(), "tier_id".into(), "name".into()];
        let joins = vec![FkJoin {
            alias: "_red_j0".into(),
            parent_alias: "_red_base".into(),
            on: vec![("tier_id".into(), "id".into())],
            to_schema: Some("main".into()),
            to_table: "tier".into(),
            select: vec![("name".into(), "tier_id.name".into())],
        }];
        assert_eq!(
            join_wrap(
                "SELECT * FROM \"main\".\"channel\";",
                &base_cols,
                &joins,
                |c| { format!("\"{c}\"") }
            ),
            "SELECT _red_base.\"id\", _red_base.\"tier_id\", \
             _red_j0.\"name\" AS \"tier_id.name\", _red_base.\"name\" \
             FROM (SELECT * FROM \"main\".\"channel\") AS _red_base \
             LEFT JOIN \"main\".\"tier\" AS _red_j0 ON _red_base.\"tier_id\" = _red_j0.\"id\""
        );
    }

    #[test]
    fn join_wrap_chains_hops_depth_first_under_the_anchor() {
        // A two-hop chain (tier → cascade): both joined columns anchor under the base
        // `tier_id`, depth-first: the deeper `tier_id.cascade_id.name` sorts before
        // the shallower `tier_id.name` (segment-wise), so the cascade subtree stays
        // grouped. The second hop's `ON` references the first hop's alias.
        let base_cols = vec!["id".into(), "tier_id".into()];
        let joins = vec![
            FkJoin {
                alias: "_red_j0".into(),
                parent_alias: "_red_base".into(),
                on: vec![("tier_id".into(), "id".into())],
                to_schema: None,
                to_table: "tier".into(),
                select: vec![("name".into(), "tier_id.name".into())],
            },
            FkJoin {
                alias: "_red_j1".into(),
                parent_alias: "_red_j0".into(),
                on: vec![("cascade_id".into(), "id".into())],
                to_schema: None,
                to_table: "cascade".into(),
                select: vec![("name".into(), "tier_id.cascade_id.name".into())],
            },
        ];
        assert_eq!(
            join_wrap("SELECT * FROM channel", &base_cols, &joins, |c| format!(
                "\"{c}\""
            )),
            "SELECT _red_base.\"id\", _red_base.\"tier_id\", \
             _red_j1.\"name\" AS \"tier_id.cascade_id.name\", \
             _red_j0.\"name\" AS \"tier_id.name\" \
             FROM (SELECT * FROM channel) AS _red_base \
             LEFT JOIN \"tier\" AS _red_j0 ON _red_base.\"tier_id\" = _red_j0.\"id\" \
             LEFT JOIN \"cascade\" AS _red_j1 ON _red_j0.\"cascade_id\" = _red_j1.\"id\""
        );
    }

    #[test]
    fn join_wrap_empty_base_cols_falls_back_to_star_and_appends() {
        // Base order unknown → `_red_base.*` then the joined columns appended (still
        // correct, just not interleaved). Also covers the composite-FK `ON` AND-chain.
        let joins = vec![FkJoin {
            alias: "_red_j0".into(),
            parent_alias: "_red_base".into(),
            on: vec![("a".into(), "x".into()), ("b".into(), "y".into())],
            to_schema: None,
            to_table: "ref".into(),
            select: vec![("label".into(), "fk.label".into())],
        }];
        assert_eq!(
            join_wrap("SELECT * FROM t", &[], &joins, |c| format!("\"{c}\"")),
            "SELECT _red_base.*, _red_j0.\"label\" AS \"fk.label\" \
             FROM (SELECT * FROM t) AS _red_base \
             LEFT JOIN \"ref\" AS _red_j0 \
             ON _red_base.\"a\" = _red_j0.\"x\" AND _red_base.\"b\" = _red_j0.\"y\""
        );
    }

    #[test]
    fn sql_literal_maps_non_key_kinds_to_null() {
        // A null / blob / display-capped value can't be an FK key; it renders as the
        // literal NULL so `col = NULL` matches nothing instead of erroring.
        assert_eq!(sql_literal(&Value::Null, false), "NULL");
        assert_eq!(sql_literal(&Value::Blob(vec![1, 2, 3]), false), "NULL");
        assert_eq!(sql_literal(&Value::Integer(5), false), "5");
        assert_eq!(sql_literal(&Value::Text("x".into()), false), "'x'");
    }

    #[test]
    fn parse_mysql_enum_splits_variants_and_unescapes_quotes() {
        assert_eq!(
            parse_mysql_enum("enum('active','inactive','pending')"),
            vec!["active", "inactive", "pending"]
        );
        // Doubled '' inside a variant is one literal quote.
        assert_eq!(parse_mysql_enum("enum('a''b','c')"), vec!["a'b", "c"]);
        // `set(...)` parses the same way; a non-enum type yields nothing.
        assert_eq!(parse_mysql_enum("set('x','y')"), vec!["x", "y"]);
        assert!(parse_mysql_enum("varchar(255)").is_empty());
        assert!(parse_mysql_enum("enum()").is_empty());
    }

    #[test]
    fn lookup_sql_projects_id_and_optional_label() {
        // No label: id only, ordered + capped.
        assert_eq!(
            lookup_sql("\"pub\".\"users\"", "\"id\"", None, 500),
            "SELECT DISTINCT \"id\" FROM \"pub\".\"users\" ORDER BY \"id\" LIMIT 500"
        );
        // With a distinct label column: both projected.
        assert_eq!(
            lookup_sql("\"users\"", "\"id\"", Some("\"name\""), 200),
            "SELECT DISTINCT \"id\", \"name\" FROM \"users\" ORDER BY \"id\" LIMIT 200"
        );
        // Label equal to the id: no duplicate projection.
        assert_eq!(
            lookup_sql("\"t\"", "\"id\"", Some("\"id\""), 10),
            "SELECT DISTINCT \"id\" FROM \"t\" ORDER BY \"id\" LIMIT 10"
        );
    }

    #[test]
    fn stats_sql_orders_aggregates_and_gates_optional_ones() {
        let q = |c: &str| format!("\"{c}\"");
        // Cheap (non-numeric, distinct withheld): count/count/min/max only.
        assert_eq!(
            stats_sql("SELECT * FROM t;", "name", false, false, q),
            "SELECT count(*), count(\"name\"), min(\"name\"), max(\"name\") \
             FROM (SELECT * FROM t) AS _red"
        );
        // Numeric + distinct: the optional columns slot into the fixed order.
        assert_eq!(
            stats_sql("SELECT * FROM t", "n", true, true, q),
            "SELECT count(*), count(\"n\"), count(distinct \"n\"), min(\"n\"), max(\"n\"), \
             sum(\"n\"), avg(\"n\") FROM (SELECT * FROM t) AS _red"
        );
    }

    #[test]
    fn parse_stats_reads_positional_aggregates() {
        // Numeric + distinct: count, count, count(distinct), min, max, sum, avg.
        let cells = vec![
            Value::Integer(100),
            Value::Integer(90),
            Value::Integer(42),
            Value::Integer(3),
            Value::Integer(99),
            Value::Integer(4500),
            Value::Real(50.0),
        ];
        let s = parse_stats(&cells, true, true);
        assert_eq!(s.total, 100);
        assert_eq!(s.non_null, 90);
        assert_eq!(s.distinct, Some(42));
        assert_eq!(s.min, Value::Integer(3));
        assert_eq!(s.max, Value::Integer(99));
        assert_eq!(s.sum, Some(Value::Integer(4500)));
        assert_eq!(s.avg, Some(Value::Real(50.0)));

        // Distinct withheld + non-numeric: no distinct/sum/avg, min/max shift left.
        let cells = vec![
            Value::Integer(5),
            Value::Integer(5),
            Value::Text("a".into()),
            Value::Text("z".into()),
        ];
        let s = parse_stats(&cells, false, false);
        assert_eq!(s.distinct, None);
        assert_eq!(s.min, Value::Text("a".into()));
        assert_eq!(s.max, Value::Text("z".into()));
        assert_eq!(s.sum, None);
        assert_eq!(s.avg, None);

        // A count returned as a numeric string (e.g. an engine's bigint-as-text) and
        // an all-null numeric column: counts parse, sum/avg collapse to None.
        let cells = vec![
            Value::Text("12".into()),
            Value::Integer(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ];
        let s = parse_stats(&cells, true, false);
        assert_eq!(s.total, 12);
        assert_eq!(s.non_null, 0);
        assert_eq!(s.min, Value::Null);
        assert_eq!(s.sum, None);
        assert_eq!(s.avg, None);
    }
}
