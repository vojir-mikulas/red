//! The Redis seam's agent tools: the `KvDriver` half of the catalog.
//!
//! [`kv_run_tool`] is the executor. Layout mirrors [`sql`](super::sql): tool
//! definitions and the prompt in [`catalog`], the paging/diagnostic reads in
//! [`tools`], the write gate and its approval shapes in [`write`], the `kv_set`
//! plan in [`set`], and the INFO/analysis/value rendering in [`format`].

use std::sync::Arc;
use std::time::Duration;

use red_ai::CancelToken;
use red_core::AiPolicy;
use red_core::kv::{RespValue, analyze_keyspace, infer_key_templates};
use red_driver::KvDriver;
use serde_json::Value as Json;

use super::export::kv_export;
use super::gate::WriteAssessment;
use super::grounding::{run_recent_keys, run_search_history};
use super::knowledge::run_save_knowledge;
use super::report::run_generate_report;
use super::state::ReportSink;
use super::util::{cap_result_bytes, fmt_bytes};
use catalog::{KV_BIGGEST_TOP, KV_BULK_MAX, KV_SAMPLE_MAX, KV_TEMPLATE_TOP};
use format::{
    fmt_kv_value, kv_format_analysis, kv_format_templates, kv_info_summary, kv_ttl, resp_scalar,
};
use set::{kv_apply_set, kv_set_plan};
use tools::{kv_collect_keys, kv_read_collection, kv_stream_groups};
use write::{
    assess_kv_write, is_kv_write_tool, is_secret_config_param, kv_allowed_command, kv_command_argv,
    kv_delete_targets,
};

pub(super) mod catalog;
pub(super) mod format;
pub(super) mod set;
pub(super) mod tools;
pub(super) mod write;

/// Execute one Redis agent tool, the KV analogue of
/// [`run_tool`](super::sql::run_tool). Every arm goes through the `KvDriver`
/// seam, and shares the tier gate, the byte cap, and the `generate_report`
/// pipeline with the SQL path. The mutating arms run only after the turn loop
/// has taken the user's approval, and are re-vetted here regardless.
pub(in crate::ai) async fn kv_run_tool(
    driver: &Arc<dyn KvDriver>,
    conn: super::ConnCtx<'_>,
    name: &str,
    input: &Json,
    policy: &AiPolicy,
    _cancel: &CancelToken,
    report: &ReportSink,
) -> (String, bool) {
    if !policy.tier.allows_tool(name) {
        return (
            format!("error: the `{name}` tool is not available at this access tier"),
            false,
        );
    }
    // Defense in depth: run_turn already vetted and prompted, but re-run the
    // catastrophic-shape guard here so no caller (a subagent, a future path) can
    // execute a keyspace-wide write that assess_kv_write would refuse.
    if is_kv_write_tool(name)
        && let WriteAssessment::Reject(why) = assess_kv_write(name, input)
    {
        return (format!("error: {why}"), false);
    }
    let limits = &policy.limits;
    let conn_id = conn.conn_id;
    let (content, ok) = match name {
        "kv_server_info" => {
            // Topology and DBSIZE lead: this is the "call this first" tool, and
            // both change how every later answer should be read (a SCAN fans out
            // on a cluster; a sample is only meaningful against a total).
            let header = format!(
                "Topology: {:?} · DBSIZE {} key(s) in the selected database\n",
                driver.topology(),
                driver.db_size().await.unwrap_or(0),
            );
            match driver.command(&["INFO".to_string()]).await {
                Ok(RespValue::Bulk(info)) | Ok(RespValue::Simple(info)) => {
                    (format!("{header}{}", kv_info_summary(&info)), true)
                }
                Ok(other) => (format!("{header}unexpected INFO reply: {other:?}"), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_scan_keys" => {
            let pattern = input
                .get("pattern")
                .and_then(Json::as_str)
                .filter(|p| !p.is_empty());
            let limit = input
                .get("limit")
                .and_then(Json::as_u64)
                .map(|n| n as usize)
                .unwrap_or(limits.max_rows.max(1))
                .clamp(1, limits.max_rows.max(1));
            match kv_collect_keys(driver, pattern, limit).await {
                Ok((keys, exhausted)) => {
                    if keys.is_empty() {
                        ("No keys matched.".to_string(), true)
                    } else {
                        let mut out = format!("{} key(s):\n", keys.len());
                        for k in &keys {
                            out.push_str(&format!(
                                "  {}  [{}, {}, ~{}]\n",
                                k.key,
                                k.kv_type.label(),
                                kv_ttl(k.ttl),
                                fmt_bytes(k.approx_bytes),
                            ));
                        }
                        if !exhausted && keys.len() >= limit {
                            out.push_str(
                                "(more keys may match; raise `limit` or narrow the pattern)\n",
                            );
                        }
                        (out, true)
                    }
                }
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_key_schema" => {
            let pattern = input
                .get("pattern")
                .and_then(Json::as_str)
                .filter(|p| !p.is_empty());
            let total = driver.db_size().await.unwrap_or(0);
            match kv_collect_keys(driver, pattern, KV_SAMPLE_MAX).await {
                Ok((keys, exhausted)) => {
                    let templates = infer_key_templates(&keys, KV_TEMPLATE_TOP);
                    (
                        cap_result_bytes(
                            kv_format_templates(&templates, keys.len(), total, exhausted),
                            limits.max_result_bytes,
                        ),
                        true,
                    )
                }
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_key_info" => {
            let key = input.get("key").and_then(Json::as_str).unwrap_or("");
            if key.is_empty() {
                return ("error: `key` is required".into(), false);
            }
            match driver.probe_key(key).await {
                Ok(Some(m)) => (
                    format!(
                        "{}\n  type: {}\n  ttl: {}\n  encoding: {}\n  memory: ~{}",
                        m.key,
                        m.kv_type.label(),
                        kv_ttl(m.ttl),
                        m.encoding,
                        fmt_bytes(m.approx_bytes),
                    ),
                    true,
                ),
                Ok(None) => (format!("key `{key}` does not exist"), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_get_value" => {
            let key = input.get("key").and_then(Json::as_str).unwrap_or("");
            if key.is_empty() {
                return ("error: `key` is required".into(), false);
            }
            match driver.read_value(key).await {
                Ok(Some(v)) => (
                    cap_result_bytes(
                        format!("{key} =\n{}", fmt_kv_value(&v)),
                        limits.max_result_bytes,
                    ),
                    true,
                ),
                Ok(None) => (format!("key `{key}` does not exist"), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_read_collection" => kv_read_collection(driver, input, limits).await,
        "kv_stream_groups" => kv_stream_groups(driver, input, limits).await,
        "kv_keyspace_notifications" => match driver.notify_config().await {
            Ok(flags) if flags.trim().is_empty() => (
                "notify-keyspace-events is EMPTY: keyspace notifications are off, so nothing will \
                 be delivered to any subscriber until they are enabled."
                    .to_string(),
                true,
            ),
            Ok(flags) => (
                format!(
                    "notify-keyspace-events = {flags} (notifications are on for these classes)"
                ),
                true,
            ),
            Err(e) => (format!("error: {e}"), false),
        },
        "kv_command" => {
            let argv = kv_command_argv(input);
            match kv_allowed_command(&argv) {
                // Re-checked here as well as in the gate: this is the one tool
                // whose whole safety story is the allowlist, so it is enforced at
                // the point of execution, not only where the prompt was built.
                Err(why) => (format!("error: {why}"), false),
                Ok(()) => match driver.command(&argv).await {
                    Ok(v) => (
                        cap_result_bytes(format!("{v:?}"), limits.max_result_bytes),
                        true,
                    ),
                    Err(e) => (format!("error: {e}"), false),
                },
            }
        }
        "kv_client_list" => match driver.client_list().await {
            Ok(clients) if clients.is_empty() => ("No clients are connected.".to_string(), true),
            Ok(clients) => {
                let mut out = format!("{} connected client(s):\n", clients.len());
                for c in &clients {
                    out.push_str(&format!(
                        "  id={} {} db={} age={}s idle={}s flags={} last={}{}\n",
                        c.id,
                        c.addr,
                        c.db,
                        c.age,
                        c.idle,
                        c.flags,
                        c.cmd,
                        if c.name.is_empty() {
                            String::new()
                        } else {
                            format!(" name={}", c.name)
                        },
                    ));
                }
                (cap_result_bytes(out, limits.max_result_bytes), true)
            }
            Err(e) => (format!("error: {e}"), false),
        },
        "kv_biggest_keys" => {
            let pattern = input
                .get("pattern")
                .and_then(Json::as_str)
                .filter(|p| !p.is_empty());
            let top = input
                .get("top")
                .and_then(Json::as_u64)
                .map(|n| n as usize)
                .unwrap_or(KV_BIGGEST_TOP)
                .clamp(1, 200);
            match kv_collect_keys(driver, pattern, KV_SAMPLE_MAX).await {
                Ok((mut keys, exhausted)) => {
                    let sampled = keys.len();
                    keys.sort_by_key(|k| std::cmp::Reverse(k.approx_bytes));
                    keys.truncate(top);
                    let mut out = format!(
                        "Top {} of {} sampled key(s) by memory{}:\n",
                        keys.len(),
                        sampled,
                        if exhausted { "" } else { " (sample truncated)" },
                    );
                    for k in &keys {
                        out.push_str(&format!(
                            "  ~{}  {}  [{}, {}]\n",
                            fmt_bytes(k.approx_bytes),
                            k.key,
                            k.kv_type.label(),
                            kv_ttl(k.ttl),
                        ));
                    }
                    (out, true)
                }
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_analyze" => {
            let pattern = input
                .get("pattern")
                .and_then(Json::as_str)
                .filter(|p| !p.is_empty());
            let total = driver.db_size().await.unwrap_or(0);
            match kv_collect_keys(driver, pattern, KV_SAMPLE_MAX).await {
                Ok((keys, exhausted)) => {
                    let report = analyze_keyspace(&keys, total, !exhausted, 0);
                    (kv_format_analysis(&report), true)
                }
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_slowlog" => {
            let count = input
                .get("count")
                .and_then(Json::as_u64)
                .map(|n| n as usize)
                .unwrap_or(32)
                .clamp(1, 256);
            match driver.slowlog(count).await {
                Ok(entries) if entries.is_empty() => ("The slow log is empty.".to_string(), true),
                Ok(entries) => {
                    let mut out = format!("{} slow-log entr(ies):\n", entries.len());
                    for e in &entries {
                        out.push_str(&format!(
                            "  #{} {:.1}ms  {}\n",
                            e.id,
                            e.micros as f64 / 1000.0,
                            e.argv.join(" "),
                        ));
                    }
                    (cap_result_bytes(out, limits.max_result_bytes), true)
                }
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_config_get" => {
            let param = input.get("parameter").and_then(Json::as_str).unwrap_or("");
            if param.is_empty() {
                return ("error: `parameter` is required".into(), false);
            }
            let argv = ["CONFIG".to_string(), "GET".to_string(), param.to_string()];
            match driver.command(&argv).await {
                Ok(RespValue::Array(items)) if items.is_empty() => {
                    (format!("no CONFIG parameter matched `{param}`"), true)
                }
                Ok(RespValue::Array(items)) => {
                    let mut out = String::new();
                    for pair in items.chunks(2) {
                        let k = resp_scalar(pair.first());
                        // Never echo an auth secret back to the model/provider: a
                        // CONFIG GET of `requirepass` (or a glob like `*`) would
                        // otherwise exfiltrate the server's credentials as tool
                        // output. Redact the value while keeping the key visible.
                        let v = if is_secret_config_param(&k) {
                            "<redacted>".to_string()
                        } else {
                            resp_scalar(pair.get(1))
                        };
                        out.push_str(&format!("{k} = {v}\n"));
                    }
                    (out, true)
                }
                Ok(other) => (format!("unexpected CONFIG reply: {other:?}"), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        // --- gated writes: run_turn already surfaced the Allow/Deny prompt and
        // ran these only on approval; execute directly here.
        "kv_set" => match kv_set_plan(input) {
            Ok(plan) => kv_apply_set(driver, &plan).await,
            Err(why) => (format!("error: {why}"), false),
        },
        "kv_expire" => {
            let seconds = input.get("seconds").and_then(Json::as_i64);
            let ttl = match seconds {
                Some(s) if s > 0 => Some(Duration::from_secs(s as u64)),
                _ => None,
            };
            let verb = if ttl.is_some() {
                "Set expiry on"
            } else {
                "Removed expiry from"
            };
            if let Some(key) = input
                .get("key")
                .and_then(Json::as_str)
                .filter(|k| !k.is_empty())
            {
                match driver.set_ttl(key, ttl).await {
                    Ok(()) => (format!("{verb} `{key}`."), true),
                    Err(e) => (format!("error: {e}"), false),
                }
            } else if let Some(pattern) = input
                .get("pattern")
                .and_then(Json::as_str)
                .filter(|p| !p.is_empty())
            {
                match kv_collect_keys(driver, Some(pattern), KV_BULK_MAX).await {
                    Ok((keys, exhausted)) => {
                        let mut n = 0u64;
                        for k in &keys {
                            match driver.set_ttl(&k.key, ttl).await {
                                Ok(()) => n += 1,
                                Err(e) => return (format!("error after {n} key(s): {e}"), false),
                            }
                        }
                        let more = if exhausted {
                            ""
                        } else {
                            " (bound hit; run again for the rest)"
                        };
                        (
                            format!("{verb} {n} key(s) matching `{pattern}`{more}."),
                            true,
                        )
                    }
                    Err(e) => (format!("error: {e}"), false),
                }
            } else {
                ("error: kv_expire needs `key` or `pattern`".into(), false)
            }
        }
        "kv_delete" => {
            let mut targets = kv_delete_targets(input);
            let mut note = "";
            if targets.is_empty()
                && let Some(pattern) = input
                    .get("pattern")
                    .and_then(Json::as_str)
                    .filter(|p| !p.is_empty())
            {
                match kv_collect_keys(driver, Some(pattern), KV_BULK_MAX).await {
                    Ok((keys, exhausted)) => {
                        targets = keys.into_iter().map(|k| k.key).collect();
                        if !exhausted {
                            note = " (bound hit; run again for the rest)";
                        }
                    }
                    Err(e) => return (format!("error: {e}"), false),
                }
            }
            if targets.is_empty() {
                ("No keys matched; nothing deleted.".to_string(), true)
            } else {
                match driver.delete_keys(&targets).await {
                    Ok(n) => (format!("Deleted {n} key(s){note}."), true),
                    Err(e) => (format!("error: {e}"), false),
                }
            }
        }
        "kv_rename" => {
            let from = input.get("from").and_then(Json::as_str).unwrap_or("");
            let to = input.get("to").and_then(Json::as_str).unwrap_or("");
            if from.is_empty() || to.is_empty() {
                return ("error: kv_rename needs `from` and `to`".into(), false);
            }
            match driver.rename_key(from, to).await {
                Ok(()) => (format!("Renamed `{from}` to `{to}`."), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_copy_key" => {
            let from = input.get("from").and_then(Json::as_str).unwrap_or("");
            let to = input.get("to").and_then(Json::as_str).unwrap_or("");
            if from.is_empty() || to.is_empty() {
                return ("error: kv_copy_key needs `from` and `to`".into(), false);
            }
            let keep_ttl = input
                .get("keep_ttl")
                .and_then(Json::as_bool)
                .unwrap_or(true);
            let replace = input
                .get("replace")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            match driver.dump_key(from).await {
                Ok(None) => (
                    format!("key `{from}` does not exist; nothing copied."),
                    true,
                ),
                Ok(Some((payload, ttl))) => {
                    match driver
                        .restore_key(to, keep_ttl.then_some(ttl).flatten(), &payload, replace)
                        .await
                    {
                        Ok(()) => (format!("Copied `{from}` to `{to}`."), true),
                        // BUSYKEY is the guard doing its job, not a transport
                        // failure; say which so the model offers `replace` rather
                        // than retrying blindly.
                        Err(e) => (
                            format!(
                                "error: copying to `{to}` failed: {e}. If the key already exists, \
                                 set `replace` to overwrite it deliberately."
                            ),
                            false,
                        ),
                    }
                }
                Err(e) => (format!("error: reading `{from}` failed: {e}"), false),
            }
        }
        "kv_client_kill" => {
            let Some(id) = input.get("id").and_then(Json::as_i64) else {
                return ("error: kv_client_kill needs an `id`".into(), false);
            };
            // Re-resolve before cutting: a connection id the user approved may
            // have closed and been handed to somebody else since the prompt.
            match driver.client_list().await {
                Ok(clients) => match clients.iter().find(|c| c.id == id) {
                    None => (
                        format!("client {id} is no longer connected; nothing to disconnect."),
                        true,
                    ),
                    Some(live) => {
                        if let Some(claimed) = input
                            .get("addr")
                            .and_then(Json::as_str)
                            .filter(|a| !a.is_empty())
                            && live.addr != claimed
                        {
                            return (
                                format!(
                                    "error: client {id} is now {}, not {claimed}: the id was \
                                     reused since you read it. Re-read kv_client_list and propose \
                                     again.",
                                    live.addr
                                ),
                                false,
                            );
                        }
                        match driver.client_kill(id).await {
                            Ok(()) => (format!("Disconnected client {id} ({}).", live.addr), true),
                            Err(e) => (format!("error: {e}"), false),
                        }
                    }
                },
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "kv_config_set" => {
            let param = input.get("parameter").and_then(Json::as_str).unwrap_or("");
            let value = input.get("value").and_then(Json::as_str).unwrap_or("");
            if param.is_empty() {
                return ("error: kv_config_set needs `parameter`".into(), false);
            }
            let argv = [
                "CONFIG".to_string(),
                "SET".to_string(),
                param.to_string(),
                value.to_string(),
            ];
            match driver.command(&argv).await {
                Ok(_) => (format!("Applied CONFIG SET {param} {value}."), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "export_result" => kv_export(driver, input, report).await,
        "generate_report" => run_generate_report(input, report),
        "save_knowledge" => run_save_knowledge(input, report),
        // The Redis console records into the same per-connection log the SQL
        // editor does, so "what has a human run here" works on this seam too.
        "search_query_history" => run_search_history(conn_id, conn.dialect, input, limits),
        "kv_recent_keys" => run_recent_keys(conn_id, input, limits),
        other => (format!("error: unknown tool `{other}`"), false),
    };
    (content, ok)
}
