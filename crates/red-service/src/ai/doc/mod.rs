//! The MongoDB seam's agent tools: the `DocDriver` half of the catalog.
//!
//! [`doc_run_tool`] is the executor. Layout mirrors [`sql`](super::sql) and
//! [`kv`](super::kv): definitions and prompt in [`catalog`], the composed reads
//! in [`tools`], the gated writes in [`write`], and document/schema/plan
//! rendering in [`format`].

use std::sync::Arc;
use std::time::Duration;

use red_ai::CancelToken;
use red_core::doc::{CollKind, DocValue, FindQuery, pipeline_write_stage};
use red_core::{AiPolicy, RedError};
use red_driver::{AbortSignal, DocDriver};
use serde_json::Value as Json;

use super::export::doc_export;
use super::gate::WriteAssessment;
use super::knowledge::run_save_knowledge;
use super::report::run_generate_report;
use super::state::ReportSink;
use super::util::{cap_result_bytes, fmt_bytes, truncate_summary};
use format::{fmt_doc_indexes, fmt_doc_list, fmt_doc_plan, fmt_doc_profile, fmt_doc_schema};
use tools::{doc_audit_collection, doc_index_advice, doc_kill_op, doc_reference_map};
use write::{assess_doc_write, doc_apply_write, doc_arg_value, doc_index_spec, is_doc_write_tool};

pub(super) mod catalog;
pub(super) mod format;
pub(super) mod tools;
pub(super) mod write;

/// Run one MongoDB tool call against `driver`. Read tools compose the driver's
/// read methods; the `propose_*` writes execute only after the turn loop's
/// approval (re-vetted here as defense in depth).
pub(in crate::ai) async fn doc_run_tool(
    driver: &Arc<dyn DocDriver>,
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
    // Defense in depth: re-run the write gate here so a destructive shape can't
    // slip through even if the turn loop's check were ever bypassed.
    if is_doc_write_tool(name)
        && let WriteAssessment::Reject(why) = assess_doc_write(name, input)
    {
        return (format!("error: {why}"), false);
    }
    let limits = &policy.limits;
    let abort = AbortSignal::new();
    let db = || input.get("db").and_then(Json::as_str).unwrap_or("");
    let coll = || input.get("coll").and_then(Json::as_str).unwrap_or("");

    let (content, ok) = match name {
        "doc_server_info" => match driver.list_databases().await {
            Ok(dbs) => {
                let mut out = format!(
                    "MongoDB {}, topology: {:?}\nDatabases:\n",
                    driver.server_version(),
                    driver.topology()
                );
                for d in &dbs {
                    out.push_str(&format!(
                        "  {} ({} bytes on disk)\n",
                        d.name, d.size_on_disk
                    ));
                }
                (out, true)
            }
            Err(e) => (format!("error: {e}"), false),
        },
        "list_collections" => {
            let dbs: Vec<String> = match input.get("db").and_then(Json::as_str) {
                Some(d) if !d.is_empty() => vec![d.to_string()],
                _ => match driver.list_databases().await {
                    Ok(list) => list.into_iter().map(|d| d.name).collect(),
                    Err(e) => return (format!("error: {e}"), false),
                },
            };
            let mut out = String::new();
            for d in &dbs {
                match driver.list_collections(d).await {
                    Ok(colls) => {
                        out.push_str(&format!("{d}:\n"));
                        for c in &colls {
                            let kind = match c.kind {
                                CollKind::Collection => "",
                                CollKind::View => " (view)",
                                CollKind::Timeseries => " (timeseries)",
                            };
                            let capped = if c.capped { " capped" } else { "" };
                            let size = if c.size > 0 {
                                format!(", {}", fmt_bytes(c.size))
                            } else {
                                String::new()
                            };
                            let validator = if c.validator.is_some() {
                                " [has a validator]"
                            } else {
                                ""
                            };
                            out.push_str(&format!(
                                "  {} — ~{} docs{size}{kind}{capped}{validator}\n",
                                c.name, c.est_count
                            ));
                        }
                    }
                    Err(e) => out.push_str(&format!("{d}: error: {e}\n")),
                }
            }
            (out, true)
        }
        "describe_collection" => {
            let sample = 200;
            let schema = match driver.infer_schema(db(), coll(), sample, &abort).await {
                Ok(s) => s,
                Err(e) => return (format!("error: {e}"), false),
            };
            let indexes = driver.indexes(db(), coll()).await.unwrap_or_default();
            let mut out = format!(
                "{}\nIndexes:\n{}",
                fmt_doc_schema(&schema),
                fmt_doc_indexes(&indexes)
            );
            // A declared validator is the only *enforced* rule in a schemaless
            // store: a write that violates it bounces, so the model has to see it
            // here rather than discover it from a rejected insert.
            let validator = driver.list_collections(db()).await.ok().and_then(|list| {
                list.into_iter()
                    .find(|c| c.name == coll())
                    .and_then(|c| c.validator)
            });
            out.push_str(&match validator {
                Some(v) => format!(
                    "\nValidator (writes that violate this are rejected by the server):\n  {v}\n"
                ),
                None => "\nValidator: none declared.\n".to_string(),
            });
            (cap_result_bytes(out, limits.max_result_bytes), true)
        }
        "get_document" => {
            let id = match doc_arg_value(driver, input, "id") {
                Ok(Some(v)) => v,
                Ok(None) => return ("error: `id` is required".into(), false),
                Err(e) => return (format!("error: {e}"), false),
            };
            match driver.get_document(db(), coll(), &id).await {
                Ok(Some(doc)) => (
                    cap_result_bytes(
                        doc.to_doc_value().to_extended_json(),
                        limits.max_result_bytes,
                    ),
                    true,
                ),
                Ok(None) => (
                    format!(
                        "no document with that _id in {}.{}. If the _id is an ObjectId, pass it \
                         as {{\"$oid\": \"…\"}} rather than a bare string.",
                        db(),
                        coll()
                    ),
                    true,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "profile_collection" => {
            let sample = input
                .get("sample")
                .and_then(Json::as_u64)
                .map(|n| n as usize)
                .unwrap_or(200);
            match driver.infer_schema(db(), coll(), sample, &abort).await {
                Ok(schema) => (
                    cap_result_bytes(fmt_doc_profile(&schema), limits.max_result_bytes),
                    true,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "sample_documents" => {
            let n = input
                .get("n")
                .and_then(Json::as_u64)
                .map(|n| n as usize)
                .unwrap_or(5)
                .min(limits.max_rows.max(1));
            let pipeline = vec![DocValue::Document(vec![(
                "$sample".into(),
                DocValue::Document(vec![("size".into(), DocValue::Int64(n as i64))]),
            )])];
            match driver.aggregate(db(), coll(), &pipeline, n, &abort).await {
                Ok(page) => (
                    cap_result_bytes(fmt_doc_list(&page.docs), limits.max_result_bytes),
                    true,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "find" => {
            let cap = input
                .get("limit")
                .and_then(Json::as_u64)
                .map(|l| l as usize)
                .unwrap_or(limits.max_rows)
                .min(limits.max_rows.max(1));
            let filter = match doc_arg_value(driver, input, "filter") {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), false),
            };
            let projection = match doc_arg_value(driver, input, "projection") {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), false),
            };
            let sort = match doc_arg_value(driver, input, "sort") {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), false),
            };
            let query = FindQuery {
                db: db().to_string(),
                coll: coll().to_string(),
                filter,
                projection,
                sort,
                skip: 0,
                limit: Some(cap as u64),
                batch: cap,
            };
            match driver.find(&query, &abort).await {
                Ok(page) => (
                    cap_result_bytes(fmt_doc_list(&page.docs), limits.max_result_bytes),
                    true,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "aggregate" => {
            let stages = match doc_arg_value(driver, input, "pipeline") {
                Ok(Some(DocValue::Array(s))) => s,
                Ok(_) => {
                    return (
                        "error: `pipeline` must be a JSON array of stages".into(),
                        false,
                    );
                }
                Err(e) => return (format!("error: {e}"), false),
            };
            if let Some(bad) = pipeline_write_stage(&stages) {
                return (
                    format!("error: write stage `{bad}` is not allowed in a read-only aggregate"),
                    false,
                );
            }
            match driver
                .aggregate(db(), coll(), &stages, limits.max_rows.max(1), &abort)
                .await
            {
                Ok(page) => (
                    cap_result_bytes(fmt_doc_list(&page.docs), limits.max_result_bytes),
                    true,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "count" => {
            let filter = match doc_arg_value(driver, input, "filter") {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), false),
            };
            match driver.count(db(), coll(), filter.as_ref()).await {
                Ok(n) => (format!("{n} documents"), true),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "distinct" => {
            let field = input.get("field").and_then(Json::as_str).unwrap_or("");
            if field.is_empty() {
                return ("error: `field` is required".into(), false);
            }
            let filter = match doc_arg_value(driver, input, "filter") {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), false),
            };
            match driver.distinct(db(), coll(), field, filter.as_ref()).await {
                Ok(values) => {
                    let rendered: Vec<String> = values
                        .iter()
                        .take(limits.max_rows.max(1))
                        .map(DocValue::to_extended_json)
                        .collect();
                    (
                        cap_result_bytes(
                            format!(
                                "{} distinct value(s):\n{}",
                                values.len(),
                                rendered.join(", ")
                            ),
                            limits.max_result_bytes,
                        ),
                        true,
                    )
                }
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "explain_query" => {
            let filter = match doc_arg_value(driver, input, "filter") {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), false),
            };
            let query = FindQuery {
                db: db().to_string(),
                coll: coll().to_string(),
                filter,
                projection: None,
                sort: None,
                skip: 0,
                limit: None,
                batch: 1,
            };
            // `DocDriver::explain` always asks for `executionStats`, so the plan
            // already carries actuals beside the estimates and needs no `analyze`
            // flag. It does run the plan to gather them, though, so bound it like
            // any other read: a find can't be destructive, only slow.
            let explain = driver.explain(&query);
            let result = match limits.statement_timeout_ms {
                0 => explain.await,
                ms => tokio::time::timeout(Duration::from_millis(ms), explain)
                    .await
                    .unwrap_or(Err(RedError::Timeout)),
            };
            match result {
                Ok(plan) => (fmt_doc_plan(&plan), true),
                Err(RedError::Timeout) => (
                    "error: the explain exceeded the agent's statement timeout; narrow the filter."
                        .into(),
                    false,
                ),
                Err(e) => (format!("error: {e}"), false),
            }
        }
        "doc_reference_map" => doc_reference_map(driver, input, limits).await,
        "doc_current_op" => match driver.current_ops().await {
            Ok(ops) if ops.is_empty() => ("Nothing is running right now.".to_string(), true),
            Ok(ops) => {
                let mut out = format!("{} running operation(s), longest first:\n", ops.len());
                for o in &ops {
                    out.push_str(&format!(
                        "  opid {} {} on {} for {:.1}s{}{}\n",
                        o.opid,
                        o.op,
                        if o.namespace.is_empty() {
                            "(no namespace)"
                        } else {
                            &o.namespace
                        },
                        o.secs_running,
                        o.client
                            .as_deref()
                            .map(|c| format!(" from {c}"))
                            .unwrap_or_default(),
                        if o.waiting_for_lock {
                            " — WAITING FOR LOCK"
                        } else {
                            ""
                        },
                    ));
                    if let Some(cmd) = &o.command {
                        out.push_str(&format!("    {}\n", truncate_summary(cmd, 300)));
                    }
                }
                (cap_result_bytes(out, limits.max_result_bytes), true)
            }
            Err(e) => (format!("error: {e}"), false),
        },
        "doc_kill_op" => doc_kill_op(driver, input).await,
        "index_advice" => doc_index_advice(driver, input).await,
        "audit_collection" => doc_audit_collection(driver, input, limits).await,
        "export_result" => doc_export(driver, input, report).await,
        "generate_report" => run_generate_report(input, report),
        "save_knowledge" => run_save_knowledge(input, report),
        // Gated writes — the approval already happened in the turn loop.
        "propose_doc_write" => doc_apply_write(driver, input).await,
        "propose_index" => match doc_index_spec(input) {
            Ok(spec) => match driver.create_index(db(), coll(), &spec).await {
                Ok(()) => ("index created".into(), true),
                Err(e) => (format!("error: {e}"), false),
            },
            Err(e) => (format!("error: {e}"), false),
        },
        "propose_collection_op" => match input.get("op").and_then(Json::as_str).unwrap_or("") {
            "create" => match driver.create_collection(db(), coll()).await {
                Ok(()) => (format!("created collection {}.{}", db(), coll()), true),
                Err(e) => (format!("error: {e}"), false),
            },
            "drop" => match driver.drop_collection(db(), coll()).await {
                Ok(()) => (format!("dropped collection {}.{}", db(), coll()), true),
                Err(e) => (format!("error: {e}"), false),
            },
            other => (format!("error: unknown collection op `{other}`"), false),
        },
        other => (format!("error: unknown tool `{other}`"), false),
    };
    (content, ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::testutil::{doc_stub, doc_tool};
    use red_core::AiTier;
    use serde_json::json;

    #[tokio::test]
    async fn get_document_fetches_by_id_and_reports_a_miss() {
        let driver = doc_stub();
        let (content, ok) = doc_tool(
            &driver,
            "get_document",
            json!({ "db": "app", "coll": "customers", "id": 2 }),
        )
        .await;
        assert!(ok, "{content}");
        assert!(content.contains("\"c2\""), "{content}");
        // A miss is a normal answer, not an error, and it says how to spell an
        // ObjectId in case that was the mistake.
        let (content, ok) = doc_tool(
            &driver,
            "get_document",
            json!({ "db": "app", "coll": "customers", "id": 99 }),
        )
        .await;
        assert!(ok, "{content}");
        assert!(content.contains("$oid"), "{content}");
    }

    #[tokio::test]
    async fn describe_collection_reports_whether_a_validator_exists() {
        let driver = doc_stub();
        let (content, ok) = doc_tool(
            &driver,
            "describe_collection",
            json!({ "db": "app", "coll": "customers" }),
        )
        .await;
        assert!(ok, "{content}");
        // Absence is stated rather than left to inference: "no validator line"
        // and "no validator" must not look the same.
        assert!(content.contains("Validator: none declared."), "{content}");
    }

    /// A denied write must come back as a recoverable error, not a dead turn.
    /// `run_tool` is the executor half of that: a rejected assessment is an
    /// `is_error` result carrying the reason.
    #[tokio::test]
    async fn a_refused_kill_is_a_recoverable_tool_error() {
        let driver = doc_stub();
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        let (content, ok) = doc_run_tool(
            &driver,
            "doc_kill_op",
            &json!({}),
            &write,
            &CancelToken::new(),
            &ReportSink::disabled(),
        )
        .await;
        assert!(!ok, "a refusal must be an is_error result");
        assert!(content.starts_with("error:"), "{content}");
        assert!(content.contains("doc_current_op"), "{content}");
    }
}
