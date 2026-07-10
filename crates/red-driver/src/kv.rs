//! `KvDriver`: the parallel seam for engines that aren't SQL-shaped (Redis
//! today; see `docs/plans/redis.md` for why this can't just be another
//! `DatabaseDriver` arm — no table/column/FK/DDL model, no orderable key
//! space, no `OFFSET`/keyset paging). Object-safe like `DatabaseDriver`, held
//! as `Arc<dyn KvDriver>`, one impl per engine.
//!
//! R0 landed connect/identity/`DBSIZE`; R1 (this module's `scan_keys`/
//! `probe_key`) adds keyspace browsing. Per-type value reads, the console,
//! and editing land in later phases per the plan's R2–R3 breakdown.

use async_trait::async_trait;
use red_core::kv::{CollectionKind, KeyMeta, KvCollectionPage, KvScanPage, KvValue, ScanBudget};
use red_core::Result;

use crate::AbortSignal;

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
    /// filtered when `pattern` is set) followed by one pipelined metadata
    /// round trip for the batch (`TYPE`/`PTTL`/`OBJECT ENCODING`/`MEMORY
    /// USAGE` per key). Stateless like `DatabaseDriver::fetch_seek`: `cursor`
    /// is whatever `next_cursor` the previous call returned (`0` to start),
    /// not a handle the driver holds open between calls — the caller (the
    /// service, then the UI's grid buffer) owns scan position, same as it
    /// owns a seek boundary key for the SQL grid.
    async fn scan_keys(
        &self,
        cursor: u64,
        pattern: Option<&str>,
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
}
