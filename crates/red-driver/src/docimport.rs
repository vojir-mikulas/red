//! Streaming document readers for import into a collection, the read-side mirror of
//! [`docexport`](crate::docexport). Engine-independent by construction: every source
//! is normalized to one **extended-JSON object per document**, which the target
//! driver then parses with its own dialect (`DocDriver::parse_ext_json`). That is
//! what keeps `$oid` / `$date` / `$numberDecimal` typed on the way in without this
//! module knowing what BSON is.
//!
//! Holds one document at a time, never the file: the JSON-array arm reuses the
//! element scanner the SQL import already streams with, NDJSON reads line by line,
//! and CSV reads record by record.

use std::io::{self, BufRead};

use red_core::doc::DocImportFormat;

use crate::import::{
    CsvReader, invalid, peek_byte, read_byte, read_json_value_bytes, read_nonempty_line, skip_ws,
};

/// A streaming reader that yields one document's extended-JSON text at a time.
pub struct DocImportReader<R: BufRead> {
    inner: Inner<R>,
}

enum Inner<R: BufRead> {
    /// One top-level JSON array of objects, scanned element by element.
    JsonArray {
        reader: R,
        array_started: bool,
        first_element: bool,
        done: bool,
    },
    /// One JSON object per line.
    Ndjson(R),
    Csv {
        reader: CsvReader<R>,
        /// The dotted field paths from the header record.
        header: Vec<String>,
    },
}

impl<R: BufRead> DocImportReader<R> {
    /// Open `reader` as `format`, consuming a CSV header if there is one.
    ///
    /// # Errors
    /// Fails when the CSV header cannot be read.
    pub fn begin(reader: R, format: DocImportFormat) -> io::Result<Self> {
        let inner = match format {
            DocImportFormat::Json => Inner::JsonArray {
                reader,
                array_started: false,
                first_element: true,
                done: false,
            },
            DocImportFormat::Ndjson => Inner::Ndjson(reader),
            DocImportFormat::Csv => {
                let mut csv = CsvReader::new(reader);
                let header = csv.next_record()?.unwrap_or_default();
                Inner::Csv {
                    reader: csv,
                    header,
                }
            }
        };
        Ok(Self { inner })
    }

    /// The next document as extended-JSON object text, or `None` at end of source.
    ///
    /// # Errors
    /// Fails on malformed JSON framing, a non-object element, or an IO error.
    pub fn next_document(&mut self) -> io::Result<Option<String>> {
        match &mut self.inner {
            Inner::JsonArray {
                reader,
                array_started,
                first_element,
                done,
            } => next_array_element(reader, array_started, first_element, done),
            Inner::Ndjson(reader) => match read_nonempty_line(reader)? {
                None => Ok(None),
                Some(line) => {
                    let trimmed = line.trim();
                    if !trimmed.starts_with('{') {
                        return Err(invalid("each NDJSON line must be a JSON object"));
                    }
                    Ok(Some(trimmed.to_string()))
                }
            },
            Inner::Csv { reader, header } => match reader.next_record()? {
                None => Ok(None),
                Some(cells) => Ok(Some(csv_record_to_json(header, &cells))),
            },
        }
    }
}

/// Read one element of a top-level JSON array, consuming the `[` / `,` / `]`
/// framing around it. The scanning half is the SQL importer's, so both paths stream
/// a large array identically.
fn next_array_element<R: BufRead>(
    reader: &mut R,
    array_started: &mut bool,
    first_element: &mut bool,
    done: &mut bool,
) -> io::Result<Option<String>> {
    if *done {
        return Ok(None);
    }
    if !*array_started {
        skip_ws(reader)?;
        match peek_byte(reader)? {
            // An empty source is an empty array, not an error: nothing to import.
            None => {
                *done = true;
                return Ok(None);
            }
            Some(b'[') => {
                read_byte(reader)?;
                *array_started = true;
            }
            Some(_) => return Err(invalid("expected '[' at the start of a JSON array")),
        }
    }
    skip_ws(reader)?;
    match peek_byte(reader)? {
        None => return Err(invalid("unterminated JSON array")),
        Some(b']') => {
            read_byte(reader)?;
            *done = true;
            return Ok(None);
        }
        Some(b',') => {
            if *first_element {
                return Err(invalid("unexpected ',' before the first array element"));
            }
            read_byte(reader)?;
            skip_ws(reader)?;
        }
        Some(_) if *first_element => {}
        Some(_) => return Err(invalid("expected ',' or ']' between array elements")),
    }
    let bytes = read_json_value_bytes(reader)?;
    *first_element = false;
    let text =
        String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if !text.trim_start().starts_with('{') {
        return Err(invalid("every array element must be a JSON object"));
    }
    Ok(Some(text))
}

/// Assemble one CSV record into a JSON object, splitting dotted header paths back
/// into nested objects (`user.city` -> `{"user":{"city":…}}`), the inverse of the
/// export's flattening.
///
/// An **empty cell is omitted**, not written as `null`: a document store's absent
/// field and its null field are different things, and a rectangular CSV of a sparse
/// collection is mostly absent fields. A cell that is itself valid JSON (an object,
/// array, number, boolean, or `null`) is embedded as that value, which is what makes
/// an exported CSV round-trip; anything else becomes a JSON string.
fn csv_record_to_json(header: &[String], cells: &[String]) -> String {
    let mut root = JsonNode::default();
    for (path, cell) in header.iter().zip(cells) {
        if cell.is_empty() {
            continue;
        }
        root.insert(path, json_scalar(cell));
    }
    root.render()
}

/// A partially-built JSON object: leaves in insertion order, children keyed by their
/// path segment. Order is preserved so a round-tripped document keeps its columns'
/// order rather than being re-sorted on the way in.
#[derive(Default)]
struct JsonNode {
    /// `(key, rendered JSON value)` for leaves at this level.
    leaves: Vec<(String, String)>,
    children: Vec<(String, JsonNode)>,
}

impl JsonNode {
    fn insert(&mut self, path: &str, value: String) {
        match path.split_once('.') {
            None => self.leaves.push((path.to_string(), value)),
            Some((head, rest)) => {
                if let Some((_, child)) = self.children.iter_mut().find(|(k, _)| k == head) {
                    child.insert(rest, value);
                } else {
                    let mut child = JsonNode::default();
                    child.insert(rest, value);
                    self.children.push((head.to_string(), child));
                }
            }
        }
    }

    fn render(&self) -> String {
        let mut out = String::from("{");
        let mut first = true;
        for (key, value) in &self.leaves {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&json_string(key));
            out.push(':');
            out.push_str(value);
        }
        for (key, child) in &self.children {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&json_string(key));
            out.push(':');
            out.push_str(&child.render());
        }
        out.push('}');
        out
    }
}

/// Render one CSV cell as a JSON value: valid JSON is embedded verbatim (so an
/// exported nested value or an extended-JSON `$oid` survives), everything else is a
/// string.
fn json_scalar(cell: &str) -> String {
    let trimmed = cell.trim();
    let structural = trimmed.starts_with('{') || trimmed.starts_with('[');
    let scalar = matches!(trimmed, "true" | "false" | "null")
        || trimmed.parse::<f64>().is_ok_and(|_| {
            // `parse::<f64>` accepts `inf`/`NaN`, which JSON does not.
            trimmed
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
        });
    if (structural || scalar) && serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return trimmed.to_string();
    }
    json_string(cell)
}

/// A JSON string literal with the minimal escaping.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
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

    fn read_all(source: &str, format: DocImportFormat) -> Vec<String> {
        let mut reader = DocImportReader::begin(source.as_bytes(), format).unwrap();
        let mut out = Vec::new();
        while let Some(doc) = reader.next_document().unwrap() {
            out.push(doc);
        }
        out
    }

    #[test]
    fn json_array_yields_one_object_at_a_time() {
        let docs = read_all(
            r#"[ {"_id": 1, "tags": ["a","b"]}, {"_id": {"$oid": "507f1f77bcf86cd799439011"}} ]"#,
            DocImportFormat::Json,
        );
        assert_eq!(docs.len(), 2);
        assert!(docs[0].contains("\"tags\""));
        // The extended-JSON tag rides through untouched; typing is the driver's.
        assert!(docs[1].contains("$oid"));
        assert!(read_all("[]", DocImportFormat::Json).is_empty());
        assert!(read_all("", DocImportFormat::Json).is_empty());
    }

    #[test]
    fn ndjson_skips_blank_lines_and_rejects_non_objects() {
        let docs = read_all("{\"a\":1}\n\n{\"a\":2}\n", DocImportFormat::Ndjson);
        assert_eq!(docs, vec!["{\"a\":1}", "{\"a\":2}"]);
        let mut reader =
            DocImportReader::begin("[1,2]".as_bytes(), DocImportFormat::Ndjson).unwrap();
        assert!(reader.next_document().is_err());
    }

    #[test]
    fn csv_rebuilds_nested_paths_and_omits_empty_cells() {
        let docs = read_all(
            "_id,user.city,user.age,note\n1,London,30,hello\n2,,,\n",
            DocImportFormat::Csv,
        );
        assert_eq!(
            docs[0],
            r#"{"_id":1,"note":"hello","user":{"city":"London","age":30}}"#
        );
        // An empty cell is an absent field, not a null one.
        assert_eq!(docs[1], r#"{"_id":2}"#);
    }

    #[test]
    fn csv_cells_that_are_json_stay_json() {
        let docs = read_all(
            "_id,tags,meta,label\n1,\"[1,2]\",\"{\"\"k\"\":true}\",007\n",
            DocImportFormat::Csv,
        );
        // Structural cells embed; a leading-zero code is text, not the number 7.
        assert_eq!(
            docs[0],
            r#"{"_id":1,"tags":[1,2],"meta":{"k":true},"label":"007"}"#
        );
    }
}
