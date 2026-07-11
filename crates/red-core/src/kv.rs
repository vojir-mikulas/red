//! Domain types for the `KvDriver` seam (Redis; see `docs/plans/redis.md`).
//! Parallel to the SQL-shaped `ResultPage`/`RowWindow`/`KeySpec` in `lib.rs`,
//! but nothing here assumes a table/column model or an orderable key.

use std::time::Duration;

use crate::Value;

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

/// An opaque keyspace-scan position (see docs/plans/redis.md's cluster
/// fan-out). Callers treat it as a token: begin from [`ScanCursor::START`],
/// echo back whatever [`KvScanPage::next_cursor`] the previous page carried,
/// and stop on `exhausted` — never inspect or construct the `Cluster` shape
/// themselves. On a standalone/Sentinel server it wraps a single `SCAN`
/// cursor; on a Cluster, `SCAN` is per-node, so it also tracks which master
/// is being walked (the driver advances to the next master when one exhausts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanCursor {
    /// Standalone/Sentinel: one `SCAN` cursor (`0` = start/end, disambiguated
    /// by [`KvScanPage::exhausted`]).
    Single(u64),
    /// Cluster: the master node index being scanned plus that node's `SCAN`
    /// cursor. The driver walks masters in order, advancing to `node + 1` at
    /// cursor `0` once a node's scan exhausts.
    Cluster { node: u32, cursor: u64 },
}

impl ScanCursor {
    /// The starting position for a fresh scan or a filter restart. The driver
    /// maps this to "the first master at cursor 0" under a cluster topology.
    pub const START: ScanCursor = ScanCursor::Single(0);
}

impl Default for ScanCursor {
    fn default() -> Self {
        ScanCursor::START
    }
}

/// One page of a keyspace scan. `next_cursor` is what the caller echoes back
/// as the next page's cursor; `exhausted` (not `next_cursor == START`)
/// signals the walk is done, since a `SCAN` cursor of `0` occurs both at the
/// very start and at genuine exhaustion.
#[derive(Debug, Clone)]
pub struct KvScanPage {
    pub keys: Vec<KeyMeta>,
    pub next_cursor: ScanCursor,
    pub exhausted: bool,
}

/// The three collection types pageable via a `*SCAN` command. Lists aren't
/// included: they have no `LSCAN`, so a big list pages by `LRANGE` window
/// instead (see [`KvValue::List`] and the plan's documented limitation on
/// deep-middle list access).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    Hash,
    Set,
    ZSet,
}

/// One element of a hash/set/zset page (`HSCAN`/`SSCAN`/`ZSCAN`).
#[derive(Debug, Clone)]
pub enum KvElement {
    Member(String),
    Field(String, String),
    Scored(String, f64),
}

/// One page of a big collection's elements, from
/// [`KvDriver::read_collection_page`](crate). Stateless like [`KvScanPage`]:
/// `next_cursor` is what the caller echoes back as the next call's `cursor`.
#[derive(Debug, Clone)]
pub struct KvCollectionPage {
    pub elements: Vec<KvElement>,
    pub next_cursor: u64,
    pub exhausted: bool,
}

/// One entry of a Redis stream (`XRANGE`/`XREVRANGE`): its ID (`<ms>-<seq>`,
/// monotonic and unique) and that entry's flat field/value pairs. A stream
/// entry is a small ordered map, not a single value, so it carries its own
/// `Vec` of pairs rather than one scalar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEntry {
    pub id: String,
    pub fields: Vec<(String, String)>,
}

/// One page of a big stream's entries, newest-first (`XREVRANGE`), from
/// [`KvDriver::read_stream_range`](crate). Streams have no `*SCAN` cursor;
/// they page by entry-ID range instead, so unlike [`KvCollectionPage`] the
/// continuation is the oldest ID loaded so far (`next_before`), which the
/// caller feeds back as the next page's exclusive upper bound to walk further
/// back in time. `None` at exhaustion (the start of the stream is reached).
#[derive(Debug, Clone)]
pub struct KvStreamPage {
    pub entries: Vec<StreamEntry>,
    pub next_before: Option<String>,
    pub exhausted: bool,
}

/// One consumer group on a stream (`XINFO GROUPS <key>`): the read-position
/// bookkeeping Redis keeps per group. A stream can carry several independent
/// groups, each with its own last-delivered position, pending set, and pool of
/// consumers — the shape the plan's "consumer-group management" gap is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamGroup {
    pub name: String,
    /// Number of consumers registered in the group (`consumers`).
    pub consumers: u64,
    /// Entries delivered-but-not-acked across the whole group (`pending`).
    pub pending: u64,
    /// The last entry ID the group delivered to any consumer
    /// (`last-delivered-id`).
    pub last_delivered_id: String,
    /// How many entries the group is behind the tip of the stream (`lag`,
    /// Redis 7+). `None` when the server can't compute it (an older server, or
    /// after certain trims Redis reports lag as nil).
    pub lag: Option<i64>,
}

/// One consumer within a group (`XINFO CONSUMERS <key> <group>`): a named
/// reader with its own slice of the group's pending set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamConsumer {
    pub name: String,
    /// Entries delivered to this consumer and not yet acked (`pending`).
    pub pending: u64,
    /// Time since this consumer last interacted with the group (`idle`, ms).
    pub idle: Duration,
}

/// One pending (delivered-but-unacknowledged) entry in a group's PEL, from the
/// extended `XPENDING <key> <group> - + <count>` form: which consumer holds it,
/// how long it's been idle, and how many times it's been delivered. A high
/// `delivery_count` with a large `idle` is the signature of a stuck message the
/// operator reclaims with `XCLAIM` or discards with `XACK`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEntry {
    pub id: String,
    pub consumer: String,
    /// Milliseconds since the entry was last delivered (`idle`).
    pub idle: Duration,
    /// How many times the entry has been delivered (`delivery-count`).
    pub delivery_count: u64,
}

/// Which consumer-group write just completed, echoed on
/// [`crate::kv`]'s stream-action reply so the UI knows what it applied (and
/// can word its toast/refresh accordingly) without a reply type per verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamAction {
    /// `XACK`: the entries were acknowledged and dropped from the PEL.
    Ack,
    /// `XCLAIM`: the entries were reassigned to another consumer.
    Claim,
}

/// A consumer-group write to apply to a group's pending set, carried through
/// `Command::KvStreamAction`. Mirrors [`KvEdit`]'s "one enum per family of
/// writes" shape, kept separate because these are stream/group-scoped (they
/// carry a group name and entry IDs, not a bare key) and gated on the same
/// read-only check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvStreamActionReq {
    /// `XACK <key> <group> <ids...>`.
    Ack { ids: Vec<String> },
    /// `XCLAIM <key> <group> <consumer> <min_idle_ms> <ids...> JUSTID`.
    Claim {
        consumer: String,
        min_idle_ms: u64,
        ids: Vec<String>,
    },
}

impl KvStreamActionReq {
    /// Which verb this is, for the reply the UI pattern-matches on.
    pub fn action(&self) -> StreamAction {
        match self {
            KvStreamActionReq::Ack { .. } => StreamAction::Ack,
            KvStreamActionReq::Claim { .. } => StreamAction::Claim,
        }
    }
}

/// A collection value below vs. at/above the small-collection threshold (see
/// docs/plans/redis.md's "big collections inside a single key"): `Loaded`
/// carries every element, fetched once in `read_value`; `Large` carries only
/// the O(1) length probe (`HLEN`/`SCARD`/`ZCARD`/`LLEN`) and nothing else —
/// the caller pages the rest on demand via `read_collection_page`/
/// `read_list_window` rather than ever issuing a `*GETALL`/`*MEMBERS` on it.
#[derive(Debug, Clone)]
pub enum KvCollection<T> {
    Loaded(Vec<T>),
    Large { len: u64 },
}

/// One key's value, from `KvDriver::read_value`.
#[derive(Debug, Clone)]
pub enum KvValue {
    /// Capped like a SQL cell ([`Value::Capped`]) so a huge blob never fully
    /// materializes just to preview it.
    Str(Value),
    Hash(KvCollection<(String, String)>),
    Set(KvCollection<String>),
    ZSet(KvCollection<(String, f64)>),
    List(KvCollection<String>),
    /// A stream, newest-first. `Loaded` below the small-collection threshold
    /// (one `XREVRANGE + -`); `Large` carries only the `XLEN`, and the caller
    /// pages the newest entries on demand via
    /// [`KvDriver::read_stream_range`](crate) rather than this ever loading a
    /// huge stream whole.
    Stream(KvCollection<StreamEntry>),
    /// A type this build has no preview for yet. Distinct from `Ok(None)`
    /// (the key doesn't exist): the key is real, its value just isn't shown.
    Unsupported(KvType),
}

/// One in-grid edit (see `KvDriver::set_string`/`set_field`/`set_ttl`/
/// `rename_key`/`delete_keys`), carried through `Command::KvApplyEdit` and
/// echoed back on `Event::KvEditApplied` so the UI can pattern-match what
/// just succeeded without a separate event type per edit kind (mirrors
/// `FetchRun`'s "echo the request back" shape).
#[derive(Debug, Clone)]
pub enum KvEdit {
    SetString {
        key: String,
        value: String,
        ttl: Option<Duration>,
    },
    SetField {
        key: String,
        field: String,
        value: String,
    },
    SetTtl {
        key: String,
        ttl: Option<Duration>,
    },
    Rename {
        from: String,
        to: String,
    },
    Delete {
        keys: Vec<String>,
    },
}

/// A generic RESP reply (the console needs to render *any* command's result,
/// not one per command), and the redis crate's own `Value` isn't `Send`-free
/// of engine-specific dependencies for a wire type shared across the
/// service/UI boundary, so this is a small hand-rolled mirror of RESP2/3's
/// shapes.
#[derive(Debug, Clone)]
pub enum RespValue {
    Nil,
    Ok,
    Int(i64),
    Double(f64),
    Bool(bool),
    /// A short status/simple-string reply (rendered plainly, no quoting).
    Simple(String),
    /// A bulk string, decoded lossily if not valid UTF-8 (the console is a
    /// text log, not a hex viewer).
    Bulk(String),
    Array(Vec<RespValue>),
    /// A server error reply (e.g. `WRONGTYPE`), rendered distinctly (not the
    /// same as `KvDriver::command` itself returning `Err`, which is a
    /// transport/connection failure).
    Error(String),
}

/// How a raw console command line is classified, for the read-only gate and
/// the destructive-command confirm (see docs/plans/redis.md's console
/// phase). Unknown commands default to `Write` (the safer default under a
/// read-only connection) but never `Destructive` on their own, so a typo
/// doesn't trigger a confirm prompt for no reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandClass {
    Read,
    Write,
    /// Wide-blast-radius or irreversible: gated behind a confirm even on a
    /// writable connection (`FLUSHALL`, `FLUSHDB`, `DEL`, `UNLINK`, `SWAPDB`,
    /// `SHUTDOWN`).
    Destructive,
}

/// Every read-only-safe command this build knows about. Anything not listed
/// here classifies as `Write` (blocked on a read-only connection) even if
/// it's actually a read in real Redis — a conservative default is cheaper
/// than an exhaustive command table, and the list already covers the
/// commands a keyspace-browsing tool's console gets used for.
const READ_COMMANDS: &[&str] = &[
    "GET",
    "MGET",
    "STRLEN",
    "GETRANGE",
    "TYPE",
    "TTL",
    "PTTL",
    "EXISTS",
    "SCAN",
    "KEYS",
    "HGET",
    "HGETALL",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HMGET",
    "HSCAN",
    "HEXISTS",
    "HSTRLEN",
    "SMEMBERS",
    "SCARD",
    "SISMEMBER",
    "SSCAN",
    "SRANDMEMBER",
    "SINTER",
    "SUNION",
    "SDIFF",
    "ZRANGE",
    "ZREVRANGE",
    "ZSCORE",
    "ZCARD",
    "ZSCAN",
    "ZRANK",
    "ZREVRANK",
    "ZCOUNT",
    "LRANGE",
    "LLEN",
    "LINDEX",
    "XRANGE",
    "XREVRANGE",
    "XLEN",
    "DBSIZE",
    "INFO",
    "PING",
    "ECHO",
    "TIME",
    "COMMAND",
    "OBJECT",
    "MEMORY",
    "LASTSAVE",
    "RANDOMKEY",
    "DUMP",
];

/// Commands gated behind the destructive-command confirm even though the
/// connection is writable.
const DESTRUCTIVE_COMMANDS: &[&str] =
    &["FLUSHALL", "FLUSHDB", "SHUTDOWN", "DEL", "UNLINK", "SWAPDB"];

pub fn classify_command(argv: &[String]) -> CommandClass {
    let Some(name) = argv.first() else {
        return CommandClass::Read;
    };
    let upper = name.to_ascii_uppercase();
    if DESTRUCTIVE_COMMANDS.contains(&upper.as_str()) {
        CommandClass::Destructive
    } else if READ_COMMANDS.contains(&upper.as_str()) {
        CommandClass::Read
    } else {
        CommandClass::Write
    }
}

/// Split a console command line into argv, `redis-cli`-style: whitespace
/// separated, `'single'` and `"double"` quoting to include whitespace in one
/// argument, `\`-escaping inside double quotes. Malformed quoting (an
/// unterminated quote) still returns the tokens parsed so far rather than
/// failing outright, so the console can report "unterminated quote" as a
/// normal command error instead of a special UI state.
pub fn tokenize_command(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut tok = String::new();
        match chars.peek() {
            Some('\'') => {
                chars.next();
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    tok.push(c);
                }
            }
            Some('"') => {
                chars.next();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(next) = chars.next() {
                                tok.push(next);
                            }
                        }
                        other => tok.push(other),
                    }
                }
            }
            _ => {
                while matches!(chars.peek(), Some(c) if !c.is_whitespace()) {
                    tok.push(chars.next().unwrap());
                }
            }
        }
        out.push(tok);
    }
    out
}

/// One Pub/Sub message delivered to a pattern subscription (`PSUBSCRIBE`).
#[derive(Debug, Clone)]
pub struct KvMessage {
    pub channel: String,
    pub payload: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_on_whitespace() {
        assert_eq!(tokenize_command("SET foo bar"), vec!["SET", "foo", "bar"]);
        assert_eq!(tokenize_command("  GET   foo  "), vec!["GET", "foo"]);
        assert_eq!(tokenize_command(""), Vec::<String>::new());
    }

    #[test]
    fn tokenize_honors_single_and_double_quotes() {
        assert_eq!(
            tokenize_command(r#"SET foo "hello world""#),
            vec!["SET", "foo", "hello world"]
        );
        assert_eq!(
            tokenize_command("SET foo 'hello world'"),
            vec!["SET", "foo", "hello world"]
        );
    }

    #[test]
    fn tokenize_unescapes_backslashes_inside_double_quotes() {
        assert_eq!(
            tokenize_command(r#"SET foo "a\"b""#),
            vec!["SET", "foo", "a\"b"]
        );
    }

    #[test]
    fn classify_command_knows_reads_writes_and_destructive() {
        assert_eq!(
            classify_command(&["GET".into(), "foo".into()]),
            CommandClass::Read
        );
        assert_eq!(
            classify_command(&["SET".into(), "foo".into(), "bar".into()]),
            CommandClass::Write
        );
        assert_eq!(
            classify_command(&["DEL".into(), "foo".into()]),
            CommandClass::Destructive
        );
        assert_eq!(
            classify_command(&["FLUSHALL".into()]),
            CommandClass::Destructive
        );
        // Case-insensitive, and an unknown command defaults to Write (the
        // conservative choice under a read-only connection).
        assert_eq!(
            classify_command(&["get".into(), "foo".into()]),
            CommandClass::Read
        );
        assert_eq!(
            classify_command(&["SOMENEWCOMMAND".into()]),
            CommandClass::Write
        );
        assert_eq!(classify_command(&[]), CommandClass::Read);
    }
}
