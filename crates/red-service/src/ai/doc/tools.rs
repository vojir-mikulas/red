//! The MongoDB seam's composed reads, and its one server-state write.
//!
//! [`doc_reference_map`] is the significant one: Mongo declares no foreign keys,
//! so a reference can only be *guessed* from a field name and then **tested**.
//! Every guess is probed against the target's `_id` and reported with its hit
//! rate -- and a guess that resolves nothing is reported as unresolved rather
//! than dropped, because a silent omission reads as "no reference exists", which
//! is the opposite of what was found.

use std::sync::Arc;

use red_core::AiLimits;
use red_core::doc::{DocValue, Document, FindQuery};
use red_driver::{AbortSignal, DocDriver};
use serde_json::Value as Json;

use super::super::util::cap_result_bytes;
use super::write::doc_arg_value;

/// Documents sampled per collection when hunting reference candidates, and per
/// candidate field when collecting values to probe with. 200 is the plan's
/// number: enough that a hit rate means something, small enough that the whole
/// map is a handful of bounded reads.
const DOC_REF_SAMPLE: usize = 200;
/// Cap on candidate fields probed in one `doc_reference_map` call. Each is one
/// `find` plus one `count`, so this is what keeps the tool a map rather than a
/// crawl.
const DOC_REF_MAX_FIELDS: usize = 20;
/// Cap on collections whose schema is inferred in one call.
const DOC_REF_MAX_COLLECTIONS: usize = 25;
/// Databases that are the server's own bookkeeping, never a user's data model.
const DOC_SYSTEM_DBS: &[&str] = &["admin", "local", "config"];
/// One field that *looks* like a reference, with the target it would resolve
/// against. Built before any probing so the candidate list can be capped and
/// reported whole — including the ones that turn out to resolve nothing.
/// The Mongo analogue of `relationship_map`: guess which fields reference other
/// collections from their names, then *test each guess* against the target's
/// `_id` and report the hit rate.
///
/// The hit rate is the entire point. A name-based guess alone is exactly the
/// failure mode this tool exists to prevent, so an unresolved candidate is
/// reported as unresolved and never silently dropped: an omission would read as
/// "no reference exists", which is the opposite of what was found.
pub(in crate::ai) async fn doc_reference_map(
    driver: &Arc<dyn DocDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    let abort = AbortSignal::new();
    let dbs: Vec<String> = match input
        .get("db")
        .and_then(Json::as_str)
        .filter(|d| !d.is_empty())
    {
        Some(d) => vec![d.to_string()],
        None => match driver.list_databases().await {
            Ok(list) => list
                .into_iter()
                .map(|d| d.name)
                .filter(|n| !DOC_SYSTEM_DBS.contains(&n.as_str()))
                .collect(),
            Err(e) => return (format!("error: {e}"), false),
        },
    };
    let wanted: Vec<String> = input
        .get("collections")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut out = String::new();
    for db in &dbs {
        let catalog: Vec<String> = match driver.list_collections(db).await {
            Ok(list) => list.into_iter().map(|c| c.name).collect(),
            Err(e) => {
                out.push_str(&format!("{db}: error: {e}\n"));
                continue;
            }
        };
        let scanned: Vec<&String> = catalog
            .iter()
            .filter(|c| wanted.is_empty() || wanted.iter().any(|w| w == *c))
            .take(DOC_REF_MAX_COLLECTIONS)
            .collect();
        let mut candidates: Vec<red_core::doc::RefCandidate> = Vec::new();
        let mut truncated = scanned.len() < catalog.len();
        for coll in &scanned {
            let Ok(schema) = driver.infer_schema(db, coll, DOC_REF_SAMPLE, &abort).await else {
                continue;
            };
            candidates.extend(red_core::doc::reference_candidates(coll, &schema, &catalog));
            // Stopping here leaves later collections unexamined whether or not the
            // count lands exactly on the cap, so the truncation is recorded from
            // the break rather than inferred from the length afterwards.
            if candidates.len() >= DOC_REF_MAX_FIELDS {
                truncated = true;
                break;
            }
        }
        candidates.truncate(DOC_REF_MAX_FIELDS);

        out.push_str(&format!(
            "{db} ({} of {} collection(s) sampled, {} candidate field(s)):\n",
            scanned.len(),
            catalog.len(),
            candidates.len(),
        ));
        if candidates.is_empty() {
            out.push_str(
                "  No field names suggest a reference. Mongo declares none, so if these \
                 collections are related the link is by a name this heuristic does not \
                 recognize.\n",
            );
        }
        for c in &candidates {
            out.push_str(&probe_reference(driver, db, c, &abort).await);
        }
        if truncated {
            out.push_str(&format!(
                "  …(stopped early, at {DOC_REF_MAX_FIELDS} candidate fields or \
                 {DOC_REF_MAX_COLLECTIONS} collections; narrow with `collections` for the rest)\n"
            ));
        }
    }
    (cap_result_bytes(out, limits.max_result_bytes), true)
}
/// Sample one candidate field's values and count how many resolve to a document
/// in the target collection. One `find` and one `count`, both bounded.
async fn probe_reference(
    driver: &Arc<dyn DocDriver>,
    db: &str,
    c: &red_core::doc::RefCandidate,
    abort: &AbortSignal,
) -> String {
    let query = FindQuery {
        db: db.to_string(),
        coll: c.coll.clone(),
        // Ask only for the candidate path, so a wide document costs one field.
        projection: Some(DocValue::Document(vec![(
            c.path.clone(),
            DocValue::Int32(1),
        )])),
        filter: None,
        sort: None,
        skip: 0,
        limit: Some(DOC_REF_SAMPLE as u64),
        batch: DOC_REF_SAMPLE,
    };
    let page = match driver.find(&query, abort).await {
        Ok(p) => p,
        Err(e) => return format!("  {}.{} -> ? probe failed: {e}\n", c.coll, c.path),
    };
    let mut values: Vec<DocValue> = Vec::new();
    for doc in &page.docs {
        if let Some(v) = doc_path_value(doc, &c.path)
            && !matches!(v, DocValue::Null)
            && !values.contains(v)
        {
            values.push(v.clone());
        }
    }
    if values.is_empty() {
        return format!(
            "  {}.{} -> ? no values sampled ({}, {} doc(s) had no value here)\n",
            c.coll,
            c.path,
            c.doc_type.label(),
            page.docs.len(),
        );
    }
    let sampled = values.len();
    let filter = DocValue::Document(vec![(
        "_id".into(),
        DocValue::Document(vec![("$in".into(), DocValue::Array(values))]),
    )]);
    match driver.count(db, &c.target, Some(&filter)).await {
        Ok(0) => format!(
            "  {}.{} -> ? UNRESOLVED ({}, 0/{sampled} sampled values match any {}._id)\n",
            c.coll,
            c.path,
            c.doc_type.label(),
            c.target,
        ),
        Ok(hits) => format!(
            "  {}.{} -> {}._id ({}, {hits}/{sampled} sampled values resolve)\n",
            c.coll,
            c.path,
            c.target,
            c.doc_type.label(),
        ),
        Err(e) => format!(
            "  {}.{} -> {}._id probe failed: {e}\n",
            c.coll, c.path, c.target
        ),
    }
}
/// The value at a dotted `path` in a document, descending sub-documents. `_id`
/// is held beside the fields, so it is resolved explicitly rather than searched
/// for. `None` when any segment is missing or the path runs into a scalar.
fn doc_path_value<'a>(doc: &'a Document, path: &str) -> Option<&'a DocValue> {
    let mut segments = path.split('.');
    let first = segments.next()?;
    let mut current = if first == "_id" {
        &doc.id
    } else {
        doc.fields
            .iter()
            .find(|(k, _)| k == first)
            .map(|(_, v)| v)?
    };
    for segment in segments {
        let DocValue::Document(fields) = current else {
            return None;
        };
        current = fields.iter().find(|(k, _)| k == segment).map(|(_, v)| v)?;
    }
    Some(current)
}
pub(super) async fn doc_index_advice(driver: &Arc<dyn DocDriver>, input: &Json) -> (String, bool) {
    let db = input.get("db").and_then(Json::as_str).unwrap_or("");
    let coll = input.get("coll").and_then(Json::as_str).unwrap_or("");
    let filter = match doc_arg_value(driver, input, "filter") {
        Ok(v) => v,
        Err(e) => return (format!("error: {e}"), false),
    };
    let fields: Vec<String> = match &filter {
        Some(DocValue::Document(f)) => f
            .iter()
            .map(|(k, _)| k.clone())
            .filter(|k| !k.starts_with('$'))
            .collect(),
        _ => Vec::new(),
    };
    let query = FindQuery {
        db: db.to_string(),
        coll: coll.to_string(),
        filter,
        projection: None,
        sort: None,
        skip: 0,
        limit: None,
        batch: 1,
    };
    match driver.explain(&query).await {
        Ok(plan) => {
            if !plan.collscan {
                let idx = plan.index_used.as_deref().unwrap_or("an index");
                (
                    format!("Covered: the query uses {idx}. No new index needed."),
                    true,
                )
            } else if fields.is_empty() {
                (
                    "COLLSCAN, but the filter has no fields to index (it matches everything)."
                        .into(),
                    true,
                )
            } else {
                let spec = fields
                    .iter()
                    .map(|f| format!("\"{f}\": 1"))
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    format!(
                        "COLLSCAN — no index covers this filter. Suggested index on {db}.{coll}: \
                         {{ {spec} }}. Propose it with propose_index if the user wants it."
                    ),
                    true,
                )
            }
        }
        Err(e) => (format!("error: {e}"), false),
    }
}
/// `audit_collection`: sample the schema + read indexes, roll into a health report.
pub(super) async fn doc_audit_collection(
    driver: &Arc<dyn DocDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    let db = input.get("db").and_then(Json::as_str).unwrap_or("");
    let coll = input.get("coll").and_then(Json::as_str).unwrap_or("");
    let abort = AbortSignal::new();
    let schema = match driver.infer_schema(db, coll, 200, &abort).await {
        Ok(s) => s,
        Err(e) => return (format!("error: {e}"), false),
    };
    let indexes = driver.indexes(db, coll).await.unwrap_or_default();
    let count = driver.count(db, coll, None).await.ok();

    let drift: Vec<String> = schema
        .fields
        .iter()
        .filter(|f| f.types.len() > 1)
        .map(|f| {
            let types = f
                .types
                .iter()
                .map(|(t, _)| t.label())
                .collect::<Vec<_>>()
                .join("/");
            format!("{} ({types})", f.path)
        })
        .collect();
    let sparse: Vec<String> = schema
        .fields
        .iter()
        .filter(|f| f.present_ratio < 0.9 && f.path != "_id")
        .map(|f| format!("{} ({:.0}%)", f.path, f.present_ratio * 100.0))
        .collect();

    let mut out = format!("Health report for {db}.{coll}");
    if let Some(n) = count {
        out.push_str(&format!(" (~{n} documents)"));
    }
    out.push_str(":\n");
    out.push_str(&format!(
        "- Schema drift (mixed-type fields): {}\n",
        if drift.is_empty() {
            "none".into()
        } else {
            drift.join(", ")
        }
    ));
    out.push_str(&format!(
        "- Optional/sparse fields (present <90%): {}\n",
        if sparse.is_empty() {
            "none".into()
        } else {
            sparse.join(", ")
        }
    ));
    let secondary = indexes.iter().filter(|ix| ix.name != "_id_").count();
    out.push_str(&format!(
        "- Indexes: {} ({} secondary){}\n",
        indexes.len(),
        secondary,
        if secondary == 0 {
            " — only the default _id index; unindexed filters will collection-scan"
        } else {
            ""
        }
    ));
    (cap_result_bytes(out, limits.max_result_bytes), true)
}
/// `index_advice`: explain the filter, then report coverage / suggest a key.
/// Stop a running Mongo operation, re-resolving the opid against the live
/// deployment first. Same contract as [`kill_session`](crate::ai::sql::tools::kill_session): what the user approved
/// was a specific operation, and an opid can be reused, so the echoed facts are
/// verified rather than trusted.
pub(in crate::ai) async fn doc_kill_op(
    driver: &Arc<dyn DocDriver>,
    input: &Json,
) -> (String, bool) {
    let Some(opid) = input.get("opid").and_then(Json::as_i64) else {
        return ("error: doc_kill_op needs an `opid`".into(), false);
    };
    let ops = match driver.current_ops().await {
        Ok(o) => o,
        Err(e) => return (format!("error: {e}"), false),
    };
    let Some(live) = ops.iter().find(|o| o.opid == opid) else {
        return (
            format!("operation {opid} is no longer running; nothing to stop."),
            true,
        );
    };
    if let Some(claimed) = input
        .get("namespace")
        .and_then(Json::as_str)
        .filter(|n| !n.is_empty())
        && live.namespace != claimed
    {
        return (
            format!(
                "error: opid {opid} now runs on {}, not {claimed}: it was reused since you read \
                 it. Re-read doc_current_op and propose again.",
                live.namespace
            ),
            false,
        );
    }
    match driver.kill_op(opid).await {
        Ok(()) => (
            format!(
                "Stopped opid {opid} on {}. Mongo does not roll back a partially-applied \
                 multi-document write, so verify the data if it was one.",
                live.namespace
            ),
            true,
        ),
        Err(e) => (format!("error: {e}"), false),
    }
}

#[cfg(test)]
mod tests {
    use crate::ai::testutil::{doc_stub, doc_tool};
    use serde_json::json;

    #[tokio::test]
    async fn doc_reference_map_reports_hit_rates_and_names_the_unresolved() {
        let driver = doc_stub();
        let (content, ok) = doc_tool(&driver, "doc_reference_map", json!({ "db": "app" })).await;
        assert!(ok, "{content}");
        // A resolving reference reports its hit rate, not just its existence.
        assert!(
            content.contains("orders.customer_id -> customers._id"),
            "{content}"
        );
        assert!(content.contains("3/3 sampled values resolve"), "{content}");
        // A field whose values match nothing is reported as UNRESOLVED. Omitting
        // it would read as "no reference exists", the opposite of what was found.
        assert!(
            content.contains("orders.customerRef -> ? UNRESOLVED"),
            "{content}"
        );
        assert!(
            content.contains("0/3 sampled values match any customers._id"),
            "{content}"
        );
    }
}
