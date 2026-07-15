//! RED's SQL domain logic for the editor: a hand-rolled tokenizer feeding Flint's
//! generic `Highlighter` seam, plus the keyword set and word-prefix helper the
//! completion provider uses. SQL-dialect knowledge stays here, behind the
//! highlighter seam, so Flint stays domain-free.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use flint::TokenStyle;
use red_core::DbKind;

/// SQL keywords, lowercase. Drives both highlighting and (upper-cased) completion.
pub const KEYWORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "insert",
    "into",
    "values",
    "update",
    "set",
    "delete",
    "create",
    "table",
    "drop",
    "alter",
    "add",
    "column",
    "join",
    "left",
    "right",
    "inner",
    "outer",
    "full",
    "cross",
    "on",
    "using",
    "group",
    "by",
    "order",
    "asc",
    "desc",
    "limit",
    "offset",
    "as",
    "and",
    "or",
    "not",
    "null",
    "is",
    "in",
    "like",
    "ilike",
    "between",
    "distinct",
    "case",
    "when",
    "then",
    "else",
    "end",
    "union",
    "intersect",
    "except",
    "all",
    "having",
    "primary",
    "key",
    "foreign",
    "references",
    "default",
    "unique",
    "check",
    "constraint",
    "index",
    "view",
    "with",
    "exists",
    "cast",
    "begin",
    "commit",
    "rollback",
    "transaction",
    "if",
    "returning",
    "true",
    "false",
];

/// SQL functions offered in completion as `(name, signature, guide)`. The
/// signature shows beside the candidate; the guide fills the doc panel.
pub const FUNCTIONS: &[(&str, &str, &str)] = &[
    (
        "count",
        "count(expr) → bigint",
        "Counts rows, or non-null values.",
    ),
    ("sum", "sum(expr) → numeric", "Sum of a numeric expression."),
    (
        "avg",
        "avg(expr) → numeric",
        "Mean of a numeric expression.",
    ),
    ("min", "min(expr)", "Smallest value in the group."),
    ("max", "max(expr)", "Largest value in the group."),
    ("coalesce", "coalesce(a, b, …)", "First non-null argument."),
    (
        "now",
        "now() → timestamptz",
        "Current transaction timestamp.",
    ),
    (
        "length",
        "length(text) → int",
        "Character length of a string.",
    ),
    ("lower", "lower(text) → text", "Lower-case a string."),
    ("upper", "upper(text) → text", "Upper-case a string."),
    ("round", "round(num, digits)", "Round to N decimal places."),
    (
        "date_trunc",
        "date_trunc(unit, ts)",
        "Truncate a timestamp to a unit.",
    ),
    // --- strings ---
    ("concat", "concat(a, b, …) → text", "Concatenate strings."),
    (
        "concat_ws",
        "concat_ws(sep, a, b, …) → text",
        "Concatenate strings with a separator, skipping nulls.",
    ),
    (
        "substring",
        "substring(text, from, len) → text",
        "Extract a substring by position and length.",
    ),
    (
        "trim",
        "trim(text) → text",
        "Strip leading and trailing spaces.",
    ),
    ("ltrim", "ltrim(text) → text", "Strip leading spaces."),
    ("rtrim", "rtrim(text) → text", "Strip trailing spaces."),
    (
        "replace",
        "replace(text, from, to) → text",
        "Replace every occurrence of a substring.",
    ),
    ("left", "left(text, n) → text", "The first n characters."),
    ("right", "right(text, n) → text", "The last n characters."),
    (
        "lpad",
        "lpad(text, len, fill) → text",
        "Left-pad a string to a length.",
    ),
    (
        "rpad",
        "rpad(text, len, fill) → text",
        "Right-pad a string to a length.",
    ),
    ("reverse", "reverse(text) → text", "Reverse a string."),
    (
        "split_part",
        "split_part(text, sep, n) → text",
        "The nth field, splitting on a separator.",
    ),
    // --- numbers ---
    ("abs", "abs(num) → num", "Absolute value."),
    (
        "ceil",
        "ceil(num) → num",
        "Round up to the nearest integer.",
    ),
    (
        "floor",
        "floor(num) → num",
        "Round down to the nearest integer.",
    ),
    ("mod", "mod(a, b) → num", "Remainder of a divided by b."),
    (
        "power",
        "power(base, exp) → num",
        "base raised to the power exp.",
    ),
    ("sqrt", "sqrt(num) → num", "Square root."),
    ("sign", "sign(num) → int", "Sign of a number: -1, 0, or 1."),
    (
        "trunc",
        "trunc(num, digits) → num",
        "Truncate toward zero to N decimals.",
    ),
    // --- dates ---
    ("current_date", "current_date → date", "Today's date."),
    (
        "current_timestamp",
        "current_timestamp → timestamptz",
        "The current date and time.",
    ),
    (
        "extract",
        "extract(field FROM ts) → num",
        "Pull a field (year, month, …) from a timestamp.",
    ),
    (
        "date_part",
        "date_part(field, ts) → num",
        "Get a field from a timestamp.",
    ),
    // --- conditional / null ---
    ("nullif", "nullif(a, b)", "NULL when a equals b, else a."),
    ("ifnull", "ifnull(a, b)", "b when a is NULL (MySQL/SQLite)."),
    (
        "greatest",
        "greatest(a, b, …)",
        "The largest of the arguments.",
    ),
    ("least", "least(a, b, …)", "The smallest of the arguments."),
    // --- aggregates ---
    (
        "string_agg",
        "string_agg(expr, sep) → text",
        "Concatenate values across a group.",
    ),
    (
        "group_concat",
        "group_concat(expr) → text",
        "Concatenate values across a group (MySQL/SQLite).",
    ),
    (
        "array_agg",
        "array_agg(expr)",
        "Collect values across a group into an array.",
    ),
];

// Engine bits for the function availability matrix below.
const PG: u8 = 1; // Postgres
const MY: u8 = 2; // MySQL
const SL: u8 = 4; // SQLite
const CH: u8 = 8; // ClickHouse
const ALL: u8 = PG | MY | SL | CH;

/// Which engines a [`FUNCTIONS`] entry is available on, by name. Absent → `ALL`.
///
/// Best-effort and deliberately *conservative*: when a function's presence (or its
/// spelling) on an engine is doubtful, it's left out, so completion/hover under-offer
/// rather than suggest something the connected engine rejects. Names that differ per
/// engine (`trunc` vs MySQL `truncate`, `ifnull` vs Postgres `coalesce`,
/// `string_agg` vs MySQL `group_concat`) are scoped to where that exact spelling works.
fn function_engines(name: &str) -> u8 {
    match name {
        // Postgres-only spellings.
        "split_part" | "date_part" | "string_agg" | "array_agg" => PG,
        // Postgres + MySQL.
        "lpad" | "rpad" | "extract" => PG | MY,
        // Postgres + MySQL + ClickHouse (no SQLite equivalent by this name).
        "now" | "left" | "right" | "reverse" | "greatest" | "least" | "substring" => PG | MY | CH,
        // Postgres + MySQL + SQLite (ClickHouse spells these differently).
        "ltrim" | "rtrim" | "replace" | "mod" => PG | MY | SL,
        // Postgres + SQLite + ClickHouse (MySQL uses `truncate`).
        "trunc" => PG | SL | CH,
        // Postgres + ClickHouse.
        "date_trunc" => PG | CH,
        // MySQL + SQLite + ClickHouse (Postgres uses `coalesce`).
        "ifnull" => MY | SL | CH,
        // MySQL + SQLite.
        "group_concat" => MY | SL,
        // Everything else is broadly portable.
        _ => ALL,
    }
}

/// The functions available on `kind`, for completion + signature hover — [`FUNCTIONS`]
/// filtered by the [`function_engines`] matrix so a MySQL connection is never offered
/// `string_agg`, nor a Postgres one `group_concat`.
pub fn functions_for(kind: DbKind) -> Vec<(&'static str, &'static str, &'static str)> {
    // Redis has no SQL editor surface at all (see docs/plans/redis.md), so no
    // bit in this matrix names it; `functions_for` returns empty for it below
    // rather than reaching an `unreachable!()` this function's own callers
    // don't (yet) structurally rule out for a Redis connection.
    let Some(bit) = (match kind {
        DbKind::Postgres => Some(PG),
        DbKind::Mysql => Some(MY),
        DbKind::Sqlite => Some(SL),
        DbKind::Clickhouse => Some(CH),
        // Neither Redis nor MongoDB has a SQL editor surface, so no bit names
        // them; `functions_for` returns empty rather than reaching an
        // `unreachable!()` for a non-SQL connection.
        DbKind::Redis | DbKind::Mongo => None,
    }) else {
        return Vec::new();
    };
    FUNCTIONS
        .iter()
        .filter(|(name, _, _)| function_engines(name) & bit != 0)
        .copied()
        .collect()
}

/// A one-line guide for a (lower-cased) SQL keyword, shown in the completion doc
/// panel. `None` for keywords without a note; they still complete, just bare.
pub fn keyword_doc(kw: &str) -> Option<&'static str> {
    Some(match kw {
        "select" => "Columns or expressions to return.",
        "from" => "Source table, view, or subquery.",
        "where" => "Filter rows by a boolean predicate.",
        "group" => "GROUP BY: collapse rows into groups.",
        "order" => "ORDER BY: sort the result set.",
        "by" => "Pairs with GROUP / ORDER.",
        "having" => "Filter groups after aggregation.",
        "limit" => "Cap the number of returned rows.",
        "offset" => "Skip N rows before returning.",
        "join" => "Combine rows from another table.",
        "left" => "LEFT JOIN: keep unmatched left rows.",
        "right" => "RIGHT JOIN: keep unmatched right rows.",
        "inner" => "INNER JOIN: only matched rows.",
        "outer" | "full" | "cross" => "Outer / cross join variant.",
        "on" => "Join predicate.",
        "using" => "Join on shared column names.",
        "as" => "Alias a column or table.",
        "and" => "Logical conjunction.",
        "or" => "Logical disjunction.",
        "not" => "Logical negation.",
        "in" => "Match against a value list.",
        "like" | "ilike" => "Pattern match on text.",
        "between" => "Range test: x BETWEEN a AND b.",
        "is" => "Null / boolean identity test.",
        "null" => "Absence of a value.",
        "distinct" => "Remove duplicate rows.",
        "case" => "Conditional expression.",
        "when" => "CASE branch condition.",
        "then" => "CASE branch result.",
        "else" => "CASE fallback.",
        "end" => "Close a CASE expression.",
        "union" | "intersect" | "except" => "Combine two result sets.",
        "all" => "Keep duplicates (e.g. UNION ALL).",
        "with" => "Common table expression (CTE).",
        "asc" => "Ascending sort order.",
        "desc" => "Descending sort order.",
        "exists" => "True if a subquery returns rows.",
        "cast" => "Convert a value's type.",
        "insert" => "Insert new rows.",
        "into" => "Target table for INSERT.",
        "values" => "Row literals for INSERT.",
        "update" => "Modify existing rows.",
        "set" => "Assign columns in UPDATE.",
        "delete" => "Remove rows.",
        "returning" => "Return the affected rows.",
        "create" | "alter" | "drop" => "Schema definition (DDL).",
        _ => return None,
    })
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}
fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
fn is_operator(b: u8) -> bool {
    matches!(
        b,
        b'+' | b'-' | b'*' | b'/' | b'=' | b'<' | b'>' | b'!' | b'%' | b'|' | b'&' | b'^' | b'~'
    )
}

/// Tokenize `src` into `(byte range, style)` spans. Gaps between spans
/// (whitespace, punctuation) render in the default text color.
pub fn tokenize(src: &str) -> Vec<(Range<usize>, TokenStyle)> {
    let b = src.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0;

    while i < n {
        let c = b[i];

        // Line comment: -- to end of line.
        if c == b'-' && i + 1 < n && b[i + 1] == b'-' {
            let start = i;
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            out.push((start..i, TokenStyle::Comment));
            continue;
        }

        // Block comment: /* ... */
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            let start = i;
            i += 2;
            while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            out.push((start..i, TokenStyle::Comment));
            continue;
        }

        // String / quoted literal (' or "). A doubled quote (`''`) is treated as
        // close-then-reopen rather than an escaped inner quote. Deliberately: it
        // keeps the lexer trivial, and since the content between the doubled quotes
        // stays "inside a string", a `;` there is still not a statement boundary, so
        // `classify`/`split_statements` (the destructive-query guard) stay correct.
        // The only cost is cosmetic: highlighting can split mid-literal on `''`.
        if c == b'\'' || c == b'"' {
            let quote = c;
            let start = i;
            i += 1;
            while i < n && b[i] != quote {
                i += 1;
            }
            if i < n {
                i += 1; // closing quote
            }
            out.push((start..i, TokenStyle::String));
            continue;
        }

        // Backtick-quoted identifier (MySQL). The inner name is always an identifier,
        // even if it collides with a keyword or is followed by `(`; emit one span for
        // the inner text (backticks excluded) so highlighting and the schema-aware
        // checks treat `` `select` `` / `` `table` `` as the identifier they are.
        if c == b'`' {
            let start = i + 1;
            i += 1;
            while i < n && b[i] != b'`' {
                i += 1;
            }
            let end = i;
            if i < n {
                i += 1; // closing backtick
            }
            if end > start {
                out.push((start..end, TokenStyle::Identifier));
            }
            continue;
        }

        // Number.
        if c.is_ascii_digit() {
            let start = i;
            while i < n && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            out.push((start..i, TokenStyle::Number));
            continue;
        }

        // Identifier / keyword / function.
        if is_ident_start(c) {
            let start = i;
            while i < n && is_ident_continue(b[i]) {
                i += 1;
            }
            let word = &src[start..i];
            let lower = word.to_ascii_lowercase();
            let style = if KEYWORDS.contains(&lower.as_str()) {
                TokenStyle::Keyword
            } else {
                // Function = identifier immediately followed by '(' (skip spaces).
                let mut j = i;
                while j < n && (b[j] == b' ' || b[j] == b'\t') {
                    j += 1;
                }
                if j < n && b[j] == b'(' {
                    TokenStyle::Function
                } else {
                    TokenStyle::Identifier
                }
            };
            out.push((start..i, style));
            continue;
        }

        // Operator run.
        if is_operator(c) {
            let start = i;
            while i < n && is_operator(b[i]) {
                i += 1;
            }
            out.push((start..i, TokenStyle::Operator));
            continue;
        }

        // Whitespace / punctuation: default color, no span.
        i += 1;
    }

    out
}

/// What kind of statement the editor is about to run; drives whether it streams
/// into the result grid, executes in a transaction, or first asks for confirmation.
///
/// Variant order is the severity order (`Query` < `Write` < `Destructive`) so a
/// batch's kind is the `max` of its statements' kinds; see [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatementKind {
    /// Row-returning; opens in the result grid.
    Query,
    /// A write/DDL that's safe to run after a plain transaction (INSERT, CREATE…).
    Write,
    /// A write that destroys or rewrites existing data: confirm before running.
    Destructive,
}

/// Classify a statement *batch* by its most-destructive statement. A paste like
/// `SELECT 1; DROP TABLE users` must confirm, so each `;`-delimited statement is
/// classified and the highest severity wins (`Destructive` > `Write` > `Query`);
/// classifying only the leading keyword would let a destructive tail slip past the
/// confirm modal. A trailing empty statement (after the last `;`) is ignored.
pub fn classify(sql: &str) -> StatementKind {
    split_statements(sql)
        .into_iter()
        .filter(|s| !first_keyword(s).is_empty())
        .map(classify_one)
        .max()
        .unwrap_or(StatementKind::Query)
}

/// A conservative read-only gate for **AI-suggested** SQL that would auto-execute
/// (the agent's `open_query` tool / the "Open in a query tab" chip): the statement
/// must be a single SELECT/WITH/EXPLAIN/VALUES with no statement separator and no
/// embedded write keyword or dangerous server-side function. Anything else is loaded
/// into the tab but *not* run, so a model can never silently execute a write on a
/// writable connection, closing the [`classify`]-only gate's hole where a
/// data-modifying CTE (`WITH x AS (DELETE … RETURNING …) SELECT …`) or a
/// side-effecting function (`SELECT lo_export(…)`) leads with a read keyword.
///
/// This is the UI twin of `red-service`'s `is_read_only_select`, and reasons the
/// same way: over a noise-stripped copy (literals/quoted-identifiers/comments
/// blanked) so a write word inside a string can't fool it and a quoted column named
/// like one can't trip it. Keep the two token lists in sync. False positives are
/// fine: a rejected read just doesn't auto-run; the user can still press Run.
pub fn is_read_only(sql: &str) -> bool {
    let stripped = strip_noise(sql);
    let trimmed = stripped.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return false;
    }
    // A `;` (outside a literal, already blanked) could chain a write past the prefix.
    if trimmed.contains(';') {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    let read_prefix = ["select", "with", "explain", "values"]
        .iter()
        .any(|p| lower.starts_with(p));
    if !read_prefix {
        return false;
    }
    // Whole-word write verbs (the data-modifying CTE verbs, `INTO` for
    // `SELECT … INTO`/`OUTFILE`, sequence advancers) and the well-known file/exec/
    // remote-SQL functions: reserved or underscore-qualified names that can't be a
    // bare column in a real read (a column so named would be quoted, hence blanked).
    const WRITE_TOKENS: &[&str] = &[
        "insert", "update", "delete", "merge", "into", "nextval", "setval",
    ];
    const DANGEROUS_FNS: &[&str] = &[
        "lo_import",
        "lo_export",
        "pg_read_file",
        "pg_read_binary_file",
        "pg_ls_dir",
        "pg_stat_file",
        "pg_logical_emit_message",
        // `dblink`/`dblink_send_query` run arbitrary SQL on a remote (often the
        // same loopback) server from inside a SELECT, a write channel that reads
        // as read-only. Block the bare and async forms, not just `dblink_exec`.
        "dblink",
        "dblink_exec",
        "dblink_open",
        "dblink_send_query",
        "pg_file_write",
        "pg_file_unlink",
        "pg_file_rename",
        "load_file",
        "sys_exec",
        "sys_eval",
    ];
    !WRITE_TOKENS
        .iter()
        .chain(DANGEROUS_FNS)
        .any(|w| has_word(&lower, w))
}

/// A copy of `sql` with string literals, quoted identifiers, and comments blanked to
/// spaces (reusing [`tokenize`], which marks both `'…'` and `"…"` as `String`), so a
/// keyword scan sees only live SQL. Length- and boundary-preserving: every blanked
/// span is a whole token, so replacing its bytes with ASCII spaces keeps valid UTF-8.
fn strip_noise(sql: &str) -> String {
    let mut bytes = sql.as_bytes().to_vec();
    for (range, style) in tokenize(sql) {
        if matches!(style, TokenStyle::String | TokenStyle::Comment) {
            for b in &mut bytes[range] {
                *b = b' ';
            }
        }
    }
    String::from_utf8(bytes).unwrap_or_default()
}

/// Whether `word` occurs in `haystack` as a whole ASCII word, not as a fragment of
/// a longer identifier (so `updated_at` doesn't match `update`). Both are lower-case.
fn has_word(haystack: &str, word: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(word) {
        let start = from + rel;
        let end = start + word.len();
        let left_ok = start == 0 || !is_ident_continue(bytes[start - 1]);
        let right_ok = end == bytes.len() || !is_ident_continue(bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// The single `;`-delimited statement to run for a caret at `cursor`: the
/// statement the caret sits in, or, when it sits in a blank/comment-only region
/// (commonly just past the final `;`), the nearest non-empty statement before it.
/// The editor's "run" uses this so a buffer holding several statements runs just
/// the one under the caret, not the whole buffer: the `SELECT * FROM (<sql>)`
/// paging wrap can't accept a `;`-separated batch and would bounce back a bare
/// syntax error. Returns the trimmed slice; empty only when there's no statement.
pub fn statement_at(content: &str, cursor: usize) -> &str {
    let bounds = statement_bounds(content, cursor);
    let stmt = content[bounds.clone()].trim();
    if !first_keyword(stmt).is_empty() {
        return stmt;
    }
    // Caret in a blank region: fall back to the last non-empty statement before
    // it rather than running nothing.
    split_statements(&content[..bounds.start])
        .into_iter()
        .map(str::trim)
        .rfind(|s| !first_keyword(s).is_empty())
        .unwrap_or(stmt)
}

/// True when `sql` holds nothing runnable: only whitespace, comments, and bare
/// `;` terminators. The editor's run skips these so an empty/comment-only buffer
/// never reaches the engine, where the `SELECT * FROM (<sql>)` paging wrap would
/// collapse to a bare `db error`. Reuses [`tokenize`]: every such input yields
/// only comment tokens (or none), while any real content produces a non-comment.
pub fn is_blank(sql: &str) -> bool {
    tokenize(sql)
        .iter()
        .all(|(_, style)| *style == TokenStyle::Comment)
}

/// How many non-empty `;`-delimited statements `sql` holds. Lets the run path tell
/// a single statement (opens as a result) from a batch (which the paging wrap
/// can't run) so the latter gets a clear message, not a cryptic engine error.
pub fn statement_count(sql: &str) -> usize {
    split_statements(sql)
        .into_iter()
        .filter(|s| !first_keyword(s).is_empty())
        .count()
}

/// Classify a single statement by its leading keyword (comments + whitespace
/// skipped). See [`classify`] for the batch entry point.
fn classify_one(sql: &str) -> StatementKind {
    match first_keyword(sql).to_ascii_uppercase().as_str() {
        "SELECT" | "WITH" | "PRAGMA" | "EXPLAIN" | "VALUES" => StatementKind::Query,
        "DROP" | "DELETE" | "UPDATE" | "ALTER" | "TRUNCATE" | "REPLACE" => {
            StatementKind::Destructive
        }
        _ => StatementKind::Write,
    }
}

/// Split `sql` into its top-level `;`-delimited statements, with `;` inside string
/// literals and comments ignored (the same boundary rules [`statement_bounds`]
/// uses). Borrows; no allocation per statement.
fn split_statements(sql: &str) -> Vec<&str> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < n {
        let c = b[i];
        // Line comment: -- to end of line.
        if c == b'-' && i + 1 < n && b[i + 1] == b'-' {
            i += 2;
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment: /* ... */
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        // String / quoted literal.
        if c == b'\'' || c == b'"' {
            let quote = c;
            i += 1;
            while i < n && b[i] != quote {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        if c == b';' {
            out.push(&sql[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    out.push(&sql[start..]);
    out
}

/// Append `LIMIT n` to a bare row-returning `SELECT` that doesn't already limit
/// itself, so a fat table can't flood the grid (RED's big-result safety rail).
/// Returns `None` (leave the SQL untouched) when `n` is 0, the statement isn't a
/// plain `SELECT`, it already has a `LIMIT`, or it's a multi-statement batch.
/// Deliberately conservative: anything it isn't sure about, it leaves alone.
pub fn auto_limit(sql: &str, n: u32) -> Option<String> {
    if n == 0 || !first_keyword(sql).eq_ignore_ascii_case("select") {
        return None;
    }
    if has_limit_clause(sql) {
        return None;
    }
    let trimmed = sql.trim_end().trim_end_matches(';').trim_end();
    // A `;` left after stripping the trailing one means several statements; don't
    // rewrite, since the `LIMIT` would bind only to the last one.
    if trimmed.contains(';') {
        return None;
    }
    Some(format!("{trimmed} LIMIT {n}"))
}

/// The single physical table a hand-typed `SELECT * FROM <table>` reads, as an
/// optional-schema + table name, so a browse typed into the editor gets the same
/// foreign-key affordances (in-grid accent, click-through, the reference-column
/// tree) as one opened from the schema tree, which only fire when the result maps
/// to one known base table (Track B7).
///
/// Deliberately narrow: returns `None` for anything that isn't provably a
/// single-table star select, because the FK machinery keys off the result's
/// columns *being exactly that table's columns*:
///
/// - only `SELECT *` (a projection could omit the PK the keyset pages on, or an FK
///   column the reference-column join wraps on; both would break);
/// - one table in `FROM`, no `JOIN`, no comma list, no `FROM (subquery)`;
/// - no top-level set operation (`UNION`/`INTERSECT`/`EXCEPT`), whose shape differs.
///
/// A trailing `WHERE`/`ORDER BY`/`LIMIT`/… and subqueries *inside* them are fine;
/// they don't change the column set. Scans a noise-stripped copy so keywords inside
/// string literals / quoted identifiers / comments can't fool it. The bare table
/// name still has to be resolved against the connection catalog by the caller.
pub fn single_table_star(sql: &str) -> Option<(Option<String>, String)> {
    // Lex the raw SQL: [`lex_simple`] skips string literals and comments and unwraps
    // quoted identifiers ("pg/sqlite" or `mysql`), so a quoted table name still parses
    // and a keyword inside a literal/comment can't fool the scan. A stray statement
    // separator surfaces as `Other`, which the parse below rejects.
    let toks = lex_simple(sql.trim().trim_end_matches(';'));
    if toks.is_empty() {
        return None;
    }

    // A top-level set operation makes the result a union of shapes, not the table;
    // reject it wherever it appears at paren-depth 0 (nested in a subquery is fine).
    let mut depth = 0i32;
    for t in &toks {
        match t {
            SimpleToken::LParen => depth += 1,
            SimpleToken::RParen => depth = (depth - 1).max(0),
            SimpleToken::Word(w) if depth == 0 && is_setop(w) => return None,
            _ => {}
        }
    }

    let mut it = toks.iter().peekable();
    // `SELECT`
    match it.next() {
        Some(SimpleToken::Word(w)) if w.eq_ignore_ascii_case("select") => {}
        _ => return None,
    }
    // exactly `*`: no projection, no `DISTINCT`/`ALL`/aggregate.
    if !matches!(it.next(), Some(SimpleToken::Star)) {
        return None;
    }
    // `FROM`
    match it.next() {
        Some(SimpleToken::Word(w)) if w.eq_ignore_ascii_case("from") => {}
        _ => return None,
    }
    // The dotted table reference: `table`, `schema.table`, or `db.schema.table`.
    // The last part is the table; the one before it (if any) is the schema.
    let mut parts: Vec<String> = Vec::new();
    match it.next() {
        Some(SimpleToken::Word(w)) if !is_from_stop(w) => parts.push(w.clone()),
        _ => return None,
    }
    while matches!(it.peek(), Some(SimpleToken::Dot)) {
        it.next();
        match it.next() {
            Some(SimpleToken::Word(w)) => parts.push(w.clone()),
            _ => return None,
        }
    }
    let table = parts.last().cloned()?;
    let schema = (parts.len() >= 2).then(|| parts[parts.len() - 2].clone());

    // An optional alias: `AS x` or a bare `x` that isn't a clause keyword.
    if let Some(SimpleToken::Word(w)) = it.peek() {
        if w.eq_ignore_ascii_case("as") {
            it.next();
            if !matches!(it.next(), Some(SimpleToken::Word(_))) {
                return None;
            }
        } else if !is_from_stop(w) {
            it.next();
        }
    }

    // Whatever follows the `FROM` item must be a shape-preserving tail clause or the
    // end; a `JOIN`, a comma (second table), or another `(subquery)` disqualifies.
    match it.peek() {
        None => {}
        Some(SimpleToken::Word(w)) if is_tail_clause(w) => {}
        _ => return None,
    }

    Some((schema, table))
}

/// A word that ends the `FROM` table item: a clause keyword or a join word (so it's
/// never mistaken for the table's alias, and a `JOIN` after the table disqualifies).
fn is_from_stop(w: &str) -> bool {
    is_tail_clause(w)
        || is_join_word(w)
        || is_setop(w)
        || w.eq_ignore_ascii_case("on")
        || w.eq_ignore_ascii_case("using")
}

/// A clause that can follow the single table without changing its column set.
fn is_tail_clause(w: &str) -> bool {
    matches!(
        w.to_ascii_lowercase().as_str(),
        "where" | "group" | "order" | "having" | "limit" | "offset" | "window" | "fetch" | "for"
    )
}

fn is_join_word(w: &str) -> bool {
    matches!(
        w.to_ascii_lowercase().as_str(),
        "join" | "inner" | "left" | "right" | "full" | "cross" | "natural"
    )
}

fn is_setop(w: &str) -> bool {
    matches!(
        w.to_ascii_lowercase().as_str(),
        "union" | "intersect" | "except"
    )
}

/// A coarse token for [`single_table_star`]'s FROM-clause parse: words (bare or
/// quoted identifiers) plus the punctuation that changes a query's shape (`*`, `,`,
/// `.`, parens). Everything else (operators, numbers, `;`) collapses to `Other`,
/// which the parse just rejects on.
#[derive(PartialEq, Eq)]
enum SimpleToken {
    Word(String),
    Star,
    Comma,
    Dot,
    LParen,
    RParen,
    Other,
}

/// Lex for the single-table sniff: identifiers (bare + quoted), the shape
/// punctuation, and nothing else. String literals and comments are skipped so a
/// keyword inside them can't fool the parse; a quoted identifier (`"…"`/`` `…` ``)
/// yields its inner name as a `Word` so quoted table/schema names still resolve.
fn lex_simple(s: &str) -> Vec<SimpleToken> {
    let b = s.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let c = b[i];
        // Line comment: `--` to end of line.
        if c == b'-' && i + 1 < n && b[i + 1] == b'-' {
            i += 2;
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment: `/* … */`.
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        // String literal: its content is data; a `SELECT *` never has one where a
        // token is expected, so emit `Other` to fail the parse if it appears.
        if c == b'\'' {
            i += 1;
            while i < n && b[i] != b'\'' {
                i += 1;
            }
            i = (i + 1).min(n);
            out.push(SimpleToken::Other);
            continue;
        }
        // Quoted identifier (`"pg/sqlite"` or `` `mysql` ``) → its inner name.
        if c == b'"' || c == b'`' {
            let quote = c;
            let start = i + 1;
            i += 1;
            while i < n && b[i] != quote {
                i += 1;
            }
            out.push(SimpleToken::Word(s[start..i.min(n)].to_string()));
            i = (i + 1).min(n);
            continue;
        }
        if is_ident_start(c) {
            let start = i;
            while i < n && is_ident_continue(b[i]) {
                i += 1;
            }
            out.push(SimpleToken::Word(s[start..i].to_string()));
            continue;
        }
        match c {
            b'*' => out.push(SimpleToken::Star),
            b',' => out.push(SimpleToken::Comma),
            b'.' => out.push(SimpleToken::Dot),
            b'(' => out.push(SimpleToken::LParen),
            b')' => out.push(SimpleToken::RParen),
            _ if c.is_ascii_whitespace() => {}
            _ => out.push(SimpleToken::Other),
        }
        i += 1;
    }
    out
}

/// Replace non-ASCII Unicode whitespace (most commonly U+00A0, the non-breaking
/// space macOS types for Option+Space) with a plain ASCII space, *outside* string
/// literals, quoted identifiers, and comments where such a character could be
/// intentional. Engines reject a bare U+00A0 as an invalid token rather than
/// treating it as whitespace, so one slipped into a query turns a valid-looking
/// statement into a cryptic `syntax error`; the editor run path scrubs it first.
/// Returns `None` (leave the SQL untouched) when there's nothing to normalize, so
/// the common path never allocates.
pub fn normalize_spaces(sql: &str) -> Option<String> {
    // Fast path: only bother scanning when a non-ASCII whitespace char is present.
    if !sql.chars().any(|c| c.is_whitespace() && !c.is_ascii()) {
        return None;
    }
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    let mut changed = false;
    while i < n {
        let c = chars[i];
        // Line comment: copy verbatim to (not including) the newline.
        if c == '-' && i + 1 < n && chars[i + 1] == '-' {
            while i < n && chars[i] != '\n' {
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }
        // Block comment: copy verbatim through the closing `*/` (or to the end).
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            out.push('/');
            out.push('*');
            i += 2;
            while i < n && !(chars[i] == '*' && i + 1 < n && chars[i + 1] == '/') {
                out.push(chars[i]);
                i += 1;
            }
            for _ in 0..2 {
                if i < n {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            continue;
        }
        // String literal / quoted identifier: copy verbatim, honoring `''`/`""`
        // doubled-quote escapes so an escaped quote doesn't end the span early.
        if c == '\'' || c == '"' {
            out.push(c);
            i += 1;
            while i < n {
                out.push(chars[i]);
                if chars[i] == c {
                    if i + 1 < n && chars[i + 1] == c {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // Ordinary text: swap any non-ASCII whitespace for a normal space.
        if c.is_whitespace() && !c.is_ascii() {
            out.push(' ');
            changed = true;
        } else {
            out.push(c);
        }
        i += 1;
    }
    changed.then_some(out)
}

/// Whether `sql` already carries a `LIMIT` keyword. Word-boundary, case-insensitive
/// scan; a false positive (a column literally named `limit`, or `limit` inside a
/// string) only *suppresses* the auto-limit, which is the safe direction.
fn has_limit_clause(sql: &str) -> bool {
    sql.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| word.eq_ignore_ascii_case("limit"))
}

/// The leading keyword of `sql`, skipping leading line/block comments + whitespace.
pub fn first_keyword(sql: &str) -> String {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest
                .split_once('\n')
                .map_or("", |(_, after)| after)
                .trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            match rest.split_once("*/") {
                Some((_, after)) => s = after.trim_start(),
                None => return String::new(),
            }
        } else {
            break;
        }
    }
    s.chars().take_while(|c| c.is_ascii_alphabetic()).collect()
}

/// The identifier immediately before `cursor` (byte offset): the token a
/// completion replaces. Empty when the cursor isn't right after an identifier.
pub fn word_prefix(content: &str, cursor: usize) -> &str {
    let end = cursor.min(content.len());
    let bytes = content.as_bytes();
    let mut start = end;
    while start > 0 && is_ident_continue(bytes[start - 1]) {
        start -= 1;
    }
    &content[start..end]
}

/// True for a reserved SQL keyword (case-insensitive).
fn is_keyword(word: &str) -> bool {
    KEYWORDS.contains(&word.to_ascii_lowercase().as_str())
}

// --- schema-aware completion ---

/// Where the cursor sits in a statement, deciding what the editor suggests. The
/// completion provider in `editor.rs` maps each variant onto schema candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContext {
    /// Right after `qualifier.`: suggest the columns of that table or alias.
    Dot { qualifier: String },
    /// After FROM/INTO/UPDATE: suggest table names.
    Table,
    /// After JOIN: suggest table names, but lead with auto-`JOIN` completions
    /// (`table alias ON …`) synthesised from the connection's foreign-key graph
    /// for tables related to one already in the statement. See `editor::join_items`.
    Join,
    /// Inside an expression (SELECT/WHERE/ON/…): suggest columns, then keywords.
    Column,
    /// Statement start or anywhere else: suggest keywords, then tables.
    Keyword,
}

/// The byte range of the `;`-delimited statement containing `cursor`, with string
/// and comment boundaries respected so a `;` inside a literal or comment never
/// splits. Completion scopes its analysis to this one statement.
fn statement_bounds(content: &str, cursor: usize) -> Range<usize> {
    let b = content.as_bytes();
    let n = b.len();
    let cur = cursor.min(n);
    let mut start = 0;
    let mut end = n;
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'-' && i + 1 < n && b[i + 1] == b'-' {
            i += 2;
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        if c == b'\'' || c == b'"' {
            let quote = c;
            i += 1;
            while i < n && b[i] != quote {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        if c == b';' {
            if i < cur {
                start = i + 1;
            } else {
                end = i;
                break;
            }
        }
        i += 1;
    }
    start..end
}

/// A coarse lexical atom used only by completion's clause parsing; finer than
/// raw bytes, cruder than [`tokenize`]: identifiers/keywords collapse to `Word`
/// (carrying their byte range, so diagnostics can point at the exact token),
/// `.` and `,` are kept (they separate qualifiers and table lists), everything
/// else is `Other`. Strings and comments are skipped entirely.
enum Atom {
    Word(String, Range<usize>),
    Dot,
    Comma,
    Other,
}

fn atomize(s: &str) -> Vec<Atom> {
    let b = s.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'-' && i + 1 < n && b[i + 1] == b'-' {
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        if c == b'\'' || c == b'"' {
            let quote = c;
            i += 1;
            while i < n && b[i] != quote {
                i += 1;
            }
            i = (i + 1).min(n);
            out.push(Atom::Other);
            continue;
        }
        // Backtick-quoted identifier (MySQL): keep the inner name as a `Word` (with
        // its inner range) so `` `schema`.`table` `` parses as a qualified name — the
        // backticks aren't tokens that could break the schema/table detection.
        if c == b'`' {
            let start = i + 1;
            i += 1;
            while i < n && b[i] != b'`' {
                i += 1;
            }
            let end = i;
            i = (i + 1).min(n);
            if end > start {
                out.push(Atom::Word(s[start..end].to_string(), start..end));
            }
            continue;
        }
        if is_ident_start(c) {
            let start = i;
            while i < n && is_ident_continue(b[i]) {
                i += 1;
            }
            out.push(Atom::Word(s[start..i].to_string(), start..i));
            continue;
        }
        match c {
            b'.' => out.push(Atom::Dot),
            b',' => out.push(Atom::Comma),
            _ if c.is_ascii_whitespace() => {}
            _ => out.push(Atom::Other),
        }
        i += 1;
    }
    out
}

/// Classify the completion context at `cursor` (a byte offset into `content`),
/// scoped to the statement under the cursor.
pub fn analyze(content: &str, cursor: usize) -> CompletionContext {
    let stmt = statement_bounds(content, cursor);
    let local = cursor.min(content.len()) - stmt.start;
    let s = &content[stmt];
    let prefix = word_prefix(content, cursor);
    let before = &s[..local - prefix.len()];

    // `qualifier.` (optionally followed by the word being typed) → table columns.
    let trimmed = before.trim_end();
    if let Some(head) = trimmed.strip_suffix('.') {
        let qualifier = word_prefix(head, head.len());
        if !qualifier.is_empty() {
            return CompletionContext::Dot {
                qualifier: qualifier.to_string(),
            };
        }
    }

    // Otherwise the nearest *clause* keyword decides what fits here. Modifiers
    // (AS, AND, OR, NOT, DISTINCT, …) are transparent: `SELECT a AS x, col|` is
    // still a column position governed by SELECT, and `WHERE a AND col|` by WHERE.
    let last_clause = tokenize(before)
        .iter()
        .rev()
        .filter(|(_, style)| *style == TokenStyle::Keyword)
        .map(|(r, _)| before[r.clone()].to_ascii_lowercase())
        .find(|kw| {
            matches!(
                kw.as_str(),
                "from"
                    | "join"
                    | "into"
                    | "update"
                    | "table"
                    | "select"
                    | "where"
                    | "on"
                    | "set"
                    | "having"
                    | "by"
                    | "returning"
                    | "values"
                    | "using"
            )
        });

    match last_clause.as_deref() {
        Some("join") => CompletionContext::Join,
        Some("from" | "into" | "update" | "table") => CompletionContext::Table,
        Some(_) => CompletionContext::Column,
        None => CompletionContext::Keyword,
    }
}

/// Resolve the `(alias-or-name → real table)` references in the statement at
/// `cursor`, scanning its whole FROM/JOIN/UPDATE/INTO clause (which may sit after
/// the cursor). Scopes column suggestions and resolves `alias.` completions. The
/// alias key is lower-cased; the table name keeps its original case.
pub fn referenced_tables_at(content: &str, cursor: usize) -> Vec<(String, String)> {
    let stmt = statement_bounds(content, cursor);
    referenced_tables(&content[stmt])
}

fn referenced_tables(stmt: &str) -> Vec<(String, String)> {
    referenced_tables_ranged(stmt)
        .into_iter()
        .map(|(alias, _, table, _)| (alias, table))
        .collect()
}

/// Like [`referenced_tables`], but each entry also carries the schema qualifier (the
/// segment before the table in `schema.table`, `None` when unqualified) and the byte
/// range of the table-name token. Backs the diagnostics pass, which underlines an
/// unknown table at exactly that span and skips one qualified by an unknown schema.
fn referenced_tables_ranged(stmt: &str) -> Vec<(String, Option<String>, String, Range<usize>)> {
    let atoms = atomize(stmt);
    let n = atoms.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let introduces = matches!(&atoms[i], Atom::Word(w, _)
            if matches!(w.to_ascii_lowercase().as_str(), "from" | "join" | "into" | "update" | "table"));
        if !introduces {
            i += 1;
            continue;
        }
        i += 1;
        // A comma-separated table list (FROM a, b); JOIN/UPDATE/INTO carry one.
        loop {
            while i < n && matches!(atoms[i], Atom::Other) {
                i += 1;
            }
            let Some(Atom::Word(first, first_range)) = atoms.get(i) else {
                break;
            };
            if is_keyword(first) {
                break;
            }
            // `schema.table`: the trailing word is the table name (and its range);
            // the segment before it is the schema qualifier.
            let mut table = first.clone();
            let mut table_range = first_range.clone();
            let mut schema: Option<String> = None;
            i += 1;
            while matches!(atoms.get(i), Some(Atom::Dot)) {
                if let Some(Atom::Word(part, part_range)) = atoms.get(i + 1) {
                    schema = Some(table.clone());
                    table = part.clone();
                    table_range = part_range.clone();
                    i += 2;
                } else {
                    break;
                }
            }
            // Optional alias: `AS x` or a bare following identifier.
            let mut alias = None;
            if let Some(Atom::Word(w, _)) = atoms.get(i)
                && w.eq_ignore_ascii_case("as")
            {
                i += 1;
            }
            if let Some(Atom::Word(w, _)) = atoms.get(i)
                && !is_keyword(w)
            {
                alias = Some(w.clone());
                i += 1;
            }
            let key = alias.unwrap_or_else(|| table.clone()).to_lowercase();
            out.push((key, schema, table, table_range));
            if matches!(atoms.get(i), Some(Atom::Comma)) {
                i += 1;
                continue;
            }
            break;
        }
    }
    out
}

// --- schema-aware diagnostics ---

/// A read-only view of the connection's catalog, supplied by the UI so the
/// diagnostics pass stays domain-agnostic (it never touches `red-core` or the
/// completion index directly). Names are compared lower-cased.
pub trait SchemaView {
    /// Whether a table/view named `table_lower` exists in the catalog skeleton
    /// (always loaded, so a miss is a genuine "unknown table", not a lazy gap).
    fn has_table(&self, table_lower: &str) -> bool;
    /// The lower-cased column names of `table_lower` once its detail is loaded, or
    /// `None` when it isn't — in which case column checks for that table are skipped
    /// (a not-yet-expanded table must never read as "unknown column").
    fn columns(&self, table_lower: &str) -> Option<&HashSet<String>>;
    /// Whether `schema_lower` is a known namespace (database/schema). A table
    /// qualified by an *unknown* schema (a cross-database reference we haven't
    /// loaded) is left unvalidated rather than flagged as unknown.
    fn has_schema(&self, schema_lower: &str) -> bool;
}

/// One editor diagnostic: the byte range to underline and the message to show on
/// hover. Byte offsets are absolute into the whole editor buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range<usize>,
    pub message: String,
}

/// Validate every statement in `content` against `schema`, returning a diagnostic
/// per unknown table or column. Conservative by design — it only flags what it can
/// resolve unambiguously, so a false "unknown" never fires:
///
/// * an unknown table in a FROM/JOIN/INTO/UPDATE position (CTE names excluded);
/// * an unknown *qualified* column `alias.col` when the alias resolves to a table
///   whose columns are loaded;
/// * an unknown *unqualified* column, but only when the statement references exactly
///   one table (so there's no ambiguity about which table owns it) and that table's
///   columns are loaded.
pub fn diagnostics(content: &str, schema: &dyn SchemaView) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for srange in statement_ranges(content) {
        diagnose_statement(&content[srange.clone()], srange.start, schema, &mut out);
    }
    out
}

/// The 0-based line of each non-empty statement's first line — the editor's gutter
/// run markers. A whitespace/comment-only statement gets none; leading blank lines
/// are skipped so the marker sits on the line that actually holds SQL.
pub fn statement_start_lines(content: &str) -> Vec<usize> {
    statement_ranges(content)
        .into_iter()
        .filter(|r| !is_blank(&content[r.clone()]))
        .map(|r| {
            let stmt = &content[r.clone()];
            let lead = stmt.len() - stmt.trim_start().len();
            let start = r.start + lead;
            content[..start].bytes().filter(|&b| b == b'\n').count()
        })
        .collect()
}

/// The byte offset at the start of 0-based `line` in `content` (clamped to the
/// end). Backs "run the statement whose gutter marker was clicked".
pub fn line_start_offset(content: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut seen = 0;
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            seen += 1;
            if seen == line {
                return i + 1;
            }
        }
    }
    content.len()
}

/// The byte ranges of the `;`-delimited statements in `content`, using the same
/// string/comment-aware boundary rules as [`statement_bounds`].
fn statement_ranges(content: &str) -> Vec<Range<usize>> {
    let b = content.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'-' && i + 1 < n && b[i + 1] == b'-' {
            i += 2;
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        if c == b'\'' || c == b'"' {
            let quote = c;
            i += 1;
            while i < n && b[i] != quote {
                i += 1;
            }
            i = (i + 1).min(n);
            continue;
        }
        if c == b';' {
            out.push(start..i);
            start = i + 1;
        }
        i += 1;
    }
    out.push(start..n);
    out
}

/// The lower-cased names of the CTEs a statement defines (`WITH name AS ( … )`,
/// including the comma-separated tail). Collected so a reference to a CTE isn't
/// flagged as an unknown table. Over-collecting only suppresses diagnostics, so the
/// loose `Word AS (` match is safe.
fn cte_names(stmt: &str) -> HashSet<String> {
    let atoms = atomize(stmt);
    let mut names = HashSet::new();
    for w in atoms.windows(2) {
        if let (Atom::Word(name, _), Atom::Word(kw, _)) = (&w[0], &w[1]) {
            // `name AS` where `name` isn't itself a keyword: a CTE (or table alias)
            // binding. Cheap and safe to treat both the same here.
            if kw.eq_ignore_ascii_case("as") && !is_keyword(name) {
                names.insert(name.to_lowercase());
            }
        }
    }
    names
}

/// The lower-cased names a statement binds via `... AS name` — output-column aliases
/// (`SELECT total... AS total`) and table aliases (`FROM foo AS f`) both match, since
/// distinguishing them isn't needed here: the unqualified-column check just needs to
/// know a bare word is a defined name, not a schema column, wherever else it's used
/// (e.g. `ORDER BY total`). Over-collecting only suppresses diagnostics, so the loose
/// match is safe.
fn as_aliases(stmt: &str) -> HashSet<String> {
    let atoms = atomize(stmt);
    let mut names = HashSet::new();
    for w in atoms.windows(2) {
        if let (Atom::Word(kw, _), Atom::Word(name, _)) = (&w[0], &w[1])
            && kw.eq_ignore_ascii_case("as")
            && !is_keyword(name)
        {
            names.insert(name.to_lowercase());
        }
    }
    names
}

/// Whether the gap `stmt[from..to]` between two identifier tokens is a member-access
/// `.` — how the token-level column check spots `qualifier.column` without the
/// tokenizer emitting a dot token. Tolerates the backticks around a quoted qualifier
/// (`` `a`.`b` ``), whose tokens exclude the quotes, so the gap is `` `.` ``.
fn gap_is_dot(stmt: &str, from: usize, to: usize) -> bool {
    from <= to
        && stmt
            .get(from..to)
            .map(|g| g.trim_matches(|c: char| c.is_whitespace() || c == '`'))
            == Some(".")
}

fn diagnose_statement(stmt: &str, base: usize, schema: &dyn SchemaView, out: &mut Vec<Diagnostic>) {
    let ctes = cte_names(stmt);
    let aliases = as_aliases(stmt);
    let refs = referenced_tables_ranged(stmt);

    // 1. Unknown tables in FROM/JOIN/INTO/UPDATE positions.
    for (_, qualifier, table, range) in &refs {
        let low = table.to_lowercase();
        if ctes.contains(&low) || schema.has_table(&low) {
            continue;
        }
        // Qualified by a schema we don't know (a cross-database reference we haven't
        // loaded) → can't validate the table, so leave it alone rather than flag it.
        if let Some(q) = qualifier
            && !schema.has_schema(&q.to_lowercase())
        {
            continue;
        }
        out.push(Diagnostic {
            range: base + range.start..base + range.end,
            message: format!("Unknown table \u{201c}{table}\u{201d}"),
        });
    }

    // Alias/name → table (lower-cased), for resolving qualified columns.
    let alias_map: HashMap<String, String> = refs
        .iter()
        .map(|(alias, _, table, _)| (alias.clone(), table.to_lowercase()))
        .collect();
    // The single referenced table, and only if its columns are loaded: the gate for
    // checking *unqualified* columns without ambiguity.
    let single_table = match refs.as_slice() {
        [(_, _, table, _)] => {
            let low = table.to_lowercase();
            schema.columns(&low).is_some().then_some(low)
        }
        _ => None,
    };

    // 2/3. Column checks, walking the statement's identifier tokens (which carry
    // ranges) and reading the tiny gaps around them to spot `qualifier.column`.
    let toks = tokenize(stmt);
    for (idx, (range, style)) in toks.iter().enumerate() {
        if *style != TokenStyle::Identifier {
            continue;
        }
        let word = &stmt[range.clone()];
        let prev = idx.checked_sub(1).and_then(|j| toks.get(j));
        let next = toks.get(idx + 1);
        let qualified = prev.is_some_and(|(pr, ps)| {
            *ps == TokenStyle::Identifier && gap_is_dot(stmt, pr.end, range.start)
        });
        let is_qualifier = next.is_some_and(|(nr, ns)| {
            *ns == TokenStyle::Identifier && gap_is_dot(stmt, range.end, nr.start)
        });
        if is_qualifier {
            continue; // the left side of `x.y` — a table/alias, not a column
        }

        if qualified {
            // `qualifier.word`: resolve the qualifier to a table, check the column.
            #[allow(
                clippy::expect_used,
                reason = "qualified path implies a previous token"
            )]
            let (pr, _) = prev.expect("qualified implies a previous token");
            let q = stmt[pr.clone()].to_lowercase();
            let table = alias_map
                .get(&q)
                .cloned()
                .or_else(|| schema.has_table(&q).then(|| q.clone()));
            if let Some(table) = table
                && let Some(cols) = schema.columns(&table)
                && !cols.contains(&word.to_lowercase())
            {
                out.push(Diagnostic {
                    range: base + range.start..base + range.end,
                    message: format!("No column \u{201c}{word}\u{201d} on \u{201c}{table}\u{201d}"),
                });
            }
            continue;
        }

        // Unqualified column, only in the unambiguous single-table case.
        if let Some(table) = &single_table {
            let low = word.to_lowercase();
            // Skip the table's own name/alias, reserved words, and any `AS`-defined
            // alias (an output alias like `SELECT expr AS foo`, referenceable later
            // in `ORDER BY`/`GROUP BY`/`HAVING`).
            if is_keyword(word)
                || alias_map.contains_key(&low)
                || schema.has_table(&low)
                || aliases.contains(&low)
            {
                continue;
            }
            #[allow(
                clippy::expect_used,
                reason = "single_table implies its columns are loaded"
            )]
            let cols = schema
                .columns(table)
                .expect("single_table implies loaded columns");
            if !cols.contains(&low) {
                out.push(Diagnostic {
                    range: base + range.start..base + range.end,
                    message: format!("Unknown column \u{201c}{word}\u{201d}"),
                });
            }
        }
    }
}

/// Beautify a SQL string for the editor's Format action: re-indent (2 spaces),
/// upper-case keywords, and put each major clause on its own line. Multi-statement
/// input is handled per statement. A whitespace-only input is returned unchanged so
/// the action is a no-op on an empty editor.
///
/// SQL formatting is deceptively hard (subqueries, CASE, function args), so this
/// leans on the well-tested `sqlformat` crate rather than a hand-rolled pass; see
/// the dependency note in the workspace manifest.
pub fn format_sql(sql: &str) -> String {
    if sql.trim().is_empty() {
        return sql.to_string();
    }
    let options = sqlformat::FormatOptions {
        indent: sqlformat::Indent::Spaces(2),
        uppercase: true,
        ..Default::default()
    };
    sqlformat::format(sql, &sqlformat::QueryParams::None, options)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The formatter upper-cases keywords, breaks major clauses onto their own
    /// lines, and no-ops on blank input.
    #[test]
    fn format_sql_beautifies_and_no_ops_on_blank() {
        assert_eq!(format_sql("   \n  "), "   \n  ");
        let out = format_sql("select id, name from users where id = 1");
        assert!(out.contains("SELECT"), "keywords upper-cased: {out}");
        assert!(out.contains("\nFROM"), "FROM on its own line: {out}");
        assert!(out.contains("\nWHERE"), "WHERE on its own line: {out}");
        // Formatting is stable: a second pass changes nothing.
        assert_eq!(format_sql(&out), out);
    }

    /// Split a fixture on the `|` caret marker into `(content, cursor)`.
    fn at(src: &str) -> (String, usize) {
        let cursor = src.find('|').expect("fixture needs a | caret");
        (src.replace('|', ""), cursor)
    }

    fn ctx(src: &str) -> CompletionContext {
        let (content, cursor) = at(src);
        analyze(&content, cursor)
    }

    fn refs(src: &str) -> Vec<(String, String)> {
        let (content, cursor) = at(src);
        referenced_tables_at(&content, cursor)
    }

    #[test]
    fn auto_limit_appends_to_bare_select() {
        assert_eq!(
            auto_limit("SELECT * FROM users", 1000).as_deref(),
            Some("SELECT * FROM users LIMIT 1000")
        );
        // A trailing terminator is stripped before the LIMIT is appended.
        assert_eq!(
            auto_limit("select id from t ;", 50).as_deref(),
            Some("select id from t LIMIT 50")
        );
    }

    #[test]
    fn auto_limit_leaves_self_limited_and_non_selects_alone() {
        assert_eq!(auto_limit("SELECT * FROM t LIMIT 5", 1000), None);
        assert_eq!(
            auto_limit("WITH x AS (SELECT 1) SELECT * FROM x", 1000),
            None
        );
        assert_eq!(auto_limit("UPDATE t SET a = 1", 1000), None);
        // Disabled by a zero limit, and skipped for multi-statement batches.
        assert_eq!(auto_limit("SELECT * FROM t", 0), None);
        assert_eq!(auto_limit("SELECT 1; SELECT 2", 1000), None);
    }

    #[test]
    fn single_table_star_matches_plain_browses() {
        let t = |s: &str| single_table_star(s);
        // Bare, schema-qualified, and db.schema.table all resolve their table (+schema).
        assert_eq!(t("SELECT * FROM users"), Some((None, "users".into())));
        assert_eq!(
            t("select * from public.users"),
            Some((Some("public".into()), "users".into()))
        );
        assert_eq!(
            t("SELECT * FROM shop.public.orders"),
            Some((Some("public".into()), "orders".into()))
        );
        // Shape-preserving tails (WHERE / ORDER BY / LIMIT, the appended auto-limit),
        // an alias, and subqueries/commas *inside* a WHERE are all fine.
        assert_eq!(
            t("SELECT * FROM users WHERE id = 1 ORDER BY id LIMIT 1000"),
            Some((None, "users".into()))
        );
        assert_eq!(t("SELECT * FROM users u"), Some((None, "users".into())));
        assert_eq!(
            t("SELECT * FROM users AS u WHERE u.active"),
            Some((None, "users".into()))
        );
        assert_eq!(
            t("SELECT * FROM users WHERE id IN (1, 2, 3)"),
            Some((None, "users".into()))
        );
        assert_eq!(
            t("SELECT * FROM users WHERE tier_id IN (SELECT id FROM tiers)"),
            Some((None, "users".into()))
        );
        // A keyword hiding in a string literal / comment can't fool the scan.
        assert_eq!(
            t("SELECT * FROM users WHERE note = 'a join b'"),
            Some((None, "users".into()))
        );
        assert_eq!(
            t("SELECT * FROM users -- join orders\n WHERE id = 1"),
            Some((None, "users".into()))
        );
        assert_eq!(t("SELECT * FROM users ;"), Some((None, "users".into())));
        // Quoted identifiers (pg/sqlite `"…"`, MySQL backticks) resolve to the inner
        // name; RED's own browse SQL is fully quoted, and users copy it.
        assert_eq!(t(r#"SELECT * FROM "users""#), Some((None, "users".into())));
        assert_eq!(
            t(r#"SELECT * FROM "public"."users""#),
            Some((Some("public".into()), "users".into()))
        );
        assert_eq!(t("SELECT * FROM `users`"), Some((None, "users".into())));
        assert_eq!(
            t(r#"SELECT * FROM "users" WHERE "id" = 1"#),
            Some((None, "users".into()))
        );
    }

    #[test]
    fn single_table_star_rejects_non_single_table_shapes() {
        let no = |s: &str| assert_eq!(single_table_star(s), None, "should reject: {s}");
        // Projections: a missing PK/FK column would break keyset paging and the
        // reference-column join wrap, so only `SELECT *` qualifies.
        no("SELECT id, name FROM users");
        no("SELECT count(*) FROM users");
        no("SELECT DISTINCT * FROM users");
        // Joins, comma lists, and a FROM subquery aren't a single base table.
        no("SELECT * FROM users u JOIN tiers t ON u.tier_id = t.id");
        no("SELECT * FROM users, tiers");
        no("SELECT * FROM (SELECT * FROM users) x");
        // Top-level set operations change the result shape.
        no("SELECT * FROM users UNION SELECT * FROM admins");
        // A chained second statement, and non-SELECT statements.
        no("SELECT * FROM users; DROP TABLE users");
        no("UPDATE users SET active = 1");
        no("WITH x AS (SELECT 1) SELECT * FROM x");
        no("");
    }

    #[test]
    fn normalize_spaces_scrubs_nbsp_outside_literals() {
        // U+00A0 (Option+Space) between tokens becomes a normal space.
        assert_eq!(
            normalize_spaces("SELECT *\u{a0}FROM t").as_deref(),
            Some("SELECT * FROM t")
        );
        // Other non-ASCII whitespace (narrow/figure/ideographic spaces) too.
        assert_eq!(
            normalize_spaces("SELECT\u{202f}1,\u{2007}2,\u{3000}3").as_deref(),
            Some("SELECT 1, 2, 3")
        );
        // Plain ASCII whitespace is left untouched (nothing to normalize).
        assert_eq!(normalize_spaces("SELECT * FROM t\n WHERE a = 1"), None);
    }

    #[test]
    fn normalize_spaces_preserves_nbsp_inside_literals_and_comments() {
        // Inside a string literal an NBSP is data; keep it (and report no change).
        assert_eq!(normalize_spaces("SELECT 'a\u{a0}b' FROM t"), None);
        // Inside a quoted identifier likewise.
        assert_eq!(normalize_spaces("SELECT \"a\u{a0}b\" FROM t"), None);
        // Inside comments too; an NBSP outside is still scrubbed in the same pass.
        assert_eq!(
            normalize_spaces("SELECT 1 -- a\u{a0}b\nFROM\u{a0}t").as_deref(),
            Some("SELECT 1 -- a\u{a0}b\nFROM t")
        );
        // A doubled-quote escape doesn't end the literal early, so an NBSP after it
        // but still inside the string is preserved.
        assert_eq!(normalize_spaces("SELECT 'O''\u{a0}x' FROM t"), None);
    }

    #[test]
    fn classify_batch_uses_most_destructive_statement() {
        // A single statement classifies by its leading keyword.
        assert_eq!(classify("SELECT * FROM t"), StatementKind::Query);
        assert_eq!(classify("INSERT INTO t VALUES (1)"), StatementKind::Write);
        assert_eq!(classify("DROP TABLE t"), StatementKind::Destructive);
        // A multi-statement paste confirms on the destructive tail; the bug this
        // guards: a leading SELECT must not mask a trailing DROP.
        assert_eq!(
            classify("SELECT 1; DROP TABLE users"),
            StatementKind::Destructive
        );
        assert_eq!(
            classify("INSERT INTO t VALUES (1); SELECT 1"),
            StatementKind::Write
        );
        assert_eq!(classify("SELECT 1; SELECT 2"), StatementKind::Query);
        // A `;` inside a string or comment doesn't start a new statement.
        assert_eq!(
            classify("SELECT 'a; DROP TABLE t' AS x"),
            StatementKind::Query
        );
        assert_eq!(
            classify("SELECT 1 -- DROP TABLE t\n; SELECT 2"),
            StatementKind::Query
        );
        // Trailing terminator / empty statements are ignored.
        assert_eq!(classify("DELETE FROM t;"), StatementKind::Destructive);
    }

    #[test]
    fn is_read_only_allows_plain_reads() {
        assert!(is_read_only("SELECT * FROM users"));
        assert!(is_read_only("  select id from t where name = 'a'  "));
        assert!(is_read_only("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(is_read_only("EXPLAIN SELECT * FROM t"));
        assert!(is_read_only("VALUES (1), (2)"));
        assert!(is_read_only("SELECT * FROM t;")); // a single trailing terminator is fine
        // A write word appearing only inside a string literal or a quoted identifier
        // is blanked before the scan, so a legitimate read isn't rejected.
        assert!(is_read_only("SELECT 'delete me' AS note FROM t"));
        assert!(is_read_only("SELECT \"delete\" FROM t"));
        // `updated_at` must not match the `update` write token (whole-word only).
        assert!(is_read_only("SELECT updated_at FROM t"));
    }

    #[test]
    fn is_read_only_rejects_writes_and_side_effects() {
        // Plain writes / DDL never lead with a read keyword.
        assert!(!is_read_only("INSERT INTO t VALUES (1)"));
        assert!(!is_read_only("UPDATE t SET a = 1"));
        assert!(!is_read_only("DROP TABLE t"));
        // The hole the leading-keyword classifier misses: a data-modifying CTE and a
        // side-effecting function that both *lead* with a read keyword.
        assert!(!is_read_only(
            "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x"
        ));
        assert!(!is_read_only("SELECT lo_export(oid, '/tmp/x') FROM t"));
        assert!(!is_read_only("SELECT load_file('/etc/passwd')"));
        assert!(!is_read_only("SELECT * INTO other FROM t"));
        // A chained statement can smuggle a write past a leading read.
        assert!(!is_read_only("SELECT 1; DROP TABLE users"));
        // Nothing runnable.
        assert!(!is_read_only(""));
        assert!(!is_read_only("  -- just a note"));
    }

    #[test]
    fn statement_at_picks_the_caret_statement() {
        // Single statement: the whole buffer, wherever the caret sits.
        let (c, cur) = at("SELECT * FROM |users");
        assert_eq!(statement_at(&c, cur), "SELECT * FROM users");
        // Multi-statement: only the one under the caret, trimmed.
        let (c, cur) = at("SELECT 1;\nSELECT |2;\nSELECT 3");
        assert_eq!(statement_at(&c, cur), "SELECT 2");
        // Caret just past the final `;` (blank tail) falls back to the last real one.
        let (c, cur) = at("SELECT 1;\nSELECT 2;\n|");
        assert_eq!(statement_at(&c, cur), "SELECT 2");
        // A `;` inside a string isn't a boundary; the whole statement comes back.
        let (c, cur) = at("SELECT 'a; b' AS |x");
        assert_eq!(statement_at(&c, cur), "SELECT 'a; b' AS x");
    }

    #[test]
    fn is_blank_detects_nothing_runnable() {
        // Empty, whitespace, comments, and bare terminators have nothing to run.
        assert!(is_blank(""));
        assert!(is_blank("   \n\t"));
        assert!(is_blank(";"));
        assert!(is_blank("-- just a note"));
        assert!(is_blank("/* block */  ;\n"));
        // Any real statement (even paren-led, which has no leading keyword) is not.
        assert!(!is_blank("SELECT 1"));
        assert!(!is_blank("-- note\nSELECT 1"));
        assert!(!is_blank("(SELECT 1)"));
    }

    #[test]
    fn statement_count_ignores_empty_statements() {
        assert_eq!(statement_count("SELECT 1"), 1);
        assert_eq!(statement_count("SELECT 1; SELECT 2"), 2);
        // Trailing terminator / whitespace-only statements don't count.
        assert_eq!(statement_count("SELECT 1;"), 1);
        assert_eq!(statement_count("SELECT 1;  ;\n"), 1);
        // A `;` inside a literal stays within one statement.
        assert_eq!(statement_count("SELECT 'a; b'"), 1);
    }

    #[test]
    fn dot_is_member_access() {
        assert_eq!(
            ctx("SELECT u.|"),
            CompletionContext::Dot {
                qualifier: "u".into()
            }
        );
        assert_eq!(
            ctx("SELECT users.na|"),
            CompletionContext::Dot {
                qualifier: "users".into()
            }
        );
    }

    #[test]
    fn keyword_decides_table_vs_column() {
        assert_eq!(ctx("SELECT * FROM |"), CompletionContext::Table);
        assert_eq!(ctx("SELECT * FROM us|"), CompletionContext::Table);
        assert_eq!(ctx("SELECT na| FROM users"), CompletionContext::Column);
        assert_eq!(
            ctx("SELECT * FROM users WHERE i|"),
            CompletionContext::Column
        );
        assert_eq!(
            ctx("SELECT * FROM a JOIN b ON a.x = b.|"),
            CompletionContext::Dot {
                qualifier: "b".into()
            }
        );
        // A bare post-JOIN position is its own context (auto-JOIN completions),
        // distinct from a plain FROM table position.
        assert_eq!(ctx("SELECT * FROM a JOIN |"), CompletionContext::Join);
        assert_eq!(ctx("SELECT * FROM a JOIN cu|"), CompletionContext::Join);
        // Past the ON keyword it's an expression again, not a table position.
        assert_eq!(
            ctx("SELECT * FROM a JOIN b ON |"),
            CompletionContext::Column
        );
    }

    #[test]
    fn statement_start_is_keyword() {
        assert_eq!(ctx("sel|"), CompletionContext::Keyword);
        assert_eq!(ctx("|"), CompletionContext::Keyword);
    }

    #[test]
    fn context_scopes_to_cursor_statement() {
        // The trailing FROM belongs to a later statement; the cursor's own
        // statement only has SELECT, so this is a column position.
        assert_eq!(
            ctx("SELECT col| ; SELECT * FROM t"),
            CompletionContext::Column
        );
        // The leading statement's keywords must not leak past the `;`.
        assert_eq!(ctx("SELECT 1; sel|"), CompletionContext::Keyword);
    }

    #[test]
    fn resolves_aliases_and_plain_names() {
        assert_eq!(
            refs("SELECT * FROM users u WHERE u.|"),
            vec![("u".into(), "users".into())]
        );
        assert_eq!(
            refs("SELECT * FROM users AS u, orders o WHERE |"),
            vec![("u".into(), "users".into()), ("o".into(), "orders".into()),]
        );
        assert_eq!(
            refs("SELECT * FROM main.users WHERE |"),
            vec![("users".into(), "users".into())]
        );
        assert_eq!(
            refs("SELECT a.x, b.y FROM a JOIN b ON a.id = b.a_id WHERE |"),
            vec![("a".into(), "a".into()), ("b".into(), "b".into())]
        );
    }

    #[test]
    fn from_clause_after_cursor_still_resolves() {
        // Aliases declared after the cursor still scope a `qualifier.` completion.
        assert_eq!(
            refs("SELECT u.| FROM users u"),
            vec![("u".into(), "users".into())]
        );
    }

    #[test]
    fn semicolon_in_string_does_not_split() {
        let (content, cursor) = at("SELECT ';' AS s, na| FROM users");
        assert_eq!(analyze(&content, cursor), CompletionContext::Column);
        assert_eq!(
            referenced_tables_at(&content, cursor),
            vec![("users".into(), "users".into())]
        );
    }

    // --- schema-aware diagnostics (Phase B) ---

    /// A test catalog: `users(id, name, email)` + `orders(id, customer_id)`, both
    /// with columns loaded. Any other table is unknown; a table added via
    /// `with_unloaded` exists but has no columns (detail not loaded yet).
    struct TestSchema {
        tables: HashSet<String>,
        schemas: HashSet<String>,
        columns: HashMap<String, HashSet<String>>,
    }

    impl TestSchema {
        fn new() -> Self {
            let cols = |names: &[&str]| names.iter().map(|s| s.to_string()).collect();
            let mut columns = HashMap::new();
            columns.insert("users".to_string(), cols(&["id", "name", "email"]));
            columns.insert("orders".to_string(), cols(&["id", "customer_id"]));
            Self {
                tables: ["users", "orders"].iter().map(|s| s.to_string()).collect(),
                schemas: ["main"].iter().map(|s| s.to_string()).collect(),
                columns,
            }
        }
        /// Add a table whose detail isn't loaded (exists, but no column info).
        fn with_unloaded(mut self, table: &str) -> Self {
            self.tables.insert(table.to_string());
            self
        }
    }

    impl SchemaView for TestSchema {
        fn has_table(&self, table_lower: &str) -> bool {
            self.tables.contains(table_lower)
        }
        fn columns(&self, table_lower: &str) -> Option<&HashSet<String>> {
            self.columns.get(table_lower)
        }
        fn has_schema(&self, schema_lower: &str) -> bool {
            self.schemas.contains(schema_lower)
        }
    }

    /// The `(underlined text, message)` pairs a diagnostics pass produces.
    fn diag_pairs(content: &str, schema: &dyn SchemaView) -> Vec<(String, String)> {
        diagnostics(content, schema)
            .into_iter()
            .map(|d| (content[d.range].to_string(), d.message))
            .collect()
    }

    #[test]
    fn flags_unknown_table_only() {
        let s = TestSchema::new();
        assert!(diag_pairs("SELECT * FROM users", &s).is_empty());
        let d = diag_pairs("SELECT * FROM userz", &s);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0, "userz");
        assert!(d[0].1.contains("Unknown table"), "{}", d[0].1);
    }

    #[test]
    fn flags_unknown_table_in_join_using_its_own_range() {
        let s = TestSchema::new();
        // Only the bad table underlines; the good one and the aliases are clean.
        let d = diag_pairs(
            "SELECT * FROM users u JOIN ordrs o ON u.id = o.customer_id",
            &s,
        );
        assert_eq!(
            d.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            ["ordrs"]
        );
    }

    #[test]
    fn flags_unknown_qualified_column() {
        let s = TestSchema::new();
        assert!(diag_pairs("SELECT u.email FROM users u", &s).is_empty());
        let d = diag_pairs("SELECT u.emial FROM users u", &s);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0, "emial");
        assert!(d[0].1.contains("No column"), "{}", d[0].1);
    }

    #[test]
    fn flags_unknown_unqualified_column_single_table_only() {
        let s = TestSchema::new();
        // Single table: an unknown bare column is flagged, a real one is not.
        let d = diag_pairs("SELECT emial FROM users", &s);
        assert_eq!(
            d.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            ["emial"]
        );
        assert!(diag_pairs("SELECT email FROM users", &s).is_empty());
        // Two tables: bare columns are ambiguous, so none are flagged (no false
        // positives) — only the qualified path and table existence still apply.
        assert!(diag_pairs("SELECT emial FROM users, orders", &s).is_empty());
    }

    #[test]
    fn unqualified_check_excludes_aliases_keywords_and_functions() {
        let s = TestSchema::new();
        // The table alias, an AS-defined output alias, a keyword, and a function
        // call must never read as unknown columns.
        assert!(diag_pairs("SELECT u.id FROM users u", &s).is_empty());
        assert!(diag_pairs("SELECT email AS addr FROM users", &s).is_empty());
        assert!(diag_pairs("SELECT count(email) FROM users", &s).is_empty());
    }

    #[test]
    fn order_by_group_by_resolve_select_list_aliases() {
        let s = TestSchema::new();
        // A SELECT-list output alias referenced later in the same statement (ORDER
        // BY, GROUP BY, HAVING) must not read as an unknown column.
        assert!(
            diag_pairs(
                "SELECT name, COUNT(*) AS total FROM users GROUP BY name ORDER BY total DESC",
                &s
            )
            .is_empty()
        );
        // A genuinely unknown name in ORDER BY is still flagged.
        assert_eq!(
            diag_pairs(
                "SELECT name, COUNT(*) AS total FROM users ORDER BY bogus",
                &s
            )
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>(),
            ["bogus"]
        );
    }

    #[test]
    fn skips_column_checks_when_detail_not_loaded() {
        // `events` exists but its columns aren't loaded: no column may be flagged,
        // and the table itself is not "unknown".
        let s = TestSchema::new().with_unloaded("events");
        assert!(diag_pairs("SELECT anything FROM events", &s).is_empty());
        assert!(diag_pairs("SELECT e.whatever FROM events e", &s).is_empty());
    }

    #[test]
    fn cte_name_is_not_an_unknown_table() {
        let s = TestSchema::new();
        // The CTE `recent` is defined in-statement, so a reference to it is fine.
        let sql = "WITH recent AS (SELECT * FROM orders) SELECT * FROM recent";
        assert!(diag_pairs(sql, &s).is_empty());
    }

    #[test]
    fn backtick_quoted_identifiers_resolve() {
        let s = TestSchema::new();
        // MySQL `` `schema`.`table` `` must resolve — the database name was being
        // mis-read as the table (an "unknown table" under the backticked db).
        assert!(diag_pairs("SELECT * FROM `orders`", &s).is_empty());
        assert!(diag_pairs("SELECT * FROM `main`.`orders`", &s).is_empty());
        // A qualified column through backticks resolves too (no false unknown column).
        assert!(diag_pairs("SELECT `o`.`customer_id` FROM `orders` `o`", &s).is_empty());
        // A genuinely unknown backticked table still flags — on its inner name only.
        let d = diag_pairs("SELECT * FROM `main`.`ghost`", &s);
        assert_eq!(
            d.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            ["ghost"]
        );
    }

    #[test]
    fn table_in_unknown_schema_is_not_flagged() {
        let s = TestSchema::new();
        // `main` is a known namespace, so an unknown table there is flagged…
        assert_eq!(
            diag_pairs("SELECT * FROM main.ghost", &s)
                .iter()
                .map(|(t, _)| t.as_str())
                .collect::<Vec<_>>(),
            ["ghost"]
        );
        // …but a table in a database we haven't loaded is left alone (can't validate).
        assert!(diag_pairs("SELECT * FROM other_db.ghost", &s).is_empty());
        assert!(diag_pairs("SELECT * FROM `other_db`.`ghost`", &s).is_empty());
    }

    #[test]
    fn diagnostics_are_scoped_per_statement() {
        let s = TestSchema::new();
        // The unknown table in the second statement gets its absolute range.
        let d = diag_pairs("SELECT * FROM users;\nSELECT * FROM ghost", &s);
        assert_eq!(
            d.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            ["ghost"]
        );
    }

    // --- gutter run markers (Phase D) ---

    #[test]
    fn statement_start_lines_marks_each_statements_first_code_line() {
        // Two statements: markers land on line 0 and the SELECT after the `;`.
        assert_eq!(statement_start_lines("SELECT 1;\nSELECT 2"), vec![0, 1]);
        // A leading blank line is skipped so the marker sits on the SQL line.
        assert_eq!(statement_start_lines("\n\nSELECT 1"), vec![2]);
        // A blank/comment-only trailing statement contributes no marker.
        assert_eq!(statement_start_lines("SELECT 1;\n-- tail"), vec![0]);
        assert_eq!(statement_start_lines("   \n"), Vec::<usize>::new());
    }

    #[test]
    fn functions_are_engine_scoped() {
        let pg = functions_for(DbKind::Postgres);
        let my = functions_for(DbKind::Mysql);
        let sl = functions_for(DbKind::Sqlite);
        let has = |list: &[(&str, &str, &str)], name: &str| list.iter().any(|(n, _, _)| *n == name);

        // Postgres-only aggregate vs MySQL/SQLite-only aggregate — never crossed.
        assert!(has(&pg, "string_agg") && !has(&pg, "group_concat"));
        assert!(has(&my, "group_concat") && !has(&my, "string_agg"));
        assert!(has(&sl, "group_concat") && !has(&sl, "string_agg"));
        // `ifnull` is MySQL/SQLite/ClickHouse; Postgres uses `coalesce`.
        assert!(!has(&pg, "ifnull") && has(&my, "ifnull"));
        // Portable functions are on every engine.
        for list in [&pg, &my, &sl] {
            assert!(has(list, "concat") && has(list, "coalesce") && has(list, "count"));
        }
        // SQLite has no `left`/`lpad`/`reverse` by those names.
        assert!(!has(&sl, "left") && !has(&sl, "lpad") && !has(&sl, "reverse"));
    }

    #[test]
    fn line_start_offset_locates_line_starts() {
        let content = "abc\ndef\nghi";
        assert_eq!(line_start_offset(content, 0), 0);
        assert_eq!(line_start_offset(content, 1), 4);
        assert_eq!(line_start_offset(content, 2), 8);
        // Past the end clamps to the buffer length.
        assert_eq!(line_start_offset(content, 9), content.len());
    }
}
