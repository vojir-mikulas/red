//! Lexical analysis of SQL scripts: splitting them into runnable statements, and
//! the shared helpers every layer uses to reason about one.
//!
//! Every engine's `execute` runs exactly **one** statement (rusqlite `execute`,
//! tokio-postgres `execute`, a single `query_drop`), so a `;`-separated script has
//! to be broken up before it reaches a driver. This is a lexer, not a parser: it
//! walks the bytes tracking whether it's inside a string literal, quoted
//! identifier, line/block comment, or a Postgres dollar-quoted body, and only
//! breaks on a `;` seen at top level. Good enough for hand-written and
//! tool-exported scripts; it is not a full SQL grammar.
//!
//! It lives in `red-core` because every layer needs the *same* boundaries: the
//! service splits an `Execute` script into a transaction, the UI classifies each
//! statement to decide whether to confirm, and the CLI runs `red exec -f seed.sql`
//! statement by statement. Lexers that disagree would let a `;` inside a `$$` body
//! read as one statement to the confirm gate and two to the engine.
//!
//! The same argument applies to *what a statement means*, so [`risk`] hangs here
//! too: the confirm gate, the AI write gate, and the AI read gate must not drift
//! apart in what they consider dangerous. [`strip_noise`] and [`has_word`] are the
//! primitives all three reason with, so that a keyword inside a string literal is
//! invisible to every one of them rather than to only whichever copy was fixed
//! last.

pub mod preflight;
pub mod risk;

pub use preflight::count_preflight;
pub use risk::{
    Assessment, DANGEROUS_FNS, DropKind, MutateVerb, Risk, RiskLevel, WRITE_TOKENS, assess,
};

/// Break `sql` into its top-level `;`-delimited statements, each trimmed, with
/// blank and separator-only stretches dropped. A `;` inside a quote, a comment, or
/// a dollar-quoted body is not a separator, and a trailing statement without a
/// final `;` is included. Borrows; no allocation per statement.
///
/// A comment-only stretch is still a "statement" here (it isn't blank); callers
/// that care whether it's runnable check for a leading keyword.
pub fn split_statements(sql: &str) -> Vec<&str> {
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
fn push_trimmed<'a>(sql: &'a str, start: usize, end: usize, out: &mut Vec<&'a str>) {
    let stmt = sql[start..end].trim();
    if !stmt.is_empty() {
        out.push(stmt);
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

/// The leading keyword of `sql`, skipping any leading line/block comments and
/// whitespace. Empty when the statement has no leading word at all: blank,
/// comment-only, or paren-led (`(SELECT 1) UNION …`). Callers use that emptiness
/// as "nothing runnable here" when filtering [`split_statements`] output.
pub fn first_keyword(sql: &str) -> &str {
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
                None => return "",
            }
        } else {
            break;
        }
    }
    let end = s
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(s.len());
    &s[..end]
}

/// A copy of `sql` with everything that is *not* SQL structure blanked to spaces:
/// single-quoted strings (honoring the `''` escape), double-quoted / backtick-quoted
/// identifiers, and `--` line and `/* */` block comments.
///
/// Every gate that scans for keywords runs over this first, so that text merely
/// *resembling* SQL cannot influence a safety decision. Two directions matter and
/// both are load-bearing: `UPDATE t SET note = 'see where'` must not read as
/// carrying a WHERE clause, and `SELECT "delete" FROM t` must not read as a write.
/// Blanking rather than deleting keeps the surrounding tokens separated, so a
/// stripped statement still lexes into the same words.
///
/// **Byte-offset preserving.** Each blanked character becomes as many spaces as it
/// had UTF-8 bytes, so an offset found in the result indexes the *original* string
/// at the same place. [`risk::count_preflight`] relies on this to locate a `WHERE`
/// in the stripped copy and then slice the predicate out of the real SQL, which is
/// the only way to find it without a parser and still emit the literals verbatim.
pub fn strip_noise(sql: &str) -> String {
    // Blank one source character: same byte width, no content.
    fn blank(out: &mut String, c: char) {
        for _ in 0..c.len_utf8() {
            out.push(' ');
        }
    }
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // String literal / quoted identifier: consume to the matching close,
            // honoring the doubled-quote escape (`''`, `""`).
            '\'' | '"' | '`' => {
                blank(&mut out, c);
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n == c {
                        if chars.peek() == Some(&c) {
                            chars.next();
                            blank(&mut out, n);
                            blank(&mut out, c);
                            continue;
                        }
                        break;
                    }
                    blank(&mut out, n);
                }
                blank(&mut out, c);
            }
            // Line comment `-- …` to end of line. The newline survives so the
            // statement keeps its line structure.
            '-' if chars.peek() == Some(&'-') => {
                blank(&mut out, c);
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    chars.next();
                    blank(&mut out, n);
                }
            }
            // Block comment `/* … */`.
            '/' if chars.peek() == Some(&'*') => {
                blank(&mut out, c);
                chars.next();
                out.push(' '); // the `*`
                while let Some(n) = chars.next() {
                    blank(&mut out, n);
                    if n == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        out.push(' ');
                        break;
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Whether `word` appears in `haystack` as a whole word rather than as a fragment
/// of a longer identifier, so `updated_at` does not match `update`. `haystack` is
/// assumed already lower-cased and, for any safety decision, already passed through
/// [`strip_noise`].
pub fn has_word(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|tok| tok == word)
}

#[cfg(test)]
mod tests {
    use super::{first_keyword, has_word, split_statements, strip_noise};

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

    #[test]
    fn keeps_a_multi_statement_ddl_script_whole() {
        let script = "CREATE TABLE t(id INTEGER PRIMARY KEY, note TEXT);\n\
                      CREATE INDEX t_note ON t(note);\n\
                      INSERT INTO t VALUES (1, 'hi');";
        let s = split_statements(script);
        assert_eq!(s.len(), 3);
        assert!(s[0].starts_with("CREATE TABLE"));
        assert!(s[2].ends_with("'hi')"));
    }

    #[test]
    fn first_keyword_skips_leading_comments() {
        assert_eq!(first_keyword("SELECT 1"), "SELECT");
        assert_eq!(first_keyword("  \n update t set a=1"), "update");
        assert_eq!(first_keyword("-- note\nDELETE FROM t"), "DELETE");
        assert_eq!(first_keyword("/* a */ /* b */ DROP TABLE t"), "DROP");
        // Stops at the first non-alphabetic byte, so `select*` still reads as SELECT.
        assert_eq!(first_keyword("select*"), "select");
        // Nothing runnable: blank, comment-only, an unterminated block comment, and
        // a paren-led statement all yield empty, the callers' "skip this" signal.
        assert_eq!(first_keyword(""), "");
        assert_eq!(first_keyword("   -- just a note"), "");
        assert_eq!(first_keyword("/* never closed"), "");
        assert_eq!(first_keyword("(SELECT 1)"), "");
    }

    #[test]
    fn strip_noise_blanks_literals_identifiers_and_comments() {
        // A keyword inside a string must not survive into a safety scan.
        let s = strip_noise("UPDATE t SET note = 'see where' , x = 1");
        assert!(!has_word(&s.to_ascii_lowercase(), "where"));
        assert!(has_word(&s.to_ascii_lowercase(), "update"));
        // All three quote styles are blanked, so a column named like a write verb
        // cannot trip a read gate.
        for q in ["\"delete\"", "`delete`", "'delete'"] {
            let s = strip_noise(&format!("SELECT {q} FROM t")).to_ascii_lowercase();
            assert!(!has_word(&s, "delete"), "{q} leaked through strip_noise");
        }
        // Comments are blanked; a `;` inside one is not a statement separator to
        // anything scanning the stripped copy.
        let s = strip_noise("SELECT 1 -- drop table t;\n, 2 /* delete */");
        let lower = s.to_ascii_lowercase();
        assert!(!has_word(&lower, "drop"));
        assert!(!has_word(&lower, "delete"));
        assert!(!s.contains(';'));
        // A doubled quote is an escape, so the literal does not end early and the
        // keyword after it stays blanked.
        let s = strip_noise("SELECT 'it''s where' AS x").to_ascii_lowercase();
        assert!(!has_word(&s, "where"));
    }

    #[test]
    fn strip_noise_preserves_byte_offsets() {
        // `count_preflight` finds a keyword in the stripped copy and slices the
        // original at that offset, so every input must map one-to-one in bytes,
        // including multi-byte content inside the parts that get blanked.
        for sql in [
            "UPDATE t SET a = 1 WHERE b = 2",
            "UPDATE t SET note = 'héllo wörld' WHERE id = 1",
            "DELETE FROM t /* naïve comment */ WHERE x = 'ü'",
            "SELECT `dé` FROM t -- trailing ünicode\n",
            "SELECT 'it''s' AS x",
        ] {
            let stripped = strip_noise(sql);
            assert_eq!(stripped.len(), sql.len(), "byte length changed for {sql:?}");
            // The offset of a keyword in the stripped copy indexes the same keyword
            // in the original.
            if let Some(at) = stripped.to_ascii_lowercase().find("where") {
                assert!(
                    sql[at..].to_ascii_lowercase().starts_with("where"),
                    "{sql:?}"
                );
            }
        }
    }

    #[test]
    fn has_word_matches_whole_words_only() {
        assert!(has_word("delete from t", "delete"));
        assert!(has_word("select a where b", "where"));
        // The bug this exists to prevent: a column named `updated_at` reading as an
        // `update`, or `nowhere` as a `where`.
        assert!(!has_word("select updated_at from t", "update"));
        assert!(!has_word("select nowhere from t", "where"));
        assert!(!has_word("", "where"));
    }
}
