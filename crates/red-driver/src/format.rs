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
    /// Begin an export: write the CSV header row, or the opening JSON `[`.
    pub(crate) fn begin(mut out: W, format: ExportFormat, names: Vec<String>) -> io::Result<Self> {
        match format {
            ExportFormat::Csv => {
                writeln!(out, "{}", csv_record(names.iter().map(String::as_str)))?;
            }
            ExportFormat::Json => write!(out, "[")?,
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
        }
        self.written += 1;
        Ok(())
    }

    /// Close the export (JSON gets its trailing `]`; CSV needs no footer), flush,
    /// and return the row count written.
    pub(crate) fn finish(mut self) -> io::Result<u64> {
        if let ExportFormat::Json = self.format {
            write!(self.out, "\n]\n")?;
        }
        self.out.flush()?;
        Ok(self.written)
    }

    /// Rows written so far — feeds the progress throttle.
    pub(crate) fn written(&self) -> u64 {
        self.written
    }
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
