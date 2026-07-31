//! The SQL seam's composed reads and its two server-state writes.
//!
//! Everything here is a *composition* over `DatabaseDriver` methods rather than
//! a new seam: `profile_table` is one pushed-down aggregate per column,
//! `search_data` is the driver's own contains-predicate fed through the windowed
//! `fetch_page`, `relationship_map` is the FK graph the ER canvas draws, and
//! `suggest_index` is `explain` plus `describe_table` with an opinion. The two
//! that mutate (`kill_session`, and the `create_index` arguments) re-resolve
//! their target against the live server before acting, because an approval names
//! a specific session and a pid can be recycled.

use std::sync::Arc;
use std::time::Duration;

use red_core::{AiLimits, FkEdge, RedError, TableRef};
use red_driver::{AbortSignal, DatabaseDriver, PageCap};
use serde_json::Value as Json;

use super::super::gate::kill_mode;
use super::super::util::{cap_result_bytes, guard_timeout};
use super::format::{fk_side, format_page, format_plan, qualified};

/// Cap on columns profiled in one `profile_table` call: each column is one
/// pushed-down aggregate query, so a very wide table is truncated (and says so) to
/// keep the tool bounded.
const MAX_PROFILE_COLUMNS: usize = 40;
/// Above this row count, skip the potentially-expensive per-column `count(distinct)`
/// (reported as "not computed"), mirroring the grid's own distinct guard.
const PROFILE_DISTINCT_MAX_ROWS: i64 = 1_000_000;
/// Implement the `profile_table` tool: describe the table, push down a per-column
/// aggregate profile (nulls, distinct, min/max, sum/avg), and summarize its
/// foreign-key relationships. Read-only; returns a compact text report, never rows.
pub(in crate::ai) async fn profile_table(
    driver: &Arc<dyn DatabaseDriver>,
    schema: &str,
    table: &str,
    limits: &AiLimits,
) -> (String, bool) {
    use std::fmt::Write;

    let detail = match driver.describe_table(schema, table).await {
        Ok(d) => d,
        Err(e) => return (format!("error: {e}"), false),
    };
    let table_ref = TableRef {
        schema: (!schema.is_empty()).then(|| schema.to_string()),
        name: table.to_string(),
    };
    let base_sql = format!("SELECT * FROM {}", driver.quote_table(&table_ref));

    // Count once up front so we can decide whether per-column count(distinct) is
    // affordable, and report the table's size.
    let abort = AbortSignal::new();
    let total = match guard_timeout(
        limits.statement_timeout_ms,
        &abort,
        driver.count(&base_sql, &abort),
    )
    .await
    {
        Ok(n) => n,
        Err(RedError::Timeout) => {
            return (
                "error: counting the table exceeded the agent's statement timeout; it may be \
                 very large. Profile a narrower view or use run_select with aggregates."
                    .into(),
                false,
            );
        }
        Err(e) => return (format!("error: {e}"), false),
    };
    let want_distinct = (0..=PROFILE_DISTINCT_MAX_ROWS).contains(&total);

    let qualified = if schema.is_empty() {
        table.to_string()
    } else {
        format!("{schema}.{table}")
    };
    let mut out = String::new();
    let _ = writeln!(out, "Profile of {qualified} — {total} rows\n");
    let _ = writeln!(out, "Columns:");

    let total_cols = detail.columns.len();
    for col in detail.columns.iter().take(MAX_PROFILE_COLUMNS) {
        let numeric = red_core::is_numeric_type(col.type_name.as_deref());
        let ty = col.type_name.as_deref().unwrap_or("?");
        let mut tags = Vec::new();
        if col.primary_key {
            tags.push("pk");
        }
        if col.not_null {
            tags.push("not null");
        }
        let tagstr = if tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tags.join(", "))
        };
        let _ = writeln!(out, "  {} {ty}{tagstr}", col.name);

        let abort = AbortSignal::new();
        let stats = guard_timeout(
            limits.statement_timeout_ms,
            &abort,
            driver.column_stats(
                &base_sql,
                &col.name,
                red_core::StatsFlags {
                    numeric,
                    distinct: want_distinct,
                },
                &abort,
            ),
        )
        .await;
        match stats {
            Ok(s) => {
                let nulls = s.total - s.non_null;
                let null_pct = if s.total > 0 {
                    nulls as f64 * 100.0 / s.total as f64
                } else {
                    0.0
                };
                let mut line = format!("    nulls: {nulls} ({null_pct:.1}%)");
                match s.distinct {
                    Some(d) => {
                        // Free data-quality hints straight from the counts.
                        let note = if s.total > 0 && nulls == 0 && d == s.total {
                            " (unique)"
                        } else if d == 1 {
                            " (constant)"
                        } else {
                            ""
                        };
                        let _ = write!(line, "  distinct: {d}{note}");
                    }
                    None => {
                        let _ = write!(line, "  distinct: not computed (table over the row guard)");
                    }
                }
                if s.non_null > 0 {
                    let _ = write!(line, "  min: {}  max: {}", s.min, s.max);
                    if let (Some(sum), Some(avg)) = (&s.sum, &s.avg) {
                        let _ = write!(line, "  sum: {sum}  avg: {avg}");
                    }
                }
                let _ = writeln!(out, "{line}");
            }
            Err(RedError::Timeout) => {
                let _ = writeln!(out, "    (stats timed out for this column)");
            }
            Err(e) => {
                let _ = writeln!(out, "    (stats unavailable: {e})");
            }
        }
    }
    if total_cols > MAX_PROFILE_COLUMNS {
        let _ = writeln!(
            out,
            "  (profiled the first {MAX_PROFILE_COLUMNS} of {total_cols} columns)"
        );
    }

    // Foreign-key relationships from the connection-wide graph (best-effort; an
    // engine without relational FKs simply reports none).
    let fks = driver.foreign_keys().await.unwrap_or_default();
    let outgoing: Vec<_> = fks.iter().filter(|e| e.from_table == table).collect();
    let incoming: Vec<_> = fks.iter().filter(|e| e.to_table == table).collect();
    if !outgoing.is_empty() {
        let _ = writeln!(out, "\nForeign keys (this table references):");
        for e in &outgoing {
            for (from, to) in &e.columns {
                let _ = writeln!(out, "  {from} → {}.{to}", e.to_table);
            }
        }
    }
    if !incoming.is_empty() {
        let _ = writeln!(out, "\nReferenced by (tables pointing here):");
        for e in &incoming {
            for (from, to) in &e.columns {
                let _ = writeln!(out, "  {}.{from} → {to}", e.from_table);
            }
        }
    }

    (out, true)
}
/// Find rows containing `term` anywhere in a table, without the model having to
/// guess which column holds it. Composes the driver's own
/// [`contains_predicate`](DatabaseDriver::contains_predicate) (the same
/// escaped, blob-skipping OR-of-LIKE the grid's find-in-result builds) with the
/// windowed `fetch_page`, so it inherits both the escaping and the row cap
/// rather than interpolating a model-supplied string into SQL.
pub(in crate::ai) async fn search_data(
    driver: &Arc<dyn DatabaseDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    let schema = input.get("schema").and_then(Json::as_str).unwrap_or("");
    let table = input.get("table").and_then(Json::as_str).unwrap_or("");
    let term = input.get("term").and_then(Json::as_str).unwrap_or("");
    if table.is_empty() || term.is_empty() {
        return (
            "error: search_data needs a non-empty `table` and `term`".into(),
            false,
        );
    }
    let detail = match driver.describe_table(schema, table).await {
        Ok(d) => d,
        Err(e) => return (format!("error: {e}"), false),
    };
    let table_ref = TableRef {
        schema: (!schema.is_empty()).then(|| schema.to_string()),
        name: table.to_string(),
    };
    let Some(predicate) = driver.contains_predicate(&detail.columns, term) else {
        return (
            format!(
                "`{table}` has no searchable columns (they are all binary/blob), so there is \
                 nothing to match `{term}` against."
            ),
            true,
        );
    };
    let max_rows = limits.max_rows.max(1);
    let limit = input
        .get("limit")
        .and_then(Json::as_u64)
        .map(|n| n as usize)
        .unwrap_or(max_rows)
        .clamp(1, max_rows);
    let sql = format!(
        "SELECT * FROM {} WHERE {predicate}",
        driver.quote_table(&table_ref)
    );
    let abort = AbortSignal::new();
    // One probe row past the cap, so "exactly `limit` matches" is told apart from
    // "there are more", exactly as run_select does.
    let fetch = driver.fetch_page(
        &sql,
        0,
        limit.saturating_add(1),
        PageCap::Display { key: None },
        &abort,
    );
    match guard_timeout(limits.statement_timeout_ms, &abort, fetch).await {
        Ok(mut page) => {
            let truncated = page.rows.len() > limit;
            page.rows.truncate(limit);
            let mut out = format_page(&page);
            if truncated {
                out.push_str(&format!(
                    "\n(truncated to {limit} rows: more rows contain `{term}`)"
                ));
            }
            (out, true)
        }
        Err(RedError::Timeout) => (
            "error: the search exceeded the agent's statement timeout. A contains-match cannot \
             use an index, so it scans; narrow it to a table you know is small, or write a \
             targeted run_select instead."
                .into(),
            false,
        ),
        Err(e) => (format!("error: {e}"), false),
    }
}
/// Decide whether an index would help a query and emit the candidate DDL as
/// text. A composition of `explain` and `describe_table`, not a new seam: the
/// plan says whether it scans, and the table's existing indexes say whether the
/// suggestion is already there.
pub(in crate::ai) async fn suggest_index(
    driver: &Arc<dyn DatabaseDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
    let schema = input.get("schema").and_then(Json::as_str).unwrap_or("");
    let table = input.get("table").and_then(Json::as_str).unwrap_or("");
    if sql.is_empty() || table.is_empty() {
        return (
            "error: suggest_index needs `sql` and the `table` it filters".into(),
            false,
        );
    }
    let explain = driver.explain(sql, false);
    let plan = match limits.statement_timeout_ms {
        0 => explain.await,
        ms => tokio::time::timeout(Duration::from_millis(ms), explain)
            .await
            .unwrap_or(Err(RedError::Timeout)),
    };
    let plan = match plan {
        Ok(p) => p,
        Err(e) => return (format!("error: {e}"), false),
    };
    let detail = match driver.describe_table(schema, table).await {
        Ok(d) => d,
        Err(e) => return (format!("error: {e}"), false),
    };
    let mut out = format!("Plan for the query:\n{}\n\n", format_plan(&plan));
    out.push_str("Existing indexes:\n");
    if detail.indexes.is_empty() {
        out.push_str("  (none)\n");
    }
    for ix in &detail.indexes {
        out.push_str(&format!(
            "  {} on ({}){}\n",
            ix.name,
            ix.columns.join(", "),
            if ix.unique { " UNIQUE" } else { "" },
        ));
    }
    let table_ref = TableRef {
        schema: (!schema.is_empty()).then(|| schema.to_string()),
        name: table.to_string(),
    };
    out.push_str(&format!(
        "\nColumns available to index: {}\n\nIf the plan above scans rather than seeks, the index \
         to consider has the query's equality-filtered columns first, then its range-filtered \
         one, then anything it sorts by. Check it is not already listed above, then propose it as \
         TEXT for the user:\n\n  CREATE INDEX idx_{}_<columns> ON {} (<columns>);\n\nNothing here \
         was created. Use create_index (which needs the user's approval) only if they ask for it, \
         and tell them how large the table is first.\n",
        detail
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        table,
        driver.quote_table(&table_ref),
    ));
    (cap_result_bytes(out, limits.max_result_bytes), true)
}
/// The validated arguments of a `create_index` call, shared by the approval
/// prompt and the executor so the index the user allows is the index that runs.
pub(in crate::ai) fn index_args(
    input: &Json,
) -> Result<(TableRef, String, Vec<String>, bool), String> {
    let table = input
        .get("table")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("create_index needs a `table`")?;
    let name = input
        .get("name")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or("create_index needs a `name` for the index")?;
    let columns: Vec<String> = input
        .get("columns")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Json::as_str)
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if columns.is_empty() {
        return Err("create_index needs a non-empty `columns` array".into());
    }
    Ok((
        TableRef {
            schema: input
                .get("schema")
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            name: table.to_string(),
        },
        name.to_string(),
        columns,
        input.get("unique").and_then(Json::as_bool).unwrap_or(false),
    ))
}
/// Stop a server session, re-resolving the target against the live server first.
///
/// The approval the user gave names a specific session doing a specific thing.
/// Between that prompt and this call the server may have finished that statement
/// and handed the same pid/thread id to something else, so the facts the model
/// echoed are *verified* rather than trusted: a mismatch refuses the kill instead
/// of stopping whatever now holds the key.
pub(in crate::ai) async fn kill_session(
    driver: &Arc<dyn DatabaseDriver>,
    input: &Json,
) -> (String, bool) {
    let key = input
        .get("key")
        .and_then(Json::as_str)
        .map(str::trim)
        .unwrap_or("");
    if key.is_empty() {
        return ("error: `key` is required".into(), false);
    }
    let mode = match kill_mode(input) {
        Ok(m) => m,
        Err(why) => return (format!("error: {why}"), false),
    };
    let sessions = match driver.server_sessions().await {
        Ok((s, _)) => s,
        Err(e) => return (format!("error: {e}"), false),
    };
    let Some(target) = sessions
        .iter()
        .find(|s| s.key == red_core::SessionKey(key.to_string()))
    else {
        return (
            format!("session `{key}` is no longer running; nothing to stop."),
            true,
        );
    };
    if target.is_self {
        return (
            "error: that is RED's own connection. Stopping it would just force a reconnect; \
             refusing."
                .into(),
            false,
        );
    }
    if let Some(claimed) = input
        .get("user")
        .and_then(Json::as_str)
        .filter(|u| !u.is_empty())
        && target.user.as_deref() != Some(claimed)
    {
        return (
            format!(
                "error: session `{key}` now belongs to {}, not {claimed}: it was recycled since \
                 you read it. Re-read server_sessions and propose again.",
                target.user.as_deref().unwrap_or("an unknown user"),
            ),
            false,
        );
    }
    match driver.kill_session(&target.key, mode).await {
        Ok(()) => (
            format!(
                "{} on session `{key}`. Confirm with server_sessions.",
                mode.verb()
            ),
            true,
        ),
        Err(e) => (format!("error: {e}"), false),
    }
}
/// Cap on the edges one `relationship_map` reports. Large enough for any schema a
/// person reasons about in one sitting; past it the map is a data dump rather than
/// a map, and the truncation is reported so the model narrows with `tables`.
const FK_EDGE_CAP: usize = 400;
/// The connection's foreign-key graph as text: every edge, then the tables no
/// edge touches. One `foreign_keys()` pass (the same graph the ER canvas draws)
/// plus the object list for the islands, because a table nobody references is a
/// fact the model cannot infer from the edges it *did* get.
pub(in crate::ai) async fn relationship_map(
    driver: &Arc<dyn DatabaseDriver>,
    input: &Json,
) -> (String, bool) {
    let schema = input
        .get("schema")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let wanted: Vec<String> = input
        .get("tables")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Json::as_str)
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default();
    let edges = match driver.foreign_keys().await {
        Ok(e) => e,
        Err(e) => return (format!("error: {e}"), false),
    };

    let in_schema = |ns: Option<&str>| match schema {
        None => true,
        Some(want) => ns.is_some_and(|got| got.eq_ignore_ascii_case(want)),
    };
    let named = |t: &str| {
        wanted.is_empty()
            || wanted
                .iter()
                .any(|w| w.eq_ignore_ascii_case(t.trim_matches('"')))
    };
    // Either side matching keeps the edge: a filter on `orders` should still show
    // what points *at* orders, which is the half a column-name guess gets wrong.
    let kept: Vec<&FkEdge> = edges
        .iter()
        .filter(|e| {
            (in_schema(e.from_schema.as_deref()) || in_schema(e.to_schema.as_deref()))
                && (named(&e.from_table) || named(&e.to_table))
        })
        .collect();

    let mut out = if kept.is_empty() {
        "No declared foreign keys. This engine or schema has none, so join keys cannot be \
         verified here: confirm any join against describe_table and the data itself.\n"
            .to_string()
    } else {
        format!("{} foreign-key edge(s):\n", kept.len())
    };
    for e in kept.iter().take(FK_EDGE_CAP) {
        out.push_str(&format!(
            "  {} -> {}\n",
            fk_side(e.from_schema.as_deref(), &e.from_table, 0, &e.columns),
            fk_side(e.to_schema.as_deref(), &e.to_table, 1, &e.columns),
        ));
    }
    if kept.len() > FK_EDGE_CAP {
        out.push_str(&format!(
            "  …({} more edges; narrow with `schema` or `tables`)\n",
            kept.len() - FK_EDGE_CAP
        ));
    }

    // Islands are a property of the *whole* graph, so they're computed against
    // every edge and only then narrowed to the requested schema for display.
    let mut touched: Vec<String> = Vec::with_capacity(edges.len() * 2);
    for e in &edges {
        touched.push(qualified(e.from_schema.as_deref(), &e.from_table).to_ascii_lowercase());
        touched.push(qualified(e.to_schema.as_deref(), &e.to_table).to_ascii_lowercase());
    }
    if let Ok(schemas) = driver.list_objects().await {
        let islands: Vec<String> = schemas
            .iter()
            .filter(|s| in_schema(Some(&s.name)))
            .flat_map(|s| {
                s.objects
                    .iter()
                    // Views hold no constraints, so listing them here would report
                    // every view as an island and drown the real ones.
                    .filter(|o| o.kind == red_core::ObjectKind::Table)
                    .map(move |o| qualified(Some(&s.name), &o.name))
            })
            .filter(|t| !touched.contains(&t.to_ascii_lowercase()) && named(t))
            .collect();
        if !islands.is_empty() {
            out.push_str(&format!(
                "\n{} table(s) with no foreign key in either direction:\n  {}\n",
                islands.len(),
                islands.join(", ")
            ));
        }
    }
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ConnCtx;
    use crate::ai::sql::run_tool;
    use crate::ai::state::ReportSink;
    use red_ai::CancelToken;
    use red_core::{AiPolicy, sql::Dialect};
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn profile_table_reports_nulls_distinct_aggregates_and_fks() {
        let db =
            std::env::temp_dir().join(format!("red-prof-{}.db", uuid::Uuid::new_v4().simple()));
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT);
                 CREATE TABLE child (
                    id INTEGER PRIMARY KEY,
                    parent_id INTEGER REFERENCES parent(id),
                    tag TEXT,
                    score INTEGER
                 );
                 INSERT INTO parent VALUES (1, 'a'), (2, 'b');
                 INSERT INTO child VALUES (1, 1, 'x', 10), (2, 1, 'x', 20), (3, NULL, 'x', NULL);",
            )
            .unwrap();
        }
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (content, ok) = run_tool(
            &driver,
            ConnCtx {
                conn_id: "",
                dialect: Dialect::Sqlite,
                conversation_id: crate::protocol::ConversationId::new(1),
                state: &std::sync::Arc::new(std::sync::Mutex::new(
                    crate::ai::state::AiState::default(),
                )),
                sandbox: None,
            },
            "profile_table",
            &json!({ "schema": "main", "table": "child" }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &ReportSink::disabled(),
        )
        .await;
        assert!(ok, "profile failed: {content}");
        assert!(content.contains("3 rows"), "row count missing: {content}");
        // The PK is all-distinct and non-null → flagged unique.
        assert!(
            content.contains("(unique)"),
            "unique hint missing: {content}"
        );
        // `tag` is 'x' in every row → flagged constant.
        assert!(
            content.contains("(constant)"),
            "constant hint missing: {content}"
        );
        // `parent_id` and `score` each have one null row.
        assert!(
            content.contains("nulls: 1"),
            "null count missing: {content}"
        );
        // Numeric `score` reports sum/avg.
        assert!(
            content.contains("sum:"),
            "numeric aggregates missing: {content}"
        );
        // The outgoing FK to `parent` is surfaced.
        assert!(
            content.contains("parent_id → parent.id"),
            "FK relationship missing: {content}"
        );
    }

    #[tokio::test]
    async fn search_data_finds_a_value_without_naming_its_column() {
        let db =
            std::env::temp_dir().join(format!("red-search-{}.db", uuid::Uuid::new_v4().simple()));
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, note TEXT);
                 INSERT INTO people VALUES (1, 'Ada', 'analytical engine'),
                                           (2, 'Grace', 'compiler');",
            )
            .unwrap();
        }
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let search = async |term: &str| {
            run_tool(
                &driver,
                ConnCtx {
                    conn_id: "",
                    dialect: Dialect::Sqlite,
                    conversation_id: crate::protocol::ConversationId::new(1),
                    state: &std::sync::Arc::new(std::sync::Mutex::new(
                        crate::ai::state::AiState::default(),
                    )),
                    sandbox: None,
                },
                "search_data",
                &json!({ "schema": "main", "table": "people", "term": term }),
                &AiPolicy::default(),
                &CancelToken::new(),
                &ReportSink::disabled(),
            )
            .await
        };
        // The term is in `note`, not `name`; the model never had to know that.
        let (content, ok) = search("engine").await;
        assert!(ok, "{content}");
        assert!(content.contains("Ada"), "{content}");
        assert!(!content.contains("Grace"), "{content}");
        // A quote in the term is escaped, not interpolated: no SQL error, no rows.
        let (content, ok) = search("' OR 1=1 --").await;
        assert!(ok, "{content}");
        assert!(
            !content.contains("Ada"),
            "injection matched rows: {content}"
        );
    }

    #[tokio::test]
    async fn relationship_map_lists_edges_and_islands() {
        let db =
            std::env::temp_dir().join(format!("red-relmap-{}.db", uuid::Uuid::new_v4().simple()));
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE orders (id INTEGER PRIMARY KEY);
                 CREATE TABLE order_items (
                    id INTEGER PRIMARY KEY,
                    order_id INTEGER REFERENCES orders(id)
                 );
                 CREATE TABLE audit_log (id INTEGER PRIMARY KEY, note TEXT);",
            )
            .unwrap();
        }
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (content, ok) = run_tool(
            &driver,
            ConnCtx {
                conn_id: "",
                dialect: Dialect::Sqlite,
                conversation_id: crate::protocol::ConversationId::new(1),
                state: &std::sync::Arc::new(std::sync::Mutex::new(
                    crate::ai::state::AiState::default(),
                )),
                sandbox: None,
            },
            "relationship_map",
            &json!({}),
            &AiPolicy::default(),
            &CancelToken::new(),
            &ReportSink::disabled(),
        )
        .await;
        assert!(ok, "{content}");
        assert!(
            content.contains("main.order_items.order_id -> main.orders.id"),
            "{content}"
        );
        // The island is named, so the model can see the graph has disconnected
        // pieces rather than inferring one from silence.
        assert!(content.contains("main.audit_log"), "{content}");
    }

    #[tokio::test]
    async fn suggest_index_reads_the_plan_and_the_existing_indexes() {
        let db = std::env::temp_dir().join(format!("red-idx-{}.db", uuid::Uuid::new_v4().simple()));
        {
            rusqlite::Connection::open(&db)
                .unwrap()
                .execute_batch(
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT, city TEXT);
                     CREATE INDEX idx_t_email ON t (email);",
                )
                .unwrap();
        }
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (content, ok) = run_tool(
            &driver,
            ConnCtx {
                conn_id: "",
                dialect: Dialect::Sqlite,
                conversation_id: crate::protocol::ConversationId::new(1),
                state: &std::sync::Arc::new(std::sync::Mutex::new(
                    crate::ai::state::AiState::default(),
                )),
                sandbox: None,
            },
            "suggest_index",
            &json!({ "sql": "SELECT * FROM t WHERE city = 'x'", "schema": "main", "table": "t" }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &ReportSink::disabled(),
        )
        .await;
        assert!(ok, "{content}");
        // It reports what already exists, so the suggestion cannot duplicate it…
        assert!(content.contains("idx_t_email"), "{content}");
        // …and it is explicit that nothing was created, so the suggestion is not
        // mistaken for an action already taken.
        assert!(content.contains("Nothing here was created."), "{content}");
        assert!(content.contains("create_index"), "{content}");
    }
}
