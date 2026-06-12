//! Engine-agnostic value/string formatters shared by the network drivers'
//! `export` path. These are pure [`Value`] → text functions with zero engine
//! knowledge, lifted here so every driver writes byte-identical CSV/JSON and a
//! new driver doesn't fork yet another copy.
//!
//! Blobs export as a `<N bytes>` length marker, not their bytes (hex/base64) — a
//! deliberate v0.1 choice: the streaming export path never materializes cell bytes,
//! and a text CSV/JSON of raw binary is rarely what a user wants. Binary-faithful
//! export is a later format option.

use std::time::{Duration, Instant};

use red_core::Value;
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
        if written - self.last_sent >= PROGRESS_ROWS || self.last_at.elapsed() >= PROGRESS_INTERVAL
        {
            let _ = self.sender.send(written);
            self.last_sent = written;
            self.last_at = Instant::now();
        }
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
