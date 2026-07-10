//! RED's SQL domain logic for the editor: a hand-rolled tokenizer feeding Flint's
//! generic `Highlighter` seam, plus the keyword set and word-prefix helper the
//! completion provider uses. SQL-dialect knowledge stays here, behind the
//! highlighter seam, so Flint stays domain-free.

use std::ops::Range;

use flint::TokenStyle;

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
];

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
    /// After FROM/JOIN/INTO/UPDATE: suggest table names.
    Table,
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
/// raw bytes, cruder than [`tokenize`]: identifiers/keywords collapse to `Word`,
/// `.` and `,` are kept (they separate qualifiers and table lists), everything
/// else is `Other`. Strings and comments are skipped entirely.
enum Atom {
    Word(String),
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
        if is_ident_start(c) {
            let start = i;
            while i < n && is_ident_continue(b[i]) {
                i += 1;
            }
            out.push(Atom::Word(s[start..i].to_string()));
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
        Some("from" | "join" | "into" | "update" | "table") => CompletionContext::Table,
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
    let atoms = atomize(stmt);
    let n = atoms.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let introduces = matches!(&atoms[i], Atom::Word(w)
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
            let Some(Atom::Word(first)) = atoms.get(i) else {
                break;
            };
            if is_keyword(first) {
                break;
            }
            // `schema.table`: the trailing word is the table name.
            let mut table = first.clone();
            i += 1;
            while matches!(atoms.get(i), Some(Atom::Dot)) {
                if let Some(Atom::Word(part)) = atoms.get(i + 1) {
                    table = part.clone();
                    i += 2;
                } else {
                    break;
                }
            }
            // Optional alias: `AS x` or a bare following identifier.
            let mut alias = None;
            if let Some(Atom::Word(w)) = atoms.get(i) {
                if w.eq_ignore_ascii_case("as") {
                    i += 1;
                }
            }
            if let Some(Atom::Word(w)) = atoms.get(i) {
                if !is_keyword(w) {
                    alias = Some(w.clone());
                    i += 1;
                }
            }
            let key = alias.unwrap_or_else(|| table.clone()).to_lowercase();
            out.push((key, table));
            if matches!(atoms.get(i), Some(Atom::Comma)) {
                i += 1;
                continue;
            }
            break;
        }
    }
    out
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
}
