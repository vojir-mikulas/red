//! The SQL seam's agent tools: the `DatabaseDriver` half of the catalog.
//!
//! [`run_tool`] is the executor -- one arm per tool, each re-checking the tier
//! and re-vetting any write before it runs, so no caller can reach a tool the
//! policy withholds. The tool *definitions* and the system prompt that
//! introduces them live in [`catalog`], the composed reads in [`tools`], the
//! two comparisons in [`diff`], and every driver-type-to-text rendering in
//! [`format`].

use std::sync::Arc;
use std::time::Duration;

use red_ai::CancelToken;
use red_core::sql::Dialect;
use red_core::{AiPolicy, RedError};
use red_driver::{AbortSignal, DatabaseDriver, PageCap};
use serde_json::Value as Json;

use super::export::export_result;
use super::gate::changeset_statements;
use super::gate::{WriteAssessment, assess_write, is_read_only_select};
use super::report::run_generate_report;
use super::state::ReportSink;
use super::util::{cap_result_bytes, guard_timeout};
use diff::{diff_data, diff_schema};
use format::{
    format_health, format_page, format_plan, format_schema, format_sessions, format_table_detail,
};
use tools::{
    index_args, kill_session, profile_table, relationship_map, search_data, suggest_index,
};

pub(super) mod catalog;
pub(super) mod diff;
pub(super) mod format;
pub(super) mod tools;

pub(in crate::ai) async fn run_tool(
    driver: &Arc<dyn DatabaseDriver>,
    dialect: Dialect,
    name: &str,
    input: &Json,
    policy: &AiPolicy,
    _cancel: &CancelToken,
    report: &ReportSink,
) -> (String, bool) {
    // Defense in depth: refuse a tool the tier doesn't expose, even if the model
    // somehow asks for it by name.
    if !policy.tier.allows_tool(name) {
        return (
            format!("error: the `{name}` tool is not available at this access tier"),
            false,
        );
    }
    let limits = &policy.limits;
    let (content, ok) = match name {
        "list_schema" => match driver.list_objects().await {
            Ok(schemas) => (format_schema(&schemas), true),
            Err(e) => (format!("error: {e}"), false),
        },
        "describe_table" => {
            let schema = input.get("schema").and_then(Json::as_str).unwrap_or("");
            let table = input.get("table").and_then(Json::as_str).unwrap_or("");
            if table.is_empty() {
                return ("error: `table` is required".into(), false);
            }
            match driver.describe_table(schema, table).await {
                Ok(detail) => (format_table_detail(schema, table, &detail), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "profile_table" => {
            let schema = input.get("schema").and_then(Json::as_str).unwrap_or("");
            let table = input.get("table").and_then(Json::as_str).unwrap_or("");
            if table.is_empty() {
                return ("error: `table` is required".into(), false);
            }
            profile_table(driver, schema, table, limits).await
        }
        "relationship_map" => relationship_map(driver, input).await,
        "run_select" => {
            let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
            if !is_read_only_select(sql, dialect) {
                return (
                    "error: only a single SELECT or WITH...SELECT query is allowed".into(),
                    false,
                );
            }
            // Clamp the requested LIMIT to the hard row cap (the model browses, it
            // doesn't bulk-export) and remember whether we clamped so the result
            // can tell the model it's partial.
            let max_rows = limits.max_rows.max(1);
            let requested = input
                .get("limit")
                .and_then(Json::as_u64)
                .map(|n| n as usize);
            let limit = requested.unwrap_or(max_rows).clamp(1, max_rows);
            let abort = AbortSignal::new();
            // Fetch one extra row so a result that's exactly `limit` long (complete)
            // is told apart from one that genuinely has more rows (truncated). The
            // probe row is dropped before the page is shown to the model.
            let probe = limit.saturating_add(1);
            let fetch = driver.fetch_page(sql, 0, probe, PageCap::Display { key: None }, &abort);
            match guard_timeout(limits.statement_timeout_ms, &abort, fetch).await {
                Ok(mut page) => {
                    let truncated = page.rows.len() > limit;
                    page.rows.truncate(limit);
                    let mut out = format_page(&page);
                    if truncated {
                        out.push_str(&format!(
                            "\n(truncated to {limit} rows: the result may have more; add LIMIT or \
                            a WHERE clause to narrow it)"
                        ));
                    }
                    (out, true)
                }
                Err(RedError::Timeout) => (
                    "error: the query exceeded the agent's statement timeout, so it was \
                    cancelled. Narrow it (add WHERE/LIMIT) or inspect the plan with explain."
                        .into(),
                    false,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "explain" => {
            let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
            if sql.is_empty() {
                return ("error: `sql` is required".into(), false);
            }
            let analyze = input
                .get("analyze")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            // `EXPLAIN ANALYZE` *executes* on Postgres and MySQL 8.0.18+, so an
            // analyze request is a run request and is graded as one. Anything above
            // `Safe` is refused outright rather than prompted: the model asked to
            // read a plan, and a user asked to approve a write here would be
            // approving something they did not request. `risk::assess` handles both
            // a bare statement and one the model already wrapped in EXPLAIN.
            if analyze {
                let verdict = red_core::sql::risk::assess(sql, dialect);
                if verdict.level != red_core::sql::risk::RiskLevel::Safe {
                    return (
                        "error: EXPLAIN ANALYZE executes the statement, and this one is not a \
                         read. Explain it without `analyze` to see the plan, or run the change \
                         yourself in a query tab."
                            .into(),
                        false,
                    );
                }
            }
            // Bound the wait like run_select. The trait gives `explain` no abort
            // seam, so on timeout we hand the model a clean error while the engine's
            // call winds down on its own; the read-only gate above is what keeps an
            // `analyze` from running away with anything but time.
            let explain = driver.explain(sql, analyze);
            let result = match limits.statement_timeout_ms {
                0 => explain.await,
                ms => tokio::time::timeout(Duration::from_millis(ms), explain)
                    .await
                    .unwrap_or(Err(RedError::Timeout)),
            };
            match result {
                Ok(plan) => (format_plan(&plan), true),
                Err(RedError::Timeout) => (
                    "error: the EXPLAIN exceeded the agent's statement timeout; \
                     simplify the statement."
                        .into(),
                    false,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "object_ddl" => {
            let schema = input.get("schema").and_then(Json::as_str).unwrap_or("");
            let name = input.get("name").and_then(Json::as_str).unwrap_or("");
            if name.is_empty() {
                return ("error: `name` is required".into(), false);
            }
            let token = input.get("kind").and_then(Json::as_str).unwrap_or("table");
            let Some(kind) = red_core::ObjectKind::from_token(token) else {
                return (
                    format!(
                        "error: unknown object kind `{token}`; use one of table/view/matview/\
                         function/procedure/trigger/sequence/type"
                    ),
                    false,
                );
            };
            match driver.object_ddl(schema, name, kind).await {
                Ok(ddl) => (ddl, true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "search_data" => search_data(driver, input, limits).await,
        "health_report" => {
            let namespace = input
                .get("schema")
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match driver.health(namespace).await {
                Ok(report) => (
                    cap_result_bytes(format_health(&report), limits.max_result_bytes),
                    true,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "server_sessions" => match driver.server_sessions().await {
            Ok((sessions, restricted)) => (
                cap_result_bytes(
                    format_sessions(&sessions, restricted),
                    limits.max_result_bytes,
                ),
                true,
            ),
            Err(e) => (format!("error: {e}"), false),
        },
        "export_result" => export_result(driver, dialect, input, report).await,
        "generate_report" => run_generate_report(input, report),
        "open_query" => {
            let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
            if sql.is_empty() {
                return ("error: open_query needs `sql`".into(), false);
            }
            // Hand the SQL to the UI, which opens a new query tab (and runs it if it's
            // a read-only SELECT). Nothing executes here.
            report.announce_open_query(sql);
            (
                "Opened the query in a new editor tab in the user's workspace.".into(),
                true,
            )
        }
        "save_query" => {
            let name = input
                .get("name")
                .and_then(Json::as_str)
                .unwrap_or("")
                .trim();
            let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
            if name.is_empty() || sql.is_empty() {
                return (
                    "error: save_query needs a non-empty `name` and `sql`".into(),
                    false,
                );
            }
            let description = input
                .get("description")
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|d| !d.is_empty());
            // Hand it to the UI, which writes the `.sql` file into the saved-queries
            // library. Nothing executes here.
            report.announce_save_query(name, description, sql);
            (
                format!("Saved the query as “{name}” to the user's saved-queries library."),
                true,
            )
        }
        "diff_schema" => diff_schema(driver, input, limits).await,
        "diff_data" => diff_data(driver, input, limits).await,
        "suggest_index" => suggest_index(driver, input, limits).await,
        "kill_session" => kill_session(driver, input).await,
        "create_index" => {
            let (table, name, columns, unique) = match index_args(input) {
                Ok(a) => a,
                Err(why) => return (format!("error: {why}"), false),
            };
            match driver.create_index(&table, &name, unique, &columns).await {
                Ok(_) => (format!("Created index {name} on {}.", table.name), true),
                Err(e) => (format!("error: creating the index failed: {e}"), false),
            }
        }
        "propose_write" => {
            // Re-vet at execution (defense in depth): tier, read-only, and the
            // statement shape are all re-checked, never trusting that the caller
            // already gated it. By here the per-call user approval has been granted
            // (run_turn / the ACP permission flow); we only *run* an allowed shape.
            match assess_write(name, input, policy, dialect) {
                WriteAssessment::NeedsApproval { sql } => match driver.execute(&sql).await {
                    Ok(affected) => {
                        // Durable record of what the agent actually changed.
                        crate::audit::record_write(&sql, affected);
                        (
                            format!(
                                "Executed the write: {affected} row(s) affected. Verify with a \
                                 SELECT if it matters."
                            ),
                            true,
                        )
                    }
                    Err(e) => (format!("error: the write failed: {e}"), false),
                },
                WriteAssessment::Reject(why) => (format!("error: {why}"), false),
                WriteAssessment::NotWrite => (
                    "error: propose_write needs an INSERT/UPDATE/DELETE statement".into(),
                    false,
                ),
            }
        }
        "propose_changeset" => {
            // Re-vet at execution (defense in depth), then run the whole set through
            // `execute_batch`: one transaction where the engine has them (all commit
            // or none do), sequential on ClickHouse, which has none. Approval was
            // already granted above.
            match assess_write(name, input, policy, dialect) {
                WriteAssessment::NeedsApproval { .. } => {
                    let statements = changeset_statements(input);
                    match driver.execute_batch(&statements).await {
                        Ok(affected) => {
                            // Audit each executed statement with its own row count.
                            for (stmt, rows) in statements.iter().zip(&affected) {
                                crate::audit::record_write(stmt, *rows);
                            }
                            let total: u64 = affected.iter().sum();
                            (
                                format!(
                                    "Executed the changeset: {} statement(s), {total} row(s) \
                                     affected. Verify with a SELECT if it matters.",
                                    statements.len()
                                ),
                                true,
                            )
                        }
                        Err(e) => (
                            format!(
                                "error: the changeset failed: {e}. On an engine with \
                                 transactions it was rolled back and nothing changed; on \
                                 ClickHouse the statements before the failure may have applied, \
                                 so verify with a SELECT."
                            ),
                            false,
                        ),
                    }
                }
                WriteAssessment::Reject(why) => (format!("error: {why}"), false),
                WriteAssessment::NotWrite => (
                    "error: propose_changeset needs a `statements` array".into(),
                    false,
                ),
            }
        }
        other => (format!("error: unknown tool `{other}`"), false),
    };
    (cap_result_bytes(content, limits.max_result_bytes), ok)
}

pub(crate) use catalog::user_turn;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;

    use crate::ai::state::ReportSink;
    use crate::protocol::ConversationId;
    use red_ai::CancelToken;
    use red_core::AiPolicy;
    use red_core::AiTier;
    use red_driver::DatabaseDriver;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn changeset_runs_atomically_and_rolls_back_on_error() {
        let db = std::env::temp_dir().join(format!("red-cs-{}.db", uuid::Uuid::new_v4().simple()));
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER);
                 INSERT INTO t VALUES (1, 10);",
            )
            .unwrap();
        }
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, false));
        let policy = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        let read_n = |driver: Arc<dyn DatabaseDriver>| async move {
            let abort = AbortSignal::new();
            let page = driver
                .fetch_page(
                    "SELECT n FROM t WHERE id = 1",
                    0,
                    1,
                    PageCap::Display { key: None },
                    &abort,
                )
                .await
                .unwrap();
            page.rows[0][0].to_string()
        };

        // Success: both statements commit together.
        let (content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
            "propose_changeset",
            &json!({ "statements": [
                "UPDATE t SET n = 20 WHERE id = 1",
                "INSERT INTO t VALUES (2, 30)",
            ] }),
            &policy,
            &CancelToken::new(),
            &ReportSink::disabled(),
        )
        .await;
        assert!(ok, "expected success, got: {content}");
        assert_eq!(read_n(driver.clone()).await, "20");

        // Failure: the second statement conflicts on the PK, so the whole batch rolls
        // back — the first UPDATE must NOT stick (n stays 20, not 99).
        let (content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
            "propose_changeset",
            &json!({ "statements": [
                "UPDATE t SET n = 99 WHERE id = 1",
                "INSERT INTO t VALUES (2, 40)",
            ] }),
            &policy,
            &CancelToken::new(),
            &ReportSink::disabled(),
        )
        .await;
        assert!(!ok, "expected failure, got: {content}");
        assert!(content.contains("rolled back"), "got: {content}");
        assert_eq!(
            read_n(driver.clone()).await,
            "20",
            "the batch must be atomic"
        );
    }

    /// `EXPLAIN ANALYZE` executes on Postgres and MySQL 8.0.18+, so an `analyze`
    /// over a write must be refused. Asserted **per dialect**: the lexing differs,
    /// and grading against the wrong one is the exact drift `risk.rs` exists to
    /// prevent.
    #[tokio::test]
    async fn explain_analyze_refuses_anything_that_is_not_a_read() {
        let db = std::env::temp_dir().join(format!("red-xa-{}.db", uuid::Uuid::new_v4().simple()));
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER);")
                .unwrap();
        }
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let explain = async |sql: &str, analyze: bool, dialect: Dialect| {
            run_tool(
                &driver,
                dialect,
                "explain",
                &json!({ "sql": sql, "analyze": analyze }),
                &AiPolicy::default(),
                &CancelToken::new(),
                &ReportSink::disabled(),
            )
            .await
        };
        for dialect in [
            Dialect::Generic,
            Dialect::Postgres,
            Dialect::MySql,
            Dialect::Sqlite,
            Dialect::ClickHouse,
        ] {
            for sql in [
                "UPDATE t SET x = 1",
                "DELETE FROM t WHERE id = 1",
                "DROP TABLE t",
                // Already wrapped by the model: `risk::assess` grades the inner
                // statement, so this must be refused too.
                "EXPLAIN ANALYZE DELETE FROM t WHERE id = 1",
            ] {
                let (content, ok) = explain(sql, true, dialect).await;
                assert!(!ok, "{dialect:?} / {sql} must be refused, got: {content}");
                assert!(content.contains("executes the statement"), "{content}");
            }
            // Without `analyze` the same statement only plans, so it is allowed.
            let (_, ok) = explain("UPDATE t SET x = 1", false, dialect).await;
            assert!(ok, "{dialect:?}: plain explain of a write must still plan");
        }
        // A read with actuals is the point of the flag, and is allowed.
        let (content, ok) = explain("SELECT * FROM t", true, Dialect::Sqlite).await;
        assert!(ok, "{content}");
    }

    #[tokio::test]
    async fn object_ddl_returns_a_view_body_describe_table_cannot() {
        let db = std::env::temp_dir().join(format!("red-ddl-{}.db", uuid::Uuid::new_v4().simple()));
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER);
                 CREATE VIEW big AS SELECT id FROM t WHERE x > 100;",
            )
            .unwrap();
        }
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let ddl = async |name: &str, kind: Json| {
            run_tool(
                &driver,
                Dialect::Sqlite,
                "object_ddl",
                &json!({ "schema": "main", "name": name, "kind": kind }),
                &AiPolicy::default(),
                &CancelToken::new(),
                &ReportSink::disabled(),
            )
            .await
        };
        let (content, ok) = ddl("big", json!("view")).await;
        assert!(ok, "{content}");
        assert!(content.contains("x > 100"), "view body missing: {content}");
        // An unknown kind is a clean error the model can correct, not a panic.
        let (content, ok) = ddl("big", json!("widget")).await;
        assert!(!ok);
        assert!(content.contains("unknown object kind"), "{content}");
    }

    #[tokio::test]
    async fn save_query_announces_a_save_with_name_and_description() {
        use futures::StreamExt;

        // save_query never touches the DB (it hands the file write to the UI); a
        // throwaway driver is enough.
        let db = std::env::temp_dir().join(format!("red-sq-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, ConversationId::new(42), None, None);

        let (content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
            "save_query",
            &json!({
                "name": "Monthly revenue",
                "sql": "SELECT month, sum(amount) FROM sales WHERE month = :month GROUP BY month",
                "description": "Revenue for a given :month",
            }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(ok, "expected success, got: {content}");
        assert!(content.contains("Monthly revenue"));

        let (_session, event) = rx.next().await.expect("an AiSaveQuery event");
        let Event::AiSaveQuery {
            conversation_id,
            name,
            description,
            sql,
        } = event
        else {
            panic!("expected AiSaveQuery, got {event:?}");
        };
        assert_eq!(conversation_id.get(), 42);
        assert_eq!(name, "Monthly revenue");
        assert_eq!(description.as_deref(), Some("Revenue for a given :month"));
        assert!(sql.contains(":month"));

        // Missing name or sql is refused, and nothing is announced.
        let (_content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
            "save_query",
            &json!({ "name": "", "sql": "SELECT 1" }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(!ok);
        assert!(rx.try_recv().is_err(), "a refused save must not announce");
    }
}
