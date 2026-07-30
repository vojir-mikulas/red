//! Rendering documents, inferred schemas, and plans as the text the model reads.
//!
//! Pure over already-fetched values. A discovered schema is reported with its
//! per-type frequencies intact rather than collapsed to a winner, because a
//! field that is a string here and an int there is the finding, not noise to
//! smooth over.

use red_core::doc::{DocPlan, DocSchema, Document, IndexInfo};

/// Render an inferred schema for `describe_collection`.
pub(super) fn fmt_doc_schema(schema: &DocSchema) -> String {
    let mut out = format!("Inferred schema ({} documents sampled):\n", schema.sampled);
    for f in &schema.fields {
        out.push_str(&format!("  {} — {}\n", f.path, fmt_doc_types(f)));
    }
    out
}
/// Render an inferred schema as a profile (emphasizing drift + presence).
pub(super) fn fmt_doc_profile(schema: &DocSchema) -> String {
    let mut out = format!("Field profile ({} documents sampled):\n", schema.sampled);
    for f in &schema.fields {
        out.push_str(&format!(
            "  {} — {} · present {:.0}%\n",
            f.path,
            fmt_doc_types(f),
            f.present_ratio * 100.0
        ));
    }
    out
}
/// A field's type distribution as `string 82%, int 18%`.
fn fmt_doc_types(f: &red_core::doc::FieldStat) -> String {
    let total: u64 = f.types.iter().map(|(_, c)| c).sum();
    f.types
        .iter()
        .map(|(t, c)| {
            let pct = (c * 100).checked_div(total).unwrap_or(0);
            format!("{} {pct}%", t.label())
        })
        .collect::<Vec<_>>()
        .join(", ")
}
/// Render an index list.
pub(super) fn fmt_doc_indexes(indexes: &[IndexInfo]) -> String {
    if indexes.is_empty() {
        return "  (none)".into();
    }
    indexes
        .iter()
        .map(|ix| {
            let keys = ix
                .keys
                .iter()
                .map(|(f, o)| format!("{f}: {o}"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut props = Vec::new();
            if ix.unique {
                props.push("unique");
            }
            if ix.sparse {
                props.push("sparse");
            }
            if ix.partial {
                props.push("partial");
            }
            let ttl = ix.ttl.map(|t| format!(" ttl={t}s")).unwrap_or_default();
            format!("  {} {{ {keys} }} {}{ttl}", ix.name, props.join(","))
        })
        .collect::<Vec<_>>()
        .join("\n")
}
/// Render a list of documents as one extended-JSON line each.
pub(super) fn fmt_doc_list(docs: &[Document]) -> String {
    if docs.is_empty() {
        return "(no documents)".into();
    }
    docs.iter()
        .map(|d| d.to_doc_value().to_extended_json())
        .collect::<Vec<_>>()
        .join("\n")
}
/// Render an explain plan.
pub(super) fn fmt_doc_plan(plan: &DocPlan) -> String {
    let mut out = String::new();
    if plan.collscan {
        out.push_str("COLLSCAN — no index used\n");
    } else if let Some(ix) = &plan.index_used {
        out.push_str(&format!("uses index {ix}\n"));
    }
    if let (Some(e), Some(r)) = (plan.docs_examined, plan.n_returned) {
        out.push_str(&format!("examined {e}, returned {r}\n"));
    }
    let stages = plan
        .stages
        .iter()
        .map(|s| s.stage.clone())
        .collect::<Vec<_>>()
        .join(" > ");
    out.push_str(&format!("winning plan: {stages}"));
    out
}
