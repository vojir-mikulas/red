//! Streamed export of a document collection to disk, the document-store arm of the
//! SQL `DatabaseDriver::export` and the Redis `kvexport` path. A free function over
//! the [`DocDriver`] seam rather than a trait method: it needs nothing engine
//! specific beyond `find_seek`, and every document store that implements the seam
//! gets the same four formats for free.
//!
//! Reads through `_id`-keyset windows ([`DocSeek::Forward`]), so an export of a
//! 50M-document collection holds one window at a time -- the same streaming
//! invariant the browse grid keeps, and the reason this is not the AI tool's capped
//! `doc_export`.
//!
//! The JSON pair writes extended JSON (every BSON type survives); the tabular pair
//! flattens onto a column list sampled from the collection's schema and reports, as
//! an [`ExportShortfall`], any document that carried a field the sample never saw.

use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use red_core::doc::{
    DocExportFormat, DocSeek, DocValue, Document, Filter, has_unmapped_fields, tabular_columns,
};
use red_core::{ExportFormat, ExportOutcome, ExportShortfall, RedError, Result};
use tokio::sync::mpsc::UnboundedSender;

use crate::format::{ExportWriter, ProgressThrottle};
use crate::{AbortSignal, DocDriver};

/// Documents pulled per keyset window. Matches the browse grid's page order of
/// magnitude: big enough that the round trips don't dominate, small enough that one
/// window is a rounding error in memory.
const EXPORT_WINDOW: usize = 500;

/// Documents sampled to derive a tabular export's columns. The same order as the
/// schema panel's sample: enough to catch the shape of a heterogeneous collection
/// without paying for a scan.
const COLUMN_SAMPLE: usize = 1_000;

/// Ceiling on a tabular export's column count, so a collection with pathological
/// per-document keys cannot produce a million-column sheet.
const MAX_COLUMNS: usize = 512;

/// What to export and how. `filter` narrows the documents exactly as the browse
/// grid's filter does; `columns` overrides the sampled column list for a tabular
/// format (empty = sample it here).
pub struct DocExportRequest {
    pub format: DocExportFormat,
    pub filter: Option<Filter>,
    pub columns: Vec<String>,
}

/// Stream every document of `db.coll` matching the request's filter into `dest`.
///
/// Cancellation is checked per window and per row: on cancel the partial file is
/// removed and [`RedError::Interrupted`] returned, so a cancelled export never
/// leaves a truncated file that looks complete.
///
/// # Errors
/// Propagates the driver's read errors and any IO failure writing `dest`.
pub async fn run_doc_export(
    driver: &Arc<dyn DocDriver>,
    db: &str,
    coll: &str,
    dest: &Path,
    req: DocExportRequest,
    cancel: &AtomicBool,
    progress: UnboundedSender<u64>,
) -> Result<ExportOutcome> {
    let abort = AbortSignal::new();
    let columns = if req.format.is_tabular() && req.columns.is_empty() {
        let schema = driver.infer_schema(db, coll, COLUMN_SAMPLE, &abort).await?;
        tabular_columns(&schema, MAX_COLUMNS)
    } else {
        req.columns
    };

    let file = std::fs::File::create(dest).map_err(io_err)?;
    let mut sink = Sink::begin(BufWriter::new(file), req.format, columns, dest)?;
    let mut throttle = ProgressThrottle::new(progress);
    let mut after: Option<DocValue> = None;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return cancelled(sink, dest);
        }
        let window = driver
            .find_seek(
                db,
                coll,
                req.filter.as_ref(),
                None,
                DocSeek::Forward {
                    after: after.clone(),
                },
                EXPORT_WINDOW,
                &abort,
            )
            .await?;
        if window.is_empty() {
            break;
        }
        for doc in &window {
            if cancel.load(Ordering::Relaxed) {
                return cancelled(sink, dest);
            }
            sink.write(doc)?;
            throttle.tick(sink.written());
        }
        // The next window seeks strictly past the last `_id` of this one, so the
        // scan never revisits a document and never depends on a stable offset.
        after = window.last().map(|d| d.id.clone());
    }
    sink.finish()
}

/// Drop the writer, remove the partial file, and report the interruption.
fn cancelled<W: Write>(sink: Sink<W>, dest: &Path) -> Result<ExportOutcome> {
    drop(sink);
    let _ = std::fs::remove_file(dest);
    Err(RedError::Interrupted)
}

/// The per-format writing half. The tabular arm delegates to the shared
/// [`ExportWriter`] so a Mongo CSV is byte-identical to a SQL one; the JSON arms
/// write extended JSON themselves, because a document is not a row and flattening
/// it into one would cost exactly the types the format exists to keep.
enum Sink<W: Write> {
    /// A JSON array of extended-JSON documents.
    Json { out: W, written: u64 },
    /// One extended-JSON document per line.
    Ndjson { out: W, written: u64 },
    Tabular {
        writer: ExportWriter<W>,
        columns: Vec<String>,
        /// Documents that carried a field outside `columns`.
        unmapped: u64,
    },
}

impl<W: Write> Sink<W> {
    fn begin(
        mut out: W,
        format: DocExportFormat,
        columns: Vec<String>,
        dest: &Path,
    ) -> Result<Self> {
        match format {
            DocExportFormat::Json => {
                out.write_all(b"[").map_err(io_err)?;
                Ok(Sink::Json { out, written: 0 })
            }
            DocExportFormat::Ndjson => Ok(Sink::Ndjson { out, written: 0 }),
            DocExportFormat::Csv | DocExportFormat::Xlsx => {
                let wire = if format == DocExportFormat::Csv {
                    ExportFormat::Csv
                } else {
                    ExportFormat::Xlsx
                };
                let writer =
                    ExportWriter::begin(out, wire, columns.clone(), dest).map_err(io_err)?;
                Ok(Sink::Tabular {
                    writer,
                    columns,
                    unmapped: 0,
                })
            }
        }
    }

    fn write(&mut self, doc: &Document) -> Result<()> {
        match self {
            Sink::Json { out, written } => {
                let sep: &[u8] = if *written == 0 { b"\n  " } else { b",\n  " };
                out.write_all(sep).map_err(io_err)?;
                out.write_all(doc.to_doc_value().to_extended_json().as_bytes())
                    .map_err(io_err)?;
                *written += 1;
            }
            Sink::Ndjson { out, written } => {
                writeln!(out, "{}", doc.to_doc_value().to_extended_json()).map_err(io_err)?;
                *written += 1;
            }
            Sink::Tabular {
                writer,
                columns,
                unmapped,
            } => {
                // Full fidelity, not the grid's display cap: an exported cell is
                // data, and a truncated one is silent loss.
                let cells: Vec<red_core::Value> = columns
                    .iter()
                    .map(|path| {
                        doc.value_at(path)
                            .map_or(red_core::Value::Null, |v| v.to_cell(usize::MAX))
                    })
                    .collect();
                if has_unmapped_fields(doc, columns) {
                    *unmapped += 1;
                }
                writer.write_row(&cells).map_err(io_err)?;
            }
        }
        Ok(())
    }

    fn written(&self) -> u64 {
        match self {
            Sink::Json { written, .. } | Sink::Ndjson { written, .. } => *written,
            Sink::Tabular { writer, .. } => writer.written(),
        }
    }

    fn finish(self) -> Result<ExportOutcome> {
        match self {
            Sink::Json { mut out, written } => {
                let tail: &[u8] = if written == 0 { b"]\n" } else { b"\n]\n" };
                out.write_all(tail).map_err(io_err)?;
                out.flush().map_err(io_err)?;
                Ok(ExportOutcome::complete(written))
            }
            Sink::Ndjson { mut out, written } => {
                out.flush().map_err(io_err)?;
                Ok(ExportOutcome::complete(written))
            }
            Sink::Tabular {
                writer, unmapped, ..
            } => {
                let mut outcome = writer.finish().map_err(io_err)?;
                // A format row limit is the worse shortfall (rows are missing, not
                // just fields), so it keeps the slot when both apply.
                if outcome.shortfall.is_none() && unmapped > 0 {
                    outcome.shortfall = Some(ExportShortfall::UnmappedFields(unmapped));
                }
                Ok(outcome)
            }
        }
    }
}

fn io_err(e: std::io::Error) -> RedError {
    RedError::Driver(format!("export failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_core::doc::DocValue;

    fn doc(id: i32, fields: Vec<(&str, DocValue)>) -> Document {
        Document {
            id: DocValue::Int32(id),
            fields: fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    /// Write `docs` through a sink of `format` and hand back the file's text.
    fn render(format: DocExportFormat, columns: Vec<String>, docs: &[Document]) -> String {
        let dest = std::env::temp_dir().join(format!("red-docexport-{}", format.extension()));
        let mut sink = Sink::begin(Vec::new(), format, columns, &dest).unwrap();
        for d in docs {
            sink.write(d).unwrap();
        }
        // Only the in-memory arms are exercised here; XLSX spools to disk and is
        // covered by the sheet's own tests.
        let written = sink.written();
        let text = match sink {
            Sink::Json { mut out, .. } => {
                out.write_all(if written == 0 { b"]\n" } else { b"\n]\n" })
                    .unwrap();
                out
            }
            Sink::Ndjson { out, .. } => out,
            Sink::Tabular { writer, .. } => {
                writer.finish().unwrap();
                Vec::new()
            }
        };
        String::from_utf8(text).unwrap()
    }

    #[test]
    fn json_array_frames_documents_as_extended_json() {
        let docs = vec![
            doc(1, vec![("name", DocValue::Str("Ada".into()))]),
            doc(2, vec![("name", DocValue::Str("Grace".into()))]),
        ];
        assert_eq!(
            render(DocExportFormat::Json, Vec::new(), &docs),
            "[\n  {\"_id\":1,\"name\":\"Ada\"},\n  {\"_id\":2,\"name\":\"Grace\"}\n]\n"
        );
        // An empty export is still a valid, closed array.
        assert_eq!(render(DocExportFormat::Json, Vec::new(), &[]), "[]\n");
    }

    #[test]
    fn ndjson_writes_one_document_per_line() {
        let docs = vec![doc(1, vec![]), doc(2, vec![])];
        assert_eq!(
            render(DocExportFormat::Ndjson, Vec::new(), &docs),
            "{\"_id\":1}\n{\"_id\":2}\n"
        );
    }

    #[test]
    fn tabular_projects_paths_and_counts_unmapped_documents() {
        let columns = vec!["_id".to_string(), "user.city".to_string()];
        let docs = vec![
            doc(
                1,
                vec![(
                    "user",
                    DocValue::Document(vec![("city".into(), DocValue::Str("London".into()))]),
                )],
            ),
            // `extra` is outside the column list: written rows lose it, and the
            // export must say so rather than report a clean run.
            doc(2, vec![("extra", DocValue::Int32(9))]),
        ];
        let dest = std::env::temp_dir().join("red-docexport-test.csv");
        let mut sink = Sink::begin(Vec::new(), DocExportFormat::Csv, columns, &dest).unwrap();
        for d in &docs {
            sink.write(d).unwrap();
        }
        let outcome = sink.finish().unwrap();
        assert_eq!(outcome.rows, 2);
        assert_eq!(
            outcome.shortfall,
            Some(ExportShortfall::UnmappedFields(1)),
            "the document with an unsampled field must be reported"
        );
    }
}
