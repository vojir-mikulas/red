//! The two same-connection comparisons: schema structure and table rows.
//!
//! Deliberately same-connection. `AiBackend` holds one driver, so a cross-server
//! comparison would need a second session and is its own feature; both sides
//! here are graded as the same engine, which is the truth and makes a spelling
//! difference a real finding rather than noise.

use std::sync::Arc;
use std::time::Duration;

use red_core::{AiLimits, TableRef};
use red_driver::DatabaseDriver;
use serde_json::Value as Json;

use super::super::util::cap_result_bytes;
use super::format::qualified;

/// Cap on the tables one `diff_schema` describes per side. Each is a catalog
/// round trip, so a schema with a thousand tables is compared by existence past
/// this bound rather than in detail, and the report says so.
const DIFF_SCHEMA_MAX_TABLES: usize = 200;
/// Cap on the differing rows one `diff_data` reports back. The merge-walk itself
/// streams the whole table; this bounds what reaches the model's context.
const DIFF_ROW_REPORT: usize = 200;
/// Compare two schemas' structure inside one connection.
///
/// Deliberately same-connection: `AiBackend` holds one driver, so a cross-server
/// comparison would need a second session and is a different feature. Both sides
/// are graded as the same engine, which is the truth here and makes a spelling
/// difference (`varchar(50)` vs `varchar(100)`) a real finding rather than noise.
pub(in crate::ai) async fn diff_schema(
    driver: &Arc<dyn DatabaseDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    use red_core::schema_diff::{SchemaSnapshot, compare};

    let name = |k: &str| {
        input
            .get(k)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let (Some(left), Some(right)) = (name("left"), name("right")) else {
        return (
            "error: diff_schema needs `left` and `right` schema names".into(),
            false,
        );
    };
    let wanted: Vec<String> = input
        .get("tables")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Json::as_str)
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default();
    let schemas = match driver.list_objects().await {
        Ok(s) => s,
        Err(e) => return (format!("error: {e}"), false),
    };
    let mut truncated = false;
    let mut snapshot = async |want: &str| -> Result<SchemaSnapshot, String> {
        let meta = schemas
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(want))
            .ok_or_else(|| format!("no schema named `{want}` in this connection"))?;
        // Same engine on both sides by construction, so `DbKind` only has to be
        // consistent, not accurate: `compare` reads it to decide cross-engine.
        let mut snap = SchemaSnapshot::from_meta(red_core::DbKind::default(), meta);
        let relations: Vec<&red_core::ObjectMeta> = meta
            .objects
            .iter()
            .filter(|o| o.kind.is_relation())
            .filter(|o| wanted.is_empty() || wanted.contains(&o.name.to_ascii_lowercase()))
            .collect();
        truncated |= relations.len() > DIFF_SCHEMA_MAX_TABLES;
        for obj in relations.into_iter().take(DIFF_SCHEMA_MAX_TABLES) {
            if let Ok(detail) = driver.describe_table(&meta.name, &obj.name).await {
                snap.details.insert(obj.name.clone(), detail);
            }
        }
        Ok(snap)
    };
    let left_snap = match snapshot(left).await {
        Ok(s) => s,
        Err(why) => return (format!("error: {why}"), false),
    };
    let right_snap = match snapshot(right).await {
        Ok(s) => s,
        Err(why) => return (format!("error: {why}"), false),
    };

    let delta = compare(&left_snap, &right_snap);
    let mut out = if delta.is_empty() {
        format!("`{left}` and `{right}` are structurally identical.\n")
    } else {
        format!(
            "{} difference(s) between `{left}` (baseline) and `{right}`:\n",
            delta.count()
        )
    };
    let list = |label: &str, items: &[red_core::ObjectMeta]| {
        if items.is_empty() {
            return String::new();
        }
        let names: Vec<&str> = items.iter().map(|o| o.name.as_str()).collect();
        format!("{label}: {}\n", names.join(", "))
    };
    out.push_str(&list(&format!("Only in `{right}`"), &delta.objects_added));
    out.push_str(&list(&format!("Only in `{left}`"), &delta.objects_removed));
    for t in &delta.tables_changed {
        out.push_str(&format!("\n{}:\n", t.name));
        for c in &t.columns_added {
            out.push_str(&format!("  + column {}\n", c.name));
        }
        for c in &t.columns_removed {
            out.push_str(&format!("  - column {}\n", c.name));
        }
        for c in &t.columns_changed {
            // The uncertain flag is load-bearing: outside the type lattice this is
            // a raw string comparison, and calling it a change without saying so
            // would send the model chasing a spelling difference.
            let note = match c.confidence {
                red_core::schema_diff::Confidence::Certain => "",
                red_core::schema_diff::Confidence::Uncertain => " (may be a spelling difference)",
            };
            out.push_str(&format!(
                "  ~ column {}: {}{note}\n",
                c.left.name, c.summary
            ));
        }
        for i in &t.indexes_added {
            out.push_str(&format!("  + index {}\n", i.name));
        }
        for i in &t.indexes_removed {
            out.push_str(&format!("  - index {}\n", i.name));
        }
        for f in &t.fks_added {
            out.push_str(&format!(
                "  + foreign key {} -> {}.{}\n",
                f.column, f.ref_table, f.ref_column
            ));
        }
        for f in &t.fks_removed {
            out.push_str(&format!(
                "  - foreign key {} -> {}.{}\n",
                f.column, f.ref_table, f.ref_column
            ));
        }
    }
    if truncated {
        out.push_str(&format!(
            "\n(only the first {DIFF_SCHEMA_MAX_TABLES} tables per side were compared in detail; \
             narrow with `tables`)\n"
        ));
    }
    (cap_result_bytes(out, limits.max_result_bytes), true)
}
/// Compare two tables' rows inside one connection, key-ordered and merge-walked.
///
/// Runs the same streaming job the UI's data diff uses, so both tables are read
/// through cursors and never materialized; only the reported differences are
/// bounded, because those are what enter the model's context.
pub(in crate::ai) async fn diff_data(
    driver: &Arc<dyn DatabaseDriver>,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    use std::sync::atomic::AtomicBool;

    let table = |schema_key: &str, table_key: &str| -> Option<TableRef> {
        let name = input
            .get(table_key)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())?;
        Some(TableRef {
            schema: input
                .get(schema_key)
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            name: name.to_string(),
        })
    };
    let (Some(left), Some(right)) = (
        table("left_schema", "left_table"),
        table("right_schema", "right_table"),
    ) else {
        return (
            "error: diff_data needs `left_table` and `right_table`".into(),
            false,
        );
    };
    let key = input
        .get("key")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    // The job reports progress to the UI; there is no toast behind an agent call,
    // so the receiver is dropped and the sends fall on the floor.
    let (events, _rx) = futures::channel::mpsc::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let job = crate::dispatch::jobs::diff_job(
        driver.clone(),
        left.clone(),
        driver.clone(),
        right.clone(),
        key,
        cancel,
        events,
        crate::protocol::OpId::new(0),
    );
    let result = match limits.statement_timeout_ms {
        0 => job.await,
        ms => match tokio::time::timeout(Duration::from_millis(ms), job).await {
            Ok(r) => r,
            Err(_) => {
                return (
                    "error: the diff exceeded the agent's statement timeout. It reads both tables \
                     whole, so compare smaller tables or do it from the UI's data diff, which can \
                     run long."
                        .into(),
                    false,
                );
            }
        },
    };
    let (plan, acc) = match result {
        Ok(pair) => pair,
        Err(e) => return (format!("error: {e}"), false),
    };
    let summary = &acc.summary;
    let l = qualified(left.schema.as_deref(), &left.name);
    let r = qualified(right.schema.as_deref(), &right.name);
    let mut out = format!(
        "Compared {l} (baseline) against {r} on `{}`:\n  {} identical, {} changed, {} only in {l}, \
         {} only in {r}\n",
        plan.key, summary.unchanged, summary.changed, summary.removed, summary.added,
    );
    if !plan.left_only.is_empty() || !plan.right_only.is_empty() {
        out.push_str(&format!(
            "  columns compared: {}; only in {l}: {}; only in {r}: {}\n",
            plan.columns.join(", "),
            if plan.left_only.is_empty() {
                "none".into()
            } else {
                plan.left_only.join(", ")
            },
            if plan.right_only.is_empty() {
                "none".into()
            } else {
                plan.right_only.join(", ")
            },
        ));
    }
    if !acc.rows.is_empty() {
        out.push_str("\nDifferences:\n");
        for row in acc.rows.iter().take(DIFF_ROW_REPORT) {
            let what = match row.kind {
                red_core::diff::DiffKind::Added => format!("only in {r}"),
                red_core::diff::DiffKind::Removed => format!("only in {l}"),
                red_core::diff::DiffKind::Changed => {
                    let cols: Vec<&str> = row
                        .changed
                        .iter()
                        .enumerate()
                        .filter(|(_, differs)| **differs)
                        .filter_map(|(i, _)| plan.columns.get(i).map(String::as_str))
                        .collect();
                    format!("differs in {}", cols.join(", "))
                }
            };
            out.push_str(&format!("  {} — {what}\n", row.key));
        }
        if acc.rows.len() > DIFF_ROW_REPORT || acc.truncated {
            out.push_str(
                "  …(more differing rows than are shown; the counts above are complete)\n",
            );
        }
    }
    (cap_result_bytes(out, limits.max_result_bytes), true)
}
