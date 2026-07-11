//! `KvDriver` over a real Redis/Valkey server. Standalone/Sentinel today;
//! Cluster topology is detected (so the UI can hide the DB-index switch) but
//! its scan fan-out lands with R1 (see `docs/plans/redis.md`).

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use red_core::kv::{
    CollectionKind, CommandClass, KeyMeta, KvCollection, KvCollectionPage, KvElement, KvMessage,
    KvScanPage, KvStreamPage, KvType, KvValue, RespValue, ScanBudget, ScanCursor, StreamEntry,
};
use red_core::{RedError, Result, Value};
use redis::aio::MultiplexedConnection;
use tokio::time::Instant;

use crate::kv::{KvDriver, KvSubscription, KvTopology};
use crate::AbortSignal;

/// Below this many elements, `read_value` fetches a hash/set/zset/list in
/// full (one round trip); at/above it, only the length is reported and the
/// caller pages the rest (see docs/plans/redis.md's "a few hundred elements"
/// guidance).
const SMALL_COLLECTION_THRESHOLD: u64 = 200;
/// Display cap for a string value preview, mirroring the SQL grid's
/// `Value::Capped` cell cap (`red_driver::DEFAULT_DISPLAY_CELL_CAP`) rather
/// than reusing it directly: a Redis string preview is a one-off inspector
/// fetch, not a per-cell grid budget, so it gets its own constant.
const STRING_PREVIEW_CAP: usize = 8 * 1024;

/// The total number of hash slots in a Redis Cluster (fixed by the protocol).
const CLUSTER_SLOTS: u16 = 16384;

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
        if self.read_only {
            Err(RedError::Query("this connection is read-only".to_string()))
        } else {
            Ok(())
        }
    }

    /// Standalone/Sentinel scan: the budgeted `SCAN` loop on the sole
    /// connection, then one pipelined metadata batch (the original R1 shape).
    async fn scan_standalone(
        &self,
        cursor: ScanCursor,
        pattern: Option<&str>,
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
            let (next_cur, batch) = scan_once(&mut conn, cur, pattern, budget.count_hint).await?;
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
        while node < cl.masters.len() {
            if abort.is_aborted() {
                return Err(RedError::Interrupted);
            }
            let mut conn = cl.masters[node].clone();
            let (next_cur, batch) = scan_once(&mut conn, cur, pattern, budget.count_hint).await?;
            cur = next_cur;
            if !batch.is_empty() {
                keys.extend(fetch_key_meta_batch(&mut conn, &batch).await?);
            }
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

/// One `SCAN` round trip: `SCAN <cursor> COUNT <hint> [MATCH <pattern>]`,
/// returning the next cursor and this batch's keys. The shared inner step of
/// both the standalone and per-node cluster scan loops.
async fn scan_once(
    conn: &mut MultiplexedConnection,
    cursor: u64,
    pattern: Option<&str>,
    count_hint: u32,
) -> Result<(u64, Vec<String>)> {
    let mut cmd = redis::cmd("SCAN");
    cmd.arg(cursor).arg("COUNT").arg(count_hint);
    if let Some(p) = pattern {
        cmd.arg("MATCH").arg(p);
    }
    cmd.query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))
}

/// Discover a cluster's masters via `CLUSTER SLOTS` and open a dedicated
/// connection to each, alongside the slot→master routing table (see
/// [`ClusterState`]). Each per-node connection reuses the seed `dsn`'s
/// scheme/credentials/TLS with only the host+port swapped, so an
/// authenticated or `rediss://` cluster fans out with the same identity as
/// the seed connection.
async fn discover_cluster(dsn: &str, conn: &mut MultiplexedConnection) -> Result<ClusterState> {
    let reply: redis::Value = redis::cmd("CLUSTER")
        .arg("SLOTS")
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    let ranges = parse_cluster_slots(&reply);
    if ranges.is_empty() {
        return Err(RedError::Driver("CLUSTER SLOTS returned no shards".into()));
    }
    // Dedup masters by (host, port) preserving first-seen order (one master
    // can own several disjoint slot ranges), building the slot table as we go.
    let mut masters_addr: Vec<(String, u16)> = Vec::new();
    let mut slot_owner = vec![0u16; CLUSTER_SLOTS as usize];
    for (start, end, host, port) in &ranges {
        let idx = match masters_addr
            .iter()
            .position(|(h, p)| h == host && p == port)
        {
            Some(i) => i,
            None => {
                masters_addr.push((host.clone(), *port));
                masters_addr.len() - 1
            }
        };
        for slot in *start..=*end {
            if let Some(owner) = slot_owner.get_mut(slot as usize) {
                *owner = idx as u16;
            }
        }
    }
    let mut masters = Vec::with_capacity(masters_addr.len());
    for (host, port) in &masters_addr {
        let dsn = node_dsn(dsn, host, *port)?;
        let node_client = redis::Client::open(dsn).map_err(|e| RedError::Connect(e.to_string()))?;
        let node_conn = node_client
            .get_multiplexed_async_connection()
            .await
            .map_err(map_connect_err)?;
        masters.push(node_conn);
    }
    Ok(ClusterState {
        masters,
        slot_owner,
    })
}

/// Parse a `CLUSTER SLOTS` reply into `(start_slot, end_slot, master_host,
/// master_port)` ranges. Each element is `[start, end, [ip, port, id, ...],
/// [replica...], ...]`; the master is the third element. An empty `ip` (some
/// servers return `""` to mean "the address you connected over") is kept as-is
/// and resolved to the seed host later by [`node_dsn`]. Malformed ranges are
/// skipped rather than fatal.
fn parse_cluster_slots(v: &redis::Value) -> Vec<(u16, u16, String, u16)> {
    let (redis::Value::Array(items) | redis::Value::Set(items)) = v else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let (redis::Value::Array(parts) | redis::Value::Set(parts)) = item else {
            continue;
        };
        if parts.len() < 3 {
            continue;
        }
        let (Some(start), Some(end)) = (value_to_i64(&parts[0]), value_to_i64(&parts[1])) else {
            continue;
        };
        let (redis::Value::Array(master) | redis::Value::Set(master)) = &parts[2] else {
            continue;
        };
        if master.len() < 2 {
            continue;
        }
        let (Some(ip), Some(port)) = (value_to_string(&master[0]), value_to_i64(&master[1])) else {
            continue;
        };
        let max = CLUSTER_SLOTS as i64 - 1;
        if !(0..=max).contains(&start) || !(0..=max).contains(&end) || start > end {
            continue;
        }
        out.push((start as u16, end as u16, ip, port as u16));
    }
    out
}

/// Rebuild the seed `dsn` with a cluster node's `host`/`port` swapped in,
/// preserving scheme (`redis`/`rediss`), credentials, and any query. An empty
/// `host` (see [`parse_cluster_slots`]) leaves the seed host in place and only
/// changes the port.
fn node_dsn(dsn: &str, host: &str, port: u16) -> Result<String> {
    let mut url =
        url::Url::parse(dsn).map_err(|e| RedError::Connect(format!("bad Redis DSN: {e}")))?;
    if !host.is_empty() {
        url.set_host(Some(host))
            .map_err(|e| RedError::Connect(format!("bad cluster node host {host}: {e}")))?;
    }
    url.set_port(Some(port))
        .map_err(|()| RedError::Connect(format!("bad cluster node port {port}")))?;
    Ok(url.to_string())
}

/// The cluster hash slot a key routes to: `CRC16(hashtag(key)) mod 16384`,
/// matching Redis's own key→slot mapping (so a read/write reaches the node
/// that owns the key).
fn key_slot(key: &str) -> u16 {
    crc16(hashtag(key.as_bytes())) % CLUSTER_SLOTS
}

/// The bytes a cluster key hashes over: if the key contains a `{...}` hash tag
/// with a non-empty body, only that body (so `{user1}:a` and `{user1}:b`
/// co-locate); otherwise the whole key. Matches Redis's hash-tag rule exactly.
fn hashtag(key: &[u8]) -> &[u8] {
    if let Some(open) = key.iter().position(|&c| c == b'{') {
        if let Some(rel) = key[open + 1..].iter().position(|&c| c == b'}') {
            if rel > 0 {
                return &key[open + 1..open + 1 + rel];
            }
        }
    }
    key
}

/// CRC16-CCITT (XMODEM, polynomial `0x1021`, zero init) — the checksum Redis
/// Cluster uses for slot assignment. Bitwise rather than table-driven: it runs
/// once per routed key, not in a hot loop, so the table isn't worth the space.
fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in bytes {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// One master a Sentinel monitors (`SENTINEL MASTERS`), for a connection
/// form's master picker (see docs/plans/redis.md's Sentinel gap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentinelMaster {
    /// The monitored name (the "service name" a Sentinel connection asks for).
    pub name: String,
    pub host: String,
    pub port: u16,
}

/// List the masters a Sentinel monitors, for populating a connection form's
/// picker before any data connection exists. `sentinel_dsn` points at a
/// Sentinel (`redis://sentinelhost:26379`, `rediss://` for TLS).
pub async fn sentinel_masters(sentinel_dsn: &str) -> Result<Vec<SentinelMaster>> {
    let client = redis::Client::open(sentinel_dsn).map_err(|e| RedError::Connect(e.to_string()))?;
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .map_err(map_connect_err)?;
    let reply: redis::Value = redis::cmd("SENTINEL")
        .arg("MASTERS")
        .query_async(&mut conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    Ok(parse_sentinel_masters(&reply))
}

/// If `dsn` carries a `?master=<name>` param, treat its host as a Sentinel:
/// resolve that master's current address (`SENTINEL get-master-addr-by-name`)
/// and return a data-node DSN pointing at it, with scheme/credentials/TLS
/// preserved and the `master` param dropped. Without the param, `dsn` is
/// returned unchanged. See [`RedisDriver::connect`].
async fn resolve_sentinel(dsn: &str) -> Result<String> {
    let url = url::Url::parse(dsn).map_err(|e| RedError::Connect(format!("bad Redis DSN: {e}")))?;
    let Some(master) = url
        .query_pairs()
        .find(|(k, _)| k == "master")
        .map(|(_, v)| v.into_owned())
    else {
        return Ok(dsn.to_string());
    };
    // A copy of the DSN with the `master` param removed: used both to dial the
    // Sentinel and as the base for the resolved master DSN (so redis's own URL
    // parser never sees the unknown `master` param).
    let mut base = url.clone();
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "master")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    base.set_query(None);
    if !kept.is_empty() {
        let mut qp = base.query_pairs_mut();
        for (k, v) in &kept {
            qp.append_pair(k, v);
        }
    }
    let client =
        redis::Client::open(base.to_string()).map_err(|e| RedError::Connect(e.to_string()))?;
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .map_err(map_connect_err)?;
    let addr: Option<(String, u16)> = redis::cmd("SENTINEL")
        .arg("get-master-addr-by-name")
        .arg(&master)
        .query_async(&mut conn)
        .await
        .map_err(|e| RedError::Connect(e.to_string()))?;
    let Some((host, port)) = addr else {
        return Err(RedError::Connect(format!(
            "Sentinel knows no master named '{master}'"
        )));
    };
    let mut data = base;
    data.set_host(Some(&host))
        .map_err(|e| RedError::Connect(format!("bad master host {host}: {e}")))?;
    data.set_port(Some(port))
        .map_err(|()| RedError::Connect(format!("bad master port {port}")))?;
    Ok(data.to_string())
}

/// Parse a `SENTINEL MASTERS` reply: an array of masters, each a flat
/// `[field, value, field, value, ...]` map. Pulls `name`/`ip`/`port`; a
/// master missing any of those (or with an unparseable port) is skipped.
fn parse_sentinel_masters(v: &redis::Value) -> Vec<SentinelMaster> {
    let (redis::Value::Array(items) | redis::Value::Set(items)) = v else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let map = pair_up(value_to_string_vec(item));
        let get = |key: &str| {
            map.iter()
                .find(|(k, _)| k == key)
                .map(|(_, val)| val.clone())
        };
        let (Some(name), Some(host), Some(port)) = (get("name"), get("ip"), get("port")) else {
            continue;
        };
        let Ok(port) = port.parse::<u16>() else {
            continue;
        };
        out.push(SentinelMaster { name, host, port });
    }
    out
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
        budget: ScanBudget,
        abort: &AbortSignal,
    ) -> Result<KvScanPage> {
        match &self.cluster {
            Some(cl) => self.scan_cluster(cl, cursor, pattern, budget, abort).await,
            None => self.scan_standalone(cursor, pattern, budget, abort).await,
        }
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
            let (next_cur, flat): (u64, Vec<String>) = cmd
                .query_async(&mut conn)
                .await
                .map_err(|e| RedError::Driver(e.to_string()))?;
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
        redis::cmd("LRANGE")
            .arg(key)
            .arg(start)
            .arg(stop)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
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

    async fn command(&self, argv: &[String]) -> Result<RespValue> {
        let Some(name) = argv.first() else {
            return Err(RedError::Query("empty command".into()));
        };
        if self.read_only && red_core::kv::classify_command(argv) != CommandClass::Read {
            self.check_writable()?;
        }
        let mut cmd = redis::cmd(name);
        for arg in &argv[1..] {
            cmd.arg(arg);
        }
        let mut conn = self.conn.clone();
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

    async fn set_string(&self, key: &str, value: String, ttl: Option<Duration>) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.route(key);
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value);
        if let Some(ttl) = ttl {
            cmd.arg("EX").arg(ttl.as_secs().max(1));
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

    async fn set_ttl(&self, key: &str, ttl: Option<Duration>) -> Result<()> {
        self.check_writable()?;
        let mut conn = self.route(key);
        match ttl {
            Some(ttl) => redis::cmd("EXPIRE")
                .arg(key)
                .arg(ttl.as_secs().max(1))
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

/// Convert a raw RESP `redis::Value` into the engine-agnostic `RespValue`
/// the console renders. Bulk strings decode lossily (the console is a text
/// log, not a hex viewer); anything genuinely binary still round-trips as
/// *a* string, just not necessarily a meaningful one.
fn to_resp_value(value: redis::Value) -> RespValue {
    match value {
        redis::Value::Nil => RespValue::Nil,
        redis::Value::Okay => RespValue::Ok,
        redis::Value::Int(i) => RespValue::Int(i),
        redis::Value::Double(d) => RespValue::Double(d),
        redis::Value::Boolean(b) => RespValue::Bool(b),
        redis::Value::SimpleString(s) => RespValue::Simple(s),
        redis::Value::BulkString(bytes) => {
            RespValue::Bulk(String::from_utf8_lossy(&bytes).into_owned())
        }
        redis::Value::VerbatimString { text, .. } => RespValue::Bulk(text),
        redis::Value::BigNumber(n) => RespValue::Simple(String::from_utf8_lossy(&n).into_owned()),
        redis::Value::Array(items) | redis::Value::Set(items) => {
            RespValue::Array(items.into_iter().map(to_resp_value).collect())
        }
        redis::Value::Map(pairs) => RespValue::Array(
            pairs
                .into_iter()
                .flat_map(|(k, v)| [to_resp_value(k), to_resp_value(v)])
                .collect(),
        ),
        redis::Value::Push { kind, data } => RespValue::Array(
            std::iter::once(RespValue::Simple(format!("{kind:?}")))
                .chain(data.into_iter().map(to_resp_value))
                .collect(),
        ),
        redis::Value::ServerError(e) => RespValue::Error(e.to_string()),
        redis::Value::Attribute { data, .. } => to_resp_value(*data),
        // `redis::Value` is `#[non_exhaustive]`; anything this build doesn't
        // know about yet renders as its `Debug` text rather than failing.
        other => RespValue::Simple(format!("{other:?}")),
    }
}

/// Cap a fetched string value like a SQL display cell: under the cap, the
/// text (or a lossy-UTF8 decode) verbatim; over it, a
/// [`red_core::CappedCell`] carrying only a char-boundary-safe prefix, never
/// the full bytes.
fn cap_string_value(bytes: Vec<u8>) -> Value {
    let len = bytes.len();
    if len <= STRING_PREVIEW_CAP {
        return Value::Text(String::from_utf8_lossy(&bytes).into_owned());
    }
    let mut head = String::from_utf8_lossy(&bytes[..STRING_PREVIEW_CAP]).into_owned();
    // `from_utf8_lossy` on a byte slice cut mid-codepoint already replaces
    // the truncated tail with U+FFFD, so `head` is always valid UTF-8 here;
    // no separate char-boundary trim needed.
    if head.len() > STRING_PREVIEW_CAP {
        head.truncate(STRING_PREVIEW_CAP);
    }
    Value::Capped(red_core::CappedCell {
        head,
        len,
        blob: false,
    })
}

/// `HGETALL`/`ZRANGE WITHSCORES` (and `SMEMBERS`/`LRANGE 0 -1`) return a flat
/// `[a, b, a, b, ...]` array; pair it up into `(a, b)` tuples. A trailing
/// unpaired element (a torn reply, shouldn't happen) is dropped rather than
/// panicking.
fn pair_up(flat: Vec<String>) -> Vec<(String, String)> {
    let mut it = flat.into_iter();
    let mut out = Vec::new();
    while let (Some(a), Some(b)) = (it.next(), it.next()) {
        out.push((a, b));
    }
    out
}

/// Like [`pair_up`], but the second element of each pair is a score.
/// `ZRANGE ... WITHSCORES`/`ZSCAN` both reply as flat
/// `[member, score, member, score, ...]` text; an unparseable score
/// (shouldn't happen) defaults to `0.0` rather than dropping the member.
fn scored_pairs(flat: Vec<String>) -> Vec<(String, f64)> {
    pair_up(flat)
        .into_iter()
        .map(|(member, score)| (member, score.parse::<f64>().unwrap_or(0.0)))
        .collect()
}

/// The `read_value` shared shape for hash/set/zset/list: probe the O(1)
/// length first; below the threshold, fetch everything in one more round
/// trip and `map` it into the collection's element type; at/above it, report
/// only the length.
async fn load_or_probe<T>(
    conn: &mut MultiplexedConnection,
    len_cmd: &str,
    load_cmd: &str,
    key: &str,
    map: impl FnOnce(Vec<String>) -> Vec<T>,
) -> Result<KvCollection<T>> {
    let len: u64 = redis::cmd(len_cmd)
        .arg(key)
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    if len >= SMALL_COLLECTION_THRESHOLD {
        return Ok(KvCollection::Large { len });
    }
    let mut cmd = redis::cmd(load_cmd);
    cmd.arg(key);
    // ZRANGE/LRANGE need an explicit whole-range span; HGETALL/SMEMBERS take
    // just the key.
    if load_cmd == "ZRANGE" {
        cmd.arg(0).arg(-1).arg("WITHSCORES");
    } else if load_cmd == "LRANGE" {
        cmd.arg(0).arg(-1);
    }
    let flat: Vec<String> = cmd
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    Ok(KvCollection::Loaded(map(flat)))
}

/// One page of a stream's entries, newest-first (`XREVRANGE key <end> - COUNT
/// n`). `before` (the previous page's oldest ID) becomes an exclusive upper
/// bound `(<id>` so paging back in time never re-yields an entry already
/// shown; `None` starts at the newest entry (`+`). `exhausted` is inferred
/// from a short page (fewer than `count` came back), and `next_before` carries
/// the oldest ID loaded here for the caller to continue from, or `None` once
/// exhausted.
async fn fetch_stream_page(
    conn: &mut MultiplexedConnection,
    key: &str,
    before: Option<&str>,
    count: usize,
) -> Result<KvStreamPage> {
    let count = count.max(1);
    let end = match before {
        Some(id) => format!("({id}"), // exclusive: don't repeat the boundary entry
        None => "+".to_string(),
    };
    let reply: redis::Value = redis::cmd("XREVRANGE")
        .arg(key)
        .arg(end)
        .arg("-")
        .arg("COUNT")
        .arg(count)
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    let entries = parse_stream_entries(&reply);
    let exhausted = entries.len() < count;
    let next_before = if exhausted {
        None
    } else {
        entries.last().map(|e| e.id.clone())
    };
    Ok(KvStreamPage {
        entries,
        next_before,
        exhausted,
    })
}

/// Decode an `XRANGE`/`XREVRANGE` reply: an array of `[id, [field, value,
/// field, value, ...]]` entries. Parsed from the raw `redis::Value` rather
/// than a typed decode so a torn or unexpected shape degrades to "fewer
/// entries" rather than failing the whole read — a malformed entry (missing
/// ID, or a field list that isn't a flat array) is skipped, not fatal.
fn parse_stream_entries(v: &redis::Value) -> Vec<StreamEntry> {
    let (redis::Value::Array(items) | redis::Value::Set(items)) = v else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let (redis::Value::Array(pair) | redis::Value::Set(pair)) = item else {
            continue;
        };
        let [id_val, fields_val] = pair.as_slice() else {
            continue;
        };
        let Some(id) = value_to_string(id_val) else {
            continue;
        };
        out.push(StreamEntry {
            id,
            fields: pair_up(value_to_string_vec(fields_val)),
        });
    }
    out
}

/// Pipeline `TYPE`/`PTTL`/`OBJECT ENCODING`/`MEMORY USAGE` for a batch of keys
/// into one round trip (see docs/plans/redis.md's "the N+1 metadata
/// problem"). `.ignore_errors()` keeps a single key that expired between
/// `SCAN` and this call from failing the whole batch: `OBJECT ENCODING` on a
/// vanished key is the one sub-command that comes back as a RESP error
/// (`TYPE` reports `"none"`, `PTTL`/`MEMORY USAGE` report `-2`/nil), and with
/// `ignore_errors()` set that position decodes as a `Value::ServerError`,
/// which `redis::from_redis_value` turns into a plain `Err` we treat as
/// "unavailable" rather than aborting the batch. Rejected alternative: a Lua
/// script batching all keys in one `EVAL` — breaks under Redis Cluster's
/// `CROSSSLOT` check once a scanned batch spans slots on the same node (see
/// the plan's seam-decision section).
async fn fetch_key_meta_batch(
    conn: &mut MultiplexedConnection,
    keys: &[String],
) -> Result<Vec<KeyMeta>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut pipe = redis::pipe();
    pipe.ignore_errors();
    for k in keys {
        pipe.cmd("TYPE").arg(k);
        pipe.cmd("PTTL").arg(k);
        pipe.cmd("OBJECT").arg("ENCODING").arg(k);
        pipe.cmd("MEMORY").arg("USAGE").arg(k);
    }
    let replies: Vec<redis::Value> = pipe
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;

    let mut out = Vec::with_capacity(keys.len());
    for (i, key) in keys.iter().enumerate() {
        let base = i * 4;
        let Some(type_raw) = value_to_string(&replies[base]) else {
            continue; // TYPE itself didn't decode: drop the row defensively.
        };
        let Some(kv_type) = KvType::parse(&type_raw) else {
            continue; // "none": vanished between SCAN and here.
        };
        let ttl = value_to_i64(&replies[base + 1]).and_then(|ms| {
            if ms < 0 {
                None
            } else {
                Some(std::time::Duration::from_millis(ms as u64))
            }
        });
        let encoding = value_to_string(&replies[base + 2]).unwrap_or_default();
        let approx_bytes = value_to_i64(&replies[base + 3]).unwrap_or(0).max(0) as u64;
        out.push(KeyMeta {
            key: key.clone(),
            kv_type,
            ttl,
            encoding,
            approx_bytes,
        });
    }
    Ok(out)
}

fn value_to_string(v: &redis::Value) -> Option<String> {
    redis::from_redis_value::<String>(v.clone()).ok()
}

fn value_to_i64(v: &redis::Value) -> Option<i64> {
    redis::from_redis_value::<i64>(v.clone()).ok()
}

fn value_to_string_vec(v: &redis::Value) -> Vec<String> {
    redis::from_redis_value::<Vec<String>>(v.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        driver.set_string(&key, "v".into(), None).await.unwrap();
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
                .set_string(&format!("{prefix}:{i}"), "v".into(), None)
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
                .scan_keys(cursor, Some(&format!("{prefix}:*")), budget(), &abort)
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
            KvValue::Str(Value::Text(s)) => assert_eq!(s, "hello"),
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
        assert!(driver.set_string(&key, "v".into(), None).await.is_err());
        assert!(driver.set_field(&key, "f", "v".into()).await.is_err());
        assert!(driver.set_ttl(&key, None).await.is_err());
        assert!(driver.rename_key(&key, "other").await.is_err());
        assert!(driver.delete_keys(&[key]).await.is_err());
        // A write command through the console is refused the same way.
        assert!(driver
            .command(&["SET".into(), tag("ro-console"), "v".into()])
            .await
            .is_err());
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
            .set_string(&key, "hello".into(), Some(Duration::from_secs(60)))
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
