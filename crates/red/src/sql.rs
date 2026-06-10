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

        // String / quoted literal (' or ").
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

/// What kind of statement the editor is about to run — drives whether it streams
/// into the result grid, executes in a transaction, or first asks for confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    /// Row-returning — opens in the result grid.
    Query,
    /// A write/DDL that's safe to run after a plain transaction (INSERT, CREATE…).
    Write,
    /// A write that destroys or rewrites existing data — confirm before running.
    Destructive,
}

/// Classify a statement by its leading keyword (comments + whitespace skipped).
pub fn classify(sql: &str) -> StatementKind {
    match first_keyword(sql).to_ascii_uppercase().as_str() {
        "SELECT" | "WITH" | "PRAGMA" | "EXPLAIN" | "VALUES" => StatementKind::Query,
        "DROP" | "DELETE" | "UPDATE" | "ALTER" | "TRUNCATE" | "REPLACE" => {
            StatementKind::Destructive
        }
        _ => StatementKind::Write,
    }
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

/// The identifier immediately before `cursor` (byte offset) — the token a
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
    /// Right after `qualifier.` — suggest the columns of that table or alias.
    Dot { qualifier: String },
    /// After FROM/JOIN/INTO/UPDATE — suggest table names.
    Table,
    /// Inside an expression (SELECT/WHERE/ON/…) — suggest columns, then keywords.
    Column,
    /// Statement start or anywhere else — suggest keywords, then tables.
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

/// A coarse lexical atom used only by completion's clause parsing — finer than
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
        // A comma-separated table list (FROM a, b) — JOIN/UPDATE/INTO carry one.
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
            // `schema.table` — the trailing word is the table name.
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

#[cfg(test)]
mod tests {
    use super::*;

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
