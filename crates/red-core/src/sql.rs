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

use crate::DbKind;

pub mod preflight;
pub mod risk;

pub use preflight::count_preflight;
pub use risk::{
    Assessment, DANGEROUS_FNS, DropKind, MutateVerb, Risk, RiskLevel, WRITE_TOKENS, assess,
};

/// The lexical profile of an engine's SQL: which comment forms exist and how
/// string escapes work. The scanner must match the engine byte for byte in both
/// directions — treating live SQL as a comment hides it from the safety gates
/// while the engine still runs it, and treating a comment as live SQL executes
/// text the user never wrote as code (a `DROP` after a `;` inside a MySQL `#`
/// comment). Neither direction is safe to guess, so every entry point takes the
/// dialect explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    /// The profile used when no engine is at hand (engine-less tooling, tests):
    /// `#` is not a comment (the conservative reading — Postgres uses it as an
    /// operator) and backslash escapes apply inside strings.
    #[default]
    Generic,
    MySql,
    Postgres,
    Sqlite,
    ClickHouse,
}

impl Dialect {
    /// The dialect of an engine kind; the non-SQL kinds lex as [`Dialect::Generic`]
    /// (they never reach the SQL scanner).
    pub fn of(kind: DbKind) -> Self {
        match kind {
            DbKind::Postgres => Dialect::Postgres,
            DbKind::Mysql => Dialect::MySql,
            DbKind::Sqlite => Dialect::Sqlite,
            DbKind::Clickhouse => Dialect::ClickHouse,
            DbKind::Redis | DbKind::Mongo => Dialect::Generic,
        }
    }

    /// MySQL alone speaks `#`-to-end-of-line comments. Everywhere else `#` is an
    /// operator (Postgres JSONB `#>`, `#` XOR), so reading it as a comment would
    /// hide live SQL from every gate that scans the stripped copy.
    fn hash_comments(self) -> bool {
        matches!(self, Dialect::MySql)
    }

    /// Whether `--` at `i` opens a line comment. MySQL requires whitespace (or
    /// end of input) after the dashes — `SELECT 5--3` is arithmetic there — and
    /// that is the *unsafe* direction to get wrong: text the engine executes
    /// must not be invisible to the gates.
    fn dash_comment_at(self, b: &[u8], i: usize) -> bool {
        if !(i + 1 < b.len() && b[i] == b'-' && b[i + 1] == b'-') {
            return false;
        }
        match self {
            Dialect::MySql => b.get(i + 2).is_none_or(|c| c.is_ascii_whitespace()),
            _ => true,
        }
    }

    /// Whether a backslash escapes the next character inside a `'…'`/`"…"`
    /// string. MySQL and ClickHouse say yes; Postgres (under
    /// `standard_conforming_strings`, its default since 9.1) and SQLite treat
    /// `\` as an ordinary character, so `'C:\'` is a complete literal there.
    /// Postgres `E'…'` strings *do* escape — handled at the quote site, where
    /// the prefix is visible ([`quote_escapes`]).
    fn backslash_in_strings(self) -> bool {
        matches!(
            self,
            Dialect::Generic | Dialect::MySql | Dialect::ClickHouse
        )
    }

    /// Whether `$$`/`$tag$` opens a dollar-quoted body (Postgres, and
    /// ClickHouse's heredoc). MySQL has none — with a `DELIMITER $$` in force a
    /// `$` inside a routine body is just a byte.
    fn dollar_quotes(self) -> bool {
        matches!(
            self,
            Dialect::Generic | Dialect::Postgres | Dialect::ClickHouse
        )
    }
}

/// Break `sql` into its top-level statements, each trimmed, with blank and
/// separator-only stretches dropped. A trailing statement without a final
/// separator is included. Borrows; no allocation per statement.
///
/// A comment-only stretch is still a "statement" here (it isn't blank); callers
/// that care whether it's runnable check for a leading keyword.
///
/// See [`statement_ranges`] for what is and isn't a separator.
pub fn split_statements(sql: &str, dialect: Dialect) -> Vec<&str> {
    statement_ranges(sql, dialect)
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
pub fn statement_ranges(sql: &str, dialect: Dialect) -> Vec<Range<usize>> {
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
                i = skip_quoted(b, i, q, quote_escapes(b, i, q, dialect));
                fresh = false;
            }
            // `-- line comment` to end of line. Leaves `fresh` alone: a comment
            // above a statement is not part of it.
            b'-' if dialect.dash_comment_at(b, i) => i = line_comment_end(b, i + 2),
            // MySQL `# line comment` to end of line.
            b'#' if dialect.hash_comments() => i = line_comment_end(b, i + 1),
            // `/* block comment */`.
            b'/' if i + 1 < n && b[i + 1] == b'*' => i = skip_block_comment(b, i),
            // Dollar-quoted body (`$$…$$` / `$tag$…$tag$`). Falls through to a
            // normal byte when the `$` isn't actually a dollar-quote opener
            // (e.g. a `$1` positional parameter).
            b'$' if dialect.dollar_quotes() => {
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
                } else if opens_block(b, word, i, dialect) {
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

/// Whether `word` at `sql[..from]` opens a block that an `END` closes.
///
/// `IF` and `REPEAT` are also functions, which is the one ambiguity here.
/// Written call-style — `(` hard against the word — they are calls
/// (`IF(a, b, c)`), matching how MySQL's own lexer reads built-ins. A *spaced*
/// `(` after `IF` is usually a parenthesized block condition
/// (`IF (available < NEW.quantity) THEN`), but can still be a call; only a
/// top-level `THEN` before the statement ends settles it. `WHILE`, `CASE` and
/// `LOOP` have no function form, so their parenthesized conditions always open
/// blocks. `IF` also introduces the `IF [NOT] EXISTS` of a header, which opens
/// nothing.
fn opens_block(b: &[u8], word: &[u8], from: usize, dialect: Dialect) -> bool {
    if !is_block_kind(word) && !word.eq_ignore_ascii_case(b"begin") {
        return false;
    }
    if word.eq_ignore_ascii_case(b"if") || word.eq_ignore_ascii_case(b"repeat") {
        if b.get(from) == Some(&b'(') {
            return false;
        }
        let mut at = from;
        while at < b.len() && b[at].is_ascii_whitespace() {
            at += 1;
        }
        if b.get(at) == Some(&b'(') {
            return word.eq_ignore_ascii_case(b"if") && then_follows(b, at, dialect);
        }
    }
    !matches!(
        next_word(b, from),
        Some((next, _)) if next.eq_ignore_ascii_case(b"not") || next.eq_ignore_ascii_case(b"exists")
    )
}

/// Whether a top-level `THEN` appears at or after `from` before the statement's
/// own `;` — how a spaced `IF (…)` is told apart from a spaced call: the block
/// form must reach a `THEN` (`IF (a) OR (b) THEN`), while a call's statement
/// ends without one (`SET x = IF (a, b, c);`). Strings, comments and nested
/// parens are skipped so a `then` inside any of them doesn't decide anything.
fn then_follows(b: &[u8], from: usize, dialect: Dialect) -> bool {
    let n = b.len();
    let mut depth = 0usize;
    let mut i = from;
    while i < n {
        match b[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b';' if depth == 0 => return false,
            q @ (b'\'' | b'"' | b'`') => i = skip_quoted(b, i, q, quote_escapes(b, i, q, dialect)),
            b'-' if dialect.dash_comment_at(b, i) => i = line_comment_end(b, i + 2),
            b'#' if dialect.hash_comments() => i = line_comment_end(b, i + 1),
            b'/' if i + 1 < n && b[i + 1] == b'*' => i = skip_block_comment(b, i),
            c if is_word_byte(c) => {
                let end = word_end(b, i);
                if depth == 0 && b[i..end].eq_ignore_ascii_case(b"then") {
                    return true;
                }
                i = end;
            }
            _ => i += 1,
        }
    }
    false
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
        // A structural DDL kind settles the question before the window can reach
        // an *object name* that happens to read like a routine kind: without
        // this, `CREATE TABLE event (…)` would arm compound mode.
        if [
            "table", "view", "index", "database", "schema", "sequence", "user", "role",
        ]
        .iter()
        .any(|k| word.eq_ignore_ascii_case(k.as_bytes()))
        {
            return false;
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

/// The `DELIMITER` directive's token: the first whitespace-separated word of the
/// line's remainder — the mysql client reads one token, so a trailing comment
/// (`DELIMITER $$ -- note`) must not become part of the separator — with the
/// index just past the newline.
fn rest_of_line(b: &[u8], from: usize) -> (&[u8], usize) {
    let mut end = from;
    while end < b.len() && b[end] != b'\n' {
        end += 1;
    }
    let line = &b[from..end];
    let lead = line.iter().take_while(|c| c.is_ascii_whitespace()).count();
    let len = line[lead..]
        .iter()
        .take_while(|c| !c.is_ascii_whitespace())
        .count();
    (&line[lead..lead + len], (end + 1).min(b.len()))
}

/// Whether a backslash escapes the next character inside the quote opened at `i`:
/// never inside a backtick identifier, otherwise per the dialect's string rule,
/// plus the Postgres `E'…'` escape-string form, recognised by the `e`/`E` hard
/// against the opening quote that starts its own token.
fn quote_escapes(b: &[u8], i: usize, q: u8, dialect: Dialect) -> bool {
    if q == b'`' {
        return false;
    }
    if dialect.backslash_in_strings() {
        return true;
    }
    dialect == Dialect::Postgres
        && q == b'\''
        && i >= 1
        && (b[i - 1] == b'e' || b[i - 1] == b'E')
        && (i < 2 || !is_word_byte(b[i - 2]))
}

/// Index just past the closing quote of the literal/identifier opened at `i`
/// (whose quote char is `q`). Handles the doubled-quote escape (`''`, `""`,
/// ` `` `) and, when `escapes` (see [`quote_escapes`]), a backslash escape
/// (`\'`). An unterminated quote consumes to end-of-input.
fn skip_quoted(b: &[u8], i: usize, q: u8, escapes: bool) -> usize {
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
        if b[j] == b'\\' && escapes && j + 1 < n {
            j += 2;
            continue;
        }
        j += 1;
    }
    n
}

/// Index of the newline ending the line comment whose *body* starts at `from`
/// (just past the `--` or `#` marker), or end-of-input. The newline itself is
/// left for the main loop to step over.
fn line_comment_end(b: &[u8], from: usize) -> usize {
    let n = b.len();
    let mut j = from.min(n);
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
///
/// Dialect-free on purpose: a leading `--` or `#` line can only be a comment (no
/// dialect starts a statement with either operator), so skipping both is safe
/// for every engine.
pub fn first_keyword(sql: &str) -> &str {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--").or_else(|| s.strip_prefix('#')) {
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
/// **Byte-offset preserving.** Each blanked byte becomes one space (newlines
/// survive, keeping line structure), so an offset found in the result indexes the
/// *original* string at the same place. [`risk::count_preflight`] relies on this
/// to locate a `WHERE` in the stripped copy and then slice the predicate out of
/// the real SQL, which is the only way to find it without a parser and still emit
/// the literals verbatim.
///
/// Built on the same quote/comment helpers as [`statement_ranges`], with the same
/// [`Dialect`], so the two can never disagree about where a literal or comment
/// ends — the drift the module doc says these primitives exist to prevent.
pub fn strip_noise(sql: &str, dialect: Dialect) -> String {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let from = i;
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i = skip_quoted(b, i, q, quote_escapes(b, i, q, dialect));
                blank_span(&mut out, b, from, i);
            }
            b'-' if dialect.dash_comment_at(b, i) => {
                i = line_comment_end(b, i + 2);
                blank_span(&mut out, b, from, i);
            }
            b'#' if dialect.hash_comments() => {
                i = line_comment_end(b, i + 1);
                blank_span(&mut out, b, from, i);
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i = skip_block_comment(b, i);
                blank_span(&mut out, b, from, i);
            }
            // A dollar-quoted body is a string literal by another name; its
            // content must be as invisible to the gates as any other literal.
            b'$' if dialect.dollar_quotes() => match dollar_quote_end(b, i) {
                Some(end) => {
                    i = end;
                    blank_span(&mut out, b, from, i);
                }
                None => {
                    out.push(b'$');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    // Blanked spans begin and end on ASCII bytes and everything else is copied
    // verbatim, so the buffer is valid UTF-8 by construction; the fallback is
    // unreachable but cheaper than a panic path.
    String::from_utf8(out).unwrap_or_else(|_| sql.to_string())
}

/// Blank `b[from..to]` into `out` as spaces, keeping newlines so the copy holds
/// its line structure (and its byte length) exactly.
fn blank_span(out: &mut Vec<u8>, b: &[u8], from: usize, to: usize) {
    out.extend(
        b[from..to]
            .iter()
            .map(|&c| if c == b'\n' { b'\n' } else { b' ' }),
    );
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
    use super::{Dialect, first_keyword, has_word};

    /// Tests that don't probe a dialect difference run under [`Dialect::Generic`];
    /// the dialect-specific behaviours have their own tests below.
    fn split_statements(sql: &str) -> Vec<&str> {
        super::split_statements(sql, Dialect::Generic)
    }

    fn strip_noise(sql: &str) -> String {
        super::strip_noise(sql, Dialect::Generic)
    }

    /// MySQL's `#` comment hides everything to end of line — including a `;` and
    /// a `DROP` after it, which must not become a statement RED executes. To
    /// Postgres `#` is an operator, so the `;` after it is a real boundary.
    #[test]
    fn hash_comments_follow_the_dialect() {
        let s = super::split_statements("SELECT 1 # tidy; DROP TABLE users", Dialect::MySql);
        assert_eq!(s.len(), 1, "got {s:#?}");
        let s = super::split_statements("SELECT a # b; SELECT 2", Dialect::Postgres);
        assert_eq!(s.len(), 2, "got {s:#?}");
    }

    /// A parenthesized block condition opens a block — `IF (cond) THEN` is the
    /// most common MySQL routine style — while the function spellings still open
    /// nothing.
    #[test]
    fn parenthesized_block_conditions_open_blocks() {
        for script in [
            "CREATE PROCEDURE p() BEGIN \
             IF (1 > 0) THEN SET @a = 1; END IF; SET @b = 2; END; SELECT 1",
            "CREATE PROCEDURE p() BEGIN \
             WHILE (1 > 0) DO SET @a = 1; END WHILE; END; SELECT 1",
            "CREATE PROCEDURE p() BEGIN \
             CASE (1) WHEN 1 THEN SET @a = 1; END CASE; END; SELECT 1",
            // A multi-group condition reaches its THEN too.
            "CREATE PROCEDURE p() BEGIN \
             IF (1 > 0) OR (2 > 1) THEN SET @a = 1; END IF; END; SELECT 1",
            // Function calls — packed and spaced — open no block.
            "CREATE PROCEDURE p() BEGIN \
             SET @a = IF(1 > 0, 1, 0); SET @b = IF (1, 2, 3); END; SELECT 1",
        ] {
            let s = super::split_statements(script, Dialect::MySql);
            assert_eq!(s.len(), 2, "{script}\ngot {s:#?}");
            assert_eq!(s[1], "SELECT 1", "{script}");
        }
    }

    /// Postgres treats `\` as an ordinary character in a plain string
    /// (`standard_conforming_strings`, its default), so `'C:\'` is closed; MySQL
    /// treats it as an escape, so the same bytes stay open. Postgres `E'…'`
    /// strings do escape.
    #[test]
    fn backslash_handling_follows_the_dialect() {
        let sql = "INSERT INTO t VALUES ('C:\\'); DELETE FROM t WHERE id = 1;";
        assert_eq!(super::split_statements(sql, Dialect::Postgres).len(), 2);
        assert_eq!(super::split_statements(sql, Dialect::MySql).len(), 1);
        let sql = "SELECT E'it\\'s; one'; SELECT 2";
        assert_eq!(super::split_statements(sql, Dialect::Postgres).len(), 2);
    }

    /// MySQL only opens a `--` comment when whitespace (or end of input)
    /// follows: `5--3` is arithmetic there, and hiding it from the gates while
    /// the engine runs it would be the unsafe direction.
    #[test]
    fn mysql_dash_dash_needs_whitespace() {
        let sql = "SELECT 5--3; SELECT 1";
        assert_eq!(super::split_statements(sql, Dialect::MySql).len(), 2);
        assert_eq!(super::split_statements(sql, Dialect::Postgres).len(), 1);
    }

    /// `DELIMITER $$ -- note` sets `$$`, not the whole trimmed tail.
    #[test]
    fn delimiter_token_stops_at_whitespace() {
        let s = super::split_statements(
            "DELIMITER $$ -- note\nSELECT 1$$\nSELECT 2$$",
            Dialect::MySql,
        );
        assert_eq!(s, vec!["SELECT 1", "SELECT 2"]);
    }

    /// An object *named* like a routine kind must not arm compound mode.
    #[test]
    fn create_table_named_event_is_not_routine_ddl() {
        assert!(!super::is_routine_ddl("CREATE TABLE event (id INT)"));
        assert!(super::is_routine_ddl(
            "CREATE EVENT e ON SCHEDULE EVERY 1 DAY DO SELECT 1"
        ));
    }

    /// The splitter and the stripper share their lexing: under MySQL `'don\'t'`
    /// is one literal to both, so the real `WHERE` stays visible to the confirm
    /// gate — the divergence that used to report a false "UPDATE with no WHERE".
    #[test]
    fn strip_noise_matches_the_splitter_on_escapes() {
        let sql = "UPDATE t SET a='don\\'t' WHERE id=1";
        let stripped = super::strip_noise(sql, Dialect::MySql);
        assert!(
            has_word(&stripped.to_ascii_lowercase(), "where"),
            "{stripped:?}"
        );
        assert_eq!(stripped.len(), sql.len());
        // A dollar-quoted body is a literal too; its content must not reach a scan.
        let stripped = super::strip_noise("SELECT $$delete$$ AS x", Dialect::Postgres);
        assert!(!has_word(&stripped.to_ascii_lowercase(), "delete"));
        // And a MySQL `#` comment is blanked with its `;` and its `DROP`.
        let stripped = super::strip_noise("SELECT 1 # drop; DROP TABLE t", Dialect::MySql);
        assert!(!has_word(&stripped.to_ascii_lowercase(), "drop"));
        assert!(!stripped.contains(';'));
    }

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
        let r = super::statement_ranges(sql, Dialect::Generic);
        assert_eq!(r.len(), 2);
        assert_eq!(&sql[r[0].clone()], "SELECT 1");
        assert_eq!(&sql[r[1].clone()], "\nSELECT 2");
        // A blank tail after the final separator is a span of its own (the caret can
        // sit there); `split_statements` is what drops it.
        let r = super::statement_ranges("SELECT 1;", Dialect::Generic);
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
