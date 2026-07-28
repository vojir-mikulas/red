//! Turning a statement into the query that says how much it will touch.
//!
//! Grading a statement tells you its *shape*: whether it has a filter, whether it
//! drops something. It cannot tell you its *scope*, and scope is what actually goes
//! wrong. `DELETE FROM orders WHERE status = 'active'` is perfectly well-formed and
//! perfectly filtered, and the only thing that reveals it was about to remove 90% of
//! the table is asking the engine.
//!
//! So [`count_preflight`] rewrites a mutation into a row-selecting query over the
//! same table and predicate. The caller hands that to `DatabaseDriver::count`, which
//! wraps it as `SELECT count(*) FROM (…)`, so the number that comes back is exactly
//! the number of rows the original statement would have hit.
//!
//! **It refuses far more than it accepts.** Returning `None` costs the user a line
//! in a dialog; returning a query that counts the *wrong* rows would put a
//! reassuring number in front of someone at the moment they decide to destroy
//! something. Every shape that cannot be rewritten by slicing (joins, multi-table
//! forms, `RETURNING`, CTEs, batches) is declined rather than guessed at.

use super::risk::target_object;
use super::{first_keyword, split_statements, strip_noise};

/// The query whose row count equals the number of rows `sql` would affect, or `None`
/// when `sql` is not a shape this can rewrite faithfully.
///
/// The result selects rows rather than counting them (`SELECT 1 FROM t WHERE …`),
/// because `DatabaseDriver::count` supplies the `count(*)` wrapper and its own
/// cancellation. Literals in the predicate are carried through verbatim from the
/// original SQL, never re-serialised.
///
/// Accepted shapes, all single-statement:
///
/// - `UPDATE t SET … [WHERE p]` -> `SELECT 1 FROM t [WHERE p]`
/// - `DELETE FROM t [WHERE p]` -> `SELECT 1 FROM t [WHERE p]`
/// - `TRUNCATE [TABLE] t` and `DROP TABLE t` -> `SELECT 1 FROM t`, so a
///   confirmation can say how much lives in the table before it goes.
pub fn count_preflight(sql: &str) -> Option<String> {
    let statements: Vec<&str> = split_statements(sql)
        .into_iter()
        .filter(|s| !first_keyword(s).is_empty())
        .collect();
    // A batch has no single "how many rows will this touch" answer.
    let [stmt] = statements.as_slice() else {
        return None;
    };
    let stripped = strip_noise(stmt);
    let lower = stripped.to_ascii_lowercase();

    // `RETURNING` would be swept into the sliced tail and make the count query
    // invalid; the multi-table and join forms mean the rewritten `FROM` would no
    // longer describe the same row set. Neither is worth guessing at.
    //
    // Top-level only: a subquery is free to join and select from whatever it likes,
    // because it rides along inside the predicate unchanged.
    if ["returning", "using", "join"]
        .iter()
        .any(|w| top_level_word(&lower, w).is_some())
    {
        return None;
    }

    let table = target_object(stmt)?;
    match first_keyword(&lower) {
        "update" => {
            // Postgres' `UPDATE t SET … FROM u` is a join by another name. Again
            // top-level: `SET a = (SELECT … FROM u)` is an ordinary scalar subquery.
            if top_level_word(&lower, "from").is_some() {
                return None;
            }
            // MySQL's comma multi-table form (`UPDATE a, b SET …`): the extractor
            // above happily returns the first table, but the count would silently
            // ignore the second. A comma *after* `SET` is just the assignment list.
            let set_at = top_level_word(&lower, "set")?;
            if top_level_comma(&lower).is_some_and(|comma| comma < set_at) {
                return None;
            }
            Some(select_over(&table, where_tail(stmt, &lower)))
        }
        "delete" => Some(select_over(&table, where_tail(stmt, &lower))),
        // No predicate to carry: these take the whole table, and the useful number is
        // how much is in it. A comma means a list of tables, and so no single count.
        "truncate" => top_level_comma(&lower)
            .is_none()
            .then(|| select_over(&table, None)),
        "drop" => (word_at(&lower, 1) == "table" && top_level_comma(&lower).is_none())
            .then(|| select_over(&table, None)),
        _ => None,
    }
}

/// `SELECT 1 FROM <table>` with the original `WHERE …` tail appended when there is
/// one.
fn select_over(table: &str, tail: Option<&str>) -> String {
    match tail {
        Some(tail) => format!("SELECT 1 FROM {table} {tail}"),
        None => format!("SELECT 1 FROM {table}"),
    }
}

/// The `WHERE …` tail of `stmt`, sliced verbatim from the original so its literals
/// survive, located via the offset of the top-level `where` in the stripped copy.
fn where_tail<'a>(stmt: &'a str, lower: &str) -> Option<&'a str> {
    let at = top_level_word(lower, "where")?;
    Some(stmt[at..].trim_end().trim_end_matches(';').trim_end())
}

/// The byte offset of `word` in `lower` at parenthesis depth zero.
///
/// Depth matters because a subquery has its own `WHERE`:
/// `UPDATE t SET a = (SELECT x FROM u WHERE u.id = 1) WHERE t.id = 2` must slice at
/// the second one. Taking the first match would silently count the wrong rows, which
/// is the one failure this module must not have.
fn top_level_word(lower: &str, word: &str) -> Option<usize> {
    let bytes = lower.as_bytes();
    let mut depth = 0i32;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b if b.is_ascii_alphanumeric() || b == b'_' => {
                let start = at;
                while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                    at += 1;
                }
                if depth == 0 && &lower[start..at] == word {
                    return Some(start);
                }
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    None
}

/// The byte offset of the first `,` at parenthesis depth zero, which in the shapes
/// here always means a list of tables rather than a list of anything harmless.
fn top_level_comma(lower: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (at, b) in lower.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => return Some(at),
            _ => {}
        }
    }
    None
}

/// The `n`th whitespace-separated word of an already-lower-cased statement.
fn word_at(lower: &str, n: usize) -> &str {
    lower.split_whitespace().nth(n).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::count_preflight;

    #[test]
    fn rewrites_the_shapes_it_accepts() {
        for (sql, want) in [
            ("DELETE FROM orders", "SELECT 1 FROM orders"),
            (
                "DELETE FROM orders WHERE status = 'active'",
                "SELECT 1 FROM orders WHERE status = 'active'",
            ),
            (
                "UPDATE public.users SET banned = true",
                "SELECT 1 FROM public.users",
            ),
            (
                "UPDATE users SET a = 1, b = 2 WHERE id > 10",
                "SELECT 1 FROM users WHERE id > 10",
            ),
            ("TRUNCATE orders", "SELECT 1 FROM orders"),
            ("TRUNCATE TABLE orders", "SELECT 1 FROM orders"),
            ("DROP TABLE orders", "SELECT 1 FROM orders"),
            ("DROP TABLE IF EXISTS orders", "SELECT 1 FROM orders"),
            // A trailing terminator is trimmed off the tail, not carried into it.
            (
                "DELETE FROM t WHERE id = 1;",
                "SELECT 1 FROM t WHERE id = 1",
            ),
        ] {
            assert_eq!(count_preflight(sql).as_deref(), Some(want), "{sql}");
        }
    }

    #[test]
    fn quoted_table_names_survive_into_the_count_query() {
        // The stripped copy blanks the quoted name, so the WHERE offset is found over
        // blanks and the name itself is taken from the original, quotes intact.
        for (sql, want) in [
            (
                r#"DELETE FROM "my table" WHERE x = 1"#,
                r#"SELECT 1 FROM "my table" WHERE x = 1"#,
            ),
            ("UPDATE `my table` SET a = 1", "SELECT 1 FROM `my table`"),
        ] {
            assert_eq!(count_preflight(sql).as_deref(), Some(want), "{sql}");
        }
    }

    #[test]
    fn carries_the_predicate_verbatim() {
        // The literal is sliced from the original, so a keyword, a `;`, or a
        // multi-byte character inside it survives exactly.
        let sql = "DELETE FROM t WHERE note = 'drop; where ünicode'";
        assert_eq!(
            count_preflight(sql).as_deref(),
            Some("SELECT 1 FROM t WHERE note = 'drop; where ünicode'")
        );
    }

    #[test]
    fn slices_at_the_top_level_where_not_a_subquery_one() {
        // The bug this guards: slicing at the subquery's WHERE would count a
        // completely different row set and report it as authoritative.
        let sql = "UPDATE t SET a = (SELECT x FROM u WHERE u.id = 1) WHERE t.id = 2";
        assert_eq!(
            count_preflight(sql).as_deref(),
            Some("SELECT 1 FROM t WHERE t.id = 2")
        );
        // A subquery inside the WHERE itself is part of the predicate and rides along.
        let sql = "DELETE FROM t WHERE id IN (SELECT id FROM u WHERE u.x = 1)";
        assert_eq!(
            count_preflight(sql).as_deref(),
            Some("SELECT 1 FROM t WHERE id IN (SELECT id FROM u WHERE u.x = 1)")
        );
    }

    #[test]
    fn declines_everything_it_cannot_rewrite_faithfully() {
        for sql in [
            // Multi-table and join forms: the rewritten FROM would not be the same
            // row set.
            "DELETE FROM a USING b WHERE a.id = b.id",
            "DELETE a FROM a JOIN b ON a.id = b.id",
            "UPDATE a, b SET a.x = b.x WHERE a.id = b.id",
            "UPDATE t SET x = u.x FROM u WHERE t.id = u.id",
            "UPDATE t JOIN u ON t.id = u.id SET t.x = 1",
            // RETURNING would be swept into the sliced tail.
            "DELETE FROM t WHERE id = 1 RETURNING *",
            // Not a mutation with a countable row set.
            "SELECT * FROM t",
            "INSERT INTO t VALUES (1)",
            "DROP DATABASE prod",
            "DROP INDEX i",
            "GRANT ALL ON t TO bob",
            // A batch has no single answer.
            "DELETE FROM a; DELETE FROM b",
            "",
        ] {
            assert_eq!(count_preflight(sql), None, "{sql}");
        }
    }
}
