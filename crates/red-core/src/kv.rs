//! Domain types for the `KvDriver` seam (Redis; see `docs/plans/redis.md`).
//! Parallel to the SQL-shaped `ResultPage`/`RowWindow`/`KeySpec` in `lib.rs`,
//! but nothing here assumes a table/column model or an orderable key.

use std::time::Duration;

/// A key's Redis data type, from `TYPE`. `Other` covers types this build
/// doesn't render a dedicated inspector for yet (bitmap, hyperloglog — both
/// report as `string` so rarely land here; a future module type would).
/// Deliberately has no `None`/`none` variant: that reply means the key
/// vanished between `SCAN` finding it and the metadata fetch reading it (a
/// real race under the weak-consistency contract `SCAN` already has, see the
/// plan's "Performance architecture" section), and the driver drops such a
/// key from the page rather than surfacing a "none" type to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvType {
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
    Other(String),
}

impl KvType {
    /// Parse a `TYPE` command reply. `None` for `"none"` (the key doesn't
    /// exist), which callers filter out rather than construct a `KeyMeta` for.
    pub fn parse(raw: &str) -> Option<KvType> {
        match raw {
            "string" => Some(KvType::String),
            "hash" => Some(KvType::Hash),
            "list" => Some(KvType::List),
            "set" => Some(KvType::Set),
            "zset" => Some(KvType::ZSet),
            "stream" => Some(KvType::Stream),
            "none" => None,
            other => Some(KvType::Other(other.to_string())),
        }
    }

    /// The short label the keyspace grid's type column and pill render.
    pub fn label(&self) -> &str {
        match self {
            KvType::String => "string",
            KvType::Hash => "hash",
            KvType::List => "list",
            KvType::Set => "set",
            KvType::ZSet => "zset",
            KvType::Stream => "stream",
            KvType::Other(s) => s,
        }
    }
}

/// One key's row in the keyspace browser: everything the grid's
/// `key | type | TTL | size | encoding` columns need, fetched in one
/// pipelined round trip per scanned batch (see `docs/plans/redis.md`'s
/// "the N+1 metadata problem").
#[derive(Debug, Clone)]
pub struct KeyMeta {
    pub key: String,
    pub kv_type: KvType,
    /// `None` means no expiry (`PTTL` returned `-1`).
    pub ttl: Option<Duration>,
    /// `OBJECT ENCODING`, e.g. `"listpack"`/`"hashtable"`/`"embstr"`.
    pub encoding: String,
    /// `MEMORY USAGE ... SAMPLES 5` — sampled, not exact, for an aggregate
    /// type; O(1)-ish rather than a full walk.
    pub approx_bytes: u64,
}

/// How hard a single `scan_keys` call may work before returning a (possibly
/// partial) page: closes the "MATCH filtering happens client-side of the
/// cursor" gap (a selective pattern on a huge keyspace can need many `SCAN`
/// round trips to fill a page) without blocking the caller unboundedly.
#[derive(Debug, Clone, Copy)]
pub struct ScanBudget {
    /// The `SCAN ... COUNT` hint per round trip.
    pub count_hint: u32,
    /// Stop looping once this many round trips' wall-clock time has passed,
    /// returning whatever's been collected so far (page may be short of
    /// `want`, or even empty, on a very sparse pattern).
    pub wall_clock: Duration,
    /// Soft target page size: once at least this many keys are collected,
    /// stop after the *current* `SCAN` round trip completes (never mid-batch
    /// — truncating a batch would drop keys `SCAN` already yielded and won't
    /// yield again, since the cursor has moved past them).
    pub want: usize,
}

/// One page of a keyspace scan. `next_cursor` is `0` both at the very start
/// and at genuine exhaustion; `exhausted` disambiguates the two (mirrors
/// Redis's own `SCAN` cursor convention, but callers shouldn't have to know
/// that convention to use this).
#[derive(Debug, Clone)]
pub struct KvScanPage {
    pub keys: Vec<KeyMeta>,
    pub next_cursor: u64,
    pub exhausted: bool,
}
