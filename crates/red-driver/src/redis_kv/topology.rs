//! Redis multi-node topology support split out of `redis_kv/mod.rs` (guidelines D):
//! Cluster discovery + slot routing (`CLUSTER SLOTS`, CRC16 key-slot hashing,
//! per-master DSN rebuild) and Sentinel master enumeration. Pure helpers over
//! `redis::Value` plus the seed connection; `ClusterState`/`RedisDriver` and the
//! shared decode helpers come from the parent via `use super::*`.

use red_core::{RedError, Result};
use redis::aio::MultiplexedConnection;

use super::*;

/// Discover a cluster's masters via `CLUSTER SLOTS` and open a dedicated
/// connection to each, alongside the slot→master routing table (see
/// [`ClusterState`]). Each per-node connection reuses the seed `dsn`'s
/// scheme/credentials/TLS with only the host+port swapped, so an
/// authenticated or `rediss://` cluster fans out with the same identity as
/// the seed connection.
pub(super) async fn discover_cluster(
    dsn: &str,
    conn: &mut MultiplexedConnection,
) -> Result<ClusterState> {
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
pub(super) fn parse_cluster_slots(v: &redis::Value) -> Vec<(u16, u16, String, u16)> {
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
        // Guard the port the same way as the slot range: an out-of-range value
        // (a malformed/proxied reply) would otherwise be silently truncated by
        // `as u16` and dial a wrong port rather than being dropped.
        if !(0..=i64::from(u16::MAX)).contains(&port) {
            continue;
        }
        out.push((start as u16, end as u16, ip, port as u16));
    }
    out
}

/// Keyless commands whose blast radius is a whole node and which a user issuing
/// them against a cluster means cluster-wide: on a `MultiplexedConnection` they
/// would touch only the seed shard, so [`RedisDriver::command`] fans them out to
/// every master. Keyed destructive commands (`DEL`, `UNLINK`) are excluded — they
/// route to their key's owner correctly.
pub(super) fn is_cluster_fanout_command(argv: &[String]) -> bool {
    let head = argv.first().map(|s| s.to_ascii_uppercase());
    let sub = argv.get(1).map(|s| s.to_ascii_uppercase());
    match head.as_deref() {
        Some("FLUSHALL" | "FLUSHDB" | "SWAPDB") => true,
        Some("SCRIPT" | "FUNCTION") => sub.as_deref() == Some("FLUSH"),
        _ => false,
    }
}

/// Rebuild the seed `dsn` with a cluster node's `host`/`port` swapped in,
/// preserving scheme (`redis`/`rediss`), credentials, and any query. An empty
/// `host` (see [`parse_cluster_slots`]) leaves the seed host in place and only
/// changes the port.
pub(super) fn node_dsn(dsn: &str, host: &str, port: u16) -> Result<String> {
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
pub(super) fn key_slot(key: &str) -> u16 {
    crc16(hashtag(key.as_bytes())) % CLUSTER_SLOTS
}

/// The bytes a cluster key hashes over: if the key contains a `{...}` hash tag
/// with a non-empty body, only that body (so `{user1}:a` and `{user1}:b`
/// co-locate); otherwise the whole key. Matches Redis's hash-tag rule exactly.
pub(super) fn hashtag(key: &[u8]) -> &[u8] {
    if let Some(open) = key.iter().position(|&c| c == b'{')
        && let Some(rel) = key[open + 1..].iter().position(|&c| c == b'}')
        && rel > 0
    {
        return &key[open + 1..open + 1 + rel];
    }
    key
}

/// CRC16-CCITT (XMODEM, polynomial `0x1021`, zero init) — the checksum Redis
/// Cluster uses for slot assignment. Bitwise rather than table-driven: it runs
/// once per routed key, not in a hot loop, so the table isn't worth the space.
pub(super) fn crc16(bytes: &[u8]) -> u16 {
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
pub(super) async fn resolve_sentinel(dsn: &str) -> Result<String> {
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
    // Dial the Sentinel with only scheme/host/port (plus any kept query, e.g.
    // TLS): a Sentinel typically has no `requirepass` and no selectable db, so
    // sending the *data node's* credentials or db path (AUTH/SELECT) makes a
    // passwordless Sentinel reject the connection ("Client sent AUTH, but no
    // password is set"). The credentials stay on `data`, the resolved master DSN.
    let mut sentinel_url = base.clone();
    let _ = sentinel_url.set_username("");
    let _ = sentinel_url.set_password(None);
    sentinel_url.set_path("");
    let client = redis::Client::open(sentinel_url.to_string())
        .map_err(|e| RedError::Connect(e.to_string()))?;
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
pub(super) fn parse_sentinel_masters(v: &redis::Value) -> Vec<SentinelMaster> {
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
