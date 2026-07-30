//! The Redis seam's bounded walks and its deep reads.
//!
//! Every function here pages: [`kv_collect_keys`] loops `SCAN` under a round
//! cap, and `kv_read_collection` dispatches on the key's actual type to the
//! windowed reader for it, handing back the continuation token rather than
//! materializing a collection.

use std::sync::Arc;
use std::time::Duration;

use red_core::kv::{KeyMeta, ScanBudget, ScanCursor};
use red_core::{AiLimits, RedError};
use red_driver::{AbortSignal, KvDriver};
use serde_json::Value as Json;

use super::super::util::cap_result_bytes;
use super::catalog::{KV_PENDING_MAX, KV_SCAN_ROUNDS_CAP};
use super::format::kv_ttl;

/// Bounded keyspace walk: loop `scan_keys` accumulating metadata until `max_keys`
/// are collected, the keyspace is exhausted, or the round cap is hit. Returns the
/// keys (truncated to `max_keys`) and whether the walk exhausted the keyspace.
pub(in crate::ai) async fn kv_collect_keys(
    driver: &Arc<dyn KvDriver>,
    pattern: Option<&str>,
    max_keys: usize,
) -> Result<(Vec<KeyMeta>, bool), RedError> {
    let abort = AbortSignal::new();
    let mut cursor = ScanCursor::START;
    let mut out: Vec<KeyMeta> = Vec::new();
    let mut exhausted = false;
    for _ in 0..KV_SCAN_ROUNDS_CAP {
        let budget = ScanBudget {
            count_hint: 300,
            wall_clock: Duration::from_millis(300),
            want: 200,
        };
        let page = driver
            .scan_keys(cursor, pattern, None, None, budget, &abort)
            .await?;
        out.extend(page.keys);
        cursor = page.next_cursor;
        exhausted = page.exhausted;
        if exhausted || out.len() >= max_keys {
            break;
        }
    }
    out.truncate(max_keys);
    Ok((out, exhausted))
}
/// Page deep into one key's contents, dispatching on the key's actual type to
/// the windowed reader for it. Never loads a whole collection: the reply carries
/// the continuation token so the model pages rather than asking for everything.
pub(in crate::ai) async fn kv_read_collection(
    driver: &Arc<dyn KvDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    use red_core::kv::{CollectionKind, KvElement, KvType};

    let key = input.get("key").and_then(Json::as_str).unwrap_or("");
    if key.is_empty() {
        return ("error: `key` is required".into(), false);
    }
    let limit = input
        .get("limit")
        .and_then(Json::as_u64)
        .map(|n| n as usize)
        .unwrap_or(limits.max_rows.max(1))
        .clamp(1, limits.max_rows.max(1));
    let meta = match driver.probe_key(key).await {
        Ok(Some(m)) => m,
        Ok(None) => return (format!("key `{key}` does not exist"), true),
        Err(e) => return (format!("error: {e}"), false),
    };
    let budget = ScanBudget {
        count_hint: limit.min(1000) as u32,
        wall_clock: Duration::from_millis(500),
        want: limit,
    };
    let abort = AbortSignal::new();
    let kind = match meta.kv_type {
        KvType::Hash => Some(CollectionKind::Hash),
        KvType::Set => Some(CollectionKind::Set),
        KvType::ZSet => Some(CollectionKind::ZSet),
        _ => None,
    };
    let out = if let Some(kind) = kind {
        let cursor = input
            .get("cursor")
            .and_then(Json::as_str)
            .and_then(|c| c.parse::<u64>().ok())
            .unwrap_or(0);
        match driver
            .read_collection_page(key, kind, cursor, budget, &abort)
            .await
        {
            Ok(page) => {
                let mut s = format!(
                    "{key} ({}) — {} element(s):\n",
                    meta.kv_type.label(),
                    page.elements.len()
                );
                for e in &page.elements {
                    s.push_str(&match e {
                        KvElement::Member(m) => format!("  {m}\n"),
                        KvElement::Field(f, v) => format!("  {f} = {v}\n"),
                        KvElement::Scored(m, score) => format!("  {score}  {m}\n"),
                    });
                }
                s.push_str(&if page.exhausted {
                    "(end of collection)\n".to_string()
                } else {
                    format!("(more: pass cursor \"{}\" to continue)\n", page.next_cursor)
                });
                s
            }
            Err(e) => return (format!("error: {e}"), false),
        }
    } else if meta.kv_type == KvType::List {
        let from_tail = input
            .get("from_tail")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        // `LRANGE`'s cost grows with the offset, so the seam offers a head or a
        // tail window and no arbitrary deep-middle access; say so rather than
        // letting the model ask for page 900.
        match driver.read_list_window(key, !from_tail, limit).await {
            Ok(values) => {
                let mut s = format!(
                    "{key} (list) — {} element(s) from the {}:\n",
                    values.len(),
                    if from_tail { "tail" } else { "head" },
                );
                for v in &values {
                    s.push_str(&format!("  {v}\n"));
                }
                s.push_str("(lists window from either end only; there is no deep-middle page)\n");
                s
            }
            Err(e) => return (format!("error: {e}"), false),
        }
    } else if meta.kv_type == KvType::Stream {
        let before = input
            .get("before")
            .and_then(Json::as_str)
            .filter(|b| !b.is_empty());
        match driver.read_stream_range(key, before, limit).await {
            Ok(page) => {
                let mut s = format!(
                    "{key} (stream) — {} entr(ies), newest first:\n",
                    page.entries.len()
                );
                for e in &page.entries {
                    let fields: Vec<String> =
                        e.fields.iter().map(|(f, v)| format!("{f}={v}")).collect();
                    s.push_str(&format!("  {}  {}\n", e.id, fields.join(" ")));
                }
                s.push_str(&match (page.exhausted, &page.next_before) {
                    (false, Some(b)) => format!("(more: pass before \"{b}\" to walk older)\n"),
                    _ => "(end of stream)\n".to_string(),
                });
                s
            }
            Err(e) => return (format!("error: {e}"), false),
        }
    } else {
        return (
            format!(
                "`{key}` is a {}, which has no pages: read it with kv_get_value.",
                meta.kv_type.label()
            ),
            false,
        );
    };
    (cap_result_bytes(out, limits.max_result_bytes), true)
}
/// A stream's consumer-group diagnostics: every group, and optionally one
/// group's consumers and oldest pending entries.
pub(in crate::ai) async fn kv_stream_groups(
    driver: &Arc<dyn KvDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    let key = input.get("key").and_then(Json::as_str).unwrap_or("");
    if key.is_empty() {
        return ("error: `key` is required".into(), false);
    }
    let groups = match driver.stream_groups(key).await {
        Ok(g) => g,
        Err(e) => return (format!("error: {e}"), false),
    };
    if groups.is_empty() {
        return (
            format!("`{key}` has no consumer groups (entries are read directly, not via a group)."),
            true,
        );
    }
    let mut out = format!("{} consumer group(s) on `{key}`:\n", groups.len());
    for g in &groups {
        out.push_str(&format!(
            "  {}: {} consumer(s), {} pending, lag {}, last-delivered {}\n",
            g.name,
            g.consumers,
            g.pending,
            g.lag
                .map(|l| l.to_string())
                // Redis reports nil lag after certain trims; "unknown" is the
                // honest reading, and it is not the same as zero.
                .unwrap_or_else(|| "unknown".into()),
            g.last_delivered_id,
        ));
    }
    if let Some(group) = input
        .get("group")
        .and_then(Json::as_str)
        .filter(|g| !g.is_empty())
    {
        let count = limits.max_rows.clamp(1, KV_PENDING_MAX);
        match driver.stream_consumers(key, group).await {
            Ok(consumers) => {
                out.push_str(&format!("\nConsumers in `{group}`:\n"));
                for c in &consumers {
                    out.push_str(&format!(
                        "  {}: {} pending, idle {}\n",
                        c.name,
                        c.pending,
                        kv_ttl(Some(c.idle)),
                    ));
                }
            }
            Err(e) => out.push_str(&format!("\nConsumers in `{group}`: error: {e}\n")),
        }
        match driver.stream_pending(key, group, count).await {
            Ok(pending) if pending.is_empty() => {
                out.push_str("\nNothing pending: every delivered entry has been acked.\n");
            }
            Ok(pending) => {
                out.push_str(&format!("\n{} pending entr(ies):\n", pending.len()));
                for p in &pending {
                    out.push_str(&format!(
                        "  {} held by {}, idle {}, delivered {}x\n",
                        p.id,
                        p.consumer,
                        kv_ttl(Some(p.idle)),
                        p.delivery_count,
                    ));
                }
            }
            Err(e) => out.push_str(&format!("\nPending entries: error: {e}\n")),
        }
    }
    (cap_result_bytes(out, limits.max_result_bytes), true)
}
