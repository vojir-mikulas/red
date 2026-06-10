//! Engine-agnostic value/string formatters shared by the network drivers'
//! `export` path. These are pure [`Value`] → text functions with zero engine
//! knowledge, lifted here so every driver writes byte-identical CSV/JSON and a
//! new driver doesn't fork yet another copy.

use red_core::Value;

/// Strip surrounding whitespace and a single trailing `;` so a user statement can
/// be wrapped in `SELECT * FROM (<sql>) AS _red` for paging/count/export.
pub(crate) fn strip_trailing(sql: &str) -> &str {
    sql.trim().strip_suffix(';').unwrap_or(sql.trim()).trim()
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
    }
}

pub(crate) fn json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => json_string(s),
        Value::Blob(b) => json_string(&format!("<{} bytes>", b.len())),
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
