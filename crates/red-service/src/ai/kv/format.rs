//! Rendering Redis replies and rollups as the text the model reads.
//!
//! Pure over already-fetched values. The curation is the point: `INFO` is a
//! thousand lines the model should not pay for, so only the fields that change
//! an answer survive, and a credential-shaped `CONFIG` value never appears here
//! at all (see `is_secret_config_param`).

use std::collections::HashMap;
use std::time::Duration;

use red_core::kv::{KeyTemplate, KvCollection, KvValue, RespValue};

use super::super::sql::format::render_cell;
use super::super::util::fmt_bytes;
use super::catalog::KV_VALUE_ELEMS;

/// Curate the giant INFO reply down to the fields that matter, plus a computed
/// hit rate and the per-database key counts.
pub(super) fn kv_info_summary(info: &str) -> String {
    let map: HashMap<&str, &str> = info
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once(':'))
        .collect();
    let get = |k: &str| map.get(k).copied().unwrap_or("?");
    let hits: f64 = get("keyspace_hits").parse().unwrap_or(0.0);
    let misses: f64 = get("keyspace_misses").parse().unwrap_or(0.0);
    let hit_rate = if hits + misses > 0.0 {
        format!("{:.1}%", hits / (hits + misses) * 100.0)
    } else {
        "n/a".to_string()
    };
    let mut s = String::new();
    s.push_str(&format!(
        "Redis {} ({}), uptime {} days\n",
        get("redis_version"),
        get("redis_mode"),
        get("uptime_in_days"),
    ));
    s.push_str(&format!(
        "Memory: {} used, maxmemory {} (policy {}), fragmentation {}\n",
        get("used_memory_human"),
        get("maxmemory_human"),
        get("maxmemory_policy"),
        get("mem_fragmentation_ratio"),
    ));
    s.push_str(&format!(
        "Clients: {} connected · {} ops/sec\n",
        get("connected_clients"),
        get("instantaneous_ops_per_sec"),
    ));
    s.push_str(&format!(
        "Hit rate: {hit_rate} ({} hits / {} misses) · evicted {} · expired {}\n",
        get("keyspace_hits"),
        get("keyspace_misses"),
        get("evicted_keys"),
        get("expired_keys"),
    ));
    let dbs: Vec<&str> = info
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("db") && l.contains("keys="))
        .collect();
    if !dbs.is_empty() {
        s.push_str("Keyspace:\n");
        for db in dbs {
            s.push_str(&format!("  {db}\n"));
        }
    }
    s
}
/// Format inferred key templates as the keyspace's schema. The sample size and
/// whether it was exhaustive lead, because every number below is only as good as
/// the walk that produced it and a truncated sample reads as fact otherwise.
pub(super) fn kv_format_templates(
    templates: &[KeyTemplate],
    sampled: usize,
    total_keys: u64,
    exhausted: bool,
) -> String {
    if templates.is_empty() {
        return "No keys matched, so there is no key structure to report.".to_string();
    }
    let scope = if exhausted {
        format!("all {sampled} key(s)")
    } else {
        format!("a truncated sample of {sampled} key(s) of ~{total_keys} in the database")
    };
    let mut s = format!("Key templates inferred from {scope}:\n");
    for t in templates {
        let types = t
            .types
            .iter()
            .map(|(label, n)| {
                if t.types.len() == 1 {
                    label.clone()
                } else {
                    format!("{label} x{n}")
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        let avg = t.bytes / t.count.max(1);
        let ttl = if t.with_ttl == 0 {
            "no TTL".to_string()
        } else {
            format!(
                "TTL on {:.0}% (~{})",
                t.with_ttl as f64 / t.count as f64 * 100.0,
                kv_ttl(t.median_ttl),
            )
        };
        s.push_str(&format!(
            "  {}  {} keys  {types}  avg ~{}  {ttl}\n",
            t.pattern,
            t.count,
            fmt_bytes(avg),
        ));
    }
    if !exhausted {
        s.push_str(
            "(the sample stopped at a bound, so counts are proportions of the sample, not the \
             whole keyspace)\n",
        );
    }
    s
}
/// Format a [`RedisAnalysis`](red_core::kv::RedisAnalysis) as compact text for the agent.
pub(super) fn kv_format_analysis(r: &red_core::kv::RedisAnalysis) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Sampled {} of {} keys ({}), ~{} total.\n",
        r.sampled,
        r.total_keys,
        if r.truncated {
            "truncated sample"
        } else {
            "full walk"
        },
        fmt_bytes(r.total_bytes),
    ));
    s.push_str("By type (memory):\n");
    for t in &r.types {
        s.push_str(&format!(
            "  {}: {} keys, ~{}\n",
            t.kv_type,
            t.count,
            fmt_bytes(t.bytes),
        ));
    }
    s.push_str("Top namespaces (memory):\n");
    for n in r.namespaces.iter().take(15) {
        s.push_str(&format!(
            "  {}: {} keys, ~{}\n",
            n.prefix,
            n.count,
            fmt_bytes(n.bytes),
        ));
    }
    let t = &r.ttl;
    s.push_str(&format!(
        "TTL: {} persistent (no expiry), {} with a TTL (<1h {}, <1d {}, <1w {}, >1w {})\n",
        t.persistent,
        t.with_ttl(),
        t.under_hour,
        t.under_day,
        t.under_week,
        t.over_week,
    ));
    s
}
/// Preview a [`KvValue`]: a string's contents, or a bounded element preview of a
/// collection. Large collections report their length, not their contents.
pub(in crate::ai) fn fmt_kv_value(v: &KvValue) -> String {
    fn coll<T>(kind: &str, c: &KvCollection<T>, fmt: impl Fn(&T) -> String) -> String {
        match c {
            KvCollection::Loaded(items) => {
                let shown = items.len().min(KV_VALUE_ELEMS);
                let mut out = format!("{kind} with {} element(s):\n", items.len());
                for it in items.iter().take(shown) {
                    out.push_str(&format!("  {}\n", fmt(it)));
                }
                if items.len() > shown {
                    out.push_str(&format!("  … {} more\n", items.len() - shown));
                }
                out
            }
            KvCollection::Large { len } => {
                format!("{kind} with {len} element(s) (large; browse it to page the contents)")
            }
        }
    }
    match v {
        KvValue::Str(val) => format!("string: {}", render_cell(val)),
        KvValue::Hash(c) => coll("hash", c, |(f, val)| format!("{f} => {val}")),
        KvValue::Set(c) => coll("set", c, |m| m.clone()),
        KvValue::ZSet(c) => coll("zset", c, |(m, score)| format!("{m} ({score})")),
        KvValue::List(c) => coll("list", c, |m| m.clone()),
        KvValue::Stream(c) => match c {
            KvCollection::Loaded(entries) => format!("stream with {} entr(ies)", entries.len()),
            KvCollection::Large { len } => format!("stream with {len} entr(ies) (large)"),
        },
        KvValue::Unsupported(kt) => format!("(no value preview for type {})", kt.label()),
    }
}
/// A RESP scalar as plain text (for CONFIG GET pairs).
pub(super) fn resp_scalar(v: Option<&RespValue>) -> String {
    match v {
        Some(RespValue::Bulk(s)) | Some(RespValue::Simple(s)) => s.clone(),
        Some(RespValue::Int(i)) => i.to_string(),
        Some(other) => format!("{other:?}"),
        None => String::new(),
    }
}
/// `"no expiry"` or a coarse remaining-time for a key's TTL.
pub(in crate::ai) fn kv_ttl(ttl: Option<Duration>) -> String {
    match ttl {
        None => "no expiry".to_string(),
        Some(d) => {
            let s = d.as_secs();
            if s < 60 {
                format!("{s}s")
            } else if s < 3600 {
                format!("{}m", s / 60)
            } else if s < 86_400 {
                format!("{}h", s / 3600)
            } else {
                format!("{}d", s / 86_400)
            }
        }
    }
}

/// Render one command argument the way `redis-cli` echoes it: bare when it is a
/// simple token, double-quoted with escapes otherwise. The approval prompt is a
/// reading aid, so this only has to be unambiguous — an empty, spaced, or
/// newline-bearing value must not silently blend into the command around it.
pub(super) fn resp_arg(s: &str) -> String {
    let simple = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '.' | '/' | '@' | '+' | '#')
        });
    if simple {
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
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}
