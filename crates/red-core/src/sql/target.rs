//! What an `UPDATE`/`DELETE` would touch: the table it targets and the predicate
//! that selects the rows, extracted so a caller can count the affected rows
//! before running it.
//!
//! "Allow this?" over `UPDATE orders SET status = 'void' WHERE created_at <
//! '2025-01-01'` is not a question anyone can answer: the number it turns on is
//! invisible, so the realistic outcome is a rubber stamp, and a rubber-stamped
//! approval is a gate that exists on paper. This module is the pure half of
//! showing that number.
//!
//! Two rules, both inherited from [`risk`](super::risk) and both load-bearing.
//!
//! **Reason over stripped SQL.** Every scan runs on a [`strip_noise`] copy, so a
//! `where` inside a string literal or a `--` comment is invisible. Because that
//! copy is *byte-offset preserving*, an offset found in it indexes the original
//! at the same place, which is how the predicate comes back with its literals
//! intact.
//!
//! **Refuse when unsure.** Any shape this cannot read confidently -- a joined or
//! multi-table update, a `USING` clause, a subquery in the table position, a
//! `LIMIT` that makes the match count not the affected count -- returns `None`.
//! `None` means *no preview*, never *no approval*: the caller still prompts, just
//! without a number. That is the only safe direction for this to fail in.

use super::{Dialect, first_keyword, has_top_level_comma, strip_noise, top_level_word};

/// The table an `UPDATE`/`DELETE` targets and the text of its `WHERE` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteTarget {
    /// The table name exactly as written, qualification and quoting included, so
    /// it can go straight back into a generated `SELECT`.
    pub table: String,
    /// The `WHERE` clause **from the original SQL**, literals intact.
    pub predicate: String,
}

/// The table and predicate of a single `UPDATE`/`DELETE`, or `None` when there is
/// no preview to be had.
///
/// `None` for an `INSERT` (nothing is being matched, so there is no count to
/// show), for a statement whose shape can't be read confidently, and for anything
/// that isn't a single data-modifying statement.
pub fn write_target(sql: &str, dialect: Dialect) -> Option<WriteTarget> {
    let stripped = strip_noise(sql, dialect);
    // `first_keyword` reports the verb as written, so fold it before matching.
    let verb = first_keyword(&stripped).to_ascii_lowercase();
    if verb != "update" && verb != "delete" {
        return None;
    }
    let lower = stripped.to_ascii_lowercase();

    // The predicate starts at the first top-level `where`; a `where` nested in a
    // subquery (`SET x = (SELECT … WHERE …)`) is not this statement's filter.
    let where_at = top_level_word(&lower, "where")?;
    let head = &lower[..where_at];

    // Shapes with a second relation in play: the row count of a join is not the
    // row count of the target, and the predicate alone can't reproduce it.
    //
    // Scanned at top level over the head only, so neither an ordinary
    // `… WHERE id IN (SELECT … FROM …)` nor a subquery inside a `SET` expression
    // is mistaken for one -- their `FROM` belongs to the subquery, not to this
    // statement.
    if top_level_word(head, "join").is_some() || top_level_word(head, "using").is_some() {
        return None;
    }
    match verb.as_str() {
        // Postgres `UPDATE t SET … FROM other WHERE …` is a join by another name.
        "update" => {
            let set_at = top_level_word(head, "set")?;
            // Postgres' `UPDATE … SET … FROM other` reads as an ordinary update
            // right up to the row count.
            if top_level_word(head, "from").is_some() {
                return None;
            }
            // `UPDATE a, b SET …`: a comma between the verb and `SET` is a second
            // table. (Commas *inside* the SET list are expected, hence the bound.)
            if has_top_level_comma(&lower[..set_at]) {
                return None;
            }
        }
        // `DELETE a, b FROM …`, or MySQL's multi-table form.
        _ => {
            if has_top_level_comma(head) {
                return None;
            }
        }
    }

    // A trailing `LIMIT` caps what the statement actually changes, so the
    // predicate's match count would overstate it. Better no number than a wrong
    // one. `ORDER BY` and `RETURNING` don't change the count, so they are simply
    // cut off the predicate.
    let tail = top_level_word(&lower, "limit");
    if tail.is_some_and(|at| at > where_at) {
        return None;
    }
    let end = ["returning", "order"]
        .iter()
        .filter_map(|w| top_level_word(&lower, w))
        .filter(|at| *at > where_at)
        .min()
        .unwrap_or(sql.len().min(lower.len()));

    // `where` is 5 bytes, and the stripped copy indexes the original one-to-one.
    let predicate = sql
        .get(where_at + 5..end)?
        .trim()
        .trim_end_matches(';')
        .trim();
    if predicate.is_empty() {
        return None;
    }
    Some(WriteTarget {
        table: super::risk::target_object(sql)?,
        predicate: predicate.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(sql: &str, dialect: Dialect) -> Option<(String, String)> {
        write_target(sql, dialect).map(|t| (t.table, t.predicate))
    }

    /// The shape this exists for, on every dialect: the table comes back
    /// qualified as written and the predicate keeps its literals.
    #[test]
    fn reads_a_plain_update_and_delete_on_every_dialect() {
        for dialect in [
            Dialect::Generic,
            Dialect::Postgres,
            Dialect::MySql,
            Dialect::Sqlite,
            Dialect::ClickHouse,
        ] {
            assert_eq!(
                target(
                    "UPDATE public.orders SET status = 'void' WHERE created_at < '2025-01-01'",
                    dialect
                ),
                Some((
                    "public.orders".to_string(),
                    "created_at < '2025-01-01'".to_string()
                )),
                "{dialect:?}"
            );
            assert_eq!(
                target("delete from orders where id = 7", dialect),
                Some(("orders".to_string(), "id = 7".to_string())),
                "{dialect:?}"
            );
        }
    }

    /// The reason the input is the stripped copy: a literal containing the word
    /// `where` must not be mistaken for the clause. Getting this wrong would
    /// count the wrong rows and show the user a confident, false number.
    #[test]
    fn a_where_inside_a_literal_or_comment_is_not_the_clause() {
        assert_eq!(
            target(
                "UPDATE notes SET body = 'see where it lands' WHERE id = 3",
                Dialect::Generic
            ),
            Some(("notes".to_string(), "id = 3".to_string()))
        );
        assert_eq!(
            target(
                "DELETE FROM notes -- where did this come from\n WHERE id = 3",
                Dialect::Generic
            ),
            Some(("notes".to_string(), "id = 3".to_string()))
        );
        // And the predicate comes out of the *original*, so its own literals
        // survive intact rather than arriving blanked.
        assert_eq!(
            target(
                "DELETE FROM notes WHERE body = 'where to?'",
                Dialect::Generic
            ),
            Some(("notes".to_string(), "body = 'where to?'".to_string()))
        );
    }

    /// A subquery predicate is handed back whole: it goes straight to the engine,
    /// which understands it perfectly well.
    #[test]
    fn a_subquery_predicate_survives_whole() {
        assert_eq!(
            target(
                "DELETE FROM invites WHERE account_id IN (SELECT id FROM accounts WHERE tier = 'free')",
                Dialect::Postgres
            ),
            Some((
                "invites".to_string(),
                "account_id IN (SELECT id FROM accounts WHERE tier = 'free')".to_string()
            ))
        );
        // The statement's own filter is the *top-level* one, not a nested one.
        assert_eq!(
            target(
                "UPDATE t SET x = (SELECT max(y) FROM u WHERE u.a = 1) WHERE t.id = 2",
                Dialect::Generic
            ),
            Some(("t".to_string(), "t.id = 2".to_string()))
        );
    }

    /// Every shape with a second relation in play refuses, because the target's
    /// row count is not the join's row count.
    #[test]
    fn multi_relation_shapes_refuse() {
        for sql in [
            "UPDATE a, b SET a.x = b.x WHERE a.id = b.id",
            "UPDATE orders o SET status = 'x' FROM accounts a WHERE a.id = o.account_id",
            "DELETE FROM a USING b WHERE a.id = b.id",
            "DELETE a FROM a JOIN b ON a.id = b.id WHERE b.x = 1",
        ] {
            assert_eq!(target(sql, Dialect::Generic), None, "{sql}");
        }
    }

    /// A `LIMIT` caps what actually changes, so the predicate's count would
    /// overstate it: refuse rather than show a wrong number.
    #[test]
    fn a_limited_write_refuses_rather_than_overstating() {
        assert_eq!(
            target(
                "DELETE FROM logs WHERE day < '2024-01-01' LIMIT 100",
                Dialect::MySql
            ),
            None
        );
        // `ORDER BY` and `RETURNING` don't change the count, so they are trimmed
        // off the predicate instead.
        assert_eq!(
            target(
                "DELETE FROM logs WHERE day < '2024-01-01' RETURNING id",
                Dialect::Postgres
            ),
            Some(("logs".to_string(), "day < '2024-01-01'".to_string()))
        );
        assert_eq!(
            target(
                "UPDATE logs SET seen = true WHERE day < '2024-01-01' ORDER BY id",
                Dialect::Generic
            ),
            Some(("logs".to_string(), "day < '2024-01-01'".to_string()))
        );
    }

    /// Nothing to preview: an INSERT matches no rows, and a read isn't a write.
    #[test]
    fn shapes_with_no_count_to_show_return_none() {
        for sql in [
            "INSERT INTO t (a) VALUES (1)",
            "SELECT * FROM t WHERE a = 1",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "",
            "   ",
        ] {
            assert_eq!(target(sql, Dialect::Generic), None, "{sql:?}");
        }
    }
}
