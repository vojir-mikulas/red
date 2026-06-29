//! Result paging: the keyset windowed-fetch path, the checkpoint index that keeps
//! deep jumps O(stride), the legacy streaming-cursor driver, and the small
//! SQL-wrapping / timeout helpers the dispatch loop's fetch arms lean on.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use red_core::{KeyKind, KeySpec, RedError, Value};
use red_driver::{AbortSignal, DatabaseDriver, PageCap};
use tokio::sync::mpsc::UnboundedReceiver as CmdReceiver;

use crate::{Command, Envelope, Event, RunFetch, SessionId};

use super::session::ActiveQuery;
use super::{emit, lock, Events};

/// Rows between checkpoints in the index (see [`CheckpointIndex`]). Serving an
/// exact jump seeks to the nearest checkpoint then skips at most this many rows,
/// so it stays O(stride) regardless of depth.
pub(crate) const CHECKPOINT_STRIDE: usize = 10_000;

/// An exact "go to row N" deeper than this kicks off a checkpoint-index build.
/// Shallower jumps are served by a plain `OFFSET` (already fast), so tables
/// nobody jumps deep into are never scanned.
pub(crate) const BUILD_TRIGGER_DEPTH: usize = 100_000;

/// A sparse `ordinal → key` index over an open keyset result: one entry every
/// [`CHECKPOINT_STRIDE`] rows, built by a single background ordered traversal.
/// Lets an exact jump to row N seek to the nearest checkpoint and skip `< stride`
/// rows — O(stride), not O(N). Shared via `Arc<Mutex<…>>` so the build task fills
/// it incrementally while fetches read it.
#[derive(Debug, Default)]
pub(crate) struct CheckpointIndex {
    /// `(ordinal, key tuple)` pairs, ascending by ordinal. `points[0]` is
    /// `(0, first_key)`. The key is the full seek tuple (lead, then tiebreaker).
    pub(crate) points: Vec<(usize, Vec<Value>)>,
    pub(crate) status: BuildStatus,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) enum BuildStatus {
    /// Not yet built (or invalidated by a write) — eligible to start.
    #[default]
    Idle,
    /// A background build is in flight; don't start another.
    Building,
    /// Fully built to the end of the result.
    Done,
}

/// What the backend remembers about one open result: the SQL it re-fetches per
/// page/run, the resolved seek key (+ its column index, for the checkpoint
/// build), the key's min/max and total (for interpolated jumps), and the
/// checkpoint index for exact jumps.
#[derive(Debug, Clone)]
pub(crate) struct OpenSpec {
    pub(crate) sql: String,
    pub(crate) key: Option<KeySpec>,
    /// Positions of the key columns within a result row (lead, then tiebreaker) —
    /// the checkpoint build reads each checkpoint's key tuple out of the row at
    /// these indices. Empty when the result isn't keyset-keyed.
    pub(crate) key_cols: Vec<usize>,
    pub(crate) bounds: Option<(i64, i64)>,
    pub(crate) total: Option<usize>,
    pub(crate) checkpoints: Arc<Mutex<CheckpointIndex>>,
}

/// The open-result map, shared with the spawned open/fetch tasks (they fill in
/// bounds/total after the fact and read specs without round-tripping commands).
pub(crate) type ResultMap = Arc<Mutex<HashMap<u64, OpenSpec>>>;

/// Run one window fetch while staying responsive to `Cancel` / `Shutdown` /
/// timeout. On a full window the cursor is parked back into `active` for the next
/// `FetchMore`; on the last window / cancel / error the cursor is dropped.
/// Returns `true` if a `Shutdown` arrived during the fetch.
pub(crate) async fn drive_fetch(
    aq: ActiveQuery,
    max: usize,
    session: SessionId,
    commands: &mut CmdReceiver<Envelope>,
    events: &Events,
    active: &mut Option<ActiveQuery>,
) -> bool {
    let session = Some(session);
    let mut aq = aq;
    let mut cancelled = false;
    let mut timed_out = false;
    let mut shutdown = false;

    let outcome = {
        let fetch = aq.cursor.next_window(max);
        tokio::pin!(fetch);
        loop {
            tokio::select! {
                res = &mut fetch => break res,
                cmd = commands.recv(), if !shutdown => match cmd {
                    // Legacy single-active streaming path: a `Cancel` (from any
                    // session) drops this cursor; `Shutdown` always stops the loop.
                    Some((_, Command::Cancel)) => { cancelled = true; aq.cancel.cancel(); }
                    Some((_, Command::Shutdown)) | None => { shutdown = true; aq.cancel.cancel(); }
                    // Pull protocol: the consumer awaits each window before
                    // sending the next command, so nothing else lands mid-fetch.
                    _ => {}
                },
                _ = sleep_for(aq.timeout), if !timed_out && aq.timeout.is_some() => {
                    timed_out = true;
                    aq.cancel.cancel();
                }
            }
        }
    };

    match outcome {
        Ok(window) => {
            // An interrupt can land between the last row and the channel reply,
            // so honor a pending cancel/timeout even on an Ok window.
            if shutdown || cancelled {
                emit(events, session, Event::QueryCancelled);
            } else if timed_out {
                emit(events, session, Event::Error(RedError::Timeout.to_string()));
            } else {
                aq.streamed += window.rows.len();
                let done = window.exhausted;
                emit(events, session, Event::QueryRows(window));
                if done {
                    emit(
                        events,
                        session,
                        Event::QueryFinished {
                            rows_streamed: aq.streamed,
                            elapsed: aq.started.elapsed(),
                        },
                    );
                } else {
                    *active = Some(aq);
                }
            }
        }
        Err(RedError::Interrupted) => {
            if timed_out {
                emit(events, session, Event::Error(RedError::Timeout.to_string()));
            } else {
                emit(events, session, Event::QueryCancelled);
            }
        }
        Err(e) => emit(events, session, Event::Error(e.to_string())),
    }

    shutdown
}

/// Serve one `FetchRun` (see [`RunFetch`]). Returns the rows plus whether
/// their ordinals are interpolated estimates (only an interpolated `Jump`).
pub(crate) async fn run_fetch(
    driver: &dyn DatabaseDriver,
    spec: &OpenSpec,
    key: &KeySpec,
    fetch: &RunFetch,
    limit: usize,
    abort: &AbortSignal,
) -> red_core::Result<(Vec<Vec<Value>>, bool)> {
    match fetch {
        RunFetch::Forward { after } => {
            let page = driver
                .fetch_seek(&spec.sql, key, after.as_deref(), false, limit, abort)
                .await?;
            Ok((page.rows, false))
        }
        RunFetch::Backward { before } => {
            let page = driver
                .fetch_seek(&spec.sql, key, Some(before.as_slice()), true, limit, abort)
                .await?;
            Ok((page.rows, false))
        }
        RunFetch::Jump { ordinal, exact } => {
            // Key-space interpolation: land near `ordinal / total` of the key
            // range in one indexed seek. Approximate (exact only for dense,
            // uniform keys) — the grid renders the run's ordinals with a `≈`.
            // Skipped for an exact "go to row N": that wants the precise row, so
            // it falls straight through to the exact `OFFSET` page below.
            // Interpolates on the *lead* column only (a one-element prefix bound),
            // so a composite sort key still gets fraction jumps when its lead is
            // an integer.
            if !exact && key.kind == KeyKind::Int {
                if let (Some((min, max)), Some(total)) = (spec.bounds, spec.total) {
                    if total > 1 && max > min {
                        let fraction = (*ordinal as f64 / (total - 1) as f64).clamp(0.0, 1.0);
                        let span = max as f64 - min as f64;
                        // Ordinal 0 is the result's first row in sort order: the
                        // smallest lead value for an ascending sort, the largest
                        // for a descending one. Seek forward (in sort order) from
                        // just past the target's neighbour so the bound row is
                        // included.
                        let bound = if key.descending {
                            let target =
                                (max as f64 - span * fraction).clamp(min as f64, max as f64);
                            (target as i64).saturating_add(1) // `< t+1` == `<= t`
                        } else {
                            let target =
                                (min as f64 + span * fraction).clamp(min as f64, max as f64);
                            (target as i64).saturating_sub(1) // `> t-1` == `>= t`
                        };
                        let page = driver
                            .fetch_seek(
                                &spec.sql,
                                key,
                                Some(&[Value::Integer(bound)]),
                                false,
                                limit,
                                abort,
                            )
                            .await?;
                        // Jumping to ordinal 0 seeks from the true start — exact.
                        if !page.rows.is_empty() {
                            return Ok((page.rows, *ordinal != 0));
                        }
                        // An empty interpolated window (data shrank underneath)
                        // falls through to the exact `OFFSET` page.
                    }
                }
            }
            // Exact jump: serve from the checkpoint index when it reaches this
            // depth — seek to the nearest checkpoint, then skip `< stride` rows
            // (O(stride), exact). The build is kicked off by the `FetchRun` arm.
            if *exact {
                if let Some((cp_ordinal, cp_key)) = nearest_checkpoint(spec, *ordinal) {
                    let skip = *ordinal - cp_ordinal;
                    if skip <= CHECKPOINT_STRIDE {
                        let page = driver
                            .fetch_seek_skip(&spec.sql, key, Some(&cp_key), skip, limit, abort)
                            .await?;
                        return Ok((page.rows, false));
                    }
                }
            }
            // No usable key/bounds/index: one `OFFSET` page — O(ordinal), but a
            // one-off; ordinals stay exact. Keyed mode, so the key column rides
            // through verbatim — these rows' keys seed the run's seek bounds.
            let page = driver
                .fetch_page(
                    &spec.sql,
                    *ordinal,
                    limit,
                    PageCap::Display {
                        key: Some(key.clone()),
                    },
                    abort,
                )
                .await?;
            Ok((page.rows, false))
        }
    }
}

/// Claim the right to build `spec`'s checkpoint index: true only for a keyset
/// result, a jump deep enough to be worth it, and an index not already built or
/// building. Flips the status to `Building` under the lock so two concurrent deep
/// jumps can't both spawn a build.
pub(crate) fn claim_build(spec: &OpenSpec, ordinal: usize) -> bool {
    if spec.key.is_none() || spec.key_cols.is_empty() || ordinal <= BUILD_TRIGGER_DEPTH {
        return false;
    }
    let mut idx = lock(&spec.checkpoints);
    if idx.status == BuildStatus::Idle {
        idx.status = BuildStatus::Building;
        true
    } else {
        false
    }
}

/// The greatest checkpoint `(ordinal, key tuple)` at or before `target`, if the
/// index has reached that far. Points are ascending, so the last one `<= target`
/// wins.
pub(crate) fn nearest_checkpoint(spec: &OpenSpec, target: usize) -> Option<(usize, Vec<Value>)> {
    let idx = lock(&spec.checkpoints);
    idx.points.iter().rev().find(|(o, _)| *o <= target).cloned()
}

/// Build `spec`'s checkpoint index: walk the result in `CHECKPOINT_STRIDE`-sized
/// strides via an indexed seek + bounded skip, recording one `(ordinal, key tuple)`
/// per stride. One row transfers per checkpoint (just its key columns), so it's a
/// background O(total)-server-work scan with flat memory. Bails if the result closes.
pub(crate) async fn build_checkpoints(
    driver: Arc<dyn DatabaseDriver>,
    spec: OpenSpec,
    results: ResultMap,
    epoch: u64,
    abort: AbortSignal,
) {
    let key = spec.key.clone();
    let (Some(key), false) = (key, spec.key_cols.is_empty()) else {
        lock(&spec.checkpoints).status = BuildStatus::Idle;
        return;
    };
    let key_cols = spec.key_cols.clone();
    let total = spec.total.unwrap_or(0);

    // One checkpoint per stride: reserve the whole index up front when the total
    // is known, so a deep walk doesn't repeatedly grow + copy the points Vec under
    // the lock as it fills (a 100M-row result is ~10k pushes otherwise).
    if total > 0 {
        lock(&spec.checkpoints)
            .points
            .reserve(total / CHECKPOINT_STRIDE + 1);
    }

    // First checkpoint: ordinal 0, seeking from the result's start. Each later
    // step seeks from the previous checkpoint key (inclusive) and skips a stride.
    let mut ordinal = 0usize;
    let mut from: Option<Vec<Value>> = None;
    let mut skip = 0usize;

    loop {
        // The tab closed or re-sorted — abandon the scan.
        if !lock(&results).contains_key(&epoch) {
            return;
        }
        let page = match driver
            .fetch_seek_skip(&spec.sql, &key, from.as_deref(), skip, 1, &abort)
            .await
        {
            Ok(page) => page,
            // A superseded build's in-flight stride comes back interrupted — a
            // clean stop, not a failure; leave the status so a later jump retries.
            Err(RedError::Interrupted) => {
                lock(&spec.checkpoints).status = BuildStatus::Idle;
                return;
            }
            Err(e) => {
                tracing::warn!(%epoch, "checkpoint build failed: {e}");
                lock(&spec.checkpoints).status = BuildStatus::Idle; // allow a later retry
                return;
            }
        };
        let Some(row) = page.rows.first() else {
            break; // walked past the last row
        };
        let cp_key: Vec<Value> = key_cols
            .iter()
            .map(|&c| row.get(c).cloned().unwrap_or(Value::Null))
            .collect();
        lock(&spec.checkpoints)
            .points
            .push((ordinal, cp_key.clone()));

        from = Some(cp_key);
        skip = CHECKPOINT_STRIDE;
        ordinal += CHECKPOINT_STRIDE;
        if total > 0 && ordinal >= total {
            break;
        }
        // Yield so a burst of interactive fetches isn't starved by the scan.
        tokio::task::yield_now().await;
    }
    lock(&spec.checkpoints).status = BuildStatus::Done;
}

/// Wrap a base query in `ORDER BY <position>` for the `OFFSET`-fallback sorted
/// path (a sorted browse with no resolvable PK). Ordering by output *position*
/// is engine-agnostic — it needs no identifier quoting — and the derived table is
/// aliased because MySQL and Postgres both reject an unaliased subquery in `FROM`.
pub(crate) fn wrap_sorted(base: &str, position: usize, descending: bool) -> String {
    let base = base.trim_end().trim_end_matches(';').trim_end();
    format!(
        "SELECT * FROM ({base}) AS _red_sort ORDER BY {position} {}",
        if descending { "DESC" } else { "ASC" }
    )
}

/// Wrap a base query in a filter `WHERE` (Track B2): `SELECT * FROM (base) WHERE
/// (pred)`. `pred` is either a driver-rendered `Contains` predicate (already an
/// OR-chain in parens) or a raw `Where` expression; the wrapping parens contain
/// its precedence. `SELECT *` is preserved, so the keyset key column survives the
/// wrap and paging is unaffected. Trailing `;` is stripped like [`wrap_sorted`].
pub(crate) fn wrap_where(base: &str, pred: &str) -> String {
    let base = base.trim_end().trim_end_matches(';').trim_end();
    format!("SELECT * FROM ({base}) AS _red_filter WHERE ({pred})")
}

/// Lift a result-set [`red_core::Column`] to a [`red_core::ColumnMeta`] for the
/// contains-filter column list, used when filtering editor SQL (no table to
/// introspect). Only the name and declared type matter — `decl_type` lets the
/// predicate skip blob columns; nullability / PK are irrelevant to a text search.
pub(crate) fn col_meta_from_result(c: &red_core::Column) -> red_core::ColumnMeta {
    red_core::ColumnMeta {
        name: c.name.clone(),
        type_name: c.decl_type.clone(),
        not_null: false,
        primary_key: false,
        default: None,
        auto_increment: false,
    }
}

/// A timeout future that never fires when no timeout is set, so the `select!`
/// branch can be a stable shape.
pub(crate) async fn sleep_for(timeout: Option<Duration>) {
    match timeout {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
    }
}

/// Race a one-shot fetch against the statement timeout. On expiry, fire the
/// fetch's [`AbortSignal`] so the engine stops, then surface [`RedError::Timeout`].
/// A `None` timeout never fires; an *externally* aborted fetch (a superseded
/// page/run) keeps its [`RedError::Interrupted`], so the caller stays silent.
pub(crate) async fn with_timeout<T>(
    timeout: Option<Duration>,
    abort: &AbortSignal,
    fut: impl std::future::Future<Output = red_core::Result<T>>,
) -> red_core::Result<T> {
    tokio::pin!(fut);
    let mut timed_out = false;
    let out = loop {
        tokio::select! {
            res = &mut fut => break res,
            _ = sleep_for(timeout), if !timed_out && timeout.is_some() => {
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

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use crate::dispatch::session::{AiOverride, SessionState, MAX_OPEN_RESULTS};
    use red_core::KeyKind;
    use red_driver::SqliteDriver;

    /// Build an `id 1..=n` table in a fresh temp DB and return a driver over it.
    fn driver_with(n: i64, tag: &str) -> (std::path::PathBuf, Arc<dyn DatabaseDriver>) {
        let path = std::env::temp_dir().join(format!("red_ckpt_{tag}_{}.db", std::process::id()));
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE t(x INTEGER PRIMARY KEY NOT NULL);
             WITH RECURSIVE c(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM c WHERE v < {n})
             INSERT INTO t SELECT v FROM c;"
        ))
        .unwrap();
        let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::new(path.clone(), true));
        (path, driver)
    }

    fn spec_for(checkpoints: &Arc<Mutex<CheckpointIndex>>, total: usize) -> OpenSpec {
        OpenSpec {
            sql: "SELECT * FROM t".into(),
            key: Some(KeySpec::single("x", KeyKind::Int)),
            key_cols: vec![0],
            bounds: None,
            total: Some(total),
            checkpoints: checkpoints.clone(),
        }
    }

    /// The build walks the result in `CHECKPOINT_STRIDE` strides, recording the
    /// key at each — and an exact jump is then served from the nearest one.
    #[tokio::test]
    async fn builds_index_and_serves_exact_jump() {
        let (path, driver) = driver_with(25_000, "build");
        let checkpoints = Arc::new(Mutex::new(CheckpointIndex::default()));
        let spec = spec_for(&checkpoints, 25_000);

        // The build aborts unless the result is still open, so register it.
        let results: ResultMap = Arc::new(Mutex::new(HashMap::new()));
        lock(&results).insert(1, spec.clone());

        build_checkpoints(driver.clone(), spec.clone(), results, 1, AbortSignal::new()).await;

        // Checkpoints every 10k rows: ids are 1-based, so ordinal N → id N+1.
        // Scoped so the guard is dropped before the `await` below.
        {
            let idx = lock(&checkpoints);
            assert_eq!(idx.status, BuildStatus::Done);
            assert_eq!(
                idx.points,
                vec![
                    (0, vec![Value::Integer(1)]),
                    (10_000, vec![Value::Integer(10_001)]),
                    (20_000, vec![Value::Integer(20_001)]),
                ]
            );
        }

        // The nearest checkpoint at/under a target, and a bounded-skip serve.
        assert_eq!(
            nearest_checkpoint(&spec, 20_500),
            Some((20_000, vec![Value::Integer(20_001)]))
        );
        let (rows, estimated) = run_fetch(
            &*driver,
            &spec,
            spec.key.as_ref().unwrap(),
            &RunFetch::Jump {
                ordinal: 20_500,
                exact: true,
            },
            5,
            &AbortSignal::new(),
        )
        .await
        .unwrap();
        assert!(!estimated, "an exact jump reports exact ordinals");
        assert_eq!(rows[0][0], Value::Integer(20_501));

        std::fs::remove_file(&path).ok();
    }

    /// `claim_build` only fires for a deep jump on a keyed result, and only once.
    #[tokio::test]
    async fn claim_build_is_deep_and_one_shot() {
        let checkpoints = Arc::new(Mutex::new(CheckpointIndex::default()));
        let spec = spec_for(&checkpoints, 1_000_000);

        // Shallow jumps don't trigger a build (OFFSET is already fast there).
        assert!(!claim_build(&spec, 50));
        // A deep jump claims it once; a second claim is refused (build in flight).
        assert!(claim_build(&spec, 500_000));
        assert!(!claim_build(&spec, 600_000));
        assert_eq!(lock(&checkpoints).status, BuildStatus::Building);
    }

    /// The open-result backstop GC: past [`MAX_OPEN_RESULTS`], reaping drops the
    /// lowest-epoch entries (and their in-flight handles) down to the cap, never
    /// touching the just-opened epoch — so a leaking caller is bounded, not
    /// unbounded. Below the cap it's a no-op.
    #[test]
    fn reap_excess_results_caps_to_the_lowest_epochs() {
        let (path, driver) = driver_with(1, "reap");
        let mut state = SessionState::new(driver, None, AiOverride::default(), false);

        // Open one more than the cap, epochs 1..=MAX+1. Every epoch also has an
        // in-flight handle, so we can assert those are reaped in lockstep.
        let over = MAX_OPEN_RESULTS as u64 + 1;
        for epoch in 1..=over {
            let checkpoints = Arc::new(Mutex::new(CheckpointIndex::default()));
            lock(&state.results).insert(epoch, spec_for(&checkpoints, 1));
            state.inflight.entry(epoch).or_default();
        }
        assert_eq!(lock(&state.results).len(), MAX_OPEN_RESULTS + 1);

        // The just-opened epoch is `over`; reaping keeps it and trims to the cap.
        state.reap_excess_results(over);

        let results = lock(&state.results);
        assert_eq!(results.len(), MAX_OPEN_RESULTS, "trimmed back to the cap");
        assert!(results.contains_key(&over), "the kept epoch survives");
        assert!(!results.contains_key(&1), "the lowest epoch was reaped");
        assert!(
            !state.inflight.contains_key(&1),
            "the reaped epoch's in-flight handle is dropped too"
        );
        assert_eq!(
            state.inflight.len(),
            MAX_OPEN_RESULTS,
            "in-flight map tracks the result map"
        );
        drop(results);

        // Under the cap, reaping is a no-op.
        state.reap_excess_results(over);
        assert_eq!(lock(&state.results).len(), MAX_OPEN_RESULTS);

        std::fs::remove_file(&path).ok();
    }
}
