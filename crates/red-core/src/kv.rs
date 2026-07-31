//! Domain types for the `KvDriver` seam (Redis).
//! Parallel to the SQL-shaped `ResultPage`/`RowWindow`/`KeySpec` in `lib.rs`,
//! but nothing here assumes a table/column model or an orderable key.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::Value;

mod json;
mod modules;

pub use json::{
    JSON_INLINE_STR_MAX, JSON_NODE_WINDOW, JSON_WHOLE_DOC_MAX, JsonDoc, JsonFetch, JsonKind,
    JsonNode, JsonNodeView, JsonPath, JsonSeg, JsonSyntaxError, json_fetch_mode,
    json_unwrap_singleton, validate_json,
};
pub use modules::{KvModule, KvModules};

/// The wire name RedisJSON answers `TYPE` with, and the argument
/// `SCAN ... TYPE` takes to filter for one. Never shown to a user: it is the
/// module's internal spelling, while [`KvType::label`] is the word for a reader.
const REJSON_WIRE_TYPE: &str = "ReJSON-RL";

/// A key's Redis data type, from `TYPE`. `Other` covers types this build
/// doesn't render a dedicated inspector for yet (bitmap, hyperloglog — both
/// report as `string` so rarely land here; another module type would).
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
    /// A RedisJSON document. Its own variant rather than `Other` because RED
    /// reads, renders and edits it; the module's wire spelling stays inside
    /// [`KvType::wire_type`].
    Json,
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
            REJSON_WIRE_TYPE => Some(KvType::Json),
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
            KvType::Json => "json",
            KvType::Other(s) => s,
        }
    }

    /// The name this type answers `TYPE` with, i.e. the argument
    /// `SCAN ... TYPE` filters on. Identical to [`label`](Self::label) for the
    /// built-in types; a module type spells itself differently on the wire than
    /// it reads in a UI, and that difference must never be resolved by
    /// re-stringifying a label.
    pub fn wire_type(&self) -> &str {
        match self {
            KvType::Json => REJSON_WIRE_TYPE,
            other => other.label(),
        }
    }
}

/// One key's row in the keyspace browser: everything the grid's `key | type | TTL |
/// size | encoding` columns need, fetched in one pipelined round trip per scanned
/// batch.
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

/// An opaque keyspace-scan position. Callers treat it as a token: begin from
/// [`ScanCursor::START`], echo back whatever [`KvScanPage::next_cursor`] the previous
/// page carried, and stop on `exhausted` — never inspect or construct the `Cluster`
/// shape themselves. On a standalone/Sentinel server it wraps a single `SCAN` cursor;
/// on a Cluster, `SCAN` is per-node, so it also tracks which master is being walked
/// (the driver advances to the next master when one exhausts).
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

/// A collection value below vs. at/above the small-collection threshold: `Loaded`
/// carries every element, fetched once in `read_value`; `Large` carries only the O(1)
/// length probe (`HLEN`/`SCARD`/`ZCARD`/`LLEN`) and nothing else — the caller pages the
/// rest on demand via `read_collection_page`/`read_list_window` rather than ever
/// issuing a `*GETALL`/`*MEMBERS` on it.
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
    /// A RedisJSON document. `Loaded` below [`JSON_WHOLE_DOC_MAX`] (one
    /// `JSON.GET $`); `Lazy` carries only the root's shape and one window of its
    /// children, with each subtree fetched on expand by path — a document can
    /// reach hundreds of megabytes, and a preview must never pull one whole.
    Json(JsonDoc),
    /// A type this build has no preview for yet. Distinct from `Ok(None)`
    /// (the key doesn't exist): the key is real, its value just isn't shown.
    Unsupported(KvType),
}

/// A key captured just before deletion, for the recycle bin's undo (see
/// `KvDriver::dump_key`/`restore_key`): its serialized value (`DUMP`) and the
/// expiry to re-apply (`PTTL`). Held by the UI after a delete and sent back in
/// `Command::KvRestoreKeys` on undo. The payload is opaque `RESTORE` wire bytes,
/// never inspected — only round-tripped through the same server.
#[derive(Debug, Clone)]
pub struct RecycledKey {
    pub key: String,
    /// Remaining expiry to re-apply on restore; `None` = no expiry.
    pub ttl: Option<Duration>,
    /// The `DUMP` serialization, fed verbatim to `RESTORE`.
    pub payload: Vec<u8>,
}

/// One in-grid edit (see `KvDriver::set_string`/`set_field`/`set_ttl`/
/// `rename_key`/`delete_keys`), carried through `Command::KvApplyEdit` and
/// echoed back on `Event::KvEditApplied` so the UI can pattern-match what
/// just succeeded without a separate event type per edit kind (mirrors
/// `FetchRun`'s "echo the request back" shape).
/// How a `SetString` write should treat the key's expiry. `SET`'s default
/// (plain, no option) clears any existing TTL, which is wrong when the edit is
/// only meant to change the value — so the intent is explicit rather than
/// inferred from an `Option<Duration>` snapshot that would otherwise reset the
/// countdown or lose sub-second precision.
#[derive(Debug, Clone, Copy)]
pub enum StringTtl {
    /// Leave the current expiry untouched (`SET ... KEEPTTL`): editing a value
    /// must not reset the key's countdown.
    Keep,
    /// Write the key with no expiry (plain `SET`): the default for a new key.
    Clear,
    /// Apply an explicit expiry with millisecond precision (`SET ... PX <ms>`).
    Set(Duration),
}

#[derive(Debug, Clone)]
pub enum KvEdit {
    SetString {
        key: String,
        value: String,
        ttl: StringTtl,
    },
    SetField {
        key: String,
        field: String,
        value: String,
    },
    /// `HDEL key field [field ...]` — remove one or more hash fields.
    HashDelete {
        key: String,
        fields: Vec<String>,
    },
    /// `SADD key member [member ...]` — add set members (also creates the key).
    SetAdd {
        key: String,
        members: Vec<String>,
    },
    /// `SREM key member [member ...]` — remove set members.
    SetRemove {
        key: String,
        members: Vec<String>,
    },
    /// Rename a set member: `SREM key old` then `SADD key new`, so an inline
    /// edit of a set element is one echoed edit rather than a delete + add the
    /// UI has to sequence itself.
    SetReplace {
        key: String,
        old: String,
        new: String,
    },
    /// `ZADD key score member` — add a sorted-set member or overwrite its
    /// score (Redis upserts on the member), covering both add and score-edit.
    ZSetAdd {
        key: String,
        member: String,
        score: f64,
    },
    /// `ZREM key member [member ...]` — remove sorted-set members.
    ZSetRemove {
        key: String,
        members: Vec<String>,
    },
    /// `LSET key index value` — overwrite the list element at `index`.
    ListSet {
        key: String,
        index: i64,
        value: String,
    },
    /// `LPUSH`/`RPUSH key value` — prepend (`head`) or append a list element
    /// (also creates the key).
    ListPush {
        key: String,
        value: String,
        head: bool,
    },
    /// `LREM key count value` — remove list elements equal to `value`. Used for
    /// value-targeted removals; a positional row delete uses [`Self::ListRemoveAt`]
    /// instead so a duplicate value can't take the wrong element.
    ListRemove {
        key: String,
        count: i64,
        value: String,
    },
    /// Delete the list element at a specific `index` (the placeholder dance:
    /// `LSET` a sentinel there, then `LREM` it). This is what a UI row-delete
    /// sends, so clicking the trash on row N removes exactly element N even when
    /// the list holds duplicate values.
    ListRemoveAt {
        key: String,
        index: i64,
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
    /// `XADD key * field value [field value ...]` — append an entry with a
    /// server-assigned id (also creates the stream). The one write that creates
    /// a stream, so the "New key" popover can offer Stream like the other types.
    StreamAdd {
        key: String,
        fields: Vec<(String, String)>,
    },
    /// `JSON.SET key <path> <value>` — write one node of a RedisJSON document,
    /// or the whole document at [`JsonPath::root`]. `value` is validated as JSON
    /// before it leaves RED (see [`validate_json`]) so a typo fails with an
    /// offset here rather than as `ERR expected value` at the server. Also how
    /// the "New key" popover creates a JSON key.
    JsonSet {
        key: String,
        path: JsonPath,
        value: String,
    },
    /// `JSON.DEL key <path>` — remove one node. At [`JsonPath::root`] this
    /// deletes the key itself, which is why it classifies as destructive.
    JsonDelete {
        key: String,
        path: JsonPath,
    },
}

/// One entry of the server's slow-command log (`SLOWLOG GET`), for the
/// diagnostics panel. Redis
/// records a command here when its execution time exceeds
/// `slowlog-log-slower-than` microseconds; the log is a fixed-size ring, so
/// this is always a bounded, recent view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowlogEntry {
    /// The entry's unique, monotonically increasing id.
    pub id: i64,
    /// Unix timestamp (seconds, server clock) the command ran at.
    pub time_secs: i64,
    /// Execution time in microseconds (the reason it was logged).
    pub micros: u64,
    /// The command and its arguments as the server recorded them. Long
    /// arguments are truncated by the server itself (to
    /// `slowlog-max-len`-adjacent limits), so this is display-safe as-is.
    pub argv: Vec<String>,
    /// The client address (`ip:port`), empty on servers predating that field.
    pub client: String,
    /// The client's `CLIENT SETNAME`, empty if unset or unsupported.
    pub client_name: String,
}

/// One connected client, from a `CLIENT LIST` reply line. Only the fields the viewer
/// surfaces are kept; `CLIENT LIST` carries many more, but a curated set reads better
/// than a raw dump and stays stable across server versions that add/rename fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientInfo {
    /// The client's unique connection id (`id=`), the handle `CLIENT KILL ID`
    /// takes.
    pub id: i64,
    /// The client's address (`addr=`, `ip:port`).
    pub addr: String,
    /// The client's `CLIENT SETNAME` (`name=`), empty if unset.
    pub name: String,
    /// The selected logical database (`db=`).
    pub db: i64,
    /// Seconds the connection has been alive (`age=`).
    pub age: u64,
    /// Seconds the connection has been idle (`idle=`).
    pub idle: u64,
    /// Client flags (`flags=`, e.g. `N`, `S` for a replica, `M` for a master).
    pub flags: String,
    /// The last command the client ran (`cmd=`, e.g. `client|list`).
    pub cmd: String,
    /// The RESP protocol version the client negotiated (`resp=`), empty on
    /// servers predating that field.
    pub resp: String,
}

/// Parse a single `CLIENT LIST` line into a [`ClientInfo`]. Each line is
/// space-separated `field=value` pairs; unknown fields are ignored and missing
/// ones default. Returns `None` for a line with no `id` (a torn or blank line),
/// which the caller drops rather than surfacing an id-less client.
pub fn parse_client_line(line: &str) -> Option<ClientInfo> {
    let mut c = ClientInfo::default();
    let mut saw_id = false;
    for tok in line.split_whitespace() {
        let Some((k, v)) = tok.split_once('=') else {
            continue;
        };
        match k {
            "id" => match v.parse::<i64>() {
                Ok(id) => {
                    c.id = id;
                    saw_id = true;
                }
                Err(_) => return None,
            },
            "addr" => c.addr = v.to_string(),
            "name" => c.name = v.to_string(),
            "db" => c.db = v.parse().unwrap_or(0),
            "age" => c.age = v.parse().unwrap_or(0),
            "idle" => c.idle = v.parse().unwrap_or(0),
            "flags" => c.flags = v.to_string(),
            "cmd" => c.cmd = v.to_string(),
            "resp" => c.resp = v.to_string(),
            _ => {}
        }
    }
    saw_id.then_some(c)
}

/// Parse a whole `CLIENT LIST` reply (one client per line) into
/// [`ClientInfo`]s, dropping any torn/blank line (see [`parse_client_line`]).
pub fn parse_client_list(reply: &str) -> Vec<ClientInfo> {
    reply.lines().filter_map(parse_client_line).collect()
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

impl RespValue {
    /// The reply's text, for the scalar string shapes. `None` for a container,
    /// an error, or a non-textual scalar — so a caller reading a named field out
    /// of a flat map reply can't silently accept the wrong shape.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            RespValue::Simple(s) | RespValue::Bulk(s) => Some(s),
            _ => None,
        }
    }

    /// The reply's integer value, accepting both a RESP integer and the bulk
    /// string an older server may answer with in the same position.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            RespValue::Int(i) => Some(*i),
            RespValue::Simple(s) | RespValue::Bulk(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// A rough resident-byte estimate of the reply tree: the string payloads plus
    /// a small per-node overhead. The console keeps replies for its scrollback, so
    /// a `LRANGE bigkey 0 -1` on a huge list would otherwise pin the whole thing;
    /// callers use this to cap what they retain.
    pub fn approx_bytes(&self) -> usize {
        match self {
            RespValue::Simple(s) | RespValue::Bulk(s) | RespValue::Error(s) => s.len() + 16,
            RespValue::Array(items) => {
                16 + items.iter().map(RespValue::approx_bytes).sum::<usize>()
            }
            _ => 16,
        }
    }
}

pub use crate::OpClass;

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
    "LASTSAVE",
    "RANDOMKEY",
    "DUMP",
    // RedisJSON reads. `JSON.DEBUG` is listed despite the bare `DEBUG` being
    // destructive: it is a separate command whose only subcommands report a
    // document's memory and depth.
    "JSON.GET",
    "JSON.MGET",
    "JSON.TYPE",
    "JSON.STRLEN",
    "JSON.ARRLEN",
    "JSON.OBJLEN",
    "JSON.OBJKEYS",
    "JSON.ARRINDEX",
    "JSON.RESP",
    "JSON.DEBUG",
];

/// `(command, subcommand)` pairs whose *specific form* is a read, for commands
/// whose other subcommands are not (`MEMORY PURGE` mutates allocator state, so
/// `MEMORY` can't sit on the whole-command read list).
const READ_SUBCOMMANDS: &[(&str, &str)] = &[
    ("MEMORY", "USAGE"),
    ("MEMORY", "STATS"),
    ("MEMORY", "DOCTOR"),
    ("MEMORY", "MALLOC-STATS"),
    ("MODULE", "LIST"),
];

/// Commands that are destructive purely by name (any invocation), gated behind
/// the confirm even on a writable connection.
///
/// `EVAL`/`EVALSHA`/`FCALL` are here because a script runs *any* command —
/// `EVAL "redis.call('flushall')" 0` is `FLUSHALL` with extra steps — so a
/// name-based gate that skipped them would be a one-line bypass. `MIGRATE`
/// moves keys to an arbitrary remote server (exfiltration shaped like a copy).
///
/// The second group destroys data without saying so in its name, which is exactly
/// why it belongs here: gating on the obvious verbs only means a hostile or
/// careless line reads as ordinary while doing the same damage. `GETDEL` is `DEL`
/// with a read attached; `LTRIM k 1 0` empties a list and `XTRIM`/`SPOP` remove
/// entries the same way; `RENAME` overwrites its destination (which is why the
/// driver's own rename path uses `RENAMENX`); `RESTORE … REPLACE` and `COPY …
/// REPLACE` overwrite whatever is already there; `PERSIST`/`EXPIRE` at 0 removes
/// the key outright.
const DESTRUCTIVE_COMMANDS: &[&str] = &[
    "FLUSHALL",
    "FLUSHDB",
    "SHUTDOWN",
    "DEL",
    "UNLINK",
    "SWAPDB",
    "DEBUG",
    "REPLICAOF",
    "SLAVEOF",
    "FAILOVER",
    "EVAL",
    "EVALSHA",
    "FCALL",
    "MIGRATE",
    // Destructive by effect rather than by name.
    "GETDEL",
    "LTRIM",
    "XTRIM",
    "SPOP",
    "RENAME",
    "XDEL",
    "LPOP",
    "RPOP",
    "LREM",
    "SREM",
    "ZREM",
    "ZREMRANGEBYSCORE",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYLEX",
    "HDEL",
    "SETRANGE",
    "RESTORE",
    "COPY",
    // RedisJSON removals. `JSON.DEL`/`JSON.FORGET` at path `$` delete the key
    // outright, and `JSON.CLEAR`/`ARRPOP`/`ARRTRIM` discard data in place.
    "JSON.DEL",
    "JSON.FORGET",
    "JSON.CLEAR",
    "JSON.ARRPOP",
    "JSON.ARRTRIM",
];

/// `(command, subcommand)` pairs that are destructive only in a specific form,
/// so the read/introspection form of the same command (`CONFIG GET`,
/// `CLIENT LIST`, `ACL WHOAMI`, ...) stays non-destructive. The `LOAD`/`RESTORE`
/// forms persist server-side code, the flip side of the `FLUSH` forms already
/// gated here.
const DESTRUCTIVE_SUBCOMMANDS: &[(&str, &str)] = &[
    ("CONFIG", "SET"),
    ("CONFIG", "RESETSTAT"),
    ("CONFIG", "REWRITE"),
    ("CLIENT", "KILL"),
    ("CLIENT", "UNPAUSE"),
    ("ACL", "SETUSER"),
    ("ACL", "DELUSER"),
    ("ACL", "LOAD"),
    ("SCRIPT", "FLUSH"),
    ("SCRIPT", "LOAD"),
    ("FUNCTION", "FLUSH"),
    ("FUNCTION", "LOAD"),
    ("FUNCTION", "RESTORE"),
    ("FUNCTION", "DELETE"),
    ("CLUSTER", "RESET"),
    ("CLUSTER", "FAILOVER"),
    ("CLUSTER", "FORGET"),
    ("CLUSTER", "SETSLOT"),
    ("XGROUP", "DESTROY"),
    ("MODULE", "LOAD"),
    ("MODULE", "LOADEX"),
    ("MODULE", "UNLOAD"),
];

/// Classify a raw console command line for the read-only gate and the
/// destructive-command confirm.
/// Unknown commands default to [`OpClass::Write`] (the safer default under a
/// read-only connection) but never [`OpClass::Destructive`] on their own, so a
/// typo doesn't trigger a confirm prompt for no reason.
pub fn classify_command(argv: &[String]) -> OpClass {
    let Some(name) = argv.first() else {
        return OpClass::Read;
    };
    let upper = name.to_ascii_uppercase();
    let sub = argv.get(1).map(|s| s.to_ascii_uppercase());
    let destructive = DESTRUCTIVE_COMMANDS.contains(&upper.as_str())
        || sub
            .as_deref()
            .is_some_and(|s| DESTRUCTIVE_SUBCOMMANDS.contains(&(upper.as_str(), s)));
    if destructive {
        OpClass::Destructive
    } else if READ_COMMANDS.contains(&upper.as_str())
        || sub
            .as_deref()
            .is_some_and(|s| READ_SUBCOMMANDS.contains(&(upper.as_str(), s)))
    {
        OpClass::Read
    } else {
        OpClass::Write
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
                    #[allow(clippy::unwrap_used, reason = "peek() above proved a next char")]
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

/// One decoded keyspace notification: a key and the event that happened to it, in which
/// logical database. Redis delivers these over Pub/Sub on two mirror channel families —
/// `__keyspace@<db>__:<key>` (payload is the event) and `__keyevent@<db>__:<event>`
/// (payload is the key) — both of which decode to this same shape via
/// [`parse_keyspace_channel`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyspaceEvent {
    pub db: i64,
    /// The event name (`set`, `del`, `expired`, `lpush`, …).
    pub event: String,
    /// The key the event happened to.
    pub key: String,
}

/// Decode a keyspace-notification Pub/Sub message into a [`KeyspaceEvent`],
/// handling both channel families (see [`KeyspaceEvent`]). Returns `None` for a
/// channel that isn't a keyspace notification (so an ordinary Pub/Sub message
/// on an unrelated channel is ignored rather than mis-parsed).
pub fn parse_keyspace_channel(channel: &str, payload: &str) -> Option<KeyspaceEvent> {
    let (is_keyspace, rest) = if let Some(r) = channel.strip_prefix("__keyspace@") {
        (true, r)
    } else {
        (false, channel.strip_prefix("__keyevent@")?)
    };
    // `rest` is `<db>__:<suffix>`: the db index, then the key (keyspace) or the
    // event name (keyevent).
    let (db_str, suffix) = rest.split_once("__:")?;
    let db = db_str.parse::<i64>().ok()?;
    let (event, key) = if is_keyspace {
        // `__keyspace@0__:mykey` → key is the suffix, event is the payload.
        (payload.to_string(), suffix.to_string())
    } else {
        // `__keyevent@0__:expired` → event is the suffix, key is the payload.
        (suffix.to_string(), payload.to_string())
    };
    Some(KeyspaceEvent { db, event, key })
}

/// The two keyspace-notification channel families a watcher can subscribe to
/// (see [`KeyspaceEvent`]). Only one is subscribed at a time — every real
/// operation fires on *both*, so watching both would double every event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyspaceScope {
    /// `__keyevent@*__:*` — grouped by event; the classic "what's happening"
    /// firehose (payload is the affected key).
    ByEvent,
    /// `__keyspace@*__:*` — grouped by key (payload is the event name).
    ByKey,
}

impl KeyspaceScope {
    /// The `PSUBSCRIBE` glob for this scope, across all logical databases.
    pub fn pattern(self) -> &'static str {
        match self {
            KeyspaceScope::ByEvent => "__keyevent@*__:*",
            KeyspaceScope::ByKey => "__keyspace@*__:*",
        }
    }
}

/// A persisted, point-in-time keyspace analysis report: a type/namespace/expiry rollup
/// over a sample of the keyspace. Distinct from the ephemeral biggest-keys sampler in
/// that it's saved per connection and can be revisited after a restart. Derives serde
/// (behind the `serde` feature) so the app edge can store it as JSON.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub struct RedisAnalysis {
    /// Unix seconds the report was generated (server-independent local clock),
    /// for the "as of …" line and to tell a stale saved report from a fresh one.
    pub generated_at: i64,
    /// Keys actually sampled and rolled up. Equals a full walk when
    /// `truncated` is false.
    pub sampled: u64,
    /// `DBSIZE` at sample time (the whole logical DB), so the UI can say
    /// "sampled X of Y". `0` if it wasn't known.
    pub total_keys: u64,
    /// Sum of `approx_bytes` (sampled `MEMORY USAGE`) across sampled keys.
    pub total_bytes: u64,
    /// True when the sample stopped at a budget bound rather than walking the
    /// whole keyspace — the rollup is then an estimate, not exhaustive.
    pub truncated: bool,
    /// Per-type counts + memory, largest by memory first.
    pub types: Vec<TypeStat>,
    /// Top key-name prefixes (up to the first `:`) by memory, largest first.
    pub namespaces: Vec<NamespaceStat>,
    pub ttl: TtlSummary,
}

/// One data-type's slice of the keyspace (`string`/`hash`/…), for
/// [`RedisAnalysis::types`].
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeStat {
    /// The `KvType` label (`"string"`, `"hash"`, …); a plain string so a
    /// module type's name round-trips through persistence.
    pub kv_type: String,
    pub count: u64,
    pub bytes: u64,
}

/// One key-name namespace (the prefix up to the first `:`, or `"(no prefix)"`
/// for a key with no delimiter), for [`RedisAnalysis::namespaces`].
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceStat {
    pub prefix: String,
    pub count: u64,
    pub bytes: u64,
}

/// The expiry breakdown of the sampled keyspace: how many keys never expire vs.
/// expire, bucketed by how soon (for [`RedisAnalysis::ttl`]).
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TtlSummary {
    /// Keys with no expiry set (`PTTL` reported `-1`).
    pub persistent: u64,
    /// Keys expiring in under an hour.
    pub under_hour: u64,
    /// Keys expiring in the next hour-to-day window.
    pub under_day: u64,
    /// Keys expiring in the next day-to-week window.
    pub under_week: u64,
    /// Keys expiring more than a week out.
    pub over_week: u64,
}

impl TtlSummary {
    /// Total keys that carry an expiry (everything but `persistent`).
    pub fn with_ttl(&self) -> u64 {
        self.under_hour + self.under_day + self.under_week + self.over_week
    }
}

/// The label used for keys with no `:` delimiter in the namespace rollup.
pub const NO_PREFIX_LABEL: &str = "(no prefix)";

/// The number of top namespaces [`analyze_keyspace`] keeps.
pub const ANALYSIS_TOP_NAMESPACES: usize = 30;

/// Roll a sample of scanned key metadata up into a [`RedisAnalysis`]: total
/// memory, a per-type breakdown, the top key-name prefixes by memory, and an
/// expiry-time summary. Pure and UI-free so it's unit-testable; the app calls
/// it once a keyspace sample finishes, then persists the result.
///
/// `total_keys` is the server's `DBSIZE` (0 if unknown), `truncated` says the
/// sample stopped at a budget bound, and `generated_at` is the local
/// wall-clock stamp (passed in rather than read here to keep this deterministic).
pub fn analyze_keyspace(
    keys: &[KeyMeta],
    total_keys: u64,
    truncated: bool,
    generated_at: i64,
) -> RedisAnalysis {
    use std::collections::HashMap;

    let mut total_bytes = 0u64;
    let mut by_type: HashMap<String, (u64, u64)> = HashMap::new();
    let mut by_ns: HashMap<&str, (u64, u64)> = HashMap::new();
    let mut ttl = TtlSummary::default();

    for k in keys {
        total_bytes = total_bytes.saturating_add(k.approx_bytes);

        let t = by_type.entry(k.kv_type.label().to_string()).or_default();
        t.0 += 1;
        t.1 = t.1.saturating_add(k.approx_bytes);

        let ns = namespace_of(&k.key);
        let n = by_ns.entry(ns).or_default();
        n.0 += 1;
        n.1 = n.1.saturating_add(k.approx_bytes);

        match k.ttl {
            None => ttl.persistent += 1,
            Some(d) => {
                let secs = d.as_secs();
                if secs < 3_600 {
                    ttl.under_hour += 1;
                } else if secs < 86_400 {
                    ttl.under_day += 1;
                } else if secs < 604_800 {
                    ttl.under_week += 1;
                } else {
                    ttl.over_week += 1;
                }
            }
        }
    }

    let mut types: Vec<TypeStat> = by_type
        .into_iter()
        .map(|(kv_type, (count, bytes))| TypeStat {
            kv_type,
            count,
            bytes,
        })
        .collect();
    // Biggest by memory first, name as a stable tiebreak.
    types.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| a.kv_type.cmp(&b.kv_type))
    });

    let mut namespaces: Vec<NamespaceStat> = by_ns
        .into_iter()
        .map(|(prefix, (count, bytes))| NamespaceStat {
            prefix: prefix.to_string(),
            count,
            bytes,
        })
        .collect();
    namespaces.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.prefix.cmp(&b.prefix)));
    namespaces.truncate(ANALYSIS_TOP_NAMESPACES);

    RedisAnalysis {
        generated_at,
        sampled: keys.len() as u64,
        total_keys,
        total_bytes,
        truncated,
        types,
        namespaces,
        ttl,
    }
}

/// The namespace a key rolls up under: everything before its first `:`
/// delimiter (Redis's near-universal key-hierarchy convention), or
/// [`NO_PREFIX_LABEL`] for a flat key with no delimiter. Grouping delimiter-less
/// keys together keeps a keyspace of unique flat keys from exploding the rollup
/// into one namespace per key.
fn namespace_of(key: &str) -> &str {
    match key.split_once(':') {
        Some((prefix, _)) if !prefix.is_empty() => prefix,
        _ => NO_PREFIX_LABEL,
    }
}

/// One inferred key *template* and the sampled keys that match it, from
/// [`infer_key_templates`]. A Redis keyspace has no schema, but it almost always
/// has a shape (`user:{id}:sessions`, `cache:v2:product:{sku}`); this is that
/// shape, made explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyTemplate {
    /// Segments joined by `:`, with each variable segment written `*`.
    pub pattern: String,
    pub count: u64,
    pub bytes: u64,
    /// Types seen under this template, most common first. More than one entry
    /// means the template covers keys of mixed type, which is usually a bug in
    /// the naming rather than an intent.
    pub types: Vec<(String, u64)>,
    /// How many matched keys carry an expiry.
    pub with_ttl: u64,
    /// The median remaining expiry of the keys that carry one. Median rather
    /// than mean because a countdown sample is dominated by where in their life
    /// the keys happen to be, and one just-refreshed key would drag a mean.
    pub median_ttl: Option<Duration>,
}

/// How many distinct values a segment position may take before it is read as a
/// variable rather than a literal. Above this, enumerating the branch would
/// produce one template per key, which is the noise the rollup exists to remove.
const TEMPLATE_MAX_FANOUT: usize = 12;

/// How deep the segmentation walks before treating the rest of a key as one
/// variable tail. A key with more colons than this is pathological, and the
/// extra levels add no structure a reader would act on.
const TEMPLATE_MAX_DEPTH: usize = 8;

/// Infer the key templates of a sampled keyspace: segment each key on `:`,
/// classify each segment position as literal or variable by how many distinct
/// values it takes, and roll the keys up under the resulting patterns.
///
/// This is the structural grounding the SQL seam gets free from
/// `information_schema` and Redis has to derive, so it is deliberately
/// conservative: a position is only collapsed to `*` when enumerating it would
/// produce roughly one branch per key ([`TEMPLATE_MAX_FANOUT`], or a distinct
/// count that is most of the rows). A keyspace of flat, delimiter-less keys
/// therefore rolls into a single `*` bucket rather than one template per key.
///
/// Pure and UI-free like [`analyze_keyspace`], and bounded by `max_templates`
/// (largest first); the caller reports the truncation.
pub fn infer_key_templates(keys: &[KeyMeta], max_templates: usize) -> Vec<KeyTemplate> {
    let segmented: Vec<KeySegments<'_>> = keys
        .iter()
        .map(|meta| KeySegments {
            segments: meta.key.split(':').collect(),
            meta,
        })
        .collect();
    let rows: Vec<&KeySegments<'_>> = segmented.iter().collect();
    let mut out = Vec::new();
    walk_templates(&rows, 0, &mut Vec::new(), &mut out);
    // Biggest first, pattern as a stable tiebreak so two runs of the same sample
    // list the same way.
    out.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.pattern.cmp(&b.pattern))
    });
    out.truncate(max_templates);
    out
}

/// One sampled key pre-split on `:`, so the recursive walk segments each key
/// once rather than per level.
struct KeySegments<'a> {
    segments: Vec<&'a str>,
    meta: &'a KeyMeta,
}

/// Recursive half of [`infer_key_templates`]: classify segment position `depth`
/// across `rows`, then recurse per literal branch (or once, for a variable).
fn walk_templates(
    rows: &[&KeySegments<'_>],
    depth: usize,
    prefix: &mut Vec<String>,
    out: &mut Vec<KeyTemplate>,
) {
    // Keys that end exactly here form a template of their own: `user:1` and
    // `user:1:sessions` are different shapes even though one prefixes the other.
    let (terminal, deeper): (Vec<_>, Vec<_>) = rows
        .iter()
        .copied()
        .partition(|row| row.segments.len() <= depth);
    if !terminal.is_empty() {
        out.push(summarize_template(prefix.join(":"), &terminal));
    }
    if deeper.is_empty() {
        return;
    }
    if depth >= TEMPLATE_MAX_DEPTH {
        prefix.push("*".into());
        out.push(summarize_template(prefix.join(":"), &deeper));
        prefix.pop();
        return;
    }
    let mut groups: BTreeMap<&str, Vec<&KeySegments<'_>>> = BTreeMap::new();
    for row in &deeper {
        groups.entry(row.segments[depth]).or_default().push(row);
    }
    if is_variable_position(groups.len(), deeper.len()) {
        prefix.push("*".into());
        walk_templates(&deeper, depth + 1, prefix, out);
        prefix.pop();
    } else {
        for (segment, group) in groups {
            prefix.push(segment.to_string());
            walk_templates(&group, depth + 1, prefix, out);
            prefix.pop();
        }
    }
}

/// Whether a segment position holding `distinct` values across `total` keys is a
/// variable. Either it fans out past what a reader would scan, or its distinct
/// count is most of the rows — the signature of an id rather than a name.
fn is_variable_position(distinct: usize, total: usize) -> bool {
    distinct > TEMPLATE_MAX_FANOUT || (distinct >= 2 && distinct * 3 >= total * 2)
}

fn summarize_template(pattern: String, rows: &[&KeySegments<'_>]) -> KeyTemplate {
    let mut bytes = 0u64;
    let mut by_type: BTreeMap<&str, u64> = BTreeMap::new();
    let mut ttls: Vec<Duration> = Vec::new();
    for meta in rows.iter().map(|row| row.meta) {
        bytes = bytes.saturating_add(meta.approx_bytes);
        *by_type.entry(meta.kv_type.label()).or_default() += 1;
        if let Some(d) = meta.ttl {
            ttls.push(d);
        }
    }
    let mut types: Vec<(String, u64)> = by_type
        .into_iter()
        .map(|(t, n)| (t.to_string(), n))
        .collect();
    types.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ttls.sort();
    KeyTemplate {
        pattern: if pattern.is_empty() {
            NO_PREFIX_LABEL.to_string()
        } else {
            pattern
        },
        count: rows.len() as u64,
        bytes,
        types,
        with_ttl: ttls.len() as u64,
        median_ttl: ttls.get(ttls.len() / 2).copied(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_templates_collapse_ids_and_keep_literals() {
        let mut keys = Vec::new();
        for id in 0..40 {
            keys.push(meta(
                &format!("user:{id}:sessions"),
                KvType::Set,
                Some(Duration::from_secs(86_400)),
                100,
            ));
            keys.push(meta(
                &format!("cache:v2:product:{id}"),
                KvType::String,
                None,
                200,
            ));
        }
        let templates = infer_key_templates(&keys, 10);
        let patterns: Vec<&str> = templates.iter().map(|t| t.pattern.as_str()).collect();
        assert_eq!(patterns, ["cache:v2:product:*", "user:*:sessions"]);

        let sessions = &templates[1];
        assert_eq!(sessions.count, 40);
        // A high-cardinality position collapses; the literal `sessions` tail does not.
        assert_eq!(sessions.types, [("set".to_string(), 40)]);
        assert_eq!(sessions.with_ttl, 40);
        assert_eq!(sessions.median_ttl, Some(Duration::from_secs(86_400)));
        // `cache:v2:product:*` keeps both literal segments and reports no expiry.
        assert_eq!(templates[0].with_ttl, 0);
        assert_eq!(templates[0].median_ttl, None);
    }

    /// A keyspace with no `:` structure must roll into ONE bucket. Emitting a
    /// template per key would be noise dressed as structure.
    #[test]
    fn key_templates_flatten_a_delimiterless_keyspace() {
        let keys: Vec<KeyMeta> = (0..25)
            .map(|i| meta(&format!("flat{i}"), KvType::String, None, 8))
            .collect();
        let templates = infer_key_templates(&keys, 10);
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].pattern, "*");
        assert_eq!(templates[0].count, 25);
    }

    /// Keys that end at different depths are different shapes, even when one
    /// prefixes the other.
    #[test]
    fn key_templates_split_by_arity() {
        let mut keys: Vec<KeyMeta> = (0..20)
            .map(|i| meta(&format!("job:{i}"), KvType::Hash, None, 10))
            .collect();
        keys.extend((0..20).map(|i| meta(&format!("job:{i}:log"), KvType::List, None, 10)));
        let patterns: Vec<String> = infer_key_templates(&keys, 10)
            .into_iter()
            .map(|t| t.pattern)
            .collect();
        assert_eq!(patterns, ["job:*", "job:*:log"]);
    }

    /// A module type reads as one word and filters as another; conflating the
    /// two sends `TYPE json` to a server that only knows `TYPE ReJSON-RL`.
    #[test]
    fn json_type_reads_as_json_and_filters_as_the_wire_name() {
        assert_eq!(KvType::parse("ReJSON-RL"), Some(KvType::Json));
        assert_eq!(KvType::Json.label(), "json");
        assert_eq!(KvType::Json.wire_type(), "ReJSON-RL");
        // Every built-in type spells itself the same either way.
        for t in [
            KvType::String,
            KvType::Hash,
            KvType::List,
            KvType::Set,
            KvType::ZSet,
            KvType::Stream,
        ] {
            assert_eq!(t.label(), t.wire_type());
        }
        // Another module type still passes through untouched.
        assert_eq!(
            KvType::parse("TSDB-TYPE"),
            Some(KvType::Other("TSDB-TYPE".into()))
        );
    }

    #[test]
    fn json_commands_classify_as_reads_writes_and_deletes() {
        assert_eq!(
            classify_command(&["JSON.GET".into(), "k".into(), "$".into()]),
            OpClass::Read
        );
        assert_eq!(
            classify_command(&["json.type".into(), "k".into()]),
            OpClass::Read
        );
        assert_eq!(
            classify_command(&["JSON.SET".into(), "k".into(), "$".into(), "{}".into()]),
            OpClass::Write
        );
        assert_eq!(
            classify_command(&["JSON.DEL".into(), "k".into(), "$".into()]),
            OpClass::Destructive
        );
        // Listing modules is introspection; loading one is not.
        assert_eq!(
            classify_command(&["MODULE".into(), "LIST".into()]),
            OpClass::Read
        );
        assert_eq!(
            classify_command(&["MODULE".into(), "UNLOAD".into(), "ReJSON".into()]),
            OpClass::Destructive
        );
    }

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
            OpClass::Read
        );
        assert_eq!(
            classify_command(&["SET".into(), "foo".into(), "bar".into()]),
            OpClass::Write
        );
        assert_eq!(
            classify_command(&["DEL".into(), "foo".into()]),
            OpClass::Destructive
        );
        assert_eq!(classify_command(&["FLUSHALL".into()]), OpClass::Destructive);
        // Case-insensitive, and an unknown command defaults to Write (the
        // conservative choice under a read-only connection).
        assert_eq!(
            classify_command(&["get".into(), "foo".into()]),
            OpClass::Read
        );
        assert_eq!(classify_command(&["SOMENEWCOMMAND".into()]), OpClass::Write);
        assert_eq!(classify_command(&[]), OpClass::Read);
        // Server/topology/persistence control is destructive by name.
        for cmd in ["DEBUG", "REPLICAOF", "SLAVEOF", "SHUTDOWN"] {
            assert_eq!(
                classify_command(&[cmd.into()]),
                OpClass::Destructive,
                "{cmd} should be destructive"
            );
        }
        // The dangerous *form* of a subcommand-bearing command is destructive,
        // while its read/introspection form is not.
        assert_eq!(
            classify_command(&["CONFIG".into(), "SET".into(), "dir".into(), "/tmp".into()]),
            OpClass::Destructive
        );
        assert_eq!(
            classify_command(&["config".into(), "get".into(), "dir".into()]),
            OpClass::Write // not destructive; CONFIG GET isn't in READ_COMMANDS
        );
        assert_eq!(
            classify_command(&["CLIENT".into(), "KILL".into(), "id".into(), "5".into()]),
            OpClass::Destructive
        );
        assert_eq!(
            classify_command(&["ACL".into(), "SETUSER".into(), "bob".into()]),
            OpClass::Destructive
        );
        // Script execution is any command with extra steps
        // (`EVAL "redis.call('flushall')" 0`), and MIGRATE ships keys to an
        // arbitrary remote — the name gate must not be a one-line bypass.
        for cmd in ["EVAL", "EVALSHA", "FCALL", "MIGRATE", "eval"] {
            assert_eq!(
                classify_command(&[cmd.into(), "x".into()]),
                OpClass::Destructive,
                "{cmd} should be destructive"
            );
        }
        // Persisting server-side code is the flip side of the gated FLUSH forms.
        assert_eq!(
            classify_command(&["SCRIPT".into(), "LOAD".into(), "return 1".into()]),
            OpClass::Destructive
        );
        assert_eq!(
            classify_command(&["FUNCTION".into(), "LOAD".into(), "…".into()]),
            OpClass::Destructive
        );
        // MEMORY is read only in its introspection forms; PURGE mutates.
        assert_eq!(
            classify_command(&["MEMORY".into(), "USAGE".into(), "k".into()]),
            OpClass::Read
        );
        assert_eq!(
            classify_command(&["MEMORY".into(), "PURGE".into()]),
            OpClass::Write
        );
    }

    #[test]
    fn parse_client_list_reads_lines_and_curated_fields() {
        // Two real-ish CLIENT LIST lines plus a blank one (dropped).
        let reply = "id=7 addr=127.0.0.1:52814 laddr=127.0.0.1:6379 fd=8 name=worker age=42 idle=3 flags=N db=2 sub=0 psub=0 resp=3 cmd=get\n\
                     id=8 addr=127.0.0.1:52820 name= age=0 idle=0 flags=N db=0 resp=2 cmd=client|list\n\
                     \n";
        let clients = parse_client_list(reply);
        assert_eq!(clients.len(), 2);
        assert_eq!(
            clients[0],
            ClientInfo {
                id: 7,
                addr: "127.0.0.1:52814".into(),
                name: "worker".into(),
                db: 2,
                age: 42,
                idle: 3,
                flags: "N".into(),
                cmd: "get".into(),
                resp: "3".into(),
            }
        );
        assert_eq!(clients[1].id, 8);
        assert_eq!(clients[1].name, "");
        assert_eq!(clients[1].cmd, "client|list");
        // A line with no id is dropped.
        assert!(parse_client_line("addr=1.2.3.4:5 flags=N").is_none());
    }

    #[test]
    fn parse_keyspace_channel_decodes_both_families() {
        // keyevent: channel suffix is the event, payload is the key.
        assert_eq!(
            parse_keyspace_channel("__keyevent@0__:expired", "session:abc"),
            Some(KeyspaceEvent {
                db: 0,
                event: "expired".into(),
                key: "session:abc".into(),
            })
        );
        // keyspace: channel suffix is the key, payload is the event.
        assert_eq!(
            parse_keyspace_channel("__keyspace@3__:user:1", "hset"),
            Some(KeyspaceEvent {
                db: 3,
                event: "hset".into(),
                key: "user:1".into(),
            })
        );
        // A key containing a colon survives (only the first `__:` splits).
        assert_eq!(
            parse_keyspace_channel("__keyspace@0__:a:b:c", "del")
                .unwrap()
                .key,
            "a:b:c"
        );
        // An unrelated Pub/Sub channel is not a keyspace notification.
        assert_eq!(parse_keyspace_channel("news.tech", "hello"), None);
        assert_eq!(KeyspaceScope::ByEvent.pattern(), "__keyevent@*__:*");
    }

    fn meta(key: &str, ty: KvType, ttl: Option<Duration>, bytes: u64) -> KeyMeta {
        KeyMeta {
            key: key.to_string(),
            kv_type: ty,
            ttl,
            encoding: String::new(),
            approx_bytes: bytes,
        }
    }

    #[test]
    fn analyze_rolls_up_types_namespaces_and_ttl() {
        let keys = vec![
            meta("user:1", KvType::Hash, None, 100),
            meta("user:2", KvType::Hash, Some(Duration::from_secs(30)), 200),
            meta(
                "session:abc",
                KvType::String,
                Some(Duration::from_secs(7200)),
                50,
            ),
            meta(
                "flat",
                KvType::String,
                Some(Duration::from_secs(1_000_000)),
                10,
            ),
        ];
        let a = analyze_keyspace(&keys, 999, true, 1_700_000_000);

        assert_eq!(a.sampled, 4);
        assert_eq!(a.total_keys, 999);
        assert_eq!(a.total_bytes, 360);
        assert!(a.truncated);

        // Types ordered by memory: hash (300) before string (60).
        assert_eq!(a.types[0].kv_type, "hash");
        assert_eq!(a.types[0].count, 2);
        assert_eq!(a.types[0].bytes, 300);
        assert_eq!(a.types[1].kv_type, "string");
        assert_eq!(a.types[1].bytes, 60);

        // Namespaces: `user` (300) biggest, `session` (50), then flat under
        // the no-prefix bucket (10).
        assert_eq!(a.namespaces[0].prefix, "user");
        assert_eq!(a.namespaces[0].bytes, 300);
        assert!(
            a.namespaces
                .iter()
                .any(|n| n.prefix == NO_PREFIX_LABEL && n.count == 1)
        );

        // TTL buckets: one persistent, one <hour, one <day (7200s = 2h), one
        // >week (1e6s ≈ 11.6d).
        assert_eq!(a.ttl.persistent, 1);
        assert_eq!(a.ttl.under_hour, 1);
        assert_eq!(a.ttl.under_day, 1);
        assert_eq!(a.ttl.over_week, 1);
        assert_eq!(a.ttl.with_ttl(), 3);
    }
}
