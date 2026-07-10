//! `KvDriver` over a real Redis/Valkey server. Standalone/Sentinel today;
//! Cluster topology is detected (so the UI can hide the DB-index switch) but
//! its scan fan-out lands with R1 (see `docs/plans/redis.md`).

use async_trait::async_trait;
use red_core::kv::{KeyMeta, KvScanPage, KvType, ScanBudget};
use red_core::{RedError, Result};
use redis::aio::MultiplexedConnection;
use tokio::time::Instant;

use crate::kv::{KvDriver, KvTopology};
use crate::AbortSignal;

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
}
