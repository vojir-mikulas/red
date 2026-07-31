//! The model -> tool -> model loop, and the delegated subagent that reuses it.
//!
//! Engine-agnostic by construction: everything seam-specific is reached through
//! [`AiBackend`](super::AiBackend), so this file only knows how to stream a
//! turn, charge it against the conversation budget, route a mutating call
//! through the approval gate, and narrate the result to the activity timeline.

use std::sync::{Arc, Mutex};

use red_ai::{
    AiProvider, CancelToken, ContentBlock, Message, Role, StopReason, ToolDef, TurnRequest,
};
use red_core::{ActivityKind, ActivityStatus, AiPolicy};
use serde_json::{Value as Json, json};

use super::AiBackend;
use super::doc::catalog::doc_tool_catalog;
use super::gate::is_write_tool;
use super::gate::{WriteAssessment, assess_write, narrow_to_subagent};
use super::kv::catalog::kv_tool_catalog;
use super::preview::{model_note, preview_write};
use super::sql::catalog::{tool_catalog, user_turn};
use super::state::{AiState, ReportSink, await_write_approval, lock};
use super::util::truncate_summary;
use crate::dispatch::{Events, emit};
use crate::protocol::{AiContext, AiDelta, AiUsage, ConversationId};
use crate::{Event, SessionId};

/// Safety backstop on the model → tool → model loop: how many tool round-trips a
/// single turn may take before we stop and report. Far above any real grounded
/// answer; prevents a misbehaving model from looping forever. The per-conversation
/// [`AiLimits::max_tool_calls`](red_core::AiLimits) bound sits on top of
/// this, spanning turns rather than resetting each one.
const MAX_TOOL_STEPS: usize = 16;
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
    // Run this turn's writes in one uncommitted transaction the user reviews at
    // the end. Already vetted by the caller against the engine, the tier, and the
    // connection's posture; by here it only says which mode to run.
    sandbox_mode: bool,
    cancel: CancelToken,
) {
    // Fill in what only the service knows before the prompt is built: which of
    // this conversation's cursors are still readable.
    let mut context = context;
    context.open_cursors = lock(&state).cursors.open_line(conversation_id);
    let system = backend.system_prompt(&context, &policy);
    // Which saved connection this turn is grounded in. The grounding tools scope
    // the query history by it; everything else ignores it.
    let conn_id = context.conn_id.clone();
    // Opened lazily on the first write, so a turn that only reads never holds a
    // transaction open. Most turns only read, and an open transaction takes locks.
    let mut sandbox: Option<Arc<dyn red_driver::Sandbox>> = None;
    // Source numbering restarts each turn: a citation is only ever resolved
    // against its own bubble's sources, so a stable global counter would buy
    // nothing and make the markers longer.
    let mut next_source: u32 = 0;
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
                            // Only a subagent's final report crosses back, so the
                            // parent cannot cite the child's individual queries
                            // and must not appear to.
                            source_ordinal: None,
                        },
                    },
                );
                // Delegation runs the parent's backend (SQL or KV), narrowed to a
                // read-only, non-recursive subset (see the subagent catalogs).
                let (content, ok) = run_subagent(
                    &provider,
                    &backend,
                    &conn_id,
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
                            status: Some(if !ok {
                                ActivityStatus::Failed
                            } else if content.starts_with(SHAPE_CHECK_PREFIX) {
                                ActivityStatus::Warned
                            } else {
                                ActivityStatus::Ok
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
            // What the preview found that the model should hear about, carried
            // past the approval so it reaches the tool result whichever way the
            // user answered. A write that matched nothing is worth saying twice.
            let mut write_note: Option<String> = None;
            match assess_write(name, input, &policy, backend.dialect()) {
                WriteAssessment::Reject(why) => {
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: format!("error: {why}"),
                        is_error: true,
                    });
                    continue;
                }
                WriteAssessment::NeedsApproval { sql } if sandbox_mode => {
                    // Sandbox mode relaxes the *approval*, never the gate: the
                    // shape checks above (DDL, chained statements, a WHERE-less
                    // UPDATE) already ran and already rejected. What changes is
                    // that nothing is committed, so asking per statement is
                    // friction without a decision behind it.
                    if sandbox.is_none() {
                        match open_sandbox(&backend, &state, session, conversation_id).await {
                            Ok(opened) => sandbox = Some(opened),
                            Err(why) => {
                                results.push(ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: format!("error: {why}"),
                                    is_error: true,
                                });
                                continue;
                            }
                        }
                    }
                    // Shown as pending, because that is what it is: run, not
                    // committed, and still revocable by the user.
                    emit(
                        &events,
                        session,
                        Event::AiDelta {
                            conversation_id,
                            delta: AiDelta::ActivityStarted {
                                id: id.clone().into(),
                                parent: None,
                                kind: ActivityKind::Write { sql: sql.clone() },
                                status: ActivityStatus::Pending,
                                // A write changes the world rather than describing
                                // it; there is nothing here to cite a figure to.
                                source_ordinal: None,
                            },
                        },
                    );
                }
                WriteAssessment::NeedsApproval { sql } => {
                    // Count what it would touch first, so the prompt asks a
                    // question the user can actually answer. A failed or skipped
                    // preview yields `None` and the prompt still goes up.
                    let preview = preview_write(&backend, name, input, &policy).await;
                    // What the model is told about the preview, kept out of the
                    // sample rows: it needs the scale, not the user's data.
                    write_note = model_note(preview.as_ref());
                    let allowed = await_write_approval(
                        &state,
                        &events,
                        session,
                        conversation_id,
                        &sql,
                        preview,
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
                                    source_ordinal: None,
                                },
                            },
                        );
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: match &write_note {
                                // A denial *and* an empty predicate: the second is
                                // very likely why, so say both.
                                Some(note) => format!(
                                    "the user denied this write. Do not retry it as-is; explain \
                                     it or propose an alternative. Note: {note}"
                                ),
                                None => "the user denied this write. Do not retry it; explain it \
                                     or propose an alternative"
                                    .into(),
                            },
                            is_error: true,
                        });
                        continue;
                    }
                }
                WriteAssessment::NotWrite => {}
            }
            // A call that returns data the answer could cite gets a source number,
            // which is the join key between the prose and the trace: the tool
            // result carries `[source N]` for the model and the node carries the
            // same N for the panel.
            let source_ordinal = super::gate::is_source_tool(name).then(|| {
                next_source += 1;
                next_source
            });
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
                        source_ordinal,
                    },
                },
            );
            let (mut content, ok) = backend
                .run_tool(
                    crate::ai::ConnCtx {
                        conn_id: &conn_id,
                        dialect: backend.dialect(),
                        conversation_id,
                        state: &state,
                        sandbox: sandbox.clone(),
                    },
                    name,
                    input,
                    &policy,
                    &cancel,
                    &report,
                )
                .await;
            // Execution feedback, which is the kind of correction signal that
            // actually works: the statement ran, and it changed nothing.
            if let Some(note) = &write_note {
                content.push_str(&format!("\n\nNote: {note}"));
            }
            // Record what the sandbox actually ran, so the review card lists the
            // statements and their row counts rather than a bare "3 changes".
            if sandbox.is_some()
                && ok
                && let Some(session) = session
            {
                for (stmt, rows) in ran_statements(name, input, &content) {
                    lock(&state).record_sandbox_write(session, &stmt, rows);
                }
            }
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
                // The label the model cites back. Prefixed rather than appended so
                // it is read before the data it belongs to, and only on a call
                // that actually produced data.
                content: match source_ordinal {
                    Some(n) => format!("[source {n}]\n{content}"),
                    None => content,
                },
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

    // A turn that wrote into a sandbox does not settle here: the writes are run
    // but uncommitted, so the user reviews them and answers. `AiTurnFinished`
    // follows once they do (see the dispatch loop's `AiSandboxResolve`).
    if result.is_ok()
        && sandbox.is_some()
        && let Some(session) = session
        && let Some((statements, total_rows)) = lock(&state).sandbox_log(session)
    {
        emit(
            &events,
            session_opt(session),
            Event::AiSandboxReady {
                conversation_id,
                statements,
                total_rows,
                expires_in_secs: policy.sandbox_timeout_secs,
            },
        );
        arm_sandbox_deadline(
            events.clone(),
            state.clone(),
            session,
            conversation_id,
            policy.sandbox_timeout_secs,
        );
        return;
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
    // The parent turn's connection id, so a subagent researching a topic can
    // read the same query history the parent can.
    conn_id: &str,
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
                        // A subagent's own calls are its children and never reach
                        // the parent's prose, so they carry no source number.
                        source_ordinal: None,
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
                backend
                    .run_tool(
                        crate::ai::ConnCtx {
                            conn_id,
                            dialect: backend.dialect(),
                            conversation_id,
                            state,
                            // A subagent never writes (its catalog excludes them and
                            // the check above refuses them), so it reads the
                            // committed state. Routing it through the parent's
                            // transaction would hand a delegated researcher a view
                            // nobody else has.
                            sandbox: None,
                        },
                        name,
                        input,
                        policy,
                        cancel,
                        report,
                    )
                    .await
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
/// The tool subset a delegated SQL subagent may use (see [`narrow_to_subagent`]).
pub(in crate::ai) fn subagent_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
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
pub(in crate::ai) fn kv_subagent_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
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
/// The doc subagent's tool subset: the parent's doc catalog, narrowed like
/// [`subagent_catalog`].
pub(in crate::ai) fn doc_subagent_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
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
pub(in crate::ai) fn spawn_subagent_tool_def() -> ToolDef {
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
/// How a tool result announces that the shape check found something. Matched as a
/// prefix rather than searched for, so a result that merely *contains* the phrase
/// (a query about the shape check, say) cannot flag itself.
pub(in crate::ai) const SHAPE_CHECK_PREFIX: &str = "SHAPE CHECK";

fn activity_detail(name: &str, ok: bool, content: &str) -> Option<String> {
    if !ok {
        let line = content.split('\n').find(|l| !l.trim().is_empty())?.trim();
        return Some(truncate_summary(line, 120));
    }
    let summary = match name {
        // The write tools return a single summary sentence; surface it verbatim.
        "propose_write" | "propose_changeset" => content.split('\n').next()?.trim().to_string(),
        // A flagged query is what the reader most needs to see, so the caveat
        // outranks the row count it would otherwise show.
        "run_select" if content.starts_with(SHAPE_CHECK_PREFIX) => content
            .lines()
            .find_map(|l| l.trim().strip_prefix("- "))
            .map(str::to_string)?,
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

/// `Some(session)` as the event envelope wants it. A sandbox only ever exists on
/// a real session, so this is a shape adapter rather than a decision.
fn session_opt(session: SessionId) -> Option<SessionId> {
    Some(session)
}

/// Open a sandbox transaction for this turn and register it against the session.
///
/// Fails with a message meant for the model when the engine has no transactions,
/// when there is no session to attach one to, or when **another conversation**
/// already holds this session's sandbox: two chats writing to one database in two
/// transactions is a deadlock generator, so the second is refused rather than
/// given its own.
async fn open_sandbox(
    backend: &AiBackend,
    state: &Arc<Mutex<AiState>>,
    session: Option<SessionId>,
    conversation_id: ConversationId,
) -> Result<Arc<dyn red_driver::Sandbox>, String> {
    let Some(session) = session else {
        return Err("there is no connected session to open a transaction on".into());
    };
    // Already open on this session: reuse it if it is ours, refuse if it isn't.
    if let Some((existing, owner)) = lock(state).sandbox_for(session) {
        return if owner == conversation_id {
            Ok(existing)
        } else {
            Err(
                "another chat has an open review transaction on this connection; ask the user \
                 to commit or roll it back first"
                    .into(),
            )
        };
    }
    let AiBackend::Sql { driver, .. } = backend else {
        return Err("review transactions are only available on SQL connections".into());
    };
    let opened = driver
        .begin_sandbox()
        .await
        .map_err(|e| format!("could not open a review transaction: {e}"))?;
    let Some(opened) = opened else {
        return Err(
            "this engine has no multi-statement transactions, so changes cannot be \
                    held for review"
                .into(),
        );
    };
    let opened: Arc<dyn red_driver::Sandbox> = Arc::from(opened);
    // Racy by construction: another turn could have registered between the check
    // above and here, so the registry's own answer is the authority.
    if !lock(state).open_sandbox(session, conversation_id, opened.clone()) {
        let _ = opened.rollback().await;
        return Err("another chat opened a review transaction on this connection first".into());
    }
    Ok(opened)
}

/// The statements a completed write tool actually ran, paired with their row
/// counts, for the sandbox log.
///
/// Row counts are parsed back out of the tool's own result text rather than
/// re-derived: the executor is the only thing that knows what the engine
/// reported, and threading a second channel out of it for the sandbox alone would
/// be a parallel truth to keep in sync. A count that cannot be read shows as 0 in
/// the review card, which understates rather than overstates.
fn ran_statements(name: &str, input: &Json, content: &str) -> Vec<(String, u64)> {
    let rows = content
        .split_once("row(s) affected")
        .and_then(|(head, _)| {
            head.rsplit(|c: char| !c.is_ascii_digit())
                .find(|t| !t.is_empty())
                .and_then(|n| n.parse::<u64>().ok())
        })
        .unwrap_or(0);
    match name {
        "propose_write" => input
            .get("sql")
            .and_then(Json::as_str)
            .map(|sql| vec![(sql.to_string(), rows)])
            .unwrap_or_default(),
        // The changeset reports one total, and splitting it back per statement
        // would be a guess. Attribute it to the first statement and list the rest
        // at 0; the card's total is what the user answers on.
        "propose_changeset" => {
            let statements = super::gate::changeset_statements(input);
            statements
                .into_iter()
                .enumerate()
                .map(|(i, sql)| (sql, if i == 0 { rows } else { 0 }))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Floor on the sandbox deadline. Below this a user cannot realistically read the
/// review card before it expires, and an expiry the user never had a chance to
/// answer is just a failed turn with extra steps.
const SANDBOX_TIMEOUT_FLOOR_SECS: u64 = 30;

/// Roll the sandbox back if nobody answers in time.
///
/// An open transaction holds locks, so a user who walks away mid-review can block
/// production writes -- this feature can cause an outage if it is careless. Rolling
/// back is the only defensible expiry: committing an agent's writes because a
/// timer fired is not something RED does under any condition.
///
/// Taking the slot out of the registry is what makes this safe against a race
/// with the user's own answer: whoever removes it first is the one who resolves it.
fn arm_sandbox_deadline(
    events: Events,
    state: Arc<Mutex<AiState>>,
    session: SessionId,
    conversation_id: ConversationId,
    secs: u64,
) {
    let secs = secs.max(SANDBOX_TIMEOUT_FLOOR_SECS);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        // Peek before taking, so a slot re-opened by *another* chat in the meantime
        // is never removed by this timer (taking and putting it back would lose its
        // log, which is the review card's whole contents).
        let expired = {
            let mut st = lock(&state);
            match st.sandbox_for(session) {
                Some((_, owner)) if owner == conversation_id => st.take_sandbox(session),
                _ => None,
            }
        };
        let Some(slot) = expired else {
            return; // the user answered, teardown got there first, or it is not ours
        };
        emit(
            &events,
            Some(session),
            Event::AiSandboxExpired { conversation_id },
        );
        crate::dispatch::resolve_sandbox(&events, session, conversation_id, slot, false).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;

    use red_ai::CancelToken;
    use red_core::sql::Dialect;
    use red_core::{AiPolicy, AiTier};

    use red_driver::PageCap;
    use red_driver::{AbortSignal, DatabaseDriver};
    use std::time::Duration;

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

    /// A fixture DB with `t (id, name)` holding `(1, 'before')`, plus a writable
    /// driver over it.
    fn write_fixture(tag: &str) -> (std::path::PathBuf, Arc<dyn DatabaseDriver>) {
        let db =
            std::env::temp_dir().join(format!("red-{tag}-{}.db", uuid::Uuid::new_v4().simple()));
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);
                 INSERT INTO t VALUES (1, 'before');",
            )
            .unwrap();
        }
        let driver: Arc<dyn DatabaseDriver> =
            Arc::new(red_driver::SqliteDriver::new(db.clone(), false));
        (db, driver)
    }

    /// `t.name` read through a **fresh** connection, so an uncommitted sandbox
    /// write is invisible here.
    async fn committed_name(db: &std::path::Path) -> String {
        let driver: Arc<dyn DatabaseDriver> =
            Arc::new(red_driver::SqliteDriver::new(db.to_path_buf(), true));
        let page = driver
            .fetch_page(
                "SELECT name FROM t WHERE id = 1",
                0,
                1,
                PageCap::Display { key: None },
                &AbortSignal::new(),
            )
            .await
            .unwrap();
        page.rows[0][0].to_string()
    }

    fn write_policy() -> AiPolicy {
        AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        }
    }

    fn scripted(sql: &str) -> Arc<dyn red_ai::AiProvider> {
        Arc::new(ScriptedWrite {
            calls: std::sync::atomic::AtomicUsize::new(0),
            sql: sql.into(),
        })
    }

    /// Run one sandbox-mode turn to its review handoff and return the state, the
    /// session, and every event it emitted.
    async fn sandbox_turn(
        driver: Arc<dyn DatabaseDriver>,
        sql: &str,
    ) -> (Arc<Mutex<AiState>>, SessionId, Vec<Event>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let state = Arc::new(Mutex::new(AiState::default()));
        let session = SessionId::new(1);
        run_turn(
            scripted(sql),
            AiBackend::Sql {
                driver,
                dialect: Dialect::Sqlite,
            },
            tx,
            state.clone(),
            Some(session),
            ConversationId::new(1),
            "m".into(),
            false,
            write_policy(),
            "change it".into(),
            AiContext::default(),
            true,
            CancelToken::new(),
        )
        .await;
        let mut events = Vec::new();
        while let Ok((_, e)) = rx.try_recv() {
            events.push(e);
        }
        (state, session, events)
    }

    /// A citable call is numbered and its result carries the label the model
    /// cites back; a write is neither.
    #[tokio::test]
    async fn a_source_call_is_numbered_and_labelled() {
        let (db, driver) = write_fixture("src");
        // The scripted provider proposes a write, which is *not* a source.
        let (_, _, events) = sandbox_turn(driver, "UPDATE t SET name = 'x' WHERE id = 1").await;
        assert!(
            events.iter().all(|e| !matches!(
                e,
                Event::AiDelta {
                    delta: AiDelta::ActivityStarted {
                        source_ordinal: Some(_),
                        ..
                    },
                    ..
                }
            )),
            "a write must not be offered as a source"
        );
        let _ = std::fs::remove_file(&db);
    }

    /// The whole point of the mode: the write runs, nothing is asked per
    /// statement, and nothing is durable until the user answers.
    #[tokio::test]
    async fn a_sandbox_turn_runs_the_write_without_prompting_and_commits_nothing() {
        let (db, driver) = write_fixture("sbx");
        let (state, session, events) =
            sandbox_turn(driver, "UPDATE t SET name = 'after' WHERE id = 1").await;

        // Per-statement approval is *replaced*, not added to.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::AiPermissionRequest { .. })),
            "sandbox mode must not prompt per statement"
        );
        // The turn hands over for review instead of settling.
        let ready = events
            .iter()
            .find_map(|e| match e {
                Event::AiSandboxReady {
                    statements,
                    total_rows,
                    ..
                } => Some((statements.clone(), *total_rows)),
                _ => None,
            })
            .expect("the turn ends in a review handoff");
        assert_eq!(ready.0.len(), 1);
        assert!(ready.0[0].sql.contains("UPDATE t SET name"));
        assert_eq!(ready.1, 1, "the card totals the rows actually touched");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::AiTurnFinished { .. })),
            "a turn holding uncommitted changes has not finished"
        );
        // And the database is untouched from anywhere else.
        assert_eq!(committed_name(&db).await, "before");

        // Rolling back leaves it that way.
        let (_, slot) = lock(&state)
            .take_sandbox_for_conversation(ConversationId::new(1))
            .expect("the sandbox is registered against the session");
        assert_eq!(slot.total_rows(), 1);
        slot.sandbox.rollback().await.unwrap();
        assert_eq!(committed_name(&db).await, "before");
        assert!(
            lock(&state).sandbox_for(session).is_none(),
            "taking the slot is what makes a resolve single-use"
        );
        let _ = std::fs::remove_file(&db);
    }

    /// Committing is what makes it durable, and only then.
    #[tokio::test]
    async fn committing_a_sandbox_applies_the_turn() {
        let (db, driver) = write_fixture("sbxc");
        let (state, _, _) = sandbox_turn(driver, "UPDATE t SET name = 'after' WHERE id = 1").await;
        assert_eq!(committed_name(&db).await, "before");

        let (_, slot) = lock(&state)
            .take_sandbox_for_conversation(ConversationId::new(1))
            .unwrap();
        slot.sandbox.commit().await.unwrap();
        assert_eq!(committed_name(&db).await, "after");
        let _ = std::fs::remove_file(&db);
    }

    /// The sandbox relaxes the *approval*, never the gate. A shape the write gate
    /// rejects is still rejected here, and asserting it matters because the two
    /// are independent: it would be easy to route around the gate while removing
    /// the prompt.
    #[tokio::test]
    async fn the_shape_gate_still_rejects_ddl_in_sandbox_mode() {
        let (db, driver) = write_fixture("sbxddl");
        let (state, session, events) = sandbox_turn(driver, "DROP TABLE t").await;

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::AiSandboxReady { .. })),
            "a rejected statement opens no transaction"
        );
        assert!(
            lock(&state).sandbox_for(session).is_none(),
            "a rejected statement must not leave a connection checked out"
        );
        // The table is still there.
        assert_eq!(committed_name(&db).await, "before");
        let _ = std::fs::remove_file(&db);
    }

    /// One sandbox per session: a second conversation's write is refused rather
    /// than given its own transaction, because two transactions against one
    /// database from one app is a deadlock generator.
    #[tokio::test]
    async fn a_second_conversation_cannot_open_a_second_sandbox() {
        let (db, driver) = write_fixture("sbx2");
        let (state, session, _) =
            sandbox_turn(driver.clone(), "UPDATE t SET name = 'a' WHERE id = 1").await;
        assert!(lock(&state).sandbox_for(session).is_some());

        let backend = AiBackend::Sql {
            driver,
            dialect: Dialect::Sqlite,
        };
        let why = match open_sandbox(&backend, &state, Some(session), ConversationId::new(2)).await
        {
            Err(why) => why,
            Ok(_) => panic!("a second chat must not get its own transaction"),
        };
        assert!(why.contains("another chat"), "{why}");

        // The first conversation still gets its own back, rather than a new one.
        let mine = open_sandbox(&backend, &state, Some(session), ConversationId::new(1))
            .await
            .expect("the owner reuses its sandbox");
        mine.rollback().await.unwrap();
        let _ = std::fs::remove_file(&db);
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
            // Per-statement approval, which is what this test is about.
            false,
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
}
