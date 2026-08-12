//! Engine-agnostic value/string formatters shared by the network drivers'
//! `export` path. These are pure [`Value`] → text functions with zero engine
//! knowledge, lifted here so every driver writes byte-identical CSV/JSON and a
//! new driver doesn't fork yet another copy.
//!
//! Blobs export as a `<N bytes>` length marker, not their bytes (hex/base64); a
//! deliberate v0.1 choice: the streaming export path never materializes cell bytes,
//! and a text CSV/JSON of raw binary is rarely what a user wants. Binary-faithful
//! export is a later format option.

use std::borrow::Cow;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use red_core::valuefmt::{
    csv_cell, csv_record, html_cell, html_escape, json_string, json_value, sql_ident, sql_value,
};
use red_core::{ExportFormat, ExportOutcome, ExportShortfall, Value};
use tokio::sync::mpsc::UnboundedSender;

/// Strip surrounding whitespace and a single trailing `;` so a user statement can
/// be wrapped in `SELECT * FROM (<sql>) AS _red` for paging/count/export.
///
/// A statement whose last line carries a line comment (`SELECT 1 -- note`, an
/// ordinary editor habit) gets a newline appended: without it the wrapper's own
/// `)` lands *inside* that comment, the engine sees an unbalanced paren, and
/// count, paging and export all fail for the whole query.
///
/// The check is deliberately loose — any `--` or `#` on the last line, string
/// literal or not. A false positive appends a newline that changes nothing; a
/// false negative breaks the query.
pub(crate) fn strip_trailing(sql: &str) -> Cow<'_, str> {
    let trimmed = sql.trim().strip_suffix(';').unwrap_or(sql.trim()).trim();
    let last_line = trimmed.rsplit('\n').next().unwrap_or(trimmed);
    if last_line.contains("--") || last_line.contains('#') {
        Cow::Owned(format!("{trimmed}\n"))
    } else {
        Cow::Borrowed(trimmed)
    }
}

/// Rows between throttled progress emits (also bounded by [`PROGRESS_INTERVAL`]).
const PROGRESS_ROWS: u64 = 1_000;
/// Min wall-clock between progress emits, so a fast export doesn't flood the
/// channel and a slow one still reports steadily.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// Coalesces an export's per-row count into throttled progress sends: at most one
/// every [`PROGRESS_ROWS`] rows or [`PROGRESS_INTERVAL`], whichever comes first.
/// Shared by every driver's export loop so they report identically.
pub(crate) struct ProgressThrottle {
    sender: UnboundedSender<u64>,
    last_sent: u64,
    last_at: Instant,
}

impl ProgressThrottle {
    pub(crate) fn new(sender: UnboundedSender<u64>) -> Self {
        Self {
            sender,
            last_sent: 0,
            last_at: Instant::now(),
        }
    }

    /// Maybe emit `written` (the running row count). A failed send (UI gone) is
    /// ignored; progress is best-effort.
    pub(crate) fn tick(&mut self, written: u64) {
        if written.saturating_sub(self.last_sent) >= PROGRESS_ROWS
            || self.last_at.elapsed() >= PROGRESS_INTERVAL
        {
            let _ = self.sender.send(written);
            self.last_sent = written;
            self.last_at = Instant::now();
        }
    }
}

/// The shared CSV/JSON framing for every driver's `export`: header/opening token,
/// per-row escaping + comma separation + JSON object framing, and the closing
/// token. Each driver keeps its own row pump (sync `rusqlite` vs. async streams)
/// and cancel check, but drives this writer so the on-disk format is byte-identical
/// and the easy-to-drift framing lives in one place.
pub(crate) struct ExportWriter<W: Write> {
    out: W,
    /// The per-format framing state. An enum rather than an `ExportFormat` plus
    /// a bag of optional fields, so "XLSX without a sheet" cannot be built.
    framing: Framing,
    names: Vec<String>,
    written: u64,
}

/// What a format needs to carry between rows. Most carry nothing; SQL carries
/// its target table, and XLSX carries the sheet being spooled.
enum Framing {
    Csv,
    Json,
    Html,
    /// Target table name for the `INSERT` statements, from the destination stem.
    Sql(String),
    Xlsx(crate::xlsx::XlsxSheet),
}

impl<W: Write> ExportWriter<W> {
    /// Begin an export: write the CSV header row, the opening JSON `[`, or the HTML
    /// document head + table header. `table` names the target for SQL `INSERT`
    /// exports and is ignored by every other format.
    pub(crate) fn begin(
        mut out: W,
        format: ExportFormat,
        names: Vec<String>,
        dest: &Path,
    ) -> io::Result<Self> {
        let framing = match format {
            ExportFormat::Csv => {
                writeln!(out, "{}", csv_record(names.iter().map(String::as_str)))?;
                Framing::Csv
            }
            ExportFormat::Json => {
                write!(out, "[")?;
                Framing::Json
            }
            ExportFormat::Html => {
                // A streamed grid export carries no model-supplied title; the
                // generate_report tool renders titled reports via `render_html_report`.
                write!(out, "{}", html_head(None))?;
                write!(out, "{}", html_thead(&names))?;
                Framing::Html
            }
            // The INSERT stream needs no preamble; each row is a standalone
            // statement carrying the table name and column list.
            ExportFormat::Sql => Framing::Sql(sql_table_name(dest)),
            // Nothing reaches `out` until `finish`: the sheet spools to disk and
            // the archive is assembled once every entry's size is known.
            ExportFormat::Xlsx => Framing::Xlsx(crate::xlsx::XlsxSheet::begin(dest, &names)?),
        };
        Ok(Self {
            out,
            framing,
            names,
            written: 0,
        })
    }

    /// Write one row (cells positionally aligned with the column names): CSV
    /// escaping for CSV, object framing + comma separation for JSON.
    pub(crate) fn write_row(&mut self, cells: &[Value]) -> io::Result<()> {
        match &mut self.framing {
            Framing::Csv => {
                let fields: Vec<String> = cells.iter().map(csv_cell).collect();
                writeln!(
                    self.out,
                    "{}",
                    csv_record(fields.iter().map(String::as_str))
                )?;
            }
            Framing::Json => {
                if self.written > 0 {
                    write!(self.out, ",")?;
                }
                write!(self.out, "\n  {{")?;
                for (i, value) in cells.iter().enumerate() {
                    if i > 0 {
                        write!(self.out, ",")?;
                    }
                    // A row wider than the header falls back to an empty key name.
                    let name = self.names.get(i).map(String::as_str).unwrap_or("");
                    write!(self.out, "{}:{}", json_string(name), json_value(value))?;
                }
                write!(self.out, "}}")?;
            }
            Framing::Html => {
                write!(self.out, "<tr>")?;
                for value in cells {
                    write!(self.out, "<td>{}</td>", html_cell(value))?;
                }
                writeln!(self.out, "</tr>")?;
            }
            Framing::Sql(table) => {
                write!(self.out, "INSERT INTO {} (", sql_ident(table))?;
                for (i, name) in self.names.iter().enumerate() {
                    if i > 0 {
                        write!(self.out, ", ")?;
                    }
                    write!(self.out, "{}", sql_ident(name))?;
                }
                write!(self.out, ") VALUES (")?;
                for (i, value) in cells.iter().enumerate() {
                    if i > 0 {
                        write!(self.out, ", ")?;
                    }
                    write!(self.out, "{}", sql_value(value))?;
                }
                writeln!(self.out, ");")?;
            }
            Framing::Xlsx(sheet) => {
                sheet.write_row(cells)?;
                // A row past Excel's limit is not written, so it is not counted:
                // the reported total must be what the file actually holds.
                if sheet.truncated() {
                    return Ok(());
                }
            }
        }
        self.written += 1;
        Ok(())
    }

    /// Close the export: JSON gets its trailing `]`, HTML closes the table + a row-
    /// count footer + the document; CSV needs no footer. Flush, and report what was
    /// written -- including a format limit that stopped it short.
    pub(crate) fn finish(mut self) -> io::Result<ExportOutcome> {
        let mut shortfall = None;
        match self.framing {
            Framing::Json => write!(self.out, "\n]\n")?,
            Framing::Html => write!(self.out, "{}", html_foot(self.written))?,
            Framing::Csv | Framing::Sql(_) => {}
            // The archive is written here, in one pass, now that the sheet's
            // checksum and length are known.
            Framing::Xlsx(sheet) => {
                if sheet.truncated() {
                    shortfall = Some(ExportShortfall::RowLimit);
                }
                sheet.finish(&mut self.out)?;
            }
        }
        self.out.flush()?;
        Ok(ExportOutcome {
            rows: self.written,
            shortfall,
        })
    }

    /// Rows written so far; feeds the progress throttle.
    pub(crate) fn written(&self) -> u64 {
        self.written
    }
}

/// The HTML report's inline stylesheet: a self-contained, themed shell (light/dark
/// via `prefers-color-scheme`, sticky header, zebra rows). No external assets, so a
/// report opens anywhere offline.
const HTML_STYLE: &str = concat!(
    "<style>",
    ":root{color-scheme:light dark}",
    "*{box-sizing:border-box}",
    "body{margin:0;font:14px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;",
    "background:#fff;color:#1a1a1a}",
    "main{max-width:1200px;margin:0 auto;padding:32px 24px}",
    "h1{font-size:20px;font-weight:600;margin:0 0 16px}",
    ".meta{margin:12px 2px;color:#6b7280;font-size:12px}",
    ".table-wrap{overflow:auto;border:1px solid #e5e7eb;border-radius:8px}",
    "table{border-collapse:collapse;width:100%;font-variant-numeric:tabular-nums}",
    "th,td{padding:7px 12px;text-align:left;border-bottom:1px solid #eceef1;",
    "white-space:nowrap;max-width:480px;overflow:hidden;text-overflow:ellipsis}",
    "th{position:sticky;top:0;background:#f6f7f9;font-weight:600;border-bottom:1px solid #e5e7eb}",
    "tbody tr:nth-child(even){background:#fafbfc}",
    "tbody tr:hover{background:#f0f4ff}",
    ".null{color:#9aa3af;font-style:italic}",
    "@media(prefers-color-scheme:dark){",
    "body{background:#0f1115;color:#e6e6e6}",
    ".meta{color:#8b93a1}.table-wrap{border-color:#262a31}",
    "th,td{border-bottom-color:#1c2128}",
    "th{background:#161a20;border-bottom-color:#262a31}",
    "tbody tr:nth-child(even){background:#13161b}",
    "tbody tr:hover{background:#1b2130}.null{color:#6b7280}}",
    "</style>",
);

/// The default report heading when the caller (or model) supplies no title.
const DEFAULT_REPORT_TITLE: &str = "RED — query report";

/// The HTML report's document head up to the opening `<table>`: the doctype, the
/// inline style, and the `<h1>` heading. `title` sets both the browser `<title>` and
/// the visible heading (escaped); `None` uses [`DEFAULT_REPORT_TITLE`].
fn html_head(title: Option<&str>) -> String {
    let title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(DEFAULT_REPORT_TITLE);
    let t = html_escape(title);
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{t}</title>{HTML_STYLE}</head><body><main><h1>{t}</h1>\
         <div class=\"table-wrap\"><table>"
    )
}

/// The `<thead>` row for the report's columns (escaped names).
fn html_thead(names: &[String]) -> String {
    let mut s = String::from("<thead><tr>");
    for name in names {
        s.push_str(&format!("<th>{}</th>", html_escape(name)));
    }
    s.push_str("</tr></thead><tbody>\n");
    s
}

/// The report's closing: the row-count footer and the document close.
fn html_foot(rows: u64) -> String {
    let plural = if rows == 1 { "" } else { "s" };
    format!("</tbody></table><p class=\"meta\">{rows} row{plural}</p></main></body></html>\n")
}

/// Derive a target table name for a SQL `INSERT` export from the destination file
/// stem: keep alphanumerics and `_`, fold everything else to `_`, and fall back to
/// `exported_table` when nothing usable remains (e.g. a leading-digit or empty
/// stem). The result is later double-quoted by [`sql_ident`].
///
/// File-path-derived, so it stays here rather than moving to `red_core::valuefmt`
/// with the rest of the formatters: the clipboard names its table from the browsed
/// table, never from a destination path.
pub(crate) fn sql_table_name(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let sanitized: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() || trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        "exported_table".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The HTML report is a self-contained document: a head, a typed table header,
    /// one escaped `<tr>` per row, a NULL marker, and a row-count footer; and a
    /// cell that smuggles markup is escaped, not interpreted.
    #[test]
    fn html_report_is_well_formed_and_escaped() {
        let mut buf: Vec<u8> = Vec::new();
        let mut w = ExportWriter::begin(
            &mut buf,
            ExportFormat::Html,
            vec!["name".to_string(), "note".to_string()],
            Path::new("report.html"),
        )
        .unwrap();
        w.write_row(&[Value::Text("<script>".into()), Value::Null])
            .unwrap();
        w.write_row(&[Value::Text("a & b".into()), Value::Integer(7)])
            .unwrap();
        let outcome = w.finish().unwrap();
        assert_eq!(outcome, ExportOutcome::complete(2));

        let html = String::from_utf8(buf).unwrap();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<th>name</th><th>note</th>"));
        // The injected tag is escaped, and the raw form never appears.
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("a &amp; b"));
        assert!(html.contains("class=\"null\">NULL"));
        assert!(html.trim_end().ends_with("</html>"));
        assert!(html.contains("2 rows"));
    }

    /// Blobs report as a length marker (escaped), never raw bytes.
    #[test]
    fn html_report_blob_is_a_length_marker() {
        assert_eq!(html_cell(&Value::Blob(vec![0u8; 5])), "&lt;5 bytes&gt;");
    }

    /// A SQL literal doubles both quotes and backslashes, so a hostile cell can't
    /// break out of the string when the dump is reloaded into MySQL/ClickHouse.
    #[test]
    fn sql_string_escapes_quotes_and_backslashes() {
        // A trailing backslash: without doubling it would escape the close quote.
        assert_eq!(
            sql_value(&Value::Text("C:\\".into())),
            "'C:\\\\'",
            "trailing backslash must be doubled"
        );
        // The injection payload from the review: the literal must terminate.
        assert_eq!(
            sql_value(&Value::Text("\\', 1); DROP TABLE users; -- ".into())),
            "'\\\\'', 1); DROP TABLE users; -- '"
        );
    }

    /// Non-finite floats are unparseable as bare `NaN`/`inf`; export writes a
    /// form each target reads or rejects cleanly, never a broken file.
    #[test]
    fn non_finite_floats_export_safely() {
        assert_eq!(json_value(&Value::Real(f64::NAN)), "null");
        assert_eq!(json_value(&Value::Real(f64::INFINITY)), "null");
        assert_eq!(sql_value(&Value::Real(f64::NAN)), "'NaN'");
        assert_eq!(sql_value(&Value::Real(f64::INFINITY)), "'Infinity'");
        assert_eq!(sql_value(&Value::Real(f64::NEG_INFINITY)), "'-Infinity'");
        // A finite float is still bare.
        assert_eq!(sql_value(&Value::Real(1.5)), "1.5");
    }

    /// SQL export emits one `INSERT` per row with quoted identifiers, ANSI string
    /// literals (embedded quotes doubled), bare numbers, and the NULL keyword.
    #[test]
    fn sql_export_emits_insert_statements() {
        let mut buf: Vec<u8> = Vec::new();
        let mut w = ExportWriter::begin(
            &mut buf,
            ExportFormat::Sql,
            vec!["id".to_string(), "name".to_string()],
            Path::new("users.sql"),
        )
        .unwrap();
        w.write_row(&[Value::Integer(1), Value::Text("O'Brien".into())])
            .unwrap();
        w.write_row(&[Value::Integer(2), Value::Null]).unwrap();
        let outcome = w.finish().unwrap();
        assert_eq!(outcome, ExportOutcome::complete(2));

        let sql = String::from_utf8(buf).unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"users\" (\"id\", \"name\") VALUES (1, 'O''Brien');\n\
             INSERT INTO \"users\" (\"id\", \"name\") VALUES (2, NULL);\n"
        );
    }

    /// The INSERT table name is sanitized from the destination file stem, folding
    /// unsafe characters and falling back when the stem is unusable.
    #[test]
    fn sql_table_name_sanitizes_the_file_stem() {
        assert_eq!(sql_table_name(Path::new("/tmp/my-export.sql")), "my_export");
        assert_eq!(sql_table_name(Path::new("orders.sql")), "orders");
        assert_eq!(sql_table_name(Path::new("/tmp/123.sql")), "exported_table");
        assert_eq!(sql_table_name(Path::new("/tmp/___.sql")), "exported_table");
    }
}
