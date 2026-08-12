//! Pure [`Value`] → text formatters, shared by every path that turns cells into
//! a text serialization: the drivers' streaming file export and the UI's
//! clipboard. One copy so a CSV file and a CSV clipboard can never disagree on
//! escaping, and so a new sink inherits the injection-safe SQL/JSON quoting
//! rather than reinventing it.
//!
//! Blobs render as a `<N bytes>` length marker rather than their bytes. The
//! export path never materializes cell bytes, and a text serialization of raw
//! binary is rarely what a user wants; both sinks keep that convention.

use crate::Value;

/// Escape one CSV field: quote it only when it holds a delimiter, a quote, or a
/// newline, doubling any embedded quote.
pub fn csv_escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// Join pre-escaped fields into one comma-separated CSV record.
pub fn csv_record<'a>(fields: impl Iterator<Item = &'a str>) -> String {
    fields.map(csv_escape).collect::<Vec<_>>().join(",")
}

/// One cell as a bare CSV field (NULL → empty, so an absent value is
/// distinguishable from the literal text "NULL").
pub fn csv_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => s.to_string(),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
        // Export never caps, so a `Capped` can't reach here; the clipboard
        // re-fetches capped cells in full before formatting. Rendered for totality.
        Value::Capped(_) => value.to_string(),
    }
}

/// A JSON string literal with the control characters escaped.
pub fn json_string(s: &str) -> String {
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

/// One cell as a JSON value.
pub fn json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Integer(n) => n.to_string(),
        // JSON has no NaN/Infinity literal, and `f64::to_string` emits bare
        // `NaN`/`inf` that fails every parser; render them as `null` (the
        // conventional JSON stand-in) rather than writing an unparseable file.
        Value::Real(x) if !x.is_finite() => "null".to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => json_string(s),
        Value::Blob(b) => json_string(&format!("<{} bytes>", b.len())),
        Value::Capped(_) => json_string(&value.to_string()),
    }
}

/// A SQL identifier (table or column) in portable ANSI form: double-quoted with
/// any embedded `"` doubled. Works for SQLite / Postgres / ClickHouse and for
/// MySQL under `ANSI_QUOTES`; a deliberately dialect-neutral default for text
/// the user carries elsewhere.
pub fn sql_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Single-quote a string literal, doubling embedded quotes **and** backslashes.
///
/// This is written for the stricter reader: MySQL (by default) and ClickHouse
/// honour `\` as an escape inside `'…'`, and without the doubled backslash a
/// value ending in `\` swallows the closing quote — a hostile cell like
/// `\', 1); DROP TABLE users; -- ` then breaks out of the literal when the text
/// is reloaded. Engines that treat `\` literally (Postgres/SQLite under
/// standard-conforming strings) would reimport a doubled backslash; that
/// data-fidelity nit is the deliberate price of injection safety.
pub fn sql_string(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

/// A SQL literal for one cell: NULL keyword, bare numbers, single-quoted
/// strings. Blobs keep the module-wide `<N bytes>` convention (as a string
/// literal), not raw/hex bytes.
pub fn sql_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(n) => n.to_string(),
        // `VALUES (NaN)` / `(inf)` load in no engine (bare `NaN` is a syntax
        // error). Postgres accepts the quoted `'NaN'`/`'Infinity'` float forms,
        // and other engines have no NaN at all, so the portable choice is a
        // single-quoted spelling that Postgres reads and others reject cleanly
        // rather than mis-parsing.
        Value::Real(x) if x.is_nan() => "'NaN'".to_string(),
        Value::Real(x) if x.is_infinite() => if *x < 0.0 {
            "'-Infinity'"
        } else {
            "'Infinity'"
        }
        .to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => sql_string(s),
        Value::Blob(b) => sql_string(&format!("<{} bytes>", b.len())),
        Value::Capped(_) => sql_string(&value.to_string()),
    }
}

/// Escape the five HTML-significant characters so cell text can't break the
/// markup (or inject it).
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

/// One HTML cell's inner content: NULL renders as a dim italic marker, blobs as
/// a length marker, everything else escaped text.
pub fn html_cell(value: &Value) -> String {
    match value {
        Value::Null => "<span class=\"null\">NULL</span>".to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => html_escape(s),
        Value::Blob(b) => format!("&lt;{} bytes&gt;", b.len()),
        Value::Capped(_) => html_escape(&value.to_string()),
    }
}

/// One cell as a plain tab-separated field (NULL → empty). The historical
/// clipboard shape, and what a spreadsheet paste expects.
pub fn tsv_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => s.to_string(),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
        Value::Capped(c) if c.blob => format!("<{} bytes>", c.len),
        Value::Capped(c) => format!("{}…", c.head),
    }
}

/// One cell as Markdown table text: escape the `|` that would otherwise open a
/// new cell, and flatten newlines to `<br>` so a multi-line value can't break
/// the row apart.
fn markdown_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        other => other
            .to_string()
            .replace('\\', "\\\\")
            .replace('|', "\\|")
            .replace(['\n', '\r'], "<br>"),
    }
}

/// What shape a copied selection takes on the clipboard.
///
/// Deliberately its own enum rather than a reuse of [`ExportFormat`](crate::ExportFormat):
/// the two sets only overlap. A clipboard has no use for XLSX (a binary archive)
/// and a file has no use for an `IN (…)` fragment, so sharing one enum would
/// make "copy as XLSX" and "export as IN-list" representable states that every
/// call site would then have to reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardFormat {
    /// Tab-separated, no header. The historical shape and what a spreadsheet
    /// paste expects, so it stays the default.
    Tsv,
    /// Tab-separated with a leading header row.
    TsvHeaders,
    Csv,
    /// A JSON array of objects keyed by column name.
    Json,
    /// One `INSERT INTO "table" (...) VALUES (...);` per row.
    Sql,
    /// A GitHub-flavoured Markdown table.
    Markdown,
    /// A parenthesised `IN` list of the first selected column's values, for
    /// pasting into a hand-written follow-up query.
    InList,
}

impl ClipboardFormat {
    /// Whether this format names its columns, so a caller knows the column
    /// headers are load-bearing rather than decorative.
    pub fn uses_column_names(self) -> bool {
        !matches!(self, Self::Tsv | Self::InList)
    }
}

/// Serializes a copied selection one row at a time.
///
/// Row-at-a-time rather than over a materialized `Vec<Vec<Value>>` so the two
/// copy paths (resident buffer rows, borrowed; and re-fetched rows, owned) share
/// one implementation without either having to build the other's shape. Mirrors
/// the drivers' `ExportWriter`.
pub struct ClipboardWriter {
    format: ClipboardFormat,
    /// The selected columns' names, positionally aligned with each row's cells.
    names: Vec<String>,
    /// The table name for [`ClipboardFormat::Sql`], already quoted-safe input.
    table: String,
    out: String,
    written: usize,
}

impl ClipboardWriter {
    /// Begin a copy. `names` are the *selected* columns in order; `table` names
    /// the source table for the SQL form and is ignored by every other format.
    pub fn begin(format: ClipboardFormat, names: Vec<String>, table: Option<&str>) -> Self {
        let mut out = String::new();
        match format {
            ClipboardFormat::TsvHeaders => {
                out.push_str(&names.join("\t"));
                out.push('\n');
            }
            ClipboardFormat::Csv => {
                out.push_str(&csv_record(names.iter().map(String::as_str)));
                out.push('\n');
            }
            ClipboardFormat::Json => out.push('['),
            ClipboardFormat::Markdown => {
                out.push_str(&format!("| {} |\n", names.join(" | ")));
                let rule: Vec<&str> = names.iter().map(|_| "---").collect();
                out.push_str(&format!("| {} |\n", rule.join(" | ")));
            }
            ClipboardFormat::InList => out.push('('),
            ClipboardFormat::Tsv | ClipboardFormat::Sql => {}
        }
        Self {
            format,
            names,
            table: table.unwrap_or("exported_table").to_string(),
            out,
            written: 0,
        }
    }

    /// Append one row, whose cells are positionally aligned with the names given
    /// to [`begin`](Self::begin).
    pub fn write_row(&mut self, cells: &[Value]) {
        match self.format {
            ClipboardFormat::Tsv | ClipboardFormat::TsvHeaders => {
                for (i, value) in cells.iter().enumerate() {
                    if i > 0 {
                        self.out.push('\t');
                    }
                    self.out.push_str(&tsv_cell(value));
                }
                self.out.push('\n');
            }
            ClipboardFormat::Csv => {
                let fields: Vec<String> = cells.iter().map(csv_cell).collect();
                self.out
                    .push_str(&csv_record(fields.iter().map(String::as_str)));
                self.out.push('\n');
            }
            ClipboardFormat::Json => {
                if self.written > 0 {
                    self.out.push(',');
                }
                self.out.push_str("\n  {");
                for (i, value) in cells.iter().enumerate() {
                    if i > 0 {
                        self.out.push(',');
                    }
                    // A row wider than the header falls back to an empty key.
                    let name = self.names.get(i).map(String::as_str).unwrap_or("");
                    self.out
                        .push_str(&format!("{}:{}", json_string(name), json_value(value)));
                }
                self.out.push('}');
            }
            ClipboardFormat::Sql => {
                self.out
                    .push_str(&format!("INSERT INTO {} (", sql_ident(&self.table)));
                for (i, name) in self.names.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(&sql_ident(name));
                }
                self.out.push_str(") VALUES (");
                for (i, value) in cells.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(&sql_value(value));
                }
                self.out.push_str(");\n");
            }
            ClipboardFormat::Markdown => {
                self.out.push_str("| ");
                for (i, value) in cells.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(" | ");
                    }
                    self.out.push_str(&markdown_cell(value));
                }
                self.out.push_str(" |\n");
            }
            // Only the first selected column is meaningful: an `IN` list is a
            // single-column predicate, and silently interleaving a second
            // column's values would build a filter that matches the wrong rows.
            ClipboardFormat::InList => {
                if let Some(value) = cells.first() {
                    if self.written > 0 {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(&sql_value(value));
                }
            }
        }
        self.written += 1;
    }

    /// Close the serialization and yield the clipboard text.
    pub fn finish(mut self) -> String {
        match self.format {
            ClipboardFormat::Json => self.out.push_str("\n]\n"),
            ClipboardFormat::InList => self.out.push(')'),
            _ => {}
        }
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render `rows` in `format` through the writer, the way both copy paths do.
    fn render(format: ClipboardFormat, rows: &[&[Value]]) -> String {
        let names = vec!["id".to_string(), "name".to_string()];
        let mut w = ClipboardWriter::begin(format, names, Some("users"));
        for row in rows {
            w.write_row(row);
        }
        w.finish()
    }

    fn sample() -> Vec<Vec<Value>> {
        vec![
            vec![Value::Integer(1), Value::Text("O'Brien".into())],
            vec![Value::Integer(2), Value::Null],
        ]
    }

    #[test]
    fn tsv_is_bare_and_headered_forms_differ_only_by_the_header() {
        let rows = sample();
        let refs: Vec<&[Value]> = rows.iter().map(Vec::as_slice).collect();
        let bare = render(ClipboardFormat::Tsv, &refs);
        let headered = render(ClipboardFormat::TsvHeaders, &refs);
        assert_eq!(bare, "1\tO'Brien\n2\t\n");
        assert_eq!(headered, format!("id\tname\n{bare}"));
    }

    #[test]
    fn csv_quotes_only_what_needs_it() {
        let row = [Value::Integer(1), Value::Text("Smith, John".into())];
        assert_eq!(
            render(ClipboardFormat::Csv, &[&row]),
            "id,name\n1,\"Smith, John\"\n"
        );
    }

    #[test]
    fn json_is_an_array_of_named_objects() {
        let rows = sample();
        let refs: Vec<&[Value]> = rows.iter().map(Vec::as_slice).collect();
        assert_eq!(
            render(ClipboardFormat::Json, &refs),
            "[\n  {\"id\":1,\"name\":\"O'Brien\"},\n  {\"id\":2,\"name\":null}\n]\n"
        );
    }

    #[test]
    fn sql_emits_one_insert_per_row_against_the_source_table() {
        let rows = sample();
        let refs: Vec<&[Value]> = rows.iter().map(Vec::as_slice).collect();
        assert_eq!(
            render(ClipboardFormat::Sql, &refs),
            "INSERT INTO \"users\" (\"id\", \"name\") VALUES (1, 'O''Brien');\n\
             INSERT INTO \"users\" (\"id\", \"name\") VALUES (2, NULL);\n"
        );
    }

    #[test]
    fn markdown_has_a_header_and_a_rule() {
        let rows = sample();
        let refs: Vec<&[Value]> = rows.iter().map(Vec::as_slice).collect();
        assert_eq!(
            render(ClipboardFormat::Markdown, &refs),
            "| id | name |\n| --- | --- |\n| 1 | O'Brien |\n| 2 |  |\n"
        );
    }

    /// The `IN` list takes only the first selected column: mixing a second
    /// column's values in would build a predicate matching the wrong rows.
    #[test]
    fn in_list_uses_only_the_first_column() {
        let rows = sample();
        let refs: Vec<&[Value]> = rows.iter().map(Vec::as_slice).collect();
        assert_eq!(render(ClipboardFormat::InList, &refs), "(1, 2)");
    }

    /// An empty selection still yields syntactically closed output rather than a
    /// dangling opener.
    #[test]
    fn empty_selections_close_their_framing() {
        assert_eq!(render(ClipboardFormat::Json, &[]), "[\n]\n");
        assert_eq!(render(ClipboardFormat::InList, &[]), "()");
        assert_eq!(render(ClipboardFormat::Tsv, &[]), "");
    }

    /// A SQL literal doubles both quotes and backslashes, so a hostile cell
    /// can't break out of the string when the text is reloaded.
    #[test]
    fn sql_string_escapes_quotes_and_backslashes() {
        assert_eq!(
            sql_value(&Value::Text("C:\\".into())),
            "'C:\\\\'",
            "trailing backslash must be doubled"
        );
        assert_eq!(
            sql_value(&Value::Text("\\', 1); DROP TABLE users; -- ".into())),
            "'\\\\'', 1); DROP TABLE users; -- '"
        );
    }

    /// Non-finite floats are unparseable as bare `NaN`/`inf`; every sink writes
    /// a form its reader either parses or rejects cleanly, never broken text.
    #[test]
    fn non_finite_floats_format_safely() {
        assert_eq!(json_value(&Value::Real(f64::NAN)), "null");
        assert_eq!(json_value(&Value::Real(f64::INFINITY)), "null");
        assert_eq!(sql_value(&Value::Real(f64::NAN)), "'NaN'");
        assert_eq!(sql_value(&Value::Real(f64::INFINITY)), "'Infinity'");
        assert_eq!(sql_value(&Value::Real(f64::NEG_INFINITY)), "'-Infinity'");
        assert_eq!(sql_value(&Value::Real(1.5)), "1.5");
    }

    /// A blob is a length marker in every text sink, never its bytes.
    #[test]
    fn blobs_are_length_markers() {
        let blob = Value::Blob(vec![0u8; 5]);
        assert_eq!(csv_cell(&blob), "<5 bytes>");
        assert_eq!(tsv_cell(&blob), "<5 bytes>");
        assert_eq!(html_cell(&blob), "&lt;5 bytes&gt;");
        assert_eq!(json_value(&blob), "\"<5 bytes>\"");
    }

    /// A cell holding the Markdown cell separator is escaped rather than
    /// splitting the row into an extra column.
    #[test]
    fn markdown_cell_escapes_the_separator() {
        assert_eq!(markdown_cell(&Value::Text("a|b".into())), "a\\|b");
        assert_eq!(markdown_cell(&Value::Text("a\nb".into())), "a<br>b");
        assert_eq!(markdown_cell(&Value::Null), "");
    }
}
