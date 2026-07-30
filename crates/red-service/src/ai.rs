//! The assistant's backend half: the agentic loop and the read-only tool catalog
//! it stands on. Mirrors the export/updater pattern: a turn runs as a spawned
//! task off the dispatch loop, streams `AiDelta` events as tokens arrive, and
//! drives the model → tool → model loop itself (the plain Messages API tool-use
//! loop, on the service thread).
//!
//! Every tool is backed by a `DatabaseDriver` seam that already exists and
//! inherits its guard: `list_schema`/`describe_table`/`explain` are always safe,
//! and `run_select` is row-capped and rejects non-`SELECT` SQL; the model gets
//! the same windowed, never-materialized reads a human does, and (in M1) cannot
//! mutate anything.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use red_ai::{
    AiProvider, CancelToken, ContentBlock, Message, Role, StopReason, ToolDef, TurnRequest,
};
use red_core::doc::{
    CollKind, DocPlan, DocSchema, DocType, DocUpdate, DocValue, DocWrite, Document, FindQuery,
    IndexInfo, IndexSpec, OpClass, classify_doc_op, pipeline_write_stage, server_js_operator,
};
use red_core::kv::{
    KeyMeta, KeyTemplate, KvCollection, KvValue, RespValue, ScanBudget, ScanCursor, StringTtl,
    analyze_keyspace, infer_key_templates,
};
// The read gate below and the write gate in `write_shape` share their stripping,
// their whole-word test, and their token lists with the UI's own gates, so a fix to
// one can't leave the others behind.
use red_core::sql::{DANGEROUS_FNS, Dialect, WRITE_TOKENS, has_word, strip_noise};
use red_core::{
    ActivityKind, ActivityStatus, AiLimits, AiPolicy, AiTier, FkEdge, RedError, TableRef, Value,
};
use red_driver::{AbortSignal, DatabaseDriver, DocDriver, KvDriver, PageCap};
use serde_json::{Value as Json, json};
use tokio::sync::oneshot;

use crate::dispatch::{Events, emit};
use crate::protocol::{AiContext, AiDelta, AiUsage, ConversationId, ReportTheme, RequestId};
use crate::{Event, SessionId};

/// Which engine the agent turn is grounded in. The model→tool loop, streaming, budget,
/// write gate, and history are identical for both; only the tool catalog, the tool
/// execution, and the system prompt differ. A KV (Redis) turn exposes the `kv_*` read
/// tools; a SQL turn the schema/query tools.
#[derive(Clone)]
pub(crate) enum AiBackend {
    Sql {
        driver: Arc<dyn DatabaseDriver>,
        /// The engine's lexical dialect, threaded into every gate that scans SQL
        /// (`is_read_only_select`, `write_shape`): scanning a statement with the
        /// wrong string/comment rules is a gate bypass, not a nicety — e.g.
        /// Postgres ends `'a\'` at the second quote, so what follows is live SQL.
        dialect: Dialect,
    },
    Kv(Arc<dyn KvDriver>),
    Doc(Arc<dyn DocDriver>),
}

impl AiBackend {
    /// The tier-filtered tool catalog this backend offers under `policy`. Routes to
    /// the SQL schema/query tools, the Redis `kv_*` tools, or the MongoDB doc tools.
    pub(crate) fn catalog(&self, policy: &AiPolicy) -> Vec<ToolDef> {
        match self {
            AiBackend::Sql { .. } => tool_catalog(policy),
            AiBackend::Kv(_) => kv_tool_catalog(policy),
            AiBackend::Doc(_) => doc_tool_catalog(policy),
        }
    }

    /// The SQL lexical dialect the gates must scan with; [`Dialect::Generic`]
    /// for the non-SQL backends (their gates never lex SQL).
    pub(crate) fn dialect(&self) -> Dialect {
        match self {
            AiBackend::Sql { dialect, .. } => *dialect,
            AiBackend::Kv(_) | AiBackend::Doc(_) => Dialect::Generic,
        }
    }

    /// The full grounding system prompt for this backend under `ctx`/`policy`.
    pub(crate) fn system_prompt(&self, ctx: &AiContext, policy: &AiPolicy) -> String {
        match self {
            AiBackend::Sql { .. } => system_prompt(ctx, policy),
            AiBackend::Kv(_) => kv_system_prompt(ctx, policy),
            AiBackend::Doc(_) => doc_system_prompt(ctx, policy),
        }
    }

    /// Whether `name` is a mutating tool for this backend. Used to withhold writes
    /// over the subscription/MCP path (each backend has its own writer set: the SQL
    /// `propose_*` tools vs. the Redis `kv_*` writers).
    pub(crate) fn is_write_tool(&self, name: &str) -> bool {
        // Both backends fail *closed*: a tool is a write unless it's explicitly
        // named in the read-only allowlist (`READ_ONLY_TOOLS`, which lists the
        // `kv_*` reads too). Classifying KV via the `KV_WRITE_TOOLS` denylist
        // here would fail *open* — a future KV writer not added to that list
        // would be advertised over MCP and auto-allowed over ACP with no
        // approval. (`is_kv_write_tool` still routes the known writers to their
        // KV-specific validator inside `assess_write`.)
        is_write_tool(name)
    }

    /// Run one tool call against this backend's driver, returning `(content, ok)`.
    pub(crate) async fn run_tool(
        &self,
        name: &str,
        input: &Json,
        policy: &AiPolicy,
        cancel: &CancelToken,
        report: &ReportSink,
    ) -> (String, bool) {
        match self {
            AiBackend::Sql { driver, dialect } => {
                run_tool(driver, *dialect, name, input, policy, cancel, report).await
            }
            AiBackend::Kv(d) => kv_run_tool(d, name, input, policy, cancel, report).await,
            AiBackend::Doc(d) => doc_run_tool(d, name, input, policy, cancel, report).await,
        }
    }
}

/// A small, UI-agnostic announcer the `generate_report` tool uses to hand a
/// freshly-written report file to the UI, which surfaces it as a card the user can
/// open. The tool stays UI-free: it just announces a path; the caller turns it into
/// an `AiReportReady` event. Both backends construct one from the
/// `events`/`session`/`conversation_id` they hold; a `disabled()` sink (no channel)
/// drops announcements (tests).
#[derive(Clone)]
pub(crate) struct ReportSink {
    events: Option<Events>,
    session: Option<SessionId>,
    conversation_id: ConversationId,
    /// The active app theme, so `generate_report` can paint the report in RED's
    /// colors. Captured when the sink is built (per turn on the API-key path; at
    /// conversation start on the subscription path).
    theme: Option<ReportTheme>,
    /// Where finished report files are written (Settings → AI agent → Report folder),
    /// captured alongside `theme`. `None` falls back to the system temp dir.
    report_dir: Option<PathBuf>,
}

impl ReportSink {
    pub(crate) fn new(
        events: Events,
        session: Option<SessionId>,
        conversation_id: ConversationId,
        theme: Option<ReportTheme>,
        report_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            events: Some(events),
            session,
            conversation_id,
            theme,
            report_dir,
        }
    }

    /// A no-op sink that drops announcements. For tests and any path with no UI
    /// (the headless `red mcp` transport, which withholds `generate_report`).
    pub(crate) fn disabled() -> Self {
        Self {
            events: None,
            session: None,
            conversation_id: ConversationId::new(0),
            theme: None,
            report_dir: None,
        }
    }

    /// The theme to paint the report with, if the UI supplied one.
    fn theme(&self) -> Option<&ReportTheme> {
        self.theme.as_ref()
    }

    /// The directory a finished report should be written to: the user's configured
    /// folder when set and usable (created on demand), else the system temp dir. A
    /// configured folder that can't be created falls back to temp rather than failing
    /// the report; the user still gets their report, just not where they asked.
    fn output_dir(&self) -> PathBuf {
        if let Some(dir) = &self.report_dir {
            match std::fs::create_dir_all(dir) {
                Ok(()) => return dir.clone(),
                Err(e) => tracing::warn!(
                    "AI report folder {} is unusable ({e}); writing to the temp dir instead",
                    dir.display()
                ),
            }
        }
        std::env::temp_dir()
    }

    /// Announce a freshly-written report so the UI surfaces it as a card.
    fn announce(&self, path: &Path, title: Option<&str>) {
        if let Some(events) = &self.events {
            emit(
                events,
                self.session,
                Event::AiReportReady {
                    conversation_id: self.conversation_id,
                    path: path.display().to_string(),
                    title: title.map(str::to_string),
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

    fn announce_save_query(&self, name: &str, description: Option<&str>, sql: &str) {
        if let Some(events) = &self.events {
            emit(
                events,
                self.session,
                Event::AiSaveQuery {
                    conversation_id: self.conversation_id,
                    name: name.to_string(),
                    description: description.map(str::to_string),
                    sql: sql.to_string(),
                },
            );
        }
    }
}

/// Safety backstop on the model → tool → model loop: how many tool round-trips a
/// single turn may take before we stop and report. Far above any real grounded
/// answer; prevents a misbehaving model from looping forever. The per-conversation
/// [`AiLimits::max_tool_calls`](red_core::AiLimits) bound sits on top of
/// this, spanning turns rather than resetting each one.
const MAX_TOOL_STEPS: usize = 16;

/// Per-conversation state shared between the dispatch loop and the spawned turn
/// tasks: the running message history (so follow-up turns keep context), the
/// in-flight cancel tokens (so `AiCancel` can stop a specific turn), and the
/// cumulative tool-call tally (so the resource-guard budget spans the whole
/// conversation, not just one turn).
#[derive(Default)]
pub(crate) struct AiState {
    histories: HashMap<ConversationId, Vec<Message>>,
    cancels: HashMap<ConversationId, CancelToken>,
    tool_calls: HashMap<ConversationId, usize>,
    /// Write-tool approval prompts awaiting the user's Allow/Deny, keyed
    /// by request id. The turn task parks a decision sink here; `AiPermission` takes
    /// it back out and fires it, the API-key analogue of the ACP path's
    /// `AcpManager.pending`.
    pending_perms: HashMap<RequestId, oneshot::Sender<bool>>,
    /// Monotonic counter for the request ids handed out by [`Self::park_permission`].
    /// Handed-out ids are offset by [`AI_REQUEST_BASE`] so they never collide with
    /// the ACP manager's (which counts up from 0); `AiPermission` can then resolve
    /// both sides unconditionally.
    next_request: u64,
}

/// Base offset for API-key permission request ids, keeping them disjoint from the
/// ACP manager's id space so a single `AiPermission` resolves exactly one prompt.
const AI_REQUEST_BASE: u64 = 1 << 48;

/// Cap on outstanding (un-answered) write-approval prompts on the API-key path;
/// past it, deny rather than grow the map. Mirrors the ACP manager's cap.
const MAX_PENDING_PERMS: usize = 32;

/// Cap on the report payload a `generate_report` call may embed (body HTML plus the
/// serialized charts/data/filters). The model assembles `data` from already-capped
/// query results, but nothing else bounds what it can echo, and the renderer
/// builds one DOM node per row with no virtualization, so an oversized payload makes
/// a multi-MB document that's slow (or hostile) to open in the browser. Past this we
/// refuse and tell the model to narrow the report rather than write the file.
const MAX_REPORT_BYTES: usize = 4 * 1024 * 1024;

impl AiState {
    /// Record an in-flight turn's cancel token so `AiCancel` can reach it.
    pub(crate) fn register(&mut self, conversation_id: ConversationId, token: CancelToken) {
        self.cancels.insert(conversation_id, token);
    }

    /// Park a write-approval decision sink and return the request id to surface, or
    /// `None` (deny) when too many are already outstanding.
    fn park_permission(&mut self, decide: oneshot::Sender<bool>) -> Option<RequestId> {
        if self.pending_perms.len() >= MAX_PENDING_PERMS {
            return None;
        }
        let id = RequestId::new(AI_REQUEST_BASE + self.next_request);
        self.next_request += 1;
        self.pending_perms.insert(id, decide);
        Some(id)
    }

    /// Answer a parked write-approval prompt (the panel's Allow/Deny). A stale id
    /// (already resolved, or owned by the ACP path) is a no-op. Also used to forget a
    /// prompt abandoned on cancel (`allow` is irrelevant then; the receiver is gone).
    pub(crate) fn resolve_permission(&mut self, request_id: RequestId, allow: bool) {
        if let Some(decide) = self.pending_perms.remove(&request_id) {
            let _ = decide.send(allow);
        }
    }

    /// Flip the cancel token for an in-flight turn, if any (the panel's Stop).
    pub(crate) fn cancel(&self, conversation_id: ConversationId) {
        if let Some(tok) = self.cancels.get(&conversation_id) {
            tok.cancel();
        }
    }

    /// Drop all per-conversation state (history, cancel token, cumulative tool tally)
    /// when the UI closes/deletes the conversation, so these maps stay bounded by
    /// what's open rather than every conversation ever touched this session. Cancels
    /// any in-flight turn first so its task winds down. (A turn still racing to its
    /// final history write can re-insert one entry; that's bounded, unlike the prior
    /// unconditional growth.)
    pub(crate) fn forget(&mut self, conversation_id: ConversationId) {
        if let Some(tok) = self.cancels.remove(&conversation_id) {
            tok.cancel();
        }
        self.histories.remove(&conversation_id);
        self.tool_calls.remove(&conversation_id);
    }

    /// Charge one tool call against the conversation's cumulative budget. Returns
    /// `false` once the budget (`max`, `0` = unlimited) is exhausted, so the loop
    /// can stop a runaway agent instead of letting it spin tools forever.
    fn charge_tool_call(&mut self, conversation_id: ConversationId, max: usize) -> bool {
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
    backend: AiBackend,
    events: Events,
    state: Arc<Mutex<AiState>>,
    session: Option<SessionId>,
    conversation_id: ConversationId,
    model: String,
    show_thinking: bool,
    policy: AiPolicy,
    user_message: String,
    context: AiContext,
    cancel: CancelToken,
) {
    let system = backend.system_prompt(&context, &policy);
    // The tier decides which tools the model is even offered: `off` grounds
    // nothing, `schema` withholds row data, `read` is the full catalog. The KV
    // (Redis) backend offers its own read-only `kv_*` catalog.
    let tools = backend.catalog(&policy);
    // Where `generate_report` delivers its file so the UI opens it (Feature C);
    // carries the active theme so the report matches RED's colors.
    let report = ReportSink::new(
        events.clone(),
        session,
        conversation_id,
        context.theme.as_deref().cloned(),
        context.report_dir.clone(),
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
            model: &model,
            max_tokens: 8192,
            show_thinking,
            system: &system,
            tools: &tools,
            messages: &messages,
        };

        // Relay the provider's deltas to the UI as they stream in.
        let (dtx, mut drx) = tokio::sync::mpsc::unbounded_channel::<red_ai::Delta>();
        let relay = {
            let events = events.clone();
            tokio::spawn(async move {
                while let Some(d) = drx.recv().await {
                    // Tool calls become activity nodes at the execution site below,
                    // where the arguments are known; the streamed `ToolUseStarted`
                    // is only an early hint, so it is dropped here.
                    let delta = match d {
                        red_ai::Delta::Thinking(t) => AiDelta::Thinking(t),
                        red_ai::Delta::Text(t) => AiDelta::Text(t),
                        red_ai::Delta::ToolUseStarted { .. } => continue,
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

        // `MaxTokens` means the answer was cut off mid-sentence at the ceiling.
        // Settling it with the ordinary "finished" footer makes a truncated turn
        // indistinguishable from a complete one — the same dishonesty as swallowing
        // a mid-stream provider error. The text already streamed stays on screen;
        // this only labels it for what it is.
        if outcome.stop_reason == StopReason::MaxTokens {
            result = Err(
                "the reply hit this agent's max-tokens ceiling and stops mid-answer; \
                 raise the limit or ask for a shorter answer"
                    .to_string(),
            );
            break;
        }
        if outcome.stop_reason != StopReason::ToolUse {
            break;
        }

        // Run every requested tool and feed one result block back per call.
        let mut results = Vec::new();
        for block in &outcome.message.content {
            let ContentBlock::ToolUse { id, name, input } = block else {
                continue;
            };
            // Charge the conversation's cumulative tool-call budget. When it
            // is exhausted, hand the model an error result instead of running the
            // tool: it can wrap up its answer, but it can't keep looping.
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
            // Delegation (Phase 1c): run a bounded, read-only child agent and feed
            // its report back as this call's result. Intercepted before the write
            // gate / `run_tool` because it drives a nested turn, not a driver call.
            // The child's own tool calls stream in as children of this node, so the
            // delegation is visible in the timeline rather than opaque.
            if name == "spawn_subagent" {
                let task = input
                    .get("task")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if task.is_empty() {
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: "error: spawn_subagent requires a non-empty `task`".into(),
                        is_error: true,
                    });
                    continue;
                }
                emit(
                    &events,
                    session,
                    Event::AiDelta {
                        conversation_id,
                        delta: AiDelta::ActivityStarted {
                            id: id.clone().into(),
                            parent: None,
                            kind: ActivityKind::Subagent {
                                task: truncate_summary(&task, 120),
                            },
                            status: ActivityStatus::Running,
                        },
                    },
                );
                // Delegation runs the parent's backend (SQL or KV), narrowed to a
                // read-only, non-recursive subset (see the subagent catalogs).
                let (content, ok) = run_subagent(
                    &provider,
                    &backend,
                    &events,
                    &state,
                    session,
                    conversation_id,
                    &model,
                    &policy,
                    &report,
                    id,
                    &task,
                    &cancel,
                )
                .await;
                emit(
                    &events,
                    session,
                    Event::AiDelta {
                        conversation_id,
                        delta: AiDelta::ActivityUpdated {
                            id: id.clone().into(),
                            status: Some(if ok {
                                ActivityStatus::Ok
                            } else {
                                ActivityStatus::Failed
                            }),
                            detail: activity_detail(name, ok, &content),
                        },
                    },
                );
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content,
                    is_error: !ok,
                });
                continue;
            }
            // Gate a mutating tool behind explicit per-call user approval (Feature
            // B). A blocked shape (wrong tier, read-only, DDL, unqualified
            // UPDATE/DELETE) is reported to the model without ever prompting; an
            // allowed shape surfaces the exact SQL as an Allow/Deny prompt and runs
            // only on Allow. A read tool falls straight through.
            match assess_write(name, input, &policy, backend.dialect()) {
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
                        // Record the denied write in the timeline as a terminal node
                        // so the audit trail shows what was proposed and refused.
                        emit(
                            &events,
                            session,
                            Event::AiDelta {
                                conversation_id,
                                delta: AiDelta::ActivityStarted {
                                    id: id.clone().into(),
                                    parent: None,
                                    kind: ActivityKind::Write { sql: sql.clone() },
                                    status: ActivityStatus::Denied,
                                },
                            },
                        );
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: "the user denied this write. Do not retry it; explain it or \
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
                    delta: AiDelta::ActivityStarted {
                        id: id.clone().into(),
                        parent: None,
                        kind: ActivityKind::Tool {
                            name: name.clone(),
                            args_summary: summarize_tool_args(name, input),
                        },
                        status: ActivityStatus::Running,
                    },
                },
            );
            let (content, ok) = backend
                .run_tool(name, input, &policy, &cancel, &report)
                .await;
            emit(
                &events,
                session,
                Event::AiDelta {
                    conversation_id,
                    delta: AiDelta::ActivityUpdated {
                        id: id.clone().into(),
                        status: Some(if ok {
                            ActivityStatus::Ok
                        } else {
                            ActivityStatus::Failed
                        }),
                        detail: activity_detail(name, ok, &content),
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
            // Model claimed tool_use but emitted no tool block; bail rather than spin.
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

/// How many model→tool rounds a delegated subagent may take before it must report
/// back. Deliberately smaller than the parent's [`MAX_TOOL_STEPS`]: a subagent is a
/// focused, bounded errand, and the shared tool-call budget caps it further.
const SUBAGENT_MAX_STEPS: usize = 6;

/// Run a bounded, read-only subagent turn for `spawn_subagent` (Phase 1c). The
/// child gets the parent's tools minus writes and minus `spawn_subagent` (so it can
/// neither mutate nor recurse), **shares** the conversation's tool-call budget (so
/// it can't blow the parent's cap), and streams its own tool calls into the
/// timeline as children of `parent_id`. Its prose is not shown; only its final
/// report text returns to the parent as the tool result.
#[allow(clippy::too_many_arguments)]
async fn run_subagent(
    provider: &Arc<dyn AiProvider>,
    backend: &AiBackend,
    events: &Events,
    state: &Arc<Mutex<AiState>>,
    session: Option<SessionId>,
    conversation_id: ConversationId,
    model: &str,
    policy: &AiPolicy,
    report: &ReportSink,
    parent_id: &str,
    task: &str,
    cancel: &CancelToken,
) -> (String, bool) {
    // The child runs the parent's backend, narrowed to reads (see the catalogs).
    let (tools, system) = match backend {
        AiBackend::Sql { .. } => (subagent_catalog(policy), subagent_system_prompt(task)),
        AiBackend::Kv(_) => (kv_subagent_catalog(policy), kv_subagent_system_prompt(task)),
        AiBackend::Doc(_) => (
            doc_subagent_catalog(policy),
            doc_subagent_system_prompt(task),
        ),
    };
    let mut messages = vec![Message::user_text(task.to_string())];
    let mut answer = String::new();

    for _ in 0..SUBAGENT_MAX_STEPS {
        if cancel.is_cancelled() {
            return ("the subagent was cancelled".into(), false);
        }
        let req = TurnRequest {
            model,
            max_tokens: 4096,
            show_thinking: false,
            system: &system,
            tools: &tools,
            messages: &messages,
        };
        // Drain the child's streamed deltas without surfacing its prose; only its
        // tool activity is shown, emitted below as children of the parent node.
        let (dtx, mut drx) = tokio::sync::mpsc::unbounded_channel::<red_ai::Delta>();
        let drain = tokio::spawn(async move { while drx.recv().await.is_some() {} });
        let outcome = provider.stream_turn(&req, &dtx, cancel).await;
        drop(dtx);
        let _ = drain.await;

        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => return (format!("the subagent failed: {e}"), false),
        };
        messages.push(outcome.message.clone());
        for block in &outcome.message.content {
            if let ContentBlock::Text { text } = block {
                answer.push_str(text);
            }
        }
        if outcome.stop_reason != StopReason::ToolUse {
            break;
        }

        let mut results = Vec::new();
        for block in &outcome.message.content {
            let ContentBlock::ToolUse { id, name, input } = block else {
                continue;
            };
            if !lock(state).charge_tool_call(conversation_id, policy.limits.max_tool_calls) {
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: "error: the shared tool-call budget is exhausted; stop and report \
                        what you have"
                        .into(),
                    is_error: true,
                });
                continue;
            }
            emit(
                events,
                session,
                Event::AiDelta {
                    conversation_id,
                    delta: AiDelta::ActivityStarted {
                        id: id.clone().into(),
                        parent: Some(parent_id.to_string().into()),
                        kind: ActivityKind::Tool {
                            name: name.clone(),
                            args_summary: summarize_tool_args(name, input),
                        },
                        status: ActivityStatus::Running,
                    },
                },
            );
            // A subagent is read-only by contract (its catalog excludes writes).
            // Enforce that at the *execution* seam too: even if the model emits a
            // write tool by name that its catalog never advertised, refuse it here
            // rather than dispatching into a mutating arm. Without this, a KV
            // subagent could reach kv_delete/kv_config_set and bypass the per-call
            // Allow/Deny gate (and the keyspace-wide refusal), which only run_turn
            // applies.
            let (content, ok) = if is_write_tool(name) {
                (
                    format!(
                        "error: `{name}` is a write tool; a subagent is read-only and cannot \
                         mutate data"
                    ),
                    false,
                )
            } else {
                backend.run_tool(name, input, policy, cancel, report).await
            };
            emit(
                events,
                session,
                Event::AiDelta {
                    conversation_id,
                    delta: AiDelta::ActivityUpdated {
                        id: id.clone().into(),
                        status: Some(if ok {
                            ActivityStatus::Ok
                        } else {
                            ActivityStatus::Failed
                        }),
                        detail: activity_detail(name, ok, &content),
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
            break;
        }
        messages.push(Message {
            role: Role::User,
            content: results,
        });
    }

    let answer = answer.trim();
    if answer.is_empty() {
        (
            "the subagent finished without producing a report".into(),
            true,
        )
    } else {
        (answer.to_string(), true)
    }
}

/// Apply the two membership gates every seam's catalog shares: the tier decides
/// which tools exist at all, and any write tool is additionally withheld on a
/// read-only connection so it's never even offered there. One helper
/// so the SQL, KV, and doc catalogs gate identically.
fn gate_catalog(all: impl IntoIterator<Item = ToolDef>, policy: &AiPolicy) -> Vec<ToolDef> {
    all.into_iter()
        .filter(|t| {
            policy.tier.allows_tool(&t.name) && !(policy.read_only && is_write_tool(&t.name))
        })
        .collect()
}

/// Narrow a parent catalog to what a delegated subagent may use: minus every
/// write tool and minus `spawn_subagent` itself, so a child can neither mutate
/// data nor recurse. Narrows (never widens) the parent's tier — even a Write-tier
/// parent yields a read-only child. The security-critical "read-only,
/// non-recursive child" rule, in one place for all three seams.
fn narrow_to_subagent(catalog: Vec<ToolDef>) -> Vec<ToolDef> {
    catalog
        .into_iter()
        .filter(|t| t.name != "spawn_subagent" && !is_write_tool(&t.name))
        .collect()
}

/// The tool subset a delegated SQL subagent may use (see [`narrow_to_subagent`]).
fn subagent_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
    narrow_to_subagent(tool_catalog(policy))
}

/// The subagent's system prompt: a focused, read-only worker that reports back.
fn subagent_system_prompt(task: &str) -> String {
    format!(
        "You are a focused sub-investigator working for a parent AI agent on ONE task. You have \
         read-only database tools (schema inspection and capped SELECTs); you cannot write data \
         or delegate further. Do the task, then reply with a concise report of your findings — \
         the key facts, figures, and any caveats — that the parent can use directly. Do not ask \
         questions; you cannot receive answers.\n\nTask: {task}"
    )
}

/// The Redis subagent's tool subset: the parent's KV catalog, narrowed like
/// [`subagent_catalog`].
fn kv_subagent_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
    narrow_to_subagent(kv_tool_catalog(policy))
}

/// The Redis subagent's system prompt (the KV analogue of [`subagent_system_prompt`]).
fn kv_subagent_system_prompt(task: &str) -> String {
    format!(
        "You are a focused sub-investigator working for a parent AI agent on ONE task against a \
         Redis server. You have read-only Redis tools (kv_server_info, kv_scan_keys, kv_key_info, \
         kv_get_value, kv_biggest_keys, kv_analyze, kv_slowlog, kv_config_get); you cannot write \
         or delegate further. Keys use glob patterns, not SQL. Do the task, then reply with a \
         concise report of your findings the parent can use directly. Do not ask questions; you \
         cannot receive answers.\n\nTask: {task}"
    )
}

/// Surface a write-approval prompt and block this turn until the user answers it,
/// the API-key path's analogue of the ACP permission flow. Parks a
/// decision sink in [`AiState`], emits an `AiPermissionRequest` carrying the exact
/// SQL, then awaits the answer while polling the turn's cancel token (a cancelled
/// turn, or too many outstanding prompts, denies). Returns whether to run the write.
async fn await_write_approval(
    state: &Arc<Mutex<AiState>>,
    events: &Events,
    session: Option<SessionId>,
    conversation_id: ConversationId,
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

/// The read-only tool catalog, filtered to the policy's access tier. Each
/// tool is backed by a `DatabaseDriver` method and auto-runs; none can mutate.
/// Filtering happens *here*, at construction, so a tool above the tier is never
/// offered; the model can't call what isn't in the catalog. Shared with the MCP
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
            name: "object_ddl".into(),
            description: "The object's REAL definition, as SQL. describe_table gives columns, keys \
                and indexes but silently drops check constraints, defaults, generated-column \
                expressions, view bodies and trigger source. Call this when the question is \"why \
                does this insert fail\", \"what does this view actually do\", or \"what does this \
                trigger/function contain\" — the DDL is the answer. Nothing is executed."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Schema/namespace name, as reported by list_schema." },
                    "name": { "type": "string", "description": "The object's name." },
                    "kind": {
                        "type": "string",
                        "enum": ["table", "view", "matview", "function", "procedure", "trigger", "sequence", "type"],
                        "description": "The object kind (default \"table\").",
                    },
                },
                "required": ["schema", "name"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "relationship_map".into(),
            description: "The database's foreign-key graph in ONE call: every declared FK edge as \
                `child.column -> parent.column`, plus the tables nothing references and that \
                reference nothing. CALL THIS BEFORE WRITING ANY QUERY THAT JOINS MORE THAN ONE \
                TABLE — it is the verified join graph, so you never have to guess a join key from \
                a column name. Omit both arguments for the whole database."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Restrict to one schema/namespace; omit for all." },
                    "tables": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Restrict to edges touching these tables (either side); omit for all.",
                    },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "profile_table".into(),
            description: "Profile one table's data: per-column null counts and ratios, distinct \
                counts (with unique-key and constant-column hints), and min/max (plus sum/avg for \
                numeric columns), followed by its foreign-key relationships (outgoing and \
                incoming). One pushed-down aggregate pass per column — it never returns raw rows — \
                so use it to understand a table's shape and data quality before querying, instead \
                of hand-writing count/distinct/min/max SELECTs."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Schema/namespace name (e.g. \"main\" or \"public\"); as reported by list_schema." },
                    "table": { "type": "string", "description": "The table to profile." },
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
                subject to a statement timeout; use LIMIT and targeted columns. This is the only \
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
            name: "search_data".into(),
            description: format!(
                "Find rows anywhere in a table containing `term`, without writing a WHERE clause: \
                it builds a case-insensitive contains-match across every searchable column and \
                returns up to {max_rows} matching rows. Use it for \"where is this value\", \
                \"which row mentions X\", or when you know a value but not which column holds it. \
                Binary/blob columns are skipped."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Schema/namespace name, as reported by list_schema." },
                    "table": { "type": "string", "description": "The table to search." },
                    "term": { "type": "string", "description": "The text to look for (matched case-insensitively as a substring)." },
                    "limit": {
                        "type": "integer",
                        "description": format!("Max rows to return (1..{max_rows})."),
                    },
                },
                "required": ["schema", "table", "term"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "explain".into(),
            description: "Return the query planner's EXPLAIN output for a SQL statement. By \
                default it only PLANS (nothing executes). Pass `analyze: true` to run the \
                statement and get actual row counts and timings beside the estimates — that \
                comparison is what makes plan reasoning real, and it is the way to prove a bad \
                cardinality estimate. Because EXPLAIN ANALYZE executes, it is allowed for \
                read-only statements ONLY; anything that could write is refused outright."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "The SQL to explain." },
                    "analyze": {
                        "type": "boolean",
                        "description": "Run the statement to collect actuals (read-only statements only). Default false.",
                    },
                },
                "required": ["sql"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "health_report".into(),
            description: "A bounded health snapshot of the connection: total and per-table sizes \
                (largest first), plus the findings the engine's catalog supports — unused and \
                redundant indexes, foreign keys with no index on the child side, tables with no \
                primary key, dead tuples/bloat, sequential-scan-heavy tables. It also lists the \
                checks that could NOT run here, so \"no findings\" is never mistaken for a clean \
                bill of health. Every query inside is a bounded catalog read, not a scan. Pair \
                with server_sessions for \"why is this database slow\": this one answers the \
                structural half."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Restrict the report to one schema/namespace; omit for the whole connection." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "server_sessions".into(),
            description: "What the server is doing RIGHT NOW: the live sessions longest-running \
                first, with their user, database, state, wait, elapsed time, running statement, \
                and which sessions block which. This is the \"why is it slow right now\" half \
                that health_report cannot answer — a blocked-on-lock wait tree looks nothing \
                like a missing index."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "diff_schema".into(),
            description: "Compare the STRUCTURE of two schemas in this connection: which objects \
                exist on one side only, and per shared table which columns/indexes/foreign keys \
                were added, removed, or changed. Use it for \"what is different between staging \
                and production\" when both live here. Nothing is executed; the differences come \
                back as text."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "left": { "type": "string", "description": "The baseline schema/namespace." },
                    "right": { "type": "string", "description": "The schema/namespace to compare against it." },
                    "tables": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Restrict the comparison to these tables; omit for all.",
                    },
                },
                "required": ["left", "right"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "diff_data".into(),
            description: "Compare the ROWS of two tables in this connection, aligned on a key \
                column: which keys are only on one side, and which shared keys have differing \
                values (and in which columns). Both tables are read key-ordered and merge-walked, \
                so nothing is materialized. Use it for \"did the copy land\", \"what drifted\", \
                \"which rows differ\"."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "left_schema": { "type": "string", "description": "Schema of the baseline table." },
                    "left_table": { "type": "string", "description": "The baseline table." },
                    "right_schema": { "type": "string", "description": "Schema of the table to compare against it." },
                    "right_table": { "type": "string", "description": "The table to compare against it." },
                    "key": { "type": "string", "description": "The column to align on; omit to use the baseline's single-column primary key." },
                },
                "required": ["left_table", "right_table"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "suggest_index".into(),
            description: "Given a query, decide whether an index would help and emit the CREATE \
                INDEX statement to consider — as TEXT, for the user to read. It explains the \
                query, and if the plan scans, reads the table's existing indexes and columns so \
                the suggestion does not duplicate one that already exists. It does NOT create \
                anything; create_index does that, behind approval."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "The slow SELECT to advise on." },
                    "schema": { "type": "string", "description": "Schema of the table the query filters (for the existing-index check)." },
                    "table": { "type": "string", "description": "The table the query filters." },
                },
                "required": ["sql", "table"],
                "additionalProperties": false,
            }),
        },
        export_tool_def(
            "Stream a read-only query's WHOLE result to a file for the user (CSV, JSON, SQL \
             INSERTs, or a standalone HTML table) and hand it over as a card in the chat they can \
             open. Unlike run_select this is not row-capped — the rows go to a file, not to you — \
             so use it when the user asks for an export/download/dump rather than an answer. Only \
             SELECT/WITH queries are accepted.",
            json!({
                "sql": { "type": "string", "description": "A single SELECT/WITH query whose full result is written." },
                "format": {
                    "type": "string",
                    "enum": ["csv", "json", "sql", "html"],
                    "description": "Output format (default \"csv\").",
                },
                "name": { "type": "string", "description": "A short name for the file, e.g. \"monthly-revenue\"." },
            }),
            &["sql"],
        ),
        report_tool_def(),
        ToolDef {
            name: "open_query".into(),
            description: "Open a SQL query in a new editor tab in the user's workspace so they have \
                it in the grid. A read-only SELECT runs automatically; anything else is just loaded \
                for the user to run themselves. Use this to hand the user a query to explore or \
                build on; it does NOT return rows to you (use run_select for that)."
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
            name: "save_query".into(),
            description: "Save a REUSABLE SQL query to the user's saved-queries library under a \
                short name, so they can reopen and rerun it later (⇧⌘O). Use this when the user \
                asks for a report/query they'll want again — e.g. \"monthly revenue\" — rather \
                than open_query (which is a one-off tab). For a parametrized query, leave named \
                `:placeholders` in the SQL (e.g. `WHERE month = :month`) and explain them in the \
                description; the user fills them in when they run it. Nothing executes."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "A short, human-readable name (e.g. \"Monthly revenue\")." },
                    "sql": { "type": "string", "description": "The SQL to save, runnable as-is (named :placeholders allowed for parameters)." },
                    "description": { "type": "string", "description": "One line on what it does and any placeholders to fill in; shown in the picker." },
                },
                "required": ["name", "sql"],
                "additionalProperties": false,
            }),
        },
        spawn_subagent_tool_def(),
        ToolDef {
            name: "create_index".into(),
            description: "Create an index, behind the user's explicit approval. This is the one \
                DDL the agent may run: an index is ADDITIVE and reversible, unlike \
                DROP/TRUNCATE/ALTER, which stay blocked. Read suggest_index and describe_table \
                first — building an index on a large table locks and loads the server, so say how \
                big the table is when you propose it."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Schema/namespace of the table." },
                    "table": { "type": "string", "description": "The table to index." },
                    "name": { "type": "string", "description": "The index name." },
                    "columns": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "The columns to index, in order.",
                        "minItems": 1,
                    },
                    "unique": { "type": "boolean", "description": "Create a UNIQUE index. Default false." },
                },
                "required": ["table", "name", "columns"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kill_session".into(),
            description: "Stop a running server session: `cancel` stops its current statement and \
                keeps the session, `terminate` drops the whole session and ROLLS BACK its open \
                transaction. Call server_sessions first to get the `key`, and copy that session's \
                `user` and `statement` into this call so the user can see what they are stopping — \
                the target is re-checked against the live server before anything happens, and the \
                kill is refused if the session has been recycled meanwhile. Requires the user's \
                explicit approval."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The session key from server_sessions." },
                    "mode": {
                        "type": "string",
                        "enum": ["cancel", "terminate"],
                        "description": "\"cancel\" stops the statement; \"terminate\" drops the session (rolls back its transaction). Default \"cancel\".",
                    },
                    "user": { "type": "string", "description": "The session's user, copied from server_sessions; verified before the kill." },
                    "statement": { "type": "string", "description": "The session's running statement, copied from server_sessions, so the approval shows what is being stopped." },
                },
                "required": ["key"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "propose_write".into(),
            description: "Execute a SINGLE data-modifying statement: INSERT, UPDATE, or DELETE. \
                EVERY call requires explicit per-statement approval: the user sees the exact SQL \
                and must Allow it before it runs; assume it may be denied. UPDATE and DELETE MUST \
                include a WHERE clause. DDL (DROP/TRUNCATE/ALTER/CREATE) and any multi-statement \
                input are rejected; tell the user to run those by hand. Use this only when the \
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
        ToolDef {
            name: "propose_changeset".into(),
            description: "Execute SEVERAL data-modifying statements as ONE approved unit, in \
                order. On an engine with multi-statement transactions they commit together, or if \
                any fails the whole set is rolled back (nothing changes); ClickHouse has no such \
                transaction, so there a failure leaves the statements before it applied. Use this \
                for a related multi-step change — e.g. insert a parent row then \
                its children, or update several rows in lockstep — where a half-applied result \
                would be wrong. EVERY call requires explicit approval: the user sees the full list \
                of statements and must Allow it before anything runs; assume it may be denied. Each \
                statement must be a single INSERT/UPDATE/DELETE (UPDATE/DELETE need a WHERE); DDL \
                and chained statements are rejected — tell the user to run those by hand. For a \
                single change use propose_write instead."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "statements": {
                        "type": "array",
                        "description": "The INSERT/UPDATE/DELETE statements to run in order, as one unit. Each is a single statement (UPDATE/DELETE need a WHERE).",
                        "items": { "type": "string" },
                        "minItems": 1,
                    },
                    "description": { "type": "string", "description": "One line on what this changeset does, shown to the user with the approval prompt." },
                },
                "required": ["statements"],
                "additionalProperties": false,
            }),
        },
    ];
    gate_catalog(all, policy)
}

/// A one-line summary of a tool call's arguments for the activity timeline: the
/// SQL's first line for query/write tools, the table name for `describe_table`.
/// Kept short so the trace reads without expanding a node. `None` when there's no
/// salient argument.
fn summarize_tool_args(name: &str, input: &Json) -> Option<String> {
    if name == "propose_changeset" {
        let n = input
            .get("statements")
            .and_then(Json::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        return Some(format!("{n} statement{}", if n == 1 { "" } else { "s" }));
    }
    let salient = match name {
        "run_select" | "explain" | "propose_write" => input.get("sql")?.as_str()?,
        "describe_table" | "profile_table" => input.get("table").and_then(Json::as_str)?,
        "save_query" => input.get("name").and_then(Json::as_str)?,
        "kv_set" | "kv_key_info" | "kv_get_value" => input.get("key").and_then(Json::as_str)?,
        _ => return None,
    };
    let line = salient.split('\n').find(|l| !l.trim().is_empty())?.trim();
    Some(truncate_summary(line, 80))
}

/// A one-line result summary for a finished tool node: on failure, the error's
/// first line; on success, a short per-tool signal (row count, rows affected) so the
/// trace reads at a glance. `None` when there's nothing concise to show.
fn activity_detail(name: &str, ok: bool, content: &str) -> Option<String> {
    if !ok {
        let line = content.split('\n').find(|l| !l.trim().is_empty())?.trim();
        return Some(truncate_summary(line, 120));
    }
    let summary = match name {
        // The write tools return a single summary sentence; surface it verbatim.
        "propose_write" | "propose_changeset" => content.split('\n').next()?.trim().to_string(),
        // `format_page` ends with a `(N rows)` line; skip the `(truncated …)` note.
        "run_select" => content
            .lines()
            .rev()
            .find(|l| {
                let t = l.trim_start();
                t.starts_with('(') && t[1..].chars().next().is_some_and(|c| c.is_ascii_digit())
            })
            .map(|l| l.trim().trim_matches(['(', ')']).to_string())?,
        // `profile_table`'s report opens with `Profile of X — N rows`.
        "profile_table" => content
            .lines()
            .next()
            .and_then(|l| l.split('—').nth(1))
            .map(|s| s.trim().to_string())?,
        _ => return None,
    };
    (!summary.is_empty()).then(|| truncate_summary(&summary, 120))
}

/// Truncate to `max` chars on a char boundary, appending an ellipsis when cut.
fn truncate_summary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Execute one tool call against the driver, under the access policy.
/// Returns `(content, ok)`; `ok = false` becomes an `is_error` tool result the
/// model can recover from. Shared with the MCP server so the API-key and
/// subscription paths run identical, guarded tools.
///
/// Two layers of guard apply here, both server-side so neither backend can slip
/// past them: the tier is re-checked (defense in depth; the catalog already
/// withholds out-of-tier tools, but a misbehaving agent could still *name* one),
/// and the [`AiLimits`](red_core::AiLimits) clamp rows, time-box the query, and
/// cap the result bytes handed back to the model.
/// The `spawn_subagent` tool definition, shared by the SQL and KV catalogs
/// (delegation is engine-agnostic — the child runs the parent's own read tools).
fn spawn_subagent_tool_def() -> ToolDef {
    ToolDef {
        name: "spawn_subagent".into(),
        description: "Delegate a self-contained READ-ONLY sub-investigation to a subagent and get \
            back its findings as a short written report. The subagent has your read-only tools \
            (it cannot write or spawn further subagents) and works in its own context, so use \
            this to parallelize or offload a focused chunk of work without cluttering your own \
            context. Give it ONE clear, bounded task and everything it needs to know; it cannot \
            ask you follow-ups. It returns only its final summary, not raw data."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "A single, self-contained read-only task for the subagent, with all needed context." },
            },
            "required": ["task"],
            "additionalProperties": false,
        }),
    }
}

/// The `export_result` tool definition. One name across all three seams, since
/// "write this out to a file for me" is the same request everywhere, but each
/// passes its own `description` and arguments: SQL exports a query, Redis a set
/// of keys, MongoDB a collection, and pretending those take the same parameters
/// would produce a schema nobody could call.
fn export_tool_def(description: &str, properties: Json, required: &[&str]) -> ToolDef {
    ToolDef {
        name: "export_result".into(),
        description: description.into(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    }
}

/// The `generate_report` tool definition, shared by the SQL and KV catalogs (the
/// report pipeline is engine-agnostic — the model authors HTML from whatever it
/// read).
fn report_tool_def() -> ToolDef {
    ToolDef {
        name: "generate_report".into(),
        description: "Write a custom HTML report for the user. It appears as a card in the \
            chat with an \"Open\" button; the user opens it in their browser when they choose \
            (it is NOT opened automatically). \
            YOU author the report: first read the data (with the read tools), then call this with \
            `html` set to the report's body: headings, prose/summary, one or more <table>s, \
            even an inline <svg> chart. Use semantic HTML and inline `style=\"…\"` for any \
            styling; a base stylesheet (light/dark) is already applied. Scripts and remote/\
            external resources (other domains, <script>, remote <img>/CSS) are stripped or \
            blocked for safety, so keep everything self-contained (data URIs for images). \
            For INTERACTIVE charts (hover tooltips, legends), pass `charts` (an array of \
            Chart.js v4 config objects) and reference each one from the body with an empty \
            <div data-red-chart=\"INDEX\"></div> placeholder (INDEX is the chart's position \
            in the array). The charts are rendered by a trusted built-in Chart.js; you supply \
            DATA only (no JavaScript/function callbacks; they are ignored). \
            For INTERACTIVE TABLES the user can search/sort/filter, pass `data` (named \
            datasets of {columns, rows}) and drop a <div data-red-table=\"NAME\"></div> \
            placeholder; the user gets a live filter box, click-to-sort headers, and per-column \
            filters. A chart can BIND to a dataset instead of carrying inline data: give it \
            {\"dataset\":\"NAME\",\"type\":\"bar\",\"x\":\"colName\",\"y\":[\"colA\"]}, and it \
            re-draws automatically when the user filters that dataset's table. \
            For DASHBOARD-style controls (like Grafana variables) that drive EVERY table and \
            bound chart at once, pass `filters`, e.g. a multi-select to show only chosen \
            regions: {\"column\":\"Region\",\"type\":\"multiselect\"}. They render as a control \
            bar at the top of the report. Prefer this (data + bound charts + a table + \
            filters) when the user wants to explore/slice the data; prefer inline-data charts \
            for a fixed visual. \
            Use this when the user asks for a report."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "html": { "type": "string", "description": "The report BODY as self-contained HTML (no <html>/<head>/<body> wrapper; that's added). Reference charts with <div data-red-chart=\"INDEX\"></div> and interactive tables with <div data-red-table=\"NAME\"></div> placeholders." },
                "title": { "type": "string", "description": "Report title (browser tab + heading)." },
                "charts": {
                    "type": "array",
                    "description": "Optional interactive charts. Each item is EITHER a full Chart.js v4 config with inline data, e.g. {\"type\":\"bar\",\"data\":{\"labels\":[…],\"datasets\":[{\"label\":\"Revenue\",\"data\":[…]}]},\"options\":{…}}, OR a dataset binding {\"dataset\":\"NAME\",\"type\":\"bar\",\"x\":\"colName\",\"y\":[\"col1\",\"col2\"],\"aggregate\":\"sum\",\"options\":{…}} that derives its data from a named `data` dataset and follows that table's filters. type is one of bar, line, pie, doughnut, radar, polarArea, scatter, bubble. aggregate (sum/avg/min/max/count/none, default none) groups rows sharing an x value. Data only; no functions/callbacks. Place a <div data-red-chart=\"INDEX\"></div> in the body for each.",
                    "items": { "type": "object" },
                },
                "data": {
                    "type": "object",
                    "description": "Optional named datasets for interactive tables and filter-linked charts, e.g. {\"sales\":{\"columns\":[\"Month\",\"Region\",\"Revenue\"],\"rows\":[[\"Jan\",\"NA\",120],[\"Feb\",\"EU\",90]]}}. Each value is {columns:[string], rows:[[cell,…]]} (cells are strings/numbers/null). Reference a dataset with <div data-red-table=\"sales\"></div> for a searchable/sortable table, and/or bind charts to it via {\"dataset\":\"sales\",…}.",
                    "additionalProperties": { "type": "object" },
                },
                "filters": {
                    "type": "array",
                    "description": "Optional report-wide filter controls (Grafana-style variables) that filter EVERY table and bound chart. Each is {\"column\":\"Region\",\"type\":\"multiselect\",\"label\":\"Region\",\"dataset\":\"sales\",\"default\":[…]}. type: multiselect (checkbox dropdown: pick which values to show; this is the 'show only selected regions' control), select (single value), range (numeric min/max), or search (substring). column must exist in the dataset(s); omit `dataset` to apply to all datasets that have that column. `default` pre-selects values (multiselect/select). They appear in a bar at the top; no body placeholder needed (optionally place <div data-red-filters></div> to position it).",
                    "items": { "type": "object" },
                },
            },
            "required": ["html"],
            "additionalProperties": false,
        }),
    }
}

/// The `generate_report` tool: wrap the model-authored HTML (+ optional
/// charts/data/filters) in a sandboxed, themed shell, size-check it, write it to
/// the report dir, and announce it as a chat card. Engine-agnostic — the report
/// pipeline is identical for SQL and Redis — so both `run_tool` and `kv_run_tool`
/// call it.
fn run_generate_report(input: &Json, report: &ReportSink) -> (String, bool) {
    let body = input
        .get("html")
        .and_then(Json::as_str)
        .unwrap_or("")
        .trim();
    if body.is_empty() {
        return (
            "error: generate_report needs `html` (the report body you authored)".into(),
            false,
        );
    }
    let title = input.get("title").and_then(Json::as_str);
    // Optional interactive charts: keep only well-formed Chart.js spec objects.
    // They are embedded as inert data and rendered by the trusted bundle (see
    // `wrap_report_html`); anything that isn't an object is dropped rather than
    // smuggled into the document.
    let charts: Vec<Json> = input
        .get("charts")
        .and_then(Json::as_array)
        .map(|items| items.iter().filter(|c| c.is_object()).cloned().collect())
        .unwrap_or_default();
    // Optional named datasets for interactive (filterable/sortable) tables and
    // filter-linked charts. Kept only if it's an object map.
    let data = input.get("data").filter(|v| v.is_object());
    // Optional report-wide filter controls (Grafana-style variables). Objects only.
    let filters: Vec<Json> = input
        .get("filters")
        .and_then(Json::as_array)
        .map(|items| items.iter().filter(|c| c.is_object()).cloned().collect())
        .unwrap_or_default();
    let html = wrap_report_html(title, body, &charts, data, &filters, report.theme());
    // Refuse an oversized report by measuring the FINAL document, discounting the
    // fixed chart bundle so the cap measures the model's contribution.
    let report_bytes = html.len().saturating_sub(REPORT_CHARTS_JS.len());
    if report_bytes > MAX_REPORT_BYTES {
        return (
            format!(
                "error: the report is too large ({} KiB; the cap is {} KiB). Summarize or \
                 aggregate the data, or narrow it, then try again.",
                report_bytes / 1024,
                MAX_REPORT_BYTES / 1024,
            ),
            false,
        );
    }
    let path = report
        .output_dir()
        .join(format!("red-report-{}.html", uuid::Uuid::new_v4().simple()));
    match write_report_file(&path, &html) {
        Ok(()) => {
            let clean_title = title.map(str::trim).filter(|t| !t.is_empty());
            report.announce(&path, clean_title);
            let label = clean_title.map(|t| format!(" “{t}”")).unwrap_or_default();
            (
                format!(
                    "Generated the report{label}. It's now available as a card in the chat for \
                     the user to open."
                ),
                true,
            )
        }
        Err(e) => (
            format!("error: could not write the report file: {e}"),
            false,
        ),
    }
}

/// Resolve a model-supplied export name to a path inside the assistant's own
/// output folder.
///
/// Only the *stem* is taken, sanitized to `[A-Za-z0-9._-]` and length-capped,
/// then suffixed with a fresh UUID. A tool argument therefore cannot escape the
/// folder (no `..`, no absolute path, no separator survives), cannot clobber an
/// existing file, and cannot choose the extension — the format decides that.
fn export_path(sink: &ReportSink, name: Option<&str>, ext: &str) -> PathBuf {
    let stem: String = name
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    let stem = stem.trim_matches(['-', '.']).to_string();
    let label = if stem.is_empty() {
        String::new()
    } else {
        format!("{stem}-")
    };
    sink.output_dir().join(format!(
        "red-export-{label}{}.{ext}",
        uuid::Uuid::new_v4().simple()
    ))
}

/// Parse the `format` argument of a SQL `export_result`.
fn export_format(input: &Json) -> Result<(red_core::ExportFormat, &'static str), String> {
    match input.get("format").and_then(Json::as_str).unwrap_or("csv") {
        "csv" => Ok((red_core::ExportFormat::Csv, "csv")),
        "json" => Ok((red_core::ExportFormat::Json, "json")),
        "sql" => Ok((red_core::ExportFormat::Sql, "sql")),
        "html" => Ok((red_core::ExportFormat::Html, "html")),
        other => Err(format!(
            "export format must be csv/json/sql/html, not `{other}`"
        )),
    }
}

/// Stream a read-only query's whole result to a file for the user.
///
/// Unlike `run_select` this is **not** row-capped: the rows go to disk, not into
/// the model's context, and the driver's export streams row by row without ever
/// materializing the result. The read gate still applies — an export is a read,
/// and `is_read_only_select` is what makes that true.
async fn export_result(
    driver: &Arc<dyn DatabaseDriver>,
    dialect: Dialect,
    input: &Json,
    sink: &ReportSink,
) -> (String, bool) {
    use std::sync::atomic::AtomicBool;

    let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
    if !is_read_only_select(sql, dialect) {
        return (
            "error: export_result runs a single SELECT or WITH...SELECT query; anything else is \
             rejected"
                .into(),
            false,
        );
    }
    let (format, ext) = match export_format(input) {
        Ok(f) => f,
        Err(why) => return (format!("error: {why}"), false),
    };
    let path = export_path(sink, input.get("name").and_then(Json::as_str), ext);
    // The driver's export reports progress on a channel and honours a cancel flag;
    // neither has a job here (there is no toast to update and no Cancel button),
    // so the flag stays clear and the receiver is dropped immediately.
    let (progress, _rx) = tokio::sync::mpsc::unbounded_channel();
    match driver
        .export(
            sql,
            &path,
            format,
            Arc::new(AtomicBool::new(false)),
            progress,
        )
        .await
    {
        Ok(rows) => {
            sink.announce(&path, Some(&format!("Export ({rows} rows)")));
            (
                format!(
                    "Wrote {rows} row(s) to {}. It is now a card in the chat the user can open.",
                    path.display()
                ),
                true,
            )
        }
        Err(e) => (format!("error: the export failed: {e}"), false),
    }
}

// --- Redis (KV) agent backend ---

/// Round-trip cap on a bounded keyspace walk, so a `kv_scan_keys`/sample never
/// loops unbounded on a huge keyspace.
const KV_SCAN_ROUNDS_CAP: usize = 400;
/// Keys sampled for `kv_analyze` / `kv_biggest_keys` (bounded, like the UI's own
/// biggest-keys/analysis samplers).
const KV_SAMPLE_MAX: usize = 20_000;
/// How many biggest keys `kv_biggest_keys` reports by default.
const KV_BIGGEST_TOP: usize = 30;
/// How many elements of a collection `kv_get_value` previews.
const KV_VALUE_ELEMS: usize = 50;
/// Max keys a single bulk write (kv_delete/kv_expire by pattern) touches per call;
/// past this it reports the bound was hit so the agent can run again.
const KV_BULK_MAX: usize = 50_000;
/// Ceiling on the pending entries `kv_stream_groups` lists for one group. The
/// PEL of a stuck consumer can be enormous; the oldest few show the pattern.
const KV_PENDING_MAX: usize = 100;
/// How many key templates `kv_key_schema` reports. A real keyspace has a handful
/// of shapes; past this the rollup has stopped rolling anything up.
const KV_TEMPLATE_TOP: usize = 40;

/// The Redis agent's read-only tool catalog, gated by tier via
/// [`AiTier::allows_tool`] exactly like the SQL [`tool_catalog`]. Redis writes
/// aren't wired yet, so every tool here is read-only.
pub(crate) fn kv_tool_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
    let all = [
        ToolDef {
            name: "kv_server_info".into(),
            description: "Summarize the server: TOPOLOGY (standalone/sentinel/cluster), total key \
                count, version, memory (used/max/fragmentation), connected clients, ops/sec, \
                keyspace hit rate, evictions/expirations, uptime, and per-database key counts. \
                CALL THIS FIRST — a SCAN means something different on a cluster (it fans out \
                across slots), so the topology frames every other answer."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "kv_scan_keys".into(),
            description: "Find keys by glob pattern (e.g. `user:*`, `session:??`) and return each \
                key's type, TTL, and approximate memory. Bounded — use a selective pattern; this \
                is how you discover what's in the keyspace."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob MATCH pattern (default `*`, all keys)." },
                    "limit": { "type": "integer", "description": "Max keys to return." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_key_schema".into(),
            description: "Infer the keyspace's STRUCTURE: sample keys, segment each on `:`, and \
                report the key templates behind them (`user:*:sessions`, `cache:v2:product:*`) \
                with each one's key count, type, average size, and TTL coverage. Redis has no \
                schema, so the key template IS the schema — CALL THIS BEFORE REASONING ABOUT WHAT \
                THE KEYSPACE HOLDS, rather than guessing patterns and scanning for them."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Optional glob to restrict the sample (e.g. `cache:*`)." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_key_info".into(),
            description: "One key's type, TTL, OBJECT ENCODING, and approximate memory (no value). \
                Use before reading a value to see what shape it is."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "key": { "type": "string", "description": "The exact key name." } },
                "required": ["key"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_get_value".into(),
            description: "Read a key's value (capped): a string's contents, or a preview of a \
                hash/set/zset/list/stream's elements. Large collections report their length and a \
                head window rather than materializing whole."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "key": { "type": "string", "description": "The exact key name." } },
                "required": ["key"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_read_collection".into(),
            description: "Page DEEP into one big key's contents, past the preview kv_get_value \
                stops at: hash fields, set/zset members (cursor-paged), list elements (a head or \
                tail window), or stream entries (newest-first by ID range). Use this when the \
                preview says the collection is larger than what it showed. Echo the `next_cursor` \
                / `next_before` from the previous page to continue."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The exact key name." },
                    "cursor": { "type": "string", "description": "Hash/set/zset: the previous page's next_cursor (omit to start)." },
                    "before": { "type": "string", "description": "Stream: the previous page's next_before, to walk older (omit to start at the newest)." },
                    "from_tail": { "type": "boolean", "description": "List: read the tail rather than the head. Default false." },
                    "limit": { "type": "integer", "description": "Max elements to return." },
                },
                "required": ["key"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_stream_groups".into(),
            description: "A stream's CONSUMER GROUPS: per group, its consumer count, pending \
                (delivered-but-unacked) entries, lag behind the tip, and last-delivered id. Pass \
                `group` to drill into that group's consumers (each with its pending count and \
                idle time) and its oldest pending entries. This is the answer to \"why is my \
                consumer lagging\", \"who owns these pending messages\", and \"is anything \
                stuck\" — a high delivery count with a large idle time is a stuck entry."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The stream key." },
                    "group": { "type": "string", "description": "Drill into this group's consumers and pending entries." },
                },
                "required": ["key"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_keyspace_notifications".into(),
            description: "Read the server's `notify-keyspace-events` setting. An empty value means \
                keyspace notifications are OFF and no watcher will ever see anything — the first \
                thing to check when a subscriber reports silence."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "kv_command".into(),
            description: "Run one INTROSPECTION command verbatim. Deliberately restricted to a \
                hard allowlist of read-only verbs — INFO, MEMORY, OBJECT, TYPE, TTL, PTTL, \
                EXISTS, STRLEN, LATENCY, COMMAND, DBSIZE, LASTSAVE, TIME, ROLE — because a \
                general command tool would route around every other gate in this catalog. \
                Anything else is refused; use the dedicated tool instead. Requires the user's \
                approval, and the approval shows the exact command."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "argv": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "The command and its arguments, e.g. [\"MEMORY\", \"DOCTOR\"].",
                        "minItems": 1,
                    },
                },
                "required": ["argv"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_client_list".into(),
            description: "The clients connected to the server (CLIENT LIST): id, address, name, \
                selected database, age, idle time, flags, and last command. The Redis analogue of \
                a SQL session list. Under a cluster this reports the seed node only."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "kv_biggest_keys".into(),
            description: "Sample the keyspace and return the largest keys by approximate memory \
                (redis-cli --bigkeys style). Bounded walk; the result says if it was truncated. Use \
                to find what's eating memory."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Optional glob to restrict the sample." },
                    "top": { "type": "integer", "description": "How many biggest keys to return." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_analyze".into(),
            description: "Roll a bounded keyspace sample up into a report: total memory, a per-type \
                breakdown, the top key-name namespaces (prefix up to the first `:`) by memory, and \
                a TTL-coverage summary (how many keys never expire vs. expire soon). Use for \
                'what's in here / why is memory high / what lacks a TTL' questions."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Optional glob to restrict the sample." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_slowlog".into(),
            description: "The server's SLOWLOG: recent commands that exceeded the slow threshold, \
                with their execution time and arguments. Use to diagnose slowness."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "count": { "type": "integer", "description": "How many entries (default 32)." } },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_config_get".into(),
            description: "Read one or more CONFIG parameters (glob allowed, e.g. `maxmemory*`). \
                Read-only; never sets."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "parameter": { "type": "string", "description": "CONFIG parameter or glob (e.g. `maxmemory-policy`)." } },
                "required": ["parameter"],
                "additionalProperties": false,
            }),
        },
        export_tool_def(
            "Write the keys matching a glob, with their values, to a file for the user (CSV or \
             JSON) and hand it over as a card in the chat they can open. Bounded: it walks a large \
             but finite number of keys and says so if it stopped early. Use it when the user asks \
             for an export/dump rather than an answer.",
            json!({
                "pattern": { "type": "string", "description": "Glob MATCH pattern (default `*`, every key)." },
                "format": {
                    "type": "string",
                    "enum": ["csv", "json"],
                    "description": "Output format (default \"json\"; CSV writes key,type,ttl,value).",
                },
                "name": { "type": "string", "description": "A short name for the file, e.g. \"session-keys\"." },
            }),
            &[],
        ),
        report_tool_def(),
        spawn_subagent_tool_def(),
        // --- gated writes (Write tier, writable connection only) ---
        ToolDef {
            name: "kv_set".into(),
            description: "Write a key's value. This is how you CREATE or UPDATE data: pick the \
                Redis `type` and pass `value` in that type's shape — a string/number for \
                `string`, a { field: value } object for `hash` and `stream`, an array for `set` \
                and `list`, a { member: score } object for `zset`. For one hash field, pass \
                `field` plus a scalar `value`. `ttl_seconds` sets an expiry. `mode` is \"set\" \
                (default: the key ends up holding exactly this, so a hash/set/zset/list is \
                cleared first) or \"append\" (add to what is already there); a stream always \
                appends. Requires the user's explicit approval, which shows the exact commands \
                that will run."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The exact key to write (not a glob)." },
                    "type": {
                        "type": "string",
                        "enum": ["string", "hash", "set", "zset", "list", "stream"],
                        "description": "The Redis type to write. Check kv_key_info first if the key may already exist as another type.",
                    },
                    "value": { "description": "The value, in the shape this `type` takes (see the tool description)." },
                    "field": { "type": "string", "description": "Hash only: write this single field, leaving the rest of the hash alone." },
                    "ttl_seconds": { "type": "integer", "description": "Expiry in seconds; omit for no expiry." },
                    "mode": {
                        "type": "string",
                        "enum": ["set", "append"],
                        "description": "\"set\" (default) replaces the key's contents; \"append\" adds to them.",
                    },
                },
                "required": ["key", "type", "value"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_expire".into(),
            description: "Set or remove a key's expiry (EXPIRE / PERSIST). Targets one `key`, or \
                every key matching a `pattern` (bulk). Requires the user's explicit approval; a \
                keyspace-wide TTL (pattern `*`) is refused. Read/scan first to know what you'll \
                affect."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "A single key to expire/persist." },
                    "pattern": { "type": "string", "description": "Glob to bulk-expire all matching keys (mutually exclusive with `key`)." },
                    "seconds": { "type": "integer", "description": "TTL in seconds; omit or 0 to PERSIST (remove expiry)." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_delete".into(),
            description: "Delete keys (DEL): one `key`, an explicit list of `keys`, or every key \
                matching a `pattern` (bulk). Requires explicit approval; deleting the whole \
                keyspace (pattern `*`) is refused. Scan/count first and tell the user how many \
                keys will go."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "A single key to delete." },
                    "keys": { "type": "array", "items": { "type": "string" }, "description": "An explicit list of keys to delete." },
                    "pattern": { "type": "string", "description": "Glob to bulk-delete all matching keys." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_rename".into(),
            description: "Rename a key (RENAME `from` `to`); overwrites `to` if it exists. Requires \
                explicit approval."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Existing key name." },
                    "to": { "type": "string", "description": "New key name." },
                },
                "required": ["from", "to"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_copy_key".into(),
            description: "Copy a key with its value and remaining expiry to a new name (DUMP then \
                RESTORE), leaving the original alone. The serialized value never passes through \
                this conversation — the server copies it — so this works for a key of any size or \
                type. Requires explicit approval; it refuses to overwrite an existing key unless \
                `replace` is set."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "The key to copy." },
                    "to": { "type": "string", "description": "The new key name." },
                    "replace": { "type": "boolean", "description": "Overwrite `to` if it already exists. Default false." },
                    "keep_ttl": { "type": "boolean", "description": "Carry the source's remaining expiry over. Default true." },
                },
                "required": ["from", "to"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_client_kill".into(),
            description: "Disconnect a client by its connection id (CLIENT KILL ID). Call \
                kv_client_list first for the `id`, and copy that client's `addr` and `cmd` into \
                this call so the user can see what they are disconnecting — the target is \
                re-checked against the live server first and refused if the id has been reused. \
                Requires explicit approval."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "The client's connection id, from kv_client_list." },
                    "addr": { "type": "string", "description": "The client's address, copied from kv_client_list; verified before the kill." },
                    "cmd": { "type": "string", "description": "The client's last command, copied from kv_client_list, so the approval shows what is being cut off." },
                },
                "required": ["id"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_config_set".into(),
            description: "Set a server CONFIG parameter (CONFIG SET). Powerful — can change memory \
                limits, persistence, eviction. Requires explicit approval; read the current value \
                with kv_config_get first."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "parameter": { "type": "string", "description": "CONFIG parameter (e.g. `maxmemory-policy`)." },
                    "value": { "type": "string", "description": "New value." },
                },
                "required": ["parameter", "value"],
                "additionalProperties": false,
            }),
        },
    ];
    gate_catalog(all, policy)
}

/// Bounded keyspace walk: loop `scan_keys` accumulating metadata until `max_keys`
/// are collected, the keyspace is exhausted, or the round cap is hit. Returns the
/// keys (truncated to `max_keys`) and whether the walk exhausted the keyspace.
async fn kv_collect_keys(
    driver: &Arc<dyn KvDriver>,
    pattern: Option<&str>,
    max_keys: usize,
) -> Result<(Vec<KeyMeta>, bool), RedError> {
    let abort = AbortSignal::new();
    let mut cursor = ScanCursor::START;
    let mut out: Vec<KeyMeta> = Vec::new();
    let mut exhausted = false;
    for _ in 0..KV_SCAN_ROUNDS_CAP {
        let budget = ScanBudget {
            count_hint: 300,
            wall_clock: Duration::from_millis(300),
            want: 200,
        };
        let page = driver
            .scan_keys(cursor, pattern, None, None, budget, &abort)
            .await?;
        out.extend(page.keys);
        cursor = page.next_cursor;
        exhausted = page.exhausted;
        if exhausted || out.len() >= max_keys {
            break;
        }
    }
    out.truncate(max_keys);
    Ok((out, exhausted))
}

/// Ceiling on the keys / documents one non-SQL `export_result` writes. The SQL
/// seam streams through the driver's own exporter and needs no bound; these two
/// walk the keyspace/collection from here, so they stop at a stated number
/// rather than running for an unbounded time.
const EXPORT_ITEM_MAX: usize = 50_000;
/// Documents fetched per keyset window while exporting a collection.
const EXPORT_DOC_WINDOW: usize = 1_000;

/// Write matching keys and their values to a file for the user.
///
/// Values are read and written key by key, so the file grows incrementally and
/// no whole-keyspace snapshot is ever held. The key *list* is the one bounded
/// materialization, and its bound is reported.
async fn kv_export(driver: &Arc<dyn KvDriver>, input: &Json, sink: &ReportSink) -> (String, bool) {
    use std::io::Write;

    let pattern = input
        .get("pattern")
        .and_then(Json::as_str)
        .filter(|p| !p.is_empty());
    let as_csv = match input.get("format").and_then(Json::as_str).unwrap_or("json") {
        "json" => false,
        "csv" => true,
        other => {
            return (
                format!("error: export format must be csv or json, not `{other}`"),
                false,
            );
        }
    };
    let (keys, exhausted) = match kv_collect_keys(driver, pattern, EXPORT_ITEM_MAX).await {
        Ok(k) => k,
        Err(e) => return (format!("error: {e}"), false),
    };
    if keys.is_empty() {
        return (
            "No keys matched, so nothing was exported.".to_string(),
            true,
        );
    }
    let path = export_path(
        sink,
        input.get("name").and_then(Json::as_str),
        if as_csv { "csv" } else { "json" },
    );
    let mut file = match std::fs::File::create(&path) {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            return (
                format!("error: could not create the export file: {e}"),
                false,
            );
        }
    };
    let mut write = |line: &str| file.write_all(line.as_bytes());
    let result = (|| -> std::io::Result<()> {
        if as_csv {
            write("key,type,ttl,value\n")?;
        } else {
            write("[\n")?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        return (format!("error: writing the export failed: {e}"), false);
    }
    let mut written = 0usize;
    for (i, meta) in keys.iter().enumerate() {
        let value = driver
            .read_value(&meta.key)
            .await
            .ok()
            .flatten()
            .map(|v| fmt_kv_value(&v))
            .unwrap_or_default();
        let ttl = kv_ttl(meta.ttl);
        let line = if as_csv {
            format!(
                "{},{},{},{}\n",
                csv_field(&meta.key),
                meta.kv_type.label(),
                ttl,
                csv_field(&value),
            )
        } else {
            format!(
                "  {{\"key\":{},\"type\":{},\"ttl\":{},\"value\":{}}}{}\n",
                json_str(&meta.key),
                json_str(meta.kv_type.label()),
                json_str(&ttl),
                json_str(&value),
                if i + 1 == keys.len() { "" } else { "," },
            )
        };
        if let Err(e) = file.write_all(line.as_bytes()) {
            return (
                format!("error: writing the export failed after {written} key(s): {e}"),
                false,
            );
        }
        written += 1;
    }
    if !as_csv && let Err(e) = file.write_all(b"]\n") {
        return (format!("error: writing the export failed: {e}"), false);
    }
    if let Err(e) = file.flush() {
        return (format!("error: flushing the export failed: {e}"), false);
    }
    let note = if exhausted {
        String::new()
    } else {
        format!(" (stopped at the {EXPORT_ITEM_MAX}-key bound; narrow the pattern for the rest)")
    };
    sink.announce(&path, Some(&format!("Export ({written} keys)")));
    (
        format!(
            "Wrote {written} key(s) to {}{note}. It is now a card in the chat the user can open.",
            path.display()
        ),
        true,
    )
}

/// Write matching documents to a JSON array file for the user.
///
/// Paged by `_id` keyset (`find_seek`), one window at a time and appended as it
/// goes, so an export of a large collection never holds more than a window.
async fn doc_export(
    driver: &Arc<dyn DocDriver>,
    input: &Json,
    sink: &ReportSink,
) -> (String, bool) {
    use std::io::Write;

    let db = input.get("db").and_then(Json::as_str).unwrap_or("");
    let coll = input.get("coll").and_then(Json::as_str).unwrap_or("");
    if db.is_empty() || coll.is_empty() {
        return ("error: export_result needs `db` and `coll`".into(), false);
    }
    let filter = match doc_arg_value(driver, input, "filter") {
        Ok(f) => f,
        Err(e) => return (format!("error: {e}"), false),
    };
    let path = export_path(sink, input.get("name").and_then(Json::as_str), "json");
    let mut file = match std::fs::File::create(&path) {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            return (
                format!("error: could not create the export file: {e}"),
                false,
            );
        }
    };
    let abort = AbortSignal::new();
    let mut after: Option<DocValue> = None;
    let mut written = 0usize;
    let mut truncated = false;
    if let Err(e) = file.write_all(b"[\n") {
        return (format!("error: writing the export failed: {e}"), false);
    }
    loop {
        let window = match driver
            .find_seek(
                db,
                coll,
                filter.as_ref(),
                red_core::doc::DocSeek::Forward {
                    after: after.clone(),
                },
                EXPORT_DOC_WINDOW.min(EXPORT_ITEM_MAX - written),
                &abort,
            )
            .await
        {
            Ok(w) => w,
            Err(e) => {
                return (
                    format!("error: the export failed after {written} document(s): {e}"),
                    false,
                );
            }
        };
        if window.is_empty() {
            break;
        }
        for doc in &window {
            let sep = if written == 0 { "  " } else { ",\n  " };
            let line = format!("{sep}{}", doc.to_doc_value().to_extended_json());
            if let Err(e) = file.write_all(line.as_bytes()) {
                return (
                    format!("error: writing the export failed after {written} document(s): {e}"),
                    false,
                );
            }
            written += 1;
        }
        after = window.last().map(|d| d.id.clone());
        if written >= EXPORT_ITEM_MAX {
            truncated = true;
            break;
        }
    }
    if let Err(e) = file.write_all(b"\n]\n").and_then(|()| file.flush()) {
        return (format!("error: writing the export failed: {e}"), false);
    }
    let note = if truncated {
        format!(
            " (stopped at the {EXPORT_ITEM_MAX}-document bound; narrow the filter for the rest)"
        )
    } else {
        String::new()
    };
    sink.announce(&path, Some(&format!("Export ({written} documents)")));
    (
        format!(
            "Wrote {written} document(s) to {}{note}. It is now a card in the chat the user can \
             open.",
            path.display()
        ),
        true,
    )
}

/// One CSV field: quoted and doubled-up when it carries a comma, quote, or
/// newline, per RFC 4180.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// One JSON string literal, escaped by `serde_json` so the export is parseable
/// whatever a Redis value happens to contain.
fn json_str(s: &str) -> String {
    Json::String(s.to_string()).to_string()
}

/// Page deep into one key's contents, dispatching on the key's actual type to
/// the windowed reader for it. Never loads a whole collection: the reply carries
/// the continuation token so the model pages rather than asking for everything.
async fn kv_read_collection(
    driver: &Arc<dyn KvDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    use red_core::kv::{CollectionKind, KvElement, KvType};

    let key = input.get("key").and_then(Json::as_str).unwrap_or("");
    if key.is_empty() {
        return ("error: `key` is required".into(), false);
    }
    let limit = input
        .get("limit")
        .and_then(Json::as_u64)
        .map(|n| n as usize)
        .unwrap_or(limits.max_rows.max(1))
        .clamp(1, limits.max_rows.max(1));
    let meta = match driver.probe_key(key).await {
        Ok(Some(m)) => m,
        Ok(None) => return (format!("key `{key}` does not exist"), true),
        Err(e) => return (format!("error: {e}"), false),
    };
    let budget = ScanBudget {
        count_hint: limit.min(1000) as u32,
        wall_clock: Duration::from_millis(500),
        want: limit,
    };
    let abort = AbortSignal::new();
    let kind = match meta.kv_type {
        KvType::Hash => Some(CollectionKind::Hash),
        KvType::Set => Some(CollectionKind::Set),
        KvType::ZSet => Some(CollectionKind::ZSet),
        _ => None,
    };
    let out = if let Some(kind) = kind {
        let cursor = input
            .get("cursor")
            .and_then(Json::as_str)
            .and_then(|c| c.parse::<u64>().ok())
            .unwrap_or(0);
        match driver
            .read_collection_page(key, kind, cursor, budget, &abort)
            .await
        {
            Ok(page) => {
                let mut s = format!(
                    "{key} ({}) — {} element(s):\n",
                    meta.kv_type.label(),
                    page.elements.len()
                );
                for e in &page.elements {
                    s.push_str(&match e {
                        KvElement::Member(m) => format!("  {m}\n"),
                        KvElement::Field(f, v) => format!("  {f} = {v}\n"),
                        KvElement::Scored(m, score) => format!("  {score}  {m}\n"),
                    });
                }
                s.push_str(&if page.exhausted {
                    "(end of collection)\n".to_string()
                } else {
                    format!("(more: pass cursor \"{}\" to continue)\n", page.next_cursor)
                });
                s
            }
            Err(e) => return (format!("error: {e}"), false),
        }
    } else if meta.kv_type == KvType::List {
        let from_tail = input
            .get("from_tail")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        // `LRANGE`'s cost grows with the offset, so the seam offers a head or a
        // tail window and no arbitrary deep-middle access; say so rather than
        // letting the model ask for page 900.
        match driver.read_list_window(key, !from_tail, limit).await {
            Ok(values) => {
                let mut s = format!(
                    "{key} (list) — {} element(s) from the {}:\n",
                    values.len(),
                    if from_tail { "tail" } else { "head" },
                );
                for v in &values {
                    s.push_str(&format!("  {v}\n"));
                }
                s.push_str("(lists window from either end only; there is no deep-middle page)\n");
                s
            }
            Err(e) => return (format!("error: {e}"), false),
        }
    } else if meta.kv_type == KvType::Stream {
        let before = input
            .get("before")
            .and_then(Json::as_str)
            .filter(|b| !b.is_empty());
        match driver.read_stream_range(key, before, limit).await {
            Ok(page) => {
                let mut s = format!(
                    "{key} (stream) — {} entr(ies), newest first:\n",
                    page.entries.len()
                );
                for e in &page.entries {
                    let fields: Vec<String> =
                        e.fields.iter().map(|(f, v)| format!("{f}={v}")).collect();
                    s.push_str(&format!("  {}  {}\n", e.id, fields.join(" ")));
                }
                s.push_str(&match (page.exhausted, &page.next_before) {
                    (false, Some(b)) => format!("(more: pass before \"{b}\" to walk older)\n"),
                    _ => "(end of stream)\n".to_string(),
                });
                s
            }
            Err(e) => return (format!("error: {e}"), false),
        }
    } else {
        return (
            format!(
                "`{key}` is a {}, which has no pages: read it with kv_get_value.",
                meta.kv_type.label()
            ),
            false,
        );
    };
    (cap_result_bytes(out, limits.max_result_bytes), true)
}

/// A stream's consumer-group diagnostics: every group, and optionally one
/// group's consumers and oldest pending entries.
async fn kv_stream_groups(
    driver: &Arc<dyn KvDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    let key = input.get("key").and_then(Json::as_str).unwrap_or("");
    if key.is_empty() {
        return ("error: `key` is required".into(), false);
    }
    let groups = match driver.stream_groups(key).await {
        Ok(g) => g,
        Err(e) => return (format!("error: {e}"), false),
    };
    if groups.is_empty() {
        return (
            format!("`{key}` has no consumer groups (entries are read directly, not via a group)."),
            true,
        );
    }
    let mut out = format!("{} consumer group(s) on `{key}`:\n", groups.len());
    for g in &groups {
        out.push_str(&format!(
            "  {}: {} consumer(s), {} pending, lag {}, last-delivered {}\n",
            g.name,
            g.consumers,
            g.pending,
            g.lag
                .map(|l| l.to_string())
                // Redis reports nil lag after certain trims; "unknown" is the
                // honest reading, and it is not the same as zero.
                .unwrap_or_else(|| "unknown".into()),
            g.last_delivered_id,
        ));
    }
    if let Some(group) = input
        .get("group")
        .and_then(Json::as_str)
        .filter(|g| !g.is_empty())
    {
        let count = limits.max_rows.clamp(1, KV_PENDING_MAX);
        match driver.stream_consumers(key, group).await {
            Ok(consumers) => {
                out.push_str(&format!("\nConsumers in `{group}`:\n"));
                for c in &consumers {
                    out.push_str(&format!(
                        "  {}: {} pending, idle {}\n",
                        c.name,
                        c.pending,
                        kv_ttl(Some(c.idle)),
                    ));
                }
            }
            Err(e) => out.push_str(&format!("\nConsumers in `{group}`: error: {e}\n")),
        }
        match driver.stream_pending(key, group, count).await {
            Ok(pending) if pending.is_empty() => {
                out.push_str("\nNothing pending: every delivered entry has been acked.\n");
            }
            Ok(pending) => {
                out.push_str(&format!("\n{} pending entr(ies):\n", pending.len()));
                for p in &pending {
                    out.push_str(&format!(
                        "  {} held by {}, idle {}, delivered {}x\n",
                        p.id,
                        p.consumer,
                        kv_ttl(Some(p.idle)),
                        p.delivery_count,
                    ));
                }
            }
            Err(e) => out.push_str(&format!("\nPending entries: error: {e}\n")),
        }
    }
    (cap_result_bytes(out, limits.max_result_bytes), true)
}

/// Execute one Redis agent tool (the KV analogue of [`run_tool`]). Read-only:
/// every arm reads through the `KvDriver` seam. Shares the tier gate, the byte
/// cap, and the `generate_report` pipeline with the SQL path.
pub(crate) async fn kv_run_tool(
    driver: &Arc<dyn KvDriver>,
    name: &str,
    input: &Json,
    policy: &AiPolicy,
    _cancel: &CancelToken,
    report: &ReportSink,
) -> (String, bool) {
    if !policy.tier.allows_tool(name) {
        return (
            format!("error: the `{name}` tool is not available at this access tier"),
            false,
        );
    }
    // Defense in depth: run_turn already vetted and prompted, but re-run the
    // catastrophic-shape guard here so no caller (a subagent, a future path) can
    // execute a keyspace-wide write that assess_kv_write would refuse.
    if is_kv_write_tool(name)
        && let WriteAssessment::Reject(why) = assess_kv_write(name, input)
    {
        return (format!("error: {why}"), false);
    }
    let limits = &policy.limits;
    let (content, ok) = match name {
        "kv_server_info" => {
            // Topology and DBSIZE lead: this is the "call this first" tool, and
            // both change how every later answer should be read (a SCAN fans out
            // on a cluster; a sample is only meaningful against a total).
            let header = format!(
                "Topology: {:?} · DBSIZE {} key(s) in the selected database\n",
                driver.topology(),
                driver.db_size().await.unwrap_or(0),
            );
            match driver.command(&["INFO".to_string()]).await {
                Ok(RespValue::Bulk(info)) | Ok(RespValue::Simple(info)) => {
                    (format!("{header}{}", kv_info_summary(&info)), true)
                }
                Ok(other) => (format!("{header}unexpected INFO reply: {other:?}"), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_scan_keys" => {
            let pattern = input
                .get("pattern")
                .and_then(Json::as_str)
                .filter(|p| !p.is_empty());
            let limit = input
                .get("limit")
                .and_then(Json::as_u64)
                .map(|n| n as usize)
                .unwrap_or(limits.max_rows.max(1))
                .clamp(1, limits.max_rows.max(1));
            match kv_collect_keys(driver, pattern, limit).await {
                Ok((keys, exhausted)) => {
                    if keys.is_empty() {
                        ("No keys matched.".to_string(), true)
                    } else {
                        let mut out = format!("{} key(s):\n", keys.len());
                        for k in &keys {
                            out.push_str(&format!(
                                "  {}  [{}, {}, ~{}]\n",
                                k.key,
                                k.kv_type.label(),
                                kv_ttl(k.ttl),
                                fmt_bytes(k.approx_bytes),
                            ));
                        }
                        if !exhausted && keys.len() >= limit {
                            out.push_str(
                                "(more keys may match; raise `limit` or narrow the pattern)\n",
                            );
                        }
                        (out, true)
                    }
                }
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_key_schema" => {
            let pattern = input
                .get("pattern")
                .and_then(Json::as_str)
                .filter(|p| !p.is_empty());
            let total = driver.db_size().await.unwrap_or(0);
            match kv_collect_keys(driver, pattern, KV_SAMPLE_MAX).await {
                Ok((keys, exhausted)) => {
                    let templates = infer_key_templates(&keys, KV_TEMPLATE_TOP);
                    (
                        cap_result_bytes(
                            kv_format_templates(&templates, keys.len(), total, exhausted),
                            limits.max_result_bytes,
                        ),
                        true,
                    )
                }
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_key_info" => {
            let key = input.get("key").and_then(Json::as_str).unwrap_or("");
            if key.is_empty() {
                return ("error: `key` is required".into(), false);
            }
            match driver.probe_key(key).await {
                Ok(Some(m)) => (
                    format!(
                        "{}\n  type: {}\n  ttl: {}\n  encoding: {}\n  memory: ~{}",
                        m.key,
                        m.kv_type.label(),
                        kv_ttl(m.ttl),
                        m.encoding,
                        fmt_bytes(m.approx_bytes),
                    ),
                    true,
                ),
                Ok(None) => (format!("key `{key}` does not exist"), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_get_value" => {
            let key = input.get("key").and_then(Json::as_str).unwrap_or("");
            if key.is_empty() {
                return ("error: `key` is required".into(), false);
            }
            match driver.read_value(key).await {
                Ok(Some(v)) => (
                    cap_result_bytes(
                        format!("{key} =\n{}", fmt_kv_value(&v)),
                        limits.max_result_bytes,
                    ),
                    true,
                ),
                Ok(None) => (format!("key `{key}` does not exist"), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_read_collection" => kv_read_collection(driver, input, limits).await,
        "kv_stream_groups" => kv_stream_groups(driver, input, limits).await,
        "kv_keyspace_notifications" => match driver.notify_config().await {
            Ok(flags) if flags.trim().is_empty() => (
                "notify-keyspace-events is EMPTY: keyspace notifications are off, so nothing will \
                 be delivered to any subscriber until they are enabled."
                    .to_string(),
                true,
            ),
            Ok(flags) => (
                format!(
                    "notify-keyspace-events = {flags} (notifications are on for these classes)"
                ),
                true,
            ),
            Err(e) => (format!("error: {e}"), false),
        },
        "kv_command" => {
            let argv = kv_command_argv(input);
            match kv_allowed_command(&argv) {
                // Re-checked here as well as in the gate: this is the one tool
                // whose whole safety story is the allowlist, so it is enforced at
                // the point of execution, not only where the prompt was built.
                Err(why) => (format!("error: {why}"), false),
                Ok(()) => match driver.command(&argv).await {
                    Ok(v) => (
                        cap_result_bytes(format!("{v:?}"), limits.max_result_bytes),
                        true,
                    ),
                    Err(e) => (format!("error: {e}"), false),
                },
            }
        }
        "kv_client_list" => match driver.client_list().await {
            Ok(clients) if clients.is_empty() => ("No clients are connected.".to_string(), true),
            Ok(clients) => {
                let mut out = format!("{} connected client(s):\n", clients.len());
                for c in &clients {
                    out.push_str(&format!(
                        "  id={} {} db={} age={}s idle={}s flags={} last={}{}\n",
                        c.id,
                        c.addr,
                        c.db,
                        c.age,
                        c.idle,
                        c.flags,
                        c.cmd,
                        if c.name.is_empty() {
                            String::new()
                        } else {
                            format!(" name={}", c.name)
                        },
                    ));
                }
                (cap_result_bytes(out, limits.max_result_bytes), true)
            }
            Err(e) => (format!("error: {e}"), false),
        },
        "kv_biggest_keys" => {
            let pattern = input
                .get("pattern")
                .and_then(Json::as_str)
                .filter(|p| !p.is_empty());
            let top = input
                .get("top")
                .and_then(Json::as_u64)
                .map(|n| n as usize)
                .unwrap_or(KV_BIGGEST_TOP)
                .clamp(1, 200);
            match kv_collect_keys(driver, pattern, KV_SAMPLE_MAX).await {
                Ok((mut keys, exhausted)) => {
                    let sampled = keys.len();
                    keys.sort_by_key(|k| std::cmp::Reverse(k.approx_bytes));
                    keys.truncate(top);
                    let mut out = format!(
                        "Top {} of {} sampled key(s) by memory{}:\n",
                        keys.len(),
                        sampled,
                        if exhausted { "" } else { " (sample truncated)" },
                    );
                    for k in &keys {
                        out.push_str(&format!(
                            "  ~{}  {}  [{}, {}]\n",
                            fmt_bytes(k.approx_bytes),
                            k.key,
                            k.kv_type.label(),
                            kv_ttl(k.ttl),
                        ));
                    }
                    (out, true)
                }
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_analyze" => {
            let pattern = input
                .get("pattern")
                .and_then(Json::as_str)
                .filter(|p| !p.is_empty());
            let total = driver.db_size().await.unwrap_or(0);
            match kv_collect_keys(driver, pattern, KV_SAMPLE_MAX).await {
                Ok((keys, exhausted)) => {
                    let report = analyze_keyspace(&keys, total, !exhausted, 0);
                    (kv_format_analysis(&report), true)
                }
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_slowlog" => {
            let count = input
                .get("count")
                .and_then(Json::as_u64)
                .map(|n| n as usize)
                .unwrap_or(32)
                .clamp(1, 256);
            match driver.slowlog(count).await {
                Ok(entries) if entries.is_empty() => ("The slow log is empty.".to_string(), true),
                Ok(entries) => {
                    let mut out = format!("{} slow-log entr(ies):\n", entries.len());
                    for e in &entries {
                        out.push_str(&format!(
                            "  #{} {:.1}ms  {}\n",
                            e.id,
                            e.micros as f64 / 1000.0,
                            e.argv.join(" "),
                        ));
                    }
                    (cap_result_bytes(out, limits.max_result_bytes), true)
                }
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_config_get" => {
            let param = input.get("parameter").and_then(Json::as_str).unwrap_or("");
            if param.is_empty() {
                return ("error: `parameter` is required".into(), false);
            }
            let argv = ["CONFIG".to_string(), "GET".to_string(), param.to_string()];
            match driver.command(&argv).await {
                Ok(RespValue::Array(items)) if items.is_empty() => {
                    (format!("no CONFIG parameter matched `{param}`"), true)
                }
                Ok(RespValue::Array(items)) => {
                    let mut out = String::new();
                    for pair in items.chunks(2) {
                        let k = resp_scalar(pair.first());
                        // Never echo an auth secret back to the model/provider: a
                        // CONFIG GET of `requirepass` (or a glob like `*`) would
                        // otherwise exfiltrate the server's credentials as tool
                        // output. Redact the value while keeping the key visible.
                        let v = if is_secret_config_param(&k) {
                            "<redacted>".to_string()
                        } else {
                            resp_scalar(pair.get(1))
                        };
                        out.push_str(&format!("{k} = {v}\n"));
                    }
                    (out, true)
                }
                Ok(other) => (format!("unexpected CONFIG reply: {other:?}"), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        // --- gated writes: run_turn already surfaced the Allow/Deny prompt and
        // ran these only on approval; execute directly here.
        "kv_set" => match kv_set_plan(input) {
            Ok(plan) => kv_apply_set(driver, &plan).await,
            Err(why) => (format!("error: {why}"), false),
        },
        "kv_expire" => {
            let seconds = input.get("seconds").and_then(Json::as_i64);
            let ttl = match seconds {
                Some(s) if s > 0 => Some(Duration::from_secs(s as u64)),
                _ => None,
            };
            let verb = if ttl.is_some() {
                "Set expiry on"
            } else {
                "Removed expiry from"
            };
            if let Some(key) = input
                .get("key")
                .and_then(Json::as_str)
                .filter(|k| !k.is_empty())
            {
                match driver.set_ttl(key, ttl).await {
                    Ok(()) => (format!("{verb} `{key}`."), true),
                    Err(e) => (format!("error: {e}"), false),
                }
            } else if let Some(pattern) = input
                .get("pattern")
                .and_then(Json::as_str)
                .filter(|p| !p.is_empty())
            {
                match kv_collect_keys(driver, Some(pattern), KV_BULK_MAX).await {
                    Ok((keys, exhausted)) => {
                        let mut n = 0u64;
                        for k in &keys {
                            match driver.set_ttl(&k.key, ttl).await {
                                Ok(()) => n += 1,
                                Err(e) => return (format!("error after {n} key(s): {e}"), false),
                            }
                        }
                        let more = if exhausted {
                            ""
                        } else {
                            " (bound hit; run again for the rest)"
                        };
                        (
                            format!("{verb} {n} key(s) matching `{pattern}`{more}."),
                            true,
                        )
                    }
                    Err(e) => (format!("error: {e}"), false),
                }
            } else {
                ("error: kv_expire needs `key` or `pattern`".into(), false)
            }
        }
        "kv_delete" => {
            let mut targets = kv_delete_targets(input);
            let mut note = "";
            if targets.is_empty()
                && let Some(pattern) = input
                    .get("pattern")
                    .and_then(Json::as_str)
                    .filter(|p| !p.is_empty())
            {
                match kv_collect_keys(driver, Some(pattern), KV_BULK_MAX).await {
                    Ok((keys, exhausted)) => {
                        targets = keys.into_iter().map(|k| k.key).collect();
                        if !exhausted {
                            note = " (bound hit; run again for the rest)";
                        }
                    }
                    Err(e) => return (format!("error: {e}"), false),
                }
            }
            if targets.is_empty() {
                ("No keys matched; nothing deleted.".to_string(), true)
            } else {
                match driver.delete_keys(&targets).await {
                    Ok(n) => (format!("Deleted {n} key(s){note}."), true),
                    Err(e) => (format!("error: {e}"), false),
                }
            }
        }
        "kv_rename" => {
            let from = input.get("from").and_then(Json::as_str).unwrap_or("");
            let to = input.get("to").and_then(Json::as_str).unwrap_or("");
            if from.is_empty() || to.is_empty() {
                return ("error: kv_rename needs `from` and `to`".into(), false);
            }
            match driver.rename_key(from, to).await {
                Ok(()) => (format!("Renamed `{from}` to `{to}`."), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_copy_key" => {
            let from = input.get("from").and_then(Json::as_str).unwrap_or("");
            let to = input.get("to").and_then(Json::as_str).unwrap_or("");
            if from.is_empty() || to.is_empty() {
                return ("error: kv_copy_key needs `from` and `to`".into(), false);
            }
            let keep_ttl = input
                .get("keep_ttl")
                .and_then(Json::as_bool)
                .unwrap_or(true);
            let replace = input
                .get("replace")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            match driver.dump_key(from).await {
                Ok(None) => (
                    format!("key `{from}` does not exist; nothing copied."),
                    true,
                ),
                Ok(Some((payload, ttl))) => {
                    match driver
                        .restore_key(to, keep_ttl.then_some(ttl).flatten(), &payload, replace)
                        .await
                    {
                        Ok(()) => (format!("Copied `{from}` to `{to}`."), true),
                        // BUSYKEY is the guard doing its job, not a transport
                        // failure; say which so the model offers `replace` rather
                        // than retrying blindly.
                        Err(e) => (
                            format!(
                                "error: copying to `{to}` failed: {e}. If the key already exists, \
                                 set `replace` to overwrite it deliberately."
                            ),
                            false,
                        ),
                    }
                }
                Err(e) => (format!("error: reading `{from}` failed: {e}"), false),
            }
        }
        "kv_client_kill" => {
            let Some(id) = input.get("id").and_then(Json::as_i64) else {
                return ("error: kv_client_kill needs an `id`".into(), false);
            };
            // Re-resolve before cutting: a connection id the user approved may
            // have closed and been handed to somebody else since the prompt.
            match driver.client_list().await {
                Ok(clients) => match clients.iter().find(|c| c.id == id) {
                    None => (
                        format!("client {id} is no longer connected; nothing to disconnect."),
                        true,
                    ),
                    Some(live) => {
                        if let Some(claimed) = input
                            .get("addr")
                            .and_then(Json::as_str)
                            .filter(|a| !a.is_empty())
                            && live.addr != claimed
                        {
                            return (
                                format!(
                                    "error: client {id} is now {}, not {claimed}: the id was \
                                     reused since you read it. Re-read kv_client_list and propose \
                                     again.",
                                    live.addr
                                ),
                                false,
                            );
                        }
                        match driver.client_kill(id).await {
                            Ok(()) => (format!("Disconnected client {id} ({}).", live.addr), true),
                            Err(e) => (format!("error: {e}"), false),
                        }
                    }
                },
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_config_set" => {
            let param = input.get("parameter").and_then(Json::as_str).unwrap_or("");
            let value = input.get("value").and_then(Json::as_str).unwrap_or("");
            if param.is_empty() {
                return ("error: kv_config_set needs `parameter`".into(), false);
            }
            let argv = [
                "CONFIG".to_string(),
                "SET".to_string(),
                param.to_string(),
                value.to_string(),
            ];
            match driver.command(&argv).await {
                Ok(_) => (format!("Applied CONFIG SET {param} {value}."), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "export_result" => kv_export(driver, input, report).await,
        "generate_report" => run_generate_report(input, report),
        other => (format!("error: unknown tool `{other}`"), false),
    };
    (content, ok)
}

/// Append the grounding footer every seam's system prompt shares: the live
/// connection line and, when set, the read-only notice. One place so the footer
/// can't drift per seam; the seam passes its already-built body and its own
/// read-only wording (SQL names the blocked ops; KV/doc keep it terse). SQL's
/// schema overview is appended by its caller afterward, since only SQL has one.
fn finish_system_prompt(mut body: String, ctx: &AiContext, read_only_note: &str) -> String {
    if !ctx.connection.is_empty() {
        body.push_str(&format!("\nConnected to: {}", ctx.connection));
    }
    if ctx.read_only {
        body.push('\n');
        body.push_str(read_only_note);
    }
    body
}

/// The Redis agent's system prompt (the KV analogue of [`system_prompt`]): the
/// same shape, but describing the `kv_*` tools and Redis idioms instead of SQL.
/// Grounding is lazy — the model calls `kv_server_info`/`kv_scan_keys` rather
/// than being handed a pre-built summary — so no per-turn keyspace context is
/// needed.
pub(crate) fn kv_system_prompt(ctx: &AiContext, policy: &AiPolicy) -> String {
    let tools_line = match policy.tier {
        AiTier::Off => {
            "You have NO Redis tools available; answer from the conversation alone and tell the \
             user you cannot read the live server."
        }
        AiTier::Schema => {
            "You have metadata-only Redis tools: kv_server_info, kv_scan_keys, kv_key_schema, and \
             kv_key_info. You can see the server's stats, the keyspace's key templates, and keys' \
             types/TTLs/sizes, but you CANNOT read a key's value."
        }
        AiTier::Read => {
            "You have read-only Redis tools: kv_server_info (INFO summary, topology and size), \
             kv_key_schema (the keyspace's inferred key templates), kv_scan_keys (find keys by \
             glob pattern), kv_key_info (a key's type/TTL/encoding/size), kv_get_value (a key's \
             value or a collection preview), kv_read_collection (page deep into a big \
             collection/list/stream), kv_stream_groups (consumer groups, pending and lag), \
             kv_biggest_keys (sample for the largest keys by memory), kv_analyze (a keyspace \
             rollup: memory by type and namespace, TTL coverage), kv_slowlog (recent slow \
             commands), kv_client_list (connected clients), kv_config_get (read a CONFIG \
             parameter), export_result (write keys to a file for the user), and generate_report \
             (author an HTML report from what you've read, with optional Chart.js charts; it \
             appears as a card the user can open — use it when the user asks for a report). Ground \
             every answer in the live server with these tools rather than guessing."
        }
        AiTier::Write => {
            "You have the read-only Redis tools (kv_server_info, kv_scan_keys, kv_key_info, \
             kv_get_value, kv_biggest_keys, kv_analyze, kv_slowlog, kv_config_get, generate_report) \
             AND gated tools: kv_set (write a key of any type — this is how you create or update \
             data), kv_expire (set/remove a key's TTL), kv_delete (delete keys), kv_rename, \
             kv_copy_key, kv_client_kill, kv_config_set, and kv_command (introspection verbs only). \
             Every one requires the user's explicit Allow on the exact operation; assume it may be \
             denied. Before a bulk kv_delete/kv_expire by pattern, scan first (kv_scan_keys) and \
             tell the user how many keys will be affected — a keyspace-wide delete or expire \
             (pattern `*`) is refused outright. Only write when the user has asked you to change \
             data."
        }
    };
    finish_system_prompt(
        format!(
            "You are RED's Redis agent, embedded in a native database explorer. You help the user \
             explore and understand the Redis server they are connected to.\n\n\
             {tools_line}\n\n\
             Call kv_server_info first — it tells you the topology, and a SCAN means something \
             different on a cluster. Call kv_key_schema before reasoning about what the keyspace \
             holds; the key template is the schema.\n\n\
             Redis keys are addressed by glob patterns (e.g. `user:*`), not SQL — there are no \
             tables or joins. Be concise: lead with the answer, then the supporting detail. When \
             you show a command, put it in a fenced ```sh block (e.g. `redis-cli GET foo`).\n",
        ),
        ctx,
        "This connection is READ-ONLY.",
    )
}

/// Curate the giant INFO reply down to the fields that matter, plus a computed
/// hit rate and the per-database key counts.
fn kv_info_summary(info: &str) -> String {
    let map: HashMap<&str, &str> = info
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once(':'))
        .collect();
    let get = |k: &str| map.get(k).copied().unwrap_or("?");
    let hits: f64 = get("keyspace_hits").parse().unwrap_or(0.0);
    let misses: f64 = get("keyspace_misses").parse().unwrap_or(0.0);
    let hit_rate = if hits + misses > 0.0 {
        format!("{:.1}%", hits / (hits + misses) * 100.0)
    } else {
        "n/a".to_string()
    };
    let mut s = String::new();
    s.push_str(&format!(
        "Redis {} ({}), uptime {} days\n",
        get("redis_version"),
        get("redis_mode"),
        get("uptime_in_days"),
    ));
    s.push_str(&format!(
        "Memory: {} used, maxmemory {} (policy {}), fragmentation {}\n",
        get("used_memory_human"),
        get("maxmemory_human"),
        get("maxmemory_policy"),
        get("mem_fragmentation_ratio"),
    ));
    s.push_str(&format!(
        "Clients: {} connected · {} ops/sec\n",
        get("connected_clients"),
        get("instantaneous_ops_per_sec"),
    ));
    s.push_str(&format!(
        "Hit rate: {hit_rate} ({} hits / {} misses) · evicted {} · expired {}\n",
        get("keyspace_hits"),
        get("keyspace_misses"),
        get("evicted_keys"),
        get("expired_keys"),
    ));
    let dbs: Vec<&str> = info
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("db") && l.contains("keys="))
        .collect();
    if !dbs.is_empty() {
        s.push_str("Keyspace:\n");
        for db in dbs {
            s.push_str(&format!("  {db}\n"));
        }
    }
    s
}

/// Format inferred key templates as the keyspace's schema. The sample size and
/// whether it was exhaustive lead, because every number below is only as good as
/// the walk that produced it and a truncated sample reads as fact otherwise.
fn kv_format_templates(
    templates: &[KeyTemplate],
    sampled: usize,
    total_keys: u64,
    exhausted: bool,
) -> String {
    if templates.is_empty() {
        return "No keys matched, so there is no key structure to report.".to_string();
    }
    let scope = if exhausted {
        format!("all {sampled} key(s)")
    } else {
        format!("a truncated sample of {sampled} key(s) of ~{total_keys} in the database")
    };
    let mut s = format!("Key templates inferred from {scope}:\n");
    for t in templates {
        let types = t
            .types
            .iter()
            .map(|(label, n)| {
                if t.types.len() == 1 {
                    label.clone()
                } else {
                    format!("{label} x{n}")
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        let avg = t.bytes / t.count.max(1);
        let ttl = if t.with_ttl == 0 {
            "no TTL".to_string()
        } else {
            format!(
                "TTL on {:.0}% (~{})",
                t.with_ttl as f64 / t.count as f64 * 100.0,
                kv_ttl(t.median_ttl),
            )
        };
        s.push_str(&format!(
            "  {}  {} keys  {types}  avg ~{}  {ttl}\n",
            t.pattern,
            t.count,
            fmt_bytes(avg),
        ));
    }
    if !exhausted {
        s.push_str(
            "(the sample stopped at a bound, so counts are proportions of the sample, not the \
             whole keyspace)\n",
        );
    }
    s
}

/// Format a [`RedisAnalysis`] as compact text for the agent.
fn kv_format_analysis(r: &red_core::kv::RedisAnalysis) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Sampled {} of {} keys ({}), ~{} total.\n",
        r.sampled,
        r.total_keys,
        if r.truncated {
            "truncated sample"
        } else {
            "full walk"
        },
        fmt_bytes(r.total_bytes),
    ));
    s.push_str("By type (memory):\n");
    for t in &r.types {
        s.push_str(&format!(
            "  {}: {} keys, ~{}\n",
            t.kv_type,
            t.count,
            fmt_bytes(t.bytes),
        ));
    }
    s.push_str("Top namespaces (memory):\n");
    for n in r.namespaces.iter().take(15) {
        s.push_str(&format!(
            "  {}: {} keys, ~{}\n",
            n.prefix,
            n.count,
            fmt_bytes(n.bytes),
        ));
    }
    let t = &r.ttl;
    s.push_str(&format!(
        "TTL: {} persistent (no expiry), {} with a TTL (<1h {}, <1d {}, <1w {}, >1w {})\n",
        t.persistent,
        t.with_ttl(),
        t.under_hour,
        t.under_day,
        t.under_week,
        t.over_week,
    ));
    s
}

/// Preview a [`KvValue`]: a string's contents, or a bounded element preview of a
/// collection. Large collections report their length, not their contents.
fn fmt_kv_value(v: &KvValue) -> String {
    fn coll<T>(kind: &str, c: &KvCollection<T>, fmt: impl Fn(&T) -> String) -> String {
        match c {
            KvCollection::Loaded(items) => {
                let shown = items.len().min(KV_VALUE_ELEMS);
                let mut out = format!("{kind} with {} element(s):\n", items.len());
                for it in items.iter().take(shown) {
                    out.push_str(&format!("  {}\n", fmt(it)));
                }
                if items.len() > shown {
                    out.push_str(&format!("  … {} more\n", items.len() - shown));
                }
                out
            }
            KvCollection::Large { len } => {
                format!("{kind} with {len} element(s) (large; browse it to page the contents)")
            }
        }
    }
    match v {
        KvValue::Str(val) => format!("string: {}", render_cell(val)),
        KvValue::Hash(c) => coll("hash", c, |(f, val)| format!("{f} => {val}")),
        KvValue::Set(c) => coll("set", c, |m| m.clone()),
        KvValue::ZSet(c) => coll("zset", c, |(m, score)| format!("{m} ({score})")),
        KvValue::List(c) => coll("list", c, |m| m.clone()),
        KvValue::Stream(c) => match c {
            KvCollection::Loaded(entries) => format!("stream with {} entr(ies)", entries.len()),
            KvCollection::Large { len } => format!("stream with {len} entr(ies) (large)"),
        },
        KvValue::Unsupported(kt) => format!("(no value preview for type {})", kt.label()),
    }
}

/// A RESP scalar as plain text (for CONFIG GET pairs).
fn resp_scalar(v: Option<&RespValue>) -> String {
    match v {
        Some(RespValue::Bulk(s)) | Some(RespValue::Simple(s)) => s.clone(),
        Some(RespValue::Int(i)) => i.to_string(),
        Some(other) => format!("{other:?}"),
        None => String::new(),
    }
}

/// `"no expiry"` or a coarse remaining-time for a key's TTL.
fn kv_ttl(ttl: Option<Duration>) -> String {
    match ttl {
        None => "no expiry".to_string(),
        Some(d) => {
            let s = d.as_secs();
            if s < 60 {
                format!("{s}s")
            } else if s < 3600 {
                format!("{}m", s / 60)
            } else if s < 86_400 {
                format!("{}h", s / 3600)
            } else {
                format!("{}d", s / 86_400)
            }
        }
    }
}

/// Coarse human byte count for the agent's text output, shared by every seam's
/// formatter.
fn fmt_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if n >= MB {
        format!("{:.1}MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1}KB", n as f64 / KB as f64)
    } else {
        format!("{n}B")
    }
}

pub(crate) async fn run_tool(
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
    content.push_str("\n…(result truncated: it exceeded the size cap; narrow the query)");
    content
}

/// A conservative read-only gate: the statement must be a single SELECT or a CTE
/// that resolves to a SELECT, with no statement separator and no embedded write.
///
/// `run_select` runs on the *user's* connection, which is writable unless the
/// connection itself was opened read-only, so this gate, not the engine, is what
/// keeps a read-tier agent from mutating data. A naive "starts with SELECT/WITH"
/// check is not enough: Postgres executes **data-modifying CTEs**
/// (`WITH x AS (DELETE … RETURNING …) SELECT * FROM x`), and `SELECT … INTO` /
/// `INTO OUTFILE` and sequence-advancing functions also write while leading with
/// SELECT. So, like [`write_shape`], we reason about a **noise-stripped** copy
/// (literals/quoted-identifiers/comments blanked) and reject any surviving write
/// keyword. The stripping, the whole-word test, and both token lists live in
/// `red_core::sql`, so this gate, the UI's `is_read_only`, and [`write_shape`] cannot
/// drift apart. False positives (a rejected legitimate read) are acceptable: the user can
/// always run such a query by hand in a query tab. (Defense in depth: opening the
/// AI's reads on an engine-level read-only connection would make this belt-and-
/// suspenders: a worthwhile follow-up, but it needs a per-call driver seam.)
fn is_read_only_select(sql: &str, dialect: Dialect) -> bool {
    let stripped = strip_noise(sql, dialect);
    let trimmed = stripped.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return false;
    }
    // No embedded statement terminator (a `;` could chain a write past the prefix).
    if trimmed.contains(';') {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("select") || lower.starts_with("with")) {
        return false;
    }
    // A statement that *starts* SELECT/WITH can still write. Reject if any write
    // keyword survives noise-stripping as a whole-word token: the data-modifying
    // CTE verbs (Postgres runs these), `INTO` (`SELECT … INTO new_table` /
    // `INTO OUTFILE`/`DUMPFILE`), and the sequence-advancing functions. These verbs
    // are reserved words, so they can't be bare column names in a real read; a
    // column legitimately named one of them would be quoted, and quoting blanks it
    // out before this check. (`FOR UPDATE` locking reads trip `update` and are
    // rejected too; fine, the assistant browses, it doesn't lock.)
    !WRITE_TOKENS
        .iter()
        .chain(DANGEROUS_FNS)
        .any(|w| has_word(&lower, w))
}

/// The tools that never mutate data and so may run on any backend without the
/// per-call write gate. This is an allowlist on purpose: anything *not* named here
/// is treated as a write, so a future tool fails *closed* (gated, withheld from the
/// MCP/ACP path) until it's explicitly vetted and added, rather than slipping
/// through a denylist someone forgot to extend.
pub(crate) const READ_ONLY_TOOLS: &[&str] = &[
    "list_schema",
    "describe_table",
    "relationship_map",
    "object_ddl",
    "profile_table",
    "run_select",
    "search_data",
    // Plans; with `analyze` it also *runs* the statement, which is why that
    // branch refuses anything `risk::assess` grades above Safe.
    "explain",
    "health_report",
    "server_sessions",
    "diff_schema",
    "diff_data",
    "suggest_index",
    "export_result",
    "generate_report",
    // Hands the user a SQL query to open in a tab; no DB mutation of its own.
    "open_query",
    // Writes a `.sql` file to the user's saved-queries library; no DB mutation.
    "save_query",
    // Redis (KV) read tools: pure reads through the `KvDriver` seam.
    "kv_server_info",
    "kv_scan_keys",
    "kv_key_info",
    "kv_key_schema",
    "kv_get_value",
    "kv_read_collection",
    "kv_stream_groups",
    "kv_biggest_keys",
    "kv_analyze",
    "kv_slowlog",
    "kv_client_list",
    "kv_config_get",
    "kv_keyspace_notifications",
    // MongoDB (doc) read tools: pure reads through the `DocDriver` seam. The
    // signature tools (`profile_collection`/`audit_collection`/`index_advice`)
    // are host-side compositions over the read methods, so they're reads too.
    "doc_server_info",
    "list_collections",
    "describe_collection",
    "doc_reference_map",
    "profile_collection",
    "sample_documents",
    "get_document",
    "find",
    "aggregate",
    "count",
    "distinct",
    "explain_query",
    "index_advice",
    "audit_collection",
    "doc_current_op",
];

/// Whether `name` is a mutating tool: it never auto-runs and never auto-allows;
/// it rides the per-call approval gate on both backends. Defined as the
/// complement of [`READ_ONLY_TOOLS`] so a new, unlisted tool is treated as a write.
pub(crate) fn is_write_tool(name: &str) -> bool {
    !READ_ONLY_TOOLS.contains(&name)
}

/// Tools that don't mutate the database but assume a running GUI: they emit UI
/// events (`open_query` opens a tab) or write into the app's on-disk libraries
/// (`save_query`, `generate_report`) for the app to surface. They're meaningless
/// over the headless `red mcp` stdio transport, so that path drops them from the
/// advertised catalog and refuses a call to them.
pub(crate) const UI_ONLY_TOOLS: &[&str] = &[
    "open_query",
    "save_query",
    "generate_report",
    // Writes a file into the app's output folder and announces it as a card for
    // the user to open. Over a headless transport there is nobody to hand it to,
    // and the folder is the app's, not the caller's.
    "export_result",
];

/// Whether `name` may run over the headless `red mcp` transport: a read-only tool
/// that isn't one of the GUI-only [`UI_ONLY_TOOLS`]. Writes are already excluded
/// by [`is_write_tool`]; this additionally drops the UI-bound reads.
pub(crate) fn is_headless_tool(name: &str) -> bool {
    !is_write_tool(name) && !UI_ONLY_TOOLS.contains(&name)
}

/// The outcome of vetting a `propose_write` call before it runs. The
/// single source of truth, called by `run_turn` (to decide reject vs. prompt) and
/// by `run_tool` (to re-validate before executing). Keeping it in one place means
/// the gate the user sees and the gate the write rides can't drift apart.
pub(crate) enum WriteAssessment {
    /// Not a write tool; run it normally (no approval).
    NotWrite,
    /// Blocked outright (wrong tier, read-only connection, or a destructive shape):
    /// report this to the model without prompting the user.
    Reject(String),
    /// An allowed single INSERT/UPDATE/DELETE: prompt the user with this exact SQL,
    /// and only run it on Allow.
    NeedsApproval { sql: String },
}

/// Vet a tool call for the write gate. A `propose_write` is allowed only at the
/// `Write` tier, on a writable connection, and for a safe statement shape; anything
/// else is rejected (never silently run, never even prompted).
pub(crate) fn assess_write(
    name: &str,
    input: &Json,
    policy: &AiPolicy,
    dialect: Dialect,
) -> WriteAssessment {
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
            "this connection is read-only: writes are disabled. Tell the user; do not retry."
                .into(),
        );
    }
    if is_kv_write_tool(name) {
        return assess_kv_write(name, input);
    }
    if is_doc_write_tool(name) {
        return assess_doc_write(name, input);
    }
    if name == "propose_changeset" {
        return assess_changeset(input, dialect);
    }
    // Not SQL, so `write_shape` has nothing to lex: a kill is graded by what it
    // stops, and the prompt has to say what that is.
    if name == "kill_session" {
        return assess_kill_session(input);
    }
    // The one DDL the agent may run. It is deliberately carved out of
    // `write_shape`'s blanket DDL block rather than loosening it: an index is
    // additive and reversible, a DROP/TRUNCATE/ALTER is not, and widening the
    // block would let all three through.
    if name == "create_index" {
        return match index_args(input) {
            Ok((table, index, columns, unique)) => WriteAssessment::NeedsApproval {
                sql: format!(
                    "CREATE{} INDEX {index} ON {} ({})\nBuilding an index locks and loads the \
                     server for the duration; it is reversible with a DROP INDEX afterwards.",
                    if unique { " UNIQUE" } else { "" },
                    qualified(table.schema.as_deref(), &table.name),
                    columns.join(", "),
                ),
            },
            Err(why) => WriteAssessment::Reject(why),
        };
    }
    let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
    match write_shape(sql, dialect) {
        WriteShape::Ok => WriteAssessment::NeedsApproval {
            sql: sql.to_string(),
        },
        WriteShape::NotWrite => WriteAssessment::Reject(
            "propose_write is only for INSERT/UPDATE/DELETE; use run_select to read".into(),
        ),
        WriteShape::Blocked(why) => WriteAssessment::Reject(why.into()),
    }
}

/// Vet a `kill_session` for the approval gate. Not a statement, so there is no
/// shape to lex; what matters is that the prompt names the *target* — the
/// session, whose it is, and what it is running — because "terminate session
/// 4711" alone is not something anyone can meaningfully approve.
fn assess_kill_session(input: &Json) -> WriteAssessment {
    let Some(key) = input
        .get("key")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty())
    else {
        return WriteAssessment::Reject(
            "kill_session needs the `key` of a session from server_sessions".into(),
        );
    };
    let mode = match kill_mode(input) {
        Ok(m) => m,
        Err(why) => return WriteAssessment::Reject(why),
    };
    let who = input
        .get("user")
        .and_then(Json::as_str)
        .filter(|u| !u.is_empty())
        .map(|u| format!(" (user {u})"))
        .unwrap_or_default();
    let mut op = format!("{} `{key}`{who}", mode.verb());
    if mode == red_core::KillMode::Terminate {
        op.push_str("\n\u{26a0} Terminating rolls back this session's open transaction.");
    }
    match input
        .get("statement")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(sql) => op.push_str(&format!("\nRunning: {}", truncate_summary(sql, 300))),
        None => op.push_str(
            "\nThe agent did not say what this session is running; read server_sessions before \
             allowing.",
        ),
    }
    WriteAssessment::NeedsApproval { sql: op }
}

/// The Redis mutating tools (KV backend): each rides the same per-call
/// approval gate as a SQL write.
const KV_WRITE_TOOLS: &[&str] = &[
    "kv_set",
    "kv_expire",
    "kv_delete",
    "kv_rename",
    "kv_copy_key",
    "kv_config_set",
    // Not a keyspace write, but a server-state one that rides the same gate.
    "kv_client_kill",
    // Read-only by allowlist, but classified as a write on purpose: this is the
    // one tool whose safety rests entirely on that allowlist, so the fail-closed
    // default (approval required, never advertised headlessly) protects it too.
    "kv_command",
];

/// The command verbs `kv_command` may run. Introspection only: nothing here
/// reads a key's value, writes anything, or reconfigures the server, so the tool
/// cannot be used to route around `kv_get_value`'s tier gate or any write gate.
const KV_COMMAND_ALLOWLIST: &[&str] = &[
    "INFO", "MEMORY", "OBJECT", "TYPE", "TTL", "PTTL", "EXISTS", "STRLEN", "LATENCY", "COMMAND",
    "DBSIZE", "LASTSAVE", "TIME", "ROLE",
];

/// The `argv` of a `kv_command` call: the non-empty string entries, in order.
fn kv_command_argv(input: &Json) -> Vec<String> {
    input
        .get("argv")
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Json::as_str)
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `argv` names a command `kv_command` may run. The verb must be on
/// [`KV_COMMAND_ALLOWLIST`], and there must be exactly one of them: Redis has no
/// in-command chaining, but refusing anything unexpected is the posture this
/// tool only exists under.
fn kv_allowed_command(argv: &[String]) -> Result<(), String> {
    let Some(verb) = argv.first() else {
        return Err("kv_command needs a non-empty `argv`".into());
    };
    let upper = verb.to_ascii_uppercase();
    if !KV_COMMAND_ALLOWLIST.contains(&upper.as_str()) {
        return Err(format!(
            "`{verb}` is not on kv_command's allowlist ({}). Use the dedicated tool for what you \
             need; this one is introspection only.",
            KV_COMMAND_ALLOWLIST.join(", ")
        ));
    }
    Ok(())
}

fn is_kv_write_tool(name: &str) -> bool {
    KV_WRITE_TOOLS.contains(&name)
}

/// Whether a Redis glob is (near) keyspace-wide: it carries no literal anchoring
/// character, so it matches essentially every key. `*`, `**`, `?*`, and `[a-z]*`
/// all qualify; `user:*` does not (the `user:` literal anchors it). A destructive
/// write over such a pattern is refused even with approval, so the keyspace-wide
/// guard can't be evaded by an equivalent glob.
fn matches_whole_keyspace(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    let mut in_class = false;
    let mut has_literal = false;
    while let Some(c) = chars.next() {
        match c {
            // An escaped character is a literal anchor.
            '\\' => has_literal |= chars.next().is_some(),
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            // Class contents are a wildcard set, not an anchor.
            _ if in_class => {}
            // Wildcards provide no selectivity.
            '*' | '?' => {}
            _ => has_literal = true,
        }
    }
    !has_literal
}

/// Vet a Redis write tool for the approval gate: build the human-readable
/// operation shown in the Allow/Deny prompt, and hard-block the catastrophic
/// shapes (a keyspace-wide DELETE or EXPIRE) even with approval — mirroring the
/// SQL gate's refusal of an unqualified UPDATE/DELETE. Tier + read-only were
/// already checked by [`assess_write`].
/// The explicit key targets of a `kv_delete` input: `key` and `keys` combined,
/// in that order. The one accumulation both the approval prompt and the executor
/// use, so what the user approves is exactly what gets deleted.
fn kv_delete_targets(input: &Json) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    if let Some(k) = input
        .get("key")
        .and_then(Json::as_str)
        .filter(|k| !k.is_empty())
    {
        targets.push(k.to_string());
    }
    if let Some(arr) = input.get("keys").and_then(Json::as_array) {
        targets.extend(arr.iter().filter_map(|v| v.as_str()).map(str::to_string));
    }
    targets
}

/// Ceiling on one `kv_set` value (a string body, a hash/stream field value, a
/// set/list member). A multi-megabyte value in a tool call is a context problem
/// before it is a Redis one — it has already crossed the wire to the provider and
/// back — so refuse it here and tell the model to write it by hand.
const KV_SET_VALUE_MAX: usize = 64 * 1024;
/// Ceiling on all of one `kv_set` call's values combined.
const KV_SET_PAYLOAD_MAX: usize = 256 * 1024;
/// Ceiling on the elements (hash fields, set/zset members, list values, stream
/// fields) one `kv_set` call may write. The per-element writers are one round
/// trip each, so this bounds the call's cost as well as the prompt's length.
const KV_SET_ELEMS_MAX: usize = 1_000;
/// How much of a `kv_set` command list the approval prompt shows before it is
/// truncated. Long enough for a realistic collection write, short enough that a
/// thousand-member payload cannot push the key name off the top of the dialog.
const KV_SET_PROMPT_CHARS: usize = 800;

/// The value shape a [`KvSetPlan`] writes, one variant per Redis type. Each maps
/// to the `KvDriver` writer for that type, so an unwritable combination (a score
/// on a plain set, a field on a list) cannot be represented.
enum KvSetBody {
    /// `SET`, whose own expiry option replaces a separate `PEXPIRE`.
    Str { value: String, ttl: StringTtl },
    /// `HSET` per field.
    Hash(Vec<(String, String)>),
    /// One `SADD` with every member.
    Set(Vec<String>),
    /// `ZADD` per `(member, score)`.
    ZSet(Vec<(String, f64)>),
    /// `RPUSH` per value.
    List(Vec<String>),
    /// One `XADD … *` with the entry's fields.
    Stream(Vec<(String, String)>),
}

/// One parsed, validated `kv_set` call: the typed `KvDriver` writes it will
/// perform, in order.
///
/// Built once by [`kv_set_plan`] and used by *both* the approval prompt (which
/// renders [`KvSetPlan::commands`]) and the executor, so what the user allows is
/// exactly what runs — the contract the SQL path gets for free by showing the
/// statement itself, and the one `kv_delete`'s shared target list restored here.
struct KvSetPlan {
    key: String,
    /// `DEL` the key before writing. Redis has no "replace this whole
    /// collection" primitive, so `mode: "set"` over a hash/set/zset/list is a
    /// delete and a rebuild; it is rendered in the prompt rather than implied.
    clear_first: bool,
    body: KvSetBody,
    /// Applied after the body as a `PEXPIRE`. The string form carries its expiry
    /// inside `SET` instead, so this stays `None` there.
    expire: Option<Duration>,
}

impl KvSetPlan {
    /// The commands this plan runs, in order, as `redis-cli` would echo them.
    /// The approval prompt's body and the executor's script are the same list.
    fn commands(&self) -> Vec<String> {
        let key = resp_arg(&self.key);
        let mut out = Vec::new();
        if self.clear_first {
            out.push(format!("DEL {key}"));
        }
        match &self.body {
            KvSetBody::Str { value, ttl } => {
                let mut cmd = format!("SET {key} {}", resp_arg(value));
                if let StringTtl::Set(d) = ttl {
                    cmd.push_str(&format!(" PX {}", kv_millis(*d)));
                }
                out.push(cmd);
            }
            KvSetBody::Hash(fields) => out.extend(
                fields
                    .iter()
                    .map(|(f, v)| format!("HSET {key} {} {}", resp_arg(f), resp_arg(v))),
            ),
            KvSetBody::Set(members) => {
                let args: Vec<String> = members.iter().map(|m| resp_arg(m)).collect();
                out.push(format!("SADD {key} {}", args.join(" ")));
            }
            KvSetBody::ZSet(members) => out.extend(
                members
                    .iter()
                    .map(|(m, score)| format!("ZADD {key} {score} {}", resp_arg(m))),
            ),
            KvSetBody::List(values) => out.extend(
                values
                    .iter()
                    .map(|v| format!("RPUSH {key} {}", resp_arg(v))),
            ),
            KvSetBody::Stream(fields) => {
                let args: Vec<String> = fields
                    .iter()
                    .flat_map(|(f, v)| [resp_arg(f), resp_arg(v)])
                    .collect();
                out.push(format!("XADD {key} * {}", args.join(" ")));
            }
        }
        if let Some(d) = self.expire {
            out.push(format!("PEXPIRE {key} {}", kv_millis(d)));
        }
        out
    }
}

/// A `Duration` as the whole milliseconds the `PX`/`PEXPIRE` writers send. Both
/// clamp to at least 1, since a zero-millisecond expiry deletes the key on
/// arrival, which is never what a `ttl_seconds` of a fraction meant.
fn kv_millis(d: Duration) -> u64 {
    (d.as_millis() as u64).max(1)
}

/// Render one command argument the way `redis-cli` echoes it: bare when it is a
/// simple token, double-quoted with escapes otherwise. The approval prompt is a
/// reading aid, so this only has to be unambiguous — an empty, spaced, or
/// newline-bearing value must not silently blend into the command around it.
fn resp_arg(s: &str) -> String {
    let simple = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '.' | '/' | '@' | '+' | '#')
        });
    if simple {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Coerce one JSON scalar to the string Redis stores. Numbers and booleans are
/// accepted (a model writing `{"value": 42}` means the string `"42"`); a nested
/// object or array is not, because silently serializing one to JSON would write
/// a shape the user never approved reading back as a document.
fn kv_scalar(v: &Json, what: &str) -> Result<String, String> {
    match v {
        Json::String(s) => Ok(s.clone()),
        Json::Number(n) => Ok(n.to_string()),
        Json::Bool(b) => Ok(b.to_string()),
        _ => Err(format!(
            "{what} must be a string, number, or boolean; Redis stores bytes, so encode a \
             document yourself if that is what you mean"
        )),
    }
}

/// Parse and validate a `kv_set` call into the plan both the prompt and the
/// executor use. Every refusal here is a `Reject` the model can act on, never a
/// prompt: an oversized payload or an unwritable shape is not something a user
/// should be asked to rubber-stamp.
fn kv_set_plan(input: &Json) -> Result<KvSetPlan, String> {
    let key = input
        .get("key")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or("kv_set needs a non-empty `key`")?
        .to_string();
    // A key with no literal character is a glob the model mistook for a key name.
    // Writing it would create a key literally called `*`, which is worse than the
    // error: it collides with every pattern the user later types.
    if matches_whole_keyspace(&key) {
        return Err(format!(
            "`{key}` is a glob pattern, not a key: kv_set writes one exact key. Scan first if you \
             meant to find keys."
        ));
    }
    let kind = input
        .get("type")
        .and_then(Json::as_str)
        .map(str::trim)
        .unwrap_or("");
    let value = input.get("value");
    let mode_replace = match input.get("mode").and_then(Json::as_str).unwrap_or("set") {
        "set" => true,
        "append" => false,
        other => {
            return Err(format!(
                "kv_set `mode` must be \"set\" or \"append\", not `{other}`"
            ));
        }
    };
    let ttl = match input.get("ttl_seconds").and_then(Json::as_i64) {
        Some(s) if s > 0 => Some(Duration::from_secs(s as u64)),
        _ => None,
    };

    // `{ field: value }` pairs from a JSON object, in the order the model wrote
    // them (serde_json preserves insertion order), so the prompt reads the way
    // the call did.
    let pairs = |what: &str| -> Result<Vec<(String, String)>, String> {
        let obj = value.and_then(Json::as_object).ok_or_else(|| {
            format!("kv_set on a {kind} needs `value` as a JSON object of {what}")
        })?;
        obj.iter()
            .map(|(k, v)| Ok((k.clone(), kv_scalar(v, "each value")?)))
            .collect()
    };
    // A list of scalars from a JSON array, or a lone scalar treated as a
    // one-element list (the shape a model reaches for when adding one member).
    let members = || -> Result<Vec<String>, String> {
        match value {
            Some(Json::Array(items)) => items
                .iter()
                .map(|v| kv_scalar(v, "each member"))
                .collect::<Result<Vec<_>, _>>(),
            Some(v) => Ok(vec![kv_scalar(v, "`value`")?]),
            None => Err(format!("kv_set on a {kind} needs `value`")),
        }
    };

    let (body, clear_first) = match kind {
        "string" => {
            let v = value.ok_or("kv_set on a string needs `value`")?;
            let body = KvSetBody::Str {
                value: kv_scalar(v, "`value`")?,
                // `SET` carries its own expiry; a plain `SET` clears any existing
                // one, which is Redis's own semantics and is what the rendered
                // command says.
                ttl: ttl.map_or(StringTtl::Clear, StringTtl::Set),
            };
            (body, false)
        }
        "hash" => match input
            .get("field")
            .and_then(Json::as_str)
            .filter(|f| !f.is_empty())
        {
            // The single-field form is an upsert of that one field, so it never
            // clears the rest of the hash regardless of `mode`.
            Some(field) => {
                let v = value.ok_or("kv_set with a `field` needs `value`")?;
                (
                    KvSetBody::Hash(vec![(field.to_string(), kv_scalar(v, "`value`")?)]),
                    false,
                )
            }
            None => (KvSetBody::Hash(pairs("field/value")?), mode_replace),
        },
        "set" => (KvSetBody::Set(members()?), mode_replace),
        "zset" => {
            let obj = value.and_then(Json::as_object).ok_or(
                "kv_set on a zset needs `value` as a JSON object of member/score, e.g. \
                 { \"ada\": 1.5 }",
            )?;
            let scored = obj
                .iter()
                .map(|(m, s)| {
                    s.as_f64()
                        .filter(|f| f.is_finite())
                        .map(|score| (m.clone(), score))
                        .ok_or_else(|| format!("zset score for `{m}` must be a finite number"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            (KvSetBody::ZSet(scored), mode_replace)
        }
        "list" => (KvSetBody::List(members()?), mode_replace),
        // A stream is an append-only log and `value` describes ONE entry, so
        // there is no "the stream is these entries" shape to replace with;
        // `mode` is deliberately inert here rather than silently wiping a log.
        "stream" => (KvSetBody::Stream(pairs("field/value")?), false),
        "" => return Err("kv_set needs `type` (string/hash/set/zset/list/stream)".into()),
        other => {
            return Err(format!(
                "kv_set `type` must be string/hash/set/zset/list/stream, not `{other}`"
            ));
        }
    };

    let size = kv_set_size(&body);
    if size.elems == 0 {
        return Err(format!("kv_set on a {kind} needs at least one value"));
    }
    if size.elems > KV_SET_ELEMS_MAX {
        return Err(format!(
            "kv_set is capped at {KV_SET_ELEMS_MAX} elements per call ({} given); split it",
            size.elems
        ));
    }
    if size.largest > KV_SET_VALUE_MAX {
        return Err(format!(
            "a single kv_set value is {} bytes, over the {KV_SET_VALUE_MAX}-byte cap; write a \
             value this large by hand",
            size.largest
        ));
    }
    if size.bytes > KV_SET_PAYLOAD_MAX {
        return Err(format!(
            "kv_set payload is {} bytes, over the {KV_SET_PAYLOAD_MAX}-byte cap; split it",
            size.bytes
        ));
    }
    Ok(KvSetPlan {
        key,
        clear_first,
        // The string form folds its expiry into `SET`; every other type needs the
        // separate `PEXPIRE`.
        expire: match body {
            KvSetBody::Str { .. } => None,
            _ => ttl,
        },
        body,
    })
}

/// What the payload guards measure: the values a `kv_set` call writes, not the
/// rendered command text. `largest` is kept alongside `bytes` because one 10 MB
/// string and ten thousand small members are different problems with different
/// advice.
struct KvSetSize {
    elems: usize,
    bytes: usize,
    largest: usize,
}

fn kv_set_size(body: &KvSetBody) -> KvSetSize {
    let measure = |lengths: &[usize]| KvSetSize {
        elems: lengths.len(),
        bytes: lengths.iter().sum(),
        largest: lengths.iter().copied().max().unwrap_or(0),
    };
    let pairs = |v: &[(String, String)]| {
        measure(&v.iter().map(|(f, s)| f.len() + s.len()).collect::<Vec<_>>())
    };
    match body {
        KvSetBody::Str { value, .. } => measure(&[value.len()]),
        KvSetBody::Hash(f) | KvSetBody::Stream(f) => pairs(f),
        KvSetBody::Set(m) | KvSetBody::List(m) => {
            measure(&m.iter().map(String::len).collect::<Vec<_>>())
        }
        KvSetBody::ZSet(m) => measure(&m.iter().map(|(s, _)| s.len()).collect::<Vec<_>>()),
    }
}

/// Run an approved [`KvSetPlan`] against the driver, one typed write per element.
/// A mid-plan failure reports how far it got: Redis has no transaction here, so
/// claiming nothing happened would be a lie.
async fn kv_apply_set(driver: &Arc<dyn KvDriver>, plan: &KvSetPlan) -> (String, bool) {
    let key = plan.key.as_str();
    if plan.clear_first
        && let Err(e) = driver.delete_keys(std::slice::from_ref(&plan.key)).await
    {
        return (format!("error: clearing `{key}` failed: {e}"), false);
    }
    let mut done = 0usize;
    let failed =
        |n: usize, e: RedError| (format!("error: after {n} write(s) to `{key}`: {e}"), false);
    match &plan.body {
        KvSetBody::Str { value, ttl } => {
            if let Err(e) = driver.set_string(key, value.clone(), *ttl).await {
                return failed(0, e);
            }
            done = 1;
        }
        KvSetBody::Hash(fields) => {
            for (field, value) in fields {
                if let Err(e) = driver.set_field(key, field, value.clone()).await {
                    return failed(done, e);
                }
                done += 1;
            }
        }
        KvSetBody::Set(members) => match driver.set_add(key, members).await {
            Ok(_) => done = members.len(),
            Err(e) => return failed(0, e),
        },
        KvSetBody::ZSet(members) => {
            for (member, score) in members {
                if let Err(e) = driver.zset_add(key, member, *score).await {
                    return failed(done, e);
                }
                done += 1;
            }
        }
        KvSetBody::List(values) => {
            for value in values {
                if let Err(e) = driver.list_push(key, value.clone(), false).await {
                    return failed(done, e);
                }
                done += 1;
            }
        }
        KvSetBody::Stream(fields) => match driver.stream_add(key, fields).await {
            Ok(_) => done = fields.len(),
            Err(e) => return failed(0, e),
        },
    }
    if let Some(d) = plan.expire
        && let Err(e) = driver.set_ttl(key, Some(d)).await
    {
        return (
            format!("error: wrote `{key}` but setting its expiry failed: {e}"),
            false,
        );
    }
    (format!("Wrote `{key}` ({done} element(s))."), true)
}

fn assess_kv_write(name: &str, input: &Json) -> WriteAssessment {
    let s = |k: &str| {
        input
            .get(k)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    match name {
        "kv_expire" => {
            let seconds = input.get("seconds").and_then(Json::as_i64);
            let target = match (s("key"), s("pattern")) {
                (Some(k), _) => format!("key `{k}`"),
                (None, Some(p)) => {
                    if matches_whole_keyspace(p) && seconds.is_some_and(|sec| sec > 0) {
                        return WriteAssessment::Reject(format!(
                            "refusing to set a TTL on the entire keyspace (pattern `{p}` matches \
                             essentially every key): this would expire every key. Narrow the \
                             pattern."
                        ));
                    }
                    format!("all keys matching `{p}`")
                }
                (None, None) => {
                    return WriteAssessment::Reject("kv_expire needs `key` or `pattern`".into());
                }
            };
            let action = match seconds {
                Some(sec) if sec > 0 => format!("EXPIRE {target} in {sec}s"),
                _ => format!("PERSIST {target} (remove any expiry)"),
            };
            WriteAssessment::NeedsApproval { sql: action }
        }
        "kv_delete" => {
            // Built from the SAME accumulation the executor deletes
            // (`kv_delete_targets`): the input schema permits `key` and `keys`
            // simultaneously, so a prompt built from `key` alone would show one
            // key while everything in `keys` rides along unseen.
            let targets = kv_delete_targets(input);
            if let [one] = targets.as_slice() {
                WriteAssessment::NeedsApproval {
                    sql: format!("DELETE key `{one}`"),
                }
            } else if !targets.is_empty() {
                WriteAssessment::NeedsApproval {
                    sql: format!("DELETE {} key(s): {}", targets.len(), targets.join(", ")),
                }
            } else if let Some(p) = s("pattern") {
                if matches_whole_keyspace(p) {
                    return WriteAssessment::Reject(format!(
                        "refusing to DELETE the entire keyspace (pattern `{p}` matches essentially \
                         every key): use FLUSHDB by hand if that's really intended. Narrow the \
                         pattern."
                    ));
                }
                WriteAssessment::NeedsApproval {
                    sql: format!("DELETE all keys matching `{p}`"),
                }
            } else {
                WriteAssessment::Reject("kv_delete needs `key`, `keys`, or `pattern`".into())
            }
        }
        "kv_set" => match kv_set_plan(input) {
            // The prompt IS the command list: the user reads what will run, not a
            // paraphrase of it.
            Ok(plan) => WriteAssessment::NeedsApproval {
                sql: truncate_summary(&plan.commands().join("\n"), KV_SET_PROMPT_CHARS),
            },
            Err(why) => WriteAssessment::Reject(why),
        },
        "kv_copy_key" => match (s("from"), s("to")) {
            (Some(f), Some(t)) => {
                let replace = input
                    .get("replace")
                    .and_then(Json::as_bool)
                    .unwrap_or(false);
                let clobber = if replace {
                    format!(", OVERWRITING `{t}` if it already exists")
                } else {
                    format!(", refusing if `{t}` already exists")
                };
                WriteAssessment::NeedsApproval {
                    sql: format!("COPY key `{f}` to `{t}` (DUMP + RESTORE){clobber}"),
                }
            }
            _ => WriteAssessment::Reject("kv_copy_key needs `from` and `to`".into()),
        },
        "kv_command" => {
            let argv = kv_command_argv(input);
            match kv_allowed_command(&argv) {
                Ok(()) => WriteAssessment::NeedsApproval {
                    sql: argv
                        .iter()
                        .map(|a| resp_arg(a))
                        .collect::<Vec<_>>()
                        .join(" "),
                },
                Err(why) => WriteAssessment::Reject(why),
            }
        }
        "kv_client_kill" => match input.get("id").and_then(Json::as_i64) {
            Some(id) => {
                let mut op = format!("CLIENT KILL ID {id}");
                if let Some(addr) = s("addr") {
                    op.push_str(&format!(" ({addr})"));
                }
                match s("cmd") {
                    Some(cmd) => op.push_str(&format!("\nLast command: {cmd}")),
                    None => op.push_str(
                        "\nThe agent did not say what this client is doing; read kv_client_list \
                         before allowing.",
                    ),
                }
                WriteAssessment::NeedsApproval { sql: op }
            }
            None => WriteAssessment::Reject(
                "kv_client_kill needs the numeric `id` of a client from kv_client_list".into(),
            ),
        },
        "kv_rename" => match (s("from"), s("to")) {
            (Some(f), Some(t)) => WriteAssessment::NeedsApproval {
                sql: format!("RENAME `{f}` -> `{t}`"),
            },
            _ => WriteAssessment::Reject("kv_rename needs `from` and `to`".into()),
        },
        "kv_config_set" => match (s("parameter"), input.get("value").and_then(Json::as_str)) {
            // A CONFIG value may legitimately be empty (e.g. `save ""`), so `value`
            // isn't filtered for emptiness like the others.
            (Some(p), Some(v)) => {
                let mut op = format!("CONFIG SET {p} {v}");
                // Surface a danger note in the approval prompt for parameters
                // that relocate/toggle persistence or change auth: a single
                // Allow on `CONFIG SET dir` + `dbfilename` is the classic
                // RDB-write server-takeover, indistinguishable in the raw
                // command from a benign `maxmemory-policy` tweak.
                if is_dangerous_config_param(p) {
                    op.push_str(
                        "\n\u{26a0} This parameter can change where/how Redis persists data or \
                         its auth, and is a known server-takeover vector. Allow only if this \
                         exact value was intended.",
                    );
                }
                WriteAssessment::NeedsApproval { sql: op }
            }
            _ => WriteAssessment::Reject("kv_config_set needs `parameter` and `value`".into()),
        },
        other => WriteAssessment::Reject(format!("unknown KV write tool `{other}`")),
    }
}

/// CONFIG parameters that can relocate or toggle Redis persistence (the
/// `CONFIG SET dir` + `dbfilename` RDB-write takeover and its AOF equivalents)
/// or change authentication/exposure. A `CONFIG SET` of one of these gets a
/// danger note in the approval prompt so it can't be waved through as routine.
fn is_dangerous_config_param(param: &str) -> bool {
    const DANGEROUS: &[&str] = &[
        "dir",
        "dbfilename",
        "appendfilename",
        "appenddirname",
        "appendonly",
        "save",
        "requirepass",
        "masterauth",
        "masteruser",
        "aclfile",
        "unixsocket",
        "logfile",
        "pidfile",
        "bind",
        "protected-mode",
        "enable-debug-command",
        "enable-module-command",
    ];
    DANGEROUS.contains(&param.trim().to_ascii_lowercase().as_str())
}

/// CONFIG parameters whose *value* is a credential and must never be echoed back
/// to the model (a `CONFIG GET requirepass` exfiltration vector). Broader than an
/// exact list so a `*pass*`-shaped parameter can't slip a secret through.
fn is_secret_config_param(param: &str) -> bool {
    let p = param.trim().to_ascii_lowercase();
    matches!(p.as_str(), "requirepass" | "masterauth" | "masteruser") || p.contains("pass")
}

/// The statements of a `propose_changeset` call: the non-empty, trimmed entries of
/// its `statements` array, in order.
fn changeset_statements(input: &Json) -> Vec<String> {
    input
        .get("statements")
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Vet a `propose_changeset`: every statement must pass the same shape gate as a
/// single write (DML only, WHERE required, no DDL, no chaining). Any failure rejects
/// the *whole* changeset — it's atomic, so a bad statement means nothing runs. On
/// success the approval prompt shows the numbered statements as one reviewable unit.
fn assess_changeset(input: &Json, dialect: Dialect) -> WriteAssessment {
    let statements = changeset_statements(input);
    if statements.is_empty() {
        return WriteAssessment::Reject(
            "propose_changeset needs a non-empty `statements` array of INSERT/UPDATE/DELETE \
             statements"
                .into(),
        );
    }
    for (i, stmt) in statements.iter().enumerate() {
        match write_shape(stmt, dialect) {
            WriteShape::Ok => {}
            WriteShape::NotWrite => {
                return WriteAssessment::Reject(format!(
                    "statement {} is not an INSERT/UPDATE/DELETE; a changeset only modifies data",
                    i + 1
                ));
            }
            WriteShape::Blocked(why) => {
                return WriteAssessment::Reject(format!("statement {}: {why}", i + 1));
            }
        }
    }
    // Numbered, one per line: the exact set the user approves as a unit.
    let body = statements
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {s}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    WriteAssessment::NeedsApproval { sql: body }
}

/// The shape verdict for a candidate write statement.
enum WriteShape {
    /// A single, qualified INSERT/UPDATE/DELETE: eligible (still needs approval).
    Ok,
    /// Not a write at all (SELECT/WITH/empty).
    NotWrite,
    /// A shape blocked even with approval, with the reason to report.
    Blocked(&'static str),
}

/// Classify a candidate write conservatively. The hard blocks (DDL and
/// privilege statements, an unqualified UPDATE/DELETE with no WHERE, and any chained
/// statement) are the cases per-call approval alone shouldn't be trusted to catch
/// (a rubber-stamped `DELETE` with no WHERE is catastrophic). False negatives are
/// fine: the user can always run those by hand in a query tab.
///
/// Classification runs on a **noise-stripped** copy (string literals, quoted
/// identifiers, and comments blanked) so a keyword or `;` *inside a literal* can't
/// fool the gate; e.g. `UPDATE t SET note = 'see where'` (no real WHERE) is still
/// blocked, and a `;` inside a string isn't read as statement chaining.
fn write_shape(sql: &str, dialect: Dialect) -> WriteShape {
    let stripped = strip_noise(sql, dialect);
    let trimmed = stripped.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return WriteShape::Blocked("the statement is empty");
    }
    // No embedded terminator: a real `;` chains a second statement past the keyword
    // check (and past the user's eyes).
    if trimmed.contains(';') {
        return WriteShape::Blocked("multiple statements are not allowed; submit one at a time");
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
                    "an UPDATE/DELETE without a WHERE clause is blocked; add a WHERE, or run a \
                     full-table change yourself in a query tab",
                )
            }
        }
        // DROP / TRUNCATE / ALTER / CREATE / RENAME / GRANT / REVOKE / …: DDL and
        // privilege changes are never run through the assistant.
        _ => WriteShape::Blocked(
            "only INSERT/UPDATE/DELETE are allowed here; DDL (DROP/TRUNCATE/ALTER/…) must be run \
             manually in a query tab",
        ),
    }
}

/// The report shell's inline stylesheet: a neutral, light/dark base the model's
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

/// The report's base document style. With a `theme` (the active RED palette) the
/// page, tables and code blocks are painted in RED's colors and pinned to its
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
/// world-readable file would let another local user read it, so restrict it at
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
/// (`assets/report-renderer.js`). This is the ONLY code allowed to run in a report;
/// it is injected behind a per-report CSP nonce, so the model's HTML and the
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
/// filtering happens client-side over what's already embedded, never a callback
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
        .unwrap_or("RED — report");
    let t = red_driver::html_escape(title);
    let safe_body = strip_scripts(body);
    // The base document style: RED's active theme if the UI supplied one, else
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

    // Unguessable per-report nonce: only our bundle carries it, so a `<script>`
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
/// only (the report's CSP already forbids script execution); this just keeps the
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
        // `i` advances only by whole chars (`ch.len_utf8()` below) or past a
        // matched ASCII `</script>`, so it always sits on a UTF-8 boundary inside
        // the `i < html.len()` guard — there is always a next char.
        #[allow(
            clippy::expect_used,
            reason = "i is maintained on a char boundary; see comment"
        )]
        let ch = html[i..]
            .chars()
            .next()
            .expect("i sits on a char boundary within bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The stable grounding instruction, tailored to the access tier. Shared
/// with the ACP path, which folds it into the agent's first prompt (ACP
/// `session/prompt` has no system role). The tier line keeps the model's
/// expectations in step with the catalog it actually receives, but the *catalog*
/// is the real gate; the prompt is just courtesy.
pub(crate) fn system_prompt(ctx: &AiContext, policy: &AiPolicy) -> String {
    let tools_line = match policy.tier {
        AiTier::Off => {
            "You have NO database tools available; answer from the schema overview and the \
             conversation alone, and tell the user you cannot read the live database."
        }
        AiTier::Schema => {
            "You have schema-only tools: list_schema, describe_table, relationship_map, and \
             object_ddl. You can inspect structure (tables, columns, types, keys, definitions) but \
             you CANNOT read row data; there is no query tool, so do not promise to run one."
        }
        AiTier::Read => {
            "You have read-only tools: list_schema, describe_table, relationship_map (the \
             foreign-key graph), object_ddl (an object's real definition), run_select (capped \
             SELECTs), search_data (find a term across a table's columns), explain (optionally \
             with actuals), health_report and server_sessions (what is wrong / what is running \
             now), export_result (write a result to a file for the user), open_query (open a SQL \
             query in a new editor tab in the user's workspace; a read-only SELECT runs \
             automatically), and generate_report (you author an HTML report from data you've read, \
             with optional interactive Chart.js charts; it appears as a card in the chat the user \
             can open; use it when the user asks for a report). Use them to ground every answer in \
             the live database rather than guessing: discover objects with list_schema, inspect \
             structure with describe_table, and read data with run_select. Use open_query to hand \
             the user a query to explore in the grid. Prefer small, targeted queries with explicit \
             columns and LIMIT."
        }
        AiTier::Write => {
            "You have the read tools (list_schema, describe_table, relationship_map, object_ddl, \
             run_select, search_data, explain, health_report, server_sessions, diff_schema, \
             diff_data, suggest_index, export_result, open_query, generate_report) AND gated write \
             tools: propose_write for a SINGLE INSERT/UPDATE/DELETE, propose_changeset for several \
             as one unit, create_index, and kill_session. Every one requires the user's explicit \
             Allow on the exact operation; assume it may be denied, and never batch or chain \
             statements inside one propose_write. UPDATE/DELETE must have a WHERE clause; \
             destructive DDL (DROP/TRUNCATE/ALTER) is not available; tell the user to run those by \
             hand. Only write when the user has asked you to change data; read first to get it \
             right, and verify after."
        }
    };
    let mut s = finish_system_prompt(
        format!(
            "You are RED's database agent, embedded in a native SQL explorer. You help the user \
             explore and understand the database they are connected to.\n\n\
             {tools_line}\n\n\
             Before any query that joins more than one table, call relationship_map; do not infer \
             join keys from column names. Before explaining a constraint failure or what a view \
             actually does, call object_ddl.\n\n\
             When you write SQL for the user, put it in a fenced ```sql block so they can run it. \
             Be concise: lead with the answer, then the supporting query or detail.\n",
        ),
        ctx,
        "This connection is READ-ONLY: do not propose INSERT/UPDATE/DELETE/DDL.",
    );
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
    // A reopened conversation seeds the prior exchange once, so the model
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
async fn profile_table(
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
async fn search_data(
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

/// Cap on the tables one `diff_schema` describes per side. Each is a catalog
/// round trip, so a schema with a thousand tables is compared by existence past
/// this bound rather than in detail, and the report says so.
const DIFF_SCHEMA_MAX_TABLES: usize = 200;
/// Cap on the differing rows one `diff_data` reports back. The merge-walk itself
/// streams the whole table; this bounds what reaches the model's context.
const DIFF_ROW_REPORT: usize = 200;

/// Compare two schemas' structure inside one connection.
///
/// Deliberately same-connection: `AiBackend` holds one driver, so a cross-server
/// comparison would need a second session and is a different feature. Both sides
/// are graded as the same engine, which is the truth here and makes a spelling
/// difference (`varchar(50)` vs `varchar(100)`) a real finding rather than noise.
async fn diff_schema(
    driver: &Arc<dyn DatabaseDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    use red_core::schema_diff::{SchemaSnapshot, compare};

    let name = |k: &str| {
        input
            .get(k)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let (Some(left), Some(right)) = (name("left"), name("right")) else {
        return (
            "error: diff_schema needs `left` and `right` schema names".into(),
            false,
        );
    };
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
    let schemas = match driver.list_objects().await {
        Ok(s) => s,
        Err(e) => return (format!("error: {e}"), false),
    };
    let mut truncated = false;
    let mut snapshot = async |want: &str| -> Result<SchemaSnapshot, String> {
        let meta = schemas
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(want))
            .ok_or_else(|| format!("no schema named `{want}` in this connection"))?;
        // Same engine on both sides by construction, so `DbKind` only has to be
        // consistent, not accurate: `compare` reads it to decide cross-engine.
        let mut snap = SchemaSnapshot::from_meta(red_core::DbKind::default(), meta);
        let relations: Vec<&red_core::ObjectMeta> = meta
            .objects
            .iter()
            .filter(|o| o.kind.is_relation())
            .filter(|o| wanted.is_empty() || wanted.contains(&o.name.to_ascii_lowercase()))
            .collect();
        truncated |= relations.len() > DIFF_SCHEMA_MAX_TABLES;
        for obj in relations.into_iter().take(DIFF_SCHEMA_MAX_TABLES) {
            if let Ok(detail) = driver.describe_table(&meta.name, &obj.name).await {
                snap.details.insert(obj.name.clone(), detail);
            }
        }
        Ok(snap)
    };
    let left_snap = match snapshot(left).await {
        Ok(s) => s,
        Err(why) => return (format!("error: {why}"), false),
    };
    let right_snap = match snapshot(right).await {
        Ok(s) => s,
        Err(why) => return (format!("error: {why}"), false),
    };

    let delta = compare(&left_snap, &right_snap);
    let mut out = if delta.is_empty() {
        format!("`{left}` and `{right}` are structurally identical.\n")
    } else {
        format!(
            "{} difference(s) between `{left}` (baseline) and `{right}`:\n",
            delta.count()
        )
    };
    let list = |label: &str, items: &[red_core::ObjectMeta]| {
        if items.is_empty() {
            return String::new();
        }
        let names: Vec<&str> = items.iter().map(|o| o.name.as_str()).collect();
        format!("{label}: {}\n", names.join(", "))
    };
    out.push_str(&list(&format!("Only in `{right}`"), &delta.objects_added));
    out.push_str(&list(&format!("Only in `{left}`"), &delta.objects_removed));
    for t in &delta.tables_changed {
        out.push_str(&format!("\n{}:\n", t.name));
        for c in &t.columns_added {
            out.push_str(&format!("  + column {}\n", c.name));
        }
        for c in &t.columns_removed {
            out.push_str(&format!("  - column {}\n", c.name));
        }
        for c in &t.columns_changed {
            // The uncertain flag is load-bearing: outside the type lattice this is
            // a raw string comparison, and calling it a change without saying so
            // would send the model chasing a spelling difference.
            let note = match c.confidence {
                red_core::schema_diff::Confidence::Certain => "",
                red_core::schema_diff::Confidence::Uncertain => " (may be a spelling difference)",
            };
            out.push_str(&format!(
                "  ~ column {}: {}{note}\n",
                c.left.name, c.summary
            ));
        }
        for i in &t.indexes_added {
            out.push_str(&format!("  + index {}\n", i.name));
        }
        for i in &t.indexes_removed {
            out.push_str(&format!("  - index {}\n", i.name));
        }
        for f in &t.fks_added {
            out.push_str(&format!(
                "  + foreign key {} -> {}.{}\n",
                f.column, f.ref_table, f.ref_column
            ));
        }
        for f in &t.fks_removed {
            out.push_str(&format!(
                "  - foreign key {} -> {}.{}\n",
                f.column, f.ref_table, f.ref_column
            ));
        }
    }
    if truncated {
        out.push_str(&format!(
            "\n(only the first {DIFF_SCHEMA_MAX_TABLES} tables per side were compared in detail; \
             narrow with `tables`)\n"
        ));
    }
    (cap_result_bytes(out, limits.max_result_bytes), true)
}

/// Compare two tables' rows inside one connection, key-ordered and merge-walked.
///
/// Runs the same streaming job the UI's data diff uses, so both tables are read
/// through cursors and never materialized; only the reported differences are
/// bounded, because those are what enter the model's context.
async fn diff_data(
    driver: &Arc<dyn DatabaseDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    use std::sync::atomic::AtomicBool;

    let table = |schema_key: &str, table_key: &str| -> Option<TableRef> {
        let name = input
            .get(table_key)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())?;
        Some(TableRef {
            schema: input
                .get(schema_key)
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            name: name.to_string(),
        })
    };
    let (Some(left), Some(right)) = (
        table("left_schema", "left_table"),
        table("right_schema", "right_table"),
    ) else {
        return (
            "error: diff_data needs `left_table` and `right_table`".into(),
            false,
        );
    };
    let key = input
        .get("key")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    // The job reports progress to the UI; there is no toast behind an agent call,
    // so the receiver is dropped and the sends fall on the floor.
    let (events, _rx) = futures::channel::mpsc::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let job = crate::dispatch::jobs::diff_job(
        driver.clone(),
        left.clone(),
        driver.clone(),
        right.clone(),
        key,
        cancel,
        events,
        crate::protocol::OpId::new(0),
    );
    let result = match limits.statement_timeout_ms {
        0 => job.await,
        ms => match tokio::time::timeout(Duration::from_millis(ms), job).await {
            Ok(r) => r,
            Err(_) => {
                return (
                    "error: the diff exceeded the agent's statement timeout. It reads both tables \
                     whole, so compare smaller tables or do it from the UI's data diff, which can \
                     run long."
                        .into(),
                    false,
                );
            }
        },
    };
    let (plan, acc) = match result {
        Ok(pair) => pair,
        Err(e) => return (format!("error: {e}"), false),
    };
    let summary = &acc.summary;
    let l = qualified(left.schema.as_deref(), &left.name);
    let r = qualified(right.schema.as_deref(), &right.name);
    let mut out = format!(
        "Compared {l} (baseline) against {r} on `{}`:\n  {} identical, {} changed, {} only in {l}, \
         {} only in {r}\n",
        plan.key, summary.unchanged, summary.changed, summary.removed, summary.added,
    );
    if !plan.left_only.is_empty() || !plan.right_only.is_empty() {
        out.push_str(&format!(
            "  columns compared: {}; only in {l}: {}; only in {r}: {}\n",
            plan.columns.join(", "),
            if plan.left_only.is_empty() {
                "none".into()
            } else {
                plan.left_only.join(", ")
            },
            if plan.right_only.is_empty() {
                "none".into()
            } else {
                plan.right_only.join(", ")
            },
        ));
    }
    if !acc.rows.is_empty() {
        out.push_str("\nDifferences:\n");
        for row in acc.rows.iter().take(DIFF_ROW_REPORT) {
            let what = match row.kind {
                red_core::diff::DiffKind::Added => format!("only in {r}"),
                red_core::diff::DiffKind::Removed => format!("only in {l}"),
                red_core::diff::DiffKind::Changed => {
                    let cols: Vec<&str> = row
                        .changed
                        .iter()
                        .enumerate()
                        .filter(|(_, differs)| **differs)
                        .filter_map(|(i, _)| plan.columns.get(i).map(String::as_str))
                        .collect();
                    format!("differs in {}", cols.join(", "))
                }
            };
            out.push_str(&format!("  {} — {what}\n", row.key));
        }
        if acc.rows.len() > DIFF_ROW_REPORT || acc.truncated {
            out.push_str(
                "  …(more differing rows than are shown; the counts above are complete)\n",
            );
        }
    }
    (cap_result_bytes(out, limits.max_result_bytes), true)
}

/// Decide whether an index would help a query and emit the candidate DDL as
/// text. A composition of `explain` and `describe_table`, not a new seam: the
/// plan says whether it scans, and the table's existing indexes say whether the
/// suggestion is already there.
async fn suggest_index(
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
fn index_args(input: &Json) -> Result<(TableRef, String, Vec<String>, bool), String> {
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

/// The [`KillMode`](red_core::KillMode) a `kill_session`/`doc_kill_op` input
/// names, defaulting to the reversible one. An unrecognized spelling is an error
/// rather than a guess: guessing wrong here means terminating a session the user
/// only meant to interrupt.
fn kill_mode(input: &Json) -> Result<red_core::KillMode, String> {
    match input.get("mode").and_then(Json::as_str).unwrap_or("cancel") {
        "cancel" => Ok(red_core::KillMode::Cancel),
        "terminate" => Ok(red_core::KillMode::Terminate),
        other => Err(format!(
            "kill mode must be \"cancel\" or \"terminate\", not `{other}`"
        )),
    }
}

/// Stop a server session, re-resolving the target against the live server first.
///
/// The approval the user gave names a specific session doing a specific thing.
/// Between that prompt and this call the server may have finished that statement
/// and handed the same pid/thread id to something else, so the facts the model
/// echoed are *verified* rather than trusted: a mismatch refuses the kill instead
/// of stopping whatever now holds the key.
async fn kill_session(driver: &Arc<dyn DatabaseDriver>, input: &Json) -> (String, bool) {
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

/// How many of the largest tables `health_report` lists. Enough to answer "where
/// did the disk go" without turning the report into a catalog dump.
const HEALTH_TOP_TABLES: usize = 20;

/// A [`HealthReport`](red_core::health::HealthReport) as text for the agent.
///
/// The `unavailable` list is not decoration: a report that silently drops the
/// unused-index check reads as a clean bill of health, so what could *not* be
/// checked is stated as plainly as what was.
fn format_health(r: &red_core::health::HealthReport) -> String {
    use std::fmt::Write;

    let scope = match &r.namespace {
        Some(ns) => format!(" (schema {ns})"),
        None => String::new(),
    };
    let mut s = format!(
        "Health of this {:?} connection{scope}\n{} across {} table(s), of which {} is index.\n",
        r.engine,
        fmt_bytes(r.totals.bytes),
        r.totals.table_count,
        fmt_bytes(r.totals.index_bytes),
    );
    if !r.tables.is_empty() {
        s.push_str("\nLargest tables:\n");
        for t in r.tables.iter().take(HEALTH_TOP_TABLES) {
            let _ = writeln!(
                s,
                "  {}  {} ({} index, ~{} rows est)",
                qualified(t.table.schema.as_deref(), &t.table.name),
                fmt_bytes(t.bytes),
                fmt_bytes(t.index_bytes),
                t.estimated_rows,
            );
        }
        if r.tables.len() > HEALTH_TOP_TABLES {
            let _ = writeln!(s, "  …({} more)", r.tables.len() - HEALTH_TOP_TABLES);
        }
    }
    let findings = r.sorted_findings();
    if findings.is_empty() {
        s.push_str("\nNo findings from the checks that ran.\n");
    } else {
        let _ = write!(s, "\n{} finding(s), worst first:\n", findings.len());
        for f in findings {
            let object = f
                .object
                .as_ref()
                .map(|t| format!(" {}", qualified(t.schema.as_deref(), &t.name)))
                .unwrap_or_default();
            let _ = writeln!(s, "  [{:?}] {:?}{object}: {}", f.severity, f.kind, f.title);
            let _ = writeln!(s, "    {}", f.detail);
            if let Some(sql) = &f.suggested_sql {
                // Text to read and paste. RED never runs a remediation itself, and
                // saying so keeps the model from treating it as something to apply.
                let _ = writeln!(s, "    suggested (NOT run; hand this to the user): {sql}");
            }
        }
    }
    if !r.unavailable.is_empty() {
        s.push_str("\nChecks that could NOT run here (so their absence proves nothing):\n");
        for u in &r.unavailable {
            let _ = writeln!(s, "  {:?}: {}", u.kind, u.reason);
        }
    }
    s
}

/// Live server sessions as text, longest-running first (the driver's own order).
fn format_sessions(sessions: &[red_core::ServerSession], restricted: bool) -> String {
    use std::fmt::Write;

    if sessions.is_empty() {
        return "No client sessions are running.".to_string();
    }
    let mut s = format!("{} session(s), longest-running first:\n", sessions.len());
    for x in sessions {
        let field = |label: &str, v: &Option<String>| match v {
            Some(v) if !v.is_empty() => format!(" {label}={v}"),
            _ => String::new(),
        };
        let _ = write!(
            s,
            "  [{}]{}{}{}{} {} for {:.1}s",
            x.key,
            field("user", &x.user),
            field("db", &x.database),
            field("app", &x.application),
            field("from", &x.client_addr),
            x.state,
            x.elapsed_secs,
        );
        if x.is_self {
            s.push_str(" (RED's own connection)");
        }
        s.push('\n');
        if let Some(w) = &x.wait {
            let _ = writeln!(s, "    waiting on {w}");
        }
        if !x.blocked_by.is_empty() {
            let by: Vec<String> = x.blocked_by.iter().map(ToString::to_string).collect();
            let _ = writeln!(s, "    blocked by {}", by.join(", "));
        }
        match &x.query {
            Some(q) => {
                let _ = writeln!(s, "    {}", truncate_summary(q.trim(), 300));
            }
            None => s.push_str("    (statement not visible to this role)\n"),
        }
    }
    if restricted {
        s.push_str(
            "(the connected role may not read other sessions' statements, so some are hidden \
             rather than absent)\n",
        );
    }
    s
}

/// Cap on the edges one `relationship_map` reports. Large enough for any schema a
/// person reasons about in one sitting; past it the map is a data dump rather than
/// a map, and the truncation is reported so the model narrows with `tables`.
const FK_EDGE_CAP: usize = 400;

/// The connection's foreign-key graph as text: every edge, then the tables no
/// edge touches. One `foreign_keys()` pass (the same graph the ER canvas draws)
/// plus the object list for the islands, because a table nobody references is a
/// fact the model cannot infer from the edges it *did* get.
async fn relationship_map(driver: &Arc<dyn DatabaseDriver>, input: &Json) -> (String, bool) {
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

/// One end of an FK edge as `schema.table.column`, or `schema.table.(a, b)` for a
/// composite key. `side` picks the column of each `(from, to)` pair.
fn fk_side(schema: Option<&str>, table: &str, side: usize, columns: &[(String, String)]) -> String {
    let cols: Vec<&str> = columns
        .iter()
        .map(|(from, to)| {
            if side == 0 {
                from.as_str()
            } else {
                to.as_str()
            }
        })
        .collect();
    let table = qualified(schema, table);
    match cols.as_slice() {
        [one] => format!("{table}.{one}"),
        many => format!("{table}.({})", many.join(", ")),
    }
}

/// `schema.table`, or the bare table on an engine with no schemas.
fn qualified(schema: Option<&str>, table: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{table}"),
        None => table.to_string(),
    }
}

fn format_schema(schemas: &[red_core::SchemaMeta]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for sch in schemas {
        let _ = writeln!(out, "schema {} ({} objects):", sch.name, sch.objects.len());
        for obj in &sch.objects {
            let _ = writeln!(out, "  {} {}", obj.kind.as_str(), obj.name);
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
    // (`<N bytes>`), exactly the compact form we want for the model.
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

// ============================================================================
// MongoDB (document) agent — the `DocDriver` backend's tools, mirroring the
// SQL and Redis (`kv_*`) catalogs. Read tools auto-run (they're in
// `READ_ONLY_TOOLS`); the `propose_*` writes ride the per-call approval gate.
// The signature tools (`profile_collection`/`audit_collection`/`index_advice`)
// are host-side compositions over the driver's read methods — no new seam.
// ============================================================================

/// The doc backend's mutating tools; each rides the same per-call approval gate
/// as a SQL/Redis write. Their complement (the reads) is the `doc_*`/`find`/…
/// set listed in [`READ_ONLY_TOOLS`].
const DOC_WRITE_TOOLS: &[&str] = &[
    "propose_doc_write",
    "propose_index",
    "propose_collection_op",
    // Not a document write, but a server-state one that rides the same gate.
    "doc_kill_op",
];

fn is_doc_write_tool(name: &str) -> bool {
    DOC_WRITE_TOOLS.contains(&name)
}

/// The tier-filtered MongoDB tool catalog. Same shape as `kv_tool_catalog`: an
/// array of every def, then a tier + read-only filter.
pub(crate) fn doc_tool_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
    let coll_args = |extra: Json| {
        // `{ db, coll }` plus tool-specific properties merged in.
        let mut props = serde_json::Map::new();
        props.insert(
            "db".into(),
            json!({ "type": "string", "description": "Database name." }),
        );
        props.insert(
            "coll".into(),
            json!({ "type": "string", "description": "Collection name." }),
        );
        if let Json::Object(m) = extra {
            props.extend(m);
        }
        json!({
            "type": "object",
            "properties": props,
            "required": ["db", "coll"],
            "additionalProperties": false,
        })
    };
    let all = [
        ToolDef {
            name: "doc_server_info".into(),
            description: "Summarize the deployment: server version, topology \
                (standalone/replica-set/sharded), and the databases with their sizes. Call this \
                first to understand what you're connected to."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "list_collections".into(),
            description: "The catalog: collections in a database (or every database when `db` is \
                omitted), with estimated document counts and view/time-series/capped kind."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "db": { "type": "string", "description": "Database to list; omit for all." } },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "describe_collection".into(),
            description: "One collection's DISCOVERED schema (sampled field paths with per-type \
                frequency and present-ratio) plus its indexes. The schema is inferred from a \
                sample, not declared — a field can legitimately be several types."
                .into(),
            input_schema: coll_args(json!({})),
        },
        ToolDef {
            name: "doc_reference_map".into(),
            description: "Discover which fields REFERENCE other collections, and how well they \
                resolve. MongoDB has no foreign keys, so a field named `user_id` may point at \
                `users._id`, at something else, or at nothing: this samples each candidate \
                field's values, probes the target collection's `_id`, and reports the HIT RATE \
                (\"198/200 resolve\" is a usable join; \"0/200\" is a name collision). CALL THIS \
                BEFORE WRITING AN AGGREGATION THAT $lookups ACROSS COLLECTIONS. Bounded: a few \
                collections, a couple of hundred sampled values per field."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "db": { "type": "string", "description": "Database to map; omit for every non-system database." },
                    "collections": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Restrict to these collections; omit for all in the database.",
                    },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "profile_collection".into(),
            description: "The signature data-quality tool: sample the collection and report, per \
                field path, its type distribution and how often it is present — surfacing schema \
                drift (a field that's string here and int there) and optional fields. Never \
                returns raw documents."
                .into(),
            input_schema: coll_args(
                json!({ "sample": { "type": "integer", "description": "Documents to sample (default 200)." } }),
            ),
        },
        ToolDef {
            name: "get_document".into(),
            description: "Fetch ONE document by its `_id`. Cheaper and less error-prone than a \
                find with an _id filter. Pass an ObjectId as { \"$oid\": \"…\" } and a plain id \
                as itself."
                .into(),
            input_schema: coll_args(json!({
                "id": { "description": "The _id to fetch, in extended JSON ({ \"$oid\": \"…\" }) or as a plain scalar." },
            })),
        },
        ToolDef {
            name: "sample_documents".into(),
            description: "Return N random documents ($sample) so you can see the real shape before \
                writing a filter — the cheap 'show me what this looks like' a schemaless store needs."
                .into(),
            input_schema: coll_args(
                json!({ "n": { "type": "integer", "description": "How many to sample (default 5)." } }),
            ),
        },
        ToolDef {
            name: "find".into(),
            description: "Run a read-only find. `filter`/`projection`/`sort` are JSON documents \
                (extended JSON, e.g. { \"status\": \"active\" }); rows are capped. The only way to \
                read actual documents."
                .into(),
            input_schema: coll_args(json!({
                "filter": { "type": "object", "description": "Match document (empty = all)." },
                "projection": { "type": "object", "description": "Fields to include/exclude." },
                "sort": { "type": "object", "description": "Sort spec, e.g. { \"age\": -1 }." },
                "limit": { "type": "integer", "description": "Max documents to return." },
            })),
        },
        ToolDef {
            name: "aggregate".into(),
            description: "Run a read-only aggregation pipeline (a JSON array of stages). Write \
                stages ($out/$merge) are rejected. This is Mongo's analytical engine — group, \
                bucket, lookup, facet — well past what a plain find can express."
                .into(),
            input_schema: coll_args(
                json!({ "pipeline": { "type": "array", "description": "Array of aggregation stage documents." } }),
            ),
        },
        ToolDef {
            name: "count".into(),
            description: "Count documents matching an optional filter — cheap cardinality without \
                pulling documents."
                .into(),
            input_schema: coll_args(json!({ "filter": { "type": "object", "description": "Match document (empty = all)." } })),
        },
        ToolDef {
            name: "distinct".into(),
            description: "The distinct values of one field over documents matching an optional filter."
                .into(),
            input_schema: coll_args(json!({
                "field": { "type": "string", "description": "Field path." },
                "filter": { "type": "object", "description": "Match document (empty = all)." },
            })),
        },
        ToolDef {
            name: "explain_query".into(),
            description: "Explain a find: the winning plan, the index used, ACTUAL docs-examined \
                vs returned (it runs with executionStats, so these are measurements rather than \
                estimates), and an explicit COLLSCAN flag. Examined far exceeding returned is the \
                missing-index signature."
                .into(),
            input_schema: coll_args(json!({ "filter": { "type": "object", "description": "The find filter to explain." } })),
        },
        ToolDef {
            name: "index_advice".into(),
            description: "Given a find filter, is it index-covered? If it's a collection scan, \
                suggest the index key to add. Does NOT create it — that's a gated write."
                .into(),
            input_schema: coll_args(json!({ "filter": { "type": "object", "description": "The find filter to advise on." } })),
        },
        ToolDef {
            name: "doc_current_op".into(),
            description: "What the deployment is running RIGHT NOW ($currentOp), longest-running \
                first: opid, operation kind, namespace, elapsed time, client, the command itself, \
                and whether it is blocked waiting for a lock. The \"why is it slow right now\" \
                answer, as opposed to audit_collection's structural one. Idle connections are \
                excluded."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "audit_collection".into(),
            description: "Roll a sample into a health report: schema drift (mixed-type fields), \
                optional/sparse fields, and index coverage. The 'what's wrong in here' answer."
                .into(),
            input_schema: coll_args(json!({})),
        },
        export_tool_def(
            "Write the documents matching a filter to a JSON file for the user (an array of \
             extended-JSON documents) and hand it over as a card in the chat they can open. \
             Bounded: it pages through a large but finite number of documents and says so if it \
             stopped early. Use it when the user asks for an export/dump rather than an answer.",
            json!({
                "db": { "type": "string", "description": "Database name." },
                "coll": { "type": "string", "description": "Collection name." },
                "filter": { "type": "object", "description": "Match document (empty = the whole collection)." },
                "name": { "type": "string", "description": "A short name for the file, e.g. \"active-users\"." },
            }),
            &["db", "coll"],
        ),
        report_tool_def(),
        spawn_subagent_tool_def(),
        // --- gated writes (Write tier, writable connection only) ---
        ToolDef {
            name: "propose_doc_write".into(),
            description: "Propose ONE write (insert/update/replace/delete) for the user to approve. \
                `update`/`delete` require a non-empty `filter`; `many:true` (affect all matches) is \
                shown explicitly in the approval. Read/find first to know what you'll affect."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": ["insert", "update", "replace", "delete"] },
                    "db": { "type": "string" },
                    "coll": { "type": "string" },
                    "filter": { "type": "object", "description": "Match document (required for update/replace/delete)." },
                    "document": { "type": "object", "description": "The document to insert, or the replacement (insert/replace)." },
                    "update": { "type": "object", "description": "The $set-style patch fields (update)." },
                    "many": { "type": "boolean", "description": "Affect all matches, not just one (update/delete)." },
                },
                "required": ["op", "db", "coll"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "propose_index".into(),
            description: "Propose creating an index for the user to approve. Building an index \
                loads the server; the user approves."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "db": { "type": "string" },
                    "coll": { "type": "string" },
                    "keys": {
                        "type": "object",
                        "description": "Index key spec, e.g. { \"email\": 1, \"createdAt\": -1 }.",
                    },
                    "unique": { "type": "boolean" },
                },
                "required": ["db", "coll", "keys"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "doc_kill_op".into(),
            description: "Stop one running operation by its opid (killOp). Call doc_current_op \
                first for the `opid`, and copy that operation's `namespace` and `command` into \
                this call so the user can see what they are stopping — the target is re-checked \
                against the live server first and refused if the opid now belongs to something \
                else. Note that Mongo does NOT roll back an interrupted multi-document write: a \
                killed updateMany leaves what it already changed. Requires explicit approval."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "opid": { "type": "integer", "description": "The operation id from doc_current_op." },
                    "namespace": { "type": "string", "description": "The operation's db.collection, copied from doc_current_op; verified before the kill." },
                    "command": { "type": "string", "description": "The operation's command, copied from doc_current_op, so the approval shows what is being stopped." },
                },
                "required": ["opid"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "propose_collection_op".into(),
            description: "Propose creating or dropping a collection for the user to approve. \
                Dropping is destructive and always requires explicit confirmation."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": ["create", "drop"] },
                    "db": { "type": "string" },
                    "coll": { "type": "string" },
                },
                "required": ["op", "db", "coll"],
                "additionalProperties": false,
            }),
        },
    ];
    gate_catalog(all, policy)
}

/// The doc subagent's tool subset: the parent's doc catalog, narrowed like
/// [`subagent_catalog`].
fn doc_subagent_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
    narrow_to_subagent(doc_tool_catalog(policy))
}

fn doc_subagent_system_prompt(task: &str) -> String {
    format!(
        "You are a focused sub-investigator working for a parent AI agent on ONE task against a \
         MongoDB deployment. You have read-only document tools (doc_server_info, list_collections, \
         describe_collection, profile_collection, sample_documents, find, aggregate, count, \
         distinct, explain_query, index_advice, audit_collection); you cannot write or delegate \
         further. Collections are schemaless — infer the shape before you filter. Do the task, then \
         reply with a concise report of your findings the parent can use directly. Do not ask \
         questions; you cannot receive answers.\n\nTask: {task}"
    )
}

/// The MongoDB agent's grounding prompt (the doc analogue of `kv_system_prompt`).
pub(crate) fn doc_system_prompt(ctx: &AiContext, policy: &AiPolicy) -> String {
    let tools_line = match policy.tier {
        AiTier::Off => {
            "You have NO MongoDB tools available; the assistant is limited to general conversation."
        }
        AiTier::Schema => {
            "You have metadata-only MongoDB tools: doc_server_info (which lists the databases), \
             list_collections, and describe_collection. You can see the catalog and each \
             collection's inferred schema, indexes and validator, but cannot read documents."
        }
        AiTier::Read => {
            "You have read-only MongoDB tools: doc_server_info (deployment, topology and the \
             database list), list_collections, describe_collection (inferred schema, indexes and \
             any declared validator), doc_reference_map (which fields reference which collections, and \
             how well they resolve), profile_collection (per-field type/drift stats), \
             sample_documents, get_document (one document by _id), find, aggregate ($out/$merge \
             rejected), count, distinct, explain_query (optionally with execution stats), \
             index_advice, audit_collection, doc_current_op (what is running now), export_result, \
             and generate_report. Ground every answer in the live deployment."
        }
        AiTier::Write => {
            "You have the read-only MongoDB tools (doc_server_info, list_collections, \
             describe_collection, profile_collection, sample_documents, find, aggregate, count, \
             distinct, explain_query, index_advice, audit_collection, doc_current_op, \
             export_result) AND gated write tools: propose_doc_write, propose_index, \
             propose_collection_op, and doc_kill_op. Every write requires the user's explicit \
             Allow; update/delete require a non-empty filter, and dropping a collection always \
             confirms."
        }
    };
    finish_system_prompt(
        format!(
            "You are RED's MongoDB agent, embedded in a native database explorer. You help the \
             user explore and understand the MongoDB deployment they are connected to.\n\n\
             {tools_line}\n\n\
             MongoDB is SCHEMALESS: a collection has no declared columns, and a field can be \
             several types across documents. So ORIENT before you act — doc_server_info to see the \
             deployment, list_collections for the catalog, describe_collection/profile_collection \
             to learn the discovered schema, sample_documents to see real shape — THEN \
             find/aggregate to read, and explain_query/index_advice/audit_collection to reason \
             about performance and health. Before writing an aggregation that joins collections \
             with $lookup, call doc_reference_map: Mongo has no foreign keys, so a field named \
             `user_id` may not resolve, and the map tells you the hit rate.\n\n\
             Queries are filter documents and aggregation pipelines (extended JSON), never SQL. Be \
             concise: lead with the answer, then the detail.\n",
        ),
        ctx,
        "This connection is READ-ONLY.",
    )
}

/// How much of a proposed document/update payload the approval prompt shows.
/// Long enough for a realistic `$set`, short enough that a huge model-supplied
/// document cannot push the actual operation off the top of the dialog.
const DOC_PAYLOAD_CHARS: usize = 600;

/// Vet a doc write tool for the approval gate: build the human-readable operation
/// shown in Allow/Deny, and hard-block the footguns (an unfiltered update/delete)
/// even with approval. Tier + read-only were already checked by [`assess_write`].
fn assess_doc_write(name: &str, input: &Json) -> WriteAssessment {
    let s = |k: &str| {
        input
            .get(k)
            .and_then(Json::as_str)
            .filter(|v| !v.is_empty())
    };
    // A filter is "present" only if it's a non-empty JSON object — the doc-seam
    // analog of "UPDATE/DELETE need a WHERE".
    let has_filter = input
        .get("filter")
        .and_then(Json::as_object)
        .is_some_and(|o| !o.is_empty());
    let ns = format!("{}.{}", s("db").unwrap_or("?"), s("coll").unwrap_or("?"));
    let filter_txt = input
        .get("filter")
        .map(|f| f.to_string())
        .unwrap_or_else(|| "{}".into());
    // What will actually be written, not just what it will be matched against.
    //
    // The SQL path shows the entire statement; this showed only op + namespace +
    // filter, while the executor went on to apply the `update`/`document` fields
    // that were never displayed. A proposal of
    // `{op:"update", filter:{email:"x"}, update:{role:"admin"}}` rendered as a bland
    // "UPDATE db.users matching {email:x}" and a reasonable user approved a
    // privilege escalation they never saw. What executes must be derived into what
    // is shown — the same rule the `kv_delete` prompt now follows.
    let payload = |key: &str| match input.get(key) {
        Some(v) => format!("\n{}", truncate_summary(&v.to_string(), DOC_PAYLOAD_CHARS)),
        None => String::new(),
    };
    match name {
        "propose_doc_write" => {
            let op = s("op").unwrap_or("");
            let many = input.get("many").and_then(Json::as_bool).unwrap_or(false);
            let many_note = if many { " (many: ALL matches)" } else { "" };
            match op {
                "insert" => WriteAssessment::NeedsApproval {
                    sql: format!("INSERT one document into {ns}{}", payload("document")),
                },
                "replace" => {
                    if !has_filter {
                        return WriteAssessment::Reject(
                            "replace requires a non-empty filter (e.g. { _id: ... })".into(),
                        );
                    }
                    WriteAssessment::NeedsApproval {
                        sql: format!(
                            "REPLACE document in {ns} matching {filter_txt}{}",
                            payload("document")
                        ),
                    }
                }
                "update" => {
                    if !has_filter {
                        return WriteAssessment::Reject(
                            "update requires a non-empty filter (refusing an unfiltered update)"
                                .into(),
                        );
                    }
                    WriteAssessment::NeedsApproval {
                        sql: format!(
                            "UPDATE {ns} matching {filter_txt}{many_note}{}",
                            payload("update")
                        ),
                    }
                }
                "delete" => {
                    if !has_filter {
                        return WriteAssessment::Reject(
                            "delete requires a non-empty filter (refusing an unfiltered delete)"
                                .into(),
                        );
                    }
                    WriteAssessment::NeedsApproval {
                        sql: format!("DELETE from {ns} matching {filter_txt}{many_note}"),
                    }
                }
                other => WriteAssessment::Reject(format!(
                    "propose_doc_write `op` must be insert/update/replace/delete, not `{other}`"
                )),
            }
        }
        "propose_index" => {
            let keys = input
                .get("keys")
                .map(|k| k.to_string())
                .unwrap_or_else(|| "{}".into());
            let unique = input.get("unique").and_then(Json::as_bool).unwrap_or(false);
            let unique_note = if unique { " UNIQUE" } else { "" };
            WriteAssessment::NeedsApproval {
                sql: format!("CREATE{unique_note} INDEX on {ns} keys {keys}"),
            }
        }
        "doc_kill_op" => match input.get("opid").and_then(Json::as_i64) {
            Some(opid) => {
                let mut op = format!("KILL operation {opid}");
                if let Some(ns) = s("namespace") {
                    op.push_str(&format!(" on {ns}"));
                }
                op.push_str(
                    "\n\u{26a0} An interrupted multi-document write is NOT rolled back: what it \
                     already changed stays changed.",
                );
                match s("command") {
                    Some(cmd) => op.push_str(&format!(
                        "\nRunning: {}",
                        truncate_summary(cmd, DOC_PAYLOAD_CHARS)
                    )),
                    None => op.push_str(
                        "\nThe agent did not say what this operation is; read doc_current_op \
                         before allowing.",
                    ),
                }
                WriteAssessment::NeedsApproval { sql: op }
            }
            None => WriteAssessment::Reject(
                "doc_kill_op needs the numeric `opid` of an operation from doc_current_op".into(),
            ),
        },
        "propose_collection_op" => match s("op").unwrap_or("") {
            "create" => WriteAssessment::NeedsApproval {
                sql: format!("CREATE collection {ns}"),
            },
            "drop" => WriteAssessment::NeedsApproval {
                sql: format!("DROP collection {ns} — destructive, cannot be undone"),
            },
            other => WriteAssessment::Reject(format!(
                "propose_collection_op `op` must be create/drop, not `{other}`"
            )),
        },
        other => WriteAssessment::Reject(format!("unknown doc write tool `{other}`")),
    }
}

/// Run one MongoDB tool call against `driver`. Read tools compose the driver's
/// read methods; the `propose_*` writes execute only after the turn loop's
/// approval (re-vetted here as defense in depth).
pub(crate) async fn doc_run_tool(
    driver: &Arc<dyn DocDriver>,
    name: &str,
    input: &Json,
    policy: &AiPolicy,
    _cancel: &CancelToken,
    report: &ReportSink,
) -> (String, bool) {
    if !policy.tier.allows_tool(name) {
        return (
            format!("error: the `{name}` tool is not available at this access tier"),
            false,
        );
    }
    // Defense in depth: re-run the write gate here so a destructive shape can't
    // slip through even if the turn loop's check were ever bypassed.
    if is_doc_write_tool(name)
        && let WriteAssessment::Reject(why) = assess_doc_write(name, input)
    {
        return (format!("error: {why}"), false);
    }
    let limits = &policy.limits;
    let abort = AbortSignal::new();
    let db = || input.get("db").and_then(Json::as_str).unwrap_or("");
    let coll = || input.get("coll").and_then(Json::as_str).unwrap_or("");

    let (content, ok) = match name {
        "doc_server_info" => match driver.list_databases().await {
            Ok(dbs) => {
                let mut out = format!(
                    "MongoDB {}, topology: {:?}\nDatabases:\n",
                    driver.server_version(),
                    driver.topology()
                );
                for d in &dbs {
                    out.push_str(&format!(
                        "  {} ({} bytes on disk)\n",
                        d.name, d.size_on_disk
                    ));
                }
                (out, true)
            }
            Err(e) => (format!("error: {e}"), false),
        },
        "list_collections" => {
            let dbs: Vec<String> = match input.get("db").and_then(Json::as_str) {
                Some(d) if !d.is_empty() => vec![d.to_string()],
                _ => match driver.list_databases().await {
                    Ok(list) => list.into_iter().map(|d| d.name).collect(),
                    Err(e) => return (format!("error: {e}"), false),
                },
            };
            let mut out = String::new();
            for d in &dbs {
                match driver.list_collections(d).await {
                    Ok(colls) => {
                        out.push_str(&format!("{d}:\n"));
                        for c in &colls {
                            let kind = match c.kind {
                                CollKind::Collection => "",
                                CollKind::View => " (view)",
                                CollKind::Timeseries => " (timeseries)",
                            };
                            let capped = if c.capped { " capped" } else { "" };
                            let size = if c.size > 0 {
                                format!(", {}", fmt_bytes(c.size))
                            } else {
                                String::new()
                            };
                            let validator = if c.validator.is_some() {
                                " [has a validator]"
                            } else {
                                ""
                            };
                            out.push_str(&format!(
                                "  {} — ~{} docs{size}{kind}{capped}{validator}\n",
                                c.name, c.est_count
                            ));
                        }
                    }
                    Err(e) => out.push_str(&format!("{d}: error: {e}\n")),
                }
            }
            (out, true)
        }
        "describe_collection" => {
            let sample = 200;
            let schema = match driver.infer_schema(db(), coll(), sample, &abort).await {
                Ok(s) => s,
                Err(e) => return (format!("error: {e}"), false),
            };
            let indexes = driver.indexes(db(), coll()).await.unwrap_or_default();
            let mut out = format!(
                "{}\nIndexes:\n{}",
                fmt_doc_schema(&schema),
                fmt_doc_indexes(&indexes)
            );
            // A declared validator is the only *enforced* rule in a schemaless
            // store: a write that violates it bounces, so the model has to see it
            // here rather than discover it from a rejected insert.
            let validator = driver.list_collections(db()).await.ok().and_then(|list| {
                list.into_iter()
                    .find(|c| c.name == coll())
                    .and_then(|c| c.validator)
            });
            out.push_str(&match validator {
                Some(v) => format!(
                    "\nValidator (writes that violate this are rejected by the server):\n  {v}\n"
                ),
                None => "\nValidator: none declared.\n".to_string(),
            });
            (cap_result_bytes(out, limits.max_result_bytes), true)
        }
        "get_document" => {
            let id = match doc_arg_value(driver, input, "id") {
                Ok(Some(v)) => v,
                Ok(None) => return ("error: `id` is required".into(), false),
                Err(e) => return (format!("error: {e}"), false),
            };
            match driver.get_document(db(), coll(), &id).await {
                Ok(Some(doc)) => (
                    cap_result_bytes(
                        doc.to_doc_value().to_extended_json(),
                        limits.max_result_bytes,
                    ),
                    true,
                ),
                Ok(None) => (
                    format!(
                        "no document with that _id in {}.{}. If the _id is an ObjectId, pass it \
                         as {{\"$oid\": \"…\"}} rather than a bare string.",
                        db(),
                        coll()
                    ),
                    true,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "profile_collection" => {
            let sample = input
                .get("sample")
                .and_then(Json::as_u64)
                .map(|n| n as usize)
                .unwrap_or(200);
            match driver.infer_schema(db(), coll(), sample, &abort).await {
                Ok(schema) => (
                    cap_result_bytes(fmt_doc_profile(&schema), limits.max_result_bytes),
                    true,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "sample_documents" => {
            let n = input
                .get("n")
                .and_then(Json::as_u64)
                .map(|n| n as usize)
                .unwrap_or(5)
                .min(limits.max_rows.max(1));
            let pipeline = vec![DocValue::Document(vec![(
                "$sample".into(),
                DocValue::Document(vec![("size".into(), DocValue::Int64(n as i64))]),
            )])];
            match driver.aggregate(db(), coll(), &pipeline, n, &abort).await {
                Ok(page) => (
                    cap_result_bytes(fmt_doc_list(&page.docs), limits.max_result_bytes),
                    true,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "find" => {
            let cap = input
                .get("limit")
                .and_then(Json::as_u64)
                .map(|l| l as usize)
                .unwrap_or(limits.max_rows)
                .min(limits.max_rows.max(1));
            let filter = match doc_arg_value(driver, input, "filter") {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), false),
            };
            let projection = match doc_arg_value(driver, input, "projection") {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), false),
            };
            let sort = match doc_arg_value(driver, input, "sort") {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), false),
            };
            let query = FindQuery {
                db: db().to_string(),
                coll: coll().to_string(),
                filter,
                projection,
                sort,
                skip: 0,
                limit: Some(cap as u64),
                batch: cap,
            };
            match driver.find(&query, &abort).await {
                Ok(page) => (
                    cap_result_bytes(fmt_doc_list(&page.docs), limits.max_result_bytes),
                    true,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "aggregate" => {
            let stages = match doc_arg_value(driver, input, "pipeline") {
                Ok(Some(DocValue::Array(s))) => s,
                Ok(_) => {
                    return (
                        "error: `pipeline` must be a JSON array of stages".into(),
                        false,
                    );
                }
                Err(e) => return (format!("error: {e}"), false),
            };
            if let Some(bad) = pipeline_write_stage(&stages) {
                return (
                    format!("error: write stage `{bad}` is not allowed in a read-only aggregate"),
                    false,
                );
            }
            match driver
                .aggregate(db(), coll(), &stages, limits.max_rows.max(1), &abort)
                .await
            {
                Ok(page) => (
                    cap_result_bytes(fmt_doc_list(&page.docs), limits.max_result_bytes),
                    true,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "count" => {
            let filter = match doc_arg_value(driver, input, "filter") {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), false),
            };
            match driver.count(db(), coll(), filter.as_ref()).await {
                Ok(n) => (format!("{n} documents"), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "distinct" => {
            let field = input.get("field").and_then(Json::as_str).unwrap_or("");
            if field.is_empty() {
                return ("error: `field` is required".into(), false);
            }
            let filter = match doc_arg_value(driver, input, "filter") {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), false),
            };
            match driver.distinct(db(), coll(), field, filter.as_ref()).await {
                Ok(values) => {
                    let rendered: Vec<String> = values
                        .iter()
                        .take(limits.max_rows.max(1))
                        .map(DocValue::to_extended_json)
                        .collect();
                    (
                        cap_result_bytes(
                            format!(
                                "{} distinct value(s):\n{}",
                                values.len(),
                                rendered.join(", ")
                            ),
                            limits.max_result_bytes,
                        ),
                        true,
                    )
                }
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "explain_query" => {
            let filter = match doc_arg_value(driver, input, "filter") {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), false),
            };
            let query = FindQuery {
                db: db().to_string(),
                coll: coll().to_string(),
                filter,
                projection: None,
                sort: None,
                skip: 0,
                limit: None,
                batch: 1,
            };
            // `DocDriver::explain` always asks for `executionStats`, so the plan
            // already carries actuals beside the estimates and needs no `analyze`
            // flag. It does run the plan to gather them, though, so bound it like
            // any other read: a find can't be destructive, only slow.
            let explain = driver.explain(&query);
            let result = match limits.statement_timeout_ms {
                0 => explain.await,
                ms => tokio::time::timeout(Duration::from_millis(ms), explain)
                    .await
                    .unwrap_or(Err(RedError::Timeout)),
            };
            match result {
                Ok(plan) => (fmt_doc_plan(&plan), true),
                Err(RedError::Timeout) => (
                    "error: the explain exceeded the agent's statement timeout; narrow the filter."
                        .into(),
                    false,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "doc_reference_map" => doc_reference_map(driver, input, limits).await,
        "doc_current_op" => match driver.current_ops().await {
            Ok(ops) if ops.is_empty() => ("Nothing is running right now.".to_string(), true),
            Ok(ops) => {
                let mut out = format!("{} running operation(s), longest first:\n", ops.len());
                for o in &ops {
                    out.push_str(&format!(
                        "  opid {} {} on {} for {:.1}s{}{}\n",
                        o.opid,
                        o.op,
                        if o.namespace.is_empty() {
                            "(no namespace)"
                        } else {
                            &o.namespace
                        },
                        o.secs_running,
                        o.client
                            .as_deref()
                            .map(|c| format!(" from {c}"))
                            .unwrap_or_default(),
                        if o.waiting_for_lock {
                            " — WAITING FOR LOCK"
                        } else {
                            ""
                        },
                    ));
                    if let Some(cmd) = &o.command {
                        out.push_str(&format!("    {}\n", truncate_summary(cmd, 300)));
                    }
                }
                (cap_result_bytes(out, limits.max_result_bytes), true)
            }
            Err(e) => (format!("error: {e}"), false),
        },
        "doc_kill_op" => doc_kill_op(driver, input).await,
        "index_advice" => doc_index_advice(driver, input).await,
        "audit_collection" => doc_audit_collection(driver, input, limits).await,
        "export_result" => doc_export(driver, input, report).await,
        "generate_report" => run_generate_report(input, report),
        // Gated writes — the approval already happened in the turn loop.
        "propose_doc_write" => doc_apply_write(driver, input).await,
        "propose_index" => match doc_index_spec(input) {
            Ok(spec) => match driver.create_index(db(), coll(), &spec).await {
                Ok(()) => ("index created".into(), true),
                Err(e) => (format!("error: {e}"), false),
            },
            Err(e) => (format!("error: {e}"), false),
        },
        "propose_collection_op" => match input.get("op").and_then(Json::as_str).unwrap_or("") {
            "create" => match driver.create_collection(db(), coll()).await {
                Ok(()) => (format!("created collection {}.{}", db(), coll()), true),
                Err(e) => (format!("error: {e}"), false),
            },
            "drop" => match driver.drop_collection(db(), coll()).await {
                Ok(()) => (format!("dropped collection {}.{}", db(), coll()), true),
                Err(e) => (format!("error: {e}"), false),
            },
            other => (format!("error: unknown collection op `{other}`"), false),
        },
        other => (format!("error: unknown tool `{other}`"), false),
    };
    (content, ok)
}

/// Parse a tool-input value (`filter`/`projection`/`sort`/`pipeline`) into a
/// [`DocValue`] via the driver's extended-JSON parser. The model may pass it as a
/// JSON object/array (the usual case) or as an extended-JSON string.
///
/// Every model-supplied document is refused here if it smuggles a server-side
/// JavaScript operator (`$where`/`$function`/`$accumulator`): those execute code
/// inside mongod, which no tool in the catalog is described as doing, and a
/// stored prompt-injection payload could plant one in an otherwise-read call.
fn doc_arg_value(
    driver: &Arc<dyn DocDriver>,
    input: &Json,
    key: &str,
) -> Result<Option<DocValue>, String> {
    let parsed = match input.get(key) {
        None | Some(Json::Null) => return Ok(None),
        Some(Json::String(s)) if s.trim().is_empty() => return Ok(None),
        Some(Json::String(s)) => driver.parse_ext_json(s).map_err(|e| e.to_string())?,
        Some(other) => driver
            .parse_ext_json(&other.to_string())
            .map_err(|e| e.to_string())?,
    };
    if let Some(op) = server_js_operator(&parsed) {
        return Err(format!(
            "`{op}` executes server-side JavaScript and is not allowed in `{key}`"
        ));
    }
    Ok(Some(parsed))
}

/// Build an [`IndexSpec`] from a `propose_index` input (`keys` object of
/// field → direction).
fn doc_index_spec(input: &Json) -> Result<IndexSpec, String> {
    let keys = input
        .get("keys")
        .and_then(Json::as_object)
        .ok_or("`keys` must be an object, e.g. { \"email\": 1 }")?;
    if keys.is_empty() {
        return Err("`keys` must name at least one field".into());
    }
    let keys = keys
        .iter()
        .map(|(field, dir)| {
            let d = dir.as_i64().unwrap_or(1);
            (field.clone(), if d < 0 { -1 } else { 1 })
        })
        .collect();
    Ok(IndexSpec {
        keys,
        unique: input.get("unique").and_then(Json::as_bool).unwrap_or(false),
        name: input.get("name").and_then(Json::as_str).map(str::to_string),
    })
}

/// Execute an approved `propose_doc_write` by building the [`DocWrite`] and
/// dispatching to the driver.
async fn doc_apply_write(driver: &Arc<dyn DocDriver>, input: &Json) -> (String, bool) {
    let db = input
        .get("db")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    let coll = input
        .get("coll")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    let op = input.get("op").and_then(Json::as_str).unwrap_or("");
    let many = input.get("many").and_then(Json::as_bool).unwrap_or(false);

    let parse = |key: &str| doc_arg_value(driver, input, key);
    let write = match op {
        "insert" => match parse("document") {
            Ok(Some(v)) => match Document::from_doc_value(v) {
                Some(doc) => DocWrite::Insert {
                    db,
                    coll,
                    docs: vec![doc],
                },
                None => return ("error: `document` must be a JSON object".into(), false),
            },
            _ => return ("error: insert needs a `document` object".into(), false),
        },
        "update" => {
            let Ok(Some(filter)) = parse("filter") else {
                return ("error: update needs a `filter`".into(), false);
            };
            let Ok(Some(patch)) = parse("update") else {
                return (
                    "error: update needs an `update` (the $set fields)".into(),
                    false,
                );
            };
            DocWrite::Update {
                db,
                coll,
                filter,
                change: DocUpdate::Patch(patch),
                many,
            }
        }
        "replace" => {
            let Ok(Some(filter)) = parse("filter") else {
                return ("error: replace needs a `filter`".into(), false);
            };
            let id = match &filter {
                DocValue::Document(fields) => fields
                    .iter()
                    .find(|(k, _)| k == "_id")
                    .map(|(_, v)| v.clone()),
                _ => None,
            };
            let Some(id) = id else {
                return ("error: replace `filter` must pin `_id`".into(), false);
            };
            match parse("document") {
                Ok(Some(v)) => match Document::from_doc_value(v) {
                    Some(doc) => DocWrite::Replace { db, coll, id, doc },
                    None => return ("error: `document` must be a JSON object".into(), false),
                },
                _ => return ("error: replace needs a `document` object".into(), false),
            }
        }
        "delete" => {
            let Ok(Some(filter)) = parse("filter") else {
                return ("error: delete needs a `filter`".into(), false);
            };
            DocWrite::Delete {
                db,
                coll,
                filter,
                many,
            }
        }
        other => return (format!("error: unknown op `{other}`"), false),
    };
    // Defense in depth: never run a destructive shape the classifier flags,
    // even though the approval gate already prompted.
    if classify_doc_op(&write) == OpClass::Destructive
        && let WriteAssessment::Reject(why) = assess_doc_write("propose_doc_write", input)
    {
        return (format!("error: {why}"), false);
    }
    match doc_execute_write(driver, write).await {
        Ok(summary) => (summary, true),
        Err(e) => (format!("error: {e}"), false),
    }
}

/// Dispatch a [`DocWrite`] to the driver, returning a short summary.
async fn doc_execute_write(
    driver: &Arc<dyn DocDriver>,
    write: DocWrite,
) -> Result<String, RedError> {
    match write {
        DocWrite::Insert { db, coll, docs } => {
            let n = driver.insert(&db, &coll, &docs).await?;
            Ok(format!("inserted {n} document(s)"))
        }
        DocWrite::Update {
            db,
            coll,
            filter,
            change,
            many,
        } => {
            let n = driver.update(&db, &coll, &filter, &change, many).await?;
            Ok(format!("updated {n} document(s)"))
        }
        DocWrite::Replace { db, coll, id, doc } => {
            driver.replace(&db, &coll, &id, &doc).await?;
            Ok("document replaced".into())
        }
        DocWrite::Delete {
            db,
            coll,
            filter,
            many,
        } => {
            let n = driver.delete(&db, &coll, &filter, many).await?;
            Ok(format!("deleted {n} document(s)"))
        }
        // The DDL writes ride their own tools; not reachable from propose_doc_write.
        DocWrite::CreateCollection { .. }
        | DocWrite::DropCollection { .. }
        | DocWrite::CreateIndex { .. } => Ok("unsupported write".into()),
    }
}

/// `index_advice`: explain the filter, then report coverage / suggest a key.
/// Stop a running Mongo operation, re-resolving the opid against the live
/// deployment first. Same contract as [`kill_session`]: what the user approved
/// was a specific operation, and an opid can be reused, so the echoed facts are
/// verified rather than trusted.
async fn doc_kill_op(driver: &Arc<dyn DocDriver>, input: &Json) -> (String, bool) {
    let Some(opid) = input.get("opid").and_then(Json::as_i64) else {
        return ("error: doc_kill_op needs an `opid`".into(), false);
    };
    let ops = match driver.current_ops().await {
        Ok(o) => o,
        Err(e) => return (format!("error: {e}"), false),
    };
    let Some(live) = ops.iter().find(|o| o.opid == opid) else {
        return (
            format!("operation {opid} is no longer running; nothing to stop."),
            true,
        );
    };
    if let Some(claimed) = input
        .get("namespace")
        .and_then(Json::as_str)
        .filter(|n| !n.is_empty())
        && live.namespace != claimed
    {
        return (
            format!(
                "error: opid {opid} now runs on {}, not {claimed}: it was reused since you read \
                 it. Re-read doc_current_op and propose again.",
                live.namespace
            ),
            false,
        );
    }
    match driver.kill_op(opid).await {
        Ok(()) => (
            format!(
                "Stopped opid {opid} on {}. Mongo does not roll back a partially-applied \
                 multi-document write, so verify the data if it was one.",
                live.namespace
            ),
            true,
        ),
        Err(e) => (format!("error: {e}"), false),
    }
}

/// Documents sampled per collection when hunting reference candidates, and per
/// candidate field when collecting values to probe with. 200 is the plan's
/// number: enough that a hit rate means something, small enough that the whole
/// map is a handful of bounded reads.
const DOC_REF_SAMPLE: usize = 200;
/// Cap on candidate fields probed in one `doc_reference_map` call. Each is one
/// `find` plus one `count`, so this is what keeps the tool a map rather than a
/// crawl.
const DOC_REF_MAX_FIELDS: usize = 20;
/// Cap on collections whose schema is inferred in one call.
const DOC_REF_MAX_COLLECTIONS: usize = 25;
/// Databases that are the server's own bookkeeping, never a user's data model.
const DOC_SYSTEM_DBS: &[&str] = &["admin", "local", "config"];

/// One field that *looks* like a reference, with the target it would resolve
/// against. Built before any probing so the candidate list can be capped and
/// reported whole — including the ones that turn out to resolve nothing.
struct RefCandidate {
    coll: String,
    path: String,
    /// The collection the field name points at, already spelled as the catalog
    /// spells it.
    target: String,
    /// The dominant BSON type of the field, for the report (a `string` field
    /// pointing at an `objectId` `_id` explains a 0/200 on its own).
    doc_type: String,
}

/// The Mongo analogue of `relationship_map`: guess which fields reference other
/// collections from their names, then *test each guess* against the target's
/// `_id` and report the hit rate.
///
/// The hit rate is the entire point. A name-based guess alone is exactly the
/// failure mode this tool exists to prevent, so an unresolved candidate is
/// reported as unresolved and never silently dropped: an omission would read as
/// "no reference exists", which is the opposite of what was found.
async fn doc_reference_map(
    driver: &Arc<dyn DocDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    let abort = AbortSignal::new();
    let dbs: Vec<String> = match input
        .get("db")
        .and_then(Json::as_str)
        .filter(|d| !d.is_empty())
    {
        Some(d) => vec![d.to_string()],
        None => match driver.list_databases().await {
            Ok(list) => list
                .into_iter()
                .map(|d| d.name)
                .filter(|n| !DOC_SYSTEM_DBS.contains(&n.as_str()))
                .collect(),
            Err(e) => return (format!("error: {e}"), false),
        },
    };
    let wanted: Vec<String> = input
        .get("collections")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut out = String::new();
    for db in &dbs {
        let catalog: Vec<String> = match driver.list_collections(db).await {
            Ok(list) => list.into_iter().map(|c| c.name).collect(),
            Err(e) => {
                out.push_str(&format!("{db}: error: {e}\n"));
                continue;
            }
        };
        let scanned: Vec<&String> = catalog
            .iter()
            .filter(|c| wanted.is_empty() || wanted.iter().any(|w| w == *c))
            .take(DOC_REF_MAX_COLLECTIONS)
            .collect();
        let mut candidates: Vec<RefCandidate> = Vec::new();
        let mut truncated = scanned.len() < catalog.len();
        for coll in &scanned {
            let Ok(schema) = driver.infer_schema(db, coll, DOC_REF_SAMPLE, &abort).await else {
                continue;
            };
            candidates.extend(reference_candidates(coll, &schema, &catalog));
            // Stopping here leaves later collections unexamined whether or not the
            // count lands exactly on the cap, so the truncation is recorded from
            // the break rather than inferred from the length afterwards.
            if candidates.len() >= DOC_REF_MAX_FIELDS {
                truncated = true;
                break;
            }
        }
        candidates.truncate(DOC_REF_MAX_FIELDS);

        out.push_str(&format!(
            "{db} ({} of {} collection(s) sampled, {} candidate field(s)):\n",
            scanned.len(),
            catalog.len(),
            candidates.len(),
        ));
        if candidates.is_empty() {
            out.push_str(
                "  No field names suggest a reference. Mongo declares none, so if these \
                 collections are related the link is by a name this heuristic does not \
                 recognize.\n",
            );
        }
        for c in &candidates {
            out.push_str(&probe_reference(driver, db, c, &abort).await);
        }
        if truncated {
            out.push_str(&format!(
                "  …(stopped early, at {DOC_REF_MAX_FIELDS} candidate fields or \
                 {DOC_REF_MAX_COLLECTIONS} collections; narrow with `collections` for the rest)\n"
            ));
        }
    }
    (cap_result_bytes(out, limits.max_result_bytes), true)
}

/// The reference candidates in one collection's inferred schema: scalar fields
/// whose name points at a collection in `catalog`.
fn reference_candidates(coll: &str, schema: &DocSchema, catalog: &[String]) -> Vec<RefCandidate> {
    schema
        .fields
        .iter()
        .filter_map(|f| {
            let name = f.path.rsplit('.').next().unwrap_or(&f.path);
            // A reference is a scalar handle. A document or array field may
            // *contain* one, but the field itself is not it, and probing an
            // array against `_id` would report a meaningless 0.
            let (doc_type, _) = f.types.first()?;
            if matches!(doc_type, DocType::Object | DocType::Array | DocType::Null) {
                return None;
            }
            // `order_id` -> `order`, else the bare name (`customer` -> `customers`).
            let base = red_core::doc::reference_base(&f.path).unwrap_or(name);
            let target = red_core::doc::match_collection(base, catalog)?;
            Some(RefCandidate {
                coll: coll.to_string(),
                path: f.path.clone(),
                target: target.to_string(),
                doc_type: doc_type.label().to_string(),
            })
        })
        .collect()
}

/// Sample one candidate field's values and count how many resolve to a document
/// in the target collection. One `find` and one `count`, both bounded.
async fn probe_reference(
    driver: &Arc<dyn DocDriver>,
    db: &str,
    c: &RefCandidate,
    abort: &AbortSignal,
) -> String {
    let query = FindQuery {
        db: db.to_string(),
        coll: c.coll.clone(),
        // Ask only for the candidate path, so a wide document costs one field.
        projection: Some(DocValue::Document(vec![(
            c.path.clone(),
            DocValue::Int32(1),
        )])),
        filter: None,
        sort: None,
        skip: 0,
        limit: Some(DOC_REF_SAMPLE as u64),
        batch: DOC_REF_SAMPLE,
    };
    let page = match driver.find(&query, abort).await {
        Ok(p) => p,
        Err(e) => return format!("  {}.{} -> ? probe failed: {e}\n", c.coll, c.path),
    };
    let mut values: Vec<DocValue> = Vec::new();
    for doc in &page.docs {
        if let Some(v) = doc_path_value(doc, &c.path)
            && !matches!(v, DocValue::Null)
            && !values.contains(v)
        {
            values.push(v.clone());
        }
    }
    if values.is_empty() {
        return format!(
            "  {}.{} -> ? no values sampled ({}, {} doc(s) had no value here)\n",
            c.coll,
            c.path,
            c.doc_type,
            page.docs.len(),
        );
    }
    let sampled = values.len();
    let filter = DocValue::Document(vec![(
        "_id".into(),
        DocValue::Document(vec![("$in".into(), DocValue::Array(values))]),
    )]);
    match driver.count(db, &c.target, Some(&filter)).await {
        Ok(0) => format!(
            "  {}.{} -> ? UNRESOLVED ({}, 0/{sampled} sampled values match any {}._id)\n",
            c.coll, c.path, c.doc_type, c.target,
        ),
        Ok(hits) => format!(
            "  {}.{} -> {}._id ({}, {hits}/{sampled} sampled values resolve)\n",
            c.coll, c.path, c.target, c.doc_type,
        ),
        Err(e) => format!(
            "  {}.{} -> {}._id probe failed: {e}\n",
            c.coll, c.path, c.target
        ),
    }
}

/// The value at a dotted `path` in a document, descending sub-documents. `_id`
/// is held beside the fields, so it is resolved explicitly rather than searched
/// for. `None` when any segment is missing or the path runs into a scalar.
fn doc_path_value<'a>(doc: &'a Document, path: &str) -> Option<&'a DocValue> {
    let mut segments = path.split('.');
    let first = segments.next()?;
    let mut current = if first == "_id" {
        &doc.id
    } else {
        doc.fields
            .iter()
            .find(|(k, _)| k == first)
            .map(|(_, v)| v)?
    };
    for segment in segments {
        let DocValue::Document(fields) = current else {
            return None;
        };
        current = fields.iter().find(|(k, _)| k == segment).map(|(_, v)| v)?;
    }
    Some(current)
}

async fn doc_index_advice(driver: &Arc<dyn DocDriver>, input: &Json) -> (String, bool) {
    let db = input.get("db").and_then(Json::as_str).unwrap_or("");
    let coll = input.get("coll").and_then(Json::as_str).unwrap_or("");
    let filter = match doc_arg_value(driver, input, "filter") {
        Ok(v) => v,
        Err(e) => return (format!("error: {e}"), false),
    };
    let fields: Vec<String> = match &filter {
        Some(DocValue::Document(f)) => f
            .iter()
            .map(|(k, _)| k.clone())
            .filter(|k| !k.starts_with('$'))
            .collect(),
        _ => Vec::new(),
    };
    let query = FindQuery {
        db: db.to_string(),
        coll: coll.to_string(),
        filter,
        projection: None,
        sort: None,
        skip: 0,
        limit: None,
        batch: 1,
    };
    match driver.explain(&query).await {
        Ok(plan) => {
            if !plan.collscan {
                let idx = plan.index_used.as_deref().unwrap_or("an index");
                (
                    format!("Covered: the query uses {idx}. No new index needed."),
                    true,
                )
            } else if fields.is_empty() {
                (
                    "COLLSCAN, but the filter has no fields to index (it matches everything)."
                        .into(),
                    true,
                )
            } else {
                let spec = fields
                    .iter()
                    .map(|f| format!("\"{f}\": 1"))
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    format!(
                        "COLLSCAN — no index covers this filter. Suggested index on {db}.{coll}: \
                         {{ {spec} }}. Propose it with propose_index if the user wants it."
                    ),
                    true,
                )
            }
        }
        Err(e) => (format!("error: {e}"), false),
    }
}

/// `audit_collection`: sample the schema + read indexes, roll into a health report.
async fn doc_audit_collection(
    driver: &Arc<dyn DocDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    let db = input.get("db").and_then(Json::as_str).unwrap_or("");
    let coll = input.get("coll").and_then(Json::as_str).unwrap_or("");
    let abort = AbortSignal::new();
    let schema = match driver.infer_schema(db, coll, 200, &abort).await {
        Ok(s) => s,
        Err(e) => return (format!("error: {e}"), false),
    };
    let indexes = driver.indexes(db, coll).await.unwrap_or_default();
    let count = driver.count(db, coll, None).await.ok();

    let drift: Vec<String> = schema
        .fields
        .iter()
        .filter(|f| f.types.len() > 1)
        .map(|f| {
            let types = f
                .types
                .iter()
                .map(|(t, _)| t.label())
                .collect::<Vec<_>>()
                .join("/");
            format!("{} ({types})", f.path)
        })
        .collect();
    let sparse: Vec<String> = schema
        .fields
        .iter()
        .filter(|f| f.present_ratio < 0.9 && f.path != "_id")
        .map(|f| format!("{} ({:.0}%)", f.path, f.present_ratio * 100.0))
        .collect();

    let mut out = format!("Health report for {db}.{coll}");
    if let Some(n) = count {
        out.push_str(&format!(" (~{n} documents)"));
    }
    out.push_str(":\n");
    out.push_str(&format!(
        "- Schema drift (mixed-type fields): {}\n",
        if drift.is_empty() {
            "none".into()
        } else {
            drift.join(", ")
        }
    ));
    out.push_str(&format!(
        "- Optional/sparse fields (present <90%): {}\n",
        if sparse.is_empty() {
            "none".into()
        } else {
            sparse.join(", ")
        }
    ));
    let secondary = indexes.iter().filter(|ix| ix.name != "_id_").count();
    out.push_str(&format!(
        "- Indexes: {} ({} secondary){}\n",
        indexes.len(),
        secondary,
        if secondary == 0 {
            " — only the default _id index; unindexed filters will collection-scan"
        } else {
            ""
        }
    ));
    (cap_result_bytes(out, limits.max_result_bytes), true)
}

/// Render an inferred schema for `describe_collection`.
fn fmt_doc_schema(schema: &DocSchema) -> String {
    let mut out = format!("Inferred schema ({} documents sampled):\n", schema.sampled);
    for f in &schema.fields {
        out.push_str(&format!("  {} — {}\n", f.path, fmt_doc_types(f)));
    }
    out
}

/// Render an inferred schema as a profile (emphasizing drift + presence).
fn fmt_doc_profile(schema: &DocSchema) -> String {
    let mut out = format!("Field profile ({} documents sampled):\n", schema.sampled);
    for f in &schema.fields {
        out.push_str(&format!(
            "  {} — {} · present {:.0}%\n",
            f.path,
            fmt_doc_types(f),
            f.present_ratio * 100.0
        ));
    }
    out
}

/// A field's type distribution as `string 82%, int 18%`.
fn fmt_doc_types(f: &red_core::doc::FieldStat) -> String {
    let total: u64 = f.types.iter().map(|(_, c)| c).sum();
    f.types
        .iter()
        .map(|(t, c)| {
            let pct = (c * 100).checked_div(total).unwrap_or(0);
            format!("{} {pct}%", t.label())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render an index list.
fn fmt_doc_indexes(indexes: &[IndexInfo]) -> String {
    if indexes.is_empty() {
        return "  (none)".into();
    }
    indexes
        .iter()
        .map(|ix| {
            let keys = ix
                .keys
                .iter()
                .map(|(f, o)| format!("{f}: {o}"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut props = Vec::new();
            if ix.unique {
                props.push("unique");
            }
            if ix.sparse {
                props.push("sparse");
            }
            if ix.partial {
                props.push("partial");
            }
            let ttl = ix.ttl.map(|t| format!(" ttl={t}s")).unwrap_or_default();
            format!("  {} {{ {keys} }} {}{ttl}", ix.name, props.join(","))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render a list of documents as one extended-JSON line each.
fn fmt_doc_list(docs: &[Document]) -> String {
    if docs.is_empty() {
        return "(no documents)".into();
    }
    docs.iter()
        .map(|d| d.to_doc_value().to_extended_json())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render an explain plan.
fn fmt_doc_plan(plan: &DocPlan) -> String {
    let mut out = String::new();
    if plan.collscan {
        out.push_str("COLLSCAN — no index used\n");
    } else if let Some(ix) = &plan.index_used {
        out.push_str(&format!("uses index {ix}\n"));
    }
    if let (Some(e), Some(r)) = (plan.docs_examined, plan.n_returned) {
        out.push_str(&format!("examined {e}, returned {r}\n"));
    }
    let stages = plan
        .stages
        .iter()
        .map(|s| s.stage.clone())
        .collect::<Vec<_>>()
        .join(" > ");
    out.push_str(&format!("winning plan: {stages}"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_core::doc::DocPage;

    /// The gate tests probe statement shapes, not dialect lexing, so they run
    /// under [`Dialect::Generic`]; the dialect-sensitive cases have their own
    /// tests below.
    fn is_read_only_select(sql: &str) -> bool {
        super::is_read_only_select(sql, Dialect::Generic)
    }

    fn assess_write(name: &str, input: &Json, policy: &AiPolicy) -> WriteAssessment {
        super::assess_write(name, input, policy, Dialect::Generic)
    }

    /// Postgres does not backslash-escape in a plain literal, so `'a\'` is a
    /// complete string and the `DELETE` after it is live SQL: the gate must see
    /// it. Under the old unconditional-backslash lexing the whole payload
    /// blanked as one string and *passed* the read gate.
    #[test]
    fn read_gate_lexes_strings_per_dialect() {
        let payload = "SELECT 'a\\'; DELETE FROM t; --'";
        assert!(!super::is_read_only_select(payload, Dialect::Postgres));
        // Under MySQL the backslash escapes the quote, so it really is one
        // SELECT with a string argument — allowed.
        assert!(super::is_read_only_select(payload, Dialect::MySql));
    }

    #[test]
    fn whole_keyspace_glob_catches_wildcard_equivalents() {
        // No literal anchor: matches essentially every key, so refused.
        for p in ["*", "**", "?*", "*?", "[a-z]*", "?", "[^a]"] {
            assert!(matches_whole_keyspace(p), "{p} should be whole-keyspace");
        }
        // A literal anchor narrows it: allowed (rides the normal approval gate).
        for p in ["user:*", "a*", "*:cache", "[a-z]_role", "\\*", "session:?"] {
            assert!(
                !matches_whole_keyspace(p),
                "{p} should NOT be whole-keyspace"
            );
        }
    }

    #[test]
    fn kv_delete_star_equivalents_are_rejected_not_prompted() {
        // The literal `*` and its glob-equivalents are hard-refused, never a
        // NeedsApproval the user could wave through.
        for p in ["*", "?*", "[a-z]*"] {
            assert!(
                matches!(
                    assess_kv_write("kv_delete", &json!({ "pattern": p })),
                    WriteAssessment::Reject(_)
                ),
                "kv_delete pattern `{p}` must be rejected"
            );
        }
        // A scoped pattern still prompts.
        assert!(matches!(
            assess_kv_write("kv_delete", &json!({ "pattern": "user:*" })),
            WriteAssessment::NeedsApproval { .. }
        ));
    }

    /// The approval prompt must be the command list itself, not a paraphrase:
    /// the user allows `HSET user:1 name "Ada"`, so that is what runs.
    #[test]
    fn kv_set_prompt_shows_the_literal_commands() {
        let detail = |input: Json| match assess_kv_write("kv_set", &input) {
            WriteAssessment::NeedsApproval { sql } => sql,
            other => panic!(
                "expected NeedsApproval, got {}",
                match other {
                    WriteAssessment::Reject(w) => format!("Reject({w})"),
                    _ => "NotWrite".into(),
                }
            ),
        };
        assert_eq!(
            detail(json!({ "key": "user:1:name", "type": "string", "value": "Ada" })),
            "SET user:1:name Ada"
        );
        // A value needing quoting gets it, and an explicit TTL rides inside SET as
        // the `PX` the driver actually sends.
        assert_eq!(
            detail(json!({
                "key": "greeting", "type": "string", "value": "hello world", "ttl_seconds": 60
            })),
            "SET greeting \"hello world\" PX 60000"
        );
        assert_eq!(
            detail(json!({ "key": "user:1", "type": "hash", "field": "name", "value": "Ada" })),
            "HSET user:1 name Ada"
        );
        // A collection replace is a DEL and a rebuild; the DEL is shown, never implied.
        let zset = detail(json!({
            "key": "board", "type": "zset", "value": { "ada": 1.5 }, "ttl_seconds": 30
        }));
        assert_eq!(zset, "DEL board\nZADD board 1.5 ada\nPEXPIRE board 30000");
        // `append` leaves what is there alone.
        assert_eq!(
            detail(json!({ "key": "q", "type": "list", "value": ["a"], "mode": "append" })),
            "RPUSH q a"
        );
        // A stream always appends: no DEL, whatever `mode` says.
        assert_eq!(
            detail(json!({
                "key": "events", "type": "stream", "value": { "kind": "signup" }, "mode": "set"
            })),
            "XADD events * kind signup"
        );
    }

    #[test]
    fn kv_set_refuses_bad_keys_and_oversized_payloads() {
        let rejected = |input: Json| {
            matches!(
                assess_kv_write("kv_set", &input),
                WriteAssessment::Reject(_)
            )
        };
        // An empty key, and a glob the model mistook for one.
        assert!(rejected(
            json!({ "key": "", "type": "string", "value": "x" })
        ));
        assert!(rejected(
            json!({ "key": "  ", "type": "string", "value": "x" })
        ));
        for pattern in ["*", "?*", "[a-z]*"] {
            assert!(
                rejected(json!({ "key": pattern, "type": "string", "value": "x" })),
                "key `{pattern}` must be refused"
            );
        }
        // Oversized: one huge value, and too many small ones. Both refused at the
        // gate, so neither reaches the driver.
        let huge = "x".repeat(KV_SET_VALUE_MAX + 1);
        assert!(rejected(
            json!({ "key": "k", "type": "string", "value": huge })
        ));
        let many: Vec<String> = (0..KV_SET_ELEMS_MAX + 1).map(|i| i.to_string()).collect();
        assert!(rejected(
            json!({ "key": "k", "type": "set", "value": many })
        ));
        // Shape errors are rejections too, never a prompt.
        assert!(rejected(
            json!({ "key": "k", "type": "trie", "value": "x" })
        ));
        assert!(rejected(
            json!({ "key": "k", "type": "zset", "value": { "a": "not-a-score" } })
        ));
        assert!(rejected(
            json!({ "key": "k", "type": "string", "value": { "nested": 1 } })
        ));
        assert!(rejected(json!({ "key": "k", "type": "set", "value": [] })));
    }

    #[test]
    fn kv_set_is_a_gated_write_at_write_tier_only() {
        assert!(is_write_tool("kv_set"));
        assert!(!AiTier::Read.allows_tool("kv_set"));
        assert!(AiTier::Write.allows_tool("kv_set"));
        let names = |p: AiPolicy| {
            kv_tool_catalog(&p)
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        };
        assert!(names(AiPolicy::default()).iter().all(|n| n != "kv_set"));
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        assert!(names(write).iter().any(|n| n == "kv_set"));
        // A read-only connection withholds it, and a subagent never gets it.
        let write_ro = AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        };
        assert!(names(write_ro).iter().all(|n| n != "kv_set"));
        assert!(
            kv_subagent_catalog(&AiPolicy {
                tier: AiTier::Write,
                ..AiPolicy::default()
            })
            .iter()
            .all(|t| t.name != "kv_set")
        );
        // Never advertised over the headless transport either (it is a write).
        assert!(!is_headless_tool("kv_set"));
    }

    #[test]
    fn config_get_secrets_are_redacted() {
        for p in ["requirepass", "masterauth", "masteruser", "primarypass"] {
            assert!(is_secret_config_param(p), "{p} should be secret");
        }
        for p in ["maxmemory", "dir", "save"] {
            assert!(!is_secret_config_param(p), "{p} should not be secret");
        }
    }

    #[test]
    fn tool_args_summary_pulls_the_salient_scalar() {
        assert_eq!(
            summarize_tool_args("run_select", &json!({ "sql": "SELECT 1\nFROM t" })),
            Some("SELECT 1".to_string())
        );
        assert_eq!(
            summarize_tool_args("describe_table", &json!({ "table": "public.users" })),
            Some("public.users".to_string())
        );
        // Leading blank lines are skipped; the first non-empty line wins.
        assert_eq!(
            summarize_tool_args("propose_write", &json!({ "sql": "\n  UPDATE t SET x=1" })),
            Some("UPDATE t SET x=1".to_string())
        );
        // A tool with no salient scalar (or a missing field) summarizes to nothing.
        assert_eq!(summarize_tool_args("list_schema", &json!({})), None);
        assert_eq!(summarize_tool_args("run_select", &json!({})), None);
    }

    #[test]
    fn activity_detail_summarizes_success_and_surfaces_errors() {
        // Failure: the error's first line, for any tool.
        assert_eq!(
            activity_detail(
                "run_select",
                false,
                "error: relation \"t\" does not exist\nctx…"
            ),
            Some("error: relation \"t\" does not exist".to_string())
        );
        // run_select success → the trailing "(N rows)" count, ignoring a truncation note.
        assert_eq!(
            activity_detail(
                "run_select",
                true,
                "a | b\n1 | 2\n(3 rows)\n(truncated to 3 rows)"
            ),
            Some("3 rows".to_string())
        );
        // profile_table success → the header's "N rows".
        assert_eq!(
            activity_detail(
                "profile_table",
                true,
                "Profile of main.t — 42 rows\n\nColumns:"
            ),
            Some("42 rows".to_string())
        );
        // A write tool's one-line summary is surfaced verbatim.
        assert_eq!(
            activity_detail(
                "propose_write",
                true,
                "Executed the write: 2 row(s) affected."
            ),
            Some("Executed the write: 2 row(s) affected.".to_string())
        );
        // A tool with no concise success signal shows nothing (the ✓ glyph suffices).
        assert_eq!(activity_detail("list_schema", true, "schemas…"), None);
    }

    #[test]
    fn summary_truncation_is_char_safe_and_marked() {
        let long = "x".repeat(200);
        let out = truncate_summary(&long, 80);
        assert_eq!(out.chars().count(), 80);
        assert!(out.ends_with('…'));
        // Multibyte input never splits a codepoint.
        let emoji = "😀".repeat(100);
        let out = truncate_summary(&emoji, 10);
        assert_eq!(out.chars().count(), 10);
    }

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
    fn read_only_gate_rejects_data_modifying_ctes_and_select_into() {
        // A data-modifying CTE leads with WITH but Postgres executes the DELETE.
        assert!(!is_read_only_select(
            "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x"
        ));
        assert!(!is_read_only_select(
            "with g as (update t set a=1 returning id) select * from g"
        ));
        assert!(!is_read_only_select(
            "WITH n AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM n"
        ));
        // SELECT … INTO (Postgres creates a table) / INTO OUTFILE (MySQL writes a file).
        assert!(!is_read_only_select("SELECT * INTO new_t FROM t"));
        assert!(!is_read_only_select(
            "SELECT * FROM t INTO OUTFILE '/tmp/x'"
        ));
        // Sequence-advancing functions write.
        assert!(!is_read_only_select("SELECT nextval('s')"));
        assert!(!is_read_only_select("select setval('s', 1)"));
        // Server-side functions that read/write files or run remote SQL are refused.
        assert!(!is_read_only_select("SELECT lo_import('/etc/passwd')"));
        assert!(!is_read_only_select("SELECT pg_read_file('/etc/passwd')"));
        assert!(!is_read_only_select(
            "SELECT dblink_exec('dbname=x', 'DELETE FROM t')"
        ));
        // Bare and async `dblink` run arbitrary remote SQL just like `dblink_exec`.
        assert!(!is_read_only_select(
            "SELECT * FROM dblink('dbname=x', 'DELETE FROM t RETURNING id') AS r(id int)"
        ));
        assert!(!is_read_only_select(
            "SELECT dblink_send_query('c', 'DELETE FROM t')"
        ));
        assert!(!is_read_only_select("select load_file('/etc/passwd')"));
        // A write keyword merely *inside a literal or quoted identifier* is harmless
        // and must NOT block a real read (noise is stripped before the check).
        assert!(is_read_only_select("SELECT 'delete me' AS note FROM t"));
        assert!(is_read_only_select(r#"SELECT "update" FROM t"#));
        assert!(is_read_only_select("SELECT id FROM t WHERE c = 'a;b'"));
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
        // Schema tier: structure only. `object_ddl` and `relationship_map` belong
        // here because a definition and a declared constraint are catalog facts,
        // not rows.
        assert_eq!(
            names(AiTier::Schema),
            [
                "list_schema",
                "describe_table",
                "object_ddl",
                "relationship_map"
            ]
        );
        assert_eq!(
            names(AiTier::Read),
            [
                "list_schema",
                "describe_table",
                "object_ddl",
                "relationship_map",
                "profile_table",
                "run_select",
                "search_data",
                "explain",
                "health_report",
                "server_sessions",
                "diff_schema",
                "diff_data",
                "suggest_index",
                "export_result",
                "generate_report",
                "open_query",
                "save_query",
                "spawn_subagent"
            ]
        );
    }

    #[test]
    fn subagent_catalog_is_read_only_and_non_recursive() {
        use red_core::{AiPolicy, AiTier};
        // Even from a Write-tier parent, the child gets no write tool and cannot
        // spawn further subagents.
        let names: Vec<String> = subagent_catalog(&AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        })
        .into_iter()
        .map(|t| t.name)
        .collect();
        assert!(!names.iter().any(|n| n == "propose_write"));
        assert!(!names.iter().any(|n| n == "spawn_subagent"));
        assert!(names.iter().any(|n| n == "run_select"));

        // The Redis subagent catalog is likewise read-only and non-recursive:
        // no KV writes, no spawn_subagent, but the KV read tools survive.
        let kv: Vec<String> = kv_subagent_catalog(&AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        })
        .into_iter()
        .map(|t| t.name)
        .collect();
        assert!(!kv.iter().any(|n| n == "kv_delete"));
        assert!(!kv.iter().any(|n| n == "spawn_subagent"));
        assert!(kv.iter().any(|n| n == "kv_scan_keys"));
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
                "object_ddl",
                "relationship_map",
                "profile_table",
                "run_select",
                "search_data",
                "explain",
                "health_report",
                "server_sessions",
                "diff_schema",
                "diff_data",
                "suggest_index",
                "export_result",
                "generate_report",
                "open_query",
                "save_query",
                "spawn_subagent"
            ]
        );
    }

    #[test]
    fn changeset_assessment_gates_shape_tier_and_read_only() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        let ok = json!({ "statements": [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1 WHERE id = 1",
        ] });

        // A valid set at the Write tier needs approval; the prompt body numbers each.
        match assess_write("propose_changeset", &ok, &write) {
            WriteAssessment::NeedsApproval { sql } => {
                assert!(sql.contains("1. INSERT"), "got: {sql}");
                assert!(sql.contains("2. UPDATE"), "got: {sql}");
            }
            _ => panic!("expected NeedsApproval for a valid changeset"),
        }

        // Below the Write tier the whole tool is refused.
        assert!(matches!(
            assess_write("propose_changeset", &ok, &AiPolicy::default()),
            WriteAssessment::Reject(_)
        ));
        // A read-only connection refuses even at the Write tier.
        let read_only = AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        };
        assert!(matches!(
            assess_write("propose_changeset", &ok, &read_only),
            WriteAssessment::Reject(_)
        ));
        // One bad statement (DDL) rejects the whole set — it's atomic.
        let ddl = json!({ "statements": ["INSERT INTO t VALUES (1)", "DROP TABLE t"] });
        assert!(matches!(
            assess_write("propose_changeset", &ddl, &write),
            WriteAssessment::Reject(_)
        ));
        // An unqualified UPDATE/DELETE is blocked.
        let nowhere = json!({ "statements": ["DELETE FROM t"] });
        assert!(matches!(
            assess_write("propose_changeset", &nowhere, &write),
            WriteAssessment::Reject(_)
        ));
        // An empty set is refused.
        let empty = json!({ "statements": [] });
        assert!(matches!(
            assess_write("propose_changeset", &empty, &write),
            WriteAssessment::Reject(_)
        ));
    }

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
            Dialect::Sqlite,
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

    #[tokio::test]
    async fn generate_report_wraps_ai_html_and_announces_it() {
        use futures::StreamExt;

        // generate_report renders model-authored HTML (no DB call). A no-op driver is
        // enough (the tool never touches it).
        let db = std::env::temp_dir().join(format!("red-gr-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, ConversationId::new(7), None, None);

        let (content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
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
            ..
        } = event
        else {
            panic!("expected AiReportReady");
        };
        assert_eq!(conversation_id.get(), 7);
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
            Dialect::Sqlite,
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
    async fn generate_report_writes_to_the_configured_folder() {
        use futures::StreamExt;

        let db =
            std::env::temp_dir().join(format!("red-grd2-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        // A folder that doesn't exist yet: `output_dir` must create it on demand rather
        // than dropping the report into the temp dir.
        let out =
            std::env::temp_dir().join(format!("red-reports-{}", uuid::Uuid::new_v4().simple()));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, ConversationId::new(21), None, Some(out.clone()));

        let (_content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
            "generate_report",
            &json!({ "title": "Here", "html": "<h1>Here</h1>" }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(ok, "expected the report to be generated");

        let (_session, event) = rx.next().await.expect("an AiReportReady event");
        let Event::AiReportReady { path, .. } = event else {
            panic!("expected AiReportReady");
        };
        assert!(
            std::path::Path::new(&path).starts_with(&out),
            "report {path} should live under the configured folder {}",
            out.display()
        );
        assert!(
            out.is_dir(),
            "the configured folder should be created on demand"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    #[tokio::test]
    async fn generate_report_with_charts_is_nonce_gated_and_egress_free() {
        use futures::StreamExt;

        let db = std::env::temp_dir().join(format!("red-grc-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, ConversationId::new(11), None, None);

        let (content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
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
        let sink = ReportSink::new(tx, None, ConversationId::new(13), None, None);

        let (content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
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
        let sink = ReportSink::new(tx, None, ConversationId::new(17), None, None);

        let (content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
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
        // A `where` inside a string literal or comment is NOT a real WHERE; the
        // statement is still an unqualified mutation and must be blocked.
        assert!(rejected("UPDATE t SET note = 'see where you go'"));
        assert!(rejected("DELETE FROM t -- delete where id = 1"));
        // Conversely, a real WHERE with a `;` inside a string literal is a single,
        // qualified statement: allowed (the `;` isn't statement chaining).
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
        assert!(
            names(AiPolicy::default())
                .iter()
                .all(|n| n != "propose_write")
        );
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
    fn kv_read_tools_are_not_gated_as_writes() {
        // Regression guard: the KV read tools must be in READ_ONLY_TOOLS, else the
        // write gate would reject every one of them at Read tier.
        let read = AiPolicy::default();
        for t in [
            "kv_server_info",
            "kv_scan_keys",
            "kv_key_info",
            "kv_get_value",
            "kv_biggest_keys",
            "kv_analyze",
            "kv_slowlog",
            "kv_config_get",
        ] {
            assert!(!is_write_tool(t), "{t} must be read-only");
            assert!(
                matches!(
                    assess_write(t, &json!({}), &read),
                    WriteAssessment::NotWrite
                ),
                "{t} must not be gated as a write"
            );
        }
    }

    #[test]
    fn doc_read_tools_are_not_gated_as_writes() {
        // Every doc read tool must be in READ_ONLY_TOOLS, else the write gate
        // would reject it at Read tier.
        let read = AiPolicy::default();
        for t in [
            "doc_server_info",
            "list_collections",
            "describe_collection",
            "profile_collection",
            "sample_documents",
            "find",
            "aggregate",
            "count",
            "distinct",
            "explain_query",
            "index_advice",
            "audit_collection",
        ] {
            assert!(!is_write_tool(t), "{t} must be read-only");
            assert!(
                matches!(
                    assess_write(t, &json!({}), &read),
                    WriteAssessment::NotWrite
                ),
                "{t} must not be gated as a write"
            );
        }
    }

    #[test]
    fn doc_catalog_offers_writes_only_at_write_tier_and_not_read_only() {
        let names = |p: AiPolicy| {
            doc_tool_catalog(&p)
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        };
        // Read tier: the reads (incl. the signature tools), no write tools.
        let read = names(AiPolicy::default());
        assert!(read.iter().any(|n| n == "find"));
        assert!(read.iter().any(|n| n == "profile_collection"));
        assert!(read.iter().all(|n| n != "propose_doc_write"));
        // Write tier offers the gated writes…
        let write = names(AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        });
        assert!(write.iter().any(|n| n == "propose_doc_write"));
        assert!(write.iter().any(|n| n == "propose_collection_op"));
        // …but withholds them on a read-only connection.
        let write_ro = names(AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        });
        assert!(write_ro.iter().all(|n| n != "propose_doc_write"));
        assert!(write_ro.iter().any(|n| n == "find"));
    }

    #[test]
    fn doc_write_gate_requires_filter_and_confirms_drop() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        // An unfiltered update/delete is refused outright, even with approval.
        assert!(matches!(
            assess_write(
                "propose_doc_write",
                &json!({ "op": "delete", "db": "d", "coll": "c" }),
                &write
            ),
            WriteAssessment::Reject(_)
        ));
        // A filtered delete prompts.
        assert!(matches!(
            assess_write(
                "propose_doc_write",
                &json!({ "op": "delete", "db": "d", "coll": "c", "filter": { "_id": 1 } }),
                &write
            ),
            WriteAssessment::NeedsApproval { .. }
        ));
        // An insert prompts without a filter (nothing to over-match).
        assert!(matches!(
            assess_write(
                "propose_doc_write",
                &json!({ "op": "insert", "db": "d", "coll": "c" }),
                &write
            ),
            WriteAssessment::NeedsApproval { .. }
        ));
        // Dropping a collection prompts (the approval string carries the warning).
        assert!(matches!(
            assess_write(
                "propose_collection_op",
                &json!({ "op": "drop", "db": "d", "coll": "c" }),
                &write
            ),
            WriteAssessment::NeedsApproval { .. }
        ));
        // Below Write tier, every doc write is rejected without a prompt.
        let read = AiPolicy::default();
        assert!(matches!(
            assess_write(
                "propose_doc_write",
                &json!({ "op": "insert", "db": "d", "coll": "c" }),
                &read
            ),
            WriteAssessment::Reject(_)
        ));
    }

    #[test]
    fn doc_subagent_catalog_is_read_only_and_non_recursive() {
        let doc: Vec<String> = doc_subagent_catalog(&AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        })
        .into_iter()
        .map(|t| t.name)
        .collect();
        assert!(!doc.iter().any(|n| n == "propose_doc_write"));
        assert!(!doc.iter().any(|n| n == "spawn_subagent"));
        assert!(doc.iter().any(|n| n == "find"));
    }

    #[test]
    fn headless_transport_keeps_reads_drops_writes_and_gui_tools() {
        // The `red mcp` stdio transport advertises/runs only DB reads that work
        // without the GUI: writes and the UI-bound reads are withheld.
        assert!(is_headless_tool("run_select"));
        assert!(is_headless_tool("list_schema"));
        assert!(is_headless_tool("kv_get_value"));
        // Writes stay out (they're not in READ_ONLY_TOOLS).
        assert!(!is_headless_tool("propose_write"));
        assert!(!is_headless_tool("kv_delete"));
        // GUI-only reads are withheld even though they don't mutate the DB.
        for t in UI_ONLY_TOOLS {
            assert!(
                !is_headless_tool(t),
                "{t} needs the GUI; withhold it headless"
            );
        }
    }

    #[test]
    fn kv_write_gate_prompts_shapes_and_refuses_keyspace_wide() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        // A single-key or scoped-pattern op prompts for approval.
        assert!(matches!(
            assess_write("kv_delete", &json!({ "key": "user:1" }), &write),
            WriteAssessment::NeedsApproval { .. }
        ));
        assert!(matches!(
            assess_write("kv_delete", &json!({ "pattern": "session:*" }), &write),
            WriteAssessment::NeedsApproval { .. }
        ));
        assert!(matches!(
            assess_write("kv_expire", &json!({ "key": "k", "seconds": 60 }), &write),
            WriteAssessment::NeedsApproval { .. }
        ));
        assert!(matches!(
            assess_write("kv_rename", &json!({ "from": "a", "to": "b" }), &write),
            WriteAssessment::NeedsApproval { .. }
        ));
        // `key` and `keys` passed together: the prompt must name every target
        // the executor will delete, not just `key` — the drift that let 50k
        // keys ride behind a one-key approval.
        match assess_write(
            "kv_delete",
            &json!({ "key": "scratch:1", "keys": ["a", "b", "c"] }),
            &write,
        ) {
            WriteAssessment::NeedsApproval { sql } => {
                for k in ["scratch:1", "a", "b", "c"] {
                    assert!(sql.contains(k), "prompt must name `{k}`: {sql}");
                }
                assert!(sql.contains("4 key(s)"), "{sql}");
            }
            WriteAssessment::Reject(why) => panic!("expected NeedsApproval, got Reject({why})"),
            WriteAssessment::NotWrite => panic!("expected NeedsApproval, got NotWrite"),
        }
        // Keyspace-wide delete/expire is refused outright, even at Write tier.
        assert!(matches!(
            assess_write("kv_delete", &json!({ "pattern": "*" }), &write),
            WriteAssessment::Reject(_)
        ));
        assert!(matches!(
            assess_write(
                "kv_expire",
                &json!({ "pattern": "*", "seconds": 60 }),
                &write
            ),
            WriteAssessment::Reject(_)
        ));
        // The write tools are rejected below Write tier and on a read-only conn.
        assert!(matches!(
            assess_write("kv_delete", &json!({ "key": "k" }), &AiPolicy::default()),
            WriteAssessment::Reject(_)
        ));
        let write_ro = AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        };
        assert!(matches!(
            assess_write("kv_delete", &json!({ "key": "k" }), &write_ro),
            WriteAssessment::Reject(_)
        ));
    }

    #[test]
    fn kv_catalog_offers_writes_only_at_write_tier_and_not_read_only() {
        let names = |p: AiPolicy| {
            kv_tool_catalog(&p)
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        };
        // Read tier: reads only, no write tools.
        let read = names(AiPolicy::default());
        assert!(read.iter().any(|n| n == "kv_scan_keys"));
        assert!(read.iter().all(|n| n != "kv_delete"));
        // Write tier offers the write tools…
        let write = names(AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        });
        assert!(write.iter().any(|n| n == "kv_delete"));
        assert!(write.iter().any(|n| n == "kv_config_set"));
        // …but withholds them on a read-only connection.
        let write_ro = names(AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        });
        assert!(write_ro.iter().all(|n| n != "kv_delete"));
        assert!(write_ro.iter().any(|n| n == "kv_scan_keys"));
    }

    #[test]
    fn write_approval_registry_parks_resolves_and_offsets_ids() {
        let mut st = AiState::default();
        let (tx, mut rx) = oneshot::channel();
        let id = st.park_permission(tx).expect("a fresh prompt parks");
        // Ids are offset so they never collide with the ACP manager's id space.
        assert!(id.get() >= AI_REQUEST_BASE);
        st.resolve_permission(id, true);
        assert_eq!(rx.try_recv(), Ok(true));
        // Resolving a stale/unknown id is a harmless no-op.
        st.resolve_permission(id, false);
        st.resolve_permission(RequestId::new(424242), false);
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
            _req: &red_ai::TurnRequest<'_>,
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
            AiBackend::Sql {
                driver: driver.clone(),
                dialect: Dialect::Sqlite,
            },
            tx,
            state.clone(),
            None,
            ConversationId::new(1),
            "m".into(),
            false,
            policy,
            "change it".into(),
            AiContext::default(),
            CancelToken::new(),
        ));

        // The first thing the user sees is the write-approval prompt, carrying the
        // exact SQL; the write has NOT run yet.
        let request_id = tokio::time::timeout(Duration::from_secs(5), async {
            // The very first event must be the approval prompt; nothing runs first.
            match rx.next().await.expect("an event").1 {
                Event::AiPermissionRequest {
                    request_id, detail, ..
                } => {
                    assert!(
                        detail
                            .unwrap_or_default()
                            .contains("UPDATE t SET name = 'after'")
                    );
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

    /// A minimal in-memory `DocDriver` for the doc-seam tools. Purpose-built for
    /// what they actually exercise — the catalog, an inferred schema, a windowed
    /// `find`, and a **filtered** `count` — because a `count` that ignores its
    /// filter would report every reference as fully resolving, which is exactly
    /// the failure `doc_reference_map` exists to catch.
    struct DocStub {
        colls: Vec<(String, Vec<Document>)>,
    }

    impl DocStub {
        fn docs(&self, coll: &str) -> &[Document] {
            self.colls
                .iter()
                .find(|(name, _)| name == coll)
                .map(|(_, docs)| docs.as_slice())
                .unwrap_or(&[])
        }
    }

    #[async_trait::async_trait]
    impl DocDriver for DocStub {
        async fn ping(&self) -> red_core::Result<()> {
            Ok(())
        }
        fn server_version(&self) -> String {
            "7.0.0".into()
        }
        fn topology(&self) -> red_core::doc::DocTopology {
            red_core::doc::DocTopology::Standalone
        }
        async fn list_databases(&self) -> red_core::Result<Vec<red_core::doc::DbInfo>> {
            Ok(vec![red_core::doc::DbInfo {
                name: "app".into(),
                size_on_disk: 0,
                empty: false,
            }])
        }
        async fn list_collections(
            &self,
            _db: &str,
        ) -> red_core::Result<Vec<red_core::doc::CollectionInfo>> {
            Ok(self
                .colls
                .iter()
                .map(|(name, docs)| red_core::doc::CollectionInfo {
                    name: name.clone(),
                    kind: CollKind::Collection,
                    est_count: docs.len() as u64,
                    size: 0,
                    capped: false,
                    validator: None,
                })
                .collect())
        }
        async fn find(&self, q: &FindQuery, _abort: &AbortSignal) -> red_core::Result<DocPage> {
            let all = self.docs(&q.coll);
            let take = q.limit.map(|l| l as usize).unwrap_or(q.batch);
            Ok(DocPage {
                docs: all.iter().take(take).cloned().collect(),
                cursor: None,
                exhausted: true,
            })
        }
        async fn find_seek(
            &self,
            _db: &str,
            _coll: &str,
            _filter: Option<&red_core::doc::Filter>,
            _seek: red_core::doc::DocSeek,
            _limit: usize,
            _abort: &AbortSignal,
        ) -> red_core::Result<Vec<Document>> {
            Ok(Vec::new())
        }
        async fn get_document(
            &self,
            _db: &str,
            coll: &str,
            id: &DocValue,
        ) -> red_core::Result<Option<Document>> {
            Ok(self.docs(coll).iter().find(|d| &d.id == id).cloned())
        }
        /// Understands exactly one filter shape: `{_id: {$in: [...]}}`, the probe
        /// `doc_reference_map` issues. Anything else counts everything.
        async fn count(
            &self,
            _db: &str,
            coll: &str,
            filter: Option<&red_core::doc::Filter>,
        ) -> red_core::Result<u64> {
            let docs = self.docs(coll);
            let Some(DocValue::Document(fields)) = filter else {
                return Ok(docs.len() as u64);
            };
            let wanted = fields.iter().find(|(k, _)| k == "_id").and_then(|(_, v)| {
                let DocValue::Document(ops) = v else {
                    return None;
                };
                ops.iter().find(|(k, _)| k == "$in").map(|(_, v)| v)
            });
            let Some(DocValue::Array(ids)) = wanted else {
                return Ok(docs.len() as u64);
            };
            Ok(docs.iter().filter(|d| ids.contains(&d.id)).count() as u64)
        }
        async fn infer_schema(
            &self,
            _db: &str,
            coll: &str,
            sample: usize,
            _abort: &AbortSignal,
        ) -> red_core::Result<DocSchema> {
            let docs = self.docs(coll);
            Ok(DocSchema::from_documents(&docs[..docs.len().min(sample)]))
        }
        async fn aggregate(
            &self,
            _db: &str,
            _coll: &str,
            _pipeline: &[DocValue],
            _batch: usize,
            _abort: &AbortSignal,
        ) -> red_core::Result<DocPage> {
            Ok(DocPage {
                docs: Vec::new(),
                cursor: None,
                exhausted: true,
            })
        }
        async fn indexes(&self, _db: &str, _coll: &str) -> red_core::Result<Vec<IndexInfo>> {
            Ok(Vec::new())
        }
        async fn explain(&self, _q: &FindQuery) -> red_core::Result<DocPlan> {
            Ok(DocPlan {
                stages: Vec::new(),
                index_used: None,
                docs_examined: None,
                n_returned: None,
                collscan: true,
            })
        }
        async fn distinct(
            &self,
            _db: &str,
            _coll: &str,
            _field: &str,
            _filter: Option<&red_core::doc::Filter>,
        ) -> red_core::Result<Vec<DocValue>> {
            Ok(Vec::new())
        }
        async fn next_batch(
            &self,
            _cursor: &red_core::doc::DocCursor,
            _batch: usize,
        ) -> red_core::Result<DocPage> {
            Ok(DocPage {
                docs: Vec::new(),
                cursor: None,
                exhausted: true,
            })
        }
        async fn close_cursor(&self, _cursor: &red_core::doc::DocCursor) {}
        /// Enough extended JSON for the tool arguments under test: plain JSON
        /// plus `{"$oid": …}`. The real dialect is the engine's; this only has
        /// to round-trip what the tests pass in.
        fn parse_ext_json(&self, text: &str) -> red_core::Result<DocValue> {
            fn convert(v: &Json) -> DocValue {
                match v {
                    Json::Null => DocValue::Null,
                    Json::Bool(b) => DocValue::Bool(*b),
                    Json::Number(n) => match n.as_i64() {
                        Some(i) if i32::try_from(i).is_ok() => DocValue::Int32(i as i32),
                        Some(i) => DocValue::Int64(i),
                        None => DocValue::Double(n.as_f64().unwrap_or(0.0)),
                    },
                    Json::String(s) => DocValue::Str(s.clone()),
                    Json::Array(items) => DocValue::Array(items.iter().map(convert).collect()),
                    Json::Object(map) => {
                        if let Some(Json::String(hex)) = map.get("$oid")
                            && let Ok(bytes) = <[u8; 12]>::try_from(
                                (0..hex.len().min(24))
                                    .step_by(2)
                                    .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
                                    .collect::<Vec<u8>>(),
                            )
                        {
                            return DocValue::ObjectId(bytes);
                        }
                        DocValue::Document(
                            map.iter().map(|(k, v)| (k.clone(), convert(v))).collect(),
                        )
                    }
                }
            }
            serde_json::from_str::<Json>(text)
                .map(|v| convert(&v))
                .map_err(|e| RedError::Query(e.to_string()))
        }
        async fn insert(
            &self,
            _db: &str,
            _coll: &str,
            _docs: &[Document],
        ) -> red_core::Result<u64> {
            Err(RedError::Driver("read-only stub".into()))
        }
        async fn update(
            &self,
            _db: &str,
            _coll: &str,
            _filter: &red_core::doc::Filter,
            _change: &DocUpdate,
            _many: bool,
        ) -> red_core::Result<u64> {
            Err(RedError::Driver("read-only stub".into()))
        }
        async fn replace(
            &self,
            _db: &str,
            _coll: &str,
            _id: &DocValue,
            _doc: &Document,
        ) -> red_core::Result<()> {
            Err(RedError::Driver("read-only stub".into()))
        }
        async fn delete(
            &self,
            _db: &str,
            _coll: &str,
            _filter: &red_core::doc::Filter,
            _many: bool,
        ) -> red_core::Result<u64> {
            Err(RedError::Driver("read-only stub".into()))
        }
        async fn create_collection(&self, _db: &str, _coll: &str) -> red_core::Result<()> {
            Err(RedError::Driver("read-only stub".into()))
        }
        async fn drop_collection(&self, _db: &str, _coll: &str) -> red_core::Result<()> {
            Err(RedError::Driver("read-only stub".into()))
        }
        async fn create_index(
            &self,
            _db: &str,
            _coll: &str,
            _spec: &IndexSpec,
        ) -> red_core::Result<()> {
            Err(RedError::Driver("read-only stub".into()))
        }
    }

    /// `customers` holds ids 1..=3. `orders.customer_id` points at two of them
    /// and one stranger; `orders.customerRef` points at nothing.
    fn doc_stub() -> Arc<dyn DocDriver> {
        let customers = (1..=3)
            .map(|id| Document {
                id: DocValue::Int32(id),
                fields: vec![("name".into(), DocValue::Str(format!("c{id}")))],
            })
            .collect();
        let orders = (1..=3)
            .map(|i| Document {
                id: DocValue::Int32(100 + i),
                fields: vec![
                    ("customer_id".into(), DocValue::Int32(i)),
                    ("customerRef".into(), DocValue::Int32(900 + i)),
                ],
            })
            .collect();
        Arc::new(DocStub {
            colls: vec![
                ("customers".to_string(), customers),
                ("orders".to_string(), orders),
            ],
        })
    }

    async fn doc_tool(driver: &Arc<dyn DocDriver>, name: &str, input: Json) -> (String, bool) {
        doc_run_tool(
            driver,
            name,
            &input,
            &AiPolicy::default(),
            &CancelToken::new(),
            &ReportSink::disabled(),
        )
        .await
    }

    #[tokio::test]
    async fn doc_reference_map_reports_hit_rates_and_names_the_unresolved() {
        let driver = doc_stub();
        let (content, ok) = doc_tool(&driver, "doc_reference_map", json!({ "db": "app" })).await;
        assert!(ok, "{content}");
        // A resolving reference reports its hit rate, not just its existence.
        assert!(
            content.contains("orders.customer_id -> customers._id"),
            "{content}"
        );
        assert!(content.contains("3/3 sampled values resolve"), "{content}");
        // A field whose values match nothing is reported as UNRESOLVED. Omitting
        // it would read as "no reference exists", the opposite of what was found.
        assert!(
            content.contains("orders.customerRef -> ? UNRESOLVED"),
            "{content}"
        );
        assert!(
            content.contains("0/3 sampled values match any customers._id"),
            "{content}"
        );
    }

    #[tokio::test]
    async fn structure_maps_are_reads_at_their_stated_tiers() {
        // Two are structure-only (Schema tier and up); the Mongo one samples
        // values, so it starts at Read.
        for t in ["relationship_map", "kv_key_schema"] {
            assert!(!is_write_tool(t), "{t} must be read-only");
            assert!(AiTier::Schema.allows_tool(t), "{t} must exist at Schema");
            assert!(is_headless_tool(t), "{t} must be offered over MCP");
        }
        assert!(!is_write_tool("doc_reference_map"));
        assert!(!AiTier::Schema.allows_tool("doc_reference_map"));
        assert!(AiTier::Read.allows_tool("doc_reference_map"));
        // Each seam's catalog actually offers its own map at Read tier.
        let read = AiPolicy::default();
        assert!(
            tool_catalog(&read)
                .iter()
                .any(|t| t.name == "relationship_map")
        );
        assert!(
            kv_tool_catalog(&read)
                .iter()
                .any(|t| t.name == "kv_key_schema")
        );
        assert!(
            doc_tool_catalog(&read)
                .iter()
                .any(|t| t.name == "doc_reference_map")
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
                Dialect::Sqlite,
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
    async fn get_document_fetches_by_id_and_reports_a_miss() {
        let driver = doc_stub();
        let (content, ok) = doc_tool(
            &driver,
            "get_document",
            json!({ "db": "app", "coll": "customers", "id": 2 }),
        )
        .await;
        assert!(ok, "{content}");
        assert!(content.contains("\"c2\""), "{content}");
        // A miss is a normal answer, not an error, and it says how to spell an
        // ObjectId in case that was the mistake.
        let (content, ok) = doc_tool(
            &driver,
            "get_document",
            json!({ "db": "app", "coll": "customers", "id": 99 }),
        )
        .await;
        assert!(ok, "{content}");
        assert!(content.contains("$oid"), "{content}");
    }

    #[tokio::test]
    async fn describe_collection_reports_whether_a_validator_exists() {
        let driver = doc_stub();
        let (content, ok) = doc_tool(
            &driver,
            "describe_collection",
            json!({ "db": "app", "coll": "customers" }),
        )
        .await;
        assert!(ok, "{content}");
        // Absence is stated rather than left to inference: "no validator line"
        // and "no validator" must not look the same.
        assert!(content.contains("Validator: none declared."), "{content}");
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
            Dialect::Sqlite,
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

    /// Every tool a catalog advertises must also be reachable in its executor.
    /// A `ToolDef` with no `run_tool` arm is a tool that exists right up until
    /// the model calls it, which is worse than one that was never offered — so
    /// this asserts the *structural* wiring by checking nothing falls through to
    /// the unknown-tool arm. (`spawn_subagent` is intercepted in `run_turn`
    /// before `run_tool` and is excluded by design.)
    #[tokio::test]
    async fn every_advertised_tool_has_an_executor_arm() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        let db = std::env::temp_dir().join(format!("red-arm-{}.db", uuid::Uuid::new_v4().simple()));
        {
            rusqlite::Connection::open(&db)
                .unwrap()
                .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);")
                .unwrap();
        }
        let sql: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        for tool in tool_catalog(&write) {
            if tool.name == "spawn_subagent" {
                continue;
            }
            let (content, _) = run_tool(
                &sql,
                Dialect::Sqlite,
                &tool.name,
                &json!({}),
                &write,
                &CancelToken::new(),
                &ReportSink::disabled(),
            )
            .await;
            assert!(
                !content.contains("unknown tool"),
                "SQL tool `{}` is advertised but has no executor arm",
                tool.name
            );
        }
        let doc = doc_stub();
        for tool in doc_tool_catalog(&write) {
            if tool.name == "spawn_subagent" {
                continue;
            }
            let (content, _) = doc_tool(&doc, &tool.name, json!({})).await;
            assert!(
                !content.contains("unknown tool"),
                "doc tool `{}` is advertised but has no executor arm",
                tool.name
            );
        }
    }

    /// `is_write_tool` is the complement of an allowlist, so a read nobody
    /// remembered to list is silently gated as a write (and withheld from MCP).
    /// Assert the membership *and* the fail-closed property it rests on.
    #[test]
    fn new_reads_are_listed_and_an_unlisted_name_fails_closed() {
        for t in [
            "relationship_map",
            "object_ddl",
            "search_data",
            "health_report",
            "server_sessions",
            "diff_schema",
            "diff_data",
            "suggest_index",
            "export_result",
            "kv_key_schema",
            "kv_read_collection",
            "kv_stream_groups",
            "kv_client_list",
            "kv_keyspace_notifications",
            "doc_reference_map",
            "get_document",
            "doc_current_op",
        ] {
            assert!(!is_write_tool(t), "{t} must be in READ_ONLY_TOOLS");
        }
        // The property itself: an unlisted name is a write, so a future tool is
        // gated until someone vets it rather than slipping through.
        assert!(is_write_tool("some_tool_nobody_listed"));
        // And the new writes are writes, so none is auto-allowed over ACP/MCP.
        for t in [
            "kill_session",
            "create_index",
            "kv_set",
            "kv_copy_key",
            "kv_client_kill",
            "kv_command",
            "doc_kill_op",
        ] {
            assert!(is_write_tool(t), "{t} must be gated as a write");
            assert!(!is_headless_tool(t), "{t} must not be offered headlessly");
            assert!(!AiTier::Read.allows_tool(t), "{t} must not exist at Read");
            assert!(AiTier::Write.allows_tool(t), "{t} must exist at Write");
        }
        // The UI-bound reads are reads, but still withheld from the headless
        // transport: there is no app there to hand a tab or a file to.
        for t in [
            "export_result",
            "open_query",
            "save_query",
            "generate_report",
        ] {
            assert!(!is_write_tool(t));
            assert!(!is_headless_tool(t), "{t} is GUI-bound");
        }
    }

    /// A kill prompt has to say *what* is being stopped. "Terminate session 4711"
    /// is not something anyone can meaningfully approve.
    #[test]
    fn kill_prompts_name_their_target() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        let detail = |name: &str, input: Json| match assess_write(name, &input, &write) {
            WriteAssessment::NeedsApproval { sql } => sql,
            _ => panic!("{name} must prompt"),
        };
        let sql = detail(
            "kill_session",
            json!({
                "key": "4711", "mode": "terminate",
                "user": "reporting", "statement": "SELECT * FROM huge",
            }),
        );
        assert!(sql.contains("Terminate session"), "{sql}");
        assert!(sql.contains("4711") && sql.contains("reporting"), "{sql}");
        assert!(sql.contains("SELECT * FROM huge"), "{sql}");
        // Terminate says what it costs; cancel does not claim to.
        assert!(sql.contains("rolls back"), "{sql}");
        assert!(
            !detail("kill_session", json!({ "key": "4711" })).contains("rolls back"),
            "a cancel must not claim to roll anything back"
        );
        // Missing context is stated rather than papered over.
        assert!(
            detail("kill_session", json!({ "key": "4711" })).contains("did not say"),
            "an unexplained kill must say so"
        );

        let kv = detail(
            "kv_client_kill",
            json!({ "id": 12, "addr": "10.0.0.2:6379", "cmd": "keys" }),
        );
        assert!(kv.contains("CLIENT KILL ID 12"), "{kv}");
        assert!(kv.contains("10.0.0.2:6379") && kv.contains("keys"), "{kv}");

        let doc = detail(
            "doc_kill_op",
            json!({ "opid": 88, "namespace": "app.orders", "command": "{\"find\":\"orders\"}" }),
        );
        assert!(doc.contains("KILL operation 88"), "{doc}");
        assert!(doc.contains("app.orders"), "{doc}");
        assert!(doc.contains("NOT rolled back"), "{doc}");

        // A kill with no target is refused outright, never prompted.
        for (name, input) in [
            ("kill_session", json!({})),
            ("kv_client_kill", json!({})),
            ("doc_kill_op", json!({})),
            ("kill_session", json!({ "key": "1", "mode": "obliterate" })),
        ] {
            assert!(
                matches!(
                    assess_write(name, &input, &write),
                    WriteAssessment::Reject(_)
                ),
                "{name} with {input} must be refused"
            );
        }
    }

    /// A denied write must come back as a recoverable error, not a dead turn.
    /// `run_tool` is the executor half of that: a rejected assessment is an
    /// `is_error` result carrying the reason.
    #[tokio::test]
    async fn a_refused_kill_is_a_recoverable_tool_error() {
        let driver = doc_stub();
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        let (content, ok) = doc_run_tool(
            &driver,
            "doc_kill_op",
            &json!({}),
            &write,
            &CancelToken::new(),
            &ReportSink::disabled(),
        )
        .await;
        assert!(!ok, "a refusal must be an is_error result");
        assert!(content.starts_with("error:"), "{content}");
        assert!(content.contains("doc_current_op"), "{content}");
    }

    #[test]
    fn kv_command_runs_only_allowlisted_verbs() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        // An allowlisted verb prompts with the exact command.
        match assess_write(
            "kv_command",
            &json!({ "argv": ["MEMORY", "DOCTOR"] }),
            &write,
        ) {
            WriteAssessment::NeedsApproval { sql } => assert_eq!(sql, "MEMORY DOCTOR"),
            _ => panic!("an allowlisted verb must prompt"),
        }
        // Everything else is refused, including the reads it could otherwise use
        // to route around kv_get_value's tier gate and any write gate.
        for argv in [
            json!(["GET", "user:1"]),
            json!(["SET", "user:1", "x"]),
            json!(["FLUSHALL"]),
            json!(["CONFIG", "SET", "dir", "/tmp"]),
            json!(["EVAL", "return 1", "0"]),
            json!([]),
        ] {
            assert!(
                matches!(
                    assess_write("kv_command", &json!({ "argv": argv }), &write),
                    WriteAssessment::Reject(_)
                ),
                "kv_command {argv} must be refused"
            );
        }
    }

    /// `create_index` deliberately widens the blanket DDL block. Assert the
    /// widening is exactly one statement kind wide: the DDL that destroys is
    /// still refused through `propose_write`.
    #[test]
    fn create_index_is_the_only_ddl_the_agent_may_run() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        match assess_write(
            "create_index",
            &json!({ "schema": "public", "table": "orders", "name": "idx_orders_created",
                     "columns": ["created_at"], "unique": true }),
            &write,
        ) {
            WriteAssessment::NeedsApproval { sql } => {
                assert!(
                    sql.contains("CREATE UNIQUE INDEX idx_orders_created"),
                    "{sql}"
                );
                assert!(sql.contains("public.orders"), "{sql}");
                assert!(sql.contains("created_at"), "{sql}");
                // The cost is named: an index build is not free on a live server.
                assert!(sql.contains("locks and loads"), "{sql}");
            }
            _ => panic!("create_index must prompt"),
        }
        // Destructive DDL is still blocked at the shape gate.
        for sql in [
            "DROP TABLE orders",
            "TRUNCATE orders",
            "ALTER TABLE orders DROP COLUMN x",
            "DROP INDEX idx_orders_created",
        ] {
            assert!(
                matches!(
                    assess_write("propose_write", &json!({ "sql": sql }), &write),
                    WriteAssessment::Reject(_)
                ),
                "`{sql}` must stay blocked"
            );
        }
        // And a create_index with no columns is refused rather than prompted.
        assert!(matches!(
            assess_write(
                "create_index",
                &json!({ "table": "orders", "name": "idx", "columns": [] }),
                &write
            ),
            WriteAssessment::Reject(_)
        ));
    }

    /// The export path is model-supplied, so it is the one place a tool argument
    /// could reach outside the app's own folder. Assert it cannot.
    #[test]
    fn export_paths_stay_inside_the_output_folder() {
        let sink = ReportSink::disabled();
        let dir = sink.output_dir();
        for name in [
            "../../etc/passwd",
            "/etc/passwd",
            "..\\..\\windows\\system32",
            "a/b/c",
            "orders",
            "",
        ] {
            let path = export_path(&sink, Some(name), "csv");
            assert_eq!(path.parent(), Some(dir.as_path()), "escaped for `{name}`");
            let file = path.file_name().unwrap().to_string_lossy().to_string();
            assert!(file.starts_with("red-export-"), "{file}");
            assert!(file.ends_with(".csv"), "{file}");
            assert!(!file.contains(".."), "{file}");
        }
        // Two calls never collide, so an export cannot clobber an earlier one.
        assert_ne!(
            export_path(&sink, Some("x"), "csv"),
            export_path(&sink, Some("x"), "csv")
        );
    }

    /// A report that drops a check it could not run reads as a clean bill of
    /// health, so the unavailable list is part of the output contract.
    #[test]
    fn health_report_states_what_it_could_not_check() {
        use red_core::health::{
            Finding, FindingKind, HealthReport, Severity, SizeTotals, TableSize, UnavailableCheck,
        };

        let mut report = HealthReport::new(red_core::DbKind::Postgres, Some("public".into()), 0);
        report.totals = SizeTotals {
            bytes: 2 * 1024 * 1024,
            index_bytes: 1024 * 1024,
            table_count: 3,
        };
        report.tables = vec![TableSize {
            table: TableRef {
                schema: Some("public".into()),
                name: "events".into(),
            },
            bytes: 1024 * 1024,
            index_bytes: 512 * 1024,
            estimated_rows: 90_000,
        }];
        report.findings = vec![Finding {
            severity: Severity::Bad,
            kind: FindingKind::MissingFkIndex,
            object: Some(TableRef {
                schema: Some("public".into()),
                name: "order_items".into(),
            }),
            title: "foreign key with no index".into(),
            detail: "every parent delete scans".into(),
            suggested_sql: Some("CREATE INDEX ...".into()),
        }];
        report.unavailable = vec![UnavailableCheck {
            kind: FindingKind::UnusedIndex,
            reason: "needs pg_stat_user_indexes".into(),
        }];

        let out = format_health(&report);
        assert!(out.contains("public.events"), "{out}");
        assert!(out.contains("public.order_items"), "{out}");
        assert!(out.contains("Bad"), "{out}");
        // The remediation is text, and says so, so nothing reads it as applied.
        assert!(out.contains("NOT run"), "{out}");
        assert!(out.contains("needs pg_stat_user_indexes"), "{out}");
        assert!(out.contains("absence proves nothing"), "{out}");
    }

    #[test]
    fn session_list_reports_hidden_statements_as_hidden() {
        use red_core::{ServerSession, SessionKey};

        let sessions = vec![
            ServerSession {
                key: SessionKey("101".into()),
                user: Some("reporting".into()),
                application: Some("psql".into()),
                client_addr: Some("10.0.0.9".into()),
                database: Some("shop".into()),
                state: "active".into(),
                wait: None,
                blocked_by: vec![SessionKey("77".into())],
                query: Some("SELECT * FROM orders".into()),
                elapsed_secs: 12.5,
                is_self: false,
            },
            ServerSession {
                key: SessionKey("102".into()),
                user: None,
                application: None,
                client_addr: None,
                database: None,
                state: "idle".into(),
                wait: Some("Lock:transactionid".into()),
                blocked_by: Vec::new(),
                query: None,
                elapsed_secs: 0.2,
                is_self: true,
            },
        ];
        let out = format_sessions(&sessions, true);
        assert!(
            out.contains("[101]") && out.contains("user=reporting"),
            "{out}"
        );
        assert!(out.contains("blocked by 77"), "{out}");
        assert!(out.contains("waiting on Lock:transactionid"), "{out}");
        assert!(out.contains("RED's own connection"), "{out}");
        // A statement the role may not read is reported as hidden, not as absent.
        assert!(out.contains("not visible to this role"), "{out}");
        assert!(out.contains("hidden rather than absent"), "{out}");
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
            Dialect::Sqlite,
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

    #[test]
    fn tool_call_budget_is_per_conversation_and_capped() {
        let mut state = AiState::default();
        // A cap of 2 admits two calls, then refuses the third on the same conversation.
        assert!(state.charge_tool_call(ConversationId::new(1), 2));
        assert!(state.charge_tool_call(ConversationId::new(1), 2));
        assert!(!state.charge_tool_call(ConversationId::new(1), 2));
        // A different conversation has its own fresh budget.
        assert!(state.charge_tool_call(ConversationId::new(2), 2));
        // `0` means unlimited.
        assert!(state.charge_tool_call(ConversationId::new(3), 0));
        assert!(state.charge_tool_call(ConversationId::new(3), 0));
    }
}
