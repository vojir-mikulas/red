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
use red_core::kv::{
    analyze_keyspace, KeyMeta, KvCollection, KvValue, RespValue, ScanBudget, ScanCursor,
};
use red_core::{
    ActivityKind, ActivityStatus, AiLimits, AiPolicy, AiTier, RedError, TableRef, Value,
};
use red_driver::{AbortSignal, DatabaseDriver, KvDriver, PageCap};
use serde_json::{json, Value as Json};
use tokio::sync::oneshot;

use crate::dispatch::{emit, Events};
use crate::protocol::{AiContext, AiDelta, AiUsage, ReportTheme};
use crate::{Event, SessionId};

/// Which engine the agent turn is grounded in. The model→tool loop, streaming,
/// budget, write gate, and history are identical for both; only the tool
/// catalog, the tool execution, and the system prompt differ (see
/// `docs/plans/redis-workflow-parity.md` Part 1). A KV (Redis) turn exposes the
/// `kv_*` read tools; a SQL turn the schema/query tools.
#[derive(Clone)]
pub(crate) enum AiBackend {
    Sql(Arc<dyn DatabaseDriver>),
    Kv(Arc<dyn KvDriver>),
}

impl AiBackend {
    /// The tier-filtered tool catalog this backend offers under `policy`. Routes to
    /// the SQL schema/query tools or the Redis `kv_*` tools.
    pub(crate) fn catalog(&self, policy: &AiPolicy) -> Vec<ToolDef> {
        match self {
            AiBackend::Sql(_) => tool_catalog(policy),
            AiBackend::Kv(_) => kv_tool_catalog(policy),
        }
    }

    /// The full grounding system prompt for this backend under `ctx`/`policy`.
    pub(crate) fn system_prompt(&self, ctx: &AiContext, policy: &AiPolicy) -> String {
        match self {
            AiBackend::Sql(_) => system_prompt(ctx, policy),
            AiBackend::Kv(_) => kv_system_prompt(ctx, policy),
        }
    }

    /// Whether `name` is a mutating tool for this backend. Used to withhold writes
    /// over the subscription/MCP path (each backend has its own writer set: the SQL
    /// `propose_*` tools vs. the Redis `kv_*` writers).
    pub(crate) fn is_write_tool(&self, name: &str) -> bool {
        match self {
            AiBackend::Sql(_) => is_write_tool(name),
            AiBackend::Kv(_) => is_kv_write_tool(name),
        }
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
            AiBackend::Sql(d) => run_tool(d, name, input, policy, cancel, report).await,
            AiBackend::Kv(d) => kv_run_tool(d, name, input, policy, cancel, report).await,
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
    conversation_id: u64,
    /// The active app theme, so `generate_report` can paint the report in Red's
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
        conversation_id: u64,
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

    /// A no-op sink that drops announcements. For tests and any path with no UI.
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self {
            events: None,
            session: None,
            conversation_id: 0,
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
    /// it back out and fires it, the API-key analogue of the ACP path's
    /// `AcpManager.pending`.
    pending_perms: HashMap<u64, oneshot::Sender<bool>>,
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
    /// prompt abandoned on cancel (`allow` is irrelevant then; the receiver is gone).
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

    /// Drop all per-conversation state (history, cancel token, cumulative tool tally)
    /// when the UI closes/deletes the conversation, so these maps stay bounded by
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
    backend: AiBackend,
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
    let system = backend.system_prompt(&context, &policy);
    // The tier decides which tools the model is even offered (M-S7): `off` grounds
    // nothing, `schema` withholds row data, `read` is the full catalog. The KV
    // (Redis) backend offers its own read-only `kv_*` catalog.
    let tools = backend.catalog(&policy);
    // Where `generate_report` delivers its file so the UI opens it (Feature C);
    // carries the active theme so the report matches Red's colors.
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
                            id: id.clone(),
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
                            id: id.clone(),
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
                        // Record the denied write in the timeline as a terminal node
                        // so the audit trail shows what was proposed and refused.
                        emit(
                            &events,
                            session,
                            Event::AiDelta {
                                conversation_id,
                                delta: AiDelta::ActivityStarted {
                                    id: id.clone(),
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
                        id: id.clone(),
                        parent: None,
                        kind: ActivityKind::Tool {
                            name: name.clone(),
                            args_summary: summarize_tool_args(name, input),
                        },
                        status: ActivityStatus::Running,
                    },
                },
            );
            let (content, ok) = match &backend {
                AiBackend::Sql(driver) => {
                    run_tool(driver, name, input, &policy, &cancel, &report).await
                }
                AiBackend::Kv(driver) => {
                    kv_run_tool(driver, name, input, &policy, &cancel, &report).await
                }
            };
            emit(
                &events,
                session,
                Event::AiDelta {
                    conversation_id,
                    delta: AiDelta::ActivityUpdated {
                        id: id.clone(),
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
    conversation_id: u64,
    model: &str,
    policy: &AiPolicy,
    report: &ReportSink,
    parent_id: &str,
    task: &str,
    cancel: &CancelToken,
) -> (String, bool) {
    // The child runs the parent's backend, narrowed to reads (see the catalogs).
    let (tools, system) = match backend {
        AiBackend::Sql(_) => (subagent_catalog(policy), subagent_system_prompt(task)),
        AiBackend::Kv(_) => (kv_subagent_catalog(policy), kv_subagent_system_prompt(task)),
    };
    let mut messages = vec![Message::user_text(task.to_string())];
    let mut answer = String::new();

    for _ in 0..SUBAGENT_MAX_STEPS {
        if cancel.is_cancelled() {
            return ("the subagent was cancelled".into(), false);
        }
        let req = TurnRequest {
            model: model.to_string(),
            max_tokens: 4096,
            show_thinking: false,
            system: system.clone(),
            tools: tools.clone(),
            messages: messages.clone(),
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
                        id: id.clone(),
                        parent: Some(parent_id.to_string()),
                        kind: ActivityKind::Tool {
                            name: name.clone(),
                            args_summary: summarize_tool_args(name, input),
                        },
                        status: ActivityStatus::Running,
                    },
                },
            );
            let (content, ok) = match backend {
                AiBackend::Sql(d) => run_tool(d, name, input, policy, cancel, report).await,
                AiBackend::Kv(d) => kv_run_tool(d, name, input, policy, cancel, report).await,
            };
            emit(
                events,
                session,
                Event::AiDelta {
                    conversation_id,
                    delta: AiDelta::ActivityUpdated {
                        id: id.clone(),
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

/// The tool subset a delegated subagent may use: the parent's catalog minus every
/// write tool and minus `spawn_subagent` itself, so a subagent can neither mutate
/// data nor recurse. Narrows (never widens) the parent's tier — even a Write-tier
/// parent yields a read-only child.
fn subagent_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
    tool_catalog(policy)
        .into_iter()
        .filter(|t| t.name != "spawn_subagent" && !is_write_tool(&t.name))
        .collect()
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

/// The Redis subagent's tool subset: the parent's KV catalog minus writes and
/// minus `spawn_subagent` (no mutation, no recursion), like [`subagent_catalog`].
fn kv_subagent_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
    kv_tool_catalog(policy)
        .into_iter()
        .filter(|t| t.name != "spawn_subagent" && !is_write_tool(&t.name))
        .collect()
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
            description: "Execute SEVERAL data-modifying statements as ONE atomic transaction: \
                they all commit together, or if any fails the whole set is rolled back (nothing \
                changes). Use this for a related multi-step change — e.g. insert a parent row then \
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
                        "description": "The INSERT/UPDATE/DELETE statements to run in order, in one transaction. Each is a single statement (UPDATE/DELETE need a WHERE).",
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
    all.into_iter()
        // The tier gates membership; additionally, the write tool is withheld on a
        // read-only connection so it's never even offered there (Feature B).
        .filter(|t| {
            policy.tier.allows_tool(&t.name) && !(policy.read_only && is_write_tool(&t.name))
        })
        .collect()
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

/// Execute one tool call against the driver, under the access policy (M-S7).
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
/// call it (see docs/plans/redis-workflow-parity.md Part 1).
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

// --- Redis (KV) agent backend (see docs/plans/redis-workflow-parity.md Part 1) ---

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

/// The Redis agent's read-only tool catalog, gated by tier via
/// [`AiTier::allows_tool`] exactly like the SQL [`tool_catalog`]. Redis writes
/// aren't wired yet, so every tool here is read-only.
pub(crate) fn kv_tool_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
    let all = [
        ToolDef {
            name: "kv_server_info".into(),
            description: "Summarize the server's INFO: version, memory (used/max/fragmentation), \
                connected clients, ops/sec, keyspace hit rate, evictions/expirations, uptime, and \
                per-database key counts. Call this first to understand the server's health and size."
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
        report_tool_def(),
        spawn_subagent_tool_def(),
        // --- gated writes (Write tier, writable connection only) ---
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
    all.into_iter()
        // The tier gates membership; the write tools are additionally withheld on a
        // read-only connection so they're never even offered there.
        .filter(|t| {
            policy.tier.allows_tool(&t.name) && !(policy.read_only && is_write_tool(&t.name))
        })
        .collect()
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
        let page = driver.scan_keys(cursor, pattern, None, budget, &abort).await?;
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
    let limits = &policy.limits;
    let (content, ok) = match name {
        "kv_server_info" => match driver.command(&["INFO".to_string()]).await {
            Ok(RespValue::Bulk(info)) | Ok(RespValue::Simple(info)) => {
                (kv_info_summary(&info), true)
            }
            Ok(other) => (format!("unexpected INFO reply: {other:?}"), true),
            Err(e) => (format!("error: {e}"), false),
        },
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
                                kv_bytes(k.approx_bytes),
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
                        kv_bytes(m.approx_bytes),
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
                            kv_bytes(k.approx_bytes),
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
                        let v = resp_scalar(pair.get(1));
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
            let mut note = "";
            if targets.is_empty() {
                if let Some(pattern) = input
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
        "generate_report" => run_generate_report(input, report),
        other => (format!("error: unknown tool `{other}`"), false),
    };
    (content, ok)
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
            "You have metadata-only Redis tools: kv_server_info, kv_scan_keys, and kv_key_info. \
             You can see the server's stats and keys' types/TTLs/sizes but you CANNOT read a \
             key's value."
        }
        AiTier::Read => {
            "You have read-only Redis tools: kv_server_info (INFO summary), kv_scan_keys (find \
             keys by glob pattern), kv_key_info (a key's type/TTL/encoding/size), kv_get_value (a \
             key's value or a collection preview), kv_biggest_keys (sample for the largest keys by \
             memory), kv_analyze (a keyspace rollup: memory by type and namespace, TTL coverage), \
             kv_slowlog (recent slow commands), kv_config_get (read a CONFIG parameter), and \
             generate_report (author an HTML report from what you've read, with optional Chart.js \
             charts; it appears as a card the user can open — use it when the user asks for a \
             report). Ground every answer in the live server with these tools rather than guessing."
        }
        AiTier::Write => {
            "You have the read-only Redis tools (kv_server_info, kv_scan_keys, kv_key_info, \
             kv_get_value, kv_biggest_keys, kv_analyze, kv_slowlog, kv_config_get, generate_report) \
             AND gated write tools: kv_expire (set/remove a key's TTL), kv_delete (delete keys), \
             kv_rename, and kv_config_set. Every write requires the user's explicit Allow on the \
             exact operation; assume it may be denied. Before a bulk kv_delete/kv_expire by \
             pattern, scan first (kv_scan_keys) and tell the user how many keys will be affected — \
             a keyspace-wide delete or expire (pattern `*`) is refused outright. Only write when \
             the user has asked you to change data."
        }
    };
    let mut s = format!(
        "You are Red's Redis agent, embedded in a native database explorer. You help the user \
         explore and understand the Redis server they are connected to.\n\n\
         {tools_line}\n\n\
         Redis keys are addressed by glob patterns (e.g. `user:*`), not SQL — there are no tables \
         or joins. Be concise: lead with the answer, then the supporting detail. When you show a \
         command, put it in a fenced ```sh block (e.g. `redis-cli GET foo`).\n",
    );
    if !ctx.connection.is_empty() {
        s.push_str(&format!("\nConnected to: {}", ctx.connection));
    }
    if ctx.read_only {
        s.push_str("\nThis connection is READ-ONLY.");
    }
    s
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
        kv_bytes(r.total_bytes),
    ));
    s.push_str("By type (memory):\n");
    for t in &r.types {
        s.push_str(&format!(
            "  {}: {} keys, ~{}\n",
            t.kv_type,
            t.count,
            kv_bytes(t.bytes),
        ));
    }
    s.push_str("Top namespaces (memory):\n");
    for n in r.namespaces.iter().take(15) {
        s.push_str(&format!(
            "  {}: {} keys, ~{}\n",
            n.prefix,
            n.count,
            kv_bytes(n.bytes),
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

/// Coarse human byte count for the agent's text output.
fn kv_bytes(n: u64) -> String {
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
        "run_select" => {
            let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
            if !is_read_only_select(sql) {
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
            // Bound the wait like run_select. `explain(analyze=false)` only *plans*
            // (it never executes the statement), and the trait gives it no abort
            // seam, so on timeout we hand the model a clean error while the engine's
            // plan call winds down on its own; it's plan-only, so it can't run away
            // with data, only take a moment on a pathological statement.
            let explain = driver.explain(sql, false);
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
            // Re-vet at execution (defense in depth), then run the whole set in one
            // transaction: all commit or none do. Approval was already granted above.
            match assess_write(name, input, policy) {
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
                                    "Executed the changeset in one transaction: {} statement(s), \
                                     {total} row(s) affected. Verify with a SELECT if it matters.",
                                    statements.len()
                                ),
                                true,
                            )
                        }
                        Err(e) => (
                            format!(
                                "error: the changeset failed and was rolled back (nothing \
                                     changed): {e}"
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
/// keyword, sharing `strip_sql_noise`/`has_word` so the read and write gates can't
/// drift. False positives (a rejected legitimate read) are acceptable: the user can
/// always run such a query by hand in a query tab. (Defense in depth: opening the
/// AI's reads on an engine-level read-only connection would make this belt-and-
/// suspenders: a worthwhile follow-up, but it needs a per-call driver seam.)
fn is_read_only_select(sql: &str) -> bool {
    let stripped = strip_sql_noise(sql);
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
    const WRITE_TOKENS: &[&str] = &[
        "insert", "update", "delete", "merge", "into", "nextval", "setval",
    ];
    // Server-side functions callable from inside a SELECT that write/read files,
    // manipulate large objects, execute remote SQL, or emit WAL, all beyond a read
    // tier's intent (e.g. `SELECT lo_import('/etc/passwd')`, `SELECT load_file(…)`,
    // `SELECT dblink_exec('…','DELETE …')`). This is a denylist of the well-known
    // dangerous ones; the *complete* guarantee is engine-level read-only (see the
    // doc comment). The names are underscore-qualified identifiers, not plausible
    // bare column names, so blocking them won't trip a real browse query.
    const DANGEROUS_FNS: &[&str] = &[
        // Postgres: file read/write, large objects, remote exec, WAL, admin file ops.
        "lo_import",
        "lo_export",
        "pg_read_file",
        "pg_read_binary_file",
        "pg_ls_dir",
        "pg_stat_file",
        "pg_logical_emit_message",
        // `dblink`/`dblink_send_query` run arbitrary SQL on a remote (often the
        // same loopback) server from inside a SELECT: a write channel that reads
        // as read-only here. `dblink_exec` is the obvious one; the bare and async
        // forms are the same hole under a different name.
        "dblink",
        "dblink_exec",
        "dblink_open",
        "dblink_send_query",
        "pg_file_write",
        "pg_file_unlink",
        "pg_file_rename",
        // MySQL: file read and UDF command execution.
        "load_file",
        "sys_exec",
        "sys_eval",
    ];
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
    "profile_table",
    "run_select",
    "explain",
    "generate_report",
    // Hands the user a SQL query to open in a tab; no DB mutation of its own.
    "open_query",
    // Writes a `.sql` file to the user's saved-queries library; no DB mutation.
    "save_query",
    // Redis (KV) read tools: pure reads through the `KvDriver` seam.
    "kv_server_info",
    "kv_scan_keys",
    "kv_key_info",
    "kv_get_value",
    "kv_biggest_keys",
    "kv_analyze",
    "kv_slowlog",
    "kv_config_get",
];

/// Whether `name` is a mutating tool: it never auto-runs and never auto-allows;
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
            "this connection is read-only: writes are disabled. Tell the user; do not retry."
                .into(),
        );
    }
    if is_kv_write_tool(name) {
        return assess_kv_write(name, input);
    }
    if name == "propose_changeset" {
        return assess_changeset(input);
    }
    let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
    match write_shape(sql) {
        WriteShape::Ok => WriteAssessment::NeedsApproval {
            sql: sql.to_string(),
        },
        WriteShape::NotWrite => WriteAssessment::Reject(
            "propose_write is only for INSERT/UPDATE/DELETE; use run_select to read".into(),
        ),
        WriteShape::Blocked(why) => WriteAssessment::Reject(why.into()),
    }
}

/// The Redis mutating tools (Feature B, KV backend): each rides the same per-call
/// approval gate as a SQL write.
const KV_WRITE_TOOLS: &[&str] = &["kv_expire", "kv_delete", "kv_rename", "kv_config_set"];

fn is_kv_write_tool(name: &str) -> bool {
    KV_WRITE_TOOLS.contains(&name)
}

/// Vet a Redis write tool for the approval gate: build the human-readable
/// operation shown in the Allow/Deny prompt, and hard-block the catastrophic
/// shapes (a keyspace-wide DELETE or EXPIRE) even with approval — mirroring the
/// SQL gate's refusal of an unqualified UPDATE/DELETE. Tier + read-only were
/// already checked by [`assess_write`].
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
                    if p == "*" && seconds.is_some_and(|sec| sec > 0) {
                        return WriteAssessment::Reject(
                            "refusing to set a TTL on the entire keyspace (pattern `*`): this \
                             would expire every key. Narrow the pattern."
                                .into(),
                        );
                    }
                    format!("all keys matching `{p}`")
                }
                (None, None) => {
                    return WriteAssessment::Reject("kv_expire needs `key` or `pattern`".into())
                }
            };
            let action = match seconds {
                Some(sec) if sec > 0 => format!("EXPIRE {target} in {sec}s"),
                _ => format!("PERSIST {target} (remove any expiry)"),
            };
            WriteAssessment::NeedsApproval { sql: action }
        }
        "kv_delete" => {
            let keys: Vec<String> = input
                .get("keys")
                .and_then(Json::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(k) = s("key") {
                WriteAssessment::NeedsApproval {
                    sql: format!("DELETE key `{k}`"),
                }
            } else if !keys.is_empty() {
                WriteAssessment::NeedsApproval {
                    sql: format!("DELETE {} key(s): {}", keys.len(), keys.join(", ")),
                }
            } else if let Some(p) = s("pattern") {
                if p == "*" {
                    return WriteAssessment::Reject(
                        "refusing to DELETE the entire keyspace (pattern `*`): use FLUSHDB by hand \
                         if that's really intended. Narrow the pattern."
                            .into(),
                    );
                }
                WriteAssessment::NeedsApproval {
                    sql: format!("DELETE all keys matching `{p}`"),
                }
            } else {
                WriteAssessment::Reject("kv_delete needs `key`, `keys`, or `pattern`".into())
            }
        }
        "kv_rename" => match (s("from"), s("to")) {
            (Some(f), Some(t)) => WriteAssessment::NeedsApproval {
                sql: format!("RENAME `{f}` -> `{t}`"),
            },
            _ => WriteAssessment::Reject("kv_rename needs `from` and `to`".into()),
        },
        "kv_config_set" => match (s("parameter"), input.get("value").and_then(Json::as_str)) {
            // A CONFIG value may legitimately be empty (e.g. `save ""`), so `value`
            // isn't filtered for emptiness like the others.
            (Some(p), Some(v)) => WriteAssessment::NeedsApproval {
                sql: format!("CONFIG SET {p} {v}"),
            },
            _ => WriteAssessment::Reject("kv_config_set needs `parameter` and `value`".into()),
        },
        other => WriteAssessment::Reject(format!("unknown KV write tool `{other}`")),
    }
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
fn assess_changeset(input: &Json) -> WriteAssessment {
    let statements = changeset_statements(input);
    if statements.is_empty() {
        return WriteAssessment::Reject(
            "propose_changeset needs a non-empty `statements` array of INSERT/UPDATE/DELETE \
             statements"
                .into(),
        );
    }
    for (i, stmt) in statements.iter().enumerate() {
        match write_shape(stmt) {
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

/// Classify a candidate write conservatively (Feature B). The hard blocks (DDL and
/// privilege statements, an unqualified UPDATE/DELETE with no WHERE, and any chained
/// statement) are the cases per-call approval alone shouldn't be trusted to catch
/// (a rubber-stamped `DELETE` with no WHERE is catastrophic). False negatives are
/// fine: the user can always run those by hand in a query tab.
///
/// Classification runs on a **noise-stripped** copy (string literals, quoted
/// identifiers, and comments blanked) so a keyword or `;` *inside a literal* can't
/// fool the gate; e.g. `UPDATE t SET note = 'see where'` (no real WHERE) is still
/// blocked, and a `;` inside a string isn't read as statement chaining.
fn write_shape(sql: &str) -> WriteShape {
    let stripped = strip_sql_noise(sql);
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

/// Blank out the parts of `sql` that aren't structure: single-quoted strings
/// (with `''` escapes), double-quoted / backtick-quoted identifiers, and `--` line
/// and `/* */` block comments. Each run is replaced with spaces so positions and the
/// surrounding keywords are preserved. Used so the write classifier reasons about
/// real SQL keywords, never text that merely *looks* like one inside a literal.
fn strip_sql_noise(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // String literal / quoted identifier: consume to the matching close,
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
        .unwrap_or("Red — report");
    let t = red_driver::html_escape(title);
    let safe_body = strip_scripts(body);
    // The base document style: Red's active theme if the UI supplied one, else
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
/// is the real gate; the prompt is just courtesy.
pub(crate) fn system_prompt(ctx: &AiContext, policy: &AiPolicy) -> String {
    let tools_line = match policy.tier {
        AiTier::Off => {
            "You have NO database tools available; answer from the schema overview and the \
             conversation alone, and tell the user you cannot read the live database."
        }
        AiTier::Schema => {
            "You have schema-only tools: list_schema and describe_table. You can inspect \
             structure (tables, columns, types, keys) but you CANNOT read row data; there is no \
             query tool, so do not promise to run one."
        }
        AiTier::Read => {
            "You have read-only tools: list_schema, describe_table, run_select (capped SELECTs), \
             explain, open_query (open a SQL query in a new editor tab in the user's workspace; a \
             read-only SELECT runs automatically), and generate_report (you author an HTML report \
             from data you've read, with optional interactive Chart.js charts; it appears as a \
             card in the chat the user can open; use it when the user asks for a report). Use them \
             to ground every \
             answer in the live database rather than \
             guessing: discover objects with list_schema, inspect structure with describe_table, \
             and read data with run_select. Use open_query to hand the user a query to explore in \
             the grid. Prefer small, targeted queries with explicit columns and LIMIT."
        }
        AiTier::Write => {
            "You have the read tools (list_schema, describe_table, run_select, explain, open_query, \
             generate_report) AND a gated write tool, propose_write, for a SINGLE \
             INSERT/UPDATE/DELETE. Every propose_write call requires the user's explicit Allow on \
             the exact SQL; assume it may be denied, and never batch or chain statements. \
             UPDATE/DELETE must have a WHERE clause; DDL (DROP/TRUNCATE/ALTER/CREATE) is not \
             available; tell the user to run those by hand. Only write when the user has asked you \
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
            driver.column_stats(&base_sql, &col.name, numeric, want_distinct, &abort),
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(names(AiTier::Schema), ["list_schema", "describe_table"]);
        assert_eq!(
            names(AiTier::Read),
            [
                "list_schema",
                "describe_table",
                "profile_table",
                "run_select",
                "explain",
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
                "profile_table",
                "run_select",
                "explain",
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
        let sink = ReportSink::new(tx, None, 42, None, None);

        let (content, ok) = run_tool(
            &driver,
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
        assert_eq!(conversation_id, 42);
        assert_eq!(name, "Monthly revenue");
        assert_eq!(description.as_deref(), Some("Revenue for a given :month"));
        assert!(sql.contains(":month"));

        // Missing name or sql is refused, and nothing is announced.
        let (_content, ok) = run_tool(
            &driver,
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
        let sink = ReportSink::new(tx, None, 7, None, None);

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
            ..
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
        let sink = ReportSink::new(tx, None, 21, None, Some(out.clone()));

        let (_content, ok) = run_tool(
            &driver,
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
        let sink = ReportSink::new(tx, None, 11, None, None);

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
        let sink = ReportSink::new(tx, None, 13, None, None);

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
        let sink = ReportSink::new(tx, None, 17, None, None);

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
            AiBackend::Sql(driver.clone()),
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
        // exact SQL; the write has NOT run yet.
        let request_id = tokio::time::timeout(Duration::from_secs(5), async {
            // The very first event must be the approval prompt; nothing runs first.
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
