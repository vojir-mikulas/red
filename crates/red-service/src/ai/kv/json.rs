//! The Redis seam's RedisJSON tools: reading one path, mapping a document's
//! shape, and the gated write.
//!
//! The lazy document walk is only usable by a model if it has the model's
//! equivalent of expanding a node, which is what [`kv_json_get`] is. The other
//! half is [`kv_json_shape`]: paths and types with no values, the JSON analogue
//! of `kv_key_schema`, so "what is in these documents" can be answered without
//! reading any of them.

use std::sync::Arc;

use red_core::AiLimits;
use red_core::kv::{JSON_NODE_WINDOW, JsonKind, JsonNodeView, JsonPath, KvType, validate_json};
use red_driver::KvDriver;
use serde_json::Value as Json;

use super::super::util::cap_result_bytes;
use super::format::fmt_json_node;

/// How many nodes [`kv_json_shape`] visits before it stops and says so. A map
/// of the shape is meant to fit in a reply; past this it has stopped being a
/// summary of the document and started being the document.
const SHAPE_NODE_CAP: usize = 400;

/// How deep [`kv_json_shape`] descends. Deeper than this and the paths are
/// longer than the values they describe.
const SHAPE_DEPTH_CAP: usize = 6;

/// How many children of one container the shape walk descends into. A
/// thousand-element array has one shape, not a thousand.
const SHAPE_FANOUT_CAP: usize = 12;

/// How many elements of an array the shape walk even lists. Its elements are
/// almost always one shape repeated, so a couple is the sample and the rest are
/// reported as a count.
const ARRAY_SAMPLE: usize = 2;

/// Parse the `path` argument shared by the JSON tools.
///
/// Accepts the dotted/bracketed form a model naturally writes (`orders[3].id`,
/// `$.a.b`, `a["x y"]`) and returns a built [`JsonPath`], so the wire syntax is
/// still produced by the one escaping routine rather than by whatever the model
/// typed. An empty or `$` path is the document root.
fn parse_path(raw: &str) -> Result<JsonPath, String> {
    let mut path = JsonPath::root();
    let mut chars = raw.trim().chars().peekable();
    if chars.peek() == Some(&'$') {
        chars.next();
    }
    let mut pending = String::new();
    // Flush whatever bare identifier has accumulated as one member segment.
    macro_rules! flush {
        () => {
            if !pending.is_empty() {
                path = path.member(std::mem::take(&mut pending));
            }
        };
    }
    while let Some(c) = chars.next() {
        match c {
            '.' => flush!(),
            '[' => {
                flush!();
                let quote = matches!(chars.peek(), Some('"') | Some('\'')).then(|| {
                    chars.next().unwrap_or('"') // peek above proved a next char
                });
                let mut inner = String::new();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match quote {
                        Some(q) if c == q => {
                            // Consume the `]` that must follow a quoted name.
                            closed = chars.next() == Some(']');
                            break;
                        }
                        None if c == ']' => {
                            closed = true;
                            break;
                        }
                        _ => inner.push(c),
                    }
                }
                if !closed {
                    return Err(format!("unbalanced `[` in path `{raw}`"));
                }
                path = match (quote, inner.parse::<u64>()) {
                    (None, Ok(i)) => path.index(i),
                    _ => path.member(inner),
                };
            }
            c => pending.push(c),
        }
    }
    flush!();
    Ok(path)
}

/// Check that `key` is a RedisJSON document before spending `JSON.*` commands
/// on it.
///
/// Without this, pointing a JSON tool at a plain string key surfaces the raw
/// `WRONGTYPE` the server answers with, which tells the model nothing it can act
/// on. One `TYPE` round trip buys a sentence that names the actual type and the
/// tool to use instead.
async fn require_json_key(driver: &Arc<dyn KvDriver>, key: &str) -> Result<(), String> {
    match driver.probe_key(key).await {
        Ok(None) => Err(format!("key `{key}` does not exist")),
        Ok(Some(meta)) if meta.kv_type != KvType::Json => Err(format!(
            "key `{key}` is a {}, not a JSON document; read it with kv_get_value",
            meta.kv_type.label()
        )),
        Ok(Some(_)) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// `kv_json_get`: the value at one JSONPath inside a RedisJSON document. The
/// model's equivalent of expanding a node in the tree.
pub(super) async fn kv_json_get(
    driver: &Arc<dyn KvDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    let key = input.get("key").and_then(Json::as_str).unwrap_or("");
    if key.is_empty() {
        return ("error: `key` is required".into(), false);
    }
    let raw = input.get("path").and_then(Json::as_str).unwrap_or("$");
    let path = match parse_path(raw) {
        Ok(p) => p,
        Err(why) => return (format!("error: {why}"), false),
    };
    if let Err(why) = require_json_key(driver, key).await {
        return (format!("error: {why}"), false);
    }
    match driver.json_get(key, &path).await {
        Ok(None) => (
            format!(
                "nothing at `{}` in `{key}` (the key or the path does not exist)",
                path.expr()
            ),
            true,
        ),
        Ok(Some(v)) => (
            cap_result_bytes(
                format!(
                    "{key} {} =\n{}",
                    path.expr(),
                    super::super::sql::format::render_cell(&v)
                ),
                limits.max_result_bytes,
            ),
            true,
        ),
        Err(e) => (format!("error: {e}"), false),
    }
}

/// `kv_json_shape`: the document's structure as paths and types, no values.
pub(super) async fn kv_json_shape(
    driver: &Arc<dyn KvDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    let key = input.get("key").and_then(Json::as_str).unwrap_or("");
    if key.is_empty() {
        return ("error: `key` is required".into(), false);
    }
    if let Err(why) = require_json_key(driver, key).await {
        return (format!("error: {why}"), false);
    }
    let mut lines = Vec::new();
    let mut budget = SHAPE_NODE_CAP;
    if let Err(e) = walk_shape(driver, key, &JsonPath::root(), 0, &mut budget, &mut lines).await {
        return (format!("error: {e}"), false);
    }
    if lines.is_empty() {
        // A valid document whose root is a scalar or an empty container: say so
        // rather than returning a header with nothing under it.
        return (
            format!(
                "`{key}` is a JSON document with no nested structure; read it with kv_json_get"
            ),
            true,
        );
    }
    let truncated = budget == 0;
    let mut out = format!("Structure of `{key}` (paths and types, no values):\n");
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    if truncated {
        out.push_str("(stopped at the node budget; the document has more structure)\n");
    }
    (cap_result_bytes(out, limits.max_result_bytes), true)
}

/// Depth-first shape walk. Boxed because it recurses through an `async fn`,
/// which would otherwise need an infinitely-sized future.
fn walk_shape<'a>(
    driver: &'a Arc<dyn KvDriver>,
    key: &'a str,
    path: &'a JsonPath,
    depth: usize,
    budget: &'a mut usize,
    out: &'a mut Vec<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = red_core::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        if *budget == 0 || depth > SHAPE_DEPTH_CAP {
            return Ok(());
        }
        let Some(view) = driver
            .read_json_node(key, path, 0, JSON_NODE_WINDOW)
            .await?
        else {
            return Ok(());
        };
        let JsonNodeView::Container {
            kind,
            len,
            children,
            ..
        } = view
        else {
            return Ok(());
        };
        // An array's elements are almost always one shape repeated, so listing
        // all sixty of them restates the same structure sixty times. Sample the
        // first few and say how many were elided; an object's members each have
        // their own name and are all worth listing.
        let listed = if kind == JsonKind::Array {
            children.len().min(ARRAY_SAMPLE)
        } else {
            children.len()
        };
        for (i, child) in children.iter().take(listed).enumerate() {
            if *budget == 0 {
                return Ok(());
            }
            *budget -= 1;
            let child_path = path.child(&child.seg);
            let size = match child.len {
                Some(n) => format!(" ({n})"),
                None => String::new(),
            };
            out.push(format!(
                "  {} : {}{size}",
                child_path.expr(),
                child.kind.label()
            ));
            if child.kind.is_container() && i < SHAPE_FANOUT_CAP {
                walk_shape(driver, key, &child_path, depth + 1, budget, out).await?;
            }
        }
        let elided = len.saturating_sub(listed as u64);
        if elided > 0 {
            out.push(format!(
                "  {} : … {elided} more child(ren) not listed",
                path.expr(),
            ));
        }
        Ok(())
    })
}

/// `kv_json_set`: the gated write. Echo-and-verify like `kv_set` — the value is
/// written and then read back at the same path, so the reply states what is
/// actually there rather than that the command returned OK.
pub(super) async fn kv_json_set(driver: &Arc<dyn KvDriver>, input: &Json) -> (String, bool) {
    let key = input.get("key").and_then(Json::as_str).unwrap_or("");
    if key.is_empty() {
        return ("error: `key` is required".into(), false);
    }
    let raw_path = input.get("path").and_then(Json::as_str).unwrap_or("$");
    let path = match parse_path(raw_path) {
        Ok(p) => p,
        Err(why) => return (format!("error: {why}"), false),
    };
    let Some(value) = kv_json_set_value(input) else {
        return ("error: `value` is required".into(), false);
    };
    if let Err(e) = validate_json(&value) {
        return (format!("error: `value` is not valid JSON: {e}"), false);
    }
    if let Err(e) = driver.json_set(key, &path, &value).await {
        return (format!("error: {e}"), false);
    }
    match driver.read_json_node(key, &path, 0, JSON_NODE_WINDOW).await {
        Ok(Some(view)) => (
            format!(
                "Wrote `{key}` at `{}`. Reading it back: {}",
                path.expr(),
                fmt_json_node(&view)
            ),
            true,
        ),
        // The write reported success but the path is not there: say so plainly
        // rather than claiming a change that cannot be observed.
        Ok(None) => (
            format!(
                "warning: `JSON.SET` on `{key}` at `{}` reported success, but reading the path \
                 back found nothing.",
                path.expr()
            ),
            false,
        ),
        Err(e) => (
            format!(
                "Wrote `{key}` at `{}`, but reading it back failed: {e}",
                path.expr()
            ),
            true,
        ),
    }
}

/// The `value` argument as the JSON text that will actually be written.
///
/// Shared with the approval gate (`assess_kv_write`) so the user reads the exact
/// payload that runs, not a paraphrase of it. A model may send `value` either as
/// a JSON value (the natural thing) or as a string holding JSON (what it does
/// when the schema is loose), and both are accepted.
///
/// The subtle case is a *broken* string of JSON. A string that opens like a
/// container is handed back verbatim so the caller's [`validate_json`] rejects it
/// with an offset; wrapping it as a string literal would silently write
/// `"{\"a\":}"` into the document as text, which is a wrong write dressed as a
/// successful one.
pub(super) fn kv_json_set_value(input: &Json) -> Option<String> {
    match input.get("value")? {
        Json::String(s) => {
            let looks_structural = s.trim_start().starts_with(['{', '[']);
            if looks_structural || validate_json(s).is_ok() {
                Some(s.clone())
            } else {
                Some(Json::String(s.clone()).to_string())
            }
        }
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_path_accepts_the_forms_a_model_writes() {
        assert_eq!(parse_path("$").unwrap(), JsonPath::root());
        assert_eq!(parse_path("").unwrap(), JsonPath::root());
        assert_eq!(
            parse_path("$.a.b").unwrap(),
            JsonPath::root().member("a").member("b")
        );
        assert_eq!(
            parse_path("orders[3].id").unwrap(),
            JsonPath::root().member("orders").index(3).member("id")
        );
        assert_eq!(
            parse_path(r#"$["key with spaces"]"#).unwrap(),
            JsonPath::root().member("key with spaces")
        );
        assert_eq!(
            parse_path("a['x.y']").unwrap(),
            JsonPath::root().member("a").member("x.y")
        );
    }

    /// A quoted numeric subscript is a member name, not an index: `a["0"]`
    /// addresses an object's `"0"` key, which is not `a[0]`.
    #[test]
    fn parse_path_keeps_quoted_numbers_as_member_names() {
        assert_eq!(
            parse_path("a[0]").unwrap(),
            JsonPath::root().member("a").index(0)
        );
        assert_eq!(
            parse_path(r#"a["0"]"#).unwrap(),
            JsonPath::root().member("a").member("0")
        );
    }

    #[test]
    fn parse_path_rejects_an_unbalanced_bracket() {
        assert!(parse_path("a[3").is_err());
        assert!(parse_path(r#"a["x"#).is_err());
    }

    #[test]
    fn kv_json_set_value_takes_both_a_value_and_a_string_of_json() {
        let structured = serde_json::json!({ "value": { "a": 1 } });
        assert_eq!(
            kv_json_set_value(&structured).as_deref(),
            Some(r#"{"a":1}"#)
        );
        let stringified = serde_json::json!({ "value": "{\"a\":1}" });
        assert_eq!(
            kv_json_set_value(&stringified).as_deref(),
            Some(r#"{"a":1}"#)
        );
        // A bare string that is not JSON becomes a JSON string.
        let bare = serde_json::json!({ "value": "hello" });
        assert_eq!(kv_json_set_value(&bare).as_deref(), Some("\"hello\""));
        // Broken JSON that was meant AS JSON comes back verbatim, so the
        // caller's validation rejects it rather than writing it as text.
        let broken = serde_json::json!({ "value": "{\"a\":}" });
        assert_eq!(kv_json_set_value(&broken).as_deref(), Some("{\"a\":}"));
        assert_eq!(kv_json_set_value(&serde_json::json!({})), None);
    }
}
