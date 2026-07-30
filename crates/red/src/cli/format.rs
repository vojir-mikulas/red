//! Render a streamed query result to stdout in one of four shapes. The delimited
//! and JSON writers are **streaming**: each row window is written as it arrives,
//! so a huge result never materializes CLI-side. The aligned `table` writer is
//! the exception: column widths need the whole result, so it buffers (use `csv`/
//! `json` for results that don't fit in memory).

use std::io::{self, Write};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::ValueEnum;
use red_core::{Column, Value};

/// Output shape for `red query --format …`.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum OutFormat {
    /// Aligned text table (buffers the whole result to size columns).
    Table,
    /// Comma-separated values, RFC-4180-style quoting.
    Csv,
    /// Tab-separated values.
    Tsv,
    /// A streamed JSON array of `{column: value}` objects.
    Json,
}

/// Incremental result writer: [`start`](Self::start) once with the columns, then
/// [`rows`](Self::rows) per window, then [`finish`](Self::finish).
///
/// Writes are deliberately infallible at the call site — a broken pipe must not
/// panic (`red query … | head -n1` is ordinary shell usage) — but the *first*
/// failure is remembered so the caller can stop and exit non-zero. Silently
/// truncating a redirected file and then reporting success is how a seed script
/// proceeds with corrupt data.
pub struct Writer<W: Write> {
    format: OutFormat,
    out: W,
    columns: Vec<String>,
    /// Buffered rows for the `table` format only (the others stream).
    buffered: Vec<Vec<String>>,
    /// `json`: whether a row has been written yet (drives the comma separators).
    json_first: bool,
    /// The first write error seen, if any. Sticky: later writes are no-ops.
    failed: Option<io::Error>,
}

impl<W: Write> Writer<W> {
    pub fn new(format: OutFormat, out: W) -> Self {
        Self {
            format,
            out,
            columns: Vec::new(),
            buffered: Vec::new(),
            json_first: true,
            failed: None,
        }
    }

    /// Record `result` if it is the first failure.
    fn record(&mut self, result: io::Result<()>) {
        if let Err(e) = result
            && self.failed.is_none()
        {
            self.failed = Some(e);
        }
    }

    /// Whether output has failed, and the exit code to use if so. Reported once:
    /// a broken pipe is normal shell plumbing and exits `EXIT_OK`, while a real
    /// write failure (a full disk, a closed file) is an error the caller must not
    /// paper over with a success summary.
    pub fn check_io(&mut self) -> Option<u8> {
        let e = self.failed.as_ref()?;
        if e.kind() == io::ErrorKind::BrokenPipe {
            return Some(super::EXIT_OK);
        }
        eprintln!("error: writing output: {e}");
        Some(super::EXIT_QUERY)
    }

    /// Record the columns and emit any header/preamble the format needs.
    pub fn start(&mut self, columns: &[Column]) {
        self.columns = columns.iter().map(|c| c.name.clone()).collect();
        let r = match self.format {
            OutFormat::Csv => write_delim(&mut self.out, &self.columns, ','),
            OutFormat::Tsv => write_delim(&mut self.out, &self.columns, '\t'),
            OutFormat::Json => write!(self.out, "["),
            OutFormat::Table => Ok(()),
        };
        self.record(r);
    }

    /// Write one window of rows (streamed, except `table` which buffers).
    pub fn rows(&mut self, rows: &[Vec<Value>]) {
        match self.format {
            OutFormat::Table => {
                for r in rows {
                    self.buffered.push(r.iter().map(cell_text).collect());
                }
            }
            OutFormat::Csv => self.write_rows_delim(rows, ','),
            OutFormat::Tsv => self.write_rows_delim(rows, '\t'),
            OutFormat::Json => {
                for r in rows {
                    let sep = if self.json_first { "\n  " } else { ",\n  " };
                    self.json_first = false;
                    let w = write!(self.out, "{sep}{}", json_row(&self.columns, r));
                    self.record(w);
                }
            }
        }
    }

    /// Emit any trailer and flush.
    pub fn finish(&mut self) {
        match self.format {
            OutFormat::Table => self.render_table(),
            OutFormat::Json => {
                // `[]` when empty, else close the pretty-printed array.
                let r = if self.json_first {
                    writeln!(self.out, "]")
                } else {
                    writeln!(self.out, "\n]")
                };
                self.record(r);
            }
            OutFormat::Csv | OutFormat::Tsv => {}
        }
        // The flush is where a buffered writer surfaces a full disk, so it counts.
        let r = self.out.flush();
        self.record(r);
    }

    fn write_rows_delim(&mut self, rows: &[Vec<Value>], delim: char) {
        for r in rows {
            let cells: Vec<String> = r.iter().map(cell_text).collect();
            let w = write_delim(&mut self.out, &cells, delim);
            self.record(w);
        }
    }

    /// Render the buffered `table`: header, dashed rule, padded rows. Widths are
    /// measured in `char`s (close enough for aligned output; not grapheme-aware).
    fn render_table(&mut self) {
        let cols = self.columns.len();
        let mut widths: Vec<usize> = self.columns.iter().map(|c| c.chars().count()).collect();
        for row in &self.buffered {
            for (i, cell) in row.iter().enumerate().take(cols) {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        let pad = |s: &str, w: usize| format!("{s}{}", " ".repeat(w - s.chars().count()));

        let header: Vec<String> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| pad(c, widths[i]))
            .collect();
        let mut r = writeln!(self.out, "{}", header.join("  "));
        let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        r = r.and_then(|()| writeln!(self.out, "{}", rule.join("  ")));
        for row in &self.buffered {
            let cells: Vec<String> = (0..cols)
                .map(|i| {
                    let cell = row.get(i).map(String::as_str).unwrap_or("");
                    pad(cell, widths[i])
                })
                .collect();
            r = r.and_then(|()| writeln!(self.out, "{}", cells.join("  ")));
        }
        self.record(r);
    }
}

/// A cell as plain text for `table`/`csv`/`tsv`: the `Value`'s own `Display`
/// (`NULL`, numbers verbatim, `<N bytes>` for a blob).
fn cell_text(v: &Value) -> String {
    v.to_string()
}

/// Write one delimited line with RFC-4180-style quoting for the given delimiter.
fn write_delim<W: Write>(out: &mut W, cells: &[String], delim: char) -> io::Result<()> {
    let line: Vec<String> = cells.iter().map(|c| quote_field(c, delim)).collect();
    writeln!(out, "{}", line.join(&delim.to_string()))
}

/// Quote a field when it contains the delimiter, a quote, or a newline: wrap in
/// double quotes and double any embedded quote.
fn quote_field(s: &str, delim: char) -> String {
    if s.contains(delim) || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Build one JSON object `{column: value}` for a row.
fn json_row(columns: &[String], row: &[Value]) -> String {
    let mut map = serde_json::Map::with_capacity(columns.len());
    for (i, name) in columns.iter().enumerate() {
        let v = row
            .get(i)
            .map(value_to_json)
            .unwrap_or(serde_json::Value::Null);
        map.insert(name.clone(), v);
    }
    serde_json::Value::Object(map).to_string()
}

/// Map a `Value` to JSON: nulls/numbers/strings natively, blobs as base64, a
/// non-finite float or a capped cell degraded to a string.
fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Null => J::Null,
        Value::Integer(n) => J::from(*n),
        Value::Real(x) => serde_json::Number::from_f64(*x).map_or(J::Null, J::Number),
        Value::Text(s) => J::from(s.as_ref()),
        Value::Blob(b) => J::from(STANDARD.encode(b)),
        Value::Capped(c) if c.blob => J::from(format!("<{} bytes>", c.len)),
        Value::Capped(c) => J::from(format!("{}…", c.head)),
    }
}
