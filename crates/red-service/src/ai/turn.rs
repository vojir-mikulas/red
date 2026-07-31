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
/// [`AiLimits::max_tool_calls`](red_core::AiLimits) bound is the *budget* that
/// actually bites, and it spans turns rather than resetting each one — so this
/// sits high enough that a genuine investigation never meets it.
const MAX_TOOL_STEPS: usize = 32;

/// How many times a reply cut off at the output ceiling may be asked to carry on
/// before the turn settles with an error. Small on purpose: a model that keeps
/// filling the ceiling is not going to finish, and each attempt costs a full
/// ceiling's worth of output tokens.
const MAX_CONTINUATIONS: usize = 3;

/// What the model is told when its reply hit the output ceiling mid-sentence.
const CONTINUE_PROMPT: &str = "Your previous message hit the output limit and was cut off mid-way. \
     Continue from exactly where you stopped; do not repeat what you already wrote and do not \
     start over.";

/// What the model is told when the turn runs out of tool steps. Mirrors the
/// tool-budget message: the loop is over either way, so the useful thing is an
/// answer from what it already gathered rather than a truncated trace.
const OUT_OF_STEPS_PROMPT: &str = "You have used every tool step available for this turn. Stop calling tools and answer now \
     with what you already have, stating plainly anything you could not finish.";

/// What `provider` may do to keep a conversation inside its context window.
///
/// Both strategies where they exist: clearing tool results the model has already
/// drawn its conclusions from is lossless, and compacting is the fallback for
/// when clearing is not enough. A provider that reports no support gets an empty
/// request and the local trim in [`trim_history`] takes over instead.
fn context_management(provider: &Arc<dyn AiProvider>) -> red_ai::ContextManagement {
    if provider.capabilities().context_management {
        red_ai::ContextManagement {
            clear_tool_results: true,
            compact: true,
        }
    } else {
        red_ai::ContextManagement::default()
    }
}

/// Fold one model call's tokens into the turn's running figures.
///
/// Billed input and output accumulate. The prompt size does **not**: what the
/// conversation currently occupies is the last request's prompt, and every
/// earlier one is already inside it — summing them would climb past the window
/// long before the conversation did.
fn charge_step(usage: &mut AiUsage, step: &red_ai::Usage) {
    usage.input_tokens += step.input_tokens;
    usage.output_tokens += step.output_tokens;
    usage.cache_read_input_tokens += step.cache_read_input_tokens;
    usage.context_used_tokens = step.input_tokens + step.cache_read_input_tokens;
}

/// Rough size of a message history in tokens, for deciding *when* to trim.
///
/// Four bytes to the token is the usual English approximation, and approximate is
/// all this needs to be: the cost of being wrong is trimming one step early or
/// one step late. Anything precise would mean a tokenizer per provider.
fn estimate_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .map(|b| match b {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::Thinking { text, signature } => text.len() + signature.len(),
            ContentBlock::RedactedThinking { data } => data.len(),
            ContentBlock::Compaction { content } => content.len(),
            ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
            ContentBlock::ToolResult { content, .. } => content.len(),
            ContentBlock::Image { data, .. } => data.len(),
            ContentBlock::Document { source, title } => {
                let body = match source {
                    red_ai::DocumentSource::Pdf { data }
                    | red_ai::DocumentSource::Text { data } => data.len(),
                };
                body + title.as_ref().map_or(0, String::len)
            }
        })
        .sum::<usize>()
        / 4
}

/// Above this estimate, a history the provider will not manage for us gets
/// trimmed locally.
///
/// Deliberately under the smallest window a current model ships with, because
/// the estimate is rough and the system prompt, the tool catalog, and the reply
/// all have to fit alongside it.
const LOCAL_TRIM_THRESHOLD_TOKENS: usize = 100_000;

/// How many messages at the end of the transcript stay whole. The recent steps
/// are the ones the model is actually reasoning over; the older ones it has
/// already drawn its conclusions from, and those conclusions are in the
/// transcript.
const KEEP_RECENT_MESSAGES: usize = 8;

/// Marker left where a tool result used to be.
fn dropped_stub(tool: &str, bytes: usize) -> String {
    format!("[result of {tool} ({bytes} bytes) dropped to save context]")
}

/// Replace the content of tool results outside the recent tail with a one-line
/// stub, returning how many were newly stubbed. Used only for providers that
/// report no context management of their own.
///
/// Three invariants, in order of how badly breaking them hurts:
///
/// - **Every `ToolUse` keeps a matching `ToolResult`.** Results are *rewritten*,
///   never removed: a `tool_use` without its `tool_result` is a 400, which turns
///   a context optimization into a dead conversation.
/// - **Thinking blocks are never touched.** The API rejects an edited thinking
///   block on a tool-use follow-up.
/// - **The first user message survives**, because it is the question.
fn trim_history(messages: &mut [Message]) -> usize {
    let Some(cut) = messages.len().checked_sub(KEEP_RECENT_MESSAGES) else {
        return 0;
    };
    // Which tool produced each result, so the stub can name it. Read from the
    // whole history: a result's `ToolUse` is in the message before it, which may
    // itself be inside the tail we keep.
    let tools: std::collections::HashMap<String, String> = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, .. } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect();

    let mut dropped = 0;
    // The first message is the user's question and is never a tool result, but
    // skipping it says so rather than relying on that.
    for message in messages.iter_mut().take(cut).skip(1) {
        for block in &mut message.content {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
            else {
                continue;
            };
            if content.starts_with("[result of ") {
                continue; // already stubbed on an earlier step
            }
            let tool = tools
                .get(tool_use_id)
                .map(String::as_str)
                .unwrap_or("a tool");
            *content = dropped_stub(tool, content.len());
            dropped += 1;
        }
    }
    dropped
}

/// Stream one model turn, relaying its deltas to the panel as they arrive.
///
/// Extracted because the turn runs the model from three places -- the tool loop,
/// a continuation after the output ceiling, and the wrap-up turn once the steps
/// run out -- and the relay task's shutdown ordering (drop the sender, *then*
/// await the relay) is the kind of thing that only stays right in one copy.
async fn stream_step(
    provider: &Arc<dyn AiProvider>,
    req: &TurnRequest<'_>,
    events: &Events,
    session: Option<SessionId>,
    conversation_id: ConversationId,
    cancel: &CancelToken,
) -> red_ai::Result<red_ai::TurnOutcome> {
    let (dtx, mut drx) = tokio::sync::mpsc::unbounded_channel::<red_ai::Delta>();
    let relay = {
        let events = events.clone();
        tokio::spawn(async move {
            while let Some(d) = drx.recv().await {
                // Tool calls become activity nodes at the execution site, where
                // the arguments are known; the streamed `ToolUseStarted` is only
                // an early hint, so it is dropped here.
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
    let outcome = provider.stream_turn(req, &dtx, cancel).await;
    drop(dtx);
    let _ = relay.await;
    outcome
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
    // Files the user attached, already read and vetted by the UI. They become
    // sibling content blocks of the message text, ordered ahead of it.
    attachments: Vec<crate::protocol::TurnAttachment>,
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
    // What the user pointed at, resolved against the live database with the same
    // formatters the tools use — so a dragged table and a `describe_table` call
    // describe it identically.
    let references = super::resolve_references(&backend, &context.references, &policy).await;

    let mut messages = {
        let mut st = lock(&state);
        let history = st.histories.entry(conversation_id).or_default();
        history.push(super::attach::user_message(
            user_turn(&user_message, &context, references.as_deref()),
            &attachments,
        ));
        history.clone()
    };

    let context_mgmt = context_management(&provider);

    // The window this turn's history has to fit in, when the model is one we
    // recognize. Zero leaves the panel showing a token count instead of a
    // fullness percentage, which is the honest answer for an unknown model.
    let mut usage = AiUsage {
        context_tokens: red_ai::context_window(&model).unwrap_or(0),
        ..AiUsage::default()
    };
    let mut result: std::result::Result<(), String> = Ok(());
    // How many times the reply has been cut off at the output ceiling and asked
    // to carry on. Capped, because a model that keeps filling the ceiling is not
    // converging on an answer.
    let mut continuations = 0usize;

    // `true` when the loop ran out of tool steps with the model still reaching
    // for tools, as opposed to reaching any natural end. Distinguishing the two
    // is the whole point: a bare `break` settles a truncated trace under the
    // ordinary "finished" footer, which reads as a complete answer.
    let out_of_steps =
        'steps: {
            for step in 0..MAX_TOOL_STEPS {
                if cancel.is_cancelled() {
                    result = Err("cancelled".into());
                    break 'steps false;
                }

                // A provider that manages context itself was already asked to
                // (see `context_management`). For the rest, the history has to be
                // kept inside the window here, or a long investigation ends at
                // the ceiling with nothing to show for itself.
                if context_mgmt.is_empty()
                    && estimate_tokens(&messages) > LOCAL_TRIM_THRESHOLD_TOKENS
                {
                    let dropped = trim_history(&mut messages);
                    if dropped > 0 {
                        emit(
                            &events,
                            session,
                            Event::AiDelta {
                                conversation_id,
                                delta: AiDelta::ActivityStarted {
                                    id: format!("compacted-{step}").into(),
                                    parent: None,
                                    kind: ActivityKind::Compacted { dropped },
                                    status: ActivityStatus::Ok,
                                    // It produced no data, so there is nothing to
                                    // cite it for.
                                    source_ordinal: None,
                                },
                            },
                        );
                    }
                }

                let req = TurnRequest {
                    model: &model,
                    max_tokens: policy.limits.max_output_tokens,
                    show_thinking,
                    system: &system,
                    tools: &tools,
                    messages: &messages,
                    context: context_mgmt,
                };

                let outcome =
                    match stream_step(&provider, &req, &events, session, conversation_id, &cancel)
                        .await
                    {
                        Ok(o) => o,
                        Err(e) => {
                            result = Err(e.to_string());
                            break 'steps false;
                        }
                    };

                charge_step(&mut usage, &outcome.usage);
                messages.push(outcome.message.clone());

                // A truncated message that still carries a complete tool call is not a
                // continuation: dropping a `ToolUse` without its `ToolResult` is an API
                // error, and running the tool re-asks the model anyway.
                let wants_tools = outcome
                    .message
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }));

                // `MaxTokens` means the ceiling cut the reply off mid-sentence.
                // Settling it with the ordinary "finished" footer would make a
                // truncated turn indistinguishable from a complete one — but erroring
                // is not the only honest answer, and it is the least useful one. Ask
                // the model to carry on from where it stopped; the text keeps
                // streaming into the same bubble, which is what the user expects.
                if outcome.stop_reason == StopReason::MaxTokens && !wants_tools {
                    if continuations == MAX_CONTINUATIONS {
                        result = Err(
                            "the reply keeps hitting this agent's max-tokens ceiling and stops \
                         mid-answer; raise the limit in Settings → AI or ask for a shorter answer"
                                .to_string(),
                        );
                        break 'steps false;
                    }
                    continuations += 1;
                    messages.push(Message::user_text(CONTINUE_PROMPT));
                    continue;
                }
                if outcome.stop_reason != StopReason::ToolUse && !wants_tools {
                    break 'steps false;
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
                    if !lock(&state).charge_tool_call(conversation_id, policy.limits.max_tool_calls)
                    {
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
                                match open_sandbox(&backend, &state, session, conversation_id).await
                                {
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
                    break 'steps false;
                }
                messages.push(Message {
                    role: Role::User,
                    content: results,
                });
            }
            true
        };

    // Out of tool steps with the model still working. The tool-budget path
    // already gets this right: hand the model the bad news and let it answer with
    // what it has, tool-less so it cannot start another round. Breaking here
    // instead would settle a partial answer under the ordinary footer.
    if out_of_steps && result.is_ok() {
        messages.push(Message::user_text(OUT_OF_STEPS_PROMPT));
        let req = TurnRequest {
            model: &model,
            max_tokens: policy.limits.max_output_tokens,
            show_thinking,
            system: &system,
            tools: &[],
            messages: &messages,
            context: context_mgmt,
        };
        match stream_step(&provider, &req, &events, session, conversation_id, &cancel).await {
            Ok(outcome) => {
                charge_step(&mut usage, &outcome.usage);
                messages.push(outcome.message);
            }
            Err(e) => result = Err(e.to_string()),
        }
    }

    // Persist history, fold this turn's tokens into the conversation's running
    // total, and drop the cancel registration. The footer reports the total
    // rather than the last turn: "should I start a new chat" is a question about
    // the conversation.
    let usage = {
        let mut st = lock(&state);
        st.histories.insert(conversation_id, messages);
        st.cancels.remove(&conversation_id);
        st.charge_usage(conversation_id, usage)
    };

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
            // Half the parent's ceiling, floored so a thrifty setting still leaves
            // room for a report: a subagent is a bounded errand and what comes
            // back should be a summary, not a second transcript.
            max_tokens: (policy.limits.max_output_tokens / 2).max(2048),
            show_thinking: false,
            system: &system,
            tools: &tools,
            messages: &messages,
            context: context_management(provider),
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
            Vec::new(),
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
            Vec::new(),
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

    // --- context hygiene ------------------------------------------------------

    /// A provider whose first `truncated` replies are cut off at the output
    /// ceiling, after which it finishes. Counts its calls so a test can prove how
    /// many attempts the loop actually made.
    struct Ceiling {
        calls: std::sync::atomic::AtomicUsize,
        truncated: usize,
    }

    #[async_trait::async_trait]
    impl red_ai::AiProvider for Ceiling {
        async fn stream_turn(
            &self,
            _req: &red_ai::TurnRequest<'_>,
            _tx: &tokio::sync::mpsc::UnboundedSender<red_ai::Delta>,
            _cancel: &CancelToken,
        ) -> red_ai::Result<red_ai::TurnOutcome> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (text, stop_reason) = if n < self.truncated {
                (format!("part {n} "), StopReason::MaxTokens)
            } else {
                ("done".to_string(), StopReason::EndTurn)
            };
            Ok(red_ai::TurnOutcome {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text { text }],
                },
                stop_reason,
                usage: red_ai::Usage::default(),
            })
        }
    }

    /// A provider that never stops asking for a tool, so the turn runs the step
    /// budget dry. Records the tool-catalog size of the last request, which is how
    /// the wrap-up turn proves it offered no tools.
    #[derive(Default)]
    struct Insatiable {
        calls: std::sync::atomic::AtomicUsize,
        last_tool_count: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl red_ai::AiProvider for Insatiable {
        async fn stream_turn(
            &self,
            req: &red_ai::TurnRequest<'_>,
            _tx: &tokio::sync::mpsc::UnboundedSender<red_ai::Delta>,
            _cancel: &CancelToken,
        ) -> red_ai::Result<red_ai::TurnOutcome> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.last_tool_count
                .store(req.tools.len(), std::sync::atomic::Ordering::SeqCst);
            // No tools left to call is the wrap-up turn; answer it.
            if req.tools.is_empty() {
                return Ok(red_ai::TurnOutcome {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: "here is what I found".into(),
                        }],
                    },
                    stop_reason: StopReason::EndTurn,
                    usage: red_ai::Usage::default(),
                });
            }
            Ok(red_ai::TurnOutcome {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: format!("t{n}"),
                        name: "list_schema".into(),
                        input: json!({}),
                    }],
                },
                stop_reason: StopReason::ToolUse,
                usage: red_ai::Usage::default(),
            })
        }
    }

    /// Drive one read-tier turn to completion against a real SQLite driver,
    /// returning the shared state and every event it emitted.
    async fn read_turn(
        provider: Arc<dyn red_ai::AiProvider>,
        driver: Arc<dyn DatabaseDriver>,
    ) -> (Arc<Mutex<AiState>>, Vec<Event>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let state = Arc::new(Mutex::new(AiState::default()));
        run_turn(
            provider,
            AiBackend::Sql {
                driver,
                dialect: Dialect::Sqlite,
            },
            tx,
            state.clone(),
            None,
            ConversationId::new(1),
            "m".into(),
            false,
            AiPolicy::default(),
            "look into it".into(),
            Vec::new(),
            AiContext::default(),
            false,
            CancelToken::new(),
        )
        .await;
        let mut events = Vec::new();
        while let Ok((_, e)) = rx.try_recv() {
            events.push(e);
        }
        (state, events)
    }

    fn settled_ok(events: &[Event]) -> bool {
        events
            .iter()
            .any(|e| matches!(e, Event::AiTurnFinished { .. }))
    }

    fn error_message(events: &[Event]) -> Option<String> {
        events.iter().find_map(|e| match e {
            Event::AiError { message, .. } => Some(message.clone()),
            _ => None,
        })
    }

    /// A reply cut off at the ceiling is continued, not failed: the turn settles
    /// normally and the history holds every partial plus the prompt that asked for
    /// the rest.
    #[tokio::test]
    async fn a_truncated_reply_is_continued_rather_than_failed() {
        let (db, driver) = write_fixture("ceil");
        let provider = Arc::new(Ceiling {
            calls: std::sync::atomic::AtomicUsize::new(0),
            truncated: 2,
        });
        let (state, events) = read_turn(provider.clone(), driver).await;

        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "two continuations then the finishing reply"
        );
        assert!(settled_ok(&events), "{:?}", error_message(&events));

        let history = lock(&state).histories[&ConversationId::new(1)].clone();
        let assistant_text: Vec<String> = history
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(assistant_text, ["part 0 ", "part 1 ", "done"]);
        let continues = history
            .iter()
            .filter(|m| m.role == Role::User)
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::Text { text } if text == CONTINUE_PROMPT))
            .count();
        assert_eq!(continues, 2, "one prompt per truncated reply");
        let _ = std::fs::remove_file(&db);
    }

    /// The continuation is bounded: past the cap the turn settles with the honest
    /// error rather than attempting again forever.
    #[tokio::test]
    async fn continuations_are_capped() {
        let (db, driver) = write_fixture("ceilcap");
        let provider = Arc::new(Ceiling {
            calls: std::sync::atomic::AtomicUsize::new(0),
            truncated: usize::MAX,
        });
        let (_, events) = read_turn(provider.clone(), driver).await;

        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            MAX_CONTINUATIONS + 1,
            "the first reply plus one attempt per continuation, and no more"
        );
        assert!(
            !settled_ok(&events),
            "a still-truncated turn is not finished"
        );
        assert!(
            error_message(&events)
                .unwrap_or_default()
                .contains("max-tokens ceiling"),
            "the error names what actually happened"
        );
        let _ = std::fs::remove_file(&db);
    }

    /// Running out of tool steps ends in an answer, not a bare `break`: exactly
    /// one further turn runs, with no tools to reach for, and the turn settles.
    #[tokio::test]
    async fn exhausting_the_step_budget_runs_one_tool_less_wrap_up() {
        let (db, driver) = write_fixture("steps");
        let provider = Arc::new(Insatiable::default());
        let (_, events) = read_turn(provider.clone(), driver).await;

        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            MAX_TOOL_STEPS + 1,
            "every step, then exactly one wrap-up turn"
        );
        assert_eq!(
            provider
                .last_tool_count
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the wrap-up turn is offered no tools, so it cannot start another round"
        );
        assert!(
            settled_ok(&events),
            "the user gets an answer: {:?}",
            error_message(&events)
        );
        let _ = std::fs::remove_file(&db);
    }

    /// A provider that keeps context itself is asked to, and whatever it hands
    /// back for that purpose survives into the history unedited.
    ///
    /// The second half is the one a refactor breaks: a compaction block stands
    /// for the history the provider folded away, so a loop that "helpfully"
    /// filtered the assistant message down to its text would lose the
    /// conversation without any visible symptom until the next turn.
    #[tokio::test]
    async fn a_context_managing_provider_is_asked_and_its_compaction_survives() {
        struct Compacting {
            asked: Arc<Mutex<Vec<red_ai::ContextManagement>>>,
        }
        #[async_trait::async_trait]
        impl red_ai::AiProvider for Compacting {
            async fn stream_turn(
                &self,
                req: &red_ai::TurnRequest<'_>,
                _tx: &tokio::sync::mpsc::UnboundedSender<red_ai::Delta>,
                _cancel: &CancelToken,
            ) -> red_ai::Result<red_ai::TurnOutcome> {
                self.asked.lock().unwrap().push(req.context);
                Ok(red_ai::TurnOutcome {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![
                            ContentBlock::Compaction {
                                content: "earlier: the user asked about orders".into(),
                            },
                            ContentBlock::Text {
                                text: "14 columns".into(),
                            },
                        ],
                    },
                    stop_reason: StopReason::EndTurn,
                    usage: red_ai::Usage::default(),
                })
            }
            fn capabilities(&self) -> red_ai::ProviderCapabilities {
                red_ai::ProviderCapabilities {
                    context_management: true,
                }
            }
        }

        let (db, driver) = write_fixture("compact");
        let asked = Arc::new(Mutex::new(Vec::new()));
        let (state, _) = read_turn(
            Arc::new(Compacting {
                asked: asked.clone(),
            }),
            driver,
        )
        .await;

        assert_eq!(
            asked.lock().unwrap().as_slice(),
            [red_ai::ContextManagement {
                clear_tool_results: true,
                compact: true,
            }],
            "a provider that can keep context is asked for both strategies"
        );
        let history = lock(&state).histories[&ConversationId::new(1)].clone();
        let compactions: Vec<String> = history
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Compaction { content } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(compactions, ["earlier: the user asked about orders"]);
        let _ = std::fs::remove_file(&db);
    }

    /// A provider that keeps no context of its own is asked for nothing, so it
    /// never receives a beta it cannot honour.
    #[tokio::test]
    async fn a_plain_provider_is_asked_for_no_context_management() {
        let (db, driver) = write_fixture("nocompact");
        let seen = Arc::new(Mutex::new(Vec::new()));
        struct Plain(Arc<Mutex<Vec<red_ai::ContextManagement>>>);
        #[async_trait::async_trait]
        impl red_ai::AiProvider for Plain {
            async fn stream_turn(
                &self,
                req: &red_ai::TurnRequest<'_>,
                _tx: &tokio::sync::mpsc::UnboundedSender<red_ai::Delta>,
                _cancel: &CancelToken,
            ) -> red_ai::Result<red_ai::TurnOutcome> {
                self.0.lock().unwrap().push(req.context);
                Ok(red_ai::TurnOutcome {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text { text: "ok".into() }],
                    },
                    stop_reason: StopReason::EndTurn,
                    usage: red_ai::Usage::default(),
                })
            }
        }
        read_turn(Arc::new(Plain(seen.clone())), driver).await;
        assert!(seen.lock().unwrap().iter().all(|c| c.is_empty()));
        let _ = std::fs::remove_file(&db);
    }

    /// A transcript with `steps` tool round-trips: user question, then one
    /// assistant `ToolUse` and one user `ToolResult` per step, then a thinking
    /// block on the last assistant turn.
    fn transcript(steps: usize) -> Vec<Message> {
        let mut messages = vec![Message::user_text("how many orders?")];
        for i in 0..steps {
            messages.push(Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: format!("thinking about step {i}"),
                        signature: format!("sig{i}"),
                    },
                    ContentBlock::ToolUse {
                        id: format!("t{i}"),
                        name: "run_select".into(),
                        input: json!({ "sql": "SELECT 1" }),
                    },
                ],
            });
            messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: format!("t{i}"),
                    content: "a | b\n".repeat(500),
                    is_error: false,
                }],
            });
        }
        messages
    }

    /// The invariant that turns a context optimization into a 400 if broken:
    /// every `ToolUse` still has its `ToolResult` after a trim. Results are
    /// rewritten, never removed.
    #[test]
    fn trimming_never_orphans_a_tool_use() {
        let mut messages = transcript(12);
        assert!(
            trim_history(&mut messages) > 0,
            "there is something to trim"
        );

        let uses: Vec<&String> = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        let results: Vec<&String> = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id),
                _ => None,
            })
            .collect();
        assert_eq!(uses, results, "every tool_use keeps its tool_result");
    }

    /// What a trim may and may not touch: old results are stubbed, thinking
    /// blocks and the question survive, and the recent tail is left whole.
    #[test]
    fn trimming_spares_thinking_the_question_and_the_recent_tail() {
        let mut messages = transcript(12);
        let dropped = trim_history(&mut messages);

        // The question is still the question.
        assert!(matches!(
            &messages[0].content[0],
            ContentBlock::Text { text } if text == "how many orders?"
        ));
        // Thinking blocks are untouched: the API rejects an edited one.
        let thinking: Vec<String> = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Thinking { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            thinking.len(),
            12,
            "no thinking block was dropped or edited"
        );
        assert!(
            thinking
                .iter()
                .all(|t| t.starts_with("thinking about step"))
        );

        // Old results are stubbed, and they name the tool that produced them.
        let stubbed = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| {
                matches!(b, ContentBlock::ToolResult { content, .. }
                if content.starts_with("[result of run_select"))
            })
            .count();
        assert_eq!(stubbed, dropped);
        // The recent tail is whole: `KEEP_RECENT_MESSAGES` messages, which is
        // four of these two-message steps.
        let tail_intact = messages[messages.len() - KEEP_RECENT_MESSAGES..]
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| {
                matches!(b, ContentBlock::ToolResult { content, .. }
                if content.starts_with("a | b"))
            })
            .count();
        assert_eq!(tail_intact, KEEP_RECENT_MESSAGES / 2);

        // Trimming again finds nothing new: a stub is not re-stubbed.
        assert_eq!(trim_history(&mut messages), 0);
    }

    /// A short conversation is left entirely alone — there is no tail to trim
    /// behind, and a `checked_sub` that underflowed would be a panic.
    #[test]
    fn a_short_history_is_not_trimmed() {
        let mut messages = transcript(2);
        let before = messages.clone();
        assert_eq!(trim_history(&mut messages), 0);
        assert_eq!(
            format!("{messages:?}"),
            format!("{before:?}"),
            "nothing changed"
        );
    }

    /// The footer answers "should I start a new chat?", so its token figures are
    /// the conversation's running total — while the context reading tracks what
    /// the window holds *now* and does not accumulate.
    #[tokio::test]
    async fn usage_accumulates_across_turns_but_context_does_not() {
        struct Fixed;
        #[async_trait::async_trait]
        impl red_ai::AiProvider for Fixed {
            async fn stream_turn(
                &self,
                _req: &red_ai::TurnRequest<'_>,
                _tx: &tokio::sync::mpsc::UnboundedSender<red_ai::Delta>,
                _cancel: &CancelToken,
            ) -> red_ai::Result<red_ai::TurnOutcome> {
                Ok(red_ai::TurnOutcome {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text { text: "ok".into() }],
                    },
                    stop_reason: StopReason::EndTurn,
                    usage: red_ai::Usage {
                        input_tokens: 1_000,
                        output_tokens: 200,
                        cache_read_input_tokens: 9_000,
                    },
                })
            }
        }

        let (db, driver) = write_fixture("cumul");
        let state = Arc::new(Mutex::new(AiState::default()));
        let mut settled = Vec::new();
        for _ in 0..2 {
            let (tx, mut rx) = futures::channel::mpsc::unbounded();
            run_turn(
                Arc::new(Fixed),
                AiBackend::Sql {
                    driver: driver.clone(),
                    dialect: Dialect::Sqlite,
                },
                tx,
                state.clone(),
                None,
                ConversationId::new(1),
                red_ai::MODEL_OPUS.into(),
                false,
                AiPolicy::default(),
                "hello".into(),
                Vec::new(),
                AiContext::default(),
                false,
                CancelToken::new(),
            )
            .await;
            while let Ok((_, e)) = rx.try_recv() {
                if let Event::AiTurnFinished { usage, .. } = e {
                    settled.push(usage);
                }
            }
        }

        assert_eq!(settled.len(), 2);
        assert_eq!(settled[0].output_tokens, 200);
        assert_eq!(settled[1].output_tokens, 400, "spend accumulates");
        assert_eq!(settled[1].input_tokens, 2_000);
        assert_eq!(settled[1].cache_read_input_tokens, 18_000);
        // What the window holds is the last prompt's size, not a running sum:
        // uncached input plus what was read from cache.
        assert_eq!(settled[0].context_used_tokens, 10_000);
        assert_eq!(settled[1].context_used_tokens, 10_000);
        // And the window itself comes from the model id.
        assert_eq!(
            settled[1].context_tokens,
            red_ai::context_window(red_ai::MODEL_OPUS).unwrap()
        );
        let _ = std::fs::remove_file(&db);
    }

    /// The reply ceiling is the user's setting, and a subagent gets half of it.
    #[tokio::test]
    async fn the_output_ceiling_reaches_the_provider() {
        /// Captures the `max_tokens` of every request it is handed.
        struct Recorder(Arc<Mutex<Vec<u32>>>);
        #[async_trait::async_trait]
        impl red_ai::AiProvider for Recorder {
            async fn stream_turn(
                &self,
                req: &red_ai::TurnRequest<'_>,
                _tx: &tokio::sync::mpsc::UnboundedSender<red_ai::Delta>,
                _cancel: &CancelToken,
            ) -> red_ai::Result<red_ai::TurnOutcome> {
                self.0.lock().unwrap().push(req.max_tokens);
                Ok(red_ai::TurnOutcome {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text { text: "ok".into() }],
                    },
                    stop_reason: StopReason::EndTurn,
                    usage: red_ai::Usage::default(),
                })
            }
        }
        let (db, driver) = write_fixture("ceilset");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (tx, _rx) = futures::channel::mpsc::unbounded();
        let policy = AiPolicy {
            limits: red_core::AiLimits {
                max_output_tokens: 40_000,
                ..red_core::AiLimits::default()
            },
            ..AiPolicy::default()
        };
        run_turn(
            Arc::new(Recorder(seen.clone())),
            AiBackend::Sql {
                driver,
                dialect: Dialect::Sqlite,
            },
            tx,
            Arc::new(Mutex::new(AiState::default())),
            None,
            ConversationId::new(1),
            "m".into(),
            false,
            policy,
            "hello".into(),
            Vec::new(),
            AiContext::default(),
            false,
            CancelToken::new(),
        )
        .await;
        assert_eq!(seen.lock().unwrap().as_slice(), [40_000]);
        let _ = std::fs::remove_file(&db);
    }
}
