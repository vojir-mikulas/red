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
    /// A type this build has no preview for yet (streams; see
    /// docs/plans/redis.md — not in this pass's scope). Distinct from `Ok(None)`
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
