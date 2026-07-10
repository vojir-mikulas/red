//! `KvDriver` over a real Redis/Valkey server. Standalone/Sentinel today;
//! Cluster topology is detected (so the UI can hide the DB-index switch) but
//! its scan fan-out lands with R1 (see `docs/plans/redis.md`).

use async_trait::async_trait;
use red_core::{RedError, Result};
use redis::aio::MultiplexedConnection;

use crate::kv::{KvDriver, KvTopology};

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
}
