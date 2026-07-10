//! `KvDriver` over a real Redis/Valkey server. Standalone/Sentinel today;
//! Cluster topology is detected (so the UI can hide the DB-index switch) but
//! its scan fan-out lands with R1 (see `docs/plans/redis.md`).

use async_trait::async_trait;
use red_core::kv::{
    CollectionKind, KeyMeta, KvCollection, KvCollectionPage, KvElement, KvScanPage, KvType,
    KvValue, ScanBudget,
};
use red_core::{RedError, Result, Value};
use redis::aio::MultiplexedConnection;
use tokio::time::Instant;

use crate::kv::{KvDriver, KvTopology};
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

/// One Redis/Valkey session. Holds a single [`MultiplexedConnection`]:
/// documented as cheap to clone and safe to use concurrently from multiple
/// clones (it pipelines internally), so `KvDriver`'s `&self` methods clone it
/// per call rather than guarding one instance behind a lock — that would
/// serialize every command through one mutex and defeat the point of a
/// multiplexed connection.
pub struct RedisDriver {
    conn: MultiplexedConnection,
    version: String,
    topology: KvTopology,
}

impl RedisDriver {
    /// Dial `dsn` (`redis://[:password@]host:port/db` or `rediss://` for
    /// TLS) and probe `INFO server` to capture the version and topology up
    /// front, the same "fail fast on bad creds, know what we're talking to"
    /// shape as `ClickhouseDriver::connect`'s `fetch_version`.
    pub async fn connect(dsn: &str, _read_only: bool) -> Result<Self> {
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
        Ok(Self {
            conn,
            version,
            topology,
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
        let mut conn = self.conn.clone();
        redis::cmd("DBSIZE")
            .query_async(&mut conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))
    }

    async fn scan_keys(
        &self,
        cursor: u64,
        pattern: Option<&str>,
        budget: ScanBudget,
        abort: &AbortSignal,
    ) -> Result<KvScanPage> {
        let mut conn = self.conn.clone();
        let deadline = Instant::now() + budget.wall_clock;
        let mut cur = cursor;
        let mut collected: Vec<String> = Vec::new();
        loop {
            if abort.is_aborted() {
                return Err(RedError::Interrupted);
            }
            let mut cmd = redis::cmd("SCAN");
            cmd.arg(cur).arg("COUNT").arg(budget.count_hint);
            if let Some(p) = pattern {
                cmd.arg("MATCH").arg(p);
            }
            let (next_cur, batch): (u64, Vec<String>) = cmd
                .query_async(&mut conn)
                .await
                .map_err(|e| RedError::Driver(e.to_string()))?;
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
            next_cursor: cur,
            exhausted,
        })
    }

    async fn probe_key(&self, key: &str) -> Result<Option<KeyMeta>> {
        let mut conn = self.conn.clone();
        let keys = fetch_key_meta_batch(&mut conn, std::slice::from_ref(&key.to_string())).await?;
        Ok(keys.into_iter().next())
    }

    async fn read_value(&self, key: &str) -> Result<Option<KvValue>> {
        let mut conn = self.conn.clone();
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
            KvType::Stream | KvType::Other(_) => Ok(Some(KvValue::Unsupported(kv_type))),
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
        let mut conn = self.conn.clone();
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
        let mut conn = self.conn.clone();
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

    #[tokio::test]
    async fn scan_finds_every_seeded_key_across_pages() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let prefix = tag("scan");
        let seeded = seed(&mut driver.conn.clone(), &prefix, 30).await;

        let abort = AbortSignal::new();
        let mut found = std::collections::HashSet::new();
        let mut cursor = 0;
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
            .scan_keys(0, Some(&format!("{prefix}:*")), budget(), &abort)
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
    async fn read_value_reports_stream_as_unsupported() {
        let url = url_or_skip!();
        let driver = RedisDriver::connect(&url, false).await.unwrap();
        let key = tag("stream");
        let mut conn = driver.conn.clone();
        let _: String = redis::cmd("XADD")
            .arg(&key)
            .arg("*")
            .arg("f")
            .arg("v")
            .query_async(&mut conn)
            .await
            .unwrap();

        match driver.read_value(&key).await.unwrap().unwrap() {
            KvValue::Unsupported(KvType::Stream) => {}
            other => panic!("expected Unsupported(Stream), got {other:?}"),
        }
    }
}
