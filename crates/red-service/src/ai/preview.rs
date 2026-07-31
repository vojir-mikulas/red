//! Counting what a proposed write would touch, before the user approves it.
//!
//! The write gate is structurally sound - it classifies on a noise-stripped copy,
//! rejects DDL and chained statements, insists on a real `WHERE` - and then asks
//! "Allow this?" over a statement whose blast radius is invisible. The realistic
//! answer to a question nobody can evaluate is a rubber stamp, and a
//! rubber-stamped approval is a gate that exists on paper. So before the prompt
//! goes up, this runs a `count(*)` over the statement's own predicate and pulls
//! three matched rows.
//!
//! Three rules hold it together.
//!
//! **A preview never blocks a write.** Every failure - an unreadable shape, an
//! errored count, a timeout - produces a prompt *without* a number, never no
//! prompt and never a silent zero. `matches: None` and `matches: Some(0)` are
//! deliberately different things: the second is the alarming one.
//!
//! **A preview never costs more than it is worth.** Each query runs under its own
//! short budget, well below the agent's statement timeout, because a safety
//! feature that hangs is a stall. The denominator is the most optional part and is
//! dropped first.
//!
//! **The sample rows are for the human.** They ride in the event to the panel and
//! never enter the model's context. The model gets the count, which is what it
//! needs to reason about scale; it does not need the user's data read back to it.

use std::sync::Arc;

use red_core::sql::{Dialect, write_target};
use red_core::{AiLimits, AiPolicy, AiTier};
use red_driver::PageCap;
use red_driver::{AbortSignal, DatabaseDriver};
use serde_json::Value as Json;

use super::AiBackend;
use super::gate::changeset_statements;
use super::sql::format::render_cell;
use super::util::guard_timeout;
use crate::protocol::{StatementPreview, WritePreview};

/// Ceiling on one preview query. Deliberately far below `AiLimits`'s statement
/// timeout: the user is sitting in front of a modal waiting for it, and a preview
/// that takes as long as the write itself has stopped being a preview.
const PREVIEW_BUDGET_MS: u64 = 3_000;
/// Matched rows shown. Enough to recognise "yes, those are the ones I meant" or
/// "no, that is the wrong table"; not enough to be a result set.
const SAMPLE_ROWS: usize = 3;
/// Statements previewed in one changeset. A 200-statement changeset must not fire
/// 200 count queries; past this the prompt lists the rest without counts and says
/// how many.
const CHANGESET_PREVIEW_MAX: usize = 10;

/// Preview the write `name`/`input` proposes, or `None` when there is nothing to
/// show.
///
/// `None` covers every "no preview" case: the setting is off, the backend is not
/// SQL, the tool is not one that touches rows by predicate, or nothing could be
/// counted. The caller prompts either way.
pub(in crate::ai) async fn preview_write(
    backend: &AiBackend,
    name: &str,
    input: &Json,
    policy: &AiPolicy,
) -> Option<WritePreview> {
    if !policy.preview_writes || policy.tier != AiTier::Write {
        return None;
    }
    // Only the SQL seam has a predicate to count. A Redis or Mongo write is
    // described by its own approval prompt, which already names the keys.
    let AiBackend::Sql { driver, dialect } = backend else {
        return None;
    };
    let statements: Vec<String> = match name {
        "propose_write" => vec![input.get("sql").and_then(Json::as_str)?.to_string()],
        "propose_changeset" => changeset_statements(input),
        // `create_index` and `kill_session` change server state, not rows; their
        // prompts already say exactly what they touch.
        _ => return None,
    };
    if statements.is_empty() {
        return None;
    }
    let previewed = statements.len().min(CHANGESET_PREVIEW_MAX);
    let mut out = Vec::with_capacity(previewed);
    for sql in statements.iter().take(previewed) {
        if let Some(p) = preview_statement(driver, sql, *dialect, &policy.limits).await {
            out.push(p);
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(WritePreview {
        statements: out,
        not_previewed: statements.len() - previewed,
    })
}

/// Preview one statement: count the predicate's matches, pull a few of them, and
/// (best effort) the table's total. `None` when the statement has no countable
/// shape at all - an `INSERT` matches nothing, so there is no honest number to
/// show, and inventing one would be worse than showing none.
async fn preview_statement(
    driver: &Arc<dyn DatabaseDriver>,
    sql: &str,
    dialect: Dialect,
    limits: &AiLimits,
) -> Option<StatementPreview> {
    let target = write_target(sql, dialect)?;
    let matched = format!("SELECT * FROM {} WHERE {}", target.table, target.predicate);

    let (matches, note) = match count_within(driver, &matched, budget(limits)).await {
        Ok(n) => (Some(n.max(0) as u64), None),
        Err(why) => (None, Some(why)),
    };
    // No count means the predicate didn't run, so a sample would not run either.
    let (columns, sample) = match matches {
        Some(n) if n > 0 => sample_rows(driver, &matched, budget(limits)).await,
        _ => (Vec::new(), Vec::new()),
    };
    // The denominator is the first thing to go: a bare `count(*)` on a large table
    // is precisely the query the read tools already refuse to wait on. Its absence
    // costs the prompt one clause.
    let total = match matches {
        Some(_) => count_within(
            driver,
            &format!("SELECT * FROM {}", target.table),
            budget(limits),
        )
        .await
        .ok()
        .map(|n| n.max(0) as u64),
        None => None,
    };

    Some(StatementPreview {
        table: target.table,
        matches,
        total,
        columns,
        sample,
        note,
    })
}

/// This preview's time budget: the shorter of [`PREVIEW_BUDGET_MS`] and a quarter
/// of the agent's own statement timeout, so tightening the agent's limit tightens
/// the preview too. A `0` (disabled) statement timeout still gets the ceiling;
/// "no timeout" is a choice about queries the agent runs, not about a modal the
/// user is waiting on.
fn budget(limits: &AiLimits) -> u64 {
    match limits.statement_timeout_ms {
        0 => PREVIEW_BUDGET_MS,
        ms => PREVIEW_BUDGET_MS.min(ms / 4).max(1),
    }
}

/// Count `sql`'s rows within `budget_ms`, or a short human reason why not. The
/// reason is shown to the user in place of the number, so it says what happened
/// rather than leaving a blank the eye reads as zero.
async fn count_within(
    driver: &Arc<dyn DatabaseDriver>,
    sql: &str,
    budget_ms: u64,
) -> Result<i64, String> {
    let abort = AbortSignal::new();
    guard_timeout(budget_ms, &abort, driver.count(sql, &abort))
        .await
        .map_err(|e| preview_note(&e))
}

/// Why there is no count, in words the user reads *in place of* the number.
///
/// Split out so the wording is testable without a driver that hangs, and phrased
/// as something that happened rather than as an absence: a blank where a count
/// should be gets read as zero, which is the one meaning it must never carry.
fn preview_note(e: &red_core::RedError) -> String {
    match e {
        red_core::RedError::Timeout => "could not preview (the count timed out)".to_string(),
        other => format!("could not preview ({other})"),
    }
}

/// A few matched rows, rendered and cell-capped. A failure here is silent: the
/// count is the load-bearing part, and a prompt with a number and no sample is
/// still a prompt somebody can answer.
async fn sample_rows(
    driver: &Arc<dyn DatabaseDriver>,
    sql: &str,
    budget_ms: u64,
) -> (Vec<String>, Vec<Vec<String>>) {
    let abort = AbortSignal::new();
    // `PageCap::Display` is the grid's own capping, so a BLOB column arrives as
    // `<N bytes>` rather than as megabytes of binary in an event.
    let fetch = driver.fetch_page(sql, 0, SAMPLE_ROWS, PageCap::Display { key: None }, &abort);
    let Ok(page) = guard_timeout(budget_ms, &abort, fetch).await else {
        return (Vec::new(), Vec::new());
    };
    let columns = page.columns.iter().map(|c| c.name.clone()).collect();
    let rows = page
        .rows
        .iter()
        .map(|row| row.iter().map(render_cell).collect())
        .collect();
    (columns, rows)
}

/// The note handed **to the model** about what its write would touch, or `None`
/// when there is nothing it needs to know.
///
/// Only the zero-match case earns one, and it earns it in both directions: whether
/// the user allowed or denied, a statement that matches nothing is almost always a
/// wrong predicate, a wrong table, or a stale assumption, and telling the model
/// that is execution feedback it can actually act on - unlike asking it to re-read
/// its own SQL. The counts are the model's business; the sample rows are not, and
/// never appear here.
pub(in crate::ai) fn model_note(preview: Option<&WritePreview>) -> Option<String> {
    let preview = preview?;
    let empty: Vec<String> = preview
        .statements
        .iter()
        .enumerate()
        .filter(|(_, s)| s.matches == Some(0))
        .map(|(i, s)| {
            if preview.statements.len() == 1 {
                format!("this statement matches 0 rows in {}", s.table)
            } else {
                format!("statement {} matches 0 rows in {}", i + 1, s.table)
            }
        })
        .collect();
    (!empty.is_empty()).then(|| {
        format!(
            "{}; verify the predicate before retrying - a write that matches nothing is usually \
             a wrong filter, a wrong table, or a stale assumption.",
            empty.join("; ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_core::AiTier;
    use serde_json::json;

    /// A real SQLite backend over a fixture table, so the count, the sample and
    /// the denominator are exercised through an actual engine rather than a
    /// hand-rolled double that could agree with a wrong assumption.
    fn backend() -> AiBackend {
        let db =
            std::env::temp_dir().join(format!("red-preview-{}.db", uuid::Uuid::new_v4().simple()));
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE orders (id INTEGER PRIMARY KEY, status TEXT, note TEXT);
                 INSERT INTO orders (id, status, note) VALUES
                   (1, 'paid', 'a'), (2, 'paid', 'b'), (3, 'paid', 'c'),
                   (4, 'paid', 'd'), (5, 'pending', 'e');",
            )
            .unwrap();
        }
        AiBackend::Sql {
            driver: Arc::new(red_driver::SqliteDriver::new(db, false)),
            dialect: Dialect::Sqlite,
        }
    }

    fn write_policy() -> AiPolicy {
        AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        }
    }

    async fn preview_sql(sql: &str) -> Option<WritePreview> {
        preview_write(
            &backend(),
            "propose_write",
            &json!({ "sql": sql }),
            &write_policy(),
        )
        .await
    }

    /// The number the whole prompt turns on: real matches, real denominator, and
    /// a few of the actual rows for the user to recognise.
    #[tokio::test]
    async fn counts_the_matched_rows_and_samples_them() {
        let p = preview_sql("UPDATE orders SET status = 'void' WHERE status = 'paid'")
            .await
            .expect("a plain update previews");
        assert_eq!(p.statements.len(), 1);
        assert_eq!(p.not_previewed, 0);
        let s = &p.statements[0];
        assert_eq!(s.table, "orders");
        assert_eq!(s.matches, Some(4));
        assert_eq!(s.total, Some(5));
        assert_eq!(s.note, None);
        // Capped at SAMPLE_ROWS even though four matched: this is a recognition
        // aid, not a result set.
        assert_eq!(s.sample.len(), SAMPLE_ROWS);
        assert_eq!(s.columns, ["id", "status", "note"]);
        // And whatever is in those rows, the model is told nothing about them.
        assert_eq!(model_note(Some(&p)), None);
    }

    /// Zero matches is the case a bare "Allow this?" hid best, so it has to come
    /// back as a real zero (not a missing count) and reach the model.
    #[tokio::test]
    async fn a_predicate_that_matches_nothing_reports_zero_and_tells_the_model() {
        let p = preview_sql("DELETE FROM orders WHERE status = 'refunded'")
            .await
            .expect("an empty predicate still previews");
        let s = &p.statements[0];
        assert_eq!(s.matches, Some(0));
        // No rows to show, and no sample query wasted looking for them.
        assert!(s.sample.is_empty());
        let note = model_note(Some(&p)).expect("zero matches reach the model");
        assert!(note.contains("matches 0 rows in orders"), "{note}");
    }

    /// A preview that cannot run must still produce a prompt: `matches: None` with
    /// a stated reason, never a silent zero and never a missing approval.
    #[tokio::test]
    async fn a_failed_count_still_previews_with_a_stated_reason() {
        let p = preview_sql("DELETE FROM no_such_table WHERE id = 1")
            .await
            .expect("a failing count must not swallow the preview");
        let s = &p.statements[0];
        assert_eq!(s.matches, None, "an error is not a zero");
        assert!(s.note.is_some(), "the reason has to be stated");
        assert!(s.total.is_none());
        // `None` must never be reported to the model as an empty predicate.
        assert_eq!(model_note(Some(&p)), None);
    }

    /// The wording for a missing count, without needing a driver that hangs.
    #[test]
    fn a_missing_count_says_what_happened() {
        assert!(
            preview_note(&red_core::RedError::Timeout).contains("timed out"),
            "a timeout has to name itself, or the blank reads as zero"
        );
        assert!(preview_note(&red_core::RedError::Timeout).starts_with("could not preview"));
    }

    /// A long changeset previews its head and says how much it left alone, rather
    /// than firing one count per statement.
    #[tokio::test]
    async fn a_changeset_previews_up_to_the_cap_and_says_what_it_skipped() {
        let statements: Vec<String> = (1..=CHANGESET_PREVIEW_MAX + 2)
            .map(|i| format!("UPDATE orders SET note = 'x' WHERE id = {i}"))
            .collect();
        let p = preview_write(
            &backend(),
            "propose_changeset",
            &json!({ "statements": statements }),
            &write_policy(),
        )
        .await
        .expect("a changeset previews");
        assert_eq!(p.statements.len(), CHANGESET_PREVIEW_MAX);
        assert_eq!(p.not_previewed, 2);
    }

    /// Nothing to preview: an INSERT matches no rows, a non-SQL seam has no
    /// predicate, and the setting is a real off switch.
    #[tokio::test]
    async fn skips_what_it_cannot_or_should_not_preview() {
        assert!(
            preview_sql("INSERT INTO orders (id) VALUES (99)")
                .await
                .is_none()
        );
        let off = AiPolicy {
            preview_writes: false,
            ..write_policy()
        };
        assert!(
            preview_write(
                &backend(),
                "propose_write",
                &json!({ "sql": "DELETE FROM orders WHERE id = 1" }),
                &off
            )
            .await
            .is_none()
        );
        // Server-state tools are described by their own prompt, not by a row count.
        assert!(
            preview_write(
                &backend(),
                "kill_session",
                &json!({ "id": "1" }),
                &write_policy()
            )
            .await
            .is_none()
        );
    }

    fn stmt(table: &str, matches: Option<u64>) -> StatementPreview {
        StatementPreview {
            table: table.to_string(),
            matches,
            total: None,
            columns: Vec::new(),
            sample: Vec::new(),
            note: None,
        }
    }

    /// The budget tracks the agent's own limit downward but never above the
    /// ceiling, and a disabled statement timeout doesn't mean "wait forever" for
    /// something a user is watching.
    #[test]
    fn the_preview_budget_is_short_and_tracks_the_statement_timeout() {
        let with = |ms| {
            budget(&AiLimits {
                statement_timeout_ms: ms,
                ..AiLimits::default()
            })
        };
        assert_eq!(with(0), PREVIEW_BUDGET_MS);
        assert_eq!(with(60_000), PREVIEW_BUDGET_MS);
        assert_eq!(with(4_000), 1_000);
        // Never zero, which `guard_timeout` reads as "no cap at all".
        assert_eq!(with(1), 1);
    }

    /// Zero matches is the case worth telling the model about; anything else is
    /// noise it did not ask for.
    #[test]
    fn only_a_zero_match_earns_a_note_to_the_model() {
        assert_eq!(model_note(None), None);
        let fine = WritePreview {
            statements: vec![stmt("orders", Some(4213))],
            not_previewed: 0,
        };
        assert_eq!(model_note(Some(&fine)), None);
        // A preview that couldn't run is not a zero.
        let unknown = WritePreview {
            statements: vec![stmt("orders", None)],
            not_previewed: 0,
        };
        assert_eq!(model_note(Some(&unknown)), None);

        let empty = WritePreview {
            statements: vec![stmt("orders", Some(0))],
            not_previewed: 0,
        };
        let note = model_note(Some(&empty)).expect("a zero match is reported");
        assert!(note.contains("matches 0 rows in orders"), "{note}");
        assert!(note.contains("verify the predicate"), "{note}");
    }

    /// In a changeset the model needs to know *which* statement was empty.
    #[test]
    fn a_changeset_note_numbers_the_empty_statements() {
        let mixed = WritePreview {
            statements: vec![
                stmt("accounts", Some(1)),
                stmt("invites", Some(0)),
                stmt("audit", Some(0)),
            ],
            not_previewed: 0,
        };
        let note = model_note(Some(&mixed)).expect("two zero matches are reported");
        assert!(
            note.contains("statement 2 matches 0 rows in invites"),
            "{note}"
        );
        assert!(
            note.contains("statement 3 matches 0 rows in audit"),
            "{note}"
        );
        assert!(!note.contains("statement 1"), "{note}");
    }

    /// The sample rows are the user's, not the model's. Whatever else changes, the
    /// note handed back must never carry a cell.
    #[test]
    fn the_model_note_never_carries_a_sample_cell() {
        let with_sample = WritePreview {
            statements: vec![StatementPreview {
                sample: vec![vec!["hunter2".into(), "ada@example.com".into()]],
                columns: vec!["password".into(), "email".into()],
                ..stmt("users", Some(0))
            }],
            not_previewed: 0,
        };
        let note = model_note(Some(&with_sample)).expect("a zero match is reported");
        assert!(!note.contains("hunter2"), "{note}");
        assert!(!note.contains("ada@example.com"), "{note}");
    }
}
