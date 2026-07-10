//! Streaming row readers for data import (CSV / JSONL / JSON array), the read-side
//! mirror of `format.rs`'s [`ExportWriter`](crate::format::ExportWriter).
//! Engine-independent: yields one row of raw *text* cells at a time, holding at most
//! one record in memory, never the whole file. The dispatch import loop coerces each
//! cell to a typed `Value` per the target column ([`red_core::coerce_edit_value`])
//! and batches the rows into
//! [`DatabaseDriver::insert_rows`](crate::DatabaseDriver::insert_rows).
//!
//! No external CSV crate: a ~40-line RFC 4180 record reader (quoted fields, embedded
//! commas/newlines, doubled `""` escapes) per the roadmap's "port ~50 lines over a
//! dependency" rule. JSONL and the JSON-array reader ride the `serde_json` already in
//! the tree; the array reader scans one element at a time (brace-depth + string
//! aware) so a large array is still streamed, not materialized.

use std::io::{self, BufRead};

use red_core::ImportFormat;

/// A streaming reader over a CSV or JSONL source. [`begin`](Self::begin) reads the
/// source column names; each [`next_row`](Self::next_row) pulls one record's cells
/// (already projected to the source-column order). Generic over any [`BufRead`] so
/// it works on a file or an in-memory buffer (tests).
pub struct ImportReader<R: BufRead> {
    inner: Inner<R>,
}

enum Inner<R: BufRead> {
    Csv(CsvReader<R>),
    Jsonl {
        reader: R,
        columns: Vec<String>,
        /// The first object's projected cells, buffered by `begin` (which had to
        /// read it to learn the column names) and handed back on the first
        /// `next_row` so no data row is lost.
        pending: Option<Vec<String>>,
    },
    JsonArray(JsonArrayReader<R>),
}

impl<R: BufRead> ImportReader<R> {
    /// Open `reader` as `format`, returning the **source column names** and the
    /// reader positioned at the first data row. CSV: the first record is the header.
    /// JSONL: the keys of the first object (insertion/sorted order); that first
    /// object's values are buffered and returned by the first `next_row`. An empty
    /// source yields no columns and an immediately-exhausted reader.
    pub fn begin(reader: R, format: ImportFormat) -> io::Result<(Vec<String>, Self)> {
        match format {
            ImportFormat::Csv => {
                let mut csv = CsvReader {
                    reader,
                    done: false,
                };
                let columns = csv.next_record()?.unwrap_or_default();
                Ok((
                    columns,
                    Self {
                        inner: Inner::Csv(csv),
                    },
                ))
            }
            ImportFormat::Jsonl => {
                let mut reader = reader;
                match read_nonempty_line(&mut reader)? {
                    None => Ok((
                        Vec::new(),
                        Self {
                            inner: Inner::Jsonl {
                                reader,
                                columns: Vec::new(),
                                pending: None,
                            },
                        },
                    )),
                    Some(line) => {
                        let obj = parse_json_object(&line)?;
                        let columns: Vec<String> = obj.keys().cloned().collect();
                        let pending = Some(project_json(&obj, &columns));
                        Ok((
                            columns.clone(),
                            Self {
                                inner: Inner::Jsonl {
                                    reader,
                                    columns,
                                    pending,
                                },
                            },
                        ))
                    }
                }
            }
            ImportFormat::JsonArray => {
                let mut inner = JsonArrayReader {
                    reader,
                    columns: Vec::new(),
                    pending: None,
                    array_started: false,
                    first_element: true,
                    done: false,
                };
                let columns = match inner.next_object()? {
                    // An empty array (or empty file) yields no columns.
                    None => Vec::new(),
                    Some(obj) => {
                        let columns: Vec<String> = obj.keys().cloned().collect();
                        inner.pending = Some(project_json(&obj, &columns));
                        inner.columns = columns.clone();
                        columns
                    }
                };
                Ok((
                    columns,
                    Self {
                        inner: Inner::JsonArray(inner),
                    },
                ))
            }
        }
    }

    /// Pull the next data row as raw text cells (one per source column), or `None` at
    /// end of file. Cells are unparsed strings; an absent JSONL key or empty CSV
    /// field comes back as `""` (which `coerce_edit_value` maps to NULL).
    pub fn next_row(&mut self) -> io::Result<Option<Vec<String>>> {
        match &mut self.inner {
            Inner::Csv(csv) => csv.next_record(),
            Inner::Jsonl {
                reader,
                columns,
                pending,
            } => {
                if let Some(first) = pending.take() {
                    return Ok(Some(first));
                }
                match read_nonempty_line(reader)? {
                    None => Ok(None),
                    Some(line) => {
                        let obj = parse_json_object(&line)?;
                        Ok(Some(project_json(&obj, columns)))
                    }
                }
            }
            Inner::JsonArray(r) => {
                if let Some(first) = r.pending.take() {
                    return Ok(Some(first));
                }
                match r.next_object()? {
                    None => Ok(None),
                    Some(obj) => Ok(Some(project_json(&obj, &r.columns))),
                }
            }
        }
    }
}

/// A streaming reader over a single top-level JSON array of objects. Reads one
/// element at a time — never the whole array — by scanning the raw bytes of each
/// value (tracking brace/bracket depth and string/escape state) between the array's
/// `[`, `,`, and `]` framing.
struct JsonArrayReader<R: BufRead> {
    reader: R,
    columns: Vec<String>,
    /// The first object's projected cells, buffered by `begin` (which read it to
    /// learn the column names) and handed back on the first `next_row`.
    pending: Option<Vec<String>>,
    /// Whether the opening `[` has been consumed yet.
    array_started: bool,
    /// Whether the next element is the first (no leading comma expected).
    first_element: bool,
    done: bool,
}

impl<R: BufRead> JsonArrayReader<R> {
    /// Read the next array element as a JSON object, consuming the surrounding
    /// `[` / `,` / `]` framing. `None` once the closing `]` (or an empty file) is
    /// reached. Errors on malformed structure or a non-object element.
    fn next_object(&mut self) -> io::Result<Option<serde_json::Map<String, serde_json::Value>>> {
        if self.done {
            return Ok(None);
        }
        if !self.array_started {
            skip_ws(&mut self.reader)?;
            match peek_byte(&mut self.reader)? {
                None => {
                    // An empty source is treated as an empty array.
                    self.done = true;
                    return Ok(None);
                }
                Some(b'[') => {
                    read_byte(&mut self.reader)?;
                    self.array_started = true;
                }
                Some(_) => return Err(invalid("expected '[' at the start of a JSON array")),
            }
        }
        skip_ws(&mut self.reader)?;
        match peek_byte(&mut self.reader)? {
            None => return Err(invalid("unterminated JSON array")),
            Some(b']') => {
                read_byte(&mut self.reader)?;
                self.done = true;
                return Ok(None);
            }
            Some(b',') => {
                if self.first_element {
                    return Err(invalid("unexpected ',' before the first array element"));
                }
                read_byte(&mut self.reader)?;
                skip_ws(&mut self.reader)?;
            }
            Some(_) if self.first_element => {}
            Some(_) => return Err(invalid("expected ',' or ']' between array elements")),
        }
        let bytes = read_json_value_bytes(&mut self.reader)?;
        self.first_element = false;
        let text = std::str::from_utf8(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(parse_json_object(text)?))
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Read and consume one byte, or `None` at EOF.
fn read_byte<R: BufRead>(reader: &mut R) -> io::Result<Option<u8>> {
    let byte = reader.fill_buf()?.first().copied();
    if byte.is_some() {
        reader.consume(1);
    }
    Ok(byte)
}

/// Peek the next byte without consuming it, or `None` at EOF.
fn peek_byte<R: BufRead>(reader: &mut R) -> io::Result<Option<u8>> {
    Ok(reader.fill_buf()?.first().copied())
}

/// Consume any leading ASCII whitespace.
fn skip_ws<R: BufRead>(reader: &mut R) -> io::Result<()> {
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len();
        let n = buf.iter().take_while(|b| b.is_ascii_whitespace()).count();
        reader.consume(n);
        // Stop once a non-whitespace byte is in view; if the whole buffer was
        // whitespace, loop to refill.
        if n < len || n == 0 {
            return Ok(());
        }
    }
}

/// Read the raw bytes of one complete JSON value. For an object/array it tracks
/// brace/bracket depth (ignoring structure inside strings, honoring `\` escapes) and
/// stops after the matching close; for a scalar it reads until a top-level `,`, `]`,
/// or whitespace (leaving that delimiter unconsumed). Assumes leading whitespace is
/// already skipped. Structural characters are ASCII, so depth tracking on raw bytes
/// is UTF-8 safe.
fn read_json_value_bytes<R: BufRead>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let first = peek_byte(reader)?.ok_or_else(|| invalid("expected a JSON value"))?;
    let mut in_str = false;
    let mut escaped = false;
    if first == b'{' || first == b'[' {
        let mut depth = 0i32;
        loop {
            let b = read_byte(reader)?.ok_or_else(|| invalid("unterminated JSON value"))?;
            out.push(b);
            if in_str {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    in_str = false;
                }
            } else {
                match b {
                    b'"' => in_str = true,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    } else {
        // A bare scalar (number / string / true / false / null).
        while let Some(b) = peek_byte(reader)? {
            if in_str {
                out.push(b);
                read_byte(reader)?;
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    in_str = false;
                }
            } else if b == b',' || b == b']' || b.is_ascii_whitespace() {
                break;
            } else {
                if b == b'"' {
                    in_str = true;
                }
                out.push(b);
                read_byte(reader)?;
            }
        }
    }
    Ok(out)
}

/// A minimal RFC 4180 record reader. Reads whole lines and, when a line ends inside
/// an open quote (odd number of `"` so far; doubled `""` escapes count as two and
/// stay even), keeps appending lines until the quotes balance, so a field with an
/// embedded newline isn't split across records.
struct CsvReader<R: BufRead> {
    reader: R,
    done: bool,
}

impl<R: BufRead> CsvReader<R> {
    fn next_record(&mut self) -> io::Result<Option<Vec<String>>> {
        if self.done {
            return Ok(None);
        }
        let mut buf = String::new();
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                self.done = true;
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            buf.push_str(&line);
            // Complete once quotes balance (an open quote means an embedded newline).
            if buf.matches('"').count().is_multiple_of(2) {
                break;
            }
        }
        let record = buf.trim_end_matches(['\r', '\n']);
        Ok(Some(parse_csv_record(record)))
    }
}

/// Split one complete CSV record into fields, honoring `"`-quoting and doubled `""`.
fn parse_csv_record(s: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut chars = s.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => fields.push(std::mem::take(&mut field)),
                _ => field.push(c),
            }
        }
    }
    fields.push(field);
    fields
}

/// Read the next non-blank line (trimming the trailing newline), skipping empty
/// lines (a trailing blank line in a JSONL file isn't an empty record). `None` at
/// EOF.
fn read_nonempty_line<R: BufRead>(reader: &mut R) -> io::Result<Option<String>> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_string()));
        }
    }
}

fn parse_json_object(line: &str) -> io::Result<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "JSONL line is not a JSON object",
        )),
        Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
    }
}

/// Project a JSON object onto `columns`, rendering each cell as text: a string
/// verbatim, `null`/missing as `""`, a number/bool via its display form, and a
/// nested object/array stringified (so it lands in a text column, never explodes the
/// schema).
fn project_json(
    obj: &serde_json::Map<String, serde_json::Value>,
    columns: &[String],
) -> Vec<String> {
    columns
        .iter()
        .map(|key| match obj.get(key) {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Bool(b)) => b.to_string(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(other) => other.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(format: ImportFormat, data: &str) -> (Vec<String>, Vec<Vec<String>>) {
        let (cols, mut reader) = ImportReader::begin(data.as_bytes(), format).unwrap();
        let mut out = Vec::new();
        while let Some(row) = reader.next_row().unwrap() {
            out.push(row);
        }
        (cols, out)
    }

    #[test]
    fn csv_header_and_simple_rows() {
        let (cols, data) = rows(ImportFormat::Csv, "id,name\n1,alice\n2,bob\n");
        assert_eq!(cols, vec!["id", "name"]);
        assert_eq!(data, vec![vec!["1", "alice"], vec!["2", "bob"]]);
    }

    #[test]
    fn csv_quotes_commas_and_doubled_quotes() {
        let (_, data) = rows(
            ImportFormat::Csv,
            "id,note\n1,\"a, b\"\n2,\"she said \"\"hi\"\"\"\n",
        );
        assert_eq!(data[0], vec!["1", "a, b"]);
        assert_eq!(data[1], vec!["2", "she said \"hi\""]);
    }

    #[test]
    fn csv_embedded_newline_in_quoted_field() {
        let (_, data) = rows(ImportFormat::Csv, "id,note\n1,\"line1\nline2\"\n");
        assert_eq!(
            data,
            vec![vec!["1".to_string(), "line1\nline2".to_string()]]
        );
    }

    #[test]
    fn csv_empty_field_is_empty_string() {
        let (_, data) = rows(ImportFormat::Csv, "id,name\n1,\n");
        assert_eq!(data, vec![vec!["1", ""]]);
    }

    #[test]
    fn csv_no_trailing_newline() {
        let (cols, data) = rows(ImportFormat::Csv, "a,b\n1,2");
        assert_eq!(cols, vec!["a", "b"]);
        assert_eq!(data, vec![vec!["1", "2"]]);
    }

    #[test]
    fn jsonl_keys_and_values() {
        let (cols, data) = rows(
            ImportFormat::Jsonl,
            "{\"id\":1,\"name\":\"alice\"}\n{\"id\":2,\"name\":\"bob\"}\n",
        );
        assert_eq!(cols, vec!["id", "name"]);
        assert_eq!(data[0], vec!["1", "alice"]);
        assert_eq!(data[1], vec!["2", "bob"]);
    }

    #[test]
    fn jsonl_null_missing_and_nested() {
        // First object sets the columns; later rows missing a key → "", null → "",
        // nested object → stringified.
        let (cols, data) = rows(
            ImportFormat::Jsonl,
            "{\"id\":1,\"meta\":{\"a\":1}}\n{\"id\":2,\"meta\":null}\n{\"id\":3}\n",
        );
        assert_eq!(cols, vec!["id", "meta"]);
        assert_eq!(data[0], vec!["1".to_string(), "{\"a\":1}".to_string()]);
        assert_eq!(data[1], vec!["2".to_string(), "".to_string()]);
        assert_eq!(data[2], vec!["3".to_string(), "".to_string()]);
    }

    #[test]
    fn jsonl_skips_blank_lines() {
        let (_, data) = rows(ImportFormat::Jsonl, "{\"id\":1}\n\n{\"id\":2}\n");
        assert_eq!(data.len(), 2);
    }

    #[test]
    fn empty_sources_yield_nothing() {
        assert_eq!(rows(ImportFormat::Csv, "").1.len(), 0);
        assert_eq!(rows(ImportFormat::Jsonl, "").1.len(), 0);
        assert_eq!(rows(ImportFormat::JsonArray, "").1.len(), 0);
        assert_eq!(rows(ImportFormat::JsonArray, "[]").1.len(), 0);
        assert_eq!(rows(ImportFormat::JsonArray, "  [ ]  ").1.len(), 0);
    }

    #[test]
    fn json_array_keys_and_values() {
        let (cols, data) = rows(
            ImportFormat::JsonArray,
            "[{\"id\":1,\"name\":\"alice\"},{\"id\":2,\"name\":\"bob\"}]",
        );
        assert_eq!(cols, vec!["id", "name"]);
        assert_eq!(data[0], vec!["1", "alice"]);
        assert_eq!(data[1], vec!["2", "bob"]);
    }

    #[test]
    fn json_array_pretty_printed_with_nested_and_null() {
        // Whitespace/newlines between elements, a nested object (stringified), a
        // null and a missing key (both ""), and commas/braces inside string values
        // must not confuse the element scanner.
        let src = "[\n  { \"id\": 1, \"meta\": {\"a\": 1, \"b\": [2, 3]}, \"note\": \"x, }y\" },\n  { \"id\": 2, \"meta\": null },\n  { \"id\": 3 }\n]\n";
        let (cols, data) = rows(ImportFormat::JsonArray, src);
        assert_eq!(cols, vec!["id", "meta", "note"]);
        assert_eq!(
            data[0],
            vec!["1".to_string(), "{\"a\":1,\"b\":[2,3]}".to_string(), "x, }y".to_string()]
        );
        assert_eq!(data[1], vec!["2".to_string(), "".to_string(), "".to_string()]);
        assert_eq!(data[2], vec!["3".to_string(), "".to_string(), "".to_string()]);
    }

    #[test]
    fn json_array_rejects_non_array_root() {
        // begin() reads the first element eagerly to learn columns, so a non-array
        // root (or a non-object element) fails there.
        assert!(ImportReader::begin("{\"id\":1}".as_bytes(), ImportFormat::JsonArray).is_err());
        assert!(ImportReader::begin("[1, 2, 3]".as_bytes(), ImportFormat::JsonArray).is_err());
    }
}
