//! Split a SQL script into individual statements on top-level `;`.
//!
//! Every engine's `Command::Execute` runs **one** statement (rusqlite `execute`,
//! tokio-postgres `execute`, a single `query_drop`), so a multi-statement seed
//! script (the whole point of `red exec -f seed.sql`) has to be split client
//! side and sent one statement at a time. This is a lexer, not a parser: it walks
//! the bytes tracking whether it's inside a string literal, quoted identifier,
//! line/block comment, or a Postgres dollar-quoted body, and only breaks on a `;`
//! seen at top level. Good enough for hand-written and tool-exported seed scripts;
//! it is not a full SQL grammar.

/// Break `sql` into trimmed, non-empty statements on each top-level `;`. A `;`
/// inside a quote/comment/dollar-body is not a separator. A trailing statement
/// without a final `;` is included.
pub fn split_statements(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < n {
        match b[i] {
            // String literals and quoted identifiers. Backtick is MySQL's
            // identifier quote; single/double are SQL string / identifier.
            q @ (b'\'' | b'"' | b'`') => i = skip_quoted(b, i, q),
            // `-- line comment` to end of line.
            b'-' if i + 1 < n && b[i + 1] == b'-' => i = skip_line_comment(b, i),
            // `/* block comment */`.
            b'/' if i + 1 < n && b[i + 1] == b'*' => i = skip_block_comment(b, i),
            // Postgres dollar-quoted body (`$$…$$` / `$tag$…$tag$`). Falls through
            // to a normal byte when the `$` isn't actually a dollar-quote opener
            // (e.g. a `$1` positional parameter).
            b'$' => match dollar_quote_end(b, i) {
                Some(end) => i = end,
                None => i += 1,
            },
            b';' => {
                push_trimmed(sql, start, i, &mut out);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    push_trimmed(sql, start, n, &mut out);
    out
}

/// Push `sql[start..end]` trimmed to `out` when it holds anything non-blank.
fn push_trimmed(sql: &str, start: usize, end: usize, out: &mut Vec<String>) {
    let stmt = sql[start..end].trim();
    if !stmt.is_empty() {
        out.push(stmt.to_string());
    }
}

/// Index just past the closing quote of the literal/identifier opened at `i`
/// (whose quote char is `q`). Handles the doubled-quote escape (`''`, `""`,
/// ` `` `) and, for string quotes only, a backslash escape (`\'`). An unterminated
/// quote consumes to end-of-input.
fn skip_quoted(b: &[u8], i: usize, q: u8) -> usize {
    let n = b.len();
    let mut j = i + 1;
    while j < n {
        if b[j] == q {
            // A doubled quote is an escaped quote, not the terminator.
            if j + 1 < n && b[j + 1] == q {
                j += 2;
                continue;
            }
            return j + 1;
        }
        // Backslash escapes apply inside '…' / "…" (MySQL), never in `…`.
        if b[j] == b'\\' && q != b'`' && j + 1 < n {
            j += 2;
            continue;
        }
        j += 1;
    }
    n
}

/// Index of the newline ending the `--` comment opened at `i` (or end-of-input).
/// The newline itself is left for the main loop to step over.
fn skip_line_comment(b: &[u8], i: usize) -> usize {
    let n = b.len();
    let mut j = i + 2;
    while j < n && b[j] != b'\n' {
        j += 1;
    }
    j
}

/// Index just past the `*/` closing the block comment opened at `i` (or
/// end-of-input if unterminated).
fn skip_block_comment(b: &[u8], i: usize) -> usize {
    let n = b.len();
    let mut j = i + 2;
    while j + 1 < n {
        if b[j] == b'*' && b[j + 1] == b'/' {
            return j + 2;
        }
        j += 1;
    }
    n
}

/// If `i` opens a Postgres dollar-quoted string (`$$` or `$tag$`), return the
/// index just past its matching close tag (or end-of-input if unterminated).
/// Returns `None` when `i` is a lone `$` or a `$1`-style parameter, so the caller
/// treats it as an ordinary byte.
fn dollar_quote_end(b: &[u8], i: usize) -> Option<usize> {
    let n = b.len();
    // Read the tag: `$` (alnum|_)* `$`.
    let mut j = i + 1;
    while j < n && (b[j] == b'_' || b[j].is_ascii_alphanumeric()) {
        j += 1;
    }
    if j >= n || b[j] != b'$' {
        return None;
    }
    let tag = &b[i..=j]; // e.g. `$$` or `$body$`
    let mut k = j + 1;
    while k + tag.len() <= n {
        if &b[k..k + tag.len()] == tag {
            return Some(k + tag.len());
        }
        k += 1;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::split_statements;

    #[test]
    fn splits_plain_statements() {
        let s = split_statements("SELECT 1; SELECT 2 ;\nSELECT 3");
        assert_eq!(s, vec!["SELECT 1", "SELECT 2", "SELECT 3"]);
    }

    #[test]
    fn ignores_blank_and_trailing_semicolons() {
        let s = split_statements(";; SELECT 1 ;; ;");
        assert_eq!(s, vec!["SELECT 1"]);
    }

    #[test]
    fn keeps_semicolons_inside_string_literals() {
        let s = split_statements("INSERT INTO t VALUES ('a; b'); SELECT 1");
        assert_eq!(s, vec!["INSERT INTO t VALUES ('a; b')", "SELECT 1"]);
    }

    #[test]
    fn handles_doubled_and_backslash_quote_escapes() {
        let s = split_statements("SELECT 'it''s; fine'; SELECT 'a\\'; b'");
        assert_eq!(s, vec!["SELECT 'it''s; fine'", "SELECT 'a\\'; b'"]);
    }

    #[test]
    fn ignores_semicolons_in_comments() {
        // The `;`s inside the comments must not split; comment text is retained in
        // the statement (harmless; the engine ignores it), so we assert the count
        // and that the trailing SQL survived rather than exact strings.
        let s = split_statements("SELECT 1; -- a; b\nSELECT 2; /* c; d */ SELECT 3");
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], "SELECT 1");
        assert!(s[1].ends_with("SELECT 2"));
        assert!(s[2].ends_with("SELECT 3"));
    }

    #[test]
    fn keeps_dollar_quoted_body_intact() {
        let script = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN; RETURN 1; END; $$ LANGUAGE plpgsql; SELECT f()";
        let s = split_statements(script);
        assert_eq!(s.len(), 2);
        assert!(s[0].contains("BEGIN; RETURN 1; END;"));
        assert_eq!(s[1], "SELECT f()");
    }

    #[test]
    fn dollar_parameter_is_not_a_quote() {
        let s = split_statements("SELECT $1; SELECT $2");
        assert_eq!(s, vec!["SELECT $1", "SELECT $2"]);
    }

    #[test]
    fn backtick_identifier_protects_semicolon() {
        let s = split_statements("SELECT `we;ird` FROM t; SELECT 1");
        assert_eq!(s, vec!["SELECT `we;ird` FROM t", "SELECT 1"]);
    }
}
