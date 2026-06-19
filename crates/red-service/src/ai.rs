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
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use red_ai::{
    AiProvider, CancelToken, ContentBlock, Message, Role, StopReason, ToolDef, TurnRequest,
};
use red_core::{AiPolicy, AiTier, ExportFormat, RedError, Value};
use red_driver::{AbortSignal, DatabaseDriver, PageCap};
use serde_json::{json, Value as Json};
use tokio::sync::oneshot;

use crate::dispatch::{emit, Events};
use crate::protocol::{AiContext, AiDelta, AiUsage};
use crate::{Event, SessionId};

/// Where a `generate_report` tool's output file is delivered so the UI can open it
/// (Feature C, agent-initiated reports). The tool layer stays UI-agnostic — it just
/// announces "I wrote a report to `<path>`"; the caller turns that into the
/// `AiReportReady` event the UI handles. Both backends construct one from the
/// `events`/`session`/`conversation_id` they hold; a `disabled()` sink (no channel)
/// drops the announcement and is used by tests.
#[derive(Clone)]
pub(crate) struct ReportSink {
    events: Option<Events>,
    session: Option<SessionId>,
    conversation_id: u64,
}

impl ReportSink {
    pub(crate) fn new(events: Events, session: Option<SessionId>, conversation_id: u64) -> Self {
        Self {
            events: Some(events),
            session,
            conversation_id,
        }
    }

    /// A no-op sink — drops announcements. For tests and any path with no UI.
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self {
            events: None,
            session: None,
            conversation_id: 0,
        }
    }

    /// Announce a freshly-written report so the UI opens it.
    fn announce(&self, path: &Path) {
        if let Some(events) = &self.events {
            emit(
                events,
                self.session,
                Event::AiReportReady {
                    conversation_id: self.conversation_id,
                    path: path.display().to_string(),
                },
            );
        }
    }
}

/// Safety backstop on the model → tool → model loop: how many tool round-trips a
/// single turn may take before we stop and report. Far above any real grounded
/// answer; prevents a misbehaving model from looping forever. The per-conversation
/// [`AiLimits::max_tool_calls`](red_core::AiLimits) bound (M-S7) sits on top of
/// this, spanning turns rather than resetting each one.
const MAX_TOOL_STEPS: usize = 16;

/// Per-conversation state shared between the dispatch loop and the spawned turn
/// tasks: the running message history (so follow-up turns keep context), the
/// in-flight cancel tokens (so `AiCancel` can stop a specific turn), and the
/// cumulative tool-call tally (so the resource-guard budget spans the whole
/// conversation, not just one turn).
#[derive(Default)]
pub(crate) struct AiState {
    histories: HashMap<u64, Vec<Message>>,
    cancels: HashMap<u64, CancelToken>,
    tool_calls: HashMap<u64, usize>,
    /// Write-tool approval prompts (Feature B) awaiting the user's Allow/Deny, keyed
    /// by request id. The turn task parks a decision sink here; `AiPermission` takes
    /// it back out and fires it — the API-key analogue of the ACP path's
    /// `AcpManager.pending`.
    pending_perms: HashMap<u64, oneshot::Sender<bool>>,
    /// Monotonic counter for the request ids handed out by [`Self::park_permission`].
    /// Handed-out ids are offset by [`AI_REQUEST_BASE`] so they never collide with
    /// the ACP manager's (which counts up from 0) — `AiPermission` can then resolve
    /// both sides unconditionally.
    next_request: u64,
}

/// Base offset for API-key permission request ids, keeping them disjoint from the
/// ACP manager's id space so a single `AiPermission` resolves exactly one prompt.
const AI_REQUEST_BASE: u64 = 1 << 48;

/// Cap on outstanding (un-answered) write-approval prompts on the API-key path —
/// past it, deny rather than grow the map. Mirrors the ACP manager's cap.
const MAX_PENDING_PERMS: usize = 32;

impl AiState {
    /// Record an in-flight turn's cancel token so `AiCancel` can reach it.
    pub(crate) fn register(&mut self, conversation_id: u64, token: CancelToken) {
        self.cancels.insert(conversation_id, token);
    }

    /// Park a write-approval decision sink and return the request id to surface, or
    /// `None` (deny) when too many are already outstanding.
    fn park_permission(&mut self, decide: oneshot::Sender<bool>) -> Option<u64> {
        if self.pending_perms.len() >= MAX_PENDING_PERMS {
            return None;
        }
        let id = AI_REQUEST_BASE + self.next_request;
        self.next_request += 1;
        self.pending_perms.insert(id, decide);
        Some(id)
    }

    /// Answer a parked write-approval prompt (the panel's Allow/Deny). A stale id
    /// (already resolved, or owned by the ACP path) is a no-op. Also used to forget a
    /// prompt abandoned on cancel (`allow` is irrelevant then — the receiver is gone).
    pub(crate) fn resolve_permission(&mut self, request_id: u64, allow: bool) {
        if let Some(decide) = self.pending_perms.remove(&request_id) {
            let _ = decide.send(allow);
        }
    }

    /// Flip the cancel token for an in-flight turn, if any (the panel's Stop).
    pub(crate) fn cancel(&self, conversation_id: u64) {
        if let Some(tok) = self.cancels.get(&conversation_id) {
            tok.cancel();
        }
    }

    /// Charge one tool call against the conversation's cumulative budget. Returns
    /// `false` once the budget (`max`, `0` = unlimited) is exhausted, so the loop
    /// can stop a runaway agent instead of letting it spin tools forever.
    fn charge_tool_call(&mut self, conversation_id: u64, max: usize) -> bool {
        let count = self.tool_calls.entry(conversation_id).or_default();
        if max != 0 && *count >= max {
            return false;
        }
        *count += 1;
        true
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
    policy: AiPolicy,
    user_message: String,
    context: AiContext,
    cancel: CancelToken,
) {
    let system = system_prompt(&context, &policy);
    // The tier decides which tools the model is even offered (M-S7): `off` grounds
    // nothing, `schema` withholds row data, `read` is the full catalog.
    let tools = tool_catalog(&policy);
    // Where `generate_report` delivers its file so the UI opens it (Feature C).
    let report = ReportSink::new(events.clone(), session, conversation_id);

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
            // Charge the conversation's cumulative tool-call budget (M-S7). When it
            // is exhausted, hand the model an error result instead of running the
            // tool — it can wrap up its answer, but it can't keep looping.
            if !lock(&state).charge_tool_call(conversation_id, policy.limits.max_tool_calls) {
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: "error: this conversation's tool-call budget is exhausted; \
                        answer with what you have or ask the user to start a new chat"
                        .into(),
                    is_error: true,
                });
                continue;
            }
            // Gate a mutating tool behind explicit per-call user approval (Feature
            // B). A blocked shape (wrong tier, read-only, DDL, unqualified
            // UPDATE/DELETE) is reported to the model without ever prompting; an
            // allowed shape surfaces the exact SQL as an Allow/Deny prompt and runs
            // only on Allow. A read tool falls straight through.
            match assess_write(name, input, &policy) {
                WriteAssessment::Reject(why) => {
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: format!("error: {why}"),
                        is_error: true,
                    });
                    continue;
                }
                WriteAssessment::NeedsApproval { sql } => {
                    let allowed = await_write_approval(
                        &state,
                        &events,
                        session,
                        conversation_id,
                        &sql,
                        &cancel,
                    )
                    .await;
                    if !allowed {
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: "the user denied this write — do not retry it; explain it or \
                                propose an alternative"
                                .into(),
                            is_error: true,
                        });
                        continue;
                    }
                }
                WriteAssessment::NotWrite => {}
            }
            emit(
                &events,
                session,
                Event::AiDelta {
                    conversation_id,
                    delta: AiDelta::ToolStarted { name: name.clone() },
                },
            );
            let (content, ok) = run_tool(&driver, name, input, &policy, &cancel, &report).await;
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

/// Surface a write-approval prompt and block this turn until the user answers it —
/// the API-key path's analogue of the ACP permission flow (Feature B). Parks a
/// decision sink in [`AiState`], emits an `AiPermissionRequest` carrying the exact
/// SQL, then awaits the answer while polling the turn's cancel token (a cancelled
/// turn, or too many outstanding prompts, denies). Returns whether to run the write.
async fn await_write_approval(
    state: &Arc<Mutex<AiState>>,
    events: &Events,
    session: Option<SessionId>,
    conversation_id: u64,
    sql: &str,
    cancel: &CancelToken,
) -> bool {
    let (tx, mut rx) = oneshot::channel();
    let Some(request_id) = lock(state).park_permission(tx) else {
        return false; // too many outstanding prompts → deny
    };
    emit(
        events,
        session,
        Event::AiPermissionRequest {
            conversation_id,
            request_id,
            title: "run this write statement".into(),
            detail: Some(sql.to_string()),
        },
    );
    let decision = loop {
        tokio::select! {
            answer = &mut rx => break answer.unwrap_or(false),
            _ = tokio::time::sleep(Duration::from_millis(150)) => {
                if cancel.is_cancelled() {
                    break false;
                }
            }
        }
    };
    // Drop the parked sink if we bailed on cancel; a normal answer already removed
    // it in `resolve_permission`, so this is a harmless no-op then.
    lock(state).resolve_permission(request_id, false);
    decision
}

/// The read-only tool catalog, filtered to the policy's access tier (M-S7). Each
/// tool is backed by a `DatabaseDriver` method and auto-runs; none can mutate.
/// Filtering happens *here*, at construction, so a tool above the tier is never
/// offered — the model can't call what isn't in the catalog. Shared with the MCP
/// server, so the API-key and subscription/ACP paths expose the identical set.
pub(crate) fn tool_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
    let max_rows = policy.limits.max_rows;
    let all = [
        ToolDef {
            name: "list_schema".into(),
            description:
                "List the database's schemas and their tables and views (names and kinds \
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
                "Run a read-only SELECT (or WITH ... SELECT) query and return up to {max_rows} \
                rows. Non-SELECT statements are rejected. Results are row- and cell-capped and \
                subject to a statement timeout — use LIMIT and targeted columns. This is the only \
                way to read actual data."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "A single SELECT/WITH query." },
                    "limit": {
                        "type": "integer",
                        "description": format!("Max rows to return (1..{max_rows})."),
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
        ToolDef {
            name: "generate_report".into(),
            description: format!(
                "Render a read-only SELECT (or WITH ... SELECT) to a themed, standalone HTML \
                report and open it in the user's browser. Use this when the user asks for a \
                report or a shareable view of some data. The result is capped at {max_rows} rows \
                (like run_select) — write a focused query (aggregate / top-N / explicit columns). \
                Returns once the file is written and opened."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "A single read-only SELECT/WITH query." },
                    "title": { "type": "string", "description": "Optional human title for the report." },
                },
                "required": ["sql"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "propose_write".into(),
            description: "Execute a SINGLE data-modifying statement: INSERT, UPDATE, or DELETE. \
                EVERY call requires explicit per-statement approval — the user sees the exact SQL \
                and must Allow it before it runs; assume it may be denied. UPDATE and DELETE MUST \
                include a WHERE clause. DDL (DROP/TRUNCATE/ALTER/CREATE) and any multi-statement \
                input are rejected — tell the user to run those by hand. Use this only when the \
                user has asked you to change data; otherwise read with run_select."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "A single INSERT/UPDATE/DELETE statement (UPDATE/DELETE need a WHERE)." },
                },
                "required": ["sql"],
                "additionalProperties": false,
            }),
        },
    ];
    all.into_iter()
        // The tier gates membership; additionally, the write tool is withheld on a
        // read-only connection so it's never even offered there (Feature B).
        .filter(|t| {
            policy.tier.allows_tool(&t.name) && !(policy.read_only && is_write_tool(&t.name))
        })
        .collect()
}

/// Execute one tool call against the driver, under the access policy (M-S7).
/// Returns `(content, ok)`; `ok = false` becomes an `is_error` tool result the
/// model can recover from. Shared with the MCP server so the API-key and
/// subscription paths run identical, guarded tools.
///
/// Two layers of guard apply here, both server-side so neither backend can slip
/// past them: the tier is re-checked (defense in depth — the catalog already
/// withholds out-of-tier tools, but a misbehaving agent could still *name* one),
/// and the [`AiLimits`](red_core::AiLimits) clamp rows, time-box the query, and
/// cap the result bytes handed back to the model.
pub(crate) async fn run_tool(
    driver: &Arc<dyn DatabaseDriver>,
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
        "run_select" => {
            let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
            if !is_read_only_select(sql) {
                return (
                    "error: only a single SELECT or WITH...SELECT query is allowed".into(),
                    false,
                );
            }
            // Clamp the requested LIMIT to the hard row cap — the model browses, it
            // doesn't bulk-export — and remember whether we clamped so the result
            // can tell the model it's partial.
            let max_rows = limits.max_rows.max(1);
            let requested = input
                .get("limit")
                .and_then(Json::as_u64)
                .map(|n| n as usize);
            let limit = requested.unwrap_or(max_rows).clamp(1, max_rows);
            let abort = AbortSignal::new();
            let fetch = driver.fetch_page(sql, 0, limit, PageCap::Display { key: None }, &abort);
            match guard_timeout(limits.statement_timeout_ms, &abort, fetch).await {
                Ok(page) => {
                    let mut out = format_page(&page);
                    if page.rows.len() >= limit {
                        out.push_str(&format!(
                            "\n(truncated to {limit} rows — the result may have more; add LIMIT or \
                            a WHERE clause to narrow it)"
                        ));
                    }
                    (out, true)
                }
                Err(RedError::Timeout) => (
                    "error: the query exceeded the assistant's statement timeout — it was \
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
            match driver.explain(sql, false).await {
                Ok(plan) => (format_plan(&plan), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "generate_report" => {
            let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
            if !is_read_only_select(sql) {
                return (
                    "error: generate_report needs a single read-only SELECT or WITH...SELECT query"
                        .into(),
                    false,
                );
            }
            // Cap the report like run_select: wrap as a subquery with the row ceiling
            // (the paging wrap shape) so a report can't stream unbounded rows to disk.
            let max_rows = limits.max_rows.max(1);
            let inner = sql.trim_end_matches(';').trim();
            let capped = format!("SELECT * FROM ({inner}) AS _red LIMIT {max_rows}");
            let path = std::env::temp_dir()
                .join(format!("red-report-{}.html", uuid::Uuid::new_v4().simple()));
            // Time-box the render: a watchdog flips the export's cancel flag after the
            // statement timeout (the row cap bounds size; this bounds a slow query).
            let cancel = Arc::new(AtomicBool::new(false));
            let watchdog = (limits.statement_timeout_ms != 0).then(|| {
                let c = cancel.clone();
                let ms = limits.statement_timeout_ms;
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    c.store(true, Ordering::Relaxed);
                })
            });
            // A throwaway progress channel — a one-shot report has no progress UI.
            let (progress, _rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
            let result = driver
                .export(&capped, &path, ExportFormat::Html, cancel, progress)
                .await;
            if let Some(w) = watchdog {
                w.abort();
            }
            match result {
                Ok(rows) => {
                    // Hand the path to the UI so it opens the file in the browser.
                    report.announce(&path);
                    let label = input
                        .get("title")
                        .and_then(Json::as_str)
                        .filter(|t| !t.trim().is_empty())
                        .map(|t| format!(" “{}”", t.trim()))
                        .unwrap_or_default();
                    (
                        format!(
                            "Generated report{label} with {rows} row(s) and opened it in the \
                            user's browser."
                        ),
                        true,
                    )
                }
                Err(RedError::Interrupted) => (
                    "error: the report query exceeded the assistant's statement timeout — narrow \
                    it (aggregate / add WHERE / LIMIT)."
                        .into(),
                    false,
                ),
                Err(e) => (format!("error: could not generate the report: {e}"), false),
            }
        }
        "propose_write" => {
            // Re-vet at execution (defense in depth): tier, read-only, and the
            // statement shape are all re-checked, never trusting that the caller
            // already gated it. By here the per-call user approval has been granted
            // (run_turn / the ACP permission flow); we only *run* an allowed shape.
            match assess_write(name, input, policy) {
                WriteAssessment::NeedsApproval { sql } => match driver.execute(&sql).await {
                    Ok(affected) => (
                        format!(
                            "Executed the write — {affected} row(s) affected. Verify with a \
                             SELECT if it matters."
                        ),
                        true,
                    ),
                    Err(e) => (format!("error: the write failed: {e}"), false),
                },
                WriteAssessment::Reject(why) => (format!("error: {why}"), false),
                WriteAssessment::NotWrite => (
                    "error: propose_write needs an INSERT/UPDATE/DELETE statement".into(),
                    false,
                ),
            }
        }
        other => (format!("error: unknown tool `{other}`"), false),
    };
    (cap_result_bytes(content, limits.max_result_bytes), ok)
}

/// Race a one-shot tool fetch against the policy's statement timeout. On expiry,
/// fire the fetch's [`AbortSignal`] so the engine stops, then surface
/// [`RedError::Timeout`]. A `0` timeout never fires. Mirrors the dispatch loop's
/// `with_timeout` so the AI path bounds queries the same way human paging does.
async fn guard_timeout<T>(
    timeout_ms: u64,
    abort: &AbortSignal,
    fut: impl std::future::Future<Output = red_core::Result<T>>,
) -> red_core::Result<T> {
    tokio::pin!(fut);
    let mut timed_out = false;
    let out = loop {
        tokio::select! {
            res = &mut fut => break res,
            _ = sleep_ms(timeout_ms), if !timed_out && timeout_ms != 0 => {
                timed_out = true;
                abort.abort();
            }
        }
    };
    match out {
        Err(RedError::Interrupted) if timed_out => Err(RedError::Timeout),
        other => other,
    }
}

/// Sleep `ms` milliseconds, or never (a `0` timeout means "no cap").
async fn sleep_ms(ms: u64) {
    if ms == 0 {
        std::future::pending::<()>().await
    } else {
        tokio::time::sleep(Duration::from_millis(ms)).await
    }
}

/// Cap one tool result at `max` bytes so a wide/long result can't balloon the
/// model's context. Truncates on a char boundary and appends a note. `0` disables.
fn cap_result_bytes(mut content: String, max: usize) -> String {
    if max == 0 || content.len() <= max {
        return content;
    }
    let mut cut = max;
    while cut > 0 && !content.is_char_boundary(cut) {
        cut -= 1;
    }
    content.truncate(cut);
    content.push_str("\n…(result truncated — it exceeded the size cap; narrow the query)");
    content
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

/// Whether `name` is a mutating tool — it never auto-runs and never auto-allows;
/// it rides the per-call approval gate on both backends (Feature B).
pub(crate) fn is_write_tool(name: &str) -> bool {
    name == "propose_write"
}

/// The outcome of vetting a `propose_write` call before it runs (Feature B). The
/// single source of truth, called by `run_turn` (to decide reject vs. prompt) and
/// by `run_tool` (to re-validate before executing). Keeping it in one place means
/// the gate the user sees and the gate the write rides can't drift apart.
pub(crate) enum WriteAssessment {
    /// Not a write tool — run it normally (no approval).
    NotWrite,
    /// Blocked outright (wrong tier, read-only connection, or a destructive shape):
    /// report this to the model without prompting the user.
    Reject(String),
    /// An allowed single INSERT/UPDATE/DELETE — prompt the user with this exact SQL,
    /// and only run it on Allow.
    NeedsApproval { sql: String },
}

/// Vet a tool call for the write gate. A `propose_write` is allowed only at the
/// `Write` tier, on a writable connection, and for a safe statement shape; anything
/// else is rejected (never silently run, never even prompted).
pub(crate) fn assess_write(name: &str, input: &Json, policy: &AiPolicy) -> WriteAssessment {
    if !is_write_tool(name) {
        return WriteAssessment::NotWrite;
    }
    if policy.tier != AiTier::Write {
        return WriteAssessment::Reject(
            "the write tool is not available at this access tier".into(),
        );
    }
    if policy.read_only {
        return WriteAssessment::Reject(
            "this connection is read-only — writes are disabled. Tell the user; do not retry."
                .into(),
        );
    }
    let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
    match write_shape(sql) {
        WriteShape::Ok => WriteAssessment::NeedsApproval {
            sql: sql.to_string(),
        },
        WriteShape::NotWrite => WriteAssessment::Reject(
            "propose_write is only for INSERT/UPDATE/DELETE — use run_select to read".into(),
        ),
        WriteShape::Blocked(why) => WriteAssessment::Reject(why.into()),
    }
}

/// The shape verdict for a candidate write statement.
enum WriteShape {
    /// A single, qualified INSERT/UPDATE/DELETE — eligible (still needs approval).
    Ok,
    /// Not a write at all (SELECT/WITH/empty).
    NotWrite,
    /// A shape blocked even with approval, with the reason to report.
    Blocked(&'static str),
}

/// Classify a candidate write conservatively (Feature B). The hard blocks — DDL and
/// privilege statements, an unqualified UPDATE/DELETE (no WHERE), and any chained
/// statement — are the cases per-call approval alone shouldn't be trusted to catch
/// (a rubber-stamped `DELETE` with no WHERE is catastrophic). False negatives are
/// fine: the user can always run those by hand in a query tab.
fn write_shape(sql: &str) -> WriteShape {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return WriteShape::Blocked("the statement is empty");
    }
    // No embedded terminator — a `;` mid-string could chain a second statement past
    // the keyword check (and past the user's eyes).
    if trimmed.contains(';') {
        return WriteShape::Blocked("multiple statements are not allowed — submit one at a time");
    }
    let lower = trimmed.to_ascii_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    match first {
        "select" | "with" => WriteShape::NotWrite,
        "insert" => WriteShape::Ok,
        "update" | "delete" => {
            // Require a WHERE clause so a whole-table mutation can't slip through.
            if lower.contains(" where ") || lower.contains("\nwhere ") {
                WriteShape::Ok
            } else {
                WriteShape::Blocked(
                    "an UPDATE/DELETE without a WHERE clause is blocked — add a WHERE, or run a \
                     full-table change yourself in a query tab",
                )
            }
        }
        // DROP / TRUNCATE / ALTER / CREATE / RENAME / GRANT / REVOKE / … — DDL and
        // privilege changes are never run through the assistant.
        _ => WriteShape::Blocked(
            "only INSERT/UPDATE/DELETE are allowed here — DDL (DROP/TRUNCATE/ALTER/…) must be run \
             manually in a query tab",
        ),
    }
}

/// The stable grounding instruction, tailored to the access tier (M-S7). Shared
/// with the ACP path, which folds it into the agent's first prompt (ACP
/// `session/prompt` has no system role). The tier line keeps the model's
/// expectations in step with the catalog it actually receives, but the *catalog*
/// is the real gate — the prompt is just courtesy.
pub(crate) fn system_prompt(ctx: &AiContext, policy: &AiPolicy) -> String {
    let tools_line = match policy.tier {
        AiTier::Off => {
            "You have NO database tools available — answer from the schema overview and the \
             conversation alone, and tell the user you cannot read the live database."
        }
        AiTier::Schema => {
            "You have schema-only tools: list_schema and describe_table. You can inspect \
             structure (tables, columns, types, keys) but you CANNOT read row data — there is no \
             query tool, so do not promise to run one."
        }
        AiTier::Read => {
            "You have read-only tools: list_schema, describe_table, run_select (capped SELECTs), \
             explain, and generate_report (render a capped SELECT to an HTML report opened in the \
             user's browser — use it when the user asks for a report). Use them to ground every \
             answer in the live database rather than guessing — discover objects with list_schema, \
             inspect structure with describe_table, and read data with run_select. Prefer small, \
             targeted queries with explicit columns and LIMIT."
        }
        AiTier::Write => {
            "You have the read tools (list_schema, describe_table, run_select, explain, \
             generate_report) AND a gated write tool, propose_write, for a SINGLE \
             INSERT/UPDATE/DELETE. Every propose_write call requires the user's explicit Allow on \
             the exact SQL — assume it may be denied, and never batch or chain statements. \
             UPDATE/DELETE must have a WHERE clause; DDL (DROP/TRUNCATE/ALTER/CREATE) is not \
             available — tell the user to run those by hand. Only write when the user has asked you \
             to change data; read first to get it right, and verify after."
        }
    };
    let mut s = format!(
        "You are Red's database assistant, embedded in a native SQL explorer. You help the user \
         explore and understand the database they are connected to.\n\n\
         {tools_line}\n\n\
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
    // A reopened conversation (M-S5) seeds the prior exchange once, so the model
    // picks up where the saved chat left off even though its session is fresh.
    if let Some(prior) = ctx
        .prior_transcript
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        s.push_str("Earlier in this conversation (for context):\n");
        s.push_str(prior.trim());
        s.push_str("\n\n---\n\n");
    }
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
    fn catalog_filters_by_tier() {
        use red_core::{AiPolicy, AiTier};
        let names = |tier| -> Vec<String> {
            tool_catalog(&AiPolicy {
                tier,
                ..AiPolicy::default()
            })
            .into_iter()
            .map(|t| t.name)
            .collect()
        };
        assert!(names(AiTier::Off).is_empty());
        assert_eq!(names(AiTier::Schema), ["list_schema", "describe_table"]);
        assert_eq!(
            names(AiTier::Read),
            [
                "list_schema",
                "describe_table",
                "run_select",
                "explain",
                "generate_report"
            ]
        );
    }

    #[test]
    fn result_byte_cap_truncates_on_char_boundary() {
        // Under the cap: returned verbatim.
        assert_eq!(cap_result_bytes("hello".into(), 10), "hello");
        // `0` disables the cap.
        assert_eq!(cap_result_bytes("hello".into(), 0), "hello");
        // A multi-byte string capped mid-codepoint truncates at the boundary below
        // the cap (never splitting a char) and notes the truncation.
        let capped = cap_result_bytes("ééééé".into(), 5);
        assert!(capped.starts_with("éé")); // 4 bytes ≤ 5; the 3rd 'é' would cross it
        assert!(capped.contains("result truncated"));
    }

    #[test]
    fn user_turn_folds_prior_transcript_once() {
        let ctx = AiContext {
            prior_transcript: Some("You: hi\n\nAssistant: hello".into()),
            ..Default::default()
        };
        let turn = user_turn("and now?", &ctx);
        assert!(turn.contains("Earlier in this conversation"));
        assert!(turn.contains("Assistant: hello"));
        // The actual message still comes last.
        assert!(turn.trim_end().ends_with("and now?"));
        // No prior transcript → no preamble.
        let plain = user_turn("hi", &AiContext::default());
        assert!(!plain.contains("Earlier in this conversation"));
        assert_eq!(plain, "hi");
    }

    #[test]
    fn catalog_has_the_readonly_tools_at_read_tier() {
        let catalog = tool_catalog(&AiPolicy::default());
        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "list_schema",
                "describe_table",
                "run_select",
                "explain",
                "generate_report"
            ]
        );
    }

    #[tokio::test]
    async fn generate_report_writes_an_html_file_and_announces_it() {
        use futures::StreamExt;

        // A tiny fixture DB.
        let db = std::env::temp_dir().join(format!("red-gr-{}.db", uuid::Uuid::new_v4().simple()));
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE t (id INTEGER, name TEXT);
                 INSERT INTO t VALUES (1, 'alpha'), (2, 'beta');",
            )
            .unwrap();
        }
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, 7);

        let (content, ok) = run_tool(
            &driver,
            "generate_report",
            &json!({ "sql": "SELECT id, name FROM t ORDER BY id", "title": "Widgets" }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(ok, "expected success, got: {content}");
        assert!(content.contains("Generated report"));

        // The announced path is a real, well-formed HTML file with the data.
        let (_session, event) = rx.next().await.expect("an AiReportReady event");
        let Event::AiReportReady {
            conversation_id,
            path,
        } = event
        else {
            panic!("expected AiReportReady");
        };
        assert_eq!(conversation_id, 7);
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("alpha") && html.contains("beta"));

        // A non-SELECT is refused, and nothing is announced.
        let (_content, ok) = run_tool(
            &driver,
            "generate_report",
            &json!({ "sql": "DELETE FROM t" }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(!ok);
        // Nothing announced: the channel is empty but still open (Err), not an item.
        assert!(rx.try_recv().is_err(), "a refused report must not announce");
    }

    #[test]
    fn write_gate_blocks_dangerous_shapes_and_allows_qualified() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        let assess = |sql: &str| assess_write("propose_write", &json!({ "sql": sql }), &write);
        let allowed = |sql: &str| matches!(assess(sql), WriteAssessment::NeedsApproval { .. });
        let rejected = |sql: &str| matches!(assess(sql), WriteAssessment::Reject(_));

        // Qualified writes are eligible (they still need approval).
        assert!(allowed("INSERT INTO t (a) VALUES (1)"));
        assert!(allowed("UPDATE t SET a = 1 WHERE id = 5"));
        assert!(allowed("DELETE FROM t WHERE id = 5"));
        // Unqualified mass mutations are hard-blocked.
        assert!(rejected("UPDATE t SET a = 1"));
        assert!(rejected("DELETE FROM t"));
        // DDL / privilege statements are never run via the tool.
        assert!(rejected("DROP TABLE t"));
        assert!(rejected("TRUNCATE t"));
        assert!(rejected("ALTER TABLE t ADD c int"));
        // No chaining a second statement past the gate.
        assert!(rejected("UPDATE t SET a=1 WHERE id=1; DROP TABLE t"));
        // A read query isn't a write.
        assert!(rejected("SELECT * FROM t"));
    }

    #[test]
    fn write_gate_respects_tier_and_read_only() {
        let qualified = json!({ "sql": "DELETE FROM t WHERE id = 1" });
        // Below the Write tier the write tool is rejected outright.
        let read = AiPolicy::default();
        assert!(matches!(
            assess_write("propose_write", &qualified, &read),
            WriteAssessment::Reject(_)
        ));
        // A read-only connection rejects it even at the Write tier.
        let read_only = AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        };
        assert!(matches!(
            assess_write("propose_write", &qualified, &read_only),
            WriteAssessment::Reject(_)
        ));
        // A read tool is never gated as a write.
        assert!(matches!(
            assess_write("run_select", &json!({ "sql": "SELECT 1" }), &read),
            WriteAssessment::NotWrite
        ));
    }

    #[test]
    fn catalog_offers_write_tool_only_at_write_tier_and_not_read_only() {
        let names = |p: AiPolicy| {
            tool_catalog(&p)
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        };
        // Read tier never offers the write tool.
        assert!(names(AiPolicy::default())
            .iter()
            .all(|n| n != "propose_write"));
        // Write tier offers it…
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        assert!(names(write).iter().any(|n| n == "propose_write"));
        // …but withholds it on a read-only connection.
        let write_ro = AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        };
        assert!(names(write_ro).iter().all(|n| n != "propose_write"));
    }

    #[test]
    fn write_approval_registry_parks_resolves_and_offsets_ids() {
        let mut st = AiState::default();
        let (tx, mut rx) = oneshot::channel();
        let id = st.park_permission(tx).expect("a fresh prompt parks");
        // Ids are offset so they never collide with the ACP manager's id space.
        assert!(id >= AI_REQUEST_BASE);
        st.resolve_permission(id, true);
        assert_eq!(rx.try_recv(), Ok(true));
        // Resolving a stale/unknown id is a harmless no-op.
        st.resolve_permission(id, false);
        st.resolve_permission(424242, false);
    }

    #[test]
    fn tool_call_budget_is_per_conversation_and_capped() {
        let mut state = AiState::default();
        // A cap of 2 admits two calls, then refuses the third on the same conversation.
        assert!(state.charge_tool_call(1, 2));
        assert!(state.charge_tool_call(1, 2));
        assert!(!state.charge_tool_call(1, 2));
        // A different conversation has its own fresh budget.
        assert!(state.charge_tool_call(2, 2));
        // `0` means unlimited.
        assert!(state.charge_tool_call(3, 0));
        assert!(state.charge_tool_call(3, 0));
    }
}
