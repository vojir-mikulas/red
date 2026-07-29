//! Lexical analysis of SQL scripts: splitting them into runnable statements, and
//! the shared helpers every layer uses to reason about one.
//!
//! Every engine's `execute` runs exactly **one** statement (rusqlite `execute`,
//! tokio-postgres `execute`, a single `query_drop`), so a `;`-separated script has
//! to be broken up before it reaches a driver. This is a lexer, not a parser: it
//! walks the bytes tracking whether it's inside a string literal, quoted
//! identifier, line/block comment, a Postgres dollar-quoted body, or a MySQL
//! routine's compound body, and only breaks on a separator seen at top level.
//! Good enough for hand-written and tool-exported scripts; it is not a full SQL
//! grammar.
//!
//! Two rules exist for one reason — a stored routine's body holds `;` of its own
//! (`BEGIN DECLARE x INT; … END`), which is not a statement boundary:
//!
//! * inside a `CREATE`/`ALTER`/`DROP` of a TRIGGER / PROCEDURE / FUNCTION / EVENT,
//!   separators are ignored until the body's blocks have closed
//!   ([`statement_ranges`]);
//! * `DELIMITER $$`, the client directive every SQL client implements for exactly
//!   this problem, is honoured and consumed, so a script pasted from documentation
//!   runs as written instead of reaching a server that has never heard of it.
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

use std::ops::Range;

pub mod preflight;
pub mod risk;

pub use preflight::count_preflight;
pub use risk::{
    Assessment, DANGEROUS_FNS, DropKind, MutateVerb, Risk, RiskLevel, WRITE_TOKENS, assess,
};

/// Break `sql` into its top-level statements, each trimmed, with blank and
/// separator-only stretches dropped. A trailing statement without a final
/// separator is included. Borrows; no allocation per statement.
///
/// A comment-only stretch is still a "statement" here (it isn't blank); callers
/// that care whether it's runnable check for a leading keyword.
///
/// See [`statement_ranges`] for what is and isn't a separator.
pub fn split_statements(sql: &str) -> Vec<&str> {
    statement_ranges(sql)
        .into_iter()
        .map(|r| sql[r].trim())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The byte range of every statement in `sql`, separators excluded, in order. The
/// one scanner all statement boundaries in RED come from — the service's `Execute`
/// batch, the UI's caret-statement and gutter markers, and the CLI's `red exec`.
///
/// Spans are untrimmed and may be blank (a `;;`, or the empty tail after a final
/// `;`); [`split_statements`] is the trimmed, non-blank view. A `DELIMITER` line
/// yields no span at all, being a client directive rather than a statement.
///
/// A separator inside any of these is not a boundary: a string literal, a quoted
/// identifier, a line or block comment, a Postgres dollar-quoted body, or the
/// compound body of a routine (see [`is_routine_ddl`]) whose blocks are still open.
pub fn statement_ranges(sql: &str) -> Vec<Range<usize>> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    // The current separator: `;` until a `DELIMITER` directive replaces it.
    let mut delim: &[u8] = b";";
    // How many of the current routine body's blocks are still open. Only counted
    // while `compound`, so a bare `BEGIN` (transaction control) still ends at its
    // `;` rather than swallowing the rest of the script.
    let mut depth = 0usize;
    let mut compound = false;
    let mut start = 0;
    // Nothing but whitespace and comments seen since `start`, so the next word is
    // this statement's first — where `DELIMITER` is a directive and the leading
    // keywords decide whether a compound body follows.
    let mut fresh = true;
    let mut i = 0;
    while i < n {
        // Checked before the quote/comment/dollar arms so a `$$` separator isn't
        // mistaken for the opening of a dollar-quoted body.
        if depth == 0 && b[i..].starts_with(delim) {
            out.push(start..i);
            i += delim.len();
            start = i;
            fresh = true;
            compound = false;
            continue;
        }
        match b[i] {
            // String literals and quoted identifiers. Backtick is MySQL's
            // identifier quote; single/double are SQL string / identifier.
            q @ (b'\'' | b'"' | b'`') => {
                i = skip_quoted(b, i, q);
                fresh = false;
            }
            // `-- line comment` to end of line. Leaves `fresh` alone: a comment
            // above a statement is not part of it.
            b'-' if i + 1 < n && b[i + 1] == b'-' => i = skip_line_comment(b, i),
            // `/* block comment */`.
            b'/' if i + 1 < n && b[i + 1] == b'*' => i = skip_block_comment(b, i),
            // Postgres dollar-quoted body (`$$…$$` / `$tag$…$tag$`). Falls through
            // to a normal byte when the `$` isn't actually a dollar-quote opener
            // (e.g. a `$1` positional parameter).
            b'$' => {
                i = match dollar_quote_end(b, i) {
                    Some(end) => end,
                    None => i + 1,
                };
                fresh = false;
            }
            c if is_word_byte(c) => {
                let end = word_end(b, i);
                let word = &b[i..end];
                if fresh {
                    // `DELIMITER $$`: not SQL. It sets the separator for what
                    // follows and is consumed here, exactly as the mysql client
                    // does, so a pasted routine script runs as written.
                    if word.eq_ignore_ascii_case(b"delimiter") {
                        let (token, line_end) = rest_of_line(b, end);
                        if !token.is_empty() {
                            delim = token;
                        }
                        i = line_end;
                        start = i;
                        continue;
                    }
                    compound = routine_ddl_at(b, i);
                    fresh = false;
                }
                i = end;
                if !compound {
                    continue;
                }
                if word.eq_ignore_ascii_case(b"end") {
                    depth = depth.saturating_sub(1);
                    // One `END` closes one block, so the kind word of `END IF` /
                    // `END CASE` / `END LOOP` / `END WHILE` / `END REPEAT` must not
                    // then read as a fresh opener.
                    if let Some((next, next_end)) = next_word(b, i)
                        && is_block_kind(next)
                    {
                        i = next_end;
                    }
                } else if opens_block(b, word, i) {
                    depth += 1;
                }
            }
            c => {
                if !c.is_ascii_whitespace() {
                    fresh = false;
                }
                i += 1;
            }
        }
    }
    out.push(start..n);
    out
}

/// Whether `word` at `sql[..from]` opens a block that an `END` closes. `IF` and
/// `REPEAT` are also functions (`IF(a, b, c)`), and `IF` also introduces the
/// `IF [NOT] EXISTS` of a header, so neither spelling counts as a block.
fn opens_block(b: &[u8], word: &[u8], from: usize) -> bool {
    if !is_block_kind(word) && !word.eq_ignore_ascii_case(b"begin") {
        return false;
    }
    match next_word(b, from) {
        // A function call, not a block: the `(` follows with nothing between.
        None if b[from..].iter().find(|c| !c.is_ascii_whitespace()) == Some(&b'(') => false,
        Some((next, _))
            if next.eq_ignore_ascii_case(b"not") || next.eq_ignore_ascii_case(b"exists") =>
        {
            false
        }
        _ => true,
    }
}

/// The block kinds an `END` can close, as `END <kind>`. `BEGIN` is deliberately
/// absent: it closes with a bare `END`.
fn is_block_kind(word: &[u8]) -> bool {
    [
        b"if".as_slice(),
        b"case".as_slice(),
        b"loop".as_slice(),
        b"while".as_slice(),
        b"repeat".as_slice(),
    ]
    .iter()
    .any(|k| word.eq_ignore_ascii_case(k))
}

/// Whether the statement starting at `from` declares a trigger or stored routine:
/// `CREATE`/`ALTER`/`DROP` of a TRIGGER, PROCEDURE, FUNCTION or EVENT, looking past
/// the optional `OR REPLACE` / `DEFINER = user@host` / `IF NOT EXISTS` preamble
/// (hence a short window of words rather than just the one after the verb).
fn routine_ddl_at(b: &[u8], from: usize) -> bool {
    /// Words of header to scan past the verb: enough for the longest preamble,
    /// short enough never to reach a body word.
    const HEADER_WORDS: usize = 8;
    let Some((verb, mut at)) = next_word(b, from) else {
        return false;
    };
    if !["create", "alter", "drop"]
        .iter()
        .any(|v| verb.eq_ignore_ascii_case(v.as_bytes()))
    {
        return false;
    }
    for _ in 0..HEADER_WORDS {
        let Some((word, next)) = next_header_word(b, at) else {
            return false;
        };
        if ["trigger", "procedure", "function", "event"]
            .iter()
            .any(|k| word.eq_ignore_ascii_case(k.as_bytes()))
        {
            return true;
        }
        at = next;
    }
    false
}

/// Whether the statement `sql` declares a trigger or stored routine, the shape
/// whose body speaks a procedural dialect (`BEGIN … END`, `DECLARE`, `FOR EACH
/// ROW`) rather than the query language. Callers that model only queries stop
/// short of guessing at these.
pub fn is_routine_ddl(sql: &str) -> bool {
    routine_ddl_at(sql.as_bytes(), 0)
}

/// Bytes that make up an unquoted identifier or keyword.
fn is_word_byte(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

/// The index just past the word starting at `i`.
fn word_end(b: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < b.len() && is_word_byte(b[j]) {
        j += 1;
    }
    j
}

/// The next word at or after `from` with the index just past it, skipping only
/// whitespace — so it stops at punctuation rather than reading across it.
fn next_word(b: &[u8], from: usize) -> Option<(&[u8], usize)> {
    let mut i = from;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= b.len() || !is_word_byte(b[i]) {
        return None;
    }
    let end = word_end(b, i);
    Some((&b[i..end], end))
}

/// Like [`next_word`], but for walking a statement's header: the punctuation and
/// quoting of a `DEFINER = `root`@`localhost`` clause is skipped over rather than
/// stopping the walk, while a `(` stops it — every routine header names its kind
/// before its parameter list, and `CREATE TABLE t (…)` opens one before naming a
/// column that might read like a kind.
fn next_header_word(b: &[u8], from: usize) -> Option<(&[u8], usize)> {
    let mut i = from;
    while i < b.len() && !is_word_byte(b[i]) {
        if b[i] == b'(' {
            return None;
        }
        i += 1;
    }
    if i >= b.len() {
        return None;
    }
    let end = word_end(b, i);
    Some((&b[i..end], end))
}

/// The trimmed remainder of the line starting at `from` (a `DELIMITER` directive's
/// token) with the index just past the newline.
fn rest_of_line(b: &[u8], from: usize) -> (&[u8], usize) {
    let mut end = from;
    while end < b.len() && b[end] != b'\n' {
        end += 1;
    }
    let token = &b[from..end];
    let lead = token.len() - token.iter().take_while(|c| c.is_ascii_whitespace()).count();
    let token = &token[token.len() - lead..];
    let trailing = token
        .iter()
        .rev()
        .take_while(|c| c.is_ascii_whitespace())
        .count();
    (&token[..token.len() - trailing], (end + 1).min(b.len()))
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

    /// A MySQL trigger with a compound body is one statement, `;`s and all — the
    /// reported break, where the body was cut at its first `DECLARE … ;`.
    #[test]
    fn compound_routine_body_is_one_statement() {
        let script = "\
CREATE TRIGGER shop.trg_oi_bi
BEFORE INSERT ON shop.order_items
FOR EACH ROW
BEGIN
  DECLARE available INT;
  SELECT stock INTO available FROM shop.products WHERE id = NEW.product_id FOR UPDATE;
  IF available < NEW.quantity THEN
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'nope';
  END IF;
END;
SELECT 1";
        let s = split_statements(script);
        assert_eq!(s.len(), 2, "got {s:#?}");
        assert!(s[0].starts_with("CREATE TRIGGER") && s[0].ends_with("END"));
        assert_eq!(s[1], "SELECT 1");
    }

    /// Every block form a body can nest, and the `END <kind>` that closes each: one
    /// `END` per opener, so the kind word must not re-open what it just closed.
    #[test]
    fn nested_block_forms_all_close() {
        let script = "\
CREATE PROCEDURE p()
BEGIN
  WHILE 1 DO
    CASE 1 WHEN 1 THEN SET @a = 1; END CASE;
    LOOP
      IF 1 THEN LEAVE l; END IF;
    END LOOP;
    REPEAT SET @b = 1; UNTIL 1 END REPEAT;
  END WHILE;
END;
SELECT 2";
        let s = split_statements(script);
        assert_eq!(s.len(), 2, "got {s:#?}");
        assert!(s[0].ends_with("END"));
        assert_eq!(s[1], "SELECT 2");
    }

    /// `IF` is a function and a header word too, neither of which opens a block —
    /// miscounting either would swallow the rest of the script.
    #[test]
    fn if_as_function_and_if_exists_open_no_block() {
        let s = split_statements(
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW \
             SET NEW.a = IF(NEW.a > 0, NEW.a, 0); SELECT 1",
        );
        assert_eq!(s.len(), 2, "got {s:#?}");
        let s = split_statements("DROP TRIGGER IF EXISTS t; DROP PROCEDURE IF EXISTS p; SELECT 1");
        assert_eq!(s.len(), 3, "got {s:#?}");
    }

    /// A bare `BEGIN` is transaction control, not a routine body: it must still end
    /// at its own `;` rather than swallowing everything after it.
    #[test]
    fn transaction_begin_is_not_a_compound_body() {
        let s = split_statements("BEGIN; UPDATE t SET a = 1; COMMIT;");
        assert_eq!(s, vec!["BEGIN", "UPDATE t SET a = 1", "COMMIT"]);
    }

    /// `DELIMITER` is a client directive: honoured as the separator for what
    /// follows, and never emitted as a statement (the server rejects it, 1064).
    #[test]
    fn delimiter_directive_is_honoured_and_consumed() {
        let script = "\
DELIMITER $$
CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW
BEGIN
  SET NEW.a = 1;
END$$
DELIMITER ;
SELECT 1;";
        let s = split_statements(script);
        assert_eq!(s.len(), 2, "got {s:#?}");
        assert!(s[0].starts_with("CREATE TRIGGER") && s[0].ends_with("END"));
        assert_eq!(s[1], "SELECT 1");
        // A custom delimiter that isn't `$$`, and `//` (the other common choice).
        let s = split_statements("DELIMITER //\nSELECT 1//\nSELECT 2//");
        assert_eq!(s, vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn is_routine_ddl_spots_the_declaring_forms_only() {
        for sql in [
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET NEW.a = 1",
            "CREATE DEFINER=`root`@`localhost` PROCEDURE p() SELECT 1",
            "CREATE OR REPLACE FUNCTION f() RETURNS INT RETURN 1",
            "DROP TRIGGER IF EXISTS t",
            "ALTER EVENT e ON SCHEDULE EVERY 1 DAY DO SELECT 1",
        ] {
            assert!(super::is_routine_ddl(sql), "{sql}");
        }
        for sql in [
            "SELECT 1",
            "CREATE TABLE t (id INT)",
            "CREATE VIEW v AS SELECT 1",
            "UPDATE t SET trigger_count = 1",
            "",
        ] {
            assert!(!super::is_routine_ddl(sql), "{sql}");
        }
    }

    /// Ranges address the original buffer, so an offset-based caller (the editor's
    /// caret statement, its gutter markers) can map back into it.
    #[test]
    fn ranges_address_the_source_and_exclude_separators() {
        let sql = "SELECT 1;\nSELECT 2";
        let r = super::statement_ranges(sql);
        assert_eq!(r.len(), 2);
        assert_eq!(&sql[r[0].clone()], "SELECT 1");
        assert_eq!(&sql[r[1].clone()], "\nSELECT 2");
        // A blank tail after the final separator is a span of its own (the caret can
        // sit there); `split_statements` is what drops it.
        let r = super::statement_ranges("SELECT 1;");
        assert_eq!(r.len(), 2);
        assert!(r[1].is_empty());
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
