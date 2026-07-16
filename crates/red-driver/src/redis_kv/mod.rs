//! `KvDriver` over a real Redis/Valkey server. Standalone/Sentinel today;
//! Cluster topology is detected (so the UI can hide the DB-index switch) but
//! its scan fan-out lands with R1 (see `docs/plans/redis.md`).

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use red_core::kv::{
    ClientInfo, CollectionKind, KeyMeta, KvCollection, KvCollectionPage, KvElement, KvMessage,
    KvScanPage, KvStreamPage, KvType, KvValue, OpClass, PendingEntry, RespValue, ScanBudget,
    ScanCursor, SlowlogEntry, StreamConsumer, StreamGroup, StringTtl,
};
use red_core::{RedError, Result, Value};
use redis::aio::MultiplexedConnection;
use tokio::time::Instant;

use crate::AbortSignal;

mod parse;
use crate::kv::{KvDriver, KvMonitorStream, KvSubscription, KvTopology};
mod topology;
use topology::*;
pub use topology::{SentinelMaster, sentinel_masters};

use parse::*;

/// Below this many elements, `read_value` fetches a hash/set/zset/list in
/// full (one round trip); at/above it, only the length is reported and the
/// caller pages the rest (see docs/plans/redis.md's "a few hundred elements"
/// guidance).
const SMALL_COLLECTION_THRESHOLD: u64 = 200;
/// How many times a cluster scan retries the *same* master + cursor on a
/// transient error before giving up on that master. Retrying in place (rather
/// than skipping ahead) is what keeps a brief failover/partition from silently
/// dropping a master's un-scanned keys.
const MAX_SCAN_NODE_RETRIES: u32 = 3;
/// Display cap for a string value preview, mirroring the SQL grid's
/// `Value::Capped` cell cap (`red_driver::DEFAULT_DISPLAY_CELL_CAP`) rather
/// than reusing it directly: a Redis string preview is a one-off inspector
/// fetch, not a per-cell grid budget, so it gets its own constant.
const STRING_PREVIEW_CAP: usize = 8 * 1024;

/// How many leading bytes of a string value a value search reads (`GETRANGE 0
/// CAP-1`). Bounds the cost of scanning values; a match past this offset in a
/// very large value is missed (an accepted limitation for a bounded search).
const VALUE_SEARCH_CAP: usize = 64 * 1024;

/// Hard safety ceiling on a "load full value" ([`read_string_full`]) fetch. The
/// preview cap above keeps the grid/inspector light; this is the much larger
/// ceiling on the *explicit* whole-value load, so a pathological multi-hundred-MB
/// Redis string (they can reach 512 MB) can't be pulled whole into the UI process
/// and OOM it. Past this, the fetch returns a bounded `Value::Capped` prefix
/// carrying the true length, exactly like an over-preview cell — which the edit
/// path already refuses (it only edits whole `Value::Text`), so a too-large value
/// can never be truncated on save. Covers essentially every real value in full;
/// only abusive ones clip.
const STRING_FULL_CAP: usize = 8 * 1024 * 1024;

/// The total number of hash slots in a Redis Cluster (fixed by the protocol).
const CLUSTER_SLOTS: u16 = 16384;

/// Process-global counter for the positional list-delete sentinel, so each
/// `list_remove_at` uses a value that can't collide with a real element (or a
/// sentinel left by a prior, interrupted delete). See [`RedisDriver::list_remove_at`].
static SENTINEL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A live Redis Cluster's per-master fan-out state, discovered at connect via
/// `CLUSTER SLOTS` (see docs/plans/redis.md's cluster fan-out gap). `SCAN` is
/// per-node in a cluster, and single-key commands must reach the node that
/// owns the key's slot, so a plain `MultiplexedConnection` to one seed node
/// (what `RedisDriver::conn` is) only ever sees ~1/N of the keyspace. This
/// holds a dedicated connection to each master plus the slot→master routing
/// table so scans fan out across every shard and reads/writes route to the
/// owning node.
struct ClusterState {
    /// One connection per master, in `CLUSTER SLOTS` discovery order. The
    /// scan walks these in order; `slot_owner` indexes into this vec.
    masters: Vec<MultiplexedConnection>,
    /// `slot_owner[slot]` is the index into `masters` that owns that slot.
    /// Length is exactly [`CLUSTER_SLOTS`]; an unassigned slot (only in a
    /// mid-resharding cluster) maps to master `0` as a best-effort fallback.
    slot_owner: Vec<u16>,
}

/// One Redis/Valkey session. Holds a single [`MultiplexedConnection`]:
/// documented as cheap to clone and safe to use concurrently from multiple
/// clones (it pipelines internally), so `KvDriver`'s `&self` methods clone it
/// per call rather than guarding one instance behind a lock — that would
/// serialize every command through one mutex and defeat the point of a
/// multiplexed connection. `client` is kept alongside it (cheap to clone,
/// just holds parsed connection info) because Pub/Sub needs its own
/// dedicated connection, not the shared multiplexed one.
pub struct RedisDriver {
    client: redis::Client,
    conn: MultiplexedConnection,
    /// `Some` under a `Cluster` topology: per-master connections + slot
    /// routing (see [`ClusterState`]). `None` for standalone/Sentinel, where
    /// `conn` alone sees the whole keyspace.
    cluster: Option<ClusterState>,
    version: String,
    topology: KvTopology,
    read_only: bool,
}

impl RedisDriver {
    /// Dial `dsn` (`redis://[:password@]host:port/db` or `rediss://` for
    /// TLS) and probe `INFO server` to capture the version and topology up
    /// front, the same "fail fast on bad creds, know what we're talking to"
    /// shape as `ClickhouseDriver::connect`'s `fetch_version`.
    ///
    /// A `?master=<name>` query param means `dsn`'s host is a **Sentinel**,
    /// not the data node: the current master's address is resolved through it
    /// first (`SENTINEL get-master-addr-by-name`), then the real connection is
    /// made to that master. Resolving fresh at each connect is how failover is
    /// picked up. This rides the connection string exactly like a pasted
    /// `rediss://` rides TLS today (see docs/plans/redis.md's Sentinel gap),
    /// so no dedicated form field is required.
    pub async fn connect(dsn: &str, read_only: bool) -> Result<Self> {
        let dsn = resolve_sentinel(dsn).await?;
        let dsn = dsn.as_str();
        let client = redis::Client::open(dsn).map_err(|e| RedError::Connect(e.to_string()))?;
        let mut conn = client
            .get_multiplexed_async_connection()
            .await
            .map_err(map_connect_err)?;
        let info: String = redis::cmd("INFO")
            .arg("server")
            .query_async(&mut conn)
            .await
            .map_err(map_connect_err)?;
        let version = info_field(&info, "redis_version").unwrap_or_default();
        let topology = match info_field(&info, "redis_mode").as_deref() {
            Some("cluster") => KvTopology::Cluster,
            Some("sentinel") => KvTopology::Sentinel,
            _ => KvTopology::Standalone,
        };
        // Under a cluster, discover every master up front so scans fan out and
        // single-key ops route to the owning shard (see `ClusterState`). A
        // discovery failure isn't fatal: fall back to the single seed
        // connection (the old "sees one shard" behaviour) rather than refusing
        // to connect at all.
        let cluster = if topology == KvTopology::Cluster {
            match discover_cluster(dsn, &mut conn).await {
                Ok(state) => Some(state),
                Err(e) => {
                    tracing::warn!("Redis cluster discovery failed, using seed node only: {e}");
                    None
                }
            }
        } else {
            None
        };
        Ok(Self {
            client,
            conn,
            cluster,
            version,
            topology,
            read_only,
        })
    }

    /// The connection to run a single-key command on: the master that owns the
    /// key's slot under a cluster, or the sole connection otherwise. Cloning a
    /// `MultiplexedConnection` is cheap (see the struct docs).
    fn route(&self, key: &str) -> MultiplexedConnection {
        match &self.cluster {
            Some(cl) => {
                let slot = key_slot(key) as usize;
                let idx = cl.slot_owner.get(slot).copied().unwrap_or(0) as usize;
                cl.masters.get(idx).unwrap_or(&self.conn).clone()
            }
            None => self.conn.clone(),
        }
    }

    /// Redis has no native read-only connection mode to lean on (unlike
    /// SQLite's `SQLITE_OPEN_READONLY` or Postgres's
    /// `default_transaction_read_only`); every write method checks this
    /// explicitly instead. Distinct from `RedError::Auth`: this isn't a
    /// credentials problem, it's the connection's own configured policy.
    fn check_writable(&self) -> Result<()> {
        crate::refuse_if_read_only(self.read_only)
    }

    /// Value search over a scanned page: keep only string keys whose value
    /// contains `needle` (case-insensitive). Reads each string key's value with
    /// `GETRANGE 0 CAP` (bounded — a match past [`VALUE_SEARCH_CAP`] is missed);
    /// non-string keys are excluded (reading a whole collection per key would be
    /// far too costly). Routed per key so it works standalone and under cluster.
    async fn filter_keys_by_value(
        &self,
        keys: Vec<KeyMeta>,
        needle: &str,
        abort: &AbortSignal,
    ) -> Result<Vec<KeyMeta>> {
        let needle = needle.to_lowercase();
        let mut out = Vec::new();
        for k in keys {
            if abort.is_aborted() {
                return Err(RedError::Interrupted);
            }
            if k.kv_type != KvType::String {
                continue;
            }
            let mut conn = self.route(&k.key);
            // Bytes (not String) so a non-UTF8 value can't fail the read; matched
            // lossily. A vanished/retyped key errors → excluded (`.ok()`).
            let chunk: Option<Vec<u8>> = redis::cmd("GETRANGE")
                .arg(&k.key)
                .arg(0)
                .arg(VALUE_SEARCH_CAP as i64 - 1)
                .query_async(&mut conn)
                .await
                .ok();
            if let Some(bytes) = chunk
                && String::from_utf8_lossy(&bytes)
                    .to_lowercase()
                    .contains(&needle)
            {
                out.push(k);
            }
        }
        Ok(out)
    }

    /// Standalone/Sentinel scan: the budgeted `SCAN` loop on the sole
    /// connection, then one pipelined metadata batch (the original R1 shape).
    async fn scan_standalone(
        &self,
        cursor: ScanCursor,
        pattern: Option<&str>,
        type_filter: Option<&str>,
        budget: ScanBudget,
        abort: &AbortSignal,
    ) -> Result<KvScanPage> {
        let mut cur = match cursor {
            ScanCursor::Single(c) => c,
            // A cluster cursor can't reach here (topology is fixed at
            // connect); treat any stray as "start from the beginning".
            ScanCursor::Cluster { .. } => 0,
        };
        let mut conn = self.conn.clone();
        let deadline = Instant::now() + budget.wall_clock;
        let mut collected: Vec<String> = Vec::new();
        loop {
            if abort.is_aborted() {
                return Err(RedError::Interrupted);
            }
            let (next_cur, batch) =
                scan_once(&mut conn, cur, pattern, type_filter, budget.count_hint).await?;
            cur = next_cur;
            collected.extend(batch);
            // Never truncate mid-batch: a `SCAN` batch's keys are gone from
            // future calls the moment the cursor moves past them, so keeping
            // a whole overshoot batch is the only way not to silently drop
            // keys `SCAN` already handed us.
            if cur == 0 || collected.len() >= budget.want || Instant::now() >= deadline {
                break;
            }
        }
        let exhausted = cur == 0;
        let keys = fetch_key_meta_batch(&mut conn, &collected).await?;
        Ok(KvScanPage {
            keys,
            next_cursor: ScanCursor::Single(cur),
            exhausted,
        })
    }

    /// Cluster scan: walk the masters in order, `SCAN`ning each to exhaustion
    /// before advancing to the next (see docs/plans/redis.md's cluster
    /// fan-out). Metadata is fetched per round trip against the node the keys
    /// came from — every `SCAN`ned key is owned by that node, so its pipelined
    /// `TYPE`/`PTTL`/`OBJECT ENCODING`/`MEMORY USAGE` never crosses a shard.
    /// Position is carried across pages as `ScanCursor::Cluster { node,
    /// cursor }`, so this is stateless per call like the standalone path.
    async fn scan_cluster(
        &self,
        cl: &ClusterState,
        cursor: ScanCursor,
        pattern: Option<&str>,
        type_filter: Option<&str>,
        budget: ScanBudget,
        abort: &AbortSignal,
    ) -> Result<KvScanPage> {
        let (mut node, mut cur) = match cursor {
            ScanCursor::Cluster { node, cursor } => (node as usize, cursor),
            // `START` (or a stray single cursor): begin at the first master.
            ScanCursor::Single(_) => (0, 0),
        };
        let deadline = Instant::now() + budget.wall_clock;
        let mut keys: Vec<KeyMeta> = Vec::new();
        // Retries against the current master at the current cursor. Only reset
        // when we make forward progress (a fully successful round trip) or give
        // up and advance to the next master.
        let mut node_retries = 0u32;
        while node < cl.masters.len() {
            if abort.is_aborted() {
                return Err(RedError::Interrupted);
            }
            let mut conn = cl.masters[node].clone();
            // On a transient error (failover in progress, brief partition, a
            // dropped multiplexed connection) retry the SAME cursor a few times
            // before isolating the master — advancing past it would silently
            // drop the keys it had not yet handed us. Only a master that stays
            // unreachable across every retry is skipped, so the fan-out still
            // survives a genuinely-down shard without failing the whole browse.
            let scanned = scan_once(&mut conn, cur, pattern, type_filter, budget.count_hint).await;
            let (next_cur, batch) = match scanned {
                Ok(v) => v,
                Err(e) => {
                    node_retries += 1;
                    if node_retries <= MAX_SCAN_NODE_RETRIES {
                        tracing::warn!(
                            "Redis cluster scan retry {node_retries} on master {node} at cursor {cur}: {e}"
                        );
                        continue;
                    }
                    tracing::warn!(
                        "Redis cluster scan skipped master {node} after {} retries: {e}",
                        node_retries - 1
                    );
                    node_retries = 0;
                    node += 1;
                    cur = 0;
                    continue;
                }
            };
            if !batch.is_empty() {
                match fetch_key_meta_batch(&mut conn, &batch).await {
                    Ok(metas) => keys.extend(metas),
                    Err(e) => {
                        // Don't advance `cur`: retry this position so the batch
                        // `SCAN` already returned isn't lost to a metadata blip.
                        node_retries += 1;
                        if node_retries <= MAX_SCAN_NODE_RETRIES {
                            tracing::warn!(
                                "Redis cluster metadata retry {node_retries} on master {node}: {e}"
                            );
                            continue;
                        }
                        tracing::warn!(
                            "Redis cluster metadata skipped master {node} after {} retries: {e}",
                            node_retries - 1
                        );
                        node_retries = 0;
                        node += 1;
                        cur = 0;
                        continue;
                    }
                }
            }
            // Forward progress: commit the new cursor and clear the retry count.
            node_retries = 0;
            cur = next_cur;
            // This node exhausted: advance to the next master, its cursor at 0.
            if cur == 0 {
                node += 1;
            }
            if keys.len() >= budget.want || Instant::now() >= deadline {
                break;
            }
        }
        let exhausted = node >= cl.masters.len();
        let next_cursor = ScanCursor::Cluster {
            node: node as u32,
            cursor: cur,
        };
        Ok(KvScanPage {
            keys,
            next_cursor,
            exhausted,
        })
    }
}

/// Redis reports auth/connect failures as generic errors with no stable code
/// to branch on; treat its own error-message vocabulary for bad credentials
/// as user-correctable (stops the UI's retry/backoff loop, like
/// `RedError::Auth` does for the SQL engines), everything else as transient.
///
/// Matches on the crate's own [`redis::ErrorKind`] classification, not the
/// error's `Display` text: a live check against a `--requirepass` server
/// found the actual message ("Password authentication failed -
/// AuthenticationFailed") doesn't contain the RESP error codes (`NOAUTH`,
/// `WRONGPASS`) a naive substring match would look for — the crate already
/// did that classification, so ask it directly instead of re-deriving it.
fn map_connect_err(e: redis::RedisError) -> RedError {
    let msg = e.to_string();
    match e.kind() {
        redis::ErrorKind::AuthenticationFailed => RedError::Auth(msg),
        _ => RedError::Connect(msg),
    }
}

/// Pull one `key:value` line's value out of an `INFO` section's text block.
fn info_field(info: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    info.lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .map(|v| v.trim().to_string())
}

/// One `SCAN` round trip: `SCAN <cursor> COUNT <hint> [MATCH <pattern>] [TYPE
/// <type>]`, returning the next cursor and this batch's keys. The shared inner
/// step of both the standalone and per-node cluster scan loops.
async fn scan_once(
    conn: &mut MultiplexedConnection,
    cursor: u64,
    pattern: Option<&str>,
    type_filter: Option<&str>,
    count_hint: u32,
) -> Result<(u64, Vec<String>)> {
    let mut cmd = redis::cmd("SCAN");
    cmd.arg(cursor).arg("COUNT").arg(count_hint);
    if let Some(p) = pattern {
        cmd.arg("MATCH").arg(p);
    }
    if let Some(t) = type_filter {
        cmd.arg("TYPE").arg(t);
    }
    // Read keys as raw bytes (binary-safe) so one non-UTF-8 key doesn't fail
    // the whole scan; convert lossily for display.
    let (next, raw): (u64, Vec<Vec<u8>>) = cmd
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    Ok((next, raw.into_iter().map(lossy_utf8).collect()))
}

#[async_trait]
impl KvDriver for RedisDriver {
    async fn ping(&self) -> Result<()> {
        let mut conn = self.conn.clone();
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .map_err(|e| RedError::Connect(e.to_string()))?;
        Ok(())
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    fn topology(&self) -> KvTopology {
        self.topology
    }

    async fn db_size(&self) -> Result<u64> {
        // `DBSIZE` is per-node, so a cluster's total is the sum across every
        // master (the seed connection alone would report only its own shard).
        if let Some(cl) = &self.cluster {
            let mut total = 0u64;
            for master in &cl.masters {
                let mut conn = master.clone();
                let n: u64 = redis::cmd("DBSIZE")
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| RedError::Driver(e.to_string()))?;
                total += n;
            }
            return Ok(total);
        }
        let mut conn = self.conn.clone();
        redis::cmd("DBSIZE")
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn scan_keys(
        &self,
        cursor: ScanCursor,
        pattern: Option<&str>,
        type_filter: Option<&str>,
        value_needle: Option<&str>,
        budget: ScanBudget,
        abort: &AbortSignal,
    ) -> Result<KvScanPage> {
        let mut page = match &self.cluster {
            Some(cl) => {
                self.scan_cluster(cl, cursor, pattern, type_filter, budget, abort)
                    .await?
            }
            None => {
                self.scan_standalone(cursor, pattern, type_filter, budget, abort)
                    .await?
            }
        };
        // A value search reads each scanned string key's value and keeps only
        // matches — the cursor already advanced, so a page may shrink (like a
        // MATCH glob), and the caller pages on via `next_cursor` for more.
        if let Some(needle) = value_needle.filter(|n| !n.is_empty()) {
            page.keys = self.filter_keys_by_value(page.keys, needle, abort).await?;
        }
        Ok(page)
    }

    async fn probe_key(&self, key: &str) -> Result<Option<KeyMeta>> {
        let mut conn = self.route(key);
        let keys = fetch_key_meta_batch(&mut conn, std::slice::from_ref(&key.to_string())).await?;
        Ok(keys.into_iter().next())
    }

    async fn read_value(&self, key: &str) -> Result<Option<KvValue>> {
        let mut conn = self.route(key);
        let type_raw: String = redis::cmd("TYPE")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        let Some(kv_type) = KvType::parse(&type_raw) else {
            return Ok(None); // vanished, or never existed
        };
        match kv_type {
            KvType::String => {
                // A `GET` for a key that vanished between `TYPE` and here
                // comes back nil; treat that the same as "doesn't exist".
                let raw: Option<Vec<u8>> = redis::cmd("GET")
                    .arg(key)
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| RedError::Driver(e.to_string()))?;
                Ok(raw.map(|bytes| KvValue::Str(cap_string_value(bytes))))
            }
            KvType::Hash => {
                let collection = load_or_probe(&mut conn, "HLEN", "HGETALL", key, pair_up).await?;
                Ok(Some(KvValue::Hash(collection)))
            }
            KvType::Set => {
                let collection =
                    load_or_probe(&mut conn, "SCARD", "SMEMBERS", key, |v: Vec<String>| v).await?;
                Ok(Some(KvValue::Set(collection)))
            }
            KvType::ZSet => {
                let collection =
                    load_or_probe(&mut conn, "ZCARD", "ZRANGE", key, scored_pairs).await?;
                Ok(Some(KvValue::ZSet(collection)))
            }
            KvType::List => {
                let collection =
                    load_or_probe(&mut conn, "LLEN", "LRANGE", key, |v: Vec<String>| v).await?;
                Ok(Some(KvValue::List(collection)))
            }
            KvType::Stream => {
                // Triage like the other collections: probe the O(1) length,
                // then either load the whole (small) stream newest-first in
                // one `XREVRANGE + -`, or report just the length and let the
                // caller page it back in time via `read_stream_range`.
                let len: u64 = redis::cmd("XLEN")
                    .arg(key)
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| RedError::Driver(e.to_string()))?;
                if len >= SMALL_COLLECTION_THRESHOLD {
                    return Ok(Some(KvValue::Stream(KvCollection::Large { len })));
                }
                let page =
                    fetch_stream_page(&mut conn, key, None, SMALL_COLLECTION_THRESHOLD as usize)
                        .await?;
                Ok(Some(KvValue::Stream(KvCollection::Loaded(page.entries))))
            }
            KvType::Other(_) => Ok(Some(KvValue::Unsupported(kv_type))),
        }
    }

    async fn read_string_full(&self, key: &str) -> Result<Option<Value>> {
        let mut conn = self.route(key);
        // The caller asked for the whole value (unlike `read_value`'s 8 KiB
        // preview), but a Redis string can reach 512 MB; check the length first
        // (`STRLEN`, O(1)) so a pathological value is never GET-ed whole into the
        // process. `STRLEN` is 0 for a missing *or* empty key — both fall into the
        // small path below, where a plain `GET` disambiguates them (`nil` → `None`).
        let len: usize = redis::cmd("STRLEN")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        if len <= STRING_FULL_CAP {
            // Under the ceiling: the whole thing, verbatim. `nil` (key gone / not a
            // string) reads as `None`; a non-UTF-8 body stays exact as a `Blob`.
            let raw: Option<Vec<u8>> = redis::cmd("GET")
                .arg(key)
                .query_async(&mut conn)
                .await
                .map_err(|e| RedError::Driver(e.to_string()))?;
            return Ok(raw.map(|bytes| match String::from_utf8(bytes) {
                Ok(s) => Value::Text(s.into()),
                Err(e) => Value::Blob(e.into_bytes()),
            }));
        }
        // Over the ceiling: read only a bounded prefix (`GETRANGE 0..cap-1`) and
        // return it as a `Value::Capped` carrying the true length, never the tail.
        let window: Vec<u8> = redis::cmd("GETRANGE")
            .arg(key)
            .arg(0)
            .arg(STRING_FULL_CAP as isize - 1)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        // Genuinely binary (an invalid byte, not just a codepoint sliced at the
        // window edge) shows no text head, like a capped blob; large text keeps its
        // prefix. Avoids a multi-MB U+FFFD-expanded head for a huge binary value.
        let blob = matches!(std::str::from_utf8(&window), Err(e) if e.error_len().is_some());
        let head = if blob {
            String::new()
        } else {
            String::from_utf8_lossy(&window).into_owned()
        };
        Ok(Some(Value::Capped(Box::new(red_core::CappedCell {
            head,
            len,
            blob,
        }))))
    }

    async fn read_collection_page(
        &self,
        key: &str,
        kind: CollectionKind,
        cursor: u64,
        budget: ScanBudget,
        abort: &AbortSignal,
    ) -> Result<KvCollectionPage> {
        let mut conn = self.route(key);
        let cmd_name = match kind {
            CollectionKind::Hash => "HSCAN",
            CollectionKind::Set => "SSCAN",
            CollectionKind::ZSet => "ZSCAN",
        };
        let deadline = Instant::now() + budget.wall_clock;
        let mut cur = cursor;
        let mut elements = Vec::new();
        loop {
            if abort.is_aborted() {
                return Err(RedError::Interrupted);
            }
            let mut cmd = redis::cmd(cmd_name);
            cmd.arg(key).arg(cur).arg("COUNT").arg(budget.count_hint);
            // Binary-safe: decode raw bytes so a non-UTF-8 member/field doesn't
            // fail the page (see [`lossy_utf8`]).
            let (next_cur, raw): (u64, Vec<Vec<u8>>) = cmd
                .query_async(&mut conn)
                .await
                .map_err(|e| RedError::Driver(e.to_string()))?;
            let flat: Vec<String> = raw.into_iter().map(lossy_utf8).collect();
            cur = next_cur;
            match kind {
                CollectionKind::Set => elements.extend(flat.into_iter().map(KvElement::Member)),
                CollectionKind::Hash => elements.extend(
                    pair_up(flat)
                        .into_iter()
                        .map(|(f, v)| KvElement::Field(f, v)),
                ),
                CollectionKind::ZSet => elements.extend(
                    scored_pairs(flat)
                        .into_iter()
                        .map(|(m, s)| KvElement::Scored(m, s)),
                ),
            }
            if cur == 0 || elements.len() >= budget.want || Instant::now() >= deadline {
                break;
            }
        }
        let exhausted = cur == 0;
        Ok(KvCollectionPage {
            elements,
            next_cursor: cur,
            exhausted,
        })
    }

    async fn read_list_window(
        &self,
        key: &str,
        from_head: bool,
        count: usize,
    ) -> Result<Vec<String>> {
        let mut conn = self.route(key);
        let count = count.max(1) as i64;
        let (start, stop): (i64, i64) = if from_head {
            (0, count - 1)
        } else {
            (-count, -1)
        };
        // Binary-safe: decode raw bytes so a non-UTF-8 list item doesn't fail
        // the window (see [`lossy_utf8`]).
        let raw: Vec<Vec<u8>> = redis::cmd("LRANGE")
            .arg(key)
            .arg(start)
            .arg(stop)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        Ok(raw.into_iter().map(lossy_utf8).collect())
    }

    async fn read_stream_range(
        &self,
        key: &str,
        before: Option<&str>,
        count: usize,
    ) -> Result<KvStreamPage> {
        let mut conn = self.route(key);
        fetch_stream_page(&mut conn, key, before, count).await
    }

    async fn stream_groups(&self, key: &str) -> Result<Vec<StreamGroup>> {
        let mut conn = self.route(key);
        let reply: redis::Value = redis::cmd("XINFO")
            .arg("GROUPS")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        Ok(parse_stream_groups(&reply))
    }

    async fn stream_consumers(&self, key: &str, group: &str) -> Result<Vec<StreamConsumer>> {
        let mut conn = self.route(key);
        let reply: redis::Value = redis::cmd("XINFO")
            .arg("CONSUMERS")
            .arg(key)
            .arg(group)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        Ok(parse_stream_consumers(&reply))
    }

    async fn stream_pending(
        &self,
        key: &str,
        group: &str,
        count: usize,
    ) -> Result<Vec<PendingEntry>> {
        let mut conn = self.route(key);
        let reply: redis::Value = redis::cmd("XPENDING")
            .arg(key)
            .arg(group)
            .arg("-")
            .arg("+")
            .arg(count.max(1))
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        Ok(parse_pending_entries(&reply))
    }

    async fn stream_ack(&self, key: &str, group: &str, ids: &[String]) -> Result<u64> {
        self.check_writable()?;
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.route(key);
        let mut cmd = redis::cmd("XACK");
        cmd.arg(key).arg(group);
        for id in ids {
            cmd.arg(id);
        }
        cmd.query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn stream_claim(
        &self,
        key: &str,
        group: &str,
        consumer: &str,
        min_idle: Duration,
        ids: &[String],
    ) -> Result<u64> {
        self.check_writable()?;
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.route(key);
        let mut cmd = redis::cmd("XCLAIM");
        cmd.arg(key)
            .arg(group)
            .arg(consumer)
            .arg(min_idle.as_millis() as u64);
        for id in ids {
            cmd.arg(id);
        }
        // `JUSTID` returns just the claimed IDs (no field/value payload), which
        // is all the count needs and avoids materializing entry bodies. It also
        // stops `XCLAIM` bumping the delivery counter, matching a plain reclaim.
        cmd.arg("JUSTID");
        let claimed: Vec<String> = cmd
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        Ok(claimed.len() as u64)
    }

    async fn stream_add(&self, key: &str, fields: &[(String, String)]) -> Result<String> {
        self.check_writable()?;
        if fields.is_empty() {
            return Err(RedError::Query("a stream entry needs a field".into()));
        }
        let mut conn = self.route(key);
        // `*` = server-assigned id (monotonic `<ms>-<seq>`).
        let mut cmd = redis::cmd("XADD");
        cmd.arg(key).arg("*");
        for (field, value) in fields {
            cmd.arg(field).arg(value);
        }
        cmd.query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn command(&self, argv: &[String]) -> Result<RespValue> {
        let Some(name) = argv.first() else {
            return Err(RedError::Query("empty command".into()));
        };
        if self.read_only && red_core::kv::classify_command(argv) != OpClass::Read {
            self.check_writable()?;
        }
        let mut cmd = redis::cmd(name);
        for arg in &argv[1..] {
            cmd.arg(arg);
        }
        // Cluster fan-out: a keyless whole-node command (FLUSHALL, FLUSHDB,
        // SWAPDB, SCRIPT/FUNCTION FLUSH) only affects the node it lands on. On a
        // cluster the user means it cluster-wide, so run it on every master and
        // aggregate rather than silently clearing just the seed shard and still
        // reporting OK.
        if let Some(cl) = &self.cluster
            && is_cluster_fanout_command(argv)
        {
            let mut last = RespValue::Simple("OK".into());
            for master in &cl.masters {
                let mut conn = master.clone();
                match cmd.query_async::<redis::Value>(&mut conn).await {
                    Ok(value) => last = to_resp_value(value),
                    Err(e) if e.code().is_some() => return Ok(RespValue::Error(e.to_string())),
                    Err(e) => return Err(RedError::Driver(e.to_string())),
                }
            }
            return Ok(last);
        }
        // Under a cluster the seed connection only owns ~1/N of the slots and a
        // plain `MultiplexedConnection` doesn't follow `MOVED`, so a console
        // command for a key on another shard would come back as a raw `MOVED`
        // line. Route by the command's key (best-effort: `argv[1]`, where the
        // key sits for the overwhelming majority of commands) the same way the
        // single-key methods do; keyless commands fall back to the seed and run
        // on any node. A stale slot map mid-reshard can still surface `MOVED`,
        // matching the single-key paths.
        let mut conn = match &self.cluster {
            Some(_) => match argv.get(1) {
                Some(key) => self.route(key),
                None => self.conn.clone(),
            },
            None => self.conn.clone(),
        };
        match cmd.query_async::<redis::Value>(&mut conn).await {
            Ok(value) => Ok(to_resp_value(value)),
            // A server-reported command error (WRONGTYPE, a bad arity, an
            // unknown subcommand, ...) is normal console output, like
            // `redis-cli`'s `(error) ...` line, not a connection failure —
            // redis-rs surfaces both as `Err`. `code()` is `Some` exactly
            // when the error carries a RESP error code from the server (even
            // an unrecognized one; `kind()` alone isn't enough here, since it
            // only maps *recognized* codes to `ErrorKind::Server` and falls
            // back to `Extension` for anything else, WRONGTYPE included).
            // Anything with no code is a genuine transport/connection error.
            Err(e) if e.code().is_some() => Ok(RespValue::Error(e.to_string())),
            Err(e) => Err(RedError::Driver(e.to_string())),
        }
    }

    async fn set_string(&self, key: &str, value: String, ttl: StringTtl) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.route(key);
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value);
        match ttl {
            // `KEEPTTL` retains the server's actual remaining expiry exactly, so
            // editing a value never resets the countdown (nor rounds a
            // sub-second remainder up to a whole second, as re-applying an `EX`
            // snapshot would).
            StringTtl::Keep => {
                cmd.arg("KEEPTTL");
            }
            // Plain `SET` clears any expiry — the right default for a new key.
            StringTtl::Clear => {}
            // `PX` (milliseconds), not `EX`, so an explicit sub-second TTL keeps
            // its precision instead of being floored to 1s.
            StringTtl::Set(d) => {
                cmd.arg("PX").arg((d.as_millis() as u64).max(1));
            }
        }
        cmd.query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn set_field(&self, key: &str, field: &str, value: String) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.route(key);
        redis::cmd("HSET")
            .arg(key)
            .arg(field)
            .arg(value)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn hash_delete(&self, key: &str, fields: &[String]) -> Result<u64> {
        self.check_writable()?;
        if fields.is_empty() {
            return Ok(0);
        }
        let mut conn = self.route(key);
        let mut cmd = redis::cmd("HDEL");
        cmd.arg(key);
        for f in fields {
            cmd.arg(f);
        }
        cmd.query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn set_add(&self, key: &str, members: &[String]) -> Result<u64> {
        self.check_writable()?;
        if members.is_empty() {
            return Ok(0);
        }
        let mut conn = self.route(key);
        let mut cmd = redis::cmd("SADD");
        cmd.arg(key);
        for m in members {
            cmd.arg(m);
        }
        cmd.query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn set_remove(&self, key: &str, members: &[String]) -> Result<u64> {
        self.check_writable()?;
        if members.is_empty() {
            return Ok(0);
        }
        let mut conn = self.route(key);
        let mut cmd = redis::cmd("SREM");
        cmd.arg(key);
        for m in members {
            cmd.arg(m);
        }
        cmd.query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn set_replace(&self, key: &str, old: &str, new: &str) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.route(key);
        // One `MULTI`/`EXEC` so the remove+add commit together: a failure
        // between them can't leave the set missing both members. Both commands
        // touch the same `key` (same slot), so this is valid under a cluster.
        redis::pipe()
            .atomic()
            .cmd("SREM")
            .arg(key)
            .arg(old)
            .ignore()
            .cmd("SADD")
            .arg(key)
            .arg(new)
            .ignore()
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn zset_add(&self, key: &str, member: &str, score: f64) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.route(key);
        redis::cmd("ZADD")
            .arg(key)
            .arg(score)
            .arg(member)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn zset_remove(&self, key: &str, members: &[String]) -> Result<u64> {
        self.check_writable()?;
        if members.is_empty() {
            return Ok(0);
        }
        let mut conn = self.route(key);
        let mut cmd = redis::cmd("ZREM");
        cmd.arg(key);
        for m in members {
            cmd.arg(m);
        }
        cmd.query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn list_set(&self, key: &str, index: i64, value: String) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.route(key);
        redis::cmd("LSET")
            .arg(key)
            .arg(index)
            .arg(value)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn list_push(&self, key: &str, value: String, head: bool) -> Result<u64> {
        self.check_writable()?;
        let mut conn = self.route(key);
        redis::cmd(if head { "LPUSH" } else { "RPUSH" })
            .arg(key)
            .arg(value)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn list_remove(&self, key: &str, count: i64, value: String) -> Result<u64> {
        self.check_writable()?;
        let mut conn = self.route(key);
        redis::cmd("LREM")
            .arg(key)
            .arg(count)
            .arg(value)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn list_remove_at(&self, key: &str, index: i64) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.route(key);
        // Atomic positional delete: LSET a unique sentinel at `index`, then LREM
        // it, wrapped in one MULTI/EXEC so no concurrent client observes the
        // sentinel or races between the two commands. An out-of-range index
        // surfaces Redis's own error and aborts the transaction. The nonce keeps
        // the sentinel from colliding with a real element even across calls.
        let nonce = SENTINEL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sentinel = format!("\u{0}__red_del_{index}_{nonce}__\u{0}");
        redis::pipe()
            .atomic()
            .cmd("LSET")
            .arg(key)
            .arg(index)
            .arg(&sentinel)
            .ignore()
            .cmd("LREM")
            .arg(key)
            .arg(1)
            .arg(&sentinel)
            .ignore()
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn set_ttl(&self, key: &str, ttl: Option<Duration>) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.route(key);
        match ttl {
            // `PEXPIRE` (milliseconds), not `EXPIRE`, so a sub-second TTL keeps
            // its precision instead of being floored to a whole second.
            Some(ttl) => redis::cmd("PEXPIRE")
                .arg(key)
                .arg((ttl.as_millis() as u64).max(1))
                .query_async(&mut conn)
                .await
                .map_err(|e| RedError::Driver(e.to_string())),
            None => redis::cmd("PERSIST")
                .arg(key)
                .query_async(&mut conn)
                .await
                .map_err(|e| RedError::Driver(e.to_string())),
        }
    }

    async fn rename_key(&self, from: &str, to: &str) -> Result<()> {
        self.check_writable()?;
        // Route by `from`; under a cluster, `RENAME` requires both keys in the
        // same slot, so a cross-slot rename surfaces Redis's own `CROSSSLOT`
        // error (an inherent cluster constraint, not something to paper over).
        let mut conn = self.route(from);
        redis::cmd("RENAME")
            .arg(from)
            .arg(to)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn delete_keys(&self, keys: &[String]) -> Result<u64> {
        self.check_writable()?;
        if keys.is_empty() {
            return Ok(0);
        }
        // `DEL` is a multi-key command; under a cluster a multi-key `DEL`
        // whose keys span slots is a `CROSSSLOT` error — and Redis checks the
        // *slot*, not the owning node, so even keys on one master but in
        // different slots are rejected. Group the selection by owning master
        // and send each master a pipeline of *single-key* `DEL`s (each is
        // trivially same-slot): one round trip per node, no CROSSSLOT.
        if let Some(cl) = &self.cluster {
            let mut by_master: std::collections::HashMap<u16, Vec<&String>> =
                std::collections::HashMap::new();
            for k in keys {
                let slot = key_slot(k) as usize;
                let idx = cl.slot_owner.get(slot).copied().unwrap_or(0);
                by_master.entry(idx).or_default().push(k);
            }
            let mut deleted = 0u64;
            for (idx, group) in by_master {
                let Some(master) = cl.masters.get(idx as usize) else {
                    continue;
                };
                let mut conn = master.clone();
                let mut pipe = redis::pipe();
                for k in group {
                    pipe.cmd("DEL").arg(k);
                }
                let counts: Vec<u64> = pipe
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| RedError::Driver(e.to_string()))?;
                deleted += counts.iter().sum::<u64>();
            }
            return Ok(deleted);
        }
        let mut conn = self.conn.clone();
        let mut cmd = redis::cmd("DEL");
        for k in keys {
            cmd.arg(k);
        }
        cmd.query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn dump_key(&self, key: &str) -> Result<Option<(Vec<u8>, Option<Duration>)>> {
        let mut conn = self.route(key);
        // DUMP + PTTL in one pipeline: the serialized value and its remaining
        // expiry, atomic enough for a snapshot (a value edit between the two is
        // a non-issue — undo restores whatever DUMP saw).
        let (payload, pttl): (Option<Vec<u8>>, i64) = redis::pipe()
            .cmd("DUMP")
            .arg(key)
            .cmd("PTTL")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        // A missing key DUMPs to nil — nothing to recycle.
        let Some(payload) = payload else {
            return Ok(None);
        };
        // PTTL: -1 = no expiry, -2 = key gone (treat as no expiry).
        let ttl = (pttl > 0).then(|| Duration::from_millis(pttl as u64));
        Ok(Some((payload, ttl)))
    }

    async fn restore_key(
        &self,
        key: &str,
        ttl: Option<Duration>,
        payload: &[u8],
        replace: bool,
    ) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.route(key);
        // RESTORE's ttl is milliseconds, 0 = no expiry. `REPLACE` overwrites an
        // existing key (a copy); without it a BUSYKEY error surfaces (an undo).
        let ttl_ms = ttl.map(|d| d.as_millis() as u64).unwrap_or(0);
        let mut cmd = redis::cmd("RESTORE");
        cmd.arg(key).arg(ttl_ms).arg(payload);
        if replace {
            cmd.arg("REPLACE");
        }
        cmd.query_async::<()>(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn slowlog(&self, count: usize) -> Result<Vec<SlowlogEntry>> {
        let mut conn = self.conn.clone();
        let reply: redis::Value = redis::cmd("SLOWLOG")
            .arg("GET")
            .arg(count.max(1))
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        Ok(parse_slowlog(&reply))
    }

    async fn slowlog_reset(&self) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.conn.clone();
        redis::cmd("SLOWLOG")
            .arg("RESET")
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn client_list(&self) -> Result<Vec<ClientInfo>> {
        let mut conn = self.conn.clone();
        // `CLIENT LIST` replies as one bulk string, one client per line. Decode
        // as bytes then lossily, so a non-UTF8 client name can't fail the read.
        let raw: Vec<u8> = redis::cmd("CLIENT")
            .arg("LIST")
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        let text = String::from_utf8_lossy(&raw);
        Ok(red_core::kv::parse_client_list(&text))
    }

    async fn client_kill(&self, id: i64) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.conn.clone();
        // `CLIENT KILL ID <id>` returns the number killed (0 if the id is
        // already gone); either way the caller just refreshes the list.
        redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(id)
            .query_async::<i64>(&mut conn)
            .await
            .map(|_| ())
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn monitor(&self) -> Result<KvMonitorStream> {
        // MONITOR monopolizes its connection (the server pushes every command
        // and accepts nothing back), so it gets its own dedicated one, exactly
        // like `subscribe` does for Pub/Sub — never the shared multiplexed
        // `conn`. Each MONITOR item decodes as a preformatted status line.
        let monitor = self
            .client
            .get_async_monitor()
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        let stream = monitor.into_on_message::<String>();
        Ok(KvMonitorStream {
            stream: Box::pin(stream),
        })
    }

    async fn notify_config(&self) -> Result<String> {
        let mut conn = self.conn.clone();
        // `CONFIG GET notify-keyspace-events` replies as `[name, value]`; the
        // value may be empty (notifications off). Decode as a flat vec so a
        // missing value degrades to "off" rather than erroring.
        let pair: Vec<String> = redis::cmd("CONFIG")
            .arg("GET")
            .arg("notify-keyspace-events")
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        Ok(pair.into_iter().nth(1).unwrap_or_default())
    }

    async fn set_notify_config(&self, flags: &str) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.conn.clone();
        redis::cmd("CONFIG")
            .arg("SET")
            .arg("notify-keyspace-events")
            .arg(flags)
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn subscribe(&self, pattern: &str) -> Result<KvSubscription> {
        let mut pubsub = self
            .client
            .get_async_pubsub()
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        pubsub
            .psubscribe(pattern)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        let stream = pubsub.into_on_message().map(|msg| {
            let channel = msg.get_channel_name().to_string();
            let payload: String = msg.get_payload().unwrap_or_default();
            KvMessage { channel, payload }
        });
        Ok(KvSubscription {
            stream: Box::pin(stream),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cluster_fanout_covers_keyless_whole_node_commands() {
        // Keyless whole-node commands fan out cluster-wide.
        assert!(is_cluster_fanout_command(&argv(&["FLUSHALL"])));
        assert!(is_cluster_fanout_command(&argv(&["flushdb"])));
        assert!(is_cluster_fanout_command(&argv(&["SWAPDB", "0", "1"])));
        assert!(is_cluster_fanout_command(&argv(&["SCRIPT", "FLUSH"])));
        assert!(is_cluster_fanout_command(&argv(&["function", "flush"])));
        // Keyed / non-flush commands route normally (must NOT fan out).
        assert!(!is_cluster_fanout_command(&argv(&["DEL", "k"])));
        assert!(!is_cluster_fanout_command(&argv(&["GET", "k"])));
        assert!(!is_cluster_fanout_command(&argv(&["SCRIPT", "LOAD", "x"])));
        assert!(!is_cluster_fanout_command(&argv(&["INFO"])));
        assert!(!is_cluster_fanout_command(&[]));
    }

    #[test]
    fn info_field_reads_a_known_key() {
        let info = "# Server\r\nredis_version:7.4.0\r\nredis_mode:standalone\r\nrun_id:abc\r\n";
        assert_eq!(info_field(info, "redis_version").as_deref(), Some("7.4.0"));
        assert_eq!(
            info_field(info, "redis_mode").as_deref(),
            Some("standalone")
        );
        assert_eq!(info_field(info, "missing"), None);
    }

    // Live test against a real server, provided via `RED_TEST_REDIS_URL`, so CI
    // without one skips cleanly (mirrors clickhouse.rs/mysql.rs/postgres.rs).
    // Spin one up with:
    //
    //   docker run --rm -d -p 6399:6379 --name red-redis redis:7
    //   export RED_TEST_REDIS_URL='redis://127.0.0.1:6399/0'

    fn test_url() -> Option<String> {
        std::env::var("RED_TEST_REDIS_URL").ok()
    }

    macro_rules! url_or_skip {
        () => {
            match test_url() {
                Some(u) => u,
                None => {
                    eprintln!("SKIP {}: RED_TEST_REDIS_URL not set", module_path!());
                    return;
                }
            }
        };
    }

    #[tokio::test]
    async fn connect_reports_version_and_topology() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, true).await.unwrap();
        assert!(!driver.server_version().is_empty());
        assert_eq!(driver.topology(), KvTopology::Standalone);
        driver.ping().await.unwrap();
    }

    #[tokio::test]
    async fn db_size_is_a_count() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, true).await.unwrap();
        // No assertion on the exact count (a shared server may hold other
        // tests' keys); just that the round-trip works end to end.
        driver.db_size().await.unwrap();
    }

    #[test]
    fn map_connect_err_classifies_bad_auth_as_fatal() {
        let noauth = redis::RedisError::from((
            redis::ErrorKind::AuthenticationFailed,
            "NOAUTH Authentication required.",
        ));
        assert!(matches!(map_connect_err(noauth), RedError::Auth(_)));
    }

    #[test]
    fn crc16_matches_the_xmodem_check_value() {
        // The canonical CRC16/XMODEM check value: CRC of b"123456789" is
        // 0x31C3. Getting this right is what makes `key_slot` agree with the
        // server's own `CLUSTER KEYSLOT`.
        assert_eq!(crc16(b"123456789"), 0x31C3);
    }

    #[test]
    fn key_slot_honors_hash_tags() {
        // A `{tag}` co-locates keys: two keys sharing `{user1000}` map to the
        // same slot, and both equal the slot of the bare tag.
        let a = key_slot("{user1000}.following");
        let b = key_slot("{user1000}.followers");
        assert_eq!(a, b);
        assert_eq!(a, key_slot("user1000"));
        // An empty tag `{}` is ignored (the whole key hashes).
        assert_ne!(key_slot("{}.a"), key_slot("{}.b"));
        // Every slot is in range.
        assert!(key_slot("anything") < CLUSTER_SLOTS);
    }

    #[test]
    fn parse_cluster_slots_extracts_masters_and_ranges() {
        use redis::Value;
        // Shape of a real `CLUSTER SLOTS` reply: [start, end, [ip, port, id],
        // <replicas...>]. The replica entries after the master are ignored.
        let reply = Value::Array(vec![Value::Array(vec![
            Value::Int(0),
            Value::Int(5460),
            Value::Array(vec![
                Value::BulkString(b"127.0.0.1".to_vec()),
                Value::Int(7100),
                Value::BulkString(b"nodeid".to_vec()),
            ]),
            Value::Array(vec![
                Value::BulkString(b"127.0.0.1".to_vec()),
                Value::Int(7103),
                Value::BulkString(b"replicaid".to_vec()),
            ]),
        ])]);
        let ranges = parse_cluster_slots(&reply);
        assert_eq!(
            ranges,
            vec![(0u16, 5460u16, "127.0.0.1".to_string(), 7100u16)]
        );
    }

    #[test]
    fn parse_sentinel_masters_pulls_name_and_address() {
        use redis::Value;
        let bulk = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        // A `SENTINEL MASTERS` reply: one master, a flat field/value map.
        let reply = Value::Array(vec![Value::Array(vec![
            bulk("name"),
            bulk("mymaster"),
            bulk("ip"),
            bulk("127.0.0.1"),
            bulk("port"),
            bulk("6379"),
            bulk("flags"),
            bulk("master"),
        ])]);
        let masters = parse_sentinel_masters(&reply);
        assert_eq!(
            masters,
            vec![SentinelMaster {
                name: "mymaster".into(),
                host: "127.0.0.1".into(),
                port: 6379,
            }]
        );
    }

    #[test]
    fn node_dsn_swaps_host_and_port_preserving_credentials_and_scheme() {
        // Credentials and the TLS scheme survive; only the authority changes.
        let out = node_dsn("rediss://:secret@seed.example:6379/0", "10.0.0.5", 7002).unwrap();
        assert!(out.starts_with("rediss://:secret@10.0.0.5:7002/"), "{out}");
        // An empty host (CLUSTER SLOTS' "connect address" sentinel) keeps the
        // seed host, changing only the port.
        let out = node_dsn("redis://seed.example:6379", "", 7001).unwrap();
        assert!(out.contains("seed.example:7001"), "{out}");
    }

    #[test]
    fn parse_stream_groups_reads_the_resp2_flat_map() {
        use redis::Value;
        let bulk = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        // One group, RESP2 flat `[field, value, ...]` shape. `lag` present.
        let reply = Value::Array(vec![Value::Array(vec![
            bulk("name"),
            bulk("g1"),
            bulk("consumers"),
            Value::Int(2),
            bulk("pending"),
            Value::Int(5),
            bulk("last-delivered-id"),
            bulk("1526569495631-0"),
            bulk("entries-read"),
            Value::Int(10),
            bulk("lag"),
            Value::Int(3),
        ])]);
        assert_eq!(
            parse_stream_groups(&reply),
            vec![StreamGroup {
                name: "g1".into(),
                consumers: 2,
                pending: 5,
                last_delivered_id: "1526569495631-0".into(),
                lag: Some(3),
            }]
        );
    }

    #[test]
    fn parse_stream_groups_tolerates_nil_lag_and_the_resp3_map_shape() {
        use redis::Value;
        let bulk = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        // RESP3 `Map` shape, and `lag` reported as nil (a trimmed stream).
        let reply = Value::Array(vec![Value::Map(vec![
            (bulk("name"), bulk("g2")),
            (bulk("consumers"), Value::Int(1)),
            (bulk("pending"), Value::Int(0)),
            (bulk("last-delivered-id"), bulk("0-0")),
            (bulk("lag"), Value::Nil),
        ])]);
        let groups = parse_stream_groups(&reply);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "g2");
        assert_eq!(groups[0].lag, None);
    }

    #[test]
    fn parse_pending_entries_reads_the_extended_rows() {
        use redis::Value;
        let bulk = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        let reply = Value::Array(vec![
            Value::Array(vec![
                bulk("1526569498055-0"),
                bulk("consumer-1"),
                Value::Int(1200),
                Value::Int(2),
            ]),
            // A torn row (too few fields) is dropped, not fatal.
            Value::Array(vec![bulk("bad")]),
        ]);
        assert_eq!(
            parse_pending_entries(&reply),
            vec![PendingEntry {
                id: "1526569498055-0".into(),
                consumer: "consumer-1".into(),
                idle: Duration::from_millis(1200),
                delivery_count: 2,
            }]
        );
    }

    #[test]
    fn parse_stream_consumers_reads_name_pending_and_idle() {
        use redis::Value;
        let bulk = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        let reply = Value::Array(vec![Value::Array(vec![
            bulk("name"),
            bulk("consumer-1"),
            bulk("pending"),
            Value::Int(4),
            bulk("idle"),
            Value::Int(9000),
        ])]);
        assert_eq!(
            parse_stream_consumers(&reply),
            vec![StreamConsumer {
                name: "consumer-1".into(),
                pending: 4,
                idle: Duration::from_millis(9000),
            }]
        );
    }

    #[test]
    fn parse_slowlog_reads_entries_with_and_without_client_fields() {
        use redis::Value;
        let bulk = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        let reply = Value::Array(vec![
            // Redis 4+ shape: id, ts, micros, argv, client_addr, client_name.
            Value::Array(vec![
                Value::Int(3),
                Value::Int(1_700_000_000),
                Value::Int(15000),
                Value::Array(vec![bulk("GET"), bulk("big:key")]),
                bulk("127.0.0.1:52814"),
                bulk("worker"),
            ]),
            // Legacy 4-field shape (no client info).
            Value::Array(vec![
                Value::Int(2),
                Value::Int(1_699_999_000),
                Value::Int(9000),
                Value::Array(vec![bulk("KEYS"), bulk("*")]),
            ]),
            // Torn entry: dropped, not fatal.
            Value::Array(vec![Value::Int(1)]),
        ]);
        let entries = parse_slowlog(&reply);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            SlowlogEntry {
                id: 3,
                time_secs: 1_700_000_000,
                micros: 15000,
                argv: vec!["GET".into(), "big:key".into()],
                client: "127.0.0.1:52814".into(),
                client_name: "worker".into(),
            }
        );
        assert_eq!(entries[1].argv, vec!["KEYS".to_string(), "*".to_string()]);
        assert_eq!(entries[1].client, "");
    }

    fn budget() -> ScanBudget {
        ScanBudget {
            count_hint: 200,
            wall_clock: std::time::Duration::from_millis(500),
            want: 50,
        }
    }

    /// Seed `n` string keys under a unique per-test prefix, tagged so
    /// concurrent test runs on a shared server don't collide (mirrors
    /// clickhouse.rs's `tag` helper).
    async fn seed(conn: &mut MultiplexedConnection, prefix: &str, n: usize) -> Vec<String> {
        let mut pipe = redis::pipe();
        let keys: Vec<String> = (0..n).map(|i| format!("{prefix}:{i}")).collect();
        for k in &keys {
            pipe.cmd("SET").arg(k).arg("v");
        }
        let _: Vec<redis::Value> = pipe.query_async(conn).await.unwrap();
        keys
    }

    fn tag(name: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        format!("red_test_{name}_{}_{n}", std::process::id())
    }

    // Live cluster test, provided via `RED_TEST_REDIS_CLUSTER_URL` (a seed node
    // of a real multi-master cluster), so a run without one skips cleanly.
    // Spin one up with:
    //
    //   docker run -d --name red-cluster -e IP=0.0.0.0 -e INITIAL_PORT=7100 \
    //       -p 7100-7105:7100-7105 grokzen/redis-cluster:7.0.10
    //   export RED_TEST_REDIS_CLUSTER_URL='redis://127.0.0.1:7100'
    fn cluster_url() -> Option<String> {
        std::env::var("RED_TEST_REDIS_CLUSTER_URL").ok()
    }

    // Live Sentinel test, provided via `RED_TEST_REDIS_SENTINEL_URL` (a
    // Sentinel address, e.g. `redis://127.0.0.1:26379`), skipped cleanly
    // without one. Spin one up with a master on 6401 + a sentinel on 26379:
    //
    //   docker run -d --name red-sentinel -p 6401:6401 -p 26379:26379 redis:7 sh -c '
    //     redis-server --port 6401 &
    //     sleep 1
    //     printf "port 26379\nsentinel monitor mymaster 127.0.0.1 6401 1\n" > /tmp/s.conf
    //     exec redis-sentinel /tmp/s.conf'
    //   export RED_TEST_REDIS_SENTINEL_URL='redis://127.0.0.1:26379'
    fn sentinel_url() -> Option<String> {
        std::env::var("RED_TEST_REDIS_SENTINEL_URL").ok()
    }

    #[tokio::test]
    async fn sentinel_lists_masters_and_resolves_the_data_node() {
        let Some(url) = sentinel_url() else {
            eprintln!(
                "SKIP {}: RED_TEST_REDIS_SENTINEL_URL not set",
                module_path!()
            );
            return;
        };
        // Discovery lists the monitored master(s).
        let masters = sentinel_masters(&url).await.unwrap();
        assert!(
            masters.iter().any(|m| m.name == "mymaster"),
            "expected a monitored master named 'mymaster', got {masters:?}"
        );

        // A `?master=` DSN resolves through the Sentinel to the real data node
        // and connects there — a normal standalone session once resolved, so a
        // round-trip write works.
        let sep = if url.contains('?') { '&' } else { '?' };
        let master_dsn = format!("{url}{sep}master=mymaster");
        let driver = RedisDriver::connect(&master_dsn, false).await.unwrap();
        assert_eq!(driver.topology(), KvTopology::Standalone);
        let key = tag("sentinel");
        driver
            .set_string(&key, "v".into(), StringTtl::Clear)
            .await
            .unwrap();
        assert!(driver.probe_key(&key).await.unwrap().is_some());
        driver
            .delete_keys(std::slice::from_ref(&key))
            .await
            .unwrap();

        // An unknown master name is a clear, fatal connect error, not a hang.
        let bad = format!("{url}{sep}master=nosuchmaster");
        assert!(RedisDriver::connect(&bad, true).await.is_err());
    }

    #[tokio::test]
    async fn cluster_scan_fans_out_across_every_master() {
        let Some(url) = cluster_url() else {
            eprintln!(
                "SKIP {}: RED_TEST_REDIS_CLUSTER_URL not set",
                module_path!()
            );
            return;
        };
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        assert_eq!(driver.topology(), KvTopology::Cluster);
        let cl = driver
            .cluster
            .as_ref()
            .expect("cluster state was discovered");
        assert!(cl.masters.len() >= 2, "expected a multi-master cluster");

        // Seed plain keys (no hash tag) so they spread across slots/shards.
        // Each `set_string` routes to the owning master, so this also exercises
        // write routing.
        let prefix = tag("cluster");
        let n = 300usize;
        for i in 0..n {
            driver
                .set_string(&format!("{prefix}:{i}"), "v".into(), StringTtl::Clear)
                .await
                .unwrap();
        }

        // The load-bearing check: any single master sees only a fraction of the
        // keys, so a scan that didn't fan out (the old behaviour) would miss
        // most of them.
        let mut one_node = 0usize;
        let mut c = 0u64;
        loop {
            let (next, batch) = scan_once(
                &mut cl.masters[0].clone(),
                c,
                Some(&format!("{prefix}:*")),
                None,
                200,
            )
            .await
            .unwrap();
            one_node += batch.len();
            c = next;
            if c == 0 {
                break;
            }
        }
        assert!(
            one_node < n,
            "a single master saw {one_node}/{n}; fan-out must exceed that"
        );

        // The real scan walks every master and finds all of them.
        let abort = AbortSignal::new();
        let mut found = std::collections::HashSet::new();
        let mut cursor = ScanCursor::START;
        loop {
            let page = driver
                .scan_keys(
                    cursor,
                    Some(&format!("{prefix}:*")),
                    None,
                    None,
                    budget(),
                    &abort,
                )
                .await
                .unwrap();
            for k in &page.keys {
                found.insert(k.key.clone());
            }
            cursor = page.next_cursor;
            if page.exhausted {
                break;
            }
        }
        for i in 0..n {
            assert!(
                found.contains(&format!("{prefix}:{i}")),
                "missing {prefix}:{i}"
            );
        }
        assert!(
            matches!(cursor, ScanCursor::Cluster { .. }),
            "a cluster scan should carry a cluster cursor"
        );

        // A routed read reaches the owning shard (would `MOVED`-error on the
        // seed node without routing), and `delete_keys` groups the cross-slot
        // batch per master.
        let some_key = format!("{prefix}:{}", n - 1);
        assert!(driver.read_value(&some_key).await.unwrap().is_some());
        let keys: Vec<String> = (0..n).map(|i| format!("{prefix}:{i}")).collect();
        assert_eq!(driver.delete_keys(&keys).await.unwrap(), n as u64);
    }

    #[tokio::test]
    async fn scan_finds_every_seeded_key_across_pages() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let prefix = tag("scan");
        let seeded = seed(&mut driver.conn.clone(), &prefix, 30).await;

        let abort = AbortSignal::new();
        let mut found = std::collections::HashSet::new();
        let mut cursor = ScanCursor::START;
        loop {
            let page = driver
                .scan_keys(
                    cursor,
                    Some(&format!("{prefix}:*")),
                    None,
                    None,
                    ScanBudget {
                        count_hint: 5,
                        want: 5,
                        ..budget()
                    },
                    &abort,
                )
                .await
                .unwrap();
            for k in &page.keys {
                found.insert(k.key.clone());
                assert_eq!(k.kv_type, KvType::String);
                assert!(k.ttl.is_none()); // no EXPIRE was set
            }
            cursor = page.next_cursor;
            if page.exhausted {
                break;
            }
        }
        for k in &seeded {
            assert!(found.contains(k), "missing {k}");
        }
    }

    #[tokio::test]
    async fn scan_reports_ttl_and_types_per_key() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let prefix = tag("types");
        let mut conn = driver.conn.clone();
        let str_key = format!("{prefix}:str");
        let hash_key = format!("{prefix}:hash");
        let _: () = redis::cmd("SET")
            .arg(&str_key)
            .arg("v")
            .arg("PX")
            .arg(60_000)
            .query_async(&mut conn)
            .await
            .unwrap();
        let _: () = redis::cmd("HSET")
            .arg(&hash_key)
            .arg("f")
            .arg("v")
            .query_async(&mut conn)
            .await
            .unwrap();

        let abort = AbortSignal::new();
        let page = driver
            .scan_keys(
                ScanCursor::START,
                Some(&format!("{prefix}:*")),
                None,
                None,
                budget(),
                &abort,
            )
            .await
            .unwrap();
        let by_key: std::collections::HashMap<_, _> =
            page.keys.iter().map(|k| (k.key.clone(), k)).collect();

        let str_meta = by_key.get(&str_key).expect("string key present");
        assert_eq!(str_meta.kv_type, KvType::String);
        assert!(str_meta.ttl.is_some());
        assert!(!str_meta.encoding.is_empty());

        let hash_meta = by_key.get(&hash_key).expect("hash key present");
        assert_eq!(hash_meta.kv_type, KvType::Hash);
        assert!(hash_meta.ttl.is_none());
    }

    #[tokio::test]
    async fn scan_type_filter_returns_only_that_type() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let prefix = tag("typefilter");
        let mut conn = driver.conn.clone();
        let str_key = format!("{prefix}:str");
        let hash_key = format!("{prefix}:hash");
        let _: () = redis::cmd("SET")
            .arg(&str_key)
            .arg("v")
            .query_async(&mut conn)
            .await
            .unwrap();
        let _: () = redis::cmd("HSET")
            .arg(&hash_key)
            .arg("f")
            .arg("v")
            .query_async(&mut conn)
            .await
            .unwrap();

        // `TYPE hash` alongside the prefix `MATCH` yields only the hash key —
        // the server skips the string one, so it never reaches the metadata
        // batch.
        let abort = AbortSignal::new();
        let mut found = std::collections::HashSet::new();
        let mut cursor = ScanCursor::START;
        loop {
            let page = driver
                .scan_keys(
                    cursor,
                    Some(&format!("{prefix}:*")),
                    Some("hash"),
                    None,
                    budget(),
                    &abort,
                )
                .await
                .unwrap();
            for k in &page.keys {
                assert_eq!(k.kv_type, KvType::Hash);
                found.insert(k.key.clone());
            }
            cursor = page.next_cursor;
            if page.exhausted {
                break;
            }
        }
        assert!(found.contains(&hash_key), "hash key should match TYPE hash");
        assert!(
            !found.contains(&str_key),
            "string key should be filtered out by TYPE hash"
        );
    }

    /// The vanished-key race this batch fetch has to survive: a key expires
    /// (or is deleted) between `SCAN` finding it and the pipelined metadata
    /// fetch reading it. `OBJECT ENCODING` on that key errors inside the
    /// pipeline; without `.ignore_errors()` this would fail the whole batch
    /// and drop every other key's metadata along with it.
    #[tokio::test]
    async fn vanished_key_is_dropped_not_fatal_to_the_batch() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let mut conn = driver.conn.clone();
        let present = tag("present");
        let gone = tag("gone");
        let _: () = redis::cmd("SET")
            .arg(&present)
            .arg("v")
            .query_async(&mut conn)
            .await
            .unwrap();
        // `gone` is never set, so `TYPE gone` reports "none" — the same
        // shape as a key that existed at SCAN time and expired before this
        // call, without the timing flakiness of a real short-TTL race.
        let keys = fetch_key_meta_batch(&mut conn, &[present.clone(), gone.clone()])
            .await
            .unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key, present);
    }

    #[tokio::test]
    async fn probe_key_finds_existing_and_reports_none_for_missing() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let present = tag("probe-present");
        let missing = tag("probe-missing");
        let _: () = redis::cmd("SET")
            .arg(&present)
            .arg("v")
            .query_async(&mut driver.conn.clone())
            .await
            .unwrap();

        let found = driver.probe_key(&present).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().kv_type, KvType::String);

        let absent = driver.probe_key(&missing).await.unwrap();
        assert!(absent.is_none());
    }

    #[tokio::test]
    async fn read_value_reports_a_capped_string() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let mut conn = driver.conn.clone();
        let small = tag("str-small");
        let big = tag("str-big");
        let _: () = redis::cmd("SET")
            .arg(&small)
            .arg("hello")
            .query_async(&mut conn)
            .await
            .unwrap();
        let big_value = "x".repeat(STRING_PREVIEW_CAP + 500);
        let _: () = redis::cmd("SET")
            .arg(&big)
            .arg(&big_value)
            .query_async(&mut conn)
            .await
            .unwrap();

        match driver.read_value(&small).await.unwrap().unwrap() {
            KvValue::Str(Value::Text(s)) => assert_eq!(s.as_ref(), "hello"),
            other => panic!("expected an uncapped Text value, got {other:?}"),
        }
        match driver.read_value(&big).await.unwrap().unwrap() {
            KvValue::Str(Value::Capped(cell)) => {
                assert_eq!(cell.len, big_value.len());
                assert_eq!(cell.head.len(), STRING_PREVIEW_CAP);
                assert!(!cell.blob);
            }
            other => panic!("expected a Capped value, got {other:?}"),
        }
        assert!(driver.read_value(&tag("missing")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_string_full_returns_the_uncapped_body() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let mut conn = driver.conn.clone();
        let big = tag("str-full-big");
        let big_value = "x".repeat(STRING_PREVIEW_CAP + 500);
        let _: () = redis::cmd("SET")
            .arg(&big)
            .arg(&big_value)
            .query_async(&mut conn)
            .await
            .unwrap();

        // `read_value` caps this key; `read_string_full` returns every byte.
        match driver.read_value(&big).await.unwrap().unwrap() {
            KvValue::Str(Value::Capped(_)) => {}
            other => panic!("expected a Capped value from read_value, got {other:?}"),
        }
        match driver.read_string_full(&big).await.unwrap().unwrap() {
            Value::Text(s) => assert_eq!(s.as_ref(), big_value.as_str()),
            other => panic!("expected the full Text value, got {other:?}"),
        }
        assert!(
            driver
                .read_string_full(&tag("missing"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn read_string_full_caps_an_over_ceiling_value() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let mut conn = driver.conn.clone();
        let huge = tag("str-full-huge");
        // A value past the safety ceiling must come back bounded, carrying its true
        // length, never the whole body — otherwise a 512 MB string OOMs the UI.
        let total = STRING_FULL_CAP + 1024;
        let huge_value = "y".repeat(total);
        let _: () = redis::cmd("SET")
            .arg(&huge)
            .arg(&huge_value)
            .query_async(&mut conn)
            .await
            .unwrap();

        match driver.read_string_full(&huge).await.unwrap().unwrap() {
            Value::Capped(cell) => {
                assert_eq!(cell.len, total, "reports the true length");
                assert_eq!(
                    cell.head.len(),
                    STRING_FULL_CAP,
                    "head is the bounded prefix"
                );
                assert!(!cell.blob);
            }
            other => panic!("expected a Capped value over the ceiling, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_value_loads_a_small_hash_fully() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let key = tag("hash-small");
        let mut conn = driver.conn.clone();
        let _: () = redis::cmd("HSET")
            .arg(&key)
            .arg("a")
            .arg("1")
            .arg("b")
            .arg("2")
            .query_async(&mut conn)
            .await
            .unwrap();

        let KvValue::Hash(KvCollection::Loaded(pairs)) =
            driver.read_value(&key).await.unwrap().unwrap()
        else {
            panic!("expected a loaded hash");
        };
        let map: std::collections::HashMap<_, _> = pairs.into_iter().collect();
        assert_eq!(map.get("a").map(String::as_str), Some("1"));
        assert_eq!(map.get("b").map(String::as_str), Some("2"));
    }

    #[tokio::test]
    async fn read_value_reports_a_large_set_as_length_only_then_pages_it() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let key = tag("set-large");
        let mut conn = driver.conn.clone();
        let n = SMALL_COLLECTION_THRESHOLD as usize + 20;
        let members: Vec<String> = (0..n).map(|i| format!("m{i}")).collect();
        let mut pipe = redis::pipe();
        for m in &members {
            pipe.cmd("SADD").arg(&key).arg(m);
        }
        let _: Vec<redis::Value> = pipe.query_async(&mut conn).await.unwrap();

        let KvValue::Set(KvCollection::Large { len }) =
            driver.read_value(&key).await.unwrap().unwrap()
        else {
            panic!("expected a large (length-only) set");
        };
        assert_eq!(len, n as u64);

        // Page it fully via read_collection_page and confirm every member
        // that was SADDed is found.
        let abort = AbortSignal::new();
        let mut found = std::collections::HashSet::new();
        let mut cursor = 0;
        loop {
            let page = driver
                .read_collection_page(&key, CollectionKind::Set, cursor, budget(), &abort)
                .await
                .unwrap();
            for el in page.elements {
                match el {
                    KvElement::Member(m) => {
                        found.insert(m);
                    }
                    other => panic!("expected Member elements for a set, got {other:?}"),
                }
            }
            cursor = page.next_cursor;
            if page.exhausted {
                break;
            }
        }
        for m in &members {
            assert!(found.contains(m), "missing {m}");
        }
    }

    #[tokio::test]
    async fn read_value_zset_carries_scores() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let key = tag("zset-small");
        let mut conn = driver.conn.clone();
        let _: () = redis::cmd("ZADD")
            .arg(&key)
            .arg(1.5)
            .arg("a")
            .arg(2.5)
            .arg("b")
            .query_async(&mut conn)
            .await
            .unwrap();

        let KvValue::ZSet(KvCollection::Loaded(pairs)) =
            driver.read_value(&key).await.unwrap().unwrap()
        else {
            panic!("expected a loaded zset");
        };
        let map: std::collections::HashMap<_, _> = pairs.into_iter().collect();
        assert_eq!(map.get("a"), Some(&1.5));
        assert_eq!(map.get("b"), Some(&2.5));
    }

    #[tokio::test]
    async fn read_list_window_reads_head_and_tail() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let key = tag("list-window");
        let mut conn = driver.conn.clone();
        let mut pipe = redis::pipe();
        for i in 0..10 {
            pipe.cmd("RPUSH").arg(&key).arg(i.to_string());
        }
        let _: Vec<redis::Value> = pipe.query_async(&mut conn).await.unwrap();

        let head = driver.read_list_window(&key, true, 3).await.unwrap();
        assert_eq!(head, vec!["0", "1", "2"]);
        let tail = driver.read_list_window(&key, false, 3).await.unwrap();
        assert_eq!(tail, vec!["7", "8", "9"]);
    }

    #[tokio::test]
    async fn read_value_loads_a_small_stream_newest_first() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let key = tag("stream-small");
        let mut conn = driver.conn.clone();
        // Three entries with explicit, ordered IDs so the newest-first
        // ordering and the field/value decode are both checkable.
        for (id, val) in [("1-1", "a"), ("2-1", "b"), ("3-1", "c")] {
            let _: String = redis::cmd("XADD")
                .arg(&key)
                .arg(id)
                .arg("f")
                .arg(val)
                .query_async(&mut conn)
                .await
                .unwrap();
        }

        let KvValue::Stream(KvCollection::Loaded(entries)) =
            driver.read_value(&key).await.unwrap().unwrap()
        else {
            panic!("expected a loaded stream");
        };
        let ids: Vec<_> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["3-1", "2-1", "1-1"]); // newest-first
        assert_eq!(entries[0].fields, vec![("f".to_string(), "c".to_string())]);
    }

    #[tokio::test]
    async fn read_value_reports_a_large_stream_then_pages_it_back_in_time() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let key = tag("stream-large");
        let mut conn = driver.conn.clone();
        let n = SMALL_COLLECTION_THRESHOLD as usize + 20;
        let mut pipe = redis::pipe();
        for i in 0..n {
            // Fixed ms with an incrementing seq keeps IDs strictly ordered.
            pipe.cmd("XADD")
                .arg(&key)
                .arg(format!("1-{}", i + 1))
                .arg("f")
                .arg(i.to_string());
        }
        let _: Vec<redis::Value> = pipe.query_async(&mut conn).await.unwrap();

        let KvValue::Stream(KvCollection::Large { len }) =
            driver.read_value(&key).await.unwrap().unwrap()
        else {
            panic!("expected a large (length-only) stream");
        };
        assert_eq!(len, n as u64);

        // Page it fully, newest-first, and confirm every entry ID is seen
        // exactly once with the IDs strictly decreasing across pages.
        let mut seen = std::collections::HashSet::new();
        let mut before: Option<String> = None;
        loop {
            let page = driver
                .read_stream_range(&key, before.as_deref(), 40)
                .await
                .unwrap();
            for e in &page.entries {
                assert!(seen.insert(e.id.clone()), "duplicate {}", e.id);
            }
            before = page.next_before.clone();
            if page.exhausted {
                break;
            }
        }
        assert_eq!(seen.len(), n);
        for i in 0..n {
            assert!(
                seen.contains(&format!("1-{}", i + 1)),
                "missing 1-{}",
                i + 1
            );
        }
    }

    #[tokio::test]
    async fn stream_consumer_groups_round_trip() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let key = tag("stream-groups");
        let mut conn = driver.conn.clone();
        // Two entries, a group, and a consumer that reads (but doesn't ack)
        // both — so they land in the group's pending list.
        for (id, val) in [("1-1", "a"), ("2-1", "b")] {
            let _: String = redis::cmd("XADD")
                .arg(&key)
                .arg(id)
                .arg("f")
                .arg(val)
                .query_async(&mut conn)
                .await
                .unwrap();
        }
        let _: () = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(&key)
            .arg("g1")
            .arg("0")
            .query_async(&mut conn)
            .await
            .unwrap();
        // `>` delivers new entries to this consumer, populating the PEL.
        let _: redis::Value = redis::cmd("XREADGROUP")
            .arg("GROUP")
            .arg("g1")
            .arg("worker-a")
            .arg("COUNT")
            .arg(10)
            .arg("STREAMS")
            .arg(&key)
            .arg(">")
            .query_async(&mut conn)
            .await
            .unwrap();

        let groups = driver.stream_groups(&key).await.unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "g1");
        assert_eq!(groups[0].pending, 2);
        assert_eq!(groups[0].consumers, 1);

        let consumers = driver.stream_consumers(&key, "g1").await.unwrap();
        assert_eq!(consumers.len(), 1);
        assert_eq!(consumers[0].name, "worker-a");
        assert_eq!(consumers[0].pending, 2);

        let pending = driver.stream_pending(&key, "g1", 10).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|p| p.consumer == "worker-a"));
        assert!(pending.iter().any(|p| p.id == "1-1"));

        // Claim one entry to a different consumer (min-idle 0 so it's eligible),
        // then ack the other. Both writes reflect in the pending set afterward.
        let claimed = driver
            .stream_claim(&key, "g1", "worker-b", Duration::ZERO, &["1-1".into()])
            .await
            .unwrap();
        assert_eq!(claimed, 1);
        let acked = driver
            .stream_ack(&key, "g1", &["2-1".into()])
            .await
            .unwrap();
        assert_eq!(acked, 1);

        let pending = driver.stream_pending(&key, "g1", 10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "1-1");
        assert_eq!(pending[0].consumer, "worker-b"); // reassigned by the claim

        let _: () = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stream_writes_refused_on_a_read_only_connection() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, true).await.unwrap();
        assert!(driver.stream_ack("k", "g", &["1-1".into()]).await.is_err());
        assert!(
            driver
                .stream_claim("k", "g", "c", Duration::ZERO, &["1-1".into()])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn slowlog_captures_and_resets() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let mut conn = driver.conn.clone();
        // Remember the server's threshold, then log *everything* so a single
        // command lands in the log deterministically.
        let prev = value_to_string_vec(
            &redis::cmd("CONFIG")
                .arg("GET")
                .arg("slowlog-log-slower-than")
                .query_async::<redis::Value>(&mut conn)
                .await
                .unwrap(),
        );
        let _: () = redis::cmd("CONFIG")
            .arg("SET")
            .arg("slowlog-log-slower-than")
            .arg(0)
            .query_async(&mut conn)
            .await
            .unwrap();
        let _: () = redis::cmd("SLOWLOG")
            .arg("RESET")
            .query_async(&mut conn)
            .await
            .unwrap();
        let key = tag("slowlog");
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg("v")
            .query_async(&mut conn)
            .await
            .unwrap();

        let entries = driver.slowlog(128).await.unwrap();
        assert!(
            !entries.is_empty(),
            "the slow log should have captured commands"
        );
        assert!(entries.iter().all(|e| !e.argv.is_empty()));

        // Restore the original threshold *before* resetting, so the reset (and
        // the verifying `SLOWLOG GET`) aren't themselves logged — otherwise the
        // "empty after reset" check races the very command doing the checking.
        if let Some(threshold) = prev.get(1) {
            let _: () = redis::cmd("CONFIG")
                .arg("SET")
                .arg("slowlog-log-slower-than")
                .arg(threshold)
                .query_async(&mut conn)
                .await
                .unwrap();
        }
        driver.slowlog_reset().await.unwrap();
        assert!(driver.slowlog(128).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn slowlog_reset_refused_on_a_read_only_connection() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, true).await.unwrap();
        assert!(driver.slowlog_reset().await.is_err());
        // Reading the log is still allowed read-only.
        driver.slowlog(16).await.unwrap();
    }

    #[tokio::test]
    async fn keyspace_notifications_config_and_delivery() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        // Remember the original setting, enable all notifications, and confirm
        // the getter reads it back.
        let prev = driver.notify_config().await.unwrap();
        driver.set_notify_config("KEA").await.unwrap();
        assert!(!driver.notify_config().await.unwrap().is_empty());

        // Subscribe to the keyevent firehose, then trigger an event and confirm
        // it arrives and decodes.
        let key = tag("keyspace");
        let mut sub = driver
            .subscribe(red_core::kv::KeyspaceScope::ByEvent.pattern())
            .await
            .unwrap();
        let mut conn = driver.conn.clone();
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg("v")
            .query_async(&mut conn)
            .await
            .unwrap();

        let got = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match sub.stream.next().await {
                    Some(m) => {
                        if let Some(ev) =
                            red_core::kv::parse_keyspace_channel(&m.channel, &m.payload)
                            && ev.key == key
                        {
                            break Some(ev);
                        }
                    }
                    None => break None,
                }
            }
        })
        .await
        .ok()
        .flatten();

        // Restore the original config before asserting, so a failure doesn't
        // leave the shared test server reconfigured.
        driver.set_notify_config(&prev).await.unwrap();

        let ev = got.expect("a keyspace notification for our SET");
        assert_eq!(ev.event, "set");
        assert_eq!(ev.key, key);
    }

    #[tokio::test]
    async fn set_notify_config_refused_on_a_read_only_connection() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, true).await.unwrap();
        assert!(driver.set_notify_config("KEA").await.is_err());
        // Reading the config is still allowed read-only.
        driver.notify_config().await.unwrap();
    }

    #[tokio::test]
    async fn client_list_includes_our_own_connection() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        // Name our own connection so we can find it in the list deterministically.
        let name = tag("client");
        let mut conn = driver.conn.clone();
        let _: () = redis::cmd("CLIENT")
            .arg("SETNAME")
            .arg(&name)
            .query_async(&mut conn)
            .await
            .unwrap();

        let clients = driver.client_list().await.unwrap();
        assert!(!clients.is_empty());
        assert!(
            clients.iter().any(|c| c.name == name),
            "our named connection should appear in CLIENT LIST"
        );
        assert!(clients.iter().all(|c| c.id > 0 && !c.addr.is_empty()));
    }

    #[tokio::test]
    async fn client_kill_refused_on_a_read_only_connection() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, true).await.unwrap();
        assert!(driver.client_kill(999_999).await.is_err());
        // Listing is still allowed read-only.
        driver.client_list().await.unwrap();
    }

    #[tokio::test]
    async fn monitor_streams_executed_commands() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let mut mon = driver.monitor().await.unwrap();
        // Run a uniquely-tagged command on a separate connection; MONITOR
        // should echo it back on the firehose.
        let key = tag("monitor");
        let mut conn = driver.conn.clone();
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg("v")
            .query_async(&mut conn)
            .await
            .unwrap();

        let found = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match mon.stream.next().await {
                    Some(line) if line.contains(&key) => break true,
                    Some(_) => continue,
                    None => break false,
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(found, "expected MONITOR to surface our tagged SET command");
    }

    #[test]
    fn parse_stream_entries_decodes_id_and_fields() {
        use redis::Value;
        let reply = Value::Array(vec![
            Value::Array(vec![
                Value::BulkString(b"5-0".to_vec()),
                Value::Array(vec![
                    Value::BulkString(b"a".to_vec()),
                    Value::BulkString(b"1".to_vec()),
                    Value::BulkString(b"b".to_vec()),
                    Value::BulkString(b"2".to_vec()),
                ]),
            ]),
            // A malformed entry (no field array) is skipped, not fatal.
            Value::Array(vec![Value::BulkString(b"6-0".to_vec())]),
        ]);
        let entries = parse_stream_entries(&reply);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "5-0");
        assert_eq!(
            entries[0].fields,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn command_runs_an_arbitrary_command() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let key = tag("command-set");
        match driver
            .command(&["SET".into(), key.clone(), "hi".into()])
            .await
            .unwrap()
        {
            RespValue::Ok => {}
            other => panic!("expected Ok, got {other:?}"),
        }
        match driver.command(&["GET".into(), key]).await.unwrap() {
            RespValue::Bulk(s) => assert_eq!(s, "hi"),
            other => panic!("expected Bulk, got {other:?}"),
        }
        match driver
            .command(&["TTL".into(), tag("missing")])
            .await
            .unwrap()
        {
            RespValue::Int(-2) => {} // -2: key doesn't exist
            other => panic!("expected Int(-2), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn command_reports_server_errors_as_error_not_err() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let key = tag("command-wrongtype");
        driver
            .command(&["SET".into(), key.clone(), "v".into()])
            .await
            .unwrap();
        // SADD on a string key is a WRONGTYPE server error, not a transport
        // failure: `command` must still return `Ok` with a `RespValue::Error`.
        match driver
            .command(&["SADD".into(), key, "member".into()])
            .await
            .unwrap()
        {
            RespValue::Error(msg) => assert!(msg.contains("WRONGTYPE"), "{msg}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_methods_are_refused_on_a_read_only_connection() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, true).await.unwrap();
        let key = tag("readonly-refused");
        assert!(
            driver
                .set_string(&key, "v".into(), StringTtl::Clear)
                .await
                .is_err()
        );
        assert!(driver.set_field(&key, "f", "v".into()).await.is_err());
        assert!(driver.set_ttl(&key, None).await.is_err());
        assert!(driver.rename_key(&key, "other").await.is_err());
        assert!(driver.delete_keys(&[key]).await.is_err());
        // A write command through the console is refused the same way.
        assert!(
            driver
                .command(&["SET".into(), tag("ro-console"), "v".into()])
                .await
                .is_err()
        );
        // But a read still works.
        assert!(driver.command(&["PING".into()]).await.is_ok());
    }

    #[tokio::test]
    async fn set_string_field_ttl_rename_and_delete_round_trip() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let key = tag("edit-string");
        let renamed = tag("edit-string-renamed");

        driver
            .set_string(
                &key,
                "hello".into(),
                StringTtl::Set(Duration::from_secs(60)),
            )
            .await
            .unwrap();
        let meta = driver.probe_key(&key).await.unwrap().unwrap();
        assert!(meta.ttl.is_some());

        driver.set_ttl(&key, None).await.unwrap(); // PERSIST
        let meta = driver.probe_key(&key).await.unwrap().unwrap();
        assert!(meta.ttl.is_none());

        driver.rename_key(&key, &renamed).await.unwrap();
        assert!(driver.probe_key(&key).await.unwrap().is_none());
        assert!(driver.probe_key(&renamed).await.unwrap().is_some());

        let deleted = driver
            .delete_keys(std::slice::from_ref(&renamed))
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        assert!(driver.probe_key(&renamed).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_field_creates_and_updates_a_hash_field() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let key = tag("edit-hash");
        driver.set_field(&key, "a", "1".into()).await.unwrap();
        driver.set_field(&key, "a", "2".into()).await.unwrap();
        let KvValue::Hash(KvCollection::Loaded(pairs)) =
            driver.read_value(&key).await.unwrap().unwrap()
        else {
            panic!("expected a loaded hash");
        };
        assert_eq!(pairs, vec![("a".to_string(), "2".to_string())]);
    }

    #[tokio::test]
    async fn subscribe_delivers_published_messages() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let channel = tag("pubsub-channel");
        let mut sub = driver.subscribe(&format!("{channel}*")).await.unwrap();

        // Give the subscription a moment to actually register server-side
        // before publishing, then publish through a second connection (a
        // subscriber connection can't also PUBLISH on some server configs).
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut publisher = driver.conn.clone();
        let full_channel = format!("{channel}:1");
        redis::cmd("PUBLISH")
            .arg(&full_channel)
            .arg("hello")
            .query_async::<i64>(&mut publisher)
            .await
            .unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(2), sub.stream.next())
            .await
            .expect("timed out waiting for a pubsub message")
            .expect("stream ended without a message");
        assert_eq!(msg.channel, full_channel);
        assert_eq!(msg.payload, "hello");
    }
}
