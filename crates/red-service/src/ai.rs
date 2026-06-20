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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use red_ai::{
    AiProvider, CancelToken, ContentBlock, Message, Role, StopReason, ToolDef, TurnRequest,
};
use red_core::{AiPolicy, AiTier, RedError, Value};
use red_driver::{AbortSignal, DatabaseDriver, PageCap};
use serde_json::{json, Value as Json};
use tokio::sync::oneshot;

use crate::dispatch::{emit, Events};
use crate::protocol::{AiContext, AiDelta, AiUsage, ReportTheme};
use crate::{Event, SessionId};

/// A small, UI-agnostic announcer the `generate_report` tool uses to hand a
/// freshly-written report file to the UI to open in the browser. The tool stays
/// UI-free — it just announces a path; the caller turns it into an `AiReportReady`
/// event. Both backends construct one from the `events`/`session`/`conversation_id`
/// they hold; a `disabled()` sink (no channel) drops announcements (tests).
#[derive(Clone)]
pub(crate) struct ReportSink {
    events: Option<Events>,
    session: Option<SessionId>,
    conversation_id: u64,
    /// The active app theme, so `generate_report` can paint the report in Red's
    /// colors. Captured when the sink is built (per turn on the API-key path; at
    /// conversation start on the subscription path).
    theme: Option<ReportTheme>,
}

impl ReportSink {
    pub(crate) fn new(
        events: Events,
        session: Option<SessionId>,
        conversation_id: u64,
        theme: Option<ReportTheme>,
    ) -> Self {
        Self {
            events: Some(events),
            session,
            conversation_id,
            theme,
        }
    }

    /// A no-op sink — drops announcements. For tests and any path with no UI.
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self {
            events: None,
            session: None,
            conversation_id: 0,
            theme: None,
        }
    }

    /// The theme to paint the report with, if the UI supplied one.
    fn theme(&self) -> Option<&ReportTheme> {
        self.theme.as_ref()
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

    /// Ask the UI to open `sql` in a new query tab (the agent's open_query tool).
    fn announce_open_query(&self, sql: &str) {
        if let Some(events) = &self.events {
            emit(
                events,
                self.session,
                Event::AiOpenQuery {
                    conversation_id: self.conversation_id,
                    sql: sql.to_string(),
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

    /// Drop all per-conversation state — history, cancel token, cumulative tool tally
    /// — when the UI closes/deletes the conversation, so these maps stay bounded by
    /// what's open rather than every conversation ever touched this session. Cancels
    /// any in-flight turn first so its task winds down. (A turn still racing to its
    /// final history write can re-insert one entry; that's bounded, unlike the prior
    /// unconditional growth.)
    pub(crate) fn forget(&mut self, conversation_id: u64) {
        if let Some(tok) = self.cancels.remove(&conversation_id) {
            tok.cancel();
        }
        self.histories.remove(&conversation_id);
        self.tool_calls.remove(&conversation_id);
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
    // Where `generate_report` delivers its file so the UI opens it (Feature C);
    // carries the active theme so the report matches Red's colors.
    let report = ReportSink::new(
        events.clone(),
        session,
        conversation_id,
        context.theme.as_deref().cloned(),
    );

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
            description: "Write a custom HTML report for the user and open it in their browser. \
                YOU author the report: first read the data with run_select, then call this with \
                `html` set to the report's body — headings, prose/summary, one or more <table>s, \
                even an inline <svg> chart. Use semantic HTML and inline `style=\"…\"` for any \
                styling; a base stylesheet (light/dark) is already applied. Scripts and remote/\
                external resources (other domains, <script>, remote <img>/CSS) are stripped or \
                blocked for safety, so keep everything self-contained (data URIs for images). \
                For INTERACTIVE charts (hover tooltips, legends), pass `charts` — an array of \
                Chart.js v4 config objects — and reference each one from the body with an empty \
                <div data-red-chart=\"INDEX\"></div> placeholder (INDEX is the chart's position \
                in the array). The charts are rendered by a trusted built-in Chart.js; you supply \
                DATA only (no JavaScript/function callbacks — they are ignored). \
                For INTERACTIVE TABLES the user can search/sort/filter, pass `data` — named \
                datasets of {columns, rows} — and drop a <div data-red-table=\"NAME\"></div> \
                placeholder; the user gets a live filter box, click-to-sort headers, and per-column \
                filters. A chart can BIND to a dataset instead of carrying inline data — give it \
                {\"dataset\":\"NAME\",\"type\":\"bar\",\"x\":\"colName\",\"y\":[\"colA\"]} — and it \
                re-draws automatically when the user filters that dataset's table. \
                For DASHBOARD-style controls (like Grafana variables) that drive EVERY table and \
                bound chart at once, pass `filters` — e.g. a multi-select to show only chosen \
                regions: {\"column\":\"Region\",\"type\":\"multiselect\"}. They render as a control \
                bar at the top of the report. Prefer this (data + bound charts + a table + \
                filters) when the user wants to explore/slice the data; prefer inline-data charts \
                for a fixed visual. \
                Use this when the user asks for a report."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "html": { "type": "string", "description": "The report BODY as self-contained HTML (no <html>/<head>/<body> wrapper — that's added). Reference charts with <div data-red-chart=\"INDEX\"></div> and interactive tables with <div data-red-table=\"NAME\"></div> placeholders." },
                    "title": { "type": "string", "description": "Report title (browser tab + heading)." },
                    "charts": {
                        "type": "array",
                        "description": "Optional interactive charts. Each item is EITHER a full Chart.js v4 config with inline data, e.g. {\"type\":\"bar\",\"data\":{\"labels\":[…],\"datasets\":[{\"label\":\"Revenue\",\"data\":[…]}]},\"options\":{…}}, OR a dataset binding {\"dataset\":\"NAME\",\"type\":\"bar\",\"x\":\"colName\",\"y\":[\"col1\",\"col2\"],\"aggregate\":\"sum\",\"options\":{…}} that derives its data from a named `data` dataset and follows that table's filters. type is one of bar, line, pie, doughnut, radar, polarArea, scatter, bubble. aggregate (sum/avg/min/max/count/none, default none) groups rows sharing an x value. Data only — no functions/callbacks. Place a <div data-red-chart=\"INDEX\"></div> in the body for each.",
                        "items": { "type": "object" },
                    },
                    "data": {
                        "type": "object",
                        "description": "Optional named datasets for interactive tables and filter-linked charts, e.g. {\"sales\":{\"columns\":[\"Month\",\"Region\",\"Revenue\"],\"rows\":[[\"Jan\",\"NA\",120],[\"Feb\",\"EU\",90]]}}. Each value is {columns:[string], rows:[[cell,…]]} (cells are strings/numbers/null). Reference a dataset with <div data-red-table=\"sales\"></div> for a searchable/sortable table, and/or bind charts to it via {\"dataset\":\"sales\",…}.",
                        "additionalProperties": { "type": "object" },
                    },
                    "filters": {
                        "type": "array",
                        "description": "Optional report-wide filter controls (Grafana-style variables) that filter EVERY table and bound chart. Each is {\"column\":\"Region\",\"type\":\"multiselect\",\"label\":\"Region\",\"dataset\":\"sales\",\"default\":[…]}. type: multiselect (checkbox dropdown — pick which values to show; this is the 'show only selected regions' control), select (single value), range (numeric min/max), or search (substring). column must exist in the dataset(s); omit `dataset` to apply to all datasets that have that column. `default` pre-selects values (multiselect/select). They appear in a bar at the top; no body placeholder needed (optionally place <div data-red-filters></div> to position it).",
                        "items": { "type": "object" },
                    },
                },
                "required": ["html"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "open_query".into(),
            description: "Open a SQL query in a new editor tab in the user's workspace so they have \
                it in the grid. A read-only SELECT runs automatically; anything else is just loaded \
                for the user to run themselves. Use this to hand the user a query to explore or \
                build on — it does NOT return rows to you (use run_select for that)."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "The SQL to open in a new query tab." },
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
                            "\n(truncated to {limit} rows — the result may have more; add LIMIT or \
                            a WHERE clause to narrow it)"
                        ));
                    }
                    (out, true)
                }
                Err(RedError::Timeout) => (
                    "error: the query exceeded the agent's statement timeout — it was \
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
            let body = input
                .get("html")
                .and_then(Json::as_str)
                .unwrap_or("")
                .trim();
            if body.is_empty() {
                return (
                    "error: generate_report needs `html` — the report body you authored".into(),
                    false,
                );
            }
            let title = input.get("title").and_then(Json::as_str);
            // Optional interactive charts: keep only well-formed Chart.js spec
            // objects. They are embedded as inert data and rendered by the trusted
            // bundle (see `wrap_report_html`); anything that isn't an object is
            // dropped rather than smuggled into the document.
            let charts: Vec<Json> = input
                .get("charts")
                .and_then(Json::as_array)
                .map(|items| items.iter().filter(|c| c.is_object()).cloned().collect())
                .unwrap_or_default();
            // Optional named datasets for interactive (filterable/sortable) tables
            // and filter-linked charts. Kept only if it's an object map; embedded as
            // inert data and rendered client-side by the trusted bundle.
            let data = input.get("data").filter(|v| v.is_object());
            // Optional report-wide filter controls (Grafana-style variables): a bar
            // of multiselect/select/range/search controls bound to dataset columns
            // that drive every table and bound chart at once. Objects only.
            let filters: Vec<Json> = input
                .get("filters")
                .and_then(Json::as_array)
                .map(|items| items.iter().filter(|c| c.is_object()).cloned().collect())
                .unwrap_or_default();
            // Wrap the model's HTML in a sandboxed, themed shell (strict CSP) and
            // open it in the browser. With charts/data, the shell adds a nonce-gated
            // bundle and a `connect-src 'none'` (no-egress) policy. The active app
            // theme (if any) paints the report in Red's palette.
            let html = wrap_report_html(title, body, &charts, data, &filters, report.theme());
            let path = std::env::temp_dir()
                .join(format!("red-report-{}.html", uuid::Uuid::new_v4().simple()));
            match write_report_file(&path, &html) {
                Ok(()) => {
                    // Hand the path to the UI so it opens the file in the browser.
                    report.announce(&path);
                    let label = title
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(|t| format!(" “{t}”"))
                        .unwrap_or_default();
                    (
                        format!("Generated the report{label} and opened it in the user's browser."),
                        true,
                    )
                }
                Err(e) => (
                    format!("error: could not write the report file: {e}"),
                    false,
                ),
            }
        }
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
        "propose_write" => {
            // Re-vet at execution (defense in depth): tier, read-only, and the
            // statement shape are all re-checked, never trusting that the caller
            // already gated it. By here the per-call user approval has been granted
            // (run_turn / the ACP permission flow); we only *run* an allowed shape.
            match assess_write(name, input, policy) {
                WriteAssessment::NeedsApproval { sql } => match driver.execute(&sql).await {
                    Ok(affected) => {
                        // Durable record of what the agent actually changed (Feature B).
                        crate::audit::record_write(&sql, affected);
                        (
                            format!(
                                "Executed the write — {affected} row(s) affected. Verify with a \
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

/// The tools that never mutate data and so may run on any backend without the
/// per-call write gate. This is an allowlist on purpose: anything *not* named here
/// is treated as a write, so a future tool fails *closed* (gated, withheld from the
/// MCP/ACP path) until it's explicitly vetted and added — rather than slipping
/// through a denylist someone forgot to extend.
pub(crate) const READ_ONLY_TOOLS: &[&str] = &[
    "list_schema",
    "describe_table",
    "run_select",
    "explain",
    "generate_report",
    // Hands the user a SQL query to open in a tab — no DB mutation of its own.
    "open_query",
];

/// Whether `name` is a mutating tool — it never auto-runs and never auto-allows;
/// it rides the per-call approval gate on both backends (Feature B). Defined as the
/// complement of [`READ_ONLY_TOOLS`] so a new, unlisted tool is treated as a write.
pub(crate) fn is_write_tool(name: &str) -> bool {
    !READ_ONLY_TOOLS.contains(&name)
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
///
/// Classification runs on a **noise-stripped** copy (string literals, quoted
/// identifiers, and comments blanked) so a keyword or `;` *inside a literal* can't
/// fool the gate — e.g. `UPDATE t SET note = 'see where'` (no real WHERE) is still
/// blocked, and a `;` inside a string isn't read as statement chaining.
fn write_shape(sql: &str) -> WriteShape {
    let stripped = strip_sql_noise(sql);
    let trimmed = stripped.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return WriteShape::Blocked("the statement is empty");
    }
    // No embedded terminator — a real `;` chains a second statement past the keyword
    // check (and past the user's eyes).
    if trimmed.contains(';') {
        return WriteShape::Blocked("multiple statements are not allowed — submit one at a time");
    }
    let lower = trimmed.to_ascii_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    match first {
        "select" | "with" => WriteShape::NotWrite,
        "insert" => WriteShape::Ok,
        "update" | "delete" => {
            // Require a real WHERE keyword (a word token, not a substring) so a
            // whole-table mutation can't slip through.
            if has_word(&lower, "where") {
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

/// Blank out the parts of `sql` that aren't structure — single-quoted strings
/// (with `''` escapes), double-quoted / backtick-quoted identifiers, and `--` line
/// and `/* */` block comments — replacing each run with spaces so positions and the
/// surrounding keywords are preserved. Used so the write classifier reasons about
/// real SQL keywords, never text that merely *looks* like one inside a literal.
fn strip_sql_noise(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // String literal / quoted identifier — consume to the matching close,
            // honoring the doubled-quote escape (`''`, `""`).
            '\'' | '"' | '`' => {
                out.push(' ');
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n == c {
                        // A doubled quote is an escape, not a close.
                        if chars.peek() == Some(&c) {
                            chars.next();
                            out.push(' ');
                            out.push(' ');
                            continue;
                        }
                        break;
                    }
                    out.push(' ');
                }
                out.push(' ');
            }
            // Line comment `-- …` to end of line.
            '-' if chars.peek() == Some(&'-') => {
                out.push(' ');
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    chars.next();
                    out.push(' ');
                }
            }
            // Block comment `/* … */`.
            '/' if chars.peek() == Some(&'*') => {
                out.push(' ');
                chars.next();
                out.push(' ');
                while let Some(n) = chars.next() {
                    out.push(' ');
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

/// Whether `word` appears in `haystack` as a whole word (delimited by non-word
/// chars), not merely as a substring. `haystack` is assumed already lowercased.
fn has_word(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|tok| tok == word)
}

/// The report shell's inline stylesheet — a neutral, light/dark base the model's
/// `style="…"` can build on. No external fonts/assets (the CSP forbids them).
const REPORT_STYLE: &str = concat!(
    "<style>",
    ":root{color-scheme:light dark}",
    "*{box-sizing:border-box}",
    "body{margin:0;padding:32px 24px;max-width:1100px;margin-inline:auto;",
    "font:15px/1.6 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;",
    "background:#fff;color:#1a1a1a}",
    "h1{font-size:22px}h2{font-size:17px;margin-top:1.6em}",
    "table{border-collapse:collapse;width:100%;margin:12px 0;font-variant-numeric:tabular-nums}",
    "th,td{padding:7px 12px;text-align:left;border-bottom:1px solid #e5e7eb}",
    "th{background:#f6f7f9;font-weight:600}",
    "tbody tr:nth-child(even){background:#fafbfc}",
    "code,pre{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;background:#f3f4f6;border-radius:4px}",
    "code{padding:1px 5px}pre{padding:12px;overflow:auto}",
    "@media(prefers-color-scheme:dark){",
    "body{background:#0f1115;color:#e6e6e6}",
    "th,td{border-bottom-color:#262a31}th{background:#161a20}",
    "tbody tr:nth-child(even){background:#13161b}",
    "code,pre{background:#1b2028}}",
    "</style>",
);

/// The report's base document style. With a `theme` (the active Red palette) the
/// page, tables and code blocks are painted in Red's colors and pinned to its
/// light/dark; without one, fall back to [`REPORT_STYLE`] (built-in, OS-driven).
fn report_style(theme: Option<&ReportTheme>) -> String {
    let Some(th) = theme else {
        return REPORT_STYLE.to_string();
    };
    let scheme = if th.is_dark { "dark" } else { "light" };
    format!(
        "<style>:root{{color-scheme:{scheme}}}*{{box-sizing:border-box}}\
         body{{margin:0;padding:32px 24px;max-width:1100px;margin-inline:auto;\
         font:15px/1.6 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;\
         background:{bg};color:{fg}}}\
         h1{{font-size:22px}}h2{{font-size:17px;margin-top:1.6em}}a{{color:{accent}}}\
         table{{border-collapse:collapse;width:100%;margin:12px 0;font-variant-numeric:tabular-nums}}\
         th,td{{padding:7px 12px;text-align:left;border-bottom:1px solid {border}}}\
         th{{background:{surface};font-weight:600}}\
         tbody tr:nth-child(even){{background:{hover}}}\
         code,pre{{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;background:{surface};border-radius:4px}}\
         code{{padding:1px 5px}}pre{{padding:12px;overflow:auto}}</style>",
        bg = th.bg,
        fg = th.fg,
        accent = th.accent,
        border = th.border,
        surface = th.surface,
        hover = th.hover,
    )
}

/// Serialize the theme into the report's inert data payload so the chart/table/
/// filter renderer paints in the same colors. Built by hand (rather than deriving
/// `Serialize`) to keep `ReportTheme` a plain data type and the key names explicit.
fn report_theme_json(theme: Option<&ReportTheme>) -> Json {
    match theme {
        None => Json::Null,
        Some(th) => json!({
            "is_dark": th.is_dark,
            "bg": th.bg,
            "surface": th.surface,
            "fg": th.fg,
            "muted": th.muted,
            "border": th.border,
            "grid": th.grid,
            "hover": th.hover,
            "accent": th.accent,
            "ring": th.ring,
            "palette": th.palette,
        }),
    }
}

/// Write a finished report to `path`, owner-readable only (`0600` on Unix). A
/// report can carry real query data, and on a shared temp dir (Linux `/tmp`) a
/// world-readable file would let another local user read it — so restrict it at
/// creation rather than writing world-readable and tightening after.
fn write_report_file(path: &Path, html: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(html.as_bytes())
}

/// The trusted in-report chart bundle: Chart.js v4 (UMD, minified) + our renderer
/// (`assets/report-renderer.js`). This is the ONLY code allowed to run in a report
/// — it is injected behind a per-report CSP nonce, so the model's HTML and the
/// chart specs (which never carry the nonce) cannot execute. See `assets/README.md`
/// to regenerate after a Chart.js bump.
const REPORT_CHARTS_JS: &str = include_str!("../assets/report-charts.js");

/// Wrap an AI-authored report body in a sandboxed, themed HTML document (Feature C).
/// The safety boundary is a strict Content-Security-Policy: `default-src 'none'`
/// blocks ALL scripts (inline and remote), remote fetches, and remote
/// images/CSS/fonts/frames; `style-src 'unsafe-inline'` allows the model's inline
/// styling; `img-src data:` allows inline (data-URI) images and SVG. So even if the
/// body (or a value injected from the data) smuggles a `<script>` or a remote URL,
/// the browser neither runs nor loads it. `<script>` blocks are also stripped
/// defensively, belt-and-suspenders.
///
/// When the model supplies `charts` or `data`, the report gains interactivity:
/// the specs/datasets/filters are embedded as inert `application/json` DATA the
/// model authors, and our trusted bundle (the only thing carrying the CSP `nonce`)
/// renders interactive charts (Chart.js), filterable/sortable tables over the
/// embedded `data`, and a report-wide filter bar (`filters`) that slices every
/// table and bound chart at once. The CSP keeps the hole tight: scripts run only with the nonce
/// (so the model cannot inject runnable code), and `connect-src 'none'` denies all
/// network egress (so even the trusted bundle cannot exfiltrate the data, and all
/// filtering happens client-side over what's already embedded — never a callback
/// to the database). The payload is pure data; the bundle never evals it and
/// writes every table cell via `textContent`.
fn wrap_report_html(
    title: Option<&str>,
    body: &str,
    charts: &[Json],
    data: Option<&Json>,
    filters: &[Json],
    theme: Option<&ReportTheme>,
) -> String {
    let title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Red — report");
    let t = red_driver::html_escape(title);
    let safe_body = strip_scripts(body);
    // The base document style — Red's active theme if the UI supplied one, else
    // the built-in light/dark (follows the OS).
    let style = report_style(theme);

    let has_data = data
        .and_then(Json::as_object)
        .is_some_and(|o| !o.is_empty());
    if charts.is_empty() && !has_data {
        return format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
             style-src 'unsafe-inline'; img-src data:\">\
             <title>{t}</title>{style}</head><body>{safe_body}</body></html>\n"
        );
    }

    // Unguessable per-report nonce — only our bundle carries it, so a `<script>`
    // smuggled through the body or a spec value has no valid nonce and won't run.
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let payload = json!({
        "charts": charts,
        "data": data.cloned().unwrap_or(Json::Null),
        "filters": filters,
        "theme": report_theme_json(theme),
    })
    .to_string();
    // Neutralize `</script>` breakout from the inert data block; `<` parses
    // back to `<` under JSON.parse, so the data round-trips intact.
    let data = payload.replace('<', "\\u003c");
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
         script-src 'nonce-{nonce}'; style-src 'unsafe-inline'; img-src data:; \
         connect-src 'none'\">\
         <title>{t}</title>{style}</head><body>{safe_body}\
         <script id=\"red-report-data\" type=\"application/json\">{data}</script>\
         <script nonce=\"{nonce}\">{REPORT_CHARTS_JS}</script></body></html>\n"
    )
}

/// Remove `<script>…</script>` blocks (case-insensitive) from `html`. Defensive
/// only — the report's CSP already forbids script execution; this just keeps the
/// rendered document clean. An unterminated `<script` drops the remainder.
fn strip_scripts(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        if lower[i..].starts_with("<script") {
            match lower[i..].find("</script>") {
                Some(rel) => {
                    i += rel + "</script>".len();
                    continue;
                }
                None => break,
            }
        }
        let ch = html[i..].chars().next().expect("char at boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
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
             explain, open_query (open a SQL query in a new editor tab in the user's workspace — a \
             read-only SELECT runs automatically), and generate_report (you author an HTML report \
             from data you've read — with optional interactive Chart.js charts — and it opens in \
             the user's browser; use it when the user asks for a report). Use them to ground every \
             answer in the live database rather than \
             guessing — discover objects with list_schema, inspect structure with describe_table, \
             and read data with run_select. Use open_query to hand the user a query to explore in \
             the grid. Prefer small, targeted queries with explicit columns and LIMIT."
        }
        AiTier::Write => {
            "You have the read tools (list_schema, describe_table, run_select, explain, open_query, \
             generate_report) AND a gated write tool, propose_write, for a SINGLE \
             INSERT/UPDATE/DELETE. Every propose_write call requires the user's explicit Allow on \
             the exact SQL — assume it may be denied, and never batch or chain statements. \
             UPDATE/DELETE must have a WHERE clause; DDL (DROP/TRUNCATE/ALTER/CREATE) is not \
             available — tell the user to run those by hand. Only write when the user has asked you \
             to change data; read first to get it right, and verify after."
        }
    };
    let mut s = format!(
        "You are Red's database agent, embedded in a native SQL explorer. You help the user \
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
    if let Some(tab) = ctx.current_tab.as_deref().filter(|s| !s.trim().is_empty()) {
        s.push_str("The user is currently viewing tab ");
        s.push_str(tab.trim());
        s.push_str(
            ". When they say \"this\"/\"the current tab/query/result\", they mean this.\n\n",
        );
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
                "generate_report",
                "open_query"
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
                "generate_report",
                "open_query"
            ]
        );
    }

    #[tokio::test]
    async fn generate_report_wraps_ai_html_and_announces_it() {
        use futures::StreamExt;

        // generate_report renders model-authored HTML — no DB call. A no-op driver is
        // enough (the tool never touches it).
        let db = std::env::temp_dir().join(format!("red-gr-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, 7, None);

        let (content, ok) = run_tool(
            &driver,
            "generate_report",
            &json!({
                "title": "Widgets",
                "html": "<h1>Top widgets</h1><p>alpha leads beta.</p>\
                         <script>fetch('http://evil')</script>",
            }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(ok, "expected success, got: {content}");
        assert!(content.contains("Generated the report"));

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
        // The model's body is present and the title is carried through.
        assert!(html.contains("<h1>Top widgets</h1>"));
        assert!(html.contains("Widgets"));
        // Sandboxed: a strict CSP is set and the smuggled <script> is stripped.
        assert!(html.contains("Content-Security-Policy"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("evil"));

        // An empty body is refused, and nothing is announced.
        let (_content, ok) = run_tool(
            &driver,
            "generate_report",
            &json!({ "html": "   " }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(!ok);
        // Nothing announced: the channel is empty but still open (Err), not an item.
        assert!(rx.try_recv().is_err(), "a refused report must not announce");
    }

    #[tokio::test]
    async fn generate_report_with_charts_is_nonce_gated_and_egress_free() {
        use futures::StreamExt;

        let db = std::env::temp_dir().join(format!("red-grc-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, 11, None);

        let (content, ok) = run_tool(
            &driver,
            "generate_report",
            &json!({
                "title": "Sales",
                "html": "<h1>Sales</h1><div data-red-chart=\"0\"></div>",
                "charts": [
                    {
                        "type": "bar",
                        // A label that tries to break out of the data block.
                        "data": { "labels": ["</script><script>alert(1)</script>"],
                                  "datasets": [{ "label": "Q1", "data": [3] }] },
                    },
                    // Non-object entries are dropped, not embedded.
                    "not-a-chart",
                ],
            }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(ok, "expected success, got: {content}");

        let (_session, event) = rx.next().await.expect("an AiReportReady event");
        let Event::AiReportReady { path, .. } = event else {
            panic!("expected AiReportReady");
        };
        let html = std::fs::read_to_string(&path).unwrap();

        // The chart hole is tight: scripts run only with the nonce, and there is
        // zero network egress so the bundle cannot leak the data it charts.
        assert!(html.contains("script-src 'nonce-"));
        assert!(html.contains("connect-src 'none'"));
        // The trusted bundle is injected behind the nonce; the inert data block is not.
        assert!(html.contains("<script nonce="));
        assert!(html.contains("Chart.js v4"));
        assert!(html.contains("id=\"red-report-data\" type=\"application/json\""));
        // The breakout attempt is neutralized: no stray executable <script> from
        // the data, and the `<` is escaped to its JSON unicode form.
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("\\u003c/script>"));
        // Non-object chart entries are filtered out of the embedded payload.
        assert!(!html.contains("not-a-chart"));
    }

    #[tokio::test]
    async fn generate_report_with_data_embeds_datasets_for_interactive_tables() {
        use futures::StreamExt;

        let db = std::env::temp_dir().join(format!("red-grd-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, 13, None);

        let (content, ok) = run_tool(
            &driver,
            "generate_report",
            &json!({
                "title": "Sales",
                "html": "<h1>Sales</h1><div data-red-table=\"sales\"></div>",
                "data": {
                    "sales": {
                        "columns": ["Month", "Region", "Revenue"],
                        "rows": [["Jan", "NA", 120], ["Feb", "EU", 90]],
                    },
                },
            }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(ok, "expected success, got: {content}");

        let (_session, event) = rx.next().await.expect("an AiReportReady event");
        let Event::AiReportReady { path, .. } = event else {
            panic!("expected AiReportReady");
        };
        let html = std::fs::read_to_string(&path).unwrap();

        // `data` alone (no charts) still triggers the interactive, no-egress shell.
        assert!(html.contains("script-src 'nonce-"));
        assert!(html.contains("connect-src 'none'"));
        assert!(html.contains("<script nonce="));
        // The dataset is embedded as inert data for client-side filtering.
        assert!(html.contains("id=\"red-report-data\" type=\"application/json\""));
        assert!(html.contains("\"sales\""));
        assert!(html.contains("Revenue"));
    }

    #[tokio::test]
    async fn generate_report_embeds_report_wide_filters() {
        use futures::StreamExt;

        let db = std::env::temp_dir().join(format!("red-grf-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, 17, None);

        let (content, ok) = run_tool(
            &driver,
            "generate_report",
            &json!({
                "title": "Sales",
                "html": "<h1>Sales</h1><div data-red-table=\"sales\"></div>",
                "data": {
                    "sales": {
                        "columns": ["Month", "Region", "Revenue"],
                        "rows": [["Jan", "NA", 120], ["Feb", "EU", 90]],
                    },
                },
                "filters": [
                    { "column": "Region", "type": "multiselect" },
                    "not-an-object",
                ],
            }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(ok, "expected success, got: {content}");

        let (_session, event) = rx.next().await.expect("an AiReportReady event");
        let Event::AiReportReady { path, .. } = event else {
            panic!("expected AiReportReady");
        };
        let html = std::fs::read_to_string(&path).unwrap();
        // The filter definition rides in the inert payload (non-object dropped).
        assert!(html.contains("\"filters\""));
        assert!(html.contains("multiselect"));
        assert!(!html.contains("not-an-object"));
        assert!(html.contains("connect-src 'none'"));
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
        // A `where` inside a string literal or comment is NOT a real WHERE — the
        // statement is still an unqualified mutation and must be blocked.
        assert!(rejected("UPDATE t SET note = 'see where you go'"));
        assert!(rejected("DELETE FROM t -- delete where id = 1"));
        // Conversely, a real WHERE with a `;` inside a string literal is a single,
        // qualified statement — allowed (the `;` isn't statement chaining).
        assert!(allowed("UPDATE t SET note = 'a;b' WHERE id = 1"));
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

    /// A scripted `AiProvider`: the first turn requests `propose_write` with `sql`,
    /// the second ends the turn. Lets the test drive the API-key write round-trip
    /// without a network or a real model.
    struct ScriptedWrite {
        calls: std::sync::atomic::AtomicUsize,
        sql: String,
    }

    #[async_trait::async_trait]
    impl red_ai::AiProvider for ScriptedWrite {
        async fn stream_turn(
            &self,
            _req: &red_ai::TurnRequest,
            _tx: &tokio::sync::mpsc::UnboundedSender<red_ai::Delta>,
            _cancel: &CancelToken,
        ) -> red_ai::Result<red_ai::TurnOutcome> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (content, stop_reason) = if n == 0 {
                (
                    vec![ContentBlock::ToolUse {
                        id: "w1".into(),
                        name: "propose_write".into(),
                        input: json!({ "sql": self.sql }),
                    }],
                    StopReason::ToolUse,
                )
            } else {
                (
                    vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                    StopReason::EndTurn,
                )
            };
            Ok(red_ai::TurnOutcome {
                message: Message {
                    role: Role::Assistant,
                    content,
                },
                stop_reason,
                usage: red_ai::Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn api_key_write_is_gated_by_approval_then_executes() {
        use futures::StreamExt;

        let db = std::env::temp_dir().join(format!("red-bw-{}.db", uuid::Uuid::new_v4().simple()));
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);
                 INSERT INTO t VALUES (1, 'before');",
            )
            .unwrap();
        }
        // Writable connection at the Write tier.
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, false));
        let provider: Arc<dyn red_ai::AiProvider> = Arc::new(ScriptedWrite {
            calls: std::sync::atomic::AtomicUsize::new(0),
            sql: "UPDATE t SET name = 'after' WHERE id = 1".into(),
        });
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let state = Arc::new(Mutex::new(AiState::default()));
        let policy = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };

        // Read the current `name` through the driver (a fresh windowed fetch).
        let name_now = |driver: Arc<dyn DatabaseDriver>| async move {
            let abort = AbortSignal::new();
            let page = driver
                .fetch_page(
                    "SELECT name FROM t WHERE id = 1",
                    0,
                    1,
                    PageCap::Display { key: None },
                    &abort,
                )
                .await
                .unwrap();
            page.rows[0][0].to_string()
        };

        let turn = tokio::spawn(run_turn(
            provider,
            driver.clone(),
            tx,
            state.clone(),
            None,
            1,
            "m".into(),
            false,
            policy,
            "change it".into(),
            AiContext::default(),
            CancelToken::new(),
        ));

        // The first thing the user sees is the write-approval prompt, carrying the
        // exact SQL — the write has NOT run yet.
        let request_id = tokio::time::timeout(Duration::from_secs(5), async {
            // The very first event must be the approval prompt — nothing runs first.
            match rx.next().await.expect("an event").1 {
                Event::AiPermissionRequest {
                    request_id, detail, ..
                } => {
                    assert!(detail
                        .unwrap_or_default()
                        .contains("UPDATE t SET name = 'after'"));
                    request_id
                }
                _ => panic!("the write must prompt before doing anything"),
            }
        })
        .await
        .expect("a permission prompt arrives");
        assert!(name_now(driver.clone()).await.contains("before"));

        // Approve → the write runs and the turn completes.
        lock(&state).resolve_permission(request_id, true);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    rx.next().await.expect("an event").1,
                    Event::AiTurnFinished { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("the turn finishes after approval");
        turn.await.unwrap();

        assert!(name_now(driver).await.contains("after"));
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
