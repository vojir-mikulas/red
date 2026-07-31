//! RedisJSON document types: the path algebra, the lazy-tree shapes, and a
//! dependency-free JSON validator.
//!
//! Everything here is pure and engine-free so the awkward half of RedisJSON --
//! quoting a member name that contains a dot, a bracket or a quote -- is decided
//! once, in one unit-tested place, rather than by string concatenation at each
//! call site. A mis-built path edits the wrong node, so [`JsonPath`] is the only
//! way to name one: it is built segment by segment and rendered to wire syntax
//! by [`JsonPath::expr`], never assembled by the caller.
//!
//! The load-bearing invariant is the same one the rest of RED runs on: a
//! RedisJSON document can be hundreds of megabytes, so only a document under
//! [`JSON_WHOLE_DOC_MAX`] is ever fetched whole ([`JsonDoc::Loaded`]). Anything
//! larger is walked one level at a time ([`JsonDoc::Lazy`]), each level bounded
//! by [`JSON_NODE_WINDOW`].

use std::fmt;

use crate::Value;

/// At or above this many bytes, a document is walked lazily rather than fetched
/// whole. Deliberately a *byte* budget rather than the element count the other
/// collections triage on ([`SMALL_COLLECTION_THRESHOLD`](crate::kv)): the probe
/// available for a JSON document (`JSON.DEBUG MEMORY`, or `MEMORY USAGE` as a
/// fallback) reports size, not arity, and size is what actually threatens the
/// process here. 64 KiB covers essentially every real config/session/product
/// document in one round trip while keeping a multi-megabyte one off the wire.
pub const JSON_WHOLE_DOC_MAX: u64 = 64 * 1024;

/// How many children of one JSON node a single window carries. A large array
/// root pages through these exactly like a list.
pub const JSON_NODE_WINDOW: usize = 100;

/// Longest string leaf inlined into a child summary. A longer one shows its
/// length and is read by opening the node, so one chatty field can't drag a
/// megabyte into a level that was meant to be an outline.
pub const JSON_INLINE_STR_MAX: u64 = 256;

/// How deep [`validate_json`] follows nesting before refusing. A document past
/// this is either generated or hostile; either way the recursion is bounded so
/// a pasted `[[[[...` can't overflow the stack.
const MAX_JSON_DEPTH: usize = 128;

/// One step along a [`JsonPath`]: an object member or an array position.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JsonSeg {
    Member(String),
    Index(u64),
}

impl fmt::Display for JsonSeg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JsonSeg::Member(name) => f.write_str(name),
            JsonSeg::Index(i) => write!(f, "[{i}]"),
        }
    }
}

/// A location inside a RedisJSON document, rooted at `$`.
///
/// Built segment by segment and rendered by [`expr`](Self::expr), which always
/// emits bracket-quoted member syntax (`$["orders"][3]["lines"]`). Bracket
/// notation is used even for a plain identifier so there is exactly one code
/// path: a member named `a.b`, `a[0]` or `a"b` is quoted and escaped by the same
/// rule as any other, and no caller ever has to decide whether dot notation is
/// safe here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct JsonPath(Vec<JsonSeg>);

impl JsonPath {
    /// The document root (`$`).
    pub fn root() -> JsonPath {
        JsonPath(Vec::new())
    }

    /// Whether this addresses the whole document.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The steps from the root, outermost first.
    pub fn segments(&self) -> &[JsonSeg] {
        &self.0
    }

    /// This path extended by one object member.
    pub fn member(&self, name: impl Into<String>) -> JsonPath {
        let mut next = self.clone();
        next.0.push(JsonSeg::Member(name.into()));
        next
    }

    /// This path extended by one array position.
    pub fn index(&self, i: u64) -> JsonPath {
        let mut next = self.clone();
        next.0.push(JsonSeg::Index(i));
        next
    }

    /// This path with `seg` appended.
    pub fn child(&self, seg: &JsonSeg) -> JsonPath {
        let mut next = self.clone();
        next.0.push(seg.clone());
        next
    }

    /// The containing node, or `None` at the root.
    pub fn parent(&self) -> Option<JsonPath> {
        let mut next = self.clone();
        next.0.pop()?;
        Some(next)
    }

    /// The wire JSONPath RedisJSON takes: `$`, `$["a"]["b"]`, `$["a"][3]`.
    pub fn expr(&self) -> String {
        let mut out = String::from("$");
        for seg in &self.0 {
            out.push('[');
            match seg {
                JsonSeg::Member(name) => push_json_string(&mut out, name),
                JsonSeg::Index(i) => out.push_str(&i.to_string()),
            }
            out.push(']');
        }
        out
    }

    /// A half-open slice of this (array) node: `$["orders"][0:100]`. The window
    /// a large array root pages through.
    pub fn slice_expr(&self, start: u64, count: u64) -> String {
        format!("{}[{}:{}]", self.expr(), start, start.saturating_add(count))
    }
}

impl fmt::Display for JsonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.expr())
    }
}

/// Append `s` to `out` as a JSON string literal, escaping the two characters
/// that would end or continue the literal plus every C0 control character (a
/// raw one is invalid JSON, and RedisJSON's path parser rejects it).
fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// What a JSON node is, from a `JSON.TYPE` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonKind {
    Object,
    Array,
    String,
    Number,
    Boolean,
    Null,
}

impl JsonKind {
    /// Parse a `JSON.TYPE` reply. RedisJSON distinguishes `integer` from
    /// `number` on the wire; both are one JSON number to a reader, so they
    /// collapse here. An unrecognised reply is `None` (the caller drops the
    /// node rather than inventing a kind for it).
    pub fn parse(raw: &str) -> Option<JsonKind> {
        match raw {
            "object" => Some(JsonKind::Object),
            "array" => Some(JsonKind::Array),
            "string" => Some(JsonKind::String),
            "integer" | "number" => Some(JsonKind::Number),
            "boolean" => Some(JsonKind::Boolean),
            "null" => Some(JsonKind::Null),
            _ => None,
        }
    }

    /// The short label the tree renders next to a node.
    pub fn label(self) -> &'static str {
        match self {
            JsonKind::Object => "object",
            JsonKind::Array => "array",
            JsonKind::String => "string",
            JsonKind::Number => "number",
            JsonKind::Boolean => "boolean",
            JsonKind::Null => "null",
        }
    }

    /// Whether this node has children to expand into.
    pub fn is_container(self) -> bool {
        matches!(self, JsonKind::Object | JsonKind::Array)
    }
}

/// One child in a node's window: enough to render a tree row and decide whether
/// expanding it is worth a round trip, without having read its subtree.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonNode {
    pub seg: JsonSeg,
    pub kind: JsonKind,
    /// Children for a container, characters for a string, `None` for a
    /// number/boolean/null (whose whole value is in `preview`).
    pub len: Option<u64>,
    /// The serialized value, for a scalar small enough to inline. `None` for a
    /// container, or for a string past [`JSON_INLINE_STR_MAX`] -- open the node
    /// to read that one.
    pub preview: Option<String>,
}

/// One node of a document, as read at its path: either a leaf's value or a
/// container's arity plus one window of its children. An enum rather than a
/// struct with optional halves, so "a scalar carrying children" cannot be built.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonNodeView {
    /// A leaf: its serialized JSON value (`"text"`, `42`, `true`, `null`).
    ///
    /// A [`Value`] rather than a `String` because a document's root can itself
    /// be a string, and RedisJSON offers no way to read part of one -- so this
    /// is capped on arrival exactly like a Redis string value, and a
    /// [`Value::Capped`] here means the leaf is longer than what is shown.
    Scalar { kind: JsonKind, value: Value },
    /// A container: how many children it holds, and the window loaded from
    /// `offset`. `children.len() < len` means there is more to page in.
    Container {
        kind: JsonKind,
        len: u64,
        offset: u64,
        children: Vec<JsonNode>,
    },
}

impl JsonNodeView {
    pub fn kind(&self) -> JsonKind {
        match self {
            JsonNodeView::Scalar { kind, .. } | JsonNodeView::Container { kind, .. } => *kind,
        }
    }
}

/// A RedisJSON document's value, from `KvDriver::read_value`.
///
/// Mirrors [`KvCollection`](crate::kv::KvCollection)'s whole-vs-summary split
/// rather than inventing a third pattern: `Loaded` under
/// [`JSON_WHOLE_DOC_MAX`] is one `JSON.GET $`; `Lazy` carries only the root's
/// shape and one window of its children, and every subtree below is fetched on
/// expand by path.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonDoc {
    Loaded {
        /// The document, serialized.
        text: String,
        /// Its size from the probe, so the inspector can state it.
        bytes: u64,
    },
    Lazy {
        bytes: u64,
        root: JsonNodeView,
    },
}

impl JsonDoc {
    /// The document's probed size in bytes.
    pub fn bytes(&self) -> u64 {
        match self {
            JsonDoc::Loaded { bytes, .. } | JsonDoc::Lazy { bytes, .. } => *bytes,
        }
    }
}

/// Whether a document is read whole or walked. The decision is a pure function
/// of the size probe so it is testable without a server, and so the "probe
/// unavailable" case is decided here rather than at each call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonFetch {
    /// One `JSON.GET <key> $`.
    Whole,
    /// Walk the root level and expand on demand.
    Lazy,
}

/// Pick the read strategy from a size probe. `None` means neither
/// `JSON.DEBUG MEMORY` nor `MEMORY USAGE` was available (some managed providers
/// restrict `DEBUG`), and the answer there is [`JsonFetch::Lazy`], never "fetch
/// it whole and hope": an unknown size is exactly the case a whole fetch must
/// not be guessed at.
pub fn json_fetch_mode(bytes: Option<u64>) -> JsonFetch {
    match bytes {
        Some(n) if n < JSON_WHOLE_DOC_MAX => JsonFetch::Whole,
        _ => JsonFetch::Lazy,
    }
}

/// A JSON document that failed to parse, with where it failed.
///
/// The offset is what makes this worth having: a malformed edit should fail in
/// RED pointing at the character, not at the server with a bare
/// `ERR expected value`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} at byte offset {offset}")]
pub struct JsonSyntaxError {
    pub offset: usize,
    pub message: String,
}

/// Check that `text` is exactly one well-formed JSON value.
///
/// # Errors
///
/// Returns the first syntax fault with its byte offset. Strict about the things
/// a hand-typed edit gets wrong and a server would reject anyway: trailing
/// commas, single quotes, unquoted member names, leading zeroes, raw control
/// characters in a string, and trailing input after the value.
pub fn validate_json(text: &str) -> Result<(), JsonSyntaxError> {
    let b = text.as_bytes();
    let start = skip_ws(b, 0);
    if start == b.len() {
        return Err(err(start, "expected a JSON value, found nothing"));
    }
    let end = skip_ws(b, scan_value(b, start, 0)?);
    if end != b.len() {
        return Err(err(end, "unexpected trailing input after the value"));
    }
    Ok(())
}

/// Unwrap the single-element array RedisJSON answers a `$`-rooted path with.
///
/// `JSON.GET key $["a"]` replies `[<value>]` because a JSONPath can match many
/// nodes; every path RED builds matches at most one, so this peels the wrapper
/// back off. `None` when the reply isn't a one-element array -- which for a
/// RED-built path means the path matched nothing (`[]`), i.e. the node is gone.
pub fn json_unwrap_singleton(text: &str) -> Option<&str> {
    let b = text.as_bytes();
    let open = skip_ws(b, 0);
    if b.get(open) != Some(&b'[') {
        return None;
    }
    let start = skip_ws(b, open + 1);
    let end = scan_value(b, start, 0).ok()?;
    let close = skip_ws(b, end);
    if b.get(close) != Some(&b']') || skip_ws(b, close + 1) != b.len() {
        return None;
    }
    text.get(start..end)
}

fn err(offset: usize, message: &str) -> JsonSyntaxError {
    JsonSyntaxError {
        offset,
        message: message.to_string(),
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while matches!(b.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        i += 1;
    }
    i
}

/// Scan one JSON value starting at `i` (its first non-whitespace byte),
/// returning the index just past it.
fn scan_value(b: &[u8], i: usize, depth: usize) -> Result<usize, JsonSyntaxError> {
    if depth > MAX_JSON_DEPTH {
        return Err(err(i, "nested too deeply"));
    }
    match b.get(i) {
        None => Err(err(i, "expected a value, found end of input")),
        Some(b'{') => scan_object(b, i, depth),
        Some(b'[') => scan_array(b, i, depth),
        Some(b'"') => scan_string(b, i),
        Some(b't') => scan_literal(b, i, b"true"),
        Some(b'f') => scan_literal(b, i, b"false"),
        Some(b'n') => scan_literal(b, i, b"null"),
        Some(c) if *c == b'-' || c.is_ascii_digit() => scan_number(b, i),
        Some(_) => Err(err(i, "expected a value")),
    }
}

fn scan_object(b: &[u8], start: usize, depth: usize) -> Result<usize, JsonSyntaxError> {
    let mut i = skip_ws(b, start + 1);
    if b.get(i) == Some(&b'}') {
        return Ok(i + 1);
    }
    loop {
        if b.get(i) != Some(&b'"') {
            return Err(err(i, "expected a quoted member name"));
        }
        i = skip_ws(b, scan_string(b, i)?);
        if b.get(i) != Some(&b':') {
            return Err(err(i, "expected `:` after a member name"));
        }
        i = skip_ws(b, i + 1);
        i = skip_ws(b, scan_value(b, i, depth + 1)?);
        match b.get(i) {
            Some(b',') => i = skip_ws(b, i + 1),
            Some(b'}') => return Ok(i + 1),
            _ => return Err(err(i, "expected `,` or `}`")),
        }
    }
}

fn scan_array(b: &[u8], start: usize, depth: usize) -> Result<usize, JsonSyntaxError> {
    let mut i = skip_ws(b, start + 1);
    if b.get(i) == Some(&b']') {
        return Ok(i + 1);
    }
    loop {
        i = skip_ws(b, scan_value(b, i, depth + 1)?);
        match b.get(i) {
            Some(b',') => i = skip_ws(b, i + 1),
            Some(b']') => return Ok(i + 1),
            _ => return Err(err(i, "expected `,` or `]`")),
        }
    }
}

fn scan_string(b: &[u8], start: usize) -> Result<usize, JsonSyntaxError> {
    let mut i = start + 1;
    while let Some(c) = b.get(i) {
        match c {
            b'"' => return Ok(i + 1),
            b'\\' => match b.get(i + 1) {
                Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => i += 2,
                Some(b'u') => {
                    let hex = b.get(i + 2..i + 6);
                    if !hex.is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) {
                        return Err(err(i, "`\\u` needs four hex digits"));
                    }
                    i += 6;
                }
                Some(_) => return Err(err(i + 1, "unknown string escape")),
                None => break,
            },
            c if *c < 0x20 => return Err(err(i, "a raw control character must be escaped")),
            _ => i += 1,
        }
    }
    Err(err(start, "unterminated string"))
}

fn scan_number(b: &[u8], start: usize) -> Result<usize, JsonSyntaxError> {
    let mut i = start;
    if b.get(i) == Some(&b'-') {
        i += 1;
    }
    let int_start = i;
    while matches!(b.get(i), Some(c) if c.is_ascii_digit()) {
        i += 1;
    }
    if i == int_start {
        return Err(err(i, "expected a digit"));
    }
    if b.get(int_start) == Some(&b'0') && i - int_start > 1 {
        return Err(err(int_start, "a number must not have a leading zero"));
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        let frac_start = i;
        while matches!(b.get(i), Some(c) if c.is_ascii_digit()) {
            i += 1;
        }
        if i == frac_start {
            return Err(err(i, "expected a digit after `.`"));
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp_start = i;
        while matches!(b.get(i), Some(c) if c.is_ascii_digit()) {
            i += 1;
        }
        if i == exp_start {
            return Err(err(i, "expected a digit in the exponent"));
        }
    }
    Ok(i)
}

fn scan_literal(b: &[u8], i: usize, lit: &[u8]) -> Result<usize, JsonSyntaxError> {
    match b.get(i..i + lit.len()) {
        Some(got) if got == lit => Ok(i + lit.len()),
        _ => Err(err(i, "expected `true`, `false` or `null`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_quotes_every_member_and_leaves_indices_bare() {
        assert_eq!(JsonPath::root().expr(), "$");
        assert_eq!(
            JsonPath::root().member("a").member("b").expr(),
            r#"$["a"]["b"]"#
        );
        assert_eq!(JsonPath::root().index(3).expr(), "$[3]");
        assert_eq!(
            JsonPath::root()
                .member("orders")
                .index(3)
                .member("lines")
                .expr(),
            r#"$["orders"][3]["lines"]"#
        );
    }

    /// The failure this type exists to prevent: a member name carrying path
    /// syntax must never be read as path syntax.
    #[test]
    fn path_escapes_names_that_look_like_syntax() {
        assert_eq!(
            JsonPath::root().member("key with spaces").expr(),
            r#"$["key with spaces"]"#
        );
        assert_eq!(JsonPath::root().member("a.b").expr(), r#"$["a.b"]"#);
        assert_eq!(JsonPath::root().member("a[0]").expr(), r#"$["a[0]"]"#);
        assert_eq!(JsonPath::root().member(r#"a"b"#).expr(), r#"$["a\"b"]"#);
        assert_eq!(JsonPath::root().member(r"a\b").expr(), r#"$["a\\b"]"#);
        assert_eq!(JsonPath::root().member("a\nb").expr(), r#"$["a\nb"]"#);
        // A C0 control character escapes rather than riding raw into the path.
        assert_eq!(
            JsonPath::root().member("a\u{1}b").expr(),
            "$[\"a\\u0001b\"]"
        );
        // Non-ASCII needs no escaping: the wire is UTF-8.
        assert_eq!(JsonPath::root().member("ключ").expr(), r#"$["ключ"]"#);
    }

    #[test]
    fn path_slices_a_window_and_walks_back_up() {
        let p = JsonPath::root().member("orders");
        assert_eq!(p.slice_expr(0, 100), r#"$["orders"][0:100]"#);
        assert_eq!(p.slice_expr(100, 100), r#"$["orders"][100:200]"#);
        assert_eq!(p.parent(), Some(JsonPath::root()));
        assert_eq!(JsonPath::root().parent(), None);
        assert!(JsonPath::root().is_root());
    }

    #[test]
    fn fetch_mode_treats_an_unknown_size_as_lazy() {
        assert_eq!(json_fetch_mode(Some(0)), JsonFetch::Whole);
        assert_eq!(
            json_fetch_mode(Some(JSON_WHOLE_DOC_MAX - 1)),
            JsonFetch::Whole
        );
        assert_eq!(json_fetch_mode(Some(JSON_WHOLE_DOC_MAX)), JsonFetch::Lazy);
        // No probe available: walk it. Never guess a whole fetch.
        assert_eq!(json_fetch_mode(None), JsonFetch::Lazy);
    }

    #[test]
    fn validate_accepts_well_formed_documents() {
        for ok in [
            "{}",
            "[]",
            "null",
            "true",
            "  42 ",
            "-0.5e+10",
            r#"{"a":[1,2,{"b":null}],"c":"x"}"#,
            r#""aéb\n""#,
            "[[[[[1]]]]]",
        ] {
            assert!(validate_json(ok).is_ok(), "{ok} should parse");
        }
    }

    #[test]
    fn validate_reports_the_offset_of_the_first_fault() {
        let e = validate_json(r#"{"a":1,}"#).unwrap_err();
        assert_eq!(e.offset, 7);
        // Trailing input after a complete value.
        assert_eq!(validate_json("{} {}").unwrap_err().offset, 3);
        // A single-quoted string is not JSON.
        assert_eq!(validate_json("{'a':1}").unwrap_err().offset, 1);
        // Unquoted member name.
        assert_eq!(validate_json("{a:1}").unwrap_err().offset, 1);
        for bad in [
            "",
            "   ",
            "{",
            "[1,",
            "01",
            "-",
            "1.",
            "1e",
            r#""unterminated"#,
            r#""\q""#,
            r#""\u12""#,
            "\"raw\ttab\"",
            "tru",
        ] {
            assert!(validate_json(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn validate_refuses_pathological_nesting_rather_than_overflowing() {
        let deep = format!("{}1{}", "[".repeat(500), "]".repeat(500));
        assert!(validate_json(&deep).is_err());
    }

    #[test]
    fn unwrap_singleton_peels_the_jsonpath_wrapper() {
        assert_eq!(json_unwrap_singleton(r#"[{"a":1}]"#), Some(r#"{"a":1}"#));
        assert_eq!(json_unwrap_singleton("[42]"), Some("42"));
        assert_eq!(json_unwrap_singleton(r#"[ "x" ]"#), Some(r#""x""#));
        // A nested array survives intact: only the outer wrapper is peeled.
        assert_eq!(json_unwrap_singleton("[[1,2]]"), Some("[1,2]"));
        // No match, more than one match, or not an array at all.
        assert_eq!(json_unwrap_singleton("[]"), None);
        assert_eq!(json_unwrap_singleton("[1,2]"), None);
        assert_eq!(json_unwrap_singleton("42"), None);
        assert_eq!(json_unwrap_singleton(""), None);
    }

    #[test]
    fn kind_collapses_integer_into_number() {
        assert_eq!(JsonKind::parse("integer"), Some(JsonKind::Number));
        assert_eq!(JsonKind::parse("number"), Some(JsonKind::Number));
        assert_eq!(JsonKind::parse("object"), Some(JsonKind::Object));
        assert_eq!(JsonKind::parse("ReJSON"), None);
        assert!(JsonKind::Array.is_container());
        assert!(!JsonKind::String.is_container());
    }
}
