//! Engine-agnostic value/string formatters shared by the network drivers'
//! `export` path. These are pure [`Value`] → text functions with zero engine
//! knowledge, lifted here so every driver writes byte-identical CSV/JSON and a
//! new driver doesn't fork yet another copy.
//!
//! Blobs export as a `<N bytes>` length marker, not their bytes (hex/base64) — a
//! deliberate v0.1 choice: the streaming export path never materializes cell bytes,
//! and a text CSV/JSON of raw binary is rarely what a user wants. Binary-faithful
//! export is a later format option.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use red_core::{ExportFormat, Value};
use tokio::sync::mpsc::UnboundedSender;

/// Strip surrounding whitespace and a single trailing `;` so a user statement can
/// be wrapped in `SELECT * FROM (<sql>) AS _red` for paging/count/export.
pub(crate) fn strip_trailing(sql: &str) -> &str {
    sql.trim().strip_suffix(';').unwrap_or(sql.trim()).trim()
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
    /// ignored — progress is best-effort.
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
    format: ExportFormat,
    names: Vec<String>,
    written: u64,
}

impl<W: Write> ExportWriter<W> {
    /// Begin an export: write the CSV header row, the opening JSON `[`, or the HTML
    /// document head + table header.
    pub(crate) fn begin(mut out: W, format: ExportFormat, names: Vec<String>) -> io::Result<Self> {
        match format {
            ExportFormat::Csv => {
                writeln!(out, "{}", csv_record(names.iter().map(String::as_str)))?;
            }
            ExportFormat::Json => write!(out, "[")?,
            ExportFormat::Html => {
                // A streamed grid export carries no model-supplied title; the
                // generate_report tool renders titled reports via `render_html_report`.
                write!(out, "{}", html_head(None))?;
                write!(out, "{}", html_thead(&names))?;
            }
        }
        Ok(Self {
            out,
            format,
            names,
            written: 0,
        })
    }

    /// Write one row (cells positionally aligned with the column names): CSV
    /// escaping for CSV, object framing + comma separation for JSON.
    pub(crate) fn write_row(&mut self, cells: &[Value]) -> io::Result<()> {
        match self.format {
            ExportFormat::Csv => {
                let fields: Vec<String> = cells.iter().map(csv_cell).collect();
                writeln!(
                    self.out,
                    "{}",
                    csv_record(fields.iter().map(String::as_str))
                )?;
            }
            ExportFormat::Json => {
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
            ExportFormat::Html => {
                write!(self.out, "<tr>")?;
                for value in cells {
                    write!(self.out, "<td>{}</td>", html_cell(value))?;
                }
                writeln!(self.out, "</tr>")?;
            }
        }
        self.written += 1;
        Ok(())
    }

    /// Close the export: JSON gets its trailing `]`, HTML closes the table + a row-
    /// count footer + the document; CSV needs no footer. Flush, return the count.
    pub(crate) fn finish(mut self) -> io::Result<u64> {
        match self.format {
            ExportFormat::Json => write!(self.out, "\n]\n")?,
            ExportFormat::Html => write!(self.out, "{}", html_foot(self.written))?,
            ExportFormat::Csv => {}
        }
        self.out.flush()?;
        Ok(self.written)
    }

    /// Rows written so far — feeds the progress throttle.
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
const DEFAULT_REPORT_TITLE: &str = "Red — query report";

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

/// One HTML cell's inner content: NULL renders as a dim italic marker, blobs as a
/// length marker, everything else HTML-escaped text.
pub(crate) fn html_cell(value: &Value) -> String {
    match value {
        Value::Null => "<span class=\"null\">NULL</span>".to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => html_escape(s),
        Value::Blob(b) => format!("&lt;{} bytes&gt;", b.len()),
        Value::Capped(_) => html_escape(&value.to_string()),
    }
}

/// Escape the five HTML-significant characters so cell text can't break the markup
/// (or inject it). Shared with the AI report path (`red-service`) so both HTML
/// emitters escape identically.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn csv_record<'a>(fields: impl Iterator<Item = &'a str>) -> String {
    fields.map(csv_escape).collect::<Vec<_>>().join(",")
}

pub(crate) fn csv_escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

pub(crate) fn csv_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
        // Export never caps, so a `Capped` can't reach here — rendered for totality.
        Value::Capped(_) => value.to_string(),
    }
}

pub(crate) fn json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => json_string(s),
        Value::Blob(b) => json_string(&format!("<{} bytes>", b.len())),
        Value::Capped(_) => json_string(&value.to_string()),
    }
}

pub(crate) fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The HTML report is a self-contained document: a head, a typed table header,
    /// one escaped `<tr>` per row, a NULL marker, and a row-count footer — and a
    /// cell that smuggles markup is escaped, not interpreted.
    #[test]
    fn html_report_is_well_formed_and_escaped() {
        let mut buf: Vec<u8> = Vec::new();
        let mut w = ExportWriter::begin(
            &mut buf,
            ExportFormat::Html,
            vec!["name".to_string(), "note".to_string()],
        )
        .unwrap();
        w.write_row(&[Value::Text("<script>".to_string()), Value::Null])
            .unwrap();
        w.write_row(&[Value::Text("a & b".to_string()), Value::Integer(7)])
            .unwrap();
        let rows = w.finish().unwrap();
        assert_eq!(rows, 2);

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
}
