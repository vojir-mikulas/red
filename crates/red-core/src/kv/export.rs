//! Turning Redis keys into a file: the three formats, the quoting each needs,
//! and the binary-safety rule that decides when a key cannot go in one.
//!
//! Everything here is pure and chunk-shaped rather than value-shaped. The export
//! walk pages a million-element set through [`command_line`] a chunk at a time
//! and never assembles it, so nothing in this module ever sees a whole
//! collection. That is also what makes the round trip testable without a server:
//! generate commands, feed them back through
//! [`tokenize_command`](super::tokenize_command), and compare the argv.
//!
//! ## The binary-safety rule
//!
//! Redis values are binary-safe byte strings; the Commands and JSON formats are
//! text. [`quote_arg`] escapes what it can (`\xHH`, matching `redis-cli
//! --no-raw`, which [`tokenize_command`](super::tokenize_command) decodes on the
//! way back), but a value that is not valid UTF-8 has already been decoded
//! lossily by the time the export sees it and cannot be recovered. Such a key is
//! **skipped and reported**, never written mangled: a silent lossy export of
//! binary data is worse than a refusal, and the DUMP format is the answer for it.

use std::fmt::Write as _;

/// How many elements of one collection ride in a single command. Keeps a line
/// readable and an argv within any server's limit, while not paying a command
/// per element for a large set.
pub const KV_EXPORT_CHUNK: usize = 200;

/// The file a Redis key export writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvExportFormat {
    /// `redis-cli`-style commands, the exact inverse of the import that already
    /// ships. Readable, cross-version, and re-importable through `KvImport`.
    Commands,
    /// One JSON object per key. Does not round-trip (binary values are marked,
    /// not carried); for feeding another tool, diffing a keyspace, or attaching
    /// to a ticket.
    Json,
    /// A framed `DUMP` payload per key. Byte-exact and the only format that
    /// carries a binary value, but **not cross-version**: a payload embeds an RDB
    /// version and `RESTORE` refuses a newer one.
    Dump,
}

impl KvExportFormat {
    /// The extension the save dialog defaults to.
    pub fn extension(self) -> &'static str {
        match self {
            KvExportFormat::Commands => "redis",
            KvExportFormat::Json => "json",
            KvExportFormat::Dump => "rdbdump",
        }
    }

    /// Whether this format carries a value's exact bytes. Only the DUMP format
    /// does, which is why it is what a skipped binary key points at.
    pub fn is_binary_exact(self) -> bool {
        matches!(self, KvExportFormat::Dump)
    }
}

/// Which keys an export covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvExportScope {
    /// Exactly these keys (the grid selection, or one key from its context menu).
    Selection(Vec<String>),
    /// Every key the browse filter currently matches, re-walked server-side.
    Matching {
        pattern: Option<String>,
        /// A `TYPE` filter as its wire name, ready for `SCAN ... TYPE`.
        type_filter: Option<String>,
    },
    /// The whole logical database.
    Database,
}

impl KvExportScope {
    /// A one-line description for the export's header comment and its toast.
    pub fn describe(&self) -> String {
        match self {
            KvExportScope::Selection(keys) => format!("{} selected key(s)", keys.len()),
            KvExportScope::Matching {
                pattern,
                type_filter,
            } => {
                let p = pattern.as_deref().unwrap_or("*");
                match type_filter {
                    Some(t) => format!("keys matching `{p}` of type {t}"),
                    None => format!("keys matching `{p}`"),
                }
            }
            KvExportScope::Database => "the whole database".to_string(),
        }
    }
}

/// The options an export offers beyond scope and format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvExportOptions {
    /// Write each key's expiry as an absolute `PEXPIREAT`. On by default.
    pub ttls: bool,
    /// Precede each key with `DEL`. **Off** by default and deliberately: an
    /// export that begins with a thousand `DEL`s is a foot-gun aimed at whichever
    /// server it is imported into.
    pub del_first: bool,
}

impl Default for KvExportOptions {
    fn default() -> Self {
        KvExportOptions {
            ttls: true,
            del_first: false,
        }
    }
}

/// Whether `s` came back from a lossy byte decode, i.e. the value is not valid
/// UTF-8 and the text formats cannot carry it.
///
/// A heuristic, and knowingly so: the paging collection readers decode to text
/// before the export sees them, so a genuine U+FFFD stored by a user reads the
/// same as one the decoder produced. It errs toward skipping, which is the safe
/// direction -- the key is reported, and the DUMP format carries it exactly.
pub fn is_lossy_text(s: &str) -> bool {
    s.contains('\u{FFFD}')
}

/// Characters that need no quoting at all, so an ordinary key or value reads as
/// itself in the file.
fn is_bare_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '@' | '+' | '#')
}

/// Quote one argument the way `redis-cli --no-raw` writes it: bare when it is
/// plainly safe, double-quoted with `\xHH`/`\n`/`\t` escapes otherwise.
///
/// The escapes matter more than the readability:
/// [`tokenize_command`](super::tokenize_command) decodes exactly this set, so
/// what this writes is what an import reconstructs.
pub fn quote_arg(s: &str) -> String {
    if !s.is_empty() && s.chars().all(is_bare_safe) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Every other control character rides as one `\xHH` per UTF-8 byte,
            // which is the form the tokenizer rejoins.
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).as_bytes() {
                    let _ = write!(out, "\\x{b:02x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One command line: the verb and its arguments, each quoted.
pub fn command_line<'a>(argv: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for (i, arg) in argv.into_iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&quote_arg(arg));
    }
    out
}

/// The absolute-expiry line for a key, in Unix milliseconds.
///
/// Absolute, not relative: a `PEXPIRE` written now and imported an hour later
/// would silently extend every TTL by an hour, which is the kind of drift nobody
/// notices until a cache never expires.
pub fn expire_line(key: &str, unix_ms: i64) -> String {
    command_line(["PEXPIREAT", key, &unix_ms.to_string()])
}

/// The header comment a Commands export opens with: what it came from, when, and
/// whether importing it will delete anything.
pub fn commands_header(
    source: &str,
    scope: &KvExportScope,
    opts: &KvExportOptions,
    at: &str,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# RED Redis key export");
    let _ = writeln!(out, "# source: {source}");
    let _ = writeln!(out, "# scope:  {}", scope.describe());
    let _ = writeln!(out, "# taken:  {at}");
    let _ = writeln!(
        out,
        "# expiry: {}",
        if opts.ttls {
            "included as absolute PEXPIREAT"
        } else {
            "not included; imported keys will not expire"
        }
    );
    let _ = writeln!(
        out,
        "# {}",
        if opts.del_first {
            "WARNING: each key is DELeted before it is written. Importing this \
             file destroys those keys on the target."
        } else {
            "no DEL: importing merges into whatever the target already holds"
        }
    );
    out
}

// --- the JSON format ---

/// A JSON string literal, escaped to RFC 8259.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A value the JSON format cannot carry as text, rendered as a tagged object so
/// a reader can tell "binary" from "the string `<12 bytes>`".
pub fn json_binary(bytes: &[u8]) -> String {
    format!("{{\"b64\":{}}}", json_string(&b64_encode(bytes)))
}

/// Standard base64, hand-rolled: twenty lines against a dependency, the same
/// call the binary decoders already make.
pub fn b64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

// --- the DUMP format ---

/// The magic line every DUMP-format file opens with, so an import can refuse a
/// file that is not one rather than reading noise as key lengths.
pub const KV_DUMP_MAGIC: &[u8] = b"RED-KVDUMP1\n";

/// Frame one key's `DUMP` payload: `key`, its expiry, and the bytes, each
/// length-prefixed so the reader never has to guess a boundary.
///
/// `ttl_ms` is `0` for a key with no expiry, which is unambiguous because a
/// Redis expiry is always in the future when it is written.
pub fn dump_frame(key: &str, ttl_ms: u64, payload: &[u8]) -> Vec<u8> {
    let key = key.as_bytes();
    let mut out = Vec::with_capacity(key.len() + payload.len() + 16);
    out.extend_from_slice(&(key.len() as u32).to_le_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(&ttl_ms.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// One frame read back from a DUMP-format file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvDumpEntry {
    pub key: String,
    /// Milliseconds of remaining expiry, `0` for none.
    pub ttl_ms: u64,
    pub payload: Vec<u8>,
}

/// Read one frame starting at `at`, returning it and the offset just past it.
///
/// `None` at a clean end of file *or* on a torn tail, deliberately: a truncated
/// export should restore what it does hold rather than refuse entirely, and the
/// caller reports the count either way.
pub fn read_dump_frame(bytes: &[u8], at: usize) -> Option<(KvDumpEntry, usize)> {
    let u32_at = |i: usize| -> Option<usize> {
        let raw: [u8; 4] = bytes.get(i..i + 4)?.try_into().ok()?;
        Some(u32::from_le_bytes(raw) as usize)
    };
    let key_len = u32_at(at)?;
    let key_end = at.checked_add(4)?.checked_add(key_len)?;
    let key = String::from_utf8_lossy(bytes.get(at + 4..key_end)?).into_owned();
    let ttl_raw: [u8; 8] = bytes.get(key_end..key_end + 8)?.try_into().ok()?;
    let payload_at = key_end + 8;
    let payload_len = u32_at(payload_at)?;
    let payload_start = payload_at + 4;
    let payload_end = payload_start.checked_add(payload_len)?;
    let payload = bytes.get(payload_start..payload_end)?.to_vec();
    Some((
        KvDumpEntry {
            key,
            ttl_ms: u64::from_le_bytes(ttl_raw),
            payload,
        },
        payload_end,
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tokenize_command;
    use super::*;

    #[test]
    fn plain_arguments_ride_bare_and_awkward_ones_are_quoted() {
        assert_eq!(quote_arg("user:1"), "user:1");
        assert_eq!(quote_arg("a-b_c.d/e@f+g#h"), "a-b_c.d/e@f+g#h");
        assert_eq!(quote_arg("has space"), "\"has space\"");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("say \"hi\""), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r"back\slash"), r#""back\\slash""#);
        assert_eq!(quote_arg("a\nb\tc"), r#""a\nb\tc""#);
        assert_eq!(quote_arg("a\u{0}b"), r#""a\x00b""#);
        // A non-ASCII value quotes but rides as its own UTF-8, not as escapes.
        assert_eq!(quote_arg("héllo"), "\"héllo\"");
    }

    /// The test that makes the feature trustworthy: what the export writes,
    /// tokenized back, is exactly the argv that would recreate the key.
    #[test]
    fn generated_commands_round_trip_through_the_tokenizer() {
        let awkward = "a \"quoted\" value\nwith\ttabs and \\ and é and \u{1}";
        let cases: Vec<Vec<&str>> = vec![
            vec!["SET", "user:1", "plain"],
            vec!["SET", "weird key", awkward],
            vec!["HSET", "h", "field one", "v1", "field\ttwo", "v2"],
            vec!["RPUSH", "l", "a", "", "c"],
            vec!["SADD", "s", "m1", "m 2"],
            vec!["ZADD", "z", "1.5", "member one"],
            vec!["XADD", "st", "1-1", "f", "v"],
            vec!["JSON.SET", "j", "$", r#"{"a":1}"#],
            vec!["PEXPIREAT", "user:1", "1700000000000"],
        ];
        for argv in cases {
            let line = command_line(argv.iter().copied());
            assert_eq!(
                tokenize_command(&line),
                argv,
                "round trip failed for {line}"
            );
        }
    }

    #[test]
    fn expiry_is_absolute_so_an_import_cannot_extend_it() {
        assert_eq!(
            expire_line("user:1", 1_700_000_000_000),
            "PEXPIREAT user:1 1700000000000"
        );
        assert_eq!(
            tokenize_command(&expire_line("a b", 5)),
            vec!["PEXPIREAT", "a b", "5"]
        );
    }

    #[test]
    fn lossy_text_is_the_signal_a_key_cannot_be_written() {
        assert!(!is_lossy_text("ordinary"));
        assert!(!is_lossy_text("héllo"));
        assert!(is_lossy_text("bad\u{FFFD}bytes"));
    }

    #[test]
    fn the_header_says_whether_importing_it_deletes_anything() {
        let scope = KvExportScope::Matching {
            pattern: Some("user:*".into()),
            type_filter: None,
        };
        let safe = commands_header(
            "redis://localhost:6379/0",
            &scope,
            &KvExportOptions::default(),
            "now",
        );
        assert!(safe.contains("keys matching `user:*`"));
        assert!(safe.contains("no DEL"));
        assert!(safe.contains("absolute PEXPIREAT"));

        let destructive = commands_header(
            "s",
            &KvExportScope::Database,
            &KvExportOptions {
                ttls: false,
                del_first: true,
            },
            "now",
        );
        assert!(destructive.contains("WARNING"));
        assert!(destructive.contains("the whole database"));
        assert!(destructive.contains("will not expire"));
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(b64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64_encode(&[0xFF, 0xFE, 0xFD]), "//79");
    }

    #[test]
    fn json_strings_escape_what_json_requires() {
        assert_eq!(json_string("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(json_string("a\u{1}b"), "\"a\\u0001b\"");
        assert_eq!(json_string("a\u{1}b"), "\"a\\u0001b\"");
        assert_eq!(json_binary(&[0xFF]), r#"{"b64":"/w=="}"#);
    }

    #[test]
    fn dump_frames_round_trip_including_binary_and_a_torn_tail() {
        let mut file = KV_DUMP_MAGIC.to_vec();
        file.extend(dump_frame("plain", 0, &[1, 2, 3]));
        file.extend(dump_frame("with:ttl", 1234, &[0xFF, 0x00, 0xFE]));

        let (first, at) = read_dump_frame(&file, KV_DUMP_MAGIC.len()).unwrap();
        assert_eq!(first.key, "plain");
        assert_eq!(first.ttl_ms, 0);
        assert_eq!(first.payload, vec![1, 2, 3]);
        let (second, at) = read_dump_frame(&file, at).unwrap();
        assert_eq!(second.key, "with:ttl");
        assert_eq!(second.ttl_ms, 1234);
        assert_eq!(second.payload, vec![0xFF, 0x00, 0xFE]);
        // Clean end of file.
        assert_eq!(read_dump_frame(&file, at), None);
        // A truncated tail stops rather than panicking, so a torn export still
        // restores the frames it does hold.
        file.truncate(file.len() - 2);
        let (_, at) = read_dump_frame(&file, KV_DUMP_MAGIC.len()).unwrap();
        assert_eq!(read_dump_frame(&file, at), None);
    }

    #[test]
    fn scopes_describe_themselves_for_the_header_and_the_toast() {
        assert_eq!(
            KvExportScope::Selection(vec!["a".into(), "b".into()]).describe(),
            "2 selected key(s)"
        );
        assert_eq!(
            KvExportScope::Matching {
                pattern: None,
                type_filter: Some("hash".into())
            }
            .describe(),
            "keys matching `*` of type hash"
        );
        assert_eq!(KvExportScope::Database.describe(), "the whole database");
    }
}
