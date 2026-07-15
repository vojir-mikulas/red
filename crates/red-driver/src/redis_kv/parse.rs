//! Redis RESP → domain decoding and the small read helpers the `KvDriver` impl
//! leans on: value coercion, stream/group/consumer/pending/slowlog parsing, and
//! the `load_or_probe`/`fetch_stream_page`/`fetch_key_meta_batch` fetch helpers.
//! Split out of `redis_kv/mod.rs` (guidelines D); pure functions over `redis::Value`
//! plus a couple of `MultiplexedConnection` reads. Parent items come via `use super::*`.

use red_core::kv::{
    KeyMeta, KvType, PendingEntry, RespValue, SlowlogEntry, StreamConsumer, StreamEntry,
    StreamGroup,
};
use red_core::{RedError, Result, Value};
use redis::aio::MultiplexedConnection;

use super::*;

/// Convert a raw RESP `redis::Value` into the engine-agnostic `RespValue`
/// the console renders. Bulk strings decode lossily (the console is a text
/// log, not a hex viewer); anything genuinely binary still round-trips as
/// *a* string, just not necessarily a meaningful one.
pub(super) fn to_resp_value(value: redis::Value) -> RespValue {
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
/// value verbatim; over it, a [`red_core::CappedCell`] carrying only a
/// char-boundary-safe prefix, never the full bytes.
///
/// Below the cap, valid UTF-8 becomes [`Value::Text`], while binary (a Redis
/// string holding msgpack/protobuf/pickle/an image) becomes [`Value::Blob`]
/// with its **exact** bytes preserved — so the inspector's binary decoders and
/// hex view see the real bytes, not a lossy-UTF8 mangling of them.
pub(super) fn cap_string_value(bytes: Vec<u8>) -> Value {
    let len = bytes.len();
    if len <= STRING_PREVIEW_CAP {
        return match String::from_utf8(bytes) {
            Ok(s) => Value::Text(s.into()),
            Err(e) => Value::Blob(e.into_bytes()),
        };
    }
    let window = &bytes[..STRING_PREVIEW_CAP];
    // Is the value binary? `error_len() == None` means the only fault is a
    // codepoint sliced off at the window's end (valid UTF-8, just truncated);
    // `Some(_)` is a genuine invalid byte sequence, i.e. binary content — so a
    // large msgpack/protobuf/image value is flagged `blob` and the inspector
    // offers its binary/hex affordance instead of a U+FFFD-riddled text preview.
    let blob = matches!(std::str::from_utf8(window), Err(e) if e.error_len().is_some());
    let mut head = String::from_utf8_lossy(window).into_owned();
    // `from_utf8_lossy` on a byte slice cut mid-codepoint already replaces
    // the truncated tail with U+FFFD, so `head` is always valid UTF-8 here;
    // no separate char-boundary trim needed.
    if head.len() > STRING_PREVIEW_CAP {
        head.truncate(STRING_PREVIEW_CAP);
    }
    Value::Capped(Box::new(red_core::CappedCell { head, len, blob }))
}

/// Redis keys, set members, hash fields and list items are all binary-safe, so
/// decoding a `SCAN`/`SMEMBERS`/`LRANGE` reply straight into `Vec<String>`
/// makes redis-rs reject the *whole* batch on a single non-UTF-8 element —
/// failing the entire browse for one binary key. Read raw bytes instead and
/// convert lossily, so a binary element degrades to a replacement-char label
/// (matching how binary string *values* fall back to a `Blob`) rather than
/// taking the page down with it.
pub(super) fn lossy_utf8(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

/// `HGETALL`/`ZRANGE WITHSCORES` (and `SMEMBERS`/`LRANGE 0 -1`) return a flat
/// `[a, b, a, b, ...]` array; pair it up into `(a, b)` tuples. A trailing
/// unpaired element (a torn reply, shouldn't happen) is dropped rather than
/// panicking.
pub(super) fn pair_up(flat: Vec<String>) -> Vec<(String, String)> {
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
pub(super) fn scored_pairs(flat: Vec<String>) -> Vec<(String, f64)> {
    pair_up(flat)
        .into_iter()
        .map(|(member, score)| (member, score.parse::<f64>().unwrap_or(0.0)))
        .collect()
}

/// The `read_value` shared shape for hash/set/zset/list: probe the O(1)
/// length first; below the threshold, fetch everything in one more round
/// trip and `map` it into the collection's element type; at/above it, report
/// only the length.
pub(super) async fn load_or_probe<T>(
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
    // Binary-safe read: decode raw bytes, then convert lossily (see
    // [`lossy_utf8`]) so a binary member/field doesn't fail the whole load.
    let raw: Vec<Vec<u8>> = cmd
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    let flat: Vec<String> = raw.into_iter().map(lossy_utf8).collect();
    Ok(KvCollection::Loaded(map(flat)))
}

/// One page of a stream's entries, newest-first (`XREVRANGE key <end> - COUNT
/// n`). `before` (the previous page's oldest ID) is used as an *inclusive*
/// upper bound and the boundary entry is dropped client-side, rather than the
/// `(<id>` exclusive syntax which only exists on Redis >= 6.2 and hard-errors
/// on older servers. `None` starts at the newest entry (`+`). `exhausted` is
/// inferred from a short page, and `next_before` carries the oldest ID loaded
/// here for the caller to continue from, or `None` once exhausted.
pub(super) async fn fetch_stream_page(
    conn: &mut MultiplexedConnection,
    key: &str,
    before: Option<&str>,
    count: usize,
) -> Result<KvStreamPage> {
    let count = count.max(1);
    // Inclusive upper bound; when continuing, fetch one extra so a full page
    // still yields `count` fresh entries after the boundary entry is dropped.
    let (end, fetch) = match before {
        Some(id) => (id.to_string(), count + 1),
        None => ("+".to_string(), count),
    };
    let reply: redis::Value = redis::cmd("XREVRANGE")
        .arg(key)
        .arg(end)
        .arg("-")
        .arg("COUNT")
        .arg(fetch)
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    // Base exhaustion on how many entries Redis actually returned, not on how
    // many decoded: `parse_stream_entries` silently skips a malformed entry, so
    // deriving it from `entries.len()` would treat a full page with one skipped
    // entry as the end of the stream and stop paging, hiding older entries.
    let raw_len = match &reply {
        redis::Value::Array(items) | redis::Value::Set(items) => items.len(),
        _ => 0,
    };
    let mut entries = parse_stream_entries(&reply);
    // Drop the boundary entry (the previous page's oldest ID) if it came back,
    // so paging back in time never re-yields an already-shown entry.
    if let Some(b) = before
        && entries.first().map(|e| e.id.as_str()) == Some(b)
    {
        entries.remove(0);
    }
    let exhausted = raw_len < fetch;
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
pub(super) fn parse_stream_entries(v: &redis::Value) -> Vec<StreamEntry> {
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
pub(super) async fn fetch_key_meta_batch(
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
        // Index through `.get()`, not `replies[base + n]`: the pipeline is
        // expected to yield exactly 4 replies per key, but a short reply (a
        // proxy collapsing errored positions, RESP3 push interleaving) would
        // otherwise panic and abort the whole scan instead of dropping the row.
        let Some(type_raw) = replies.get(base).and_then(value_to_string) else {
            continue; // TYPE itself didn't decode / is missing: drop the row defensively.
        };
        let Some(kv_type) = KvType::parse(&type_raw) else {
            continue; // "none": vanished between SCAN and here.
        };
        let ttl = replies.get(base + 1).and_then(value_to_i64).and_then(|ms| {
            if ms < 0 {
                None
            } else {
                Some(std::time::Duration::from_millis(ms as u64))
            }
        });
        let encoding = replies
            .get(base + 2)
            .and_then(value_to_string)
            .unwrap_or_default();
        let approx_bytes = replies
            .get(base + 3)
            .and_then(value_to_i64)
            .unwrap_or(0)
            .max(0) as u64;
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

/// Flatten a RESP reply that models a `field -> value` map into ordered
/// `(field, value)` pairs, tolerating both wire shapes the redis crate hands
/// back: a RESP3 `Map`, or the RESP2 flat `[field, value, field, value, ...]`
/// array that `XINFO`/`XPENDING` use over a RESP2 connection (what a default
/// `MultiplexedConnection` negotiates). Values stay as `redis::Value` so a
/// caller can pull an integer field (`pending`, `idle`) without a lossy string
/// round trip. A trailing unpaired element in the flat form is dropped.
pub(super) fn resp_map(v: &redis::Value) -> Vec<(String, redis::Value)> {
    match v {
        redis::Value::Map(pairs) => pairs
            .iter()
            .filter_map(|(k, val)| value_to_string(k).map(|k| (k, val.clone())))
            .collect(),
        redis::Value::Array(items) | redis::Value::Set(items) => {
            let mut out = Vec::with_capacity(items.len() / 2);
            let mut it = items.iter();
            while let (Some(k), Some(val)) = (it.next(), it.next()) {
                if let Some(k) = value_to_string(k) {
                    out.push((k, val.clone()));
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Look up one field in a [`resp_map`]-flattened reply.
pub(super) fn map_field<'a>(
    map: &'a [(String, redis::Value)],
    key: &str,
) -> Option<&'a redis::Value> {
    map.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Parse an `XINFO GROUPS` reply: an array of per-group maps. A group missing
/// its `name` is skipped; numeric fields default to `0` and `lag` stays `None`
/// when the server omits it or reports it as nil (an older server, or a trimmed
/// stream Redis can't compute lag for).
pub(super) fn parse_stream_groups(v: &redis::Value) -> Vec<StreamGroup> {
    let (redis::Value::Array(items) | redis::Value::Set(items)) = v else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let map = resp_map(item);
            let name = map_field(&map, "name").and_then(value_to_string)?;
            Some(StreamGroup {
                name,
                consumers: map_field(&map, "consumers")
                    .and_then(value_to_i64)
                    .unwrap_or(0)
                    .max(0) as u64,
                pending: map_field(&map, "pending")
                    .and_then(value_to_i64)
                    .unwrap_or(0)
                    .max(0) as u64,
                last_delivered_id: map_field(&map, "last-delivered-id")
                    .and_then(value_to_string)
                    .unwrap_or_default(),
                lag: map_field(&map, "lag").and_then(value_to_i64),
            })
        })
        .collect()
}

/// Parse an `XINFO CONSUMERS` reply: an array of per-consumer maps. A consumer
/// missing its `name` is skipped.
pub(super) fn parse_stream_consumers(v: &redis::Value) -> Vec<StreamConsumer> {
    let (redis::Value::Array(items) | redis::Value::Set(items)) = v else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let map = resp_map(item);
            let name = map_field(&map, "name").and_then(value_to_string)?;
            let idle_ms = map_field(&map, "idle")
                .and_then(value_to_i64)
                .unwrap_or(0)
                .max(0) as u64;
            Some(StreamConsumer {
                name,
                pending: map_field(&map, "pending")
                    .and_then(value_to_i64)
                    .unwrap_or(0)
                    .max(0) as u64,
                idle: Duration::from_millis(idle_ms),
            })
        })
        .collect()
}

/// Parse the extended `XPENDING key group - + count` reply: an array of
/// `[id, consumer, idle_ms, delivery_count]` rows. A torn row (wrong arity, or
/// a missing id/consumer) is skipped rather than fatal, like the stream-entry
/// parser.
pub(super) fn parse_pending_entries(v: &redis::Value) -> Vec<PendingEntry> {
    let (redis::Value::Array(items) | redis::Value::Set(items)) = v else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let (redis::Value::Array(row) | redis::Value::Set(row)) = item else {
                return None;
            };
            let [id_v, consumer_v, idle_v, count_v] = row.as_slice() else {
                return None;
            };
            let id = value_to_string(id_v)?;
            let consumer = value_to_string(consumer_v)?;
            let idle_ms = value_to_i64(idle_v).unwrap_or(0).max(0) as u64;
            Some(PendingEntry {
                id,
                consumer,
                idle: Duration::from_millis(idle_ms),
                delivery_count: value_to_i64(count_v).unwrap_or(0).max(0) as u64,
            })
        })
        .collect()
}

/// Parse a `SLOWLOG GET` reply: an array of entries, each
/// `[id, timestamp, micros, [argv...], client_addr?, client_name?]`. The last
/// two fields arrived in Redis 4.0, so they're read defensively (empty when
/// absent). A torn entry (too few fields) is skipped rather than fatal.
pub(super) fn parse_slowlog(v: &redis::Value) -> Vec<SlowlogEntry> {
    let (redis::Value::Array(items) | redis::Value::Set(items)) = v else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let (redis::Value::Array(parts) | redis::Value::Set(parts)) = item else {
                return None;
            };
            if parts.len() < 4 {
                return None;
            }
            let id = value_to_i64(&parts[0])?;
            let time_secs = value_to_i64(&parts[1])?;
            let micros = value_to_i64(&parts[2]).unwrap_or(0).max(0) as u64;
            let argv = value_to_string_vec(&parts[3]);
            Some(SlowlogEntry {
                id,
                time_secs,
                micros,
                argv,
                client: parts.get(4).and_then(value_to_string).unwrap_or_default(),
                client_name: parts.get(5).and_then(value_to_string).unwrap_or_default(),
            })
        })
        .collect()
}

pub(super) fn value_to_string(v: &redis::Value) -> Option<String> {
    redis::from_redis_value::<String>(v.clone()).ok()
}

pub(super) fn value_to_i64(v: &redis::Value) -> Option<i64> {
    redis::from_redis_value::<i64>(v.clone()).ok()
}

pub(super) fn value_to_string_vec(v: &redis::Value) -> Vec<String> {
    redis::from_redis_value::<Vec<String>>(v.clone()).unwrap_or_default()
}
