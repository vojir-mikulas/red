//! Every MongoDB mutation, and the approval shape each one is shown under.
//!
//! Two rules carry the file. The prompt renders what will actually be *written*,
//! not just what it will be matched against -- showing a filter alone once let a
//! `{role: "admin"}` patch ride along unseen. And a model-supplied document is
//! refused if it smuggles a server-side JavaScript operator, since those execute
//! code inside mongod and no tool here is described as doing that.

use std::sync::Arc;

use red_core::doc::{
    DocUpdate, DocValue, DocWrite, Document, IndexSpec, OpClass, classify_doc_op,
    server_js_operator,
};
use red_driver::DocDriver;
use serde_json::Value as Json;

use super::super::gate::WriteAssessment;
use super::super::util::truncate_summary;
use red_core::RedError;

// ============================================================================
// MongoDB (document) agent — the `DocDriver` backend's tools, mirroring the
// SQL and Redis (`kv_*`) catalogs. Read tools auto-run (they're in
// `READ_ONLY_TOOLS`); the `propose_*` writes ride the per-call approval gate.
// The signature tools (`profile_collection`/`audit_collection`/`index_advice`)
// are host-side compositions over the driver's read methods — no new seam.
// ============================================================================

/// The doc backend's mutating tools; each rides the same per-call approval gate
/// as a SQL/Redis write. Their complement (the reads) is the `doc_*`/`find`/…
/// set listed in [`READ_ONLY_TOOLS`](crate::ai::gate::READ_ONLY_TOOLS).
const DOC_WRITE_TOOLS: &[&str] = &[
    "propose_doc_write",
    "propose_index",
    "propose_collection_op",
    // Not a document write, but a server-state one that rides the same gate.
    "doc_kill_op",
];
pub(in crate::ai) fn is_doc_write_tool(name: &str) -> bool {
    DOC_WRITE_TOOLS.contains(&name)
}
/// How much of a proposed document/update payload the approval prompt shows.
/// Long enough for a realistic `$set`, short enough that a huge model-supplied
/// document cannot push the actual operation off the top of the dialog.
const DOC_PAYLOAD_CHARS: usize = 600;
/// Vet a doc write tool for the approval gate: build the human-readable operation
/// shown in Allow/Deny, and hard-block the footguns (an unfiltered update/delete)
/// even with approval. Tier + read-only were already checked by [`assess_write`](crate::ai::gate::assess_write).
pub(in crate::ai) fn assess_doc_write(name: &str, input: &Json) -> WriteAssessment {
    let s = |k: &str| {
        input
            .get(k)
            .and_then(Json::as_str)
            .filter(|v| !v.is_empty())
    };
    // A filter is "present" only if it's a non-empty JSON object — the doc-seam
    // analog of "UPDATE/DELETE need a WHERE".
    let has_filter = input
        .get("filter")
        .and_then(Json::as_object)
        .is_some_and(|o| !o.is_empty());
    let ns = format!("{}.{}", s("db").unwrap_or("?"), s("coll").unwrap_or("?"));
    let filter_txt = input
        .get("filter")
        .map(|f| f.to_string())
        .unwrap_or_else(|| "{}".into());
    // What will actually be written, not just what it will be matched against.
    //
    // The SQL path shows the entire statement; this showed only op + namespace +
    // filter, while the executor went on to apply the `update`/`document` fields
    // that were never displayed. A proposal of
    // `{op:"update", filter:{email:"x"}, update:{role:"admin"}}` rendered as a bland
    // "UPDATE db.users matching {email:x}" and a reasonable user approved a
    // privilege escalation they never saw. What executes must be derived into what
    // is shown — the same rule the `kv_delete` prompt now follows.
    let payload = |key: &str| match input.get(key) {
        Some(v) => format!("\n{}", truncate_summary(&v.to_string(), DOC_PAYLOAD_CHARS)),
        None => String::new(),
    };
    match name {
        "propose_doc_write" => {
            let op = s("op").unwrap_or("");
            let many = input.get("many").and_then(Json::as_bool).unwrap_or(false);
            let many_note = if many { " (many: ALL matches)" } else { "" };
            match op {
                "insert" => WriteAssessment::NeedsApproval {
                    sql: format!("INSERT one document into {ns}{}", payload("document")),
                },
                "replace" => {
                    if !has_filter {
                        return WriteAssessment::Reject(
                            "replace requires a non-empty filter (e.g. { _id: ... })".into(),
                        );
                    }
                    WriteAssessment::NeedsApproval {
                        sql: format!(
                            "REPLACE document in {ns} matching {filter_txt}{}",
                            payload("document")
                        ),
                    }
                }
                "update" => {
                    if !has_filter {
                        return WriteAssessment::Reject(
                            "update requires a non-empty filter (refusing an unfiltered update)"
                                .into(),
                        );
                    }
                    WriteAssessment::NeedsApproval {
                        sql: format!(
                            "UPDATE {ns} matching {filter_txt}{many_note}{}",
                            payload("update")
                        ),
                    }
                }
                "delete" => {
                    if !has_filter {
                        return WriteAssessment::Reject(
                            "delete requires a non-empty filter (refusing an unfiltered delete)"
                                .into(),
                        );
                    }
                    WriteAssessment::NeedsApproval {
                        sql: format!("DELETE from {ns} matching {filter_txt}{many_note}"),
                    }
                }
                other => WriteAssessment::Reject(format!(
                    "propose_doc_write `op` must be insert/update/replace/delete, not `{other}`"
                )),
            }
        }
        "propose_index" => {
            let keys = input
                .get("keys")
                .map(|k| k.to_string())
                .unwrap_or_else(|| "{}".into());
            let unique = input.get("unique").and_then(Json::as_bool).unwrap_or(false);
            let unique_note = if unique { " UNIQUE" } else { "" };
            WriteAssessment::NeedsApproval {
                sql: format!("CREATE{unique_note} INDEX on {ns} keys {keys}"),
            }
        }
        "doc_kill_op" => match input.get("opid").and_then(Json::as_i64) {
            Some(opid) => {
                let mut op = format!("KILL operation {opid}");
                if let Some(ns) = s("namespace") {
                    op.push_str(&format!(" on {ns}"));
                }
                op.push_str(
                    "\n\u{26a0} An interrupted multi-document write is NOT rolled back: what it \
                     already changed stays changed.",
                );
                match s("command") {
                    Some(cmd) => op.push_str(&format!(
                        "\nRunning: {}",
                        truncate_summary(cmd, DOC_PAYLOAD_CHARS)
                    )),
                    None => op.push_str(
                        "\nThe agent did not say what this operation is; read doc_current_op \
                         before allowing.",
                    ),
                }
                WriteAssessment::NeedsApproval { sql: op }
            }
            None => WriteAssessment::Reject(
                "doc_kill_op needs the numeric `opid` of an operation from doc_current_op".into(),
            ),
        },
        "propose_collection_op" => match s("op").unwrap_or("") {
            "create" => WriteAssessment::NeedsApproval {
                sql: format!("CREATE collection {ns}"),
            },
            "drop" => WriteAssessment::NeedsApproval {
                sql: format!("DROP collection {ns} — destructive, cannot be undone"),
            },
            other => WriteAssessment::Reject(format!(
                "propose_collection_op `op` must be create/drop, not `{other}`"
            )),
        },
        other => WriteAssessment::Reject(format!("unknown doc write tool `{other}`")),
    }
}
/// Parse a tool-input value (`filter`/`projection`/`sort`/`pipeline`) into a
/// [`DocValue`] via the driver's extended-JSON parser. The model may pass it as a
/// JSON object/array (the usual case) or as an extended-JSON string.
///
/// Every model-supplied document is refused here if it smuggles a server-side
/// JavaScript operator (`$where`/`$function`/`$accumulator`): those execute code
/// inside mongod, which no tool in the catalog is described as doing, and a
/// stored prompt-injection payload could plant one in an otherwise-read call.
pub(in crate::ai) fn doc_arg_value(
    driver: &Arc<dyn DocDriver>,
    input: &Json,
    key: &str,
) -> Result<Option<DocValue>, String> {
    let parsed = match input.get(key) {
        None | Some(Json::Null) => return Ok(None),
        Some(Json::String(s)) if s.trim().is_empty() => return Ok(None),
        Some(Json::String(s)) => driver.parse_ext_json(s).map_err(|e| e.to_string())?,
        Some(other) => driver
            .parse_ext_json(&other.to_string())
            .map_err(|e| e.to_string())?,
    };
    if let Some(op) = server_js_operator(&parsed) {
        return Err(format!(
            "`{op}` executes server-side JavaScript and is not allowed in `{key}`"
        ));
    }
    Ok(Some(parsed))
}
/// Build an [`IndexSpec`] from a `propose_index` input (`keys` object of
/// field → direction).
pub(super) fn doc_index_spec(input: &Json) -> Result<IndexSpec, String> {
    let keys = input
        .get("keys")
        .and_then(Json::as_object)
        .ok_or("`keys` must be an object, e.g. { \"email\": 1 }")?;
    if keys.is_empty() {
        return Err("`keys` must name at least one field".into());
    }
    let keys = keys
        .iter()
        .map(|(field, dir)| {
            // The agent's `keys` object speaks directions only. A special index
            // type (text, hashed, 2dsphere) is a deliberate choice a human makes
            // in the index dialog, not one a model should reach for from a filter
            // it has just explained.
            let d = dir.as_i64().unwrap_or(1);
            let kind = if d < 0 {
                red_core::doc::IndexKey::Desc
            } else {
                red_core::doc::IndexKey::Asc
            };
            (field.clone(), kind)
        })
        .collect();
    Ok(IndexSpec {
        keys,
        unique: input.get("unique").and_then(Json::as_bool).unwrap_or(false),
        name: input.get("name").and_then(Json::as_str).map(str::to_string),
        sparse: false,
        ttl_seconds: None,
        partial_filter: None,
        collation_locale: None,
    })
}
/// Execute an approved `propose_doc_write` by building the [`DocWrite`] and
/// dispatching to the driver.
pub(super) async fn doc_apply_write(driver: &Arc<dyn DocDriver>, input: &Json) -> (String, bool) {
    let db = input
        .get("db")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    let coll = input
        .get("coll")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    let op = input.get("op").and_then(Json::as_str).unwrap_or("");
    let many = input.get("many").and_then(Json::as_bool).unwrap_or(false);

    let parse = |key: &str| doc_arg_value(driver, input, key);
    let write = match op {
        "insert" => match parse("document") {
            Ok(Some(v)) => match Document::from_doc_value(v) {
                Some(doc) => DocWrite::Insert {
                    db,
                    coll,
                    docs: vec![doc],
                },
                None => return ("error: `document` must be a JSON object".into(), false),
            },
            _ => return ("error: insert needs a `document` object".into(), false),
        },
        "update" => {
            let Ok(Some(filter)) = parse("filter") else {
                return ("error: update needs a `filter`".into(), false);
            };
            let Ok(Some(patch)) = parse("update") else {
                return (
                    "error: update needs an `update` (the $set fields)".into(),
                    false,
                );
            };
            DocWrite::Update {
                db,
                coll,
                filter,
                change: DocUpdate::Patch(patch),
                many,
            }
        }
        "replace" => {
            let Ok(Some(filter)) = parse("filter") else {
                return ("error: replace needs a `filter`".into(), false);
            };
            let id = match &filter {
                DocValue::Document(fields) => fields
                    .iter()
                    .find(|(k, _)| k == "_id")
                    .map(|(_, v)| v.clone()),
                _ => None,
            };
            let Some(id) = id else {
                return ("error: replace `filter` must pin `_id`".into(), false);
            };
            match parse("document") {
                Ok(Some(v)) => match Document::from_doc_value(v) {
                    Some(doc) => DocWrite::Replace { db, coll, id, doc },
                    None => return ("error: `document` must be a JSON object".into(), false),
                },
                _ => return ("error: replace needs a `document` object".into(), false),
            }
        }
        "delete" => {
            let Ok(Some(filter)) = parse("filter") else {
                return ("error: delete needs a `filter`".into(), false);
            };
            DocWrite::Delete {
                db,
                coll,
                filter,
                many,
            }
        }
        other => return (format!("error: unknown op `{other}`"), false),
    };
    // Defense in depth: never run a destructive shape the classifier flags,
    // even though the approval gate already prompted.
    if classify_doc_op(&write) == OpClass::Destructive
        && let WriteAssessment::Reject(why) = assess_doc_write("propose_doc_write", input)
    {
        return (format!("error: {why}"), false);
    }
    match doc_execute_write(driver, write).await {
        Ok(summary) => (summary, true),
        Err(e) => (format!("error: {e}"), false),
    }
}
/// Dispatch a [`DocWrite`] to the driver, returning a short summary.
async fn doc_execute_write(
    driver: &Arc<dyn DocDriver>,
    write: DocWrite,
) -> Result<String, RedError> {
    match write {
        DocWrite::Insert { db, coll, docs } => {
            let n = driver.insert(&db, &coll, &docs).await?;
            Ok(format!("inserted {n} document(s)"))
        }
        DocWrite::Update {
            db,
            coll,
            filter,
            change,
            many,
        } => {
            let n = driver.update(&db, &coll, &filter, &change, many).await?;
            Ok(format!("updated {n} document(s)"))
        }
        DocWrite::Replace { db, coll, id, doc } => {
            driver.replace(&db, &coll, &id, &doc).await?;
            Ok("document replaced".into())
        }
        DocWrite::Delete {
            db,
            coll,
            filter,
            many,
        } => {
            let n = driver.delete(&db, &coll, &filter, many).await?;
            Ok(format!("deleted {n} document(s)"))
        }
        // The DDL writes ride their own tools; not reachable from propose_doc_write.
        DocWrite::CreateCollection { .. }
        | DocWrite::DropCollection { .. }
        | DocWrite::CreateIndex { .. }
        | DocWrite::DropIndex { .. }
        | DocWrite::SetValidator { .. } => Ok("unsupported write".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::testutil::assess_write;
    use red_core::{AiPolicy, AiTier};
    use serde_json::json;

    #[test]
    fn doc_write_gate_requires_filter_and_confirms_drop() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        // An unfiltered update/delete is refused outright, even with approval.
        assert!(matches!(
            assess_write(
                "propose_doc_write",
                &json!({ "op": "delete", "db": "d", "coll": "c" }),
                &write
            ),
            WriteAssessment::Reject(_)
        ));
        // A filtered delete prompts.
        assert!(matches!(
            assess_write(
                "propose_doc_write",
                &json!({ "op": "delete", "db": "d", "coll": "c", "filter": { "_id": 1 } }),
                &write
            ),
            WriteAssessment::NeedsApproval { .. }
        ));
        // An insert prompts without a filter (nothing to over-match).
        assert!(matches!(
            assess_write(
                "propose_doc_write",
                &json!({ "op": "insert", "db": "d", "coll": "c" }),
                &write
            ),
            WriteAssessment::NeedsApproval { .. }
        ));
        // Dropping a collection prompts (the approval string carries the warning).
        assert!(matches!(
            assess_write(
                "propose_collection_op",
                &json!({ "op": "drop", "db": "d", "coll": "c" }),
                &write
            ),
            WriteAssessment::NeedsApproval { .. }
        ));
        // Below Write tier, every doc write is rejected without a prompt.
        let read = AiPolicy::default();
        assert!(matches!(
            assess_write(
                "propose_doc_write",
                &json!({ "op": "insert", "db": "d", "coll": "c" }),
                &read
            ),
            WriteAssessment::Reject(_)
        ));
    }
}
