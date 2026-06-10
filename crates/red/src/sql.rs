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
