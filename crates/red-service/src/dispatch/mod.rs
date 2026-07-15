//! The dispatch loop: the backend thread's command pump. Owns the active
//! session and cursor, the open-result map, and the page-fetch concurrency
//! limit; runs queries through a windowed cursor and races each fetch against
//! incoming commands so a cancel or timeout can abort one in flight.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use red_core::kv::{KvEdit, RecycledKey};
use red_core::{
    Column, ColumnMeta, KeyKind, KeySpec, QueryOptions, RedError, ResultFilter, Value,
    coerce_edit_value,
};
use red_driver::{AbortSignal, ImportReader, PageCap};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedReceiver as CmdReceiver;

use crate::{Command, Envelope, Event, OpId, RunFetch, SessionId};

mod connect;
mod paging;
mod session;

// The dispatch loop's command arms reference these by their bare names; glob
// re-import keeps the (large) `dispatch` match body unchanged after the split
// into submodules. Each submodule owns one concern: `paging` the windowed
// fetch + checkpoint path, `session` the keep-alive session state + lifecycle,
// `connect` the off-loop dial.
use connect::*;
use paging::*;
use session::*;

/// The event sender carries each event tagged with the session it belongs to
/// (`None` for the session-less probe replies).
pub(crate) type Events = UnboundedSender<(Option<SessionId>, Event)>;

/// Cap on page fetches running at once. The grid can request a burst of pages
/// (several tabs, or a viewport spanning page boundaries); without a cap a flung
/// scrollbar could otherwise fan out dozens of simultaneous deep-`OFFSET` scans
/// and saturate the server. The UI also throttles requests (see `FLING_ROWS`);
/// this is the backstop.
const MAX_CONCURRENT_PAGE_FETCHES: usize = 6;

/// How many exports may stream at once across all sessions. Each holds a driver
/// connection for the file's lifetime, so this bounds connection pinning. Generous,
/// since exports are user-initiated (one per toast), but no longer unbounded.
const MAX_CONCURRENT_EXPORTS: usize = 4;

/// How many imports may stream at once across all sessions. Writes are heavier than
/// reads (and hold a connection in a transaction), so this is tighter than exports.
const MAX_CONCURRENT_IMPORTS: usize = 2;

/// How many table copies may stream at once across all sessions. A copy pins a
/// connection on *each* end (source read + target write) for its whole lifetime, so
/// this is kept as tight as imports: a couple of millions-of-rows transfers can run
/// together without fanning out an unbounded number of pinned connections.
const MAX_CONCURRENT_COPIES: usize = 2;

/// Rows per source window / insert chunk in a table copy (the driver re-clamps the
/// insert to its bound-parameter cap). Keeps the copy one-chunk-resident regardless
/// of how many rows move; a `[copy]` knob is a later refinement, like import's.
const COPY_CHUNK_ROWS: usize = 500;

/// Hard ceiling on rows pulled by one `CopyRows` (clipboard) request. `CopyRows`
/// fetches at full fidelity into a single `Vec` carried in one event, so a
/// "select all" over a 50M-row result would otherwise spike memory and the event
/// queue. A million rows is far more than any clipboard usefully holds; beyond it
/// the copy is capped (and the cap logged) rather than letting the backend balloon.
const MAX_COPY_ROWS: usize = 1_000_000;

/// How often the dispatch loop wakes (absent any command) to sweep idle sessions.
const EVICT_SWEEP: Duration = Duration::from_secs(30);

/// One configured AI agent in the dispatch registry, built once per `ConfigureAi`
/// from an [`AiAgentProfile`](crate::protocol::AiAgentProfile). An `Api` agent
/// holds its pre-built provider (`None` when it has no key; a turn then reports
/// "not configured") and resolved model; an `Acp` agent holds its resolved launch
/// command. A turn names an id, the loop looks it up here and routes accordingly.
enum AiProfileRuntime {
    Api {
        provider: Option<Arc<dyn red_ai::AiProvider>>,
        model: String,
    },
    Acp {
        command: String,
    },
}

/// Lock a mutex, tolerating poison. A detached page-fetch task can panic while
/// holding `results`; recovering the guard keeps one bad task from bricking the
/// whole backend. The worst case is a half-written entry, which dispatch already
/// tolerates: a fetch for an epoch absent or stale in the map is dropped.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) async fn dispatch(mut commands: CmdReceiver<Envelope>, events: Events) {
    // The warm sessions, keyed by `SessionId`. Several stay live at once so the UI
    // can switch between connections instantly (no reconnect); each owns its
    // driver, cursor, open-result map, in-flight handles, and exports. `Connect`
    // inserts, `Disconnect`/`CloseSession`/eviction remove. Per-epoch fetch state
    // lives inside each session; UI epochs start at 1, so an empty result map
    // means "no live result" for that session.
    let mut sessions: HashMap<SessionId, SessionState> = HashMap::new();
    // Which session the UI currently shows (`SetActiveSession`). Exempt from idle
    // eviction so an on-screen-but-unscrolled result stays warm.
    let mut foreground: Option<SessionId> = None;
    // The statement timeout (`query.statement_timeout`) applied to every open
    // probe and page/run fetch. `None` = no cap. Global, set by the UI at launch
    // and on each settings reload, captured into each spawned fetch task.
    let mut statement_timeout: Option<Duration> = None;
    // Bounds how many page fetches hit servers concurrently across *all* sessions
    // (see the const), a shared backstop, so a flung scrollbar on one connection
    // can't fan out dozens of deep scans. A busy session can briefly delay
    // another's page fetches; acceptable for a backstop.
    let page_fetch_limit = Arc::new(Semaphore::new(MAX_CONCURRENT_PAGE_FETCHES));
    // Bounds concurrent exports across *all* sessions. Each export holds a driver
    // connection streaming for the file's whole lifetime, so without a cap a user
    // firing many large exports could pin an unbounded number of connections. A
    // separate pool from the page-fetch limit: a long export must not starve
    // interactive paging, nor the reverse.
    let export_limit = Arc::new(Semaphore::new(MAX_CONCURRENT_EXPORTS));
    let import_limit = Arc::new(Semaphore::new(MAX_CONCURRENT_IMPORTS));
    let copy_limit = Arc::new(Semaphore::new(MAX_CONCURRENT_COPIES));
    // Wakes the loop even when no command arrives, so idle sessions get swept.
    let mut sweep = tokio::time::interval(EVICT_SWEEP);
    // `Connect`/`TestConnection` dial off the loop (a slow connect mustn't freeze
    // other sessions) and report back over this channel; the loop applies the
    // result. `connect_gen` tags each spawned connect so a superseded one is
    // dropped instead of clobbering a newer session on the same id.
    let (connect_tx, mut connect_rx) = tokio::sync::mpsc::unbounded_channel::<ConnectOutcome>();
    let mut connect_gen: HashMap<SessionId, u64> = HashMap::new();

    // The self-updater runs as its own task on this runtime (off this loop, so a
    // download never stalls query dispatch). We forward its two global commands
    // over a control channel; it emits `UpdateState` straight through the cloned
    // event sink.
    let updater = {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(crate::update::run(events.clone(), rx));
        tx
    };

    // The AI assistant's configured agents (built from `ConfigureAi.agents`), keyed
    // by id: an API agent carries its pre-built provider (None until a key is set)
    // and model; an ACP agent carries its resolved launch command. *Which* agent a
    // turn uses is decided per-turn by `AiTurn.agent` (M-S6), so several
    // conversations on different agents run concurrently. A turn runs as a spawned
    // task off this loop (like exports), sharing `ai_state` for its conversation
    // history and cancel registry.
    let mut ai_agents: HashMap<String, AiProfileRuntime> = HashMap::new();
    let mut ai_default_agent = String::new();
    let mut ai_show_thinking = false;
    // The global AI access policy (M-S7): master switch, access tier, and resource
    // guards, set by `ConfigureAi`. A turn layers the session's per-connection
    // overrides over this and enforces the result in the shared tool layer, so it
    // covers both backends and the agent can't bypass it.
    let mut ai_policy = red_core::AiPolicy::default();
    let ai_state = Arc::new(Mutex::new(crate::ai::AiState::default()));
    // The subscription (ACP) path keeps one live agent conversation per
    // `conversation_id`; the tokio Mutex lets a slow agent start await off-loop.
    let ai_acp = Arc::new(tokio::sync::Mutex::new(crate::acp::AcpManager::default()));

    loop {
        let (session_id, command) = tokio::select! {
            maybe = commands.recv() => match maybe {
                Some(envelope) => envelope,
                None => break, // UI dropped the sender (window closed)
            },
            _ = sweep.tick() => {
                evict_idle(&mut sessions, foreground, &events);
                // Reclaim long-idle subscription agents too (M-S3). Off the loop,
                // like the other ACP calls, since the manager is behind a tokio
                // Mutex a slow start may be holding.
                let manager = ai_acp.clone();
                tokio::spawn(async move { manager.lock().await.evict_idle() });
                continue;
            }
            outcome = connect_rx.recv() => {
                // The sender is held for the loop's lifetime, so `recv` only
                // resolves with a real outcome (never `None`).
                if let Some(outcome) = outcome {
                    apply_connect_outcome(outcome, &mut sessions, &connect_gen, &events, &ai_acp);
                }
                continue;
            }
        };
        // Any command routed to a session counts as activity, deferring eviction.
        if let Some(id) = session_id
            && let Some(s) = sessions.get_mut(&id)
        {
            s.last_used = Instant::now();
        }
        match command {
            Command::Connect(config) => {
                let Some(id) = session_id else { continue };
                // A re-connect on the same id (a retry, or replacing a dropped
                // session) tears down whatever was there first.
                if let Some(mut old) = sessions.remove(&id) {
                    old.teardown();
                    // The new driver replaces the old one, so any subscription
                    // agent bound to the old session must go too (M-S3); the next
                    // turn lazily rebinds a fresh agent to the new driver.
                    let manager = ai_acp.clone();
                    tokio::spawn(async move { manager.lock().await.evict_session(Some(id)) });
                }
                // Dial off the loop so a hung connect doesn't wedge dispatch; the
                // result comes back over `connect_rx`. Bump the generation so a
                // slower earlier attempt on this id is discarded when it lands.
                let generation = connect_gen.entry(id).or_default();
                *generation += 1;
                let generation = *generation;
                // Capture the connection's AI overrides before `config` moves into
                // the dial task, so the resulting session carries them (M-S7).
                let ai_override = AiOverride {
                    enabled: config.ai_enabled,
                    tier: config.ai_tier,
                };
                // The connection's read-only posture, captured before `config` moves
                // into the dial task, so the session can gate the AI write tool.
                let read_only = config.read_only;
                let tx = connect_tx.clone();
                tokio::spawn(async move {
                    let result = attempt_connect(&config).await;
                    let _ = tx.send(ConnectOutcome::Session {
                        id,
                        generation,
                        ai_override,
                        read_only,
                        result,
                    });
                });
            }

            Command::SetActiveSession(id) => foreground = id,

            Command::SetStatementTimeout(timeout) => statement_timeout = timeout,

            Command::SetDisplayCellCap(bytes) => red_driver::set_display_cell_cap(bytes),

            Command::ConfigureUpdates(config) => {
                let _ = updater.send(crate::update::UpdateControl::Configure(config));
            }

            Command::CheckForUpdate => {
                let _ = updater.send(crate::update::UpdateControl::CheckNow);
            }

            Command::ConfigureAi(cfg) => {
                ai_show_thinking = cfg.show_thinking;
                ai_policy = red_core::AiPolicy {
                    enabled: cfg.enabled,
                    tier: cfg.tier,
                    limits: cfg.limits,
                    // The global default is writable-posture; each turn overrides
                    // this with the connection's authoritative read-only flag.
                    read_only: false,
                };
                ai_default_agent = cfg.default_agent;
                // Build each configured agent's runtime. An API agent with an empty
                // key gets a `None` provider; a turn on it replies with a clear
                // AiError rather than a failed network call; an ACP agent needs no
                // key (it owns its own auth). A custom `base_url` retargets the
                // Anthropic-wire provider (e.g. a local endpoint).
                ai_agents = cfg
                    .agents
                    .into_iter()
                    .map(|a| {
                        let runtime = match a.kind {
                            crate::protocol::AiAgentKind::Api => {
                                let model = if a.model.is_empty() {
                                    red_ai::MODEL_OPUS.to_string()
                                } else {
                                    a.model
                                };
                                let provider = if a.api_key.is_empty() {
                                    None
                                } else {
                                    let mut p = red_ai::AnthropicProvider::new(a.api_key);
                                    if !a.base_url.is_empty() {
                                        // A custom endpoint is fine, but never send the
                                        // API key to an arbitrary cleartext host: only
                                        // HTTPS (or loopback http). Reject and keep the
                                        // default rather than exfiltrate the credential.
                                        if red_ai::is_safe_base_url(&a.base_url) {
                                            p = p.with_base_url(a.base_url);
                                        } else {
                                            tracing::warn!(
                                                "ignoring AI agent base_url {:?}: only https \
                                                 (or localhost http) may receive the API key",
                                                a.base_url
                                            );
                                        }
                                    }
                                    Some(Arc::new(p) as Arc<dyn red_ai::AiProvider>)
                                };
                                AiProfileRuntime::Api { provider, model }
                            }
                            crate::protocol::AiAgentKind::Acp => {
                                let command = if a.command.is_empty() {
                                    crate::DEFAULT_AGENT_COMMAND.to_string()
                                } else {
                                    a.command
                                };
                                AiProfileRuntime::Acp { command }
                            }
                        };
                        (a.id, runtime)
                    })
                    .collect();
            }

            Command::AiTurn {
                conversation_id,
                agent,
                message,
                context,
            } => {
                // The turn grounds in the connected session's driver, either the
                // SQL `DatabaseDriver` or the Redis `KvDriver` seam (each has its
                // own tool catalog; see docs/plans/redis-workflow-parity.md Part 1).
                let session_driver = session_id
                    .and_then(|id| sessions.get(&id))
                    .map(|s| s.driver.clone());
                let Some(session_driver) = session_driver else {
                    emit(
                        &events,
                        session_id,
                        Event::AiError {
                            conversation_id,
                            message: "not connected".into(),
                        },
                    );
                    continue;
                };

                // Resolve the effective AI policy (M-S7): the session's per-connection
                // overrides layered over the global one. The master switch is checked
                // here, before anything spawns; a disabled assistant starts no MCP
                // server and no agent process, it just reports the refusal.
                let ai_override = session_id
                    .and_then(|id| sessions.get(&id))
                    .map(|s| s.ai_override)
                    .unwrap_or_default();
                // The connection's authoritative read-only posture gates the write
                // tool (defense in depth alongside the driver's own rejection).
                let read_only = session_id
                    .and_then(|id| sessions.get(&id))
                    .map(|s| s.read_only)
                    .unwrap_or(false);
                let mut effective = ai_policy.with_overrides(ai_override.enabled, ai_override.tier);
                effective.read_only = read_only;
                if !effective.enabled {
                    emit(
                        &events,
                        session_id,
                        Event::AiError {
                            conversation_id,
                            message: "the AI agent is disabled for this connection".into(),
                        },
                    );
                    continue;
                }

                // Resolve which agent this turn runs on: the named id, or the default
                // when empty. An id that names no configured agent (e.g. a saved
                // chat bound to a profile the user has since deleted) fails with a
                // clear error rather than silently running a different backend.
                let agent_id = if agent.trim().is_empty() {
                    ai_default_agent.clone()
                } else {
                    agent
                };
                let Some(runtime) = ai_agents.get(&agent_id) else {
                    emit(
                        &events,
                        session_id,
                        Event::AiError {
                            conversation_id,
                            message: format!(
                                "AI agent '{agent_id}' is not configured; pick another in the \
                                 panel, or add it in Settings."
                            ),
                        },
                    );
                    continue;
                };

                match runtime {
                    AiProfileRuntime::Api { provider, model } => {
                        let Some(provider) = provider.clone() else {
                            emit(
                                &events,
                                session_id,
                                Event::AiError {
                                    conversation_id,
                                    message:
                                        "AI agent is not configured; add an API key in Settings."
                                            .into(),
                                },
                            );
                            continue;
                        };
                        let model = model.clone();
                        // Ground in whichever seam the session holds.
                        let backend = match &session_driver {
                            session::SessionDriver::Sql(d) => crate::ai::AiBackend::Sql(d.clone()),
                            session::SessionDriver::Kv(d) => crate::ai::AiBackend::Kv(d.clone()),
                        };
                        let cancel = red_ai::CancelToken::new();
                        lock(&ai_state).register(conversation_id, cancel.clone());
                        tokio::spawn(crate::ai::run_turn(
                            provider,
                            backend,
                            events.clone(),
                            ai_state.clone(),
                            session_id,
                            conversation_id,
                            model,
                            ai_show_thinking,
                            effective,
                            message,
                            context,
                            cancel,
                        ));
                    }
                    AiProfileRuntime::Acp { command } => {
                        // The external ACP agent grounds through Red's loopback MCP
                        // server, which hosts whichever seam this session holds (SQL
                        // schema/query tools or the Redis `kv_*` tools).
                        let backend = match &session_driver {
                            session::SessionDriver::Sql(d) => crate::ai::AiBackend::Sql(d.clone()),
                            session::SessionDriver::Kv(d) => crate::ai::AiBackend::Kv(d.clone()),
                        };
                        let command = command.clone();
                        // The agent loads its own config (and login) from cwd; use
                        // the process working directory.
                        let cwd = std::env::current_dir()
                            .unwrap_or_else(|_| std::path::PathBuf::from("/"));
                        tokio::spawn(crate::acp::run_turn(
                            ai_acp.clone(),
                            backend,
                            command,
                            cwd,
                            events.clone(),
                            session_id,
                            conversation_id,
                            effective,
                            message,
                            context,
                        ));
                    }
                }
            }

            Command::AiCancel { conversation_id } => {
                lock(&ai_state).cancel(conversation_id);
                let manager = ai_acp.clone();
                tokio::spawn(async move { manager.lock().await.cancel(conversation_id) });
            }

            Command::AiForget { conversation_id } => {
                // The conversation was closed/deleted in the UI, so drop its backend
                // state on both paths so the maps stay bounded. The API-key forget is
                // a quick sync lock; the ACP one awaits, so it runs off the loop.
                lock(&ai_state).forget(conversation_id);
                let manager = ai_acp.clone();
                tokio::spawn(async move { manager.lock().await.forget(conversation_id) });
            }

            Command::AiPermission {
                conversation_id: _,
                request_id,
                allow,
            } => {
                // Answer a parked permission prompt. It belongs to exactly one
                // backend: the subscription path's ACP manager (M-S2 tool prompts) or
                // the API-key path's AiState (Feature B write prompts). Their request-
                // id spaces are disjoint (AiState offsets its ids), so resolving both
                // is safe: only the owning side has the id. The API-key resolve is a
                // quick sync lock; the ACP one awaits, so it runs off the loop.
                lock(&ai_state).resolve_permission(request_id, allow);
                let manager = ai_acp.clone();
                tokio::spawn(
                    async move { manager.lock().await.resolve_permission(request_id, allow) },
                );
            }

            Command::AiReauthenticateAgent { agent_id } => {
                // Start an interactive sign-in from Settings: only meaningful for an
                // ACP agent. The relay drives the agent CLI's paste-code flow and
                // emits `AiLoginPrompt`/`AiLoginFinished`. Off the loop like the
                // other ACP calls. Sign-in is account-global, not cwd-dependent.
                if let Some(AiProfileRuntime::Acp { command }) = ai_agents.get(&agent_id) {
                    let command = command.clone();
                    tokio::spawn(crate::acp::start_login(
                        ai_acp.clone(),
                        command,
                        agent_id,
                        events.clone(),
                    ));
                }
            }

            Command::AiSubmitLoginCode { agent_id, code } => {
                // Deliver the pasted OAuth code to the in-flight sign-in. Off the
                // loop; taking the manager lock awaits.
                let manager = ai_acp.clone();
                tokio::spawn(
                    async move { manager.lock().await.submit_login_code(&agent_id, code) },
                );
            }

            Command::AiCancelLogin { agent_id } => {
                // Abandon an in-flight sign-in (kills the CLI). Off the loop.
                let manager = ai_acp.clone();
                tokio::spawn(async move { manager.lock().await.cancel_login(&agent_id) });
            }

            Command::AiSignOutAgent { agent_id } => {
                if let Some(AiProfileRuntime::Acp { command }) = ai_agents.get(&agent_id) {
                    let command = command.clone();
                    tokio::spawn(crate::acp::sign_out(
                        ai_acp.clone(),
                        command,
                        agent_id,
                        events.clone(),
                    ));
                }
            }

            Command::AiCheckAuthStatus { agent_id } => {
                if let Some(AiProfileRuntime::Acp { command }) = ai_agents.get(&agent_id) {
                    let command = command.clone();
                    tokio::spawn(crate::acp::check_auth_status(
                        command,
                        agent_id,
                        events.clone(),
                    ));
                }
            }

            Command::AiSetConfigOption {
                conversation_id,
                config_id,
                value,
            } => {
                // Change a model / reasoning selector on the subscription path. Off
                // the loop; it awaits the agent's reply, then emits the refreshed set.
                tokio::spawn(crate::acp::set_config_option(
                    ai_acp.clone(),
                    events.clone(),
                    session_id,
                    conversation_id,
                    config_id,
                    value,
                ));
            }

            Command::TestConnection(config) => {
                // A throwaway probe: connect, report, and let the driver drop. No
                // session is created or disturbed; it's session-less (`None`).
                // Spawned off the loop like `Connect`, so probing a dead host
                // doesn't stall every warm session.
                let tx = connect_tx.clone();
                tokio::spawn(async move {
                    // The Test reply only reports a message; fatality only matters
                    // for the retry loop, which a probe doesn't have.
                    let result = attempt_connect(&config)
                        .await
                        // The probe drops the driver (and any tunnel) right after
                        // reading the version (it's throwaway).
                        .map(|(driver, _tunnel)| driver.server_version())
                        .map_err(|f| f.message);
                    let _ = tx.send(ConnectOutcome::Test { result });
                });
            }

            Command::TrustSshHost { host, port, key } => {
                // Append the host key to ~/.ssh/known_hosts, on the loop (a quick
                // file write). The UI re-sends `Connect` right after; processed in
                // order on this single loop, so the retry sees the new entry. A
                // failure is logged; the retry will just re-prompt.
                if let Err(e) = crate::tunnel::trust_host(&host, port, &key) {
                    tracing::warn!("failed to trust SSH host {host}: {e}");
                }
            }

            Command::Disconnect | Command::CloseSession => {
                let Some(id) = session_id else { continue };
                if let Some(mut state) = sessions.remove(&id) {
                    state.teardown();
                }
                // Tear down any subscription agent grounded in this session: its
                // MCP server holds a now-dead driver clone (M-S3).
                let manager = ai_acp.clone();
                tokio::spawn(async move { manager.lock().await.evict_session(Some(id)) });
                // Invalidate any in-flight connect for this id so its late outcome
                // can't resurrect the session the user just closed.
                if let Some(g) = connect_gen.get_mut(&id) {
                    *g += 1;
                }
                if foreground == session_id {
                    foreground = None;
                }
                emit(&events, session_id, Event::Disconnected);
            }

            Command::Query { sql, opts } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                state.active = None; // a new query supersedes the previous cursor
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                match driver.open_cursor(&sql, opts.clone()).await {
                    Ok(cursor) => {
                        let aq = ActiveQuery {
                            cancel: cursor.cancel_token(),
                            timeout: opts.timeout,
                            streamed: 0,
                            started: Instant::now(),
                            cursor,
                        };
                        emit(
                            &events,
                            session_id,
                            Event::QueryStarted {
                                columns: aq.cursor.columns().to_vec(),
                            },
                        );
                        // Re-borrow the session's cursor slot (it can't vanish
                        // mid-await on this single-threaded loop).
                        if let Some(active) = sessions.get_mut(&id).map(|s| &mut s.active)
                            && drive_fetch(aq, opts.window, id, &mut commands, &events, active)
                                .await
                        {
                            break; // shutdown requested mid-fetch
                        }
                    }
                    Err(err) => emit(&events, session_id, Event::Error(err.to_string())),
                }
            }

            Command::FetchMore { max } => {
                let Some(id) = session_id else { continue };
                let aq = sessions.get_mut(&id).and_then(|s| s.active.take());
                match aq {
                    Some(aq) => {
                        if let Some(active) = sessions.get_mut(&id).map(|s| &mut s.active)
                            && drive_fetch(aq, max, id, &mut commands, &events, active).await
                        {
                            break;
                        }
                    }
                    None => emit(&events, session_id, Event::Error("no active query".into())),
                }
            }

            Command::LoadObjects => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                match driver.list_objects().await {
                    Ok(schemas) => emit(&events, session_id, Event::ObjectsLoaded { schemas }),
                    Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                }
            }

            Command::LoadForeignKeys => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get(&id) else {
                    continue;
                };
                // Swallow errors: FK navigation is optional, so a failed or
                // unsupported introspection (including a KV session, which has
                // no SQL driver at all) leaves the graph empty rather than
                // toasting the user on every connect.
                let Some(driver) = state.driver.as_sql().cloned() else {
                    continue;
                };
                if let Ok(graph) = driver.foreign_keys().await {
                    emit(&events, session_id, Event::ForeignKeysLoaded { graph });
                }
            }

            Command::LoadEnums { table } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get(&id) else {
                    continue;
                };
                // Optional, like the FK graph: a failed/unsupported enum lookup
                // (including a KV session) just leaves the picker without enum
                // suggestions rather than toasting.
                let Some(driver) = state.driver.as_sql().cloned() else {
                    continue;
                };
                if let Ok(columns) = driver.enum_columns(&table).await {
                    emit(&events, session_id, Event::EnumsLoaded { table, columns });
                }
            }

            Command::DescribeTable { schema, table } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                match driver.describe_table(&schema, &table).await {
                    Ok(detail) => emit(
                        &events,
                        session_id,
                        Event::TableDescribed {
                            schema,
                            table,
                            detail,
                        },
                    ),
                    Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                }
            }

            Command::OpenResult {
                sql,
                epoch,
                table,
                sort,
                filter,
                joins,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                // A re-open on the same epoch supersedes any prior probe.
                if let Some(f) = state.inflight.remove(&epoch) {
                    f.abort_all();
                }
                // Registered before the (slow) open task so an early fetch for
                // this epoch isn't mistaken for a stale one.
                lock(&state.results).insert(
                    epoch,
                    OpenSpec {
                        sql: sql.clone(),
                        key: None,
                        key_cols: Vec::new(),
                        bounds: None,
                        total: None,
                        checkpoints: Arc::new(Mutex::new(CheckpointIndex::default())),
                    },
                );
                // Backstop GC: bound the open-result map against any future UI path
                // that opens without closing its predecessor (epochs are monotonic,
                // so this only ever reaps genuinely-leaked older results).
                state.reap_excess_results(epoch);
                // One abort handle for the whole probe bundle: re-sort / close
                // cancels the (potentially full-table) `count` and column probe.
                let abort = AbortSignal::new();
                state.inflight.entry(epoch).or_default().open = Some(abort.clone());
                // Count + column metadata can be slow (a full `COUNT(*)` over a
                // large table); run them off the dispatch loop so switching to
                // another table stays instant.
                let events = events.clone();
                let results = state.results.clone();
                let timeout = statement_timeout;
                tokio::spawn(async move {
                    // A table browse resolves its seek key from the table's
                    // introspected detail: a sorted browse gets the composite
                    // `(sort_col, pk)` key, an unsorted one the plain PK. A
                    // resolution failure just means the `OFFSET` fallback (never
                    // an error). The detail is kept around; a `Contains` filter
                    // searches the table's columns.
                    let detail = match &table {
                        Some((schema, table)) => match driver.describe_table(schema, table).await {
                            Ok(detail) => Some(detail),
                            Err(e) => {
                                tracing::warn!(%schema, %table, "keyset describe failed: {e}");
                                None
                            }
                        },
                        None => None,
                    };
                    let key = match &detail {
                        Some(detail) => {
                            let key = match &sort {
                                Some(s) => KeySpec::sorted(detail, &s.column, s.direction),
                                None => KeySpec::from_detail(detail),
                            };
                            match &key {
                                Some(k) => tracing::info!(
                                    column = %k.column, tiebreak = ?k.tiebreak,
                                    direction = ?k.direction, "keyset key resolved"
                                ),
                                None => tracing::info!(
                                    "no usable key (composite/nullable/no PK); OFFSET paging"
                                ),
                            }
                            key
                        }
                        None => None,
                    };
                    // Inline FK expansion (Track B7): decorate the base with the chosen
                    // referenced columns, interleaved next to the FK column they expand
                    // from (the base column order comes from `detail`). The join runs
                    // *before* the filter so a `WHERE` can reference the expanded
                    // (dotted-alias) columns, not just base columns; the unique-target
                    // gate keeps the row count identical, so the join is transparent to
                    // keyset. Empty `joins` (or a no-FK engine) leaves `sql` untouched.
                    let base_cols: Vec<String> = detail
                        .as_ref()
                        .map(|d| d.columns.iter().map(|c| c.name.clone()).collect())
                        .unwrap_or_default();
                    let joined_sql = driver.fk_join_wrap(&sql, &base_cols, &joins);
                    // Build the filter predicate, then wrap it *around* the joined query
                    // (`SELECT * FROM (joined) WHERE <pred>`) so count, bounds, and
                    // paging all see the filtered set, and a `Where`/`Eq` predicate can
                    // name any output column, including an expanded reference column
                    // (`"tier_id.name"`). The wrap keeps `SELECT *`, so the key column
                    // survives and keyset is unaffected. A `Contains` searches the base
                    // table's columns (or, for editor SQL, a cheap `LIMIT 0` probe).
                    let pred: Option<String> = match &filter {
                        None => None,
                        Some(ResultFilter::Where(expr)) => Some(expr.clone()),
                        // FK follow (Track B7): an escaped literal `col = v [AND …]`
                        // predicate from the driver. Empty pairs (shouldn't occur)
                        // degrade to no filter rather than an invalid `WHERE ()`.
                        Some(ResultFilter::Eq(pairs)) if !pairs.is_empty() => {
                            Some(driver.eq_predicate(pairs))
                        }
                        Some(ResultFilter::Eq(_)) => None,
                        Some(ResultFilter::Contains(term)) => {
                            let cols = match &detail {
                                Some(d) => d.columns.clone(),
                                None => match driver
                                    .fetch_page(&sql, 0, 0, PageCap::Full, &abort)
                                    .await
                                {
                                    Ok(p) => p.columns.iter().map(col_meta_from_result).collect(),
                                    Err(_) => Vec::new(),
                                },
                            };
                            driver.contains_predicate(&cols, term)
                        }
                    };
                    let filtered_sql = match &pred {
                        Some(p) => wrap_where(&joined_sql, p),
                        None => joined_sql.clone(),
                    };
                    // Count / bounds narrow with the filter; with none, they're
                    // cardinality-identical to the unjoined base (the join is
                    // unique-target), so a bare count skips the join.
                    let probe_sql = if pred.is_some() {
                        filtered_sql.clone()
                    } else {
                        sql.clone()
                    };
                    // The SQL later page/run fetches re-run. Keyset orders itself
                    // (driver adds `ORDER BY (sort_col, pk)`), so it pages the
                    // *filtered* query; a sorted result that fell back to OFFSET must
                    // still be ordered, so wrap it by output position.
                    let effective_sql = match (&sort, &key) {
                        (Some(s), None) => wrap_sorted(&filtered_sql, s.position, s.direction),
                        _ => filtered_sql.clone(),
                    };
                    // `LIMIT 0` reads column metadata without stepping rows;
                    // counting and the key-bounds probe run concurrently with it.
                    // Count / bounds run on `probe_sql` (the unjoined base when there's
                    // no filter, else the joined+filtered query), so the total and
                    // bounds reflect the filter; ordering never changes either.
                    let bounds = async {
                        match &key {
                            Some(k) if k.kind == KeyKind::Int => driver
                                .key_bounds(&probe_sql, k, &abort)
                                .await
                                .ok()
                                .flatten(),
                            _ => None,
                        }
                    };
                    // Race the (potentially full-table `COUNT(*)`) probe against the
                    // statement timeout: on expiry, abort the bundle at the engine
                    // and report a timeout instead of leaving the result "running".
                    // Columns come from the *joined* SQL so the reported column set
                    // includes the expanded reference columns even with no filter.
                    let probe = async {
                        tokio::join!(
                            driver.count(&probe_sql, &abort),
                            driver.fetch_page(&joined_sql, 0, 0, PageCap::Full, &abort),
                            bounds
                        )
                    };
                    let (total, columns, bounds) = tokio::select! {
                        out = probe => out,
                        _ = sleep_for(timeout), if timeout.is_some() => {
                            abort.abort();
                            emit(&events, session_id, Event::Error(RedError::Timeout.to_string()));
                            return;
                        }
                    };
                    match (total, columns) {
                        (Ok(total), Ok(page)) => {
                            let total = total.max(0) as usize;
                            // Fill the spec in only if the result is still open.
                            // `key_cols` locate the key columns within a row so the
                            // checkpoint build can read each checkpoint's key tuple.
                            let key_cols = key
                                .as_ref()
                                .map(|k| {
                                    k.column_names()
                                        .iter()
                                        .filter_map(|name| {
                                            page.columns.iter().position(|c| &c.name == name)
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            if let Some(spec) = lock(&results).get_mut(&epoch) {
                                spec.sql = effective_sql;
                                spec.key = key.clone();
                                spec.key_cols = key_cols;
                                spec.bounds = bounds;
                                spec.total = Some(total);
                            }
                            emit(
                                &events,
                                session_id,
                                Event::ResultReady {
                                    columns: page.columns,
                                    total,
                                    epoch,
                                    key,
                                },
                            );
                        }
                        (Err(e), _) | (_, Err(e)) => {
                            emit(&events, session_id, Event::Error(e.to_string()))
                        }
                    }
                });
            }

            Command::FetchPage {
                offset,
                limit,
                epoch,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                // The tab closed or re-sorted (its epoch is gone); skip the stale
                // request rather than running an expensive query whose result
                // would be discarded.
                let Some(sql) = lock(&state.results).get(&epoch).map(|s| s.sql.clone()) else {
                    continue;
                };
                // A newer page for this epoch supersedes the last one (the viewport
                // moved); cancel its in-flight fetch so a flung scrollbar doesn't
                // back a pile of doomed deep-`OFFSET` scans up behind the semaphore.
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.page.take() {
                    prev.abort();
                }
                let abort = AbortSignal::new();
                entry.page = Some(abort.clone());
                // Pages fetch concurrently (the driver pools connections) and off
                // the dispatch loop, so a deep-`OFFSET` page never blocks the next
                // command or another page, but no more than `page_fetch_limit` at
                // once, so a burst can't saturate the server.
                let events = events.clone();
                let limit_src = page_fetch_limit.clone();
                let timeout = statement_timeout;
                tokio::spawn(async move {
                    // A flung scrollbar supersedes pages faster than the semaphore
                    // drains; a page aborted before (or while) it waits for a permit
                    // bails without touching the engine, so doomed fetches don't pile
                    // up behind the limit or hit the server.
                    if abort.is_aborted() {
                        return;
                    }
                    let _permit = limit_src.acquire_owned().await;
                    if abort.is_aborted() {
                        return;
                    }
                    // Offset-mode display page: cap fat cells; no seek key to exempt.
                    let fetch = driver.fetch_page(
                        &sql,
                        offset,
                        limit,
                        PageCap::Display { key: None },
                        &abort,
                    );
                    match with_timeout(timeout, &abort, fetch).await {
                        Ok(page) => emit(
                            &events,
                            session_id,
                            Event::ResultPageLoaded {
                                offset,
                                rows: page.rows,
                                epoch,
                            },
                        ),
                        // Superseded mid-flight: a clean cancel, not an error toast.
                        Err(RedError::Interrupted) => {}
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::FetchRun {
                epoch,
                fetch,
                limit,
                seq,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                // Stale epoch (tab closed / re-sorted); drop, like `FetchPage`.
                let Some(spec) = lock(&state.results).get(&epoch).cloned() else {
                    continue;
                };
                let Some(key) = spec.key.clone() else {
                    continue; // a keyless result never gets `FetchRun`s
                };
                // A newer run (higher `seq`) supersedes the last one; a scrollbar
                // fling emits a stream of runs and only the latest matters. Cancel
                // the previous in-flight run so its seek stops at the engine. `seq`
                // is monotonic over the ordered command stream, so the guard against
                // a lower-seq arrival is belt-and-suspenders.
                let entry = state.inflight.entry(epoch).or_default();
                match entry.run.take() {
                    Some((prev_seq, prev)) if prev_seq >= seq => {
                        entry.run = Some((prev_seq, prev));
                        continue;
                    }
                    Some((_, prev)) => prev.abort(),
                    None => {}
                }
                let abort = AbortSignal::new();
                entry.run = Some((seq, abort.clone()));
                // A deep exact jump kicks off the checkpoint index (once) so the
                // *next* deep jump is O(stride). This one still serves via OFFSET.
                if let RunFetch::Jump {
                    ordinal,
                    exact: true,
                } = &fetch
                    && claim_build(&spec, *ordinal)
                {
                    let build_abort = AbortSignal::new();
                    state.inflight.entry(epoch).or_default().build = Some(build_abort.clone());
                    tokio::spawn(build_checkpoints(
                        driver.clone(),
                        spec.clone(),
                        state.results.clone(),
                        epoch,
                        build_abort,
                    ));
                }
                let events = events.clone();
                let limit_src = page_fetch_limit.clone();
                let timeout = statement_timeout;
                tokio::spawn(async move {
                    // Like `FetchPage`: a run superseded by a higher-`seq` fling bails
                    // before/after waiting for a permit so it neither queues behind the
                    // limit nor seeks at the engine.
                    if abort.is_aborted() {
                        return;
                    }
                    let _permit = limit_src.acquire_owned().await;
                    if abort.is_aborted() {
                        return;
                    }
                    let run = run_fetch(&*driver, &spec, &key, &fetch, limit, &abort);
                    match with_timeout(timeout, &abort, run).await {
                        Ok((rows, estimated)) => emit(
                            &events,
                            session_id,
                            Event::ResultRunLoaded {
                                epoch,
                                fetch,
                                rows,
                                estimated,
                                seq,
                            },
                        ),
                        // Superseded mid-flight: the newer run will deliver; stay
                        // silent rather than marking this seq failed or toasting.
                        Err(RedError::Interrupted) => {}
                        Err(e) => {
                            tracing::warn!(%epoch, ?fetch, "run fetch failed: {e}");
                            emit(&events, session_id, Event::ResultRunFailed { epoch, seq });
                            emit(&events, session_id, Event::Error(e.to_string()));
                        }
                    }
                });
            }

            Command::CopyRows {
                offset,
                limit,
                epoch,
                id,
            } => {
                let Some(sid) = session_id else { continue };
                let Some(state) = sessions.get(&sid) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                // Stale epoch (tab closed / re-sorted); drop, like `FetchPage`.
                let Some(sql) = lock(&state.results).get(&epoch).map(|s| s.sql.clone()) else {
                    continue;
                };
                // Same windowed read as a page fetch, but `Full` so the rows carry the
                // real values (the grid's display cap is bypassed) for the clipboard.
                // Bounded by `MAX_COPY_ROWS` so a select-all can't pull an unbounded
                // result into one Vec/event.
                let limit = if limit > MAX_COPY_ROWS {
                    tracing::warn!(
                        requested = limit,
                        cap = MAX_COPY_ROWS,
                        "CopyRows capped to the row ceiling"
                    );
                    MAX_COPY_ROWS
                } else {
                    limit
                };
                let events = events.clone();
                let limit_src = page_fetch_limit.clone();
                tokio::spawn(async move {
                    let _permit = limit_src.acquire_owned().await;
                    // A deliberate clipboard re-fetch isn't superseded by scrolling,
                    // so it carries a throwaway signal that never aborts.
                    let abort = AbortSignal::new();
                    match driver
                        .fetch_page(&sql, offset, limit, PageCap::Full, &abort)
                        .await
                    {
                        Ok(page) => emit(
                            &events,
                            session_id,
                            Event::CopyRowsLoaded {
                                id,
                                rows: page.rows,
                            },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::CloseResult { epoch } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    continue;
                };
                // Stop every in-flight fetch for the tab at the engine, then forget it.
                if let Some(f) = state.inflight.remove(&epoch) {
                    f.abort_all();
                }
                lock(&state.results).remove(&epoch);
            }

            Command::KvFetchScan {
                epoch,
                pattern,
                type_filter,
                value_needle,
                cursor,
                budget,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                // A retyped filter pattern supersedes the previous scan for
                // this epoch, like a flung scrollbar supersedes a SQL page.
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_scan.take() {
                    prev.abort();
                }
                let abort = AbortSignal::new();
                entry.kv_scan = Some(abort.clone());
                let events = events.clone();
                tokio::spawn(async move {
                    match driver
                        .scan_keys(
                            cursor,
                            pattern.as_deref(),
                            // Map the typed filter to its `TYPE` label at the driver
                            // seam; the wire carries the enum, not the string.
                            type_filter.as_ref().map(red_core::kv::KvType::label),
                            value_needle.as_deref(),
                            budget,
                            &abort,
                        )
                        .await
                    {
                        Ok(page) => emit(&events, session_id, Event::KvScanPage { epoch, page }),
                        Err(RedError::Interrupted) => {}
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvProbeKey { epoch, key } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let events = events.clone();
                tokio::spawn(async move {
                    match driver.probe_key(&key).await {
                        Ok(meta) => {
                            emit(&events, session_id, Event::KvKeyProbed { epoch, key, meta })
                        }
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvDbSize { epoch } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get(&id) else {
                    continue;
                };
                // Swallow errors like `LoadForeignKeys`: a missing header stat
                // isn't worth a toast.
                let Some(driver) = state.driver.as_kv().cloned() else {
                    continue;
                };
                let events = events.clone();
                tokio::spawn(async move {
                    if let Ok(count) = driver.db_size().await {
                        emit(&events, session_id, Event::KvDbSizeReady { epoch, count });
                    }
                });
            }

            Command::KvReadValue { epoch, key } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                // A new key selection (or a re-selection of the same key)
                // supersedes whatever the inspector was fetching before.
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_value.take() {
                    prev.abort();
                }
                let abort = AbortSignal::new();
                entry.kv_value = Some(abort.clone());
                let events = events.clone();
                tokio::spawn(async move {
                    let result = driver.read_value(&key).await;
                    // `read_value` doesn't arm the abort with an engine token, so
                    // supersession is advisory: a concurrent `KvApplyEdit` (or a
                    // new selection) takes and aborts this slot while the read is
                    // in flight. Drop a late reply so it can't stomp the
                    // freshly-applied value back to its pre-edit contents.
                    if abort.is_aborted() {
                        return;
                    }
                    match result {
                        Ok(value) => emit(
                            &events,
                            session_id,
                            Event::KvValueReady { epoch, key, value },
                        ),
                        Err(RedError::Interrupted) => {}
                        Err(e) => emit(
                            &events,
                            session_id,
                            Event::KvValueError {
                                epoch,
                                key,
                                message: e.to_string(),
                            },
                        ),
                    }
                });
            }

            Command::KvReadStringFull { epoch, key } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                // Shares the inspector's in-flight slot with `KvReadValue`: a new
                // key selection mid-load supersedes this fetch.
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_value.take() {
                    prev.abort();
                }
                let abort = AbortSignal::new();
                entry.kv_value = Some(abort.clone());
                let events = events.clone();
                tokio::spawn(async move {
                    let result = driver.read_string_full(&key).await;
                    // Like `KvReadValue`: drop a late reply if a concurrent edit or
                    // a new selection superseded this fetch, so it can't overwrite
                    // freshly-applied data.
                    if abort.is_aborted() {
                        return;
                    }
                    match result {
                        // Wrap the whole string back into `KvValue::Str` and reuse
                        // `KvValueReady`: the UI's key-matched apply path swaps the
                        // capped body for this one with no new event.
                        Ok(value) => emit(
                            &events,
                            session_id,
                            Event::KvValueReady {
                                epoch,
                                key,
                                value: value.map(red_core::kv::KvValue::Str),
                            },
                        ),
                        Err(RedError::Interrupted) => {}
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvReadCollectionPage {
                epoch,
                key,
                kind,
                cursor,
                budget,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                // Its own slot (not `kv_value`): a sibling value read must not
                // abort an in-progress page scan and leave the sub-grid stuck
                // on "Loading…" (an interrupted scan emits no event).
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_collection.take() {
                    prev.abort();
                }
                let abort = AbortSignal::new();
                entry.kv_collection = Some(abort.clone());
                let events = events.clone();
                tokio::spawn(async move {
                    match driver
                        .read_collection_page(&key, kind, cursor, budget, &abort)
                        .await
                    {
                        Ok(page) => emit(
                            &events,
                            session_id,
                            Event::KvCollectionPageReady { epoch, key, page },
                        ),
                        Err(RedError::Interrupted) => {}
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvReadListWindow {
                epoch,
                key,
                from_head,
                count,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_value.take() {
                    prev.abort();
                }
                // `read_list_window` has no cancel token to pass (a single
                // bounded `LRANGE`, unlike the budgeted `SCAN` loops above);
                // still record an `AbortSignal` in `entry.kv_value` so a
                // following `KvReadValue`/`KvReadCollectionPage` is tracked
                // as superseding this fetch, for consistency with them.
                entry.kv_value = Some(AbortSignal::new());
                let events = events.clone();
                tokio::spawn(async move {
                    match driver.read_list_window(&key, from_head, count).await {
                        Ok(values) => emit(
                            &events,
                            session_id,
                            Event::KvListWindowReady {
                                epoch,
                                key,
                                from_head,
                                values,
                            },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvReadStreamPage {
                epoch,
                key,
                before,
                count,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_value.take() {
                    prev.abort();
                }
                // Like `read_list_window`, a single bounded `XREVRANGE` with
                // no cancel token; the `AbortSignal` only marks it superseded
                // by a following inspector fetch.
                entry.kv_value = Some(AbortSignal::new());
                let events = events.clone();
                tokio::spawn(async move {
                    match driver
                        .read_stream_range(&key, before.as_deref(), count)
                        .await
                    {
                        Ok(page) => emit(
                            &events,
                            session_id,
                            Event::KvStreamPageReady { epoch, key, page },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvStreamGroups { epoch, key } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_value.take() {
                    prev.abort();
                }
                entry.kv_value = Some(AbortSignal::new());
                let events = events.clone();
                tokio::spawn(async move {
                    match driver.stream_groups(&key).await {
                        Ok(groups) => emit(
                            &events,
                            session_id,
                            Event::KvStreamGroupsReady { epoch, key, groups },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvStreamConsumers { epoch, key, group } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_group_detail.take() {
                    prev.abort();
                }
                entry.kv_group_detail = Some(AbortSignal::new());
                let events = events.clone();
                tokio::spawn(async move {
                    match driver.stream_consumers(&key, &group).await {
                        Ok(consumers) => emit(
                            &events,
                            session_id,
                            Event::KvStreamConsumersReady {
                                epoch,
                                key,
                                group,
                                consumers,
                            },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvStreamPending {
                epoch,
                key,
                group,
                count,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                // Shares the `kv_value` slot's sibling `kv_group_detail` with
                // the consumers fetch above: both are the selected group's
                // detail, kicked off together, and neither should cancel the
                // other, so pending gets its own token to supersede only a
                // later pending fetch.
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_group_pending.take() {
                    prev.abort();
                }
                entry.kv_group_pending = Some(AbortSignal::new());
                let events = events.clone();
                tokio::spawn(async move {
                    match driver.stream_pending(&key, &group, count).await {
                        Ok(pending) => emit(
                            &events,
                            session_id,
                            Event::KvStreamPendingReady {
                                epoch,
                                key,
                                group,
                                pending,
                            },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvStreamAction {
                epoch,
                key,
                group,
                action,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                // Defense in depth alongside the driver's own refusal (see
                // `KvApplyEdit`): reject before touching the engine.
                if state.read_only {
                    emit(
                        &events,
                        session_id,
                        Event::Error("this connection is read-only".into()),
                    );
                    continue;
                }
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let kind = action.action();
                let events = events.clone();
                tokio::spawn(async move {
                    let result = match &action {
                        red_core::kv::KvStreamActionReq::Ack { ids } => {
                            driver.stream_ack(&key, &group, ids).await
                        }
                        red_core::kv::KvStreamActionReq::Claim {
                            consumer,
                            min_idle_ms,
                            ids,
                        } => {
                            driver
                                .stream_claim(
                                    &key,
                                    &group,
                                    consumer,
                                    Duration::from_millis(*min_idle_ms),
                                    ids,
                                )
                                .await
                        }
                    };
                    match result {
                        Ok(count) => emit(
                            &events,
                            session_id,
                            Event::KvStreamActionDone {
                                epoch,
                                key,
                                group,
                                action: kind,
                                count,
                            },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvCommand { epoch, argv, req } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                // Defense in depth alongside the driver's own `classify_command`
                // refusal (see `RedisDriver::command`): a read-only connection
                // rejects any non-read console command at the service boundary
                // too, so a classifier gap can't let a write reach the engine.
                // The driver still runs the read/write split for reads it does
                // allow.
                if state.read_only
                    && red_core::kv::classify_command(&argv) != red_core::kv::CommandClass::Read
                {
                    emit(
                        &events,
                        session_id,
                        Event::Error("this connection is read-only".into()),
                    );
                    continue;
                }
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_value.take() {
                    prev.abort();
                }
                entry.kv_value = Some(AbortSignal::new());
                let events = events.clone();
                tokio::spawn(async move {
                    match driver.command(&argv).await {
                        Ok(result) => emit(
                            &events,
                            session_id,
                            Event::KvCommandResult {
                                epoch,
                                argv,
                                result,
                                req,
                            },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvImport { epoch, commands } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                // A read-only connection can't import (every command that writes
                // would be refused anyway); reject the whole batch up front.
                if state.read_only {
                    emit(
                        &events,
                        session_id,
                        Event::Error("this connection is read-only".into()),
                    );
                    continue;
                }
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let events = events.clone();
                tokio::spawn(async move {
                    // Sequential so dependent commands (e.g. HSET after DEL) keep
                    // their file order; the read/write gate + classifier apply
                    // per command via `driver.command`.
                    let (mut ok, mut failed) = (0usize, 0usize);
                    let mut first_error = None;
                    for argv in &commands {
                        if argv.is_empty() {
                            continue;
                        }
                        match driver.command(argv).await {
                            Ok(_) => ok += 1,
                            Err(e) => {
                                failed += 1;
                                if first_error.is_none() {
                                    first_error = Some(format!("{}: {e}", argv.join(" ")));
                                }
                            }
                        }
                    }
                    emit(
                        &events,
                        session_id,
                        Event::KvImportDone {
                            epoch,
                            ok,
                            failed,
                            first_error,
                        },
                    );
                });
            }

            Command::KvApplyEdit { epoch, edit } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                // Defense in depth alongside the driver's own refusal (see
                // `RedisDriver::check_writable`): reject here too, before
                // even touching the engine.
                if state.read_only {
                    emit(
                        &events,
                        session_id,
                        Event::Error("this connection is read-only".into()),
                    );
                    continue;
                }
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_value.take() {
                    prev.abort();
                }
                entry.kv_value = Some(AbortSignal::new());
                let events = events.clone();
                tokio::spawn(async move {
                    let result = match &edit {
                        KvEdit::SetString { key, value, ttl } => {
                            driver.set_string(key, value.clone(), *ttl).await
                        }
                        KvEdit::SetField { key, field, value } => {
                            driver.set_field(key, field, value.clone()).await
                        }
                        KvEdit::HashDelete { key, fields } => {
                            driver.hash_delete(key, fields).await.map(|_| ())
                        }
                        KvEdit::SetAdd { key, members } => {
                            driver.set_add(key, members).await.map(|_| ())
                        }
                        KvEdit::SetRemove { key, members } => {
                            driver.set_remove(key, members).await.map(|_| ())
                        }
                        KvEdit::SetReplace { key, old, new } => {
                            // Atomic remove+add (one MULTI): a failure mid-way
                            // can't drop the old member without adding the new.
                            driver.set_replace(key, old, new).await
                        }
                        KvEdit::ZSetAdd { key, member, score } => {
                            driver.zset_add(key, member, *score).await
                        }
                        KvEdit::ZSetRemove { key, members } => {
                            driver.zset_remove(key, members).await.map(|_| ())
                        }
                        KvEdit::ListSet { key, index, value } => {
                            driver.list_set(key, *index, value.clone()).await
                        }
                        KvEdit::ListPush { key, value, head } => driver
                            .list_push(key, value.clone(), *head)
                            .await
                            .map(|_| ()),
                        KvEdit::ListRemove { key, count, value } => driver
                            .list_remove(key, *count, value.clone())
                            .await
                            .map(|_| ()),
                        KvEdit::ListRemoveAt { key, index } => {
                            driver.list_remove_at(key, *index).await
                        }
                        KvEdit::SetTtl { key, ttl } => driver.set_ttl(key, *ttl).await,
                        KvEdit::Rename { from, to } => driver.rename_key(from, to).await,
                        KvEdit::Delete { keys } => {
                            // Snapshot each key (DUMP + PTTL) before removing it,
                            // so the delete can be undone from the recycle bin.
                            // Best-effort: a key that can't be dumped just isn't
                            // recoverable; the delete still proceeds.
                            let mut recycled = Vec::new();
                            for k in keys {
                                if let Ok(Some((payload, ttl))) = driver.dump_key(k).await {
                                    recycled.push(RecycledKey {
                                        key: k.clone(),
                                        ttl,
                                        payload,
                                    });
                                }
                            }
                            let done = driver.delete_keys(keys).await.map(|_| ());
                            if done.is_ok() && !recycled.is_empty() {
                                emit(
                                    &events,
                                    session_id,
                                    Event::KvKeysRecycled {
                                        epoch,
                                        keys: recycled,
                                    },
                                );
                            }
                            done
                        }
                        KvEdit::StreamAdd { key, fields } => {
                            driver.stream_add(key, fields).await.map(|_| ())
                        }
                    };
                    match result {
                        Ok(()) => emit(&events, session_id, Event::KvEditApplied { epoch, edit }),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvRestoreKeys { epoch, keys } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                if state.read_only {
                    emit(
                        &events,
                        session_id,
                        Event::Error("this connection is read-only".into()),
                    );
                    continue;
                }
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let events = events.clone();
                tokio::spawn(async move {
                    let mut restored = 0u64;
                    for k in &keys {
                        match driver.restore_key(&k.key, k.ttl, &k.payload, false).await {
                            Ok(()) => restored += 1,
                            // A single failure (e.g. BUSYKEY — the key was
                            // recreated meanwhile) surfaces but doesn't abort the
                            // rest of the batch.
                            Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                        }
                    }
                    emit(
                        &events,
                        session_id,
                        Event::KvKeysRestored {
                            epoch,
                            count: restored,
                        },
                    );
                });
            }

            Command::KvCopyKeys {
                keys,
                target_session,
            } => {
                let Some(source_sid) = session_id else {
                    continue;
                };
                let Some(src_state) = sessions.get(&source_sid) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(src) = src_state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("source isn't a Redis connection".into()),
                    );
                    continue;
                };
                let src_busy = src_state.busy.clone();
                let Some(dst_state) = sessions.get(&target_session) else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("target connection isn't open".into()),
                    );
                    continue;
                };
                // Defense in depth alongside the UI's writable-target filter.
                if dst_state.read_only {
                    emit(
                        &events,
                        session_id,
                        Event::Error("target connection is read-only".into()),
                    );
                    continue;
                }
                let Some(dst) = dst_state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("target isn't a Redis connection".into()),
                    );
                    continue;
                };
                let dst_busy = dst_state.busy.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    // Pin both ends so neither is idle-evicted mid-copy.
                    let _src_pin = PinGuard::new(src_busy);
                    let _dst_pin = PinGuard::new(dst_busy);
                    let mut copied = 0u64;
                    let mut failed = 0u64;
                    for k in &keys {
                        // DUMP on the source, RESTORE ... REPLACE on the target: a
                        // vanished key or a failed restore counts as a failure but
                        // never aborts the batch.
                        match src.dump_key(k).await {
                            Ok(Some((payload, ttl))) => {
                                match dst.restore_key(k, ttl, &payload, true).await {
                                    Ok(()) => copied += 1,
                                    Err(_) => failed += 1,
                                }
                            }
                            _ => failed += 1,
                        }
                    }
                    // Global (None) session so the toast survives a ⌘P switch.
                    emit(&events, None, Event::KvKeysCopied { copied, failed });
                });
            }

            Command::KvSubscribe { epoch, pattern } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_subscribe.take() {
                    prev.abort();
                }
                let abort = AbortSignal::new();
                entry.kv_subscribe = Some(abort.clone());
                let events = events.clone();
                tokio::spawn(async move {
                    let mut sub = match driver.subscribe(&pattern).await {
                        Ok(sub) => sub,
                        Err(e) => {
                            emit(&events, session_id, Event::Error(e.to_string()));
                            return;
                        }
                    };
                    // No native cancel for a live pubsub stream (unlike the
                    // budgeted `SCAN` loops, which check `abort` between
                    // round trips): poll with a bounded timeout instead, so
                    // `CloseResult`'s abort is noticed within one tick rather
                    // than blocking forever on the next message that may
                    // never come.
                    let mut rate = StreamRate::new();
                    loop {
                        if abort.is_aborted() {
                            break;
                        }
                        match tokio::time::timeout(Duration::from_millis(500), sub.stream.next())
                            .await
                        {
                            Ok(Some(msg)) => {
                                // Rate-limit a firehose subscription (`PSUBSCRIBE *`)
                                // so it can't outgrow the event channel.
                                let (admit, dropped) = rate.admit();
                                if let Some(n) = dropped {
                                    emit(
                                        &events,
                                        session_id,
                                        Event::KvMessage {
                                            epoch,
                                            channel: "[red]".into(),
                                            payload: format!("dropped {n} messages (rate limit)"),
                                        },
                                    );
                                }
                                if admit {
                                    emit(
                                        &events,
                                        session_id,
                                        Event::KvMessage {
                                            epoch,
                                            channel: msg.channel,
                                            payload: msg.payload,
                                        },
                                    );
                                }
                            }
                            Ok(None) => break, // the subscription's connection closed
                            Err(_) => {
                                // Timed out this tick; recheck `abort` on the next
                                // loop, but first flush any drops a burst left
                                // pending so a now-quiet firehose still reports them.
                                if let Some(n) = rate.flush_drops() {
                                    emit(
                                        &events,
                                        session_id,
                                        Event::KvMessage {
                                            epoch,
                                            channel: "[red]".into(),
                                            payload: format!("dropped {n} messages (rate limit)"),
                                        },
                                    );
                                }
                            }
                        }
                    }
                });
            }

            Command::KvNotifyConfig { epoch } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_value.take() {
                    prev.abort();
                }
                entry.kv_value = Some(AbortSignal::new());
                let events = events.clone();
                tokio::spawn(async move {
                    match driver.notify_config().await {
                        Ok(value) => emit(
                            &events,
                            session_id,
                            Event::KvNotifyConfigReady { epoch, value },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvSetNotifyConfig { epoch, flags } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                if state.read_only {
                    emit(
                        &events,
                        session_id,
                        Event::Error("this connection is read-only".into()),
                    );
                    continue;
                }
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let events = events.clone();
                tokio::spawn(async move {
                    // Set, then re-read so the watcher reflects the actual stored
                    // value (Redis canonicalizes the flag string) in one reply.
                    match driver.set_notify_config(&flags).await {
                        Ok(()) => match driver.notify_config().await {
                            Ok(value) => emit(
                                &events,
                                session_id,
                                Event::KvNotifyConfigReady { epoch, value },
                            ),
                            Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                        },
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvSlowlog { epoch, count } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_value.take() {
                    prev.abort();
                }
                entry.kv_value = Some(AbortSignal::new());
                let events = events.clone();
                tokio::spawn(async move {
                    match driver.slowlog(count).await {
                        Ok(entries) => emit(
                            &events,
                            session_id,
                            Event::KvSlowlogReady { epoch, entries },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvSlowlogReset { epoch } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                if state.read_only {
                    emit(
                        &events,
                        session_id,
                        Event::Error("this connection is read-only".into()),
                    );
                    continue;
                }
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let events = events.clone();
                tokio::spawn(async move {
                    match driver.slowlog_reset().await {
                        // Reply with an empty log so the UI clears without a
                        // second round trip.
                        Ok(()) => emit(
                            &events,
                            session_id,
                            Event::KvSlowlogReady {
                                epoch,
                                entries: Vec::new(),
                            },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvMonitor { epoch } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_monitor.take() {
                    prev.abort();
                }
                let abort = AbortSignal::new();
                entry.kv_monitor = Some(abort.clone());
                let events = events.clone();
                tokio::spawn(async move {
                    let mut mon = match driver.monitor().await {
                        Ok(mon) => mon,
                        Err(e) => {
                            emit(&events, session_id, Event::Error(e.to_string()));
                            return;
                        }
                    };
                    // Same bounded-poll teardown as `KvSubscribe`: MONITOR has
                    // no native cancel, so check `abort` between reads rather
                    // than blocking forever on the next line.
                    let mut rate = StreamRate::new();
                    loop {
                        if abort.is_aborted() {
                            break;
                        }
                        match tokio::time::timeout(Duration::from_millis(500), mon.stream.next())
                            .await
                        {
                            Ok(Some(line)) => {
                                // Rate-limit the firehose so it can't outgrow the
                                // event channel; report dropped lines in-band.
                                let (admit, dropped) = rate.admit();
                                if let Some(n) = dropped {
                                    emit(
                                        &events,
                                        session_id,
                                        Event::KvMonitorLine {
                                            epoch,
                                            line: format!(
                                                "[red] dropped {n} MONITOR lines (rate limit)"
                                            ),
                                        },
                                    );
                                }
                                if admit {
                                    emit(&events, session_id, Event::KvMonitorLine { epoch, line });
                                }
                            }
                            Ok(None) => break, // the monitor connection closed
                            Err(_) => {
                                // Timed out this tick; recheck `abort` next loop,
                                // but flush any drops a burst left pending so a
                                // now-quiet firehose still reports them.
                                if let Some(n) = rate.flush_drops() {
                                    emit(
                                        &events,
                                        session_id,
                                        Event::KvMonitorLine {
                                            epoch,
                                            line: format!(
                                                "[red] dropped {n} MONITOR lines (rate limit)"
                                            ),
                                        },
                                    );
                                }
                            }
                        }
                    }
                });
            }

            Command::KvClientList { epoch } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.kv_value.take() {
                    prev.abort();
                }
                entry.kv_value = Some(AbortSignal::new());
                let events = events.clone();
                tokio::spawn(async move {
                    match driver.client_list().await {
                        Ok(clients) => emit(
                            &events,
                            session_id,
                            Event::KvClientListReady { epoch, clients },
                        ),
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::KvClientKill { epoch, id: kill_id } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                if state.read_only {
                    emit(
                        &events,
                        session_id,
                        Event::Error("this connection is read-only".into()),
                    );
                    continue;
                }
                let Some(driver) = state.driver.as_kv().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a Redis connection".into()),
                    );
                    continue;
                };
                let events = events.clone();
                tokio::spawn(async move {
                    // Kill, then refetch so the viewer reflects the removal in one
                    // reply. A kill failure is surfaced; a refetch failure after a
                    // successful kill still succeeded the kill, so it's the error.
                    match driver.client_kill(kill_id).await {
                        Ok(()) => match driver.client_list().await {
                            Ok(clients) => emit(
                                &events,
                                session_id,
                                Event::KvClientListReady { epoch, clients },
                            ),
                            Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                        },
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::ColumnStats {
                epoch,
                column,
                flags,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                // Reuse the result's stored (already-wrapped, filtered) SQL so the
                // summary matches the visible rows. A stale epoch (tab closed /
                // re-sorted) drops the request, like `FetchPage`.
                let Some(sql) = lock(&state.results).get(&epoch).map(|s| s.sql.clone()) else {
                    continue;
                };
                // A newer stats request for this epoch (the selection moved to
                // another column) supersedes the last one; cancel its in-flight
                // aggregate at the engine so a heavy `count(distinct)` doesn't linger.
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.stats.take() {
                    prev.abort();
                }
                let abort = AbortSignal::new();
                entry.stats = Some(abort.clone());
                // Off the dispatch loop (a `count(distinct)` over a big result can be
                // slow) and under the shared page-fetch cap so it can't fan out.
                let events = events.clone();
                let limit_src = page_fetch_limit.clone();
                let timeout = statement_timeout;
                tokio::spawn(async move {
                    if abort.is_aborted() {
                        return;
                    }
                    let _permit = limit_src.acquire_owned().await;
                    if abort.is_aborted() {
                        return;
                    }
                    let fetch = driver.column_stats(&sql, &column, flags, &abort);
                    match with_timeout(timeout, &abort, fetch).await {
                        Ok(stats) => emit(
                            &events,
                            session_id,
                            Event::ColumnStatsReady {
                                epoch,
                                column,
                                stats,
                            },
                        ),
                        // Superseded mid-flight (the selection moved): stay silent;
                        // the newer request delivers.
                        Err(RedError::Interrupted) => {}
                        // Pane-scoped failure (shown in the bar), not a global toast.
                        Err(e) => {
                            tracing::warn!(%epoch, %column, "column stats failed: {e}");
                            emit(
                                &events,
                                session_id,
                                Event::ColumnStatsFailed { epoch, column },
                            );
                        }
                    }
                });
            }

            Command::FetchLookup {
                epoch,
                target,
                id_column,
                label_column,
                limit,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get_mut(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                // A newer lookup for this epoch (editing moved to another FK column)
                // supersedes the last; cancel its in-flight fetch at the engine.
                let entry = state.inflight.entry(epoch).or_default();
                if let Some(prev) = entry.lookup.take() {
                    prev.abort();
                }
                let abort = AbortSignal::new();
                entry.lookup = Some(abort.clone());
                let events = events.clone();
                let limit_src = page_fetch_limit.clone();
                let timeout = statement_timeout;
                tokio::spawn(async move {
                    if abort.is_aborted() {
                        return;
                    }
                    let _permit = limit_src.acquire_owned().await;
                    if abort.is_aborted() {
                        return;
                    }
                    let fetch = driver.fetch_lookup(
                        &target,
                        &id_column,
                        label_column.as_deref(),
                        limit,
                        &abort,
                    );
                    match with_timeout(timeout, &abort, fetch).await {
                        Ok(rows) => emit(
                            &events,
                            session_id,
                            Event::LookupReady {
                                epoch,
                                target,
                                rows,
                            },
                        ),
                        Err(RedError::Interrupted) => {}
                        Err(e) => {
                            tracing::warn!(%epoch, "fk lookup failed: {e}");
                            emit(&events, session_id, Event::LookupFailed { epoch, target });
                        }
                    }
                });
            }

            Command::Execute { sql } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                let results = state.results.clone();
                match driver.execute(&sql).await {
                    Ok(affected) => {
                        // A write may have shifted rows under any open result, so
                        // drop the checkpoint indexes; they rebuild lazily on the
                        // next deep jump rather than serving from stale keys.
                        for spec in lock(&results).values() {
                            let mut idx = lock(&spec.checkpoints);
                            idx.points.clear();
                            idx.status = BuildStatus::Idle;
                        }
                        emit(
                            &events,
                            session_id,
                            Event::Executed {
                                affected: affected as usize,
                            },
                        );
                    }
                    Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                }
            }

            Command::ApplyBatch { epoch, ops } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                let results = state.results.clone();
                // An atomic batch of bounded writes, each asserted to touch exactly
                // one row by the driver (all-or-nothing). Like `Execute`, a success
                // may shift rows under any open result, so drop the checkpoint
                // indexes; the failure is pane-local (`BatchFailed`), not a global
                // error toast.
                match driver.apply_edits(&ops).await {
                    Ok(applied) => {
                        for spec in lock(&results).values() {
                            let mut idx = lock(&spec.checkpoints);
                            idx.points.clear();
                            idx.status = BuildStatus::Idle;
                        }
                        emit(&events, session_id, Event::BatchApplied { epoch, applied });
                    }
                    Err(e) => emit(
                        &events,
                        session_id,
                        Event::BatchFailed {
                            epoch,
                            failed_index: None,
                            message: e.to_string(),
                        },
                    ),
                }
            }

            Command::Explain {
                sql,
                analyze,
                epoch,
            } => {
                let Some(id) = session_id else { continue };
                let Some(state) = sessions.get(&id) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                // A plan is one bounded round-trip: no cursor, no windowing. The
                // failure is pane-local (`PlanFailed`), not a global error toast.
                match driver.explain(&sql, analyze).await {
                    Ok(plan) => emit(&events, session_id, Event::PlanReady { epoch, plan }),
                    Err(e) => emit(
                        &events,
                        session_id,
                        Event::PlanFailed {
                            epoch,
                            message: e.to_string(),
                        },
                    ),
                }
            }

            Command::Export {
                format,
                path,
                epoch,
                id,
            } => {
                let Some(sid) = session_id else { continue };
                let Some(state) = sessions.get(&sid) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                let Some(sql) = lock(&state.results).get(&epoch).map(|s| s.sql.clone()) else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("no open result to export".into()),
                    );
                    continue;
                };
                // Register the cancel flag before the task starts, so a fast
                // `CancelExport` can't race ahead of it.
                let cancel = Arc::new(AtomicBool::new(false));
                lock(&state.exports).insert(id, cancel.clone());

                // Forward the driver's throttled row counts to the UI as progress
                // events; the channel closes (loop ends) when the export drops its
                // sender on completion.
                let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
                {
                    let events = events.clone();
                    tokio::spawn(async move {
                        while let Some(rows) = progress_rx.recv().await {
                            emit(
                                &events,
                                session_id,
                                Event::ExportProgress {
                                    id,
                                    rows: rows as usize,
                                },
                            );
                        }
                    });
                }

                // Run the export off the dispatch loop so the loop keeps pumping
                // (a `CancelExport` or any other command lands while it streams).
                let events = events.clone();
                let exports = state.exports.clone();
                let export_limit = export_limit.clone();
                tokio::spawn(async move {
                    // Hold a permit for the export's lifetime so concurrent exports
                    // are capped (queued exports wait here; the cancel flag is
                    // already registered, so a wait can still be cancelled).
                    let _permit = export_limit.acquire_owned().await;
                    let path_str = path.to_string_lossy().into_owned();
                    let result = driver
                        .export(&sql, &path, format, cancel, progress_tx)
                        .await;
                    lock(&exports).remove(&id);
                    match result {
                        Ok(rows) => emit(
                            &events,
                            session_id,
                            Event::ExportFinished {
                                id,
                                path: path_str,
                                rows: rows as usize,
                            },
                        ),
                        Err(RedError::Interrupted) => {
                            emit(&events, session_id, Event::ExportCancelled { id })
                        }
                        Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
                    }
                });
            }

            Command::CancelExport { id } => {
                let Some(sid) = session_id else { continue };
                // Flip the flag; the export's per-row check picks it up, removes
                // the partial file, and replies `ExportCancelled`.
                if let Some(state) = sessions.get(&sid)
                    && let Some(cancel) = lock(&state.exports).get(&id)
                {
                    cancel.store(true, Ordering::Relaxed);
                }
            }

            Command::Import {
                path,
                format,
                target,
                mapping,
                chunk_size,
                id,
            } => {
                let Some(sid) = session_id else { continue };
                let Some(state) = sessions.get(&sid) else {
                    emit(&events, session_id, Event::Error("not connected".into()));
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        session_id,
                        Event::Error("not a SQL connection".into()),
                    );
                    continue;
                };
                // Reuse the session's transfer-cancel registry (a shared id space
                // with exports) so a `CancelImport` can flip the flag.
                let cancel = Arc::new(AtomicBool::new(false));
                lock(&state.exports).insert(id, cancel.clone());

                // Forward throttled committed-row counts to the UI as progress; the
                // channel closes when the import drops its sender on completion.
                let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
                {
                    let events = events.clone();
                    tokio::spawn(async move {
                        while let Some(rows) = progress_rx.recv().await {
                            emit(
                                &events,
                                session_id,
                                Event::ImportProgress {
                                    id,
                                    rows: rows as usize,
                                },
                            );
                        }
                    });
                }

                // Run the import off the dispatch loop (file IO on a blocking thread,
                // each chunk's `insert_rows` driven with `block_on`).
                let events = events.clone();
                let exports = state.exports.clone();
                let import_limit = import_limit.clone();
                tokio::spawn(async move {
                    let _permit = import_limit.acquire_owned().await;
                    let handle = tokio::runtime::Handle::current();
                    let outcome = tokio::task::spawn_blocking(move || {
                        run_import_blocking(
                            driver,
                            path,
                            format,
                            target,
                            mapping,
                            chunk_size,
                            cancel,
                            progress_tx,
                            handle,
                        )
                    })
                    .await;
                    lock(&exports).remove(&id);
                    let (committed, err) = match outcome {
                        Ok(pair) => pair,
                        Err(join) => (
                            0,
                            Some(RedError::Driver(format!("import task failed: {join}"))),
                        ),
                    };
                    let rows = committed as usize;
                    match err {
                        None => emit(&events, session_id, Event::ImportFinished { id, rows }),
                        Some(RedError::Interrupted) => {
                            emit(&events, session_id, Event::ImportCancelled { id, rows })
                        }
                        Some(e) => emit(
                            &events,
                            session_id,
                            Event::ImportFailed {
                                id,
                                rows,
                                message: e.to_string(),
                            },
                        ),
                    }
                });
            }

            Command::CancelImport { id } => {
                let Some(sid) = session_id else { continue };
                // Flip the flag; the import's between-rows check picks it up and
                // replies `ImportCancelled` (earlier committed chunks remain).
                if let Some(state) = sessions.get(&sid)
                    && let Some(cancel) = lock(&state.exports).get(&id)
                {
                    cancel.store(true, Ordering::Relaxed);
                }
            }

            Command::CopyTargetColumns { id, target } => {
                // Describe the copy target's columns on the *target* session (the
                // envelope's), so the UI can auto-map by name before any write.
                let Some(sid) = session_id else { continue };
                let Some(state) = sessions.get(&sid) else {
                    emit(
                        &events,
                        None,
                        Event::CopyFailed {
                            id,
                            rows: 0,
                            message: "target connection isn't open".into(),
                        },
                    );
                    continue;
                };
                let Some(driver) = state.driver.as_sql().cloned() else {
                    emit(
                        &events,
                        None,
                        Event::CopyFailed {
                            id,
                            rows: 0,
                            message: "target connection isn't a SQL connection".into(),
                        },
                    );
                    continue;
                };
                let events = events.clone();
                tokio::spawn(async move {
                    let schema = target.schema.clone().unwrap_or_default();
                    match driver.describe_table(&schema, &target.name).await {
                        Ok(detail) => {
                            let columns = detail
                                .columns
                                .iter()
                                .map(|c| Column {
                                    name: c.name.clone(),
                                    decl_type: c.type_name.clone(),
                                })
                                .collect();
                            emit(&events, None, Event::CopyTargetColumns { id, columns });
                        }
                        Err(e) => emit(
                            &events,
                            None,
                            Event::CopyFailed {
                                id,
                                rows: 0,
                                message: e.to_string(),
                            },
                        ),
                    }
                });
            }

            Command::CopyToTable {
                id,
                source_epoch,
                target,
                target_session,
                mapping,
                mode,
                create,
            } => {
                // Fail fast with a `CopyFailed` (the toast's terminal event) on any
                // missing piece, so the UI never strands a "Copying…" toast.
                macro_rules! copy_fail {
                    ($msg:expr_2021) => {{
                        emit(
                            &events,
                            None,
                            Event::CopyFailed {
                                id,
                                rows: 0,
                                message: $msg.into(),
                            },
                        );
                        continue;
                    }};
                }
                let Some(source_sid) = session_id else {
                    continue;
                };
                // Source: the open result's already-wrapped (filtered/sorted) SQL,
                // re-read at full fidelity through a fresh cursor.
                let Some(src_state) = sessions.get(&source_sid) else {
                    copy_fail!("source connection isn't open")
                };
                let Some(source_sql) = lock(&src_state.results)
                    .get(&source_epoch)
                    .map(|s| s.sql.clone())
                else {
                    copy_fail!("no open result to copy")
                };
                let Some(src) = src_state.driver.as_sql().cloned() else {
                    copy_fail!("source isn't a SQL connection")
                };
                let src_busy = src_state.busy.clone();
                let exports = src_state.exports.clone();
                // Target: another open session (or the same one). Its driver does the
                // writes; both ends are pinned for the copy's lifetime.
                let Some(dst_state) = sessions.get(&target_session) else {
                    copy_fail!("target connection isn't open")
                };
                // Defense in depth alongside the UI's target filter (see
                // `collect_targets`/`collect_namespaces`, which hide read-only
                // connections): never write to — or create a table on — a
                // read-only destination, even if a stale command reaches here.
                if dst_state.read_only {
                    copy_fail!(if create.is_some() {
                        "target connection is read-only — can't create a table there"
                    } else {
                        "target connection is read-only"
                    })
                }
                let Some(dst) = dst_state.driver.as_sql().cloned() else {
                    copy_fail!("target isn't a SQL connection")
                };
                let dst_busy = dst_state.busy.clone();

                // Register the cancel flag on the source session's transfer registry
                // (shared id space with exports/imports) so a `CancelCopy` flips it.
                let cancel = Arc::new(AtomicBool::new(false));
                lock(&exports).insert(id, cancel.clone());

                // Copy events route *globally* (`None` session): the op spans two
                // connections and its toast lives on the UI's global notification
                // list, surviving a `⌘P` connection switch. `copy_job` emits its own
                // `CopyProgress` inline so the terminal event below strictly follows
                // the last progress (no separate forwarder to race it).
                let events = events.clone();
                let copy_limit = copy_limit.clone();
                tokio::spawn(async move {
                    let _permit = copy_limit.acquire_owned().await;
                    // Pin both ends so neither is evicted mid-copy (no commands touch
                    // a background source/target for minutes); RAII so the pins lift
                    // on finish, cancel, or panic.
                    let _src_pin = PinGuard::new(src_busy);
                    let _dst_pin = PinGuard::new(dst_busy);
                    let (committed, err) = copy_job(
                        src,
                        dst,
                        source_sql,
                        target,
                        mapping,
                        mode,
                        create,
                        cancel,
                        events.clone(),
                        id,
                    )
                    .await;
                    lock(&exports).remove(&id);
                    let rows = committed as usize;
                    match err {
                        None => emit(&events, None, Event::CopyFinished { id, rows }),
                        Some(RedError::Interrupted) => {
                            emit(&events, None, Event::CopyCancelled { id, rows })
                        }
                        Some(e) => emit(
                            &events,
                            None,
                            Event::CopyFailed {
                                id,
                                rows,
                                message: e.to_string(),
                            },
                        ),
                    }
                });
            }

            Command::CancelCopy { id } => {
                let Some(sid) = session_id else { continue };
                // Flip the flag; the copy's between-chunks check picks it up and
                // replies `CopyCancelled` (earlier committed chunks remain).
                if let Some(state) = sessions.get(&sid)
                    && let Some(cancel) = lock(&state.exports).get(&id)
                {
                    cancel.store(true, Ordering::Relaxed);
                }
            }

            Command::MigrateTables {
                id,
                source_schema,
                tables,
                target_session,
                target_schema,
            } => {
                // Fail fast with a `CopyFailed` (the toast's terminal event) on any
                // missing piece, so the UI never strands a "Migrating…" toast.
                macro_rules! migrate_fail {
                    ($msg:expr_2021) => {{
                        emit(
                            &events,
                            None,
                            Event::CopyFailed {
                                id,
                                rows: 0,
                                message: $msg.into(),
                            },
                        );
                        continue;
                    }};
                }
                let Some(source_sid) = session_id else {
                    continue;
                };
                let Some(src_state) = sessions.get(&source_sid) else {
                    migrate_fail!("source connection isn't open")
                };
                let Some(src) = src_state.driver.as_sql().cloned() else {
                    migrate_fail!("source isn't a SQL connection")
                };
                let src_busy = src_state.busy.clone();
                let exports = src_state.exports.clone();
                let Some(dst_state) = sessions.get(&target_session) else {
                    migrate_fail!("target connection isn't open")
                };
                let Some(dst) = dst_state.driver.as_sql().cloned() else {
                    migrate_fail!("target isn't a SQL connection")
                };
                let dst_busy = dst_state.busy.clone();
                if tables.is_empty() {
                    migrate_fail!("no tables to migrate")
                }

                // Reuse the copy cancel registry + the `Copy*` events/toast: a migrate
                // is N copies under one id (one toast, one Cancel).
                let cancel = Arc::new(AtomicBool::new(false));
                lock(&exports).insert(id, cancel.clone());

                let events = events.clone();
                let copy_limit = copy_limit.clone();
                tokio::spawn(async move {
                    let _permit = copy_limit.acquire_owned().await;
                    // Pin both ends for the whole multi-table job (no commands touch a
                    // background source/target for minutes); RAII lifts on finish/cancel.
                    let _src_pin = PinGuard::new(src_busy);
                    let _dst_pin = PinGuard::new(dst_busy);
                    let (committed, err) = migrate_job(
                        src,
                        dst,
                        source_schema,
                        tables,
                        target_schema,
                        cancel,
                        events.clone(),
                        id,
                    )
                    .await;
                    lock(&exports).remove(&id);
                    let rows = committed as usize;
                    match err {
                        None => emit(&events, None, Event::CopyFinished { id, rows }),
                        Some(RedError::Interrupted) => {
                            emit(&events, None, Event::CopyCancelled { id, rows })
                        }
                        Some(e) => emit(
                            &events,
                            None,
                            Event::CopyFailed {
                                id,
                                rows,
                                message: e.to_string(),
                            },
                        ),
                    }
                });
            }

            Command::ImportColumns { path, format, id } => {
                // Peek the header on a blocking thread (cheap file IO, no session
                // needed); reply with the source column names or an ImportFailed.
                let events = events.clone();
                tokio::task::spawn_blocking(move || {
                    let result = File::open(&path)
                        .map_err(|e| format!("cannot open {}: {e}", path.display()))
                        .and_then(|f| {
                            ImportReader::begin(BufReader::new(f), format)
                                .map(|(cols, _)| cols)
                                .map_err(|e| format!("read error: {e}"))
                        });
                    match result {
                        Ok(columns) => {
                            emit(&events, session_id, Event::ImportColumns { id, columns })
                        }
                        Err(message) => emit(
                            &events,
                            session_id,
                            Event::ImportFailed {
                                id,
                                rows: 0,
                                message,
                            },
                        ),
                    }
                });
            }

            Command::Cancel => {
                let Some(id) = session_id else { continue };
                // No fetch is in flight here (pull protocol), so cancelling just
                // drops the cursor; the in-flight case is handled inside
                // `drive_fetch`.
                if let Some(aq) = sessions.get_mut(&id).and_then(|s| s.active.take()) {
                    aq.cancel.cancel();
                    emit(&events, session_id, Event::QueryCancelled);
                }
            }

            Command::Shutdown => break,
        }
    }

    // The window closed or the service is shutting down. Explicitly tear down any
    // live subscription agents (M-S3): the permission-relay tasks hold `Arc` clones
    // of the manager, so dropping the loop's own `Arc` alone would leave a
    // reference cycle and orphan the agent subprocesses. Clearing the map drops
    // their command channels, which unwinds the cycle and reaps the processes.
    ai_acp.lock().await.clear();
}

/// The UI may have dropped its receiver (window closed); a failed send is the
/// expected shutdown path, not an error. `session` tags the event so the UI
/// routes it to the right workspace (`None` for the session-less probe replies).
pub(crate) fn emit(events: &Events, session: Option<SessionId>, event: Event) {
    let _ = events.unbounded_send((session, event));
}

/// The per-second admission budget for a live stream (MONITOR firehose, a broad
/// `PSUBSCRIBE`). Comfortably above a readable live view, well below what would
/// grow the unbounded event channel without bound.
const MAX_STREAM_EVENTS_PER_SEC: usize = 2_000;

/// Producer-side rate limiter for the (unbounded) live-stream event channel. A
/// firehose — MONITOR on a busy server, `PSUBSCRIBE *` — can emit faster than
/// the frame-throttled UI drains, growing the channel backlog until the process
/// runs out of memory (the UI-side buffer caps don't help: they apply only
/// after an event has already left the channel). This caps admitted events per
/// rolling second and counts the rest so the loop can surface a "dropped N"
/// notice.
struct StreamRate {
    window: Instant,
    in_window: usize,
    dropped: usize,
}

impl StreamRate {
    fn new() -> Self {
        Self {
            window: Instant::now(),
            in_window: 0,
            dropped: 0,
        }
    }

    /// Record one arriving item. Returns whether to admit it, plus — roughly
    /// once a second, when the window rolls over after drops — how many were
    /// dropped, for a synthetic notice.
    fn admit(&mut self) -> (bool, Option<usize>) {
        let now = Instant::now();
        let mut notice = None;
        if now.duration_since(self.window) >= Duration::from_secs(1) {
            if self.dropped > 0 {
                notice = Some(self.dropped);
            }
            self.window = now;
            self.in_window = 0;
            self.dropped = 0;
        }
        if self.in_window < MAX_STREAM_EVENTS_PER_SEC {
            self.in_window += 1;
            (true, notice)
        } else {
            self.dropped += 1;
            (false, notice)
        }
    }

    /// Surface any pending drop count when the firehose falls quiet. `admit` only
    /// rolls the window on an arriving item, so a burst that overruns the budget
    /// and then goes silent would otherwise never report its drops; the poll
    /// loop calls this on its idle tick to flush them.
    fn flush_drops(&mut self) -> Option<usize> {
        let now = Instant::now();
        if now.duration_since(self.window) >= Duration::from_secs(1) && self.dropped > 0 {
            let n = self.dropped;
            self.window = now;
            self.in_window = 0;
            self.dropped = 0;
            Some(n)
        } else {
            None
        }
    }
}

mod jobs;
use jobs::*;
