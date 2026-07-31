//! The RedisJSON half of the Redis driver: the module probe and the lazy
//! document walk.
//!
//! The walk is the reason this file exists. The obvious implementation of "show
//! me this key" is one `JSON.GET <key> $`, which materializes the whole document
//! into the server's output buffer, the wire and RED's heap; a RedisJSON
//! document can be hundreds of megabytes, so that breaks the one invariant this
//! project does not bend. Instead a level is read as three pipelined round trips
//! that touch only that level:
//!
//! 1. `JSON.OBJKEYS`/`JSON.ARRLEN` for the child names or the count,
//! 2. one `JSON.TYPE` per child,
//! 3. one O(1) length command (`OBJLEN`/`ARRLEN`/`STRLEN`) or a `JSON.GET` per
//!    child, by kind — a scalar's whole value is tiny, and a long string reports
//!    its length instead of its contents.
//!
//! More round trips than a single `JSON.GET`, and the same trade RED makes
//! everywhere else. Parent items come via `use super::*`.

use red_core::Value;
use red_core::kv::{
    JSON_INLINE_STR_MAX, JsonDoc, JsonFetch, JsonKind, JsonNode, JsonNodeView, JsonPath, JsonSeg,
    KvModules, RespValue, json_fetch_mode, json_unwrap_singleton,
};

use super::*;

/// Probe which Redis Stack modules the server loaded.
///
/// `MODULE LIST` is the direct answer, but several managed providers restrict
/// it while still running the modules, so a refusal falls back to asking whether
/// the command itself exists (`COMMAND INFO JSON.GET` answers a non-nil row only
/// for a loaded module). Neither failure is fatal: the worst case is
/// [`KvModules::NONE`], which hides the JSON affordances but never blocks a
/// connect or a read.
pub(super) async fn probe_modules(conn: &mut MultiplexedConnection) -> KvModules {
    match redis::cmd("MODULE")
        .arg("LIST")
        .query_async::<redis::Value>(conn)
        .await
    {
        Ok(reply) => KvModules::from_module_list(&to_resp_value(reply)),
        Err(e) => {
            tracing::debug!("MODULE LIST unavailable, probing for RedisJSON directly: {e}");
            KvModules::NONE.with_json_probe(command_exists(conn, "JSON.GET").await)
        }
    }
}

/// Whether the server knows `name`, via `COMMAND INFO`. A loaded module's
/// commands are registered, so this is a module probe that needs no `MODULE`
/// permission. Any failure reads as "no".
async fn command_exists(conn: &mut MultiplexedConnection, name: &str) -> bool {
    let reply: Option<redis::Value> = redis::cmd("COMMAND")
        .arg("INFO")
        .arg(name)
        .query_async(conn)
        .await
        .ok();
    // The reply is a one-element array whose element is nil for an unknown
    // command and a spec array for a known one.
    matches!(
        reply.map(to_resp_value),
        Some(RespValue::Array(rows))
            if rows.first().is_some_and(|r| !matches!(r, RespValue::Nil))
    )
}

/// One key's RedisJSON value: probe the size, then either fetch the document
/// whole or walk its root level (see the module docs).
pub(super) async fn read_json_doc(
    conn: &mut MultiplexedConnection,
    key: &str,
) -> Result<Option<JsonDoc>> {
    let bytes = probe_doc_bytes(conn, key).await;
    let root = JsonPath::root();
    match json_fetch_mode(bytes) {
        JsonFetch::Whole => {
            let Some(text) = json_get_text(conn, key, &root).await? else {
                return Ok(None); // vanished between TYPE and here
            };
            Ok(Some(JsonDoc::Loaded {
                // The probe may have been refused even on the whole-fetch path
                // (`MEMORY USAGE` answered but `JSON.DEBUG` did not); report what
                // was actually fetched rather than a zero.
                bytes: bytes.unwrap_or(text.len() as u64),
                text,
            }))
        }
        JsonFetch::Lazy => {
            let Some(root_view) = read_node(conn, key, &root, 0, JSON_NODE_WINDOW).await? else {
                return Ok(None);
            };
            Ok(Some(JsonDoc::Lazy {
                bytes: bytes.unwrap_or(0),
                root: root_view,
            }))
        }
    }
}

/// A document's size in bytes, for the whole-vs-lazy decision.
///
/// `JSON.DEBUG MEMORY` is exact but is a `DEBUG` subcommand some providers
/// restrict, so it falls back to `MEMORY USAGE` — a different number (it counts
/// the key's total allocation, not the document's) but the right order of
/// magnitude for a threshold. `None` when neither answered, which
/// [`json_fetch_mode`] reads as "walk it".
async fn probe_doc_bytes(conn: &mut MultiplexedConnection, key: &str) -> Option<u64> {
    let debug: Option<i64> = redis::cmd("JSON.DEBUG")
        .arg("MEMORY")
        .arg(key)
        .query_async(conn)
        .await
        .ok();
    if let Some(n) = debug.filter(|n| *n >= 0) {
        return Some(n as u64);
    }
    let usage: Option<i64> = redis::cmd("MEMORY")
        .arg("USAGE")
        .arg(key)
        .query_async(conn)
        .await
        .ok();
    usage.filter(|n| *n >= 0).map(|n| n as u64)
}

/// Read one node at `path`: its kind, and either its scalar value or the window
/// of `count` children from `offset`. `Ok(None)` when the path matches nothing.
pub(super) async fn read_node(
    conn: &mut MultiplexedConnection,
    key: &str,
    path: &JsonPath,
    offset: u64,
    count: usize,
) -> Result<Option<JsonNodeView>> {
    let Some(kind) = json_type(conn, key, path).await? else {
        return Ok(None);
    };
    if !kind.is_container() {
        // A leaf has no window to page: RedisJSON can read a string's length but
        // not a slice of it, so the only bound available is the same one a Redis
        // string value gets -- fetch, then cap on arrival.
        let value = json_get_text(conn, key, path)
            .await?
            .map(|text| cap_string_value(text.into_bytes()))
            .unwrap_or(Value::Null);
        return Ok(Some(JsonNodeView::Scalar { kind, value }));
    }
    let count = count.max(1);
    let (len, segs) = match kind {
        JsonKind::Object => {
            let names = object_keys(conn, key, path).await?;
            let len = names.len() as u64;
            let window = names
                .into_iter()
                .skip(offset as usize)
                .take(count)
                .map(JsonSeg::Member)
                .collect();
            (len, window)
        }
        // An array reports its length in one O(1) call and never lists its
        // elements, so a million-element root costs the same as a ten-element
        // one; the window below is what actually gets read.
        _ => {
            let len = array_len(conn, key, path).await?;
            let window = (offset..len.min(offset.saturating_add(count as u64)))
                .map(JsonSeg::Index)
                .collect();
            (len, window)
        }
    };
    Ok(Some(JsonNodeView::Container {
        kind,
        len,
        offset,
        children: summarize_children(conn, key, path, segs).await?,
    }))
}

/// Fill in each child's kind, size and (for a small scalar) value, in two
/// pipelined round trips for the whole window.
async fn summarize_children(
    conn: &mut MultiplexedConnection,
    key: &str,
    parent: &JsonPath,
    segs: Vec<JsonSeg>,
) -> Result<Vec<JsonNode>> {
    if segs.is_empty() {
        return Ok(Vec::new());
    }
    let paths: Vec<JsonPath> = segs.iter().map(|s| parent.child(s)).collect();

    let mut types = redis::pipe();
    types.ignore_errors();
    for p in &paths {
        types.cmd("JSON.TYPE").arg(key).arg(p.expr());
    }
    let type_replies: Vec<redis::Value> = types
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    let kinds: Vec<Option<JsonKind>> = paths
        .iter()
        .enumerate()
        .map(|(i, _)| {
            type_replies
                .get(i)
                .and_then(first_string)
                .as_deref()
                .and_then(JsonKind::parse)
        })
        .collect();

    // Round two: one O(1) call per child, chosen by kind. A container and a
    // string report a length; a number/boolean/null is small enough to carry its
    // whole value, so it is fetched outright.
    let mut detail = redis::pipe();
    detail.ignore_errors();
    for (p, kind) in paths.iter().zip(&kinds) {
        match kind {
            Some(JsonKind::Object) => detail.cmd("JSON.OBJLEN").arg(key).arg(p.expr()),
            Some(JsonKind::Array) => detail.cmd("JSON.ARRLEN").arg(key).arg(p.expr()),
            Some(JsonKind::String) => detail.cmd("JSON.STRLEN").arg(key).arg(p.expr()),
            // `JSON.TYPE` on a vanished path replies with an empty array; ask
            // for the value anyway so the pipeline stays index-aligned with
            // `paths`, and drop the child below.
            _ => detail.cmd("JSON.GET").arg(key).arg(p.expr()),
        };
    }
    let detail_replies: Vec<redis::Value> = detail
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;

    // A long string leaf reports its length only; its contents are read by
    // opening it, so one chatty field can't drag a megabyte into an outline.
    let mut short_strings = Vec::new();
    let mut out = Vec::with_capacity(segs.len());
    for (i, (seg, kind)) in segs.into_iter().zip(&kinds).enumerate() {
        let Some(kind) = *kind else {
            continue; // the path went away between the scan and this read
        };
        let reply = detail_replies.get(i);
        let (len, preview) = match kind {
            JsonKind::Object | JsonKind::Array => (reply.and_then(first_int), None),
            JsonKind::String => {
                let len = reply.and_then(first_int);
                if len.is_some_and(|n| n <= JSON_INLINE_STR_MAX) {
                    short_strings.push((out.len(), parent.child(&seg)));
                }
                (len, None)
            }
            _ => (
                None,
                reply
                    .and_then(value_to_string)
                    .as_deref()
                    .and_then(json_unwrap_singleton)
                    .map(str::to_string),
            ),
        };
        out.push(JsonNode {
            seg,
            kind,
            len,
            preview,
        });
    }

    // Round three, only for the short string leaves: their contents, so a tree
    // row shows `"active"` rather than `string · 6`.
    if !short_strings.is_empty() {
        let mut values = redis::pipe();
        values.ignore_errors();
        for (_, p) in &short_strings {
            values.cmd("JSON.GET").arg(key).arg(p.expr());
        }
        let value_replies: Vec<redis::Value> = values
            .query_async(conn)
            .await
            .map_err(|e| RedError::Driver(e.to_string()))?;
        for (n, (slot, _)) in short_strings.into_iter().enumerate() {
            if let Some(node) = out.get_mut(slot) {
                node.preview = value_replies
                    .get(n)
                    .and_then(value_to_string)
                    .as_deref()
                    .and_then(json_unwrap_singleton)
                    .map(str::to_string);
            }
        }
    }
    Ok(out)
}

/// `JSON.TYPE key <path>`. `Ok(None)` when the path matches nothing (a missing
/// key, or a node deleted since it was listed).
async fn json_type(
    conn: &mut MultiplexedConnection,
    key: &str,
    path: &JsonPath,
) -> Result<Option<JsonKind>> {
    let reply: redis::Value = redis::cmd("JSON.TYPE")
        .arg(key)
        .arg(path.expr())
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    Ok(first_string(&reply).as_deref().and_then(JsonKind::parse))
}

async fn object_keys(
    conn: &mut MultiplexedConnection,
    key: &str,
    path: &JsonPath,
) -> Result<Vec<String>> {
    let reply: redis::Value = redis::cmd("JSON.OBJKEYS")
        .arg(key)
        .arg(path.expr())
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    // `$`-rooted paths answer with an array of matches, so the key list is
    // nested one level deep.
    let (redis::Value::Array(matches) | redis::Value::Set(matches)) = &reply else {
        return Ok(Vec::new());
    };
    Ok(matches.first().map(value_to_string_vec).unwrap_or_default())
}

async fn array_len(conn: &mut MultiplexedConnection, key: &str, path: &JsonPath) -> Result<u64> {
    let reply: redis::Value = redis::cmd("JSON.ARRLEN")
        .arg(key)
        .arg(path.expr())
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    Ok(first_int(&reply).unwrap_or(0))
}

/// `JSON.GET key <path>`, unwrapped from the single-element array a `$`-rooted
/// path replies with. `Ok(None)` when the path matched nothing.
pub(super) async fn json_get_text(
    conn: &mut MultiplexedConnection,
    key: &str,
    path: &JsonPath,
) -> Result<Option<String>> {
    let raw: Option<String> = redis::cmd("JSON.GET")
        .arg(key)
        .arg(path.expr())
        .query_async(conn)
        .await
        .map_err(|e| RedError::Driver(e.to_string()))?;
    Ok(raw
        .as_deref()
        .and_then(json_unwrap_singleton)
        .map(str::to_string))
}

/// The first element of the match array a `$`-rooted RedisJSON reply carries,
/// as text.
fn first_string(v: &redis::Value) -> Option<String> {
    match v {
        redis::Value::Array(items) | redis::Value::Set(items) => {
            items.first().and_then(value_to_string)
        }
        // RESP3, and the legacy-path shape, answer with the scalar directly.
        other => value_to_string(other),
    }
}

/// The first element of the match array, as a non-negative count. A nil element
/// (the path exists but has no length) reads as `None`, not `0`.
fn first_int(v: &redis::Value) -> Option<u64> {
    let first = match v {
        redis::Value::Array(items) | redis::Value::Set(items) => items.first()?,
        other => other,
    };
    value_to_i64(first).filter(|n| *n >= 0).map(|n| n as u64)
}
