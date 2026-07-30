//! `export_result`: handing the user a file rather than an answer.
//!
//! One tool name across all three seams, because "write this out for me" is the
//! same request everywhere, but three bodies, because a query, a keyspace, and a
//! collection do not take the same arguments. The shared parts are the ones that
//! must not diverge: where the file may be written ([`export_path`], which is
//! what keeps a model-supplied name inside the app's own folder) and how a field
//! is escaped.
//!
//! The SQL body streams through the driver's own exporter and is deliberately
//! **not** row-capped -- the rows go to disk, not into the model's context. The
//! other two page from here, so they stop at [`EXPORT_ITEM_MAX`] and say so.

use std::path::PathBuf;
use std::sync::Arc;

use red_ai::ToolDef;
use red_core::doc::DocValue;
use red_driver::{AbortSignal, DatabaseDriver, DocDriver, KvDriver};
use serde_json::{Value as Json, json};

use super::doc::write::doc_arg_value;
use super::gate::is_read_only_select;
use super::kv::format::{fmt_kv_value, kv_ttl};
use super::kv::tools::kv_collect_keys;
use super::state::ReportSink;
use red_core::sql::Dialect;

/// The `export_result` tool definition. One name across all three seams, since
/// "write this out to a file for me" is the same request everywhere, but each
/// passes its own `description` and arguments: SQL exports a query, Redis a set
/// of keys, MongoDB a collection, and pretending those take the same parameters
/// would produce a schema nobody could call.
pub(in crate::ai) fn export_tool_def(
    description: &str,
    properties: Json,
    required: &[&str],
) -> ToolDef {
    ToolDef {
        name: "export_result".into(),
        description: description.into(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    }
}
/// Resolve a model-supplied export name to a path inside the assistant's own
/// output folder.
///
/// Only the *stem* is taken, sanitized to `[A-Za-z0-9._-]` and length-capped,
/// then suffixed with a fresh UUID. A tool argument therefore cannot escape the
/// folder (no `..`, no absolute path, no separator survives), cannot clobber an
/// existing file, and cannot choose the extension — the format decides that.
fn export_path(sink: &ReportSink, name: Option<&str>, ext: &str) -> PathBuf {
    let stem: String = name
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    let stem = stem.trim_matches(['-', '.']).to_string();
    let label = if stem.is_empty() {
        String::new()
    } else {
        format!("{stem}-")
    };
    sink.output_dir().join(format!(
        "red-export-{label}{}.{ext}",
        uuid::Uuid::new_v4().simple()
    ))
}
/// Parse the `format` argument of a SQL `export_result`.
fn export_format(input: &Json) -> Result<(red_core::ExportFormat, &'static str), String> {
    match input.get("format").and_then(Json::as_str).unwrap_or("csv") {
        "csv" => Ok((red_core::ExportFormat::Csv, "csv")),
        "json" => Ok((red_core::ExportFormat::Json, "json")),
        "sql" => Ok((red_core::ExportFormat::Sql, "sql")),
        "html" => Ok((red_core::ExportFormat::Html, "html")),
        other => Err(format!(
            "export format must be csv/json/sql/html, not `{other}`"
        )),
    }
}
/// Stream a read-only query's whole result to a file for the user.
///
/// Unlike `run_select` this is **not** row-capped: the rows go to disk, not into
/// the model's context, and the driver's export streams row by row without ever
/// materializing the result. The read gate still applies — an export is a read,
/// and `is_read_only_select` is what makes that true.
pub(in crate::ai) async fn export_result(
    driver: &Arc<dyn DatabaseDriver>,
    dialect: Dialect,
    input: &Json,
    sink: &ReportSink,
) -> (String, bool) {
    use std::sync::atomic::AtomicBool;

    let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
    if !is_read_only_select(sql, dialect) {
        return (
            "error: export_result runs a single SELECT or WITH...SELECT query; anything else is \
             rejected"
                .into(),
            false,
        );
    }
    let (format, ext) = match export_format(input) {
        Ok(f) => f,
        Err(why) => return (format!("error: {why}"), false),
    };
    let path = export_path(sink, input.get("name").and_then(Json::as_str), ext);
    // The driver's export reports progress on a channel and honours a cancel flag;
    // neither has a job here (there is no toast to update and no Cancel button),
    // so the flag stays clear and the receiver is dropped immediately.
    let (progress, _rx) = tokio::sync::mpsc::unbounded_channel();
    match driver
        .export(
            sql,
            &path,
            format,
            Arc::new(AtomicBool::new(false)),
            progress,
        )
        .await
    {
        Ok(rows) => {
            sink.announce(&path, Some(&format!("Export ({rows} rows)")));
            (
                format!(
                    "Wrote {rows} row(s) to {}. It is now a card in the chat the user can open.",
                    path.display()
                ),
                true,
            )
        }
        Err(e) => (format!("error: the export failed: {e}"), false),
    }
}
/// Ceiling on the keys / documents one non-SQL `export_result` writes. The SQL
/// seam streams through the driver's own exporter and needs no bound; these two
/// walk the keyspace/collection from here, so they stop at a stated number
/// rather than running for an unbounded time.
const EXPORT_ITEM_MAX: usize = 50_000;
/// Documents fetched per keyset window while exporting a collection.
const EXPORT_DOC_WINDOW: usize = 1_000;
/// Write matching keys and their values to a file for the user.
///
/// Values are read and written key by key, so the file grows incrementally and
/// no whole-keyspace snapshot is ever held. The key *list* is the one bounded
/// materialization, and its bound is reported.
pub(in crate::ai) async fn kv_export(
    driver: &Arc<dyn KvDriver>,
    input: &Json,
    sink: &ReportSink,
) -> (String, bool) {
    use std::io::Write;

    let pattern = input
        .get("pattern")
        .and_then(Json::as_str)
        .filter(|p| !p.is_empty());
    let as_csv = match input.get("format").and_then(Json::as_str).unwrap_or("json") {
        "json" => false,
        "csv" => true,
        other => {
            return (
                format!("error: export format must be csv or json, not `{other}`"),
                false,
            );
        }
    };
    let (keys, exhausted) = match kv_collect_keys(driver, pattern, EXPORT_ITEM_MAX).await {
        Ok(k) => k,
        Err(e) => return (format!("error: {e}"), false),
    };
    if keys.is_empty() {
        return (
            "No keys matched, so nothing was exported.".to_string(),
            true,
        );
    }
    let path = export_path(
        sink,
        input.get("name").and_then(Json::as_str),
        if as_csv { "csv" } else { "json" },
    );
    let mut file = match std::fs::File::create(&path) {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            return (
                format!("error: could not create the export file: {e}"),
                false,
            );
        }
    };
    let mut write = |line: &str| file.write_all(line.as_bytes());
    let result = (|| -> std::io::Result<()> {
        if as_csv {
            write("key,type,ttl,value\n")?;
        } else {
            write("[\n")?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        return (format!("error: writing the export failed: {e}"), false);
    }
    let mut written = 0usize;
    for (i, meta) in keys.iter().enumerate() {
        let value = driver
            .read_value(&meta.key)
            .await
            .ok()
            .flatten()
            .map(|v| fmt_kv_value(&v))
            .unwrap_or_default();
        let ttl = kv_ttl(meta.ttl);
        let line = if as_csv {
            format!(
                "{},{},{},{}\n",
                csv_field(&meta.key),
                meta.kv_type.label(),
                ttl,
                csv_field(&value),
            )
        } else {
            format!(
                "  {{\"key\":{},\"type\":{},\"ttl\":{},\"value\":{}}}{}\n",
                json_str(&meta.key),
                json_str(meta.kv_type.label()),
                json_str(&ttl),
                json_str(&value),
                if i + 1 == keys.len() { "" } else { "," },
            )
        };
        if let Err(e) = file.write_all(line.as_bytes()) {
            return (
                format!("error: writing the export failed after {written} key(s): {e}"),
                false,
            );
        }
        written += 1;
    }
    if !as_csv && let Err(e) = file.write_all(b"]\n") {
        return (format!("error: writing the export failed: {e}"), false);
    }
    if let Err(e) = file.flush() {
        return (format!("error: flushing the export failed: {e}"), false);
    }
    let note = if exhausted {
        String::new()
    } else {
        format!(" (stopped at the {EXPORT_ITEM_MAX}-key bound; narrow the pattern for the rest)")
    };
    sink.announce(&path, Some(&format!("Export ({written} keys)")));
    (
        format!(
            "Wrote {written} key(s) to {}{note}. It is now a card in the chat the user can open.",
            path.display()
        ),
        true,
    )
}
/// Write matching documents to a JSON array file for the user.
///
/// Paged by `_id` keyset (`find_seek`), one window at a time and appended as it
/// goes, so an export of a large collection never holds more than a window.
pub(in crate::ai) async fn doc_export(
    driver: &Arc<dyn DocDriver>,
    input: &Json,
    sink: &ReportSink,
) -> (String, bool) {
    use std::io::Write;

    let db = input.get("db").and_then(Json::as_str).unwrap_or("");
    let coll = input.get("coll").and_then(Json::as_str).unwrap_or("");
    if db.is_empty() || coll.is_empty() {
        return ("error: export_result needs `db` and `coll`".into(), false);
    }
    let filter = match doc_arg_value(driver, input, "filter") {
        Ok(f) => f,
        Err(e) => return (format!("error: {e}"), false),
    };
    let path = export_path(sink, input.get("name").and_then(Json::as_str), "json");
    let mut file = match std::fs::File::create(&path) {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            return (
                format!("error: could not create the export file: {e}"),
                false,
            );
        }
    };
    let abort = AbortSignal::new();
    let mut after: Option<DocValue> = None;
    let mut written = 0usize;
    let mut truncated = false;
    if let Err(e) = file.write_all(b"[\n") {
        return (format!("error: writing the export failed: {e}"), false);
    }
    loop {
        let window = match driver
            .find_seek(
                db,
                coll,
                filter.as_ref(),
                red_core::doc::DocSeek::Forward {
                    after: after.clone(),
                },
                EXPORT_DOC_WINDOW.min(EXPORT_ITEM_MAX - written),
                &abort,
            )
            .await
        {
            Ok(w) => w,
            Err(e) => {
                return (
                    format!("error: the export failed after {written} document(s): {e}"),
                    false,
                );
            }
        };
        if window.is_empty() {
            break;
        }
        for doc in &window {
            let sep = if written == 0 { "  " } else { ",\n  " };
            let line = format!("{sep}{}", doc.to_doc_value().to_extended_json());
            if let Err(e) = file.write_all(line.as_bytes()) {
                return (
                    format!("error: writing the export failed after {written} document(s): {e}"),
                    false,
                );
            }
            written += 1;
        }
        after = window.last().map(|d| d.id.clone());
        if written >= EXPORT_ITEM_MAX {
            truncated = true;
            break;
        }
    }
    if let Err(e) = file.write_all(b"\n]\n").and_then(|()| file.flush()) {
        return (format!("error: writing the export failed: {e}"), false);
    }
    let note = if truncated {
        format!(
            " (stopped at the {EXPORT_ITEM_MAX}-document bound; narrow the filter for the rest)"
        )
    } else {
        String::new()
    };
    sink.announce(&path, Some(&format!("Export ({written} documents)")));
    (
        format!(
            "Wrote {written} document(s) to {}{note}. It is now a card in the chat the user can \
             open.",
            path.display()
        ),
        true,
    )
}
/// One CSV field: quoted and doubled-up when it carries a comma, quote, or
/// newline, per RFC 4180.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}
/// One JSON string literal, escaped by `serde_json` so the export is parseable
/// whatever a Redis value happens to contain.
fn json_str(s: &str) -> String {
    Json::String(s.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The export path is model-supplied, so it is the one place a tool argument
    /// could reach outside the app's own folder. Assert it cannot.
    #[test]
    fn export_paths_stay_inside_the_output_folder() {
        let sink = ReportSink::disabled();
        let dir = sink.output_dir();
        for name in [
            "../../etc/passwd",
            "/etc/passwd",
            "..\\..\\windows\\system32",
            "a/b/c",
            "orders",
            "",
        ] {
            let path = export_path(&sink, Some(name), "csv");
            assert_eq!(path.parent(), Some(dir.as_path()), "escaped for `{name}`");
            let file = path.file_name().unwrap().to_string_lossy().to_string();
            assert!(file.starts_with("red-export-"), "{file}");
            assert!(file.ends_with(".csv"), "{file}");
            assert!(!file.contains(".."), "{file}");
        }
        // Two calls never collide, so an export cannot clobber an earlier one.
        assert_ne!(
            export_path(&sink, Some("x"), "csv"),
            export_path(&sink, Some("x"), "csv")
        );
    }
}
