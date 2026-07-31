//! Telling the agent that its query answers a different question than it asked.
//!
//! Two passes over a read query, both landing in the **tool result** rather than
//! in a modal or a log. That placement is the design: the model has to reckon
//! with a tool result before it writes its answer, whereas advice it is merely
//! offered can be declined. It is also why neither pass blocks -- warnings inform,
//! and a checker that refuses to run legitimate SQL is one that gets turned off.
//!
//! The **static** pass ([`red_core::sql::inspect`]) reads the shape. It can see
//! that an aggregate sits above a join; it cannot know whether that join is
//! one-to-many.
//!
//! The **runtime** pass can, and that is the half with evidence behind it: asking
//! a model to re-read its own SQL moves accuracy a point or two, while grounding
//! a correction in what actually executed reliably helps. So on the shapes the
//! static pass flagged, one extra pair of counts says whether the join really did
//! multiply the rows, and by how much.

use std::sync::Arc;

use red_core::AiLimits;
use red_core::sql::{Dialect, ShapeWarning, join_probe};
use red_driver::{AbortSignal, DatabaseDriver};

use super::util::guard_timeout;

/// Ceiling on the fan-out probe. A quarter of the agent's own statement budget,
/// and never more than this: the probe is an annotation, and it must never be the
/// reason a query feels slow.
const PROBE_BUDGET_MS: u64 = 2_000;

/// How much bigger the joined count has to be before it is worth saying. A join
/// that adds a handful of rows is usually a few multi-row children rather than a
/// structural mistake; a factor is what makes an aggregate wrong.
const FANOUT_FACTOR: f64 = 1.05;

/// The static warnings for `sql`, rendered as the annotation block that rides in
/// front of a tool result. Empty when there is nothing to say.
///
/// `bounded` suppresses the unbounded-row note, for callers that clamp the row
/// count themselves; it is only real where the statement is handed to a grid.
pub(in crate::ai) fn static_notes(sql: &str, dialect: Dialect, bounded: bool) -> String {
    let warnings = red_core::sql::inspect(sql, dialect);
    let mut lines: Vec<String> = Vec::new();
    for warning in warnings {
        match warning {
            ShapeWarning::CrossJoin => lines.push(
                "This query joins relations with nothing correlating them, so every row of one \
                 is paired with every row of the other. If that is not deliberate, add the join \
                 predicate."
                    .into(),
            ),
            ShapeWarning::AggregateOverJoin { func, relations } => lines.push(format!(
                "This query computes {}() over a join of {relations} relations with no DISTINCT. \
                 If any of those joins is one-to-many, the aggregate counts the same row several \
                 times and the number is silently too large. Aggregate the child side first, or \
                 use {}(DISTINCT ...).",
                func.to_uppercase(),
                func.to_uppercase()
            )),
            ShapeWarning::NonEquiJoin { predicate } => lines.push(format!(
                "The join predicate `{predicate}` is not an equality. That is right for a range \
                 join and a classic accident otherwise - check it is what you meant."
            )),
            ShapeWarning::StarAcrossJoin => lines.push(
                "SELECT * across a join returns ambiguous column names and hides the result's \
                 grain. Name the columns you need."
                    .into(),
            ),
            ShapeWarning::Unbounded if !bounded => {
                lines.push("This query has no LIMIT, so it returns however many rows match.".into())
            }
            ShapeWarning::Unbounded => {}
        }
    }
    render(&lines)
}

/// The runtime fan-out annotation for `sql`, or an empty string.
///
/// Runs at most one extra pair of counts, and only when the static pass found a
/// shape where fan-out would matter. Every failure -- an unreadable shape, a
/// timeout, an engine error -- is a missing annotation and nothing more: this
/// must never be the reason a tool call fails.
pub(in crate::ai) async fn fanout_note(
    driver: &Arc<dyn DatabaseDriver>,
    sql: &str,
    dialect: Dialect,
    limits: &AiLimits,
) -> String {
    let flagged = red_core::sql::inspect(sql, dialect).into_iter().any(|w| {
        matches!(
            w,
            ShapeWarning::AggregateOverJoin { .. } | ShapeWarning::CrossJoin
        )
    });
    if !flagged {
        return String::new();
    }
    let Some(probe) = join_probe(sql, dialect) else {
        return String::new();
    };
    let budget = budget(limits);
    let (Some(base), Some(joined)) = (
        count_within(driver, &probe.base, budget).await,
        count_within(driver, &probe.joined, budget).await,
    ) else {
        return String::new();
    };
    if base == 0 || (joined as f64) < base as f64 * FANOUT_FACTOR {
        // One-to-one (or better): the aggregate above it is sound, and saying
        // nothing is the right answer. A warning on a correct query is exactly
        // what teaches a reader to stop reading them.
        return String::new();
    }
    render(&[format!(
        "The driving table `{}` matches {base} row(s) for this filter, but the join produced \
         {joined}. The join IS one-to-many, so any SUM/COUNT/AVG above it counts the same \
         `{}` row about {:.1} times over. This number is wrong unless you aggregate the child \
         side first or use COUNT(DISTINCT ...).",
        probe.table,
        probe.table,
        joined as f64 / base as f64
    )])
}

/// This probe's budget: a quarter of the agent's statement timeout, capped at
/// [`PROBE_BUDGET_MS`]. A disabled (`0`) statement timeout still gets the cap;
/// "no timeout" is a choice about the query the agent asked for, not about an
/// annotation nobody requested.
fn budget(limits: &AiLimits) -> u64 {
    match limits.statement_timeout_ms {
        0 => PROBE_BUDGET_MS,
        ms => PROBE_BUDGET_MS.min(ms / 4).max(1),
    }
}

/// `sql`'s row count within `budget_ms`, or `None` for any failure at all.
async fn count_within(driver: &Arc<dyn DatabaseDriver>, sql: &str, budget_ms: u64) -> Option<u64> {
    let abort = AbortSignal::new();
    guard_timeout(budget_ms, &abort, driver.count(sql, &abort))
        .await
        .ok()
        .map(|n| n.max(0) as u64)
}

/// Wrap annotation lines in the block that precedes a tool result.
fn render(lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "{} - read this before you interpret the numbers:\n",
        crate::ai::turn::SHAPE_CHECK_PREFIX
    );
    for line in lines {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wording has to name the mistake and the fix, because it is read by
    /// something that will otherwise write a confident sentence around a wrong
    /// number.
    #[test]
    fn an_aggregate_over_a_join_is_called_out_with_its_remedy() {
        let note = static_notes(
            "SELECT SUM(o.total) FROM orders o JOIN items i ON i.order_id = o.id",
            Dialect::Generic,
            true,
        );
        assert!(note.contains("SHAPE CHECK"), "{note}");
        assert!(note.contains("SUM()"), "{note}");
        assert!(note.contains("one-to-many"), "{note}");
        assert!(note.contains("DISTINCT"), "{note}");
    }

    /// A sound query gets nothing. This is the assertion that keeps the feature
    /// worth having: an annotation on every query is an annotation on none.
    #[test]
    fn a_sound_query_is_not_annotated() {
        assert_eq!(
            static_notes("SELECT SUM(total) FROM orders", Dialect::Generic, true),
            ""
        );
        assert_eq!(
            static_notes(
                "SELECT o.id FROM orders o JOIN items i ON i.order_id = o.id",
                Dialect::Generic,
                true
            ),
            ""
        );
    }

    /// The row-bound note is for callers that hand the statement onward, not for
    /// the agent's own reads, which are clamped anyway.
    #[test]
    fn the_unbounded_note_respects_who_is_asking() {
        assert_eq!(static_notes("SELECT x FROM a", Dialect::Generic, true), "");
        assert!(
            static_notes("SELECT x FROM a", Dialect::Generic, false).contains("no LIMIT"),
            "an unclamped caller is told"
        );
    }

    /// A fixture where `orders` has 2 rows and `items` has 3 children of the
    /// first order, so `orders JOIN items` genuinely fans out.
    fn fanout_driver() -> Arc<dyn DatabaseDriver> {
        let db =
            std::env::temp_dir().join(format!("red-fanout-{}.db", uuid::Uuid::new_v4().simple()));
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, status TEXT);
                 CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER, sku TEXT);
                 CREATE TABLE shipping (order_id INTEGER PRIMARY KEY, carrier TEXT);
                 INSERT INTO orders VALUES (1, 100, 'paid'), (2, 200, 'paid');
                 INSERT INTO items VALUES (1, 1, 'a'), (2, 1, 'b'), (3, 1, 'c');
                 INSERT INTO shipping VALUES (1, 'ups'), (2, 'dhl');",
            )
            .unwrap();
        }
        Arc::new(red_driver::SqliteDriver::new(db, true))
    }

    /// The half with evidence behind it: a real one-to-many join is caught with
    /// the real counts, so the model is corrected by what executed rather than by
    /// being asked to re-read its own SQL.
    #[tokio::test]
    async fn a_real_one_to_many_join_is_caught_with_its_counts() {
        let note = fanout_note(
            &fanout_driver(),
            "SELECT SUM(o.total) FROM orders o JOIN items i ON i.order_id = o.id",
            Dialect::Sqlite,
            &AiLimits::default(),
        )
        .await;
        // 2 driving rows, 3 joined rows.
        assert!(note.contains("matches 2 row(s)"), "{note}");
        assert!(note.contains("but the join produced 3"), "{note}");
        assert!(note.contains("IS one-to-many"), "{note}");
    }

    /// A one-to-one join produces nothing. This is the assertion that keeps the
    /// probe from becoming noise: it fires on the shape *and* the evidence, never
    /// on the shape alone.
    #[tokio::test]
    async fn a_one_to_one_join_is_not_annotated() {
        let note = fanout_note(
            &fanout_driver(),
            "SELECT SUM(o.total) FROM orders o JOIN shipping s ON s.order_id = o.id",
            Dialect::Sqlite,
            &AiLimits::default(),
        )
        .await;
        assert_eq!(note, "", "a sound join must not be annotated");
    }

    /// A probe that cannot run leaves the result alone. It is an annotation, and
    /// a failed annotation is a missing one, never a failed tool.
    #[tokio::test]
    async fn a_failing_probe_is_silent() {
        let note = fanout_note(
            &fanout_driver(),
            "SELECT SUM(o.total) FROM no_such_table o JOIN items i ON i.order_id = o.id",
            Dialect::Sqlite,
            &AiLimits::default(),
        )
        .await;
        assert_eq!(note, "");
        // And a shape the static pass never flagged is never probed at all.
        assert_eq!(
            fanout_note(
                &fanout_driver(),
                "SELECT SUM(total) FROM orders",
                Dialect::Sqlite,
                &AiLimits::default()
            )
            .await,
            ""
        );
    }

    #[test]
    fn the_probe_budget_is_short_and_tracks_the_statement_timeout() {
        let with = |ms| {
            budget(&AiLimits {
                statement_timeout_ms: ms,
                ..AiLimits::default()
            })
        };
        assert_eq!(with(0), PROBE_BUDGET_MS);
        assert_eq!(with(60_000), PROBE_BUDGET_MS);
        assert_eq!(with(4_000), 1_000);
        // Never zero, which `guard_timeout` reads as "no cap at all".
        assert_eq!(with(1), 1);
    }
}
