//! `KvDriver`: the parallel seam for engines that aren't SQL-shaped (Redis
//! today; see `docs/plans/redis.md` for why this can't just be another
//! `DatabaseDriver` arm — no table/column/FK/DDL model, no orderable key
//! space, no `OFFSET`/keyset paging). Object-safe like `DatabaseDriver`, held
//! as `Arc<dyn KvDriver>`, one impl per engine.
//!
//! R0 landed connect/identity/`DBSIZE`; R1 (this module's `scan_keys`/
//! `probe_key`) adds keyspace browsing. Per-type value reads, the console,
//! and editing land in later phases per the plan's R2–R3 breakdown.

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::Stream;
use red_core::kv::{
    ClientInfo, CollectionKind, KeyMeta, KvCollectionPage, KvMessage, KvScanPage, KvStreamPage,
    KvValue, PendingEntry, RespValue, ScanBudget, ScanCursor, SlowlogEntry, StreamConsumer,
    StreamGroup,
};
use red_core::Result;

use crate::AbortSignal;

/// A live Pub/Sub subscription's message stream (see
/// `KvDriver::subscribe`). Boxed because the concrete stream type is
/// engine-specific (redis-rs's `PubSubStream`) and this trait is
/// object-safe/engine-agnostic.
pub struct KvSubscription {
    pub stream: Pin<Box<dyn Stream<Item = KvMessage> + Send>>,
}

/// A live `MONITOR` firehose: every command the server executes, one raw line
/// per item (see `KvDriver::monitor`). Its own type rather than reusing
/// [`KvSubscription`] because a MONITOR line is a single preformatted string
/// (`"<ts> [<db> <client>] \"CMD\" \"arg\" ..."`), not a channel/payload pair.
pub struct KvMonitorStream {
    pub stream: Pin<Box<dyn Stream<Item = String> + Send>>,
}

/// A Redis/Valkey server's deployment topology, detected at connect from
/// `INFO server`'s `redis_mode` field. Drives UI affordances that don't apply
/// uniformly: a `Cluster` has exactly one logical database (no `SELECT
/// 0..15`), and keyspace scanning must fan out per-node under `Cluster` but
/// not under `Standalone`/`Sentinel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvTopology {
    Standalone,
    Sentinel,
    Cluster,
}

/// One open key-value session. The parallel seam to [`DatabaseDriver`](crate::DatabaseDriver)
/// for engines that aren't SQL-shaped. Object-safe so the service can hold
/// `Arc<dyn KvDriver>` and swap engines behind it, mirroring how
/// `DatabaseDriver` is held.
#[async_trait]
pub trait KvDriver: Send + Sync {
    /// Cheap liveness probe: touches the underlying connection.
    async fn ping(&self) -> Result<()>;

    /// Engine version string (e.g. `"7.4.0"`), for the status bar. Cheap and
    /// synchronous; captured once at connect.
    fn server_version(&self) -> String;

    /// The deployment topology detected at connect.
    fn topology(&self) -> KvTopology;

    /// Total key count in the selected logical database (`DBSIZE`). O(1) on
    /// the server; a header stat only — it counts the *whole* database, not a
    /// pattern-filtered browse (see docs/plans/redis.md's "Performance
    /// architecture" section on why there's no cheap filtered count).
    async fn db_size(&self) -> Result<u64>;

    /// One page of a keyspace scan: `SCAN` (looping, budgeted, `MATCH`-
    /// filtered when `pattern` is set, `TYPE`-filtered when `type_filter` is
    /// set to a type label like `"hash"`) followed by one pipelined metadata
    /// round trip for the batch (`TYPE`/`PTTL`/`OBJECT ENCODING`/`MEMORY
    /// USAGE` per key). `type_filter` is pushed down to `SCAN ... TYPE` so the
    /// server skips non-matching keys entirely rather than the caller filtering
    /// a materialized page (there's no cheap filtered count either way, but a
    /// selective type walk doesn't drag every other key's metadata over the
    /// wire). Stateless like `DatabaseDriver::fetch_seek`: `cursor` is whatever
    /// `next_cursor` the previous call returned (`0` to start), not a handle the
    /// driver holds open between calls — the caller (the service, then the UI's
    /// grid buffer) owns scan position, same as it owns a seek boundary key for
    /// the SQL grid.
    async fn scan_keys(
        &self,
        cursor: ScanCursor,
        pattern: Option<&str>,
        type_filter: Option<&str>,
        budget: ScanBudget,
        abort: &AbortSignal,
    ) -> Result<KvScanPage>;

    /// Exact-key jump (see docs/plans/redis.md's "The grid needs a third
    /// buffer mode"): resolve one key's metadata directly, bypassing `SCAN`
    /// entirely. `Ok(None)` when the key doesn't exist, not an error.
    async fn probe_key(&self, key: &str) -> Result<Option<KeyMeta>>;

    /// One key's value (see docs/plans/redis.md's "big collections inside a
    /// single key"): a string capped like a SQL cell, or a collection either
    /// fully loaded (below the small-collection threshold, one round trip)
    /// or reported as just its length (at/above it — the caller pages the
    /// rest via [`read_collection_page`](Self::read_collection_page)/
    /// [`read_list_window`](Self::read_list_window) rather than this ever
    /// issuing a `*GETALL`/`*MEMBERS` on a huge collection). `Ok(None)` when
    /// the key doesn't exist.
    async fn read_value(&self, key: &str) -> Result<Option<KvValue>>;

    /// One page of a big hash/set/zset's elements (`HSCAN`/`SSCAN`/`ZSCAN`).
    /// Stateless like [`scan_keys`](Self::scan_keys): `cursor` is the
    /// caller-supplied `next_cursor` from the previous page (`0` to start).
    async fn read_collection_page(
        &self,
        key: &str,
        kind: CollectionKind,
        cursor: u64,
        budget: ScanBudget,
        abort: &AbortSignal,
    ) -> Result<KvCollectionPage>;

    /// A windowed slice of a big list (`LRANGE`), from the head or the tail.
    /// Arbitrary deep-middle access isn't offered: `LRANGE`'s cost grows with
    /// the offset, unlike a `SCAN`-shaped read (see docs/plans/redis.md's
    /// documented limitation on this).
    async fn read_list_window(
        &self,
        key: &str,
        from_head: bool,
        count: usize,
    ) -> Result<Vec<String>>;

    /// One page of a big stream's entries, newest-first (`XREVRANGE`). `before`
    /// is the exclusive upper-bound entry ID to page older than (the previous
    /// page's `KvStreamPage::next_before`); `None` starts from the newest
    /// entry (`+`). Streams have no `*SCAN` cursor, so this pages by ID range
    /// rather than a stateless opaque cursor like
    /// [`read_collection_page`](Self::read_collection_page).
    async fn read_stream_range(
        &self,
        key: &str,
        before: Option<&str>,
        count: usize,
    ) -> Result<KvStreamPage>;

    /// A stream's consumer groups (`XINFO GROUPS <key>`), for the inspector's
    /// consumer-group management view (see docs/plans/redis.md's "stream
    /// consumer-group management" gap). An empty vec means the stream has no
    /// groups yet; an error is only a transport/`WRONGTYPE` failure.
    async fn stream_groups(&self, key: &str) -> Result<Vec<StreamGroup>>;

    /// The consumers registered in one group (`XINFO CONSUMERS <key>
    /// <group>`), each with its own pending count and idle time.
    async fn stream_consumers(&self, key: &str, group: &str) -> Result<Vec<StreamConsumer>>;

    /// Up to `count` of a group's pending (delivered-but-unacked) entries, the
    /// extended `XPENDING <key> <group> - + <count>` form (id, consumer, idle,
    /// delivery-count per entry). Newest-position order as Redis returns it.
    async fn stream_pending(
        &self,
        key: &str,
        group: &str,
        count: usize,
    ) -> Result<Vec<PendingEntry>>;

    /// Acknowledge pending entries (`XACK <key> <group> <id...>`), dropping
    /// them from the group's PEL. Returns the number actually acked. Refused
    /// on a read-only connection.
    async fn stream_ack(&self, key: &str, group: &str, ids: &[String]) -> Result<u64>;

    /// Reassign pending entries to `consumer` (`XCLAIM <key> <group>
    /// <consumer> <min_idle_ms> <id...> JUSTID`), the reclaim-stuck-messages
    /// operation. `min_idle_ms` guards against stealing an entry another
    /// consumer just picked up (only entries idle at least this long are
    /// claimed). Returns the number of entries actually claimed. Refused on a
    /// read-only connection.
    async fn stream_claim(
        &self,
        key: &str,
        group: &str,
        consumer: &str,
        min_idle: Duration,
        ids: &[String],
    ) -> Result<u64>;

    /// Run an arbitrary command (the console; see docs/plans/redis.md).
    /// `argv[0]` is the command name. Read-only enforcement and the
    /// destructive-command confirm are the caller's job
    /// (`red_core::kv::classify_command`), shared between the service's
    /// read-only gate and the UI's confirm prompt — this method itself
    /// always runs whatever it's given.
    async fn command(&self, argv: &[String]) -> Result<RespValue>;

    /// `SET key value [EX seconds]`. Refused on a read-only connection.
    async fn set_string(&self, key: &str, value: String, ttl: Option<Duration>) -> Result<()>;

    /// `HSET key field value`. Refused on a read-only connection.
    async fn set_field(&self, key: &str, field: &str, value: String) -> Result<()>;

    /// `EXPIRE key seconds` (`Some`) or `PERSIST key` (`None`). Refused on a
    /// read-only connection.
    async fn set_ttl(&self, key: &str, ttl: Option<Duration>) -> Result<()>;

    /// `RENAME from to`. Refused on a read-only connection.
    async fn rename_key(&self, from: &str, to: &str) -> Result<()>;

    /// `DEL key [key ...]`, returning the number actually removed. Refused
    /// on a read-only connection.
    async fn delete_keys(&self, keys: &[String]) -> Result<u64>;

    /// The server's slow-command log (`SLOWLOG GET <count>`), newest first,
    /// for the diagnostics panel (see docs/plans/redis.md's "slowlog viewer"
    /// gap). Under a cluster this reports the seed node's log only (the slow
    /// log is per-node; there's no aggregate view).
    async fn slowlog(&self, count: usize) -> Result<Vec<SlowlogEntry>>;

    /// Clear the slow log (`SLOWLOG RESET`). A server-state maintenance write,
    /// so it's refused on a read-only connection.
    async fn slowlog_reset(&self) -> Result<()>;

    /// The connected clients (`CLIENT LIST`), for the diagnostics panel's
    /// clients viewer (see docs/plans/redis.md's "CLIENT LIST viewer" gap).
    /// Under a cluster this reports the seed node's clients only (client lists
    /// are per-node).
    async fn client_list(&self) -> Result<Vec<ClientInfo>>;

    /// Disconnect a client by its connection id (`CLIENT KILL ID <id>`). A
    /// server-state write, so it's refused on a read-only connection.
    async fn client_kill(&self, id: i64) -> Result<()>;

    /// A live `MONITOR` stream: every command the server runs, pushed as a raw
    /// line for as long as the returned stream is read (see docs/plans/redis.md's
    /// "MONITOR-based live command profiler" gap). Like [`subscribe`](Self::subscribe),
    /// dropping the stream ends it; there's no explicit stop command. Runs over
    /// its own dedicated connection (MONITOR monopolizes a connection), so it
    /// never blocks the shared multiplexed one. Under a cluster it observes the
    /// seed node only.
    async fn monitor(&self) -> Result<KvMonitorStream>;

    /// The server's `notify-keyspace-events` setting (`CONFIG GET`), for the
    /// keyspace-notification watcher (see docs/plans/redis.md's "keyspace-
    /// notification live tooling" gap). Empty string means notifications are
    /// off — nothing will be delivered until it's enabled.
    async fn notify_config(&self) -> Result<String>;

    /// Set `notify-keyspace-events` (`CONFIG SET`), to turn keyspace
    /// notifications on/off. A server-config write, so it's refused on a
    /// read-only connection.
    async fn set_notify_config(&self, flags: &str) -> Result<()>;

    /// A live Pub/Sub pattern subscription (`PSUBSCRIBE`). The caller owns
    /// when to stop reading the stream; there's no explicit unsubscribe
    /// call, dropping the returned `KvSubscription` (and its underlying
    /// connection) is enough.
    async fn subscribe(&self, pattern: &str) -> Result<KvSubscription>;
}
