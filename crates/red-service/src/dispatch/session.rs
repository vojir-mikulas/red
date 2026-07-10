//! Keep-alive session state: what the backend owns per connection (driver,
//! streaming cursor, open-result map, in-flight abort handles, exports), the
//! lifecycle (teardown / idle eviction / backstop GC), and applying a spawned
//! connect outcome onto the loop's session map.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use red_core::AiTier;
use red_driver::{AbortSignal, CancelToken, DatabaseDriver, KvDriver, QueryCursor};

use crate::tunnel::Tunnel;
use crate::{Event, SessionId};

use super::paging::ResultMap;
use super::{emit, lock, Events};

/// A session's driver: either the SQL-shaped `DatabaseDriver` seam or the
/// parallel `KvDriver` seam (Redis; see `docs/plans/redis.md`). Every SQL
/// command handler needs the former and has no meaning for the latter, so
/// [`SessionDriver::as_sql`] is how those handlers reject a KV session with a
/// clean `Event::Error` instead of a type error or a silent no-op.
#[derive(Clone)]
pub(crate) enum SessionDriver {
    Sql(Arc<dyn DatabaseDriver>),
    Kv(Arc<dyn KvDriver>),
}

impl SessionDriver {
    /// Borrow the SQL driver, or `None` on a KV (Redis) session. Every
    /// SQL-only command handler (`Query`, `OpenResult`, `Execute`, …) calls
    /// this first and emits `Event::Error` on `None` rather than assuming a
    /// `DatabaseDriver` is always behind the session.
    pub(crate) fn as_sql(&self) -> Option<&Arc<dyn DatabaseDriver>> {
        match self {
            SessionDriver::Sql(d) => Some(d),
            SessionDriver::Kv(_) => None,
        }
    }

    /// Borrow the KV driver, or `None` on a SQL session. Unused until R1's
    /// `Kv*` command handlers land (see docs/plans/redis.md); R0 only needs
    /// `as_sql`, to reject SQL commands on a KV session.
    #[allow(dead_code)]
    pub(crate) fn as_kv(&self) -> Option<&Arc<dyn KvDriver>> {
        match self {
            SessionDriver::Sql(_) => None,
            SessionDriver::Kv(d) => Some(d),
        }
    }

    /// Engine version string, whichever seam this session's driver is behind.
    pub(crate) fn server_version(&self) -> String {
        match self {
            SessionDriver::Sql(d) => d.server_version(),
            SessionDriver::Kv(d) => d.server_version(),
        }
    }
}

/// Backstop cap on open results retained per session. The UI evicts a superseded
/// result (re-sort / filter / tab-close) by sending `CloseResult`, so the live
/// count tracks the user's open tabs, well under this. The cap is defense in
/// depth: if a future UI path ever opens a result without closing its predecessor,
/// the lowest-epoch (oldest) entries are reaped here instead of growing for the
/// session's life. Epochs are process-global and monotonic, so "lowest epoch" is
/// "oldest opened"; but it can belong to any tab, so the cap is set far above any
/// realistic open-tab count to never reap a live result in normal use.
pub(crate) const MAX_OPEN_RESULTS: usize = 256;

/// How long a non-foreground session may sit untouched before it's evicted: its
/// driver is dropped (connection released) and any in-flight work aborted. The
/// foreground session (per `SetActiveSession`) is exempt: it must stay warm
/// however long the user studies a result without scrolling.
pub(crate) const IDLE_EVICT: Duration = Duration::from_secs(600);

/// The cancellable work in flight for one open result. Each detached fetch carries
/// an [`AbortSignal`]; when a newer one supersedes it (a flung scrollbar, a new
/// page, a closed tab) the old signal is [`abort`](AbortSignal::abort)ed so the
/// engine stops the doomed query instead of running it to completion. Held only on
/// the dispatch loop (single-threaded), so no extra lock; the spawned task keeps
/// its own clone and the driver disarms it on completion, making a late abort a
/// no-op.
#[derive(Default)]
pub(crate) struct InFlight {
    /// The `OpenResult` probe bundle (`count` + `fetch_page` + `key_bounds`).
    pub(crate) open: Option<AbortSignal>,
    /// The latest offset `FetchPage` for this epoch.
    pub(crate) page: Option<AbortSignal>,
    /// The latest `FetchRun`, tagged with its `seq` so a stale (lower-seq) run
    /// arriving late doesn't cancel a newer one.
    pub(crate) run: Option<(u64, AbortSignal)>,
    /// The background checkpoint-index build, if one is running.
    pub(crate) build: Option<AbortSignal>,
    /// The latest column-stats summary fetch for this epoch (column-stats bar).
    pub(crate) stats: Option<AbortSignal>,
    /// The latest FK lookup-list fetch for this epoch (in-cell FK picker).
    pub(crate) lookup: Option<AbortSignal>,
    /// The latest `KvFetchScan` for this epoch (Redis keyspace browse); a
    /// fast-retyped filter pattern supersedes the in-flight scan the same
    /// way a flung scrollbar supersedes a SQL page fetch.
    pub(crate) kv_scan: Option<AbortSignal>,
    /// The latest `KvReadValue`/`KvReadCollectionPage`/`KvReadListWindow` for
    /// this epoch (the value inspector): opening a new key, or paging its
    /// big-collection sub-grid, supersedes whatever the inspector was
    /// fetching before.
    pub(crate) kv_value: Option<AbortSignal>,
    /// A live `KvSubscribe` for this epoch (Redis Pub/Sub monitor). Unlike
    /// every other slot here, this one is long-lived by design (it stays
    /// armed for as long as the subscription panel is open) rather than
    /// superseded by a follow-up request; it's torn down by `CloseResult`
    /// when the panel closes.
    pub(crate) kv_subscribe: Option<AbortSignal>,
}

impl InFlight {
    /// Supersede every in-flight fetch for this result (tab closed / reconnected).
    pub(crate) fn abort_all(&self) {
        for sig in [
            self.open.as_ref(),
            self.page.as_ref(),
            self.build.as_ref(),
            self.stats.as_ref(),
            self.lookup.as_ref(),
            self.kv_scan.as_ref(),
            self.kv_value.as_ref(),
            self.kv_subscribe.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            sig.abort();
        }
        if let Some((_, sig)) = &self.run {
            sig.abort();
        }
    }
}

/// The active query's cursor plus the bits needed to drive and abort it.
pub(crate) struct ActiveQuery {
    pub(crate) cursor: Box<dyn QueryCursor>,
    pub(crate) cancel: CancelToken,
    pub(crate) timeout: Option<Duration>,
    pub(crate) streamed: usize,
    pub(crate) started: Instant,
}

/// Everything the backend owns for one keep-alive session: its driver, the
/// streaming cursor (the legacy `Query` path), the open-result map, the per-epoch
/// abort handles, the in-flight export flags, and when it was last touched (for
/// idle eviction). Several of these stay warm at once, keyed by [`SessionId`], so
/// switching between connections is instant: no reconnect, no schema reload.
pub(crate) struct SessionState {
    pub(crate) driver: SessionDriver,
    /// This connection's optional AI policy overrides (M-S7), captured at connect
    /// from its [`ConnectionConfig`](red_core::ConnectionConfig). Layered over the
    /// global `[ai]` policy when a turn runs on this session, so a sensitive
    /// connection can disable the assistant or pin its tier without touching the
    /// global setting.
    pub(crate) ai_override: AiOverride,
    /// The connection's read-only posture, captured at connect. Carried into the AI
    /// policy so the write tool (`AiTier::Write`) is withheld on a read-only
    /// connection, the same guard the human write path is held to.
    pub(crate) read_only: bool,
    /// The SSH tunnel this connection rides, if any. Held only to keep it alive
    /// for the session's lifetime: dropping it (on teardown/eviction) closes the
    /// forward and the SSH session. `None` for a direct connection.
    _tunnel: Option<Tunnel>,
    /// The streaming `Query`/`FetchMore` cursor. Single-active per session; this
    /// path is legacy/test-only (the UI drives results via `OpenResult`).
    pub(crate) active: Option<ActiveQuery>,
    pub(crate) results: ResultMap,
    pub(crate) inflight: HashMap<u64, InFlight>,
    pub(crate) exports: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    /// In-use pin against idle eviction: the number of background jobs currently
    /// reading from or writing to this session. A table copy reads from a session
    /// that is, by definition, *not* the foreground (you copy A→B; at most one side
    /// is on-screen) and may run for many minutes with no commands, so without a
    /// pin its source could be evicted mid-copy (tunnel dropped, connection broken),
    /// since only *commands* bump `last_used`. The copy job increments this on both
    /// ends via a [`PinGuard`] and `evict_idle` skips any session that is foreground
    /// **or** pinned. Lock-free and miscount-proof (the guard decrements on finish,
    /// cancel, or panic). Also closes the latent single-session export-vs-eviction
    /// race for free. Shared `Arc` so a spawned job holds the pin independent of the
    /// session map.
    pub(crate) busy: Arc<AtomicUsize>,
    /// Bumped on every command routed here; idle eviction reads it.
    pub(crate) last_used: Instant,
}

/// RAII pin holding a session's [`busy`](SessionState::busy) counter up while a
/// background job (a table copy) uses it. Increments on construction, decrements on
/// drop, so the pin unwinds correctly on normal finish, cancel, or panic, and a
/// session can never be left pinned by a job that died. A copy holds one of these
/// per end (source + target).
pub(crate) struct PinGuard(Arc<AtomicUsize>);

impl PinGuard {
    pub(crate) fn new(busy: Arc<AtomicUsize>) -> Self {
        busy.fetch_add(1, Ordering::SeqCst);
        PinGuard(busy)
    }
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A connection's optional AI policy overrides (M-S7), carried from its
/// [`ConnectionConfig`](red_core::ConnectionConfig) to the session so a turn can
/// resolve the effective policy. `None` fields inherit the global `[ai]` policy.
#[derive(Clone, Copy, Default)]
pub(crate) struct AiOverride {
    pub(crate) enabled: Option<bool>,
    pub(crate) tier: Option<AiTier>,
}

impl SessionState {
    pub(crate) fn new(
        driver: SessionDriver,
        tunnel: Option<Tunnel>,
        ai_override: AiOverride,
        read_only: bool,
    ) -> Self {
        Self {
            driver,
            ai_override,
            read_only,
            _tunnel: tunnel,
            active: None,
            results: Arc::new(Mutex::new(HashMap::new())),
            inflight: HashMap::new(),
            exports: Arc::new(Mutex::new(HashMap::new())),
            busy: Arc::new(AtomicUsize::new(0)),
            last_used: Instant::now(),
        }
    }

    /// Stop everything in flight at the engine and forget every open result;
    /// the session is being dropped (disconnect / close / eviction).
    pub(crate) fn teardown(&mut self) {
        abort_all_inflight(&mut self.inflight);
        // Signal any streaming exports to stop, so they remove their partial file
        // and release their driver clone rather than streaming on for a session the
        // UI considers gone (each export's per-row check picks the flag up).
        for cancel in lock(&self.exports).values() {
            cancel.store(true, Ordering::Relaxed);
        }
        lock(&self.results).clear();
    }

    /// Backstop GC: if open results exceed [`MAX_OPEN_RESULTS`], reap the
    /// lowest-epoch (oldest-opened) ones, aborting their in-flight fetches, until
    /// back under the cap. A no-op in normal use (the UI closes superseded results);
    /// this only bites if a caller leaks epochs, turning unbounded growth into a
    /// bounded, logged drop. Never touches `keep` (the just-opened epoch).
    pub(crate) fn reap_excess_results(&mut self, keep: u64) {
        let mut results = lock(&self.results);
        while results.len() > MAX_OPEN_RESULTS {
            let Some(victim) = results.keys().copied().filter(|&e| e != keep).min() else {
                break;
            };
            results.remove(&victim);
            if let Some(f) = self.inflight.remove(&victim) {
                f.abort_all();
            }
            tracing::warn!(
                epoch = victim,
                "reaped leaked open result (exceeded MAX_OPEN_RESULTS)"
            );
        }
    }
}

/// The result of a spawned connect/probe, delivered back to the dispatch loop so
/// the (single-threaded) loop owns every mutation of `sessions`. Dialing runs off
/// the loop (see `CONNECT_TIMEOUT` and the `Connect` arm), so one slow connect
/// to a black-hole host can't freeze every other warm session's commands.
pub(crate) enum ConnectOutcome {
    /// A `Connect` finished. `gen` is the connect generation captured when it was
    /// spawned; a stale one (superseded by a newer `Connect` on the same id) is
    /// dropped rather than inserted.
    Session {
        id: SessionId,
        generation: u64,
        /// The connection's AI policy overrides (M-S7), captured at connect so the
        /// resulting session carries them.
        ai_override: AiOverride,
        /// The connection's read-only posture, captured at connect for the AI policy.
        read_only: bool,
        result: Result<(SessionDriver, Option<Tunnel>), ConnectFail>,
    },
    /// A session-less `TestConnection` finished; carries the server version on
    /// success, the error message otherwise.
    Test { result: Result<String, String> },
}

/// A failed connect attempt: the user-facing message plus whether it's `fatal`
/// (user-correctable: bad credentials, missing database) and so should stop the
/// UI's backoff loop rather than schedule another retry. `host_key`, when set,
/// turns the failure into a trustable unknown-SSH-host prompt instead.
pub(crate) struct ConnectFail {
    pub(crate) message: String,
    pub(crate) fatal: bool,
    pub(crate) host_key: Option<HostKeyPrompt>,
}

/// An unknown SSH jump-host key, carried back so the UI can offer "trust & retry".
pub(crate) struct HostKeyPrompt {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) fingerprint: String,
    pub(crate) key: String,
}

/// Apply a spawned connect/probe result on the dispatch loop. Insertion into
/// `sessions` and the `Connected`/`TestSucceeded` emit happen here, never in the
/// spawned task, so the loop stays the single owner of session state.
pub(crate) fn apply_connect_outcome(
    outcome: ConnectOutcome,
    sessions: &mut HashMap<SessionId, SessionState>,
    connect_gen: &HashMap<SessionId, u64>,
    events: &Events,
    ai_acp: &Arc<tokio::sync::Mutex<crate::acp::AcpManager>>,
) {
    match outcome {
        ConnectOutcome::Session {
            id,
            generation,
            ai_override,
            read_only,
            result,
        } => {
            // A newer `Connect` on this id superseded the one that produced this
            // result; drop it so a slow dial can't clobber a fresh session.
            if connect_gen.get(&id).copied() != Some(generation) {
                return;
            }
            match result {
                Ok((driver, tunnel)) => {
                    let version = driver.server_version();
                    sessions.insert(
                        id,
                        SessionState::new(driver, tunnel, ai_override, read_only),
                    );
                    // Evict any ACP conversation still bound to this id: it grounds in
                    // the prior connection's driver. The reconnect already fired an
                    // eviction, but an AI turn spawned just before the reconnect could
                    // insert its conversation *after* that eviction ran (the dial has
                    // since completed, so it has, closing that orphan-on-reconnect
                    // race). A first connect has no such conversation, so this no-ops.
                    let manager = ai_acp.clone();
                    tokio::spawn(async move { manager.lock().await.evict_session(Some(id)) });
                    emit(events, Some(id), Event::Connected { version });
                }
                Err(ConnectFail {
                    host_key: Some(hk), ..
                }) => emit(
                    events,
                    Some(id),
                    Event::SshHostUnknown {
                        host: hk.host,
                        port: hk.port,
                        fingerprint: hk.fingerprint,
                        key: hk.key,
                    },
                ),
                Err(fail) => emit(
                    events,
                    Some(id),
                    Event::ConnectFailed {
                        message: fail.message,
                        fatal: fail.fatal,
                    },
                ),
            }
        }
        ConnectOutcome::Test { result } => match result {
            Ok(version) => emit(events, None, Event::TestSucceeded { version }),
            Err(message) => emit(events, None, Event::TestFailed { message }),
        },
    }
}

/// Drop every session that's been idle past [`IDLE_EVICT`] and isn't the
/// foreground one: abort its in-flight work, release its driver, and tell the UI
/// (`Disconnected`) so it demotes that workspace to a plain recent.
pub(crate) fn evict_idle(
    sessions: &mut HashMap<SessionId, SessionState>,
    foreground: Option<SessionId>,
    events: &Events,
) {
    let now = Instant::now();
    let stale: Vec<SessionId> = sessions
        .iter()
        .filter(|(id, s)| {
            Some(**id) != foreground
                // A background copy pins both ends so its source/target (and their
                // tunnels) survive a multi-minute transfer with no commands.
                && s.busy.load(Ordering::Relaxed) == 0
                && now.duration_since(s.last_used) >= IDLE_EVICT
        })
        .map(|(id, _)| *id)
        .collect();
    for id in stale {
        if let Some(mut state) = sessions.remove(&id) {
            state.teardown();
            tracing::info!(id = id.0, "evicted idle session");
            emit(events, Some(id), Event::Disconnected);
        }
    }
}

/// Abort every in-flight fetch across all open results and clear the registry:
/// the connection is being dropped or replaced, so all of it is doomed work.
pub(crate) fn abort_all_inflight(inflight: &mut HashMap<u64, InFlight>) {
    for (_, f) in inflight.drain() {
        f.abort_all();
    }
}
