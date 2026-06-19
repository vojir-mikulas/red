//! The assistant's backend half: the agentic loop and the read-only tool catalog
//! it stands on. Mirrors the export/updater pattern — a turn runs as a spawned
//! task off the dispatch loop, streams `AiDelta` events as tokens arrive, and
//! drives the model → tool → model loop itself (the plain Messages API tool-use
//! loop, on the service thread).
//!
//! Every tool is backed by a `DatabaseDriver` seam that already exists and
//! inherits its guard: `list_schema`/`describe_table`/`explain` are always safe,
//! and `run_select` is row-capped and rejects non-`SELECT` SQL — the model gets
//! the same windowed, never-materialized reads a human does, and (in M1) cannot
//! mutate anything.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use red_ai::{
    AiProvider, CancelToken, ContentBlock, Message, Role, StopReason, ToolDef, TurnRequest,
};
use red_core::Value;
use red_driver::{AbortSignal, DatabaseDriver, PageCap};
use serde_json::{json, Value as Json};

use crate::dispatch::{emit, Events};
use crate::protocol::{AiContext, AiDelta, AiUsage};
use crate::{Event, SessionId};

/// Safety backstop on the model → tool → model loop: how many tool round-trips a
/// single turn may take before we stop and report. Far above any real grounded
/// answer; prevents a misbehaving model from looping forever.
const MAX_TOOL_STEPS: usize = 16;

/// Row ceiling for one `run_select` tool call. The model browses, it doesn't bulk
/// export — a few hundred rows is plenty of grounding and keeps the context lean.
const SELECT_ROW_CAP: usize = 200;

/// Per-conversation state shared between the dispatch loop and the spawned turn
/// tasks: the running message history (so follow-up turns keep context) and the
/// in-flight cancel tokens (so `AiCancel` can stop a specific turn).
#[derive(Default)]
pub(crate) struct AiState {
    histories: HashMap<u64, Vec<Message>>,
    cancels: HashMap<u64, CancelToken>,
}

impl AiState {
    /// Record an in-flight turn's cancel token so `AiCancel` can reach it.
    pub(crate) fn register(&mut self, conversation_id: u64, token: CancelToken) {
        self.cancels.insert(conversation_id, token);
    }

    /// Flip the cancel token for an in-flight turn, if any (the panel's Stop).
    pub(crate) fn cancel(&self, conversation_id: u64) {
        if let Some(tok) = self.cancels.get(&conversation_id) {
            tok.cancel();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Run one assistant turn to completion as a spawned task: build the grounded
/// prompt, loop the model against the read-only tools, and stream events. Owns
/// cleanup of its cancel-token registration.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_turn(
    provider: Arc<dyn AiProvider>,
    driver: Arc<dyn DatabaseDriver>,
    events: Events,
    state: Arc<Mutex<AiState>>,
    session: Option<SessionId>,
    conversation_id: u64,
    model: String,
    show_thinking: bool,
    user_message: String,
    context: AiContext,
    cancel: CancelToken,
) {
    let system = system_prompt(&context);
    let tools = tool_catalog();

    // Seed the conversation with the grounded user message and pull the running
    // history so a follow-up keeps prior context.
    let mut messages = {
        let mut st = lock(&state);
        let history = st.histories.entry(conversation_id).or_default();
        history.push(Message::user_text(user_turn(&user_message, &context)));
        history.clone()
    };

    let mut usage = AiUsage::default();
    let mut result: std::result::Result<(), String> = Ok(());

    for _ in 0..MAX_TOOL_STEPS {
        if cancel.is_cancelled() {
            result = Err("cancelled".into());
            break;
        }

        let req = TurnRequest {
            model: model.clone(),
            max_tokens: 8192,
            show_thinking,
            system: system.clone(),
            tools: tools.clone(),
            messages: messages.clone(),
        };

        // Relay the provider's deltas to the UI as they stream in.
        let (dtx, mut drx) = tokio::sync::mpsc::unbounded_channel::<red_ai::Delta>();
        let relay = {
            let events = events.clone();
            tokio::spawn(async move {
                while let Some(d) = drx.recv().await {
                    let delta = match d {
                        red_ai::Delta::Thinking(t) => AiDelta::Thinking(t),
                        red_ai::Delta::Text(t) => AiDelta::Text(t),
                        red_ai::Delta::ToolUseStarted { name, .. } => AiDelta::ToolStarted { name },
                    };
                    emit(
                        &events,
                        session,
                        Event::AiDelta {
                            conversation_id,
                            delta,
                        },
                    );
                }
            })
        };

        let outcome = provider.stream_turn(&req, &dtx, &cancel).await;
        drop(dtx);
        let _ = relay.await;

        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                result = Err(e.to_string());
                break;
            }
        };

        usage.input_tokens += outcome.usage.input_tokens;
        usage.output_tokens += outcome.usage.output_tokens;
        usage.cache_read_input_tokens += outcome.usage.cache_read_input_tokens;
        messages.push(outcome.message.clone());

        if outcome.stop_reason != StopReason::ToolUse {
            break;
        }

        // Run every requested tool and feed one result block back per call.
        let mut results = Vec::new();
        for block in &outcome.message.content {
            let ContentBlock::ToolUse { id, name, input } = block else {
                continue;
            };
            emit(
                &events,
                session,
                Event::AiDelta {
                    conversation_id,
                    delta: AiDelta::ToolStarted { name: name.clone() },
                },
            );
            let (content, ok) = run_tool(&driver, name, input, &cancel).await;
            emit(
                &events,
                session,
                Event::AiDelta {
                    conversation_id,
                    delta: AiDelta::ToolFinished {
                        name: name.clone(),
                        ok,
                    },
                },
            );
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content,
                is_error: !ok,
            });
        }

        if results.is_empty() {
            // Model claimed tool_use but emitted no tool block — bail rather than spin.
            break;
        }
        messages.push(Message {
            role: Role::User,
            content: results,
        });
    }

    // Persist history and drop the cancel registration.
    {
        let mut st = lock(&state);
        st.histories.insert(conversation_id, messages);
        st.cancels.remove(&conversation_id);
    }

    match result {
        Ok(()) => emit(
            &events,
            session,
            Event::AiTurnFinished {
                conversation_id,
                usage,
            },
        ),
        Err(message) => emit(
            &events,
            session,
            Event::AiError {
                conversation_id,
                message,
            },
        ),
    }
}

/// The read-only tool catalog (M1). Each tool is backed by a `DatabaseDriver`
/// method and auto-runs; none can mutate. Shared with the MCP server (the
/// subscription/ACP path serves the same four tools over MCP).
pub(crate) fn tool_catalog() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "list_schema".into(),
            description: "List the database's schemas and their tables and views (names and kinds \
                only). Call this to discover what objects exist before describing or querying them."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "describe_table".into(),
            description: "Get one table or view's columns (name, type, nullability, primary key), \
                foreign keys, and indexes. Use this before writing a query against a table."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Schema/namespace name (e.g. \"main\" or \"public\")." },
                    "table": { "type": "string", "description": "The table or view name." },
                },
                "required": ["schema", "table"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "run_select".into(),
            description: format!(
                "Run a read-only SELECT (or WITH ... SELECT) query and return up to {SELECT_ROW_CAP} \
                rows. Non-SELECT statements are rejected. Results are row- and cell-capped — use \
                LIMIT and targeted columns. This is the only way to read actual data."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "A single SELECT/WITH query." },
                    "limit": {
                        "type": "integer",
                        "description": format!("Max rows to return (1..{SELECT_ROW_CAP})."),
                    },
                },
                "required": ["sql"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "explain".into(),
            description: "Return the query planner's EXPLAIN output for a SQL statement (it does \
                not execute the statement). Use this to reason about performance."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "The SQL to explain." },
                },
                "required": ["sql"],
                "additionalProperties": false,
            }),
        },
    ]
}

/// Execute one tool call against the driver. Returns `(content, ok)`; `ok = false`
/// becomes an `is_error` tool result the model can recover from. Shared with the
/// MCP server so the API-key and subscription paths run identical, guarded tools.
pub(crate) async fn run_tool(
    driver: &Arc<dyn DatabaseDriver>,
    name: &str,
    input: &Json,
    _cancel: &CancelToken,
) -> (String, bool) {
    match name {
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
        "run_select" => {
            let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
            if !is_read_only_select(sql) {
                return (
                    "error: only a single SELECT or WITH...SELECT query is allowed".into(),
                    false,
                );
            }
            let limit = input
                .get("limit")
                .and_then(Json::as_u64)
                .map(|n| (n as usize).clamp(1, SELECT_ROW_CAP))
                .unwrap_or(SELECT_ROW_CAP);
            let abort = AbortSignal::new();
            match driver
                .fetch_page(sql, 0, limit, PageCap::Display { key: None }, &abort)
                .await
            {
                Ok(page) => (format_page(&page), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "explain" => {
            let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
            if sql.is_empty() {
                return ("error: `sql` is required".into(), false);
            }
            match driver.explain(sql, false).await {
                Ok(plan) => (format_plan(&plan), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        other => (format!("error: unknown tool `{other}`"), false),
    }
}

/// A conservative read-only gate: the statement must be a single SELECT or a CTE
/// that resolves to a SELECT, with no statement separator that could smuggle a
/// write past the prefix check.
fn is_read_only_select(sql: &str) -> bool {
    let trimmed = sql.trim().trim_end_matches(';');
    if trimmed.is_empty() {
        return false;
    }
    // No embedded statement terminator (a `;` mid-string could chain a write).
    if trimmed.contains(';') {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("select") || lower.starts_with("with")
}

/// The stable grounding instruction. Shared with the ACP path, which folds it
/// into the agent's first prompt (ACP `session/prompt` has no system role).
pub(crate) fn system_prompt(ctx: &AiContext) -> String {
    let mut s = String::from(
        "You are Red's database assistant, embedded in a native SQL explorer. You help the user \
         explore and understand the database they are connected to.\n\n\
         You have read-only tools: list_schema, describe_table, run_select (capped SELECTs), and \
         explain. Use them to ground every answer in the live database rather than guessing — \
         discover objects with list_schema, inspect structure with describe_table, and read data \
         with run_select. Prefer small, targeted queries with explicit columns and LIMIT.\n\n\
         When you write SQL for the user, put it in a fenced ```sql block so they can run it. Be \
         concise: lead with the answer, then the supporting query or detail.\n",
    );
    if !ctx.connection.is_empty() {
        s.push_str(&format!("\nConnected to: {}", ctx.connection));
    }
    if ctx.read_only {
        s.push_str("\nThis connection is READ-ONLY: do not propose INSERT/UPDATE/DELETE/DDL.");
    }
    if !ctx.schema_summary.is_empty() {
        s.push_str("\n\nSchema overview (use describe_table for full detail):\n");
        s.push_str(&ctx.schema_summary);
    }
    s
}

/// Fold the volatile, per-turn context (editor SQL, last error, selection) into
/// the user's message so the stable system prompt stays prompt-cacheable. Shared
/// with the ACP path for the same per-turn grounding.
pub(crate) fn user_turn(message: &str, ctx: &AiContext) -> String {
    let mut s = String::new();
    if let Some(sql) = ctx.editor_sql.as_deref().filter(|s| !s.trim().is_empty()) {
        s.push_str("Current editor SQL:\n```sql\n");
        s.push_str(sql.trim());
        s.push_str("\n```\n\n");
    }
    if let Some(err) = ctx.last_error.as_deref().filter(|s| !s.trim().is_empty()) {
        s.push_str("Last error shown:\n");
        s.push_str(err.trim());
        s.push_str("\n\n");
    }
    if let Some(sel) = ctx.selection.as_deref().filter(|s| !s.trim().is_empty()) {
        s.push_str("Selected rows:\n");
        s.push_str(sel.trim());
        s.push_str("\n\n");
    }
    s.push_str(message);
    s
}

fn format_schema(schemas: &[red_core::SchemaMeta]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for sch in schemas {
        let _ = writeln!(out, "schema {} ({} objects):", sch.name, sch.objects.len());
        for obj in &sch.objects {
            let kind = match obj.kind {
                red_core::ObjectKind::Table => "table",
                red_core::ObjectKind::View => "view",
            };
            let _ = writeln!(out, "  {kind} {}", obj.name);
        }
    }
    if out.is_empty() {
        out.push_str("(no objects)");
    }
    out
}

fn format_table_detail(schema: &str, table: &str, d: &red_core::TableDetail) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "{schema}.{table}");
    let _ = writeln!(out, "columns:");
    for c in &d.columns {
        let ty = c.type_name.as_deref().unwrap_or("?");
        let mut flags = Vec::new();
        if c.primary_key {
            flags.push("PK");
        }
        if c.not_null {
            flags.push("NOT NULL");
        }
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", flags.join(", "))
        };
        let _ = writeln!(out, "  {} {ty}{flags}", c.name);
    }
    if !d.foreign_keys.is_empty() {
        let _ = writeln!(out, "foreign keys:");
        for fk in &d.foreign_keys {
            let _ = writeln!(out, "  {} -> {}.{}", fk.column, fk.ref_table, fk.ref_column);
        }
    }
    if !d.indexes.is_empty() {
        let _ = writeln!(out, "indexes:");
        for ix in &d.indexes {
            let uniq = if ix.unique { "unique " } else { "" };
            let _ = writeln!(out, "  {uniq}{} ({})", ix.name, ix.columns.join(", "));
        }
    }
    out
}

fn format_page(page: &red_core::ResultPage) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let header: Vec<&str> = page.columns.iter().map(|c| c.name.as_str()).collect();
    let _ = writeln!(out, "{}", header.join(" | "));
    for row in &page.rows {
        let cells: Vec<String> = row.iter().map(render_cell).collect();
        let _ = writeln!(out, "{}", cells.join(" | "));
    }
    let _ = write!(out, "({} rows)", page.rows.len());
    out
}

fn render_cell(v: &Value) -> String {
    // `Value`'s Display already renders NULL, capped text (`head…`), and blobs
    // (`<N bytes>`) — exactly the compact form we want for the model.
    v.to_string()
}

fn format_plan(plan: &red_core::QueryPlan) -> String {
    if plan.nodes.is_empty() {
        return plan.raw.clone();
    }
    let mut out = String::new();
    for node in &plan.nodes {
        write_plan_node(&mut out, node, 0);
    }
    out
}

fn write_plan_node(out: &mut String, node: &red_core::PlanNode, depth: usize) {
    use std::fmt::Write;
    let indent = "  ".repeat(depth);
    let _ = write!(out, "{indent}{}", node.label);
    if let Some(d) = &node.detail {
        let _ = write!(out, " — {d}");
    }
    if !node.metrics.is_empty() {
        let m: Vec<String> = node
            .metrics
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let _ = write!(out, " [{}]", m.join(", "));
    }
    out.push('\n');
    for child in &node.children {
        write_plan_node(out, child, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_gate_rejects_writes_and_chains() {
        assert!(is_read_only_select("SELECT 1"));
        assert!(is_read_only_select(
            "  with x as (select 1) select * from x  "
        ));
        assert!(is_read_only_select("select 1;"));
        assert!(!is_read_only_select("UPDATE t SET x=1"));
        assert!(!is_read_only_select("DELETE FROM t"));
        assert!(!is_read_only_select("select 1; drop table t"));
        assert!(!is_read_only_select(""));
    }

    #[test]
    fn catalog_has_the_four_readonly_tools() {
        let catalog = tool_catalog();
        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            ["list_schema", "describe_table", "run_select", "explain"]
        );
    }
}
