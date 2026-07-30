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
}
