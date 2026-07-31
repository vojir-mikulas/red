//! The `.xlsx` container, hand-rolled: CRC-32, a STORED-entry ZIP writer, and
//! the five XML parts Excel and LibreOffice both accept.
//!
//! No `zip` or `flate2` dependency, the same call the project already made for
//! the binary decoders. A ZIP entry may be *stored* rather than deflated and
//! every reader accepts it, which reduces the container to a local header per
//! part, the bytes, a central directory, and an end-of-central-directory record.
//! The cost is file size: an uncompressed XLSX is several times the CSV of the
//! same data, which the export dialog says up front.
//!
//! Two decisions carry the correctness of this module:
//!
//! - **The sheet is spooled to a temp file, not buffered.** A ZIP local header
//!   carries its entry's CRC and length *before* the bytes, and the row count is
//!   not known in advance. Buffering the sheet would violate the never-
//!   materialize rule, and data descriptors are legal but historically flaky in
//!   Excel; spooling keeps memory flat and the archive strictly conformant.
//! - **Cell type comes from the [`Value`] variant, never from parsing the text.**
//!   A numeric *string* -- a leading-zero account number, an id past 2^53 --
//!   stays a string. Inferring here is how a spreadsheet eats data, which is
//!   worse than not shipping the format.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use red_core::Value;

/// Excel's hard limit on rows per sheet, header included. Past this the export
/// stops and reports the truncation; silently dropping rows is the one
/// unacceptable behaviour here.
pub(crate) const XLSX_MAX_ROWS: u64 = 1_048_576;

/// Excel's hard limit on columns per sheet. A result this wide does not exist in
/// practice, so it errors rather than truncating.
pub(crate) const XLSX_MAX_COLS: usize = 16_384;

// --- CRC-32 (IEEE 802.3), the one checksum a ZIP entry needs ---

/// The reversed IEEE polynomial, as the table build uses it.
const CRC_POLY: u32 = 0xEDB8_8320;

/// Running CRC-32 over a byte stream, so the sheet's checksum is computed as it
/// spools rather than by re-reading the file.
#[derive(Debug)]
pub(crate) struct Crc32 {
    state: u32,
}

impl Crc32 {
    pub(crate) fn new() -> Crc32 {
        Crc32 { state: 0xFFFF_FFFF }
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        let table = crc_table();
        for &b in bytes {
            let idx = ((self.state ^ u32::from(b)) & 0xFF) as usize;
            self.state = (self.state >> 8) ^ table[idx];
        }
    }

    pub(crate) fn finish(&self) -> u32 {
        self.state ^ 0xFFFF_FFFF
    }
}

/// The 256-entry lookup table, built once.
fn crc_table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    CRC_POLY ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        table
    })
}

/// CRC-32 of a complete buffer.
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finish()
}

// --- the ZIP container ---

/// A fixed MS-DOS timestamp (1980-01-01 00:00) for every entry.
///
/// Deliberate: a real clock would make two exports of the same result differ
/// byte for byte, which costs a reproducible test and buys a date nobody reads
/// out of a spreadsheet's archive metadata.
const DOS_TIME: u16 = 0;
const DOS_DATE: u16 = 0x0021;

/// One entry already written, kept for the central directory.
struct ZipEntryMeta {
    name: &'static str,
    crc: u32,
    size: u32,
    offset: u32,
}

/// Assembles a ZIP of STORED entries into `out`, tracking what the central
/// directory will need.
struct ZipWriter<W: Write> {
    out: W,
    offset: u32,
    entries: Vec<ZipEntryMeta>,
}

impl<W: Write> ZipWriter<W> {
    fn new(out: W) -> ZipWriter<W> {
        ZipWriter {
            out,
            offset: 0,
            entries: Vec::new(),
        }
    }

    /// Write a whole in-memory part. Used for the four small XML parts, whose
    /// size is known the moment they are built.
    fn add(&mut self, name: &'static str, body: &[u8]) -> io::Result<()> {
        let size = zip_size(body.len())?;
        self.header(name, crc32(body), size)?;
        self.out.write_all(body)?;
        self.offset = advance(self.offset, size)?;
        Ok(())
    }

    /// Write an entry whose bytes come from `reader`, with the CRC and length
    /// already known (the spooled sheet). Streams in fixed-size chunks so a
    /// multi-gigabyte sheet never lands in memory.
    fn add_streamed(
        &mut self,
        name: &'static str,
        crc: u32,
        size: u32,
        reader: &mut impl Read,
    ) -> io::Result<()> {
        self.header(name, crc, size)?;
        let copied = io::copy(reader, &mut self.out)?;
        if copied != u64::from(size) {
            return Err(io::Error::other(format!(
                "sheet spool changed size while writing ({copied} bytes, expected {size})"
            )));
        }
        self.offset = advance(self.offset, size)?;
        Ok(())
    }

    /// The local file header, common to both entry kinds.
    fn header(&mut self, name: &'static str, crc: u32, size: u32) -> io::Result<()> {
        let name_len = u16::try_from(name.len()).map_err(|_| io::Error::other("entry name"))?;
        self.entries.push(ZipEntryMeta {
            name,
            crc,
            size,
            offset: self.offset,
        });
        self.out.write_all(&0x0403_4B50u32.to_le_bytes())?; // local file header
        self.out.write_all(&20u16.to_le_bytes())?; // version needed
        self.out.write_all(&0u16.to_le_bytes())?; // flags
        self.out.write_all(&0u16.to_le_bytes())?; // method: stored
        self.out.write_all(&DOS_TIME.to_le_bytes())?;
        self.out.write_all(&DOS_DATE.to_le_bytes())?;
        self.out.write_all(&crc.to_le_bytes())?;
        self.out.write_all(&size.to_le_bytes())?; // compressed
        self.out.write_all(&size.to_le_bytes())?; // uncompressed
        self.out.write_all(&name_len.to_le_bytes())?;
        self.out.write_all(&0u16.to_le_bytes())?; // extra field length
        self.out.write_all(name.as_bytes())?;
        self.offset = advance(self.offset, 30 + u32::from(name_len))?;
        Ok(())
    }

    /// The central directory and the end-of-central-directory record.
    fn finish(mut self) -> io::Result<W> {
        let dir_start = self.offset;
        for e in &self.entries {
            let name_len =
                u16::try_from(e.name.len()).map_err(|_| io::Error::other("entry name"))?;
            self.out.write_all(&0x0201_4B50u32.to_le_bytes())?; // central directory header
            self.out.write_all(&20u16.to_le_bytes())?; // version made by
            self.out.write_all(&20u16.to_le_bytes())?; // version needed
            self.out.write_all(&0u16.to_le_bytes())?; // flags
            self.out.write_all(&0u16.to_le_bytes())?; // method: stored
            self.out.write_all(&DOS_TIME.to_le_bytes())?;
            self.out.write_all(&DOS_DATE.to_le_bytes())?;
            self.out.write_all(&e.crc.to_le_bytes())?;
            self.out.write_all(&e.size.to_le_bytes())?;
            self.out.write_all(&e.size.to_le_bytes())?;
            self.out.write_all(&name_len.to_le_bytes())?;
            self.out.write_all(&0u16.to_le_bytes())?; // extra
            self.out.write_all(&0u16.to_le_bytes())?; // comment
            self.out.write_all(&0u16.to_le_bytes())?; // disk number
            self.out.write_all(&0u16.to_le_bytes())?; // internal attrs
            self.out.write_all(&0u32.to_le_bytes())?; // external attrs
            self.out.write_all(&e.offset.to_le_bytes())?;
            self.out.write_all(e.name.as_bytes())?;
            self.offset = advance(self.offset, 46 + u32::from(name_len))?;
        }
        let count = u16::try_from(self.entries.len()).map_err(|_| io::Error::other("entries"))?;
        self.out.write_all(&0x0605_4B50u32.to_le_bytes())?; // end of central directory
        self.out.write_all(&0u16.to_le_bytes())?; // this disk
        self.out.write_all(&0u16.to_le_bytes())?; // disk with directory
        self.out.write_all(&count.to_le_bytes())?;
        self.out.write_all(&count.to_le_bytes())?;
        self.out
            .write_all(&(self.offset - dir_start).to_le_bytes())?;
        self.out.write_all(&dir_start.to_le_bytes())?;
        self.out.write_all(&0u16.to_le_bytes())?; // comment length
        Ok(self.out)
    }
}

/// A part's size as the ZIP32 field it has to fit in. A sheet past 4 GiB would
/// silently wrap, so it errors instead of producing an archive that opens as
/// "unreadable content".
fn zip_size(len: usize) -> io::Result<u32> {
    u32::try_from(len).map_err(|_| {
        io::Error::other(
            "the sheet exceeds 4 GiB, which this ZIP container cannot address; export CSV instead",
        )
    })
}

/// Advance a running archive offset, erroring rather than wrapping past 4 GiB.
fn advance(offset: u32, by: u32) -> io::Result<u32> {
    offset
        .checked_add(by)
        .ok_or_else(|| io::Error::other("the workbook exceeds 4 GiB; export CSV instead"))
}

// --- the fixed XML parts ---

const XML_DECL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;

const CONTENT_TYPES: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
    r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#,
    r#"<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>"#,
    r#"<Default Extension="xml" ContentType="application/xml"/>"#,
    r#"<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>"#,
    r#"<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#,
    "</Types>",
);

const ROOT_RELS: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>"#,
    "</Relationships>",
);

const WORKBOOK_RELS: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>"#,
    "</Relationships>",
);

/// The workbook part, naming the single sheet. `name` is the visible tab label.
fn workbook_xml(name: &str) -> String {
    format!(
        r#"{XML_DECL}<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="{}" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        xlsx_text(name)
    )
}

/// Excel's rules for a sheet tab: 31 characters, and none of `[ ] : * ? / \`.
/// An unusable name falls back rather than producing a workbook Excel refuses.
fn sheet_name(stem: &str) -> String {
    let cleaned: String = stem
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\'))
        .take(31)
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "Sheet1".to_string()
    } else {
        cleaned
    }
}

// --- the sheet ---

/// One in-progress sheet: the spool holding its XML, and the checksum/length the
/// ZIP header will need. Dropping it removes the spool, so a cancelled export
/// leaves nothing behind.
#[derive(Debug)]
pub(crate) struct XlsxSheet {
    spool_path: PathBuf,
    /// `None` once [`finish`](Self::finish) has taken it.
    spool: Option<BufWriter<File>>,
    crc: Crc32,
    len: u64,
    /// Rows written to the sheet, header included, for the row-limit check.
    rows: u64,
    /// Set once the row limit is reached; further rows are refused so the caller
    /// can report the truncation rather than write a file Excel rejects.
    full: bool,
    sheet_name: String,
}

impl XlsxSheet {
    /// Begin a sheet for a workbook destined for `dest`, with `names` as the
    /// header row.
    ///
    /// # Errors
    ///
    /// Fails if the spool cannot be created, or if the result is wider than
    /// [`XLSX_MAX_COLS`] -- a width no real result has, and one that would
    /// otherwise produce a workbook Excel refuses to open.
    pub(crate) fn begin(dest: &Path, names: &[String]) -> io::Result<XlsxSheet> {
        if names.len() > XLSX_MAX_COLS {
            return Err(io::Error::other(format!(
                "{} columns exceeds Excel's limit of {XLSX_MAX_COLS}; export CSV instead",
                names.len()
            )));
        }
        let spool_path = spool_path_for(dest);
        // Read/write, not `File::create`: `finish` seeks back to the start and
        // streams the sheet into the archive through this same handle.
        let spool = BufWriter::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&spool_path)?,
        );
        let mut sheet = XlsxSheet {
            spool_path,
            spool: Some(spool),
            crc: Crc32::new(),
            len: 0,
            rows: 0,
            full: false,
            sheet_name: sheet_name(
                dest.file_stem()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default()
                    .as_ref(),
            ),
        };
        sheet.push(XML_DECL)?;
        sheet.push(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
        )?;
        // The header row is a row like any other, and counts against the limit.
        let header: Vec<Value> = names
            .iter()
            .map(|n| Value::Text(n.clone().into()))
            .collect();
        sheet.write_row(&header)?;
        Ok(sheet)
    }

    /// Whether the sheet has reached Excel's row limit. The caller reports this;
    /// [`write_row`](Self::write_row) simply stops accepting rows.
    pub(crate) fn truncated(&self) -> bool {
        self.full
    }

    /// Append one row. Silently a no-op once the row limit is reached -- silent
    /// only here, because [`truncated`](Self::truncated) is what the export
    /// surfaces to the user.
    pub(crate) fn write_row(&mut self, cells: &[Value]) -> io::Result<()> {
        if self.full {
            return Ok(());
        }
        let r = self.rows + 1;
        let mut row = format!("<row r=\"{r}\">");
        for (i, value) in cells.iter().take(XLSX_MAX_COLS).enumerate() {
            push_cell(&mut row, &col_ref(i), r, value);
        }
        row.push_str("</row>");
        self.push(&row)?;
        self.rows = r;
        self.full = self.rows >= XLSX_MAX_ROWS;
        Ok(())
    }

    /// Close the sheet and write the whole workbook into `out`, streaming the
    /// spool back through. Removes the spool on the way out.
    pub(crate) fn finish<W: Write>(mut self, out: W) -> io::Result<()> {
        self.push("</sheetData></worksheet>")?;
        let Some(spool) = self.spool.take() else {
            return Err(io::Error::other("the sheet spool was already taken"));
        };
        let mut file = spool.into_inner().map_err(io::Error::other)?;
        file.flush()?;
        file.seek(SeekFrom::Start(0))?;

        let size = zip_size(usize::try_from(self.len).unwrap_or(usize::MAX))?;
        let mut zip = ZipWriter::new(out);
        zip.add("[Content_Types].xml", CONTENT_TYPES.as_bytes())?;
        zip.add("_rels/.rels", ROOT_RELS.as_bytes())?;
        zip.add("xl/workbook.xml", workbook_xml(&self.sheet_name).as_bytes())?;
        zip.add("xl/_rels/workbook.xml.rels", WORKBOOK_RELS.as_bytes())?;
        zip.add_streamed(
            "xl/worksheets/sheet1.xml",
            self.crc.finish(),
            size,
            &mut file,
        )?;
        let mut out = zip.finish()?;
        out.flush()?;
        drop(file);
        let _ = std::fs::remove_file(&self.spool_path);
        Ok(())
    }

    /// Append text to the spool, keeping the checksum and length in step.
    fn push(&mut self, text: &str) -> io::Result<()> {
        let Some(spool) = self.spool.as_mut() else {
            return Err(io::Error::other("the sheet spool was already taken"));
        };
        spool.write_all(text.as_bytes())?;
        self.crc.update(text.as_bytes());
        self.len += text.len() as u64;
        Ok(())
    }
}

impl Drop for XlsxSheet {
    fn drop(&mut self) {
        // A cancelled or failed export drops the writer; the spool must not
        // outlive it (`CancelExport` promises no partial file is left behind).
        if self.spool.is_some() {
            let _ = std::fs::remove_file(&self.spool_path);
        }
    }
}

/// Where to spool a workbook's sheet: beside the destination, so the spool lands
/// on the same filesystem and inherits its free space, with the process id in the
/// name so two exports never share one.
fn spool_path_for(dest: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = dest.as_os_str().to_os_string();
    name.push(format!(".red-sheet-{}-{n}", std::process::id()));
    PathBuf::from(name)
}

/// Append one `<c>` element for `value` at `col`/`row`.
///
/// The type comes from the [`Value`] variant and nothing else. A `Value::Text`
/// that happens to look numeric stays an inline string, which is the whole
/// reason a leading-zero account number survives the round trip.
fn push_cell(out: &mut String, col: &str, row: u64, value: &Value) {
    match value {
        // An omitted cell is an empty cell. Writing the text "NULL" would turn
        // absence into data.
        Value::Null => {}
        Value::Integer(n) => out.push_str(&format!("<c r=\"{col}{row}\"><v>{n}</v></c>")),
        // A non-finite float has no numeric spelling a spreadsheet accepts, so it
        // rides as text rather than producing a file Excel refuses.
        Value::Real(x) if x.is_finite() => {
            out.push_str(&format!("<c r=\"{col}{row}\"><v>{x}</v></c>"));
        }
        Value::Real(x) => push_inline(out, col, row, &x.to_string()),
        Value::Text(s) => push_inline(out, col, row, s),
        Value::Blob(b) => push_inline(out, col, row, &format!("<{} bytes>", b.len())),
        // A capped cell renders exactly as it does in CSV/JSON/HTML: the head
        // plus its marker, never the missing tail dressed as the whole value.
        Value::Capped(_) => push_inline(out, col, row, &value.to_string()),
    }
}

fn push_inline(out: &mut String, col: &str, row: u64, text: &str) {
    out.push_str(&format!(
        "<c r=\"{col}{row}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",
        xlsx_text(text)
    ));
}

/// A spreadsheet column reference: `A`, `B`, … `Z`, `AA`, `AB`, …
fn col_ref(index: usize) -> String {
    let mut n = index;
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (n % 26) as u8);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    out.reverse();
    String::from_utf8_lossy(&out).into_owned()
}

/// Escape text for an XML element body, to SpreadsheetML's rules.
///
/// The five entities are the easy half. The half that decides whether the file
/// opens at all: XML 1.0 forbids control characters other than tab, LF and CR,
/// and a stray `\x00` in a `TEXT` column is common in real data. Those become
/// Excel's own `_xHHHH_` escape -- which in turn means a literal `_x0041_` in
/// the data must have its underscore escaped, or it would round-trip as `A`.
pub(crate) fn xlsx_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '_' if looks_like_escape(&chars[i..]) => out.push_str("_x005F_"),
            '\t' | '\n' | '\r' => out.push(c),
            // Control characters and the two noncharacters XML rejects outright.
            c if (c as u32) < 0x20 || matches!(c as u32, 0xFFFE | 0xFFFF) => {
                out.push_str(&format!("_x{:04X}_", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Whether `rest` starts with a literal `_xHHHH_` sequence, which Excel would
/// otherwise decode back into a character on read.
fn looks_like_escape(rest: &[char]) -> bool {
    matches!(rest, ['_', 'x', a, b, c, d, '_', ..]
        if a.is_ascii_hexdigit() && b.is_ascii_hexdigit()
            && c.is_ascii_hexdigit() && d.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_standard_check_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        // Incremental and one-shot agree.
        let mut inc = Crc32::new();
        inc.update(b"12345");
        inc.update(b"6789");
        assert_eq!(inc.finish(), 0xCBF4_3926);
    }

    #[test]
    fn column_refs_carry_past_z() {
        assert_eq!(col_ref(0), "A");
        assert_eq!(col_ref(25), "Z");
        assert_eq!(col_ref(26), "AA");
        assert_eq!(col_ref(27), "AB");
        assert_eq!(col_ref(51), "AZ");
        assert_eq!(col_ref(52), "BA");
        assert_eq!(col_ref(701), "ZZ");
        assert_eq!(col_ref(702), "AAA");
        // The last column Excel accepts.
        assert_eq!(col_ref(XLSX_MAX_COLS - 1), "XFD");
    }

    #[test]
    fn text_escapes_the_five_entities() {
        assert_eq!(xlsx_text("a & b"), "a &amp; b");
        assert_eq!(xlsx_text("<script>"), "&lt;script&gt;");
        assert_eq!(xlsx_text(r#"say "hi""#), "say &quot;hi&quot;");
        assert_eq!(xlsx_text("O'Brien"), "O&apos;Brien");
    }

    /// The escaping that decides whether Excel opens the file at all.
    #[test]
    fn text_encodes_control_characters_and_their_literal_form() {
        // A stray NUL in a TEXT column must not reach the XML.
        assert_eq!(xlsx_text("a\u{0}b"), "a_x0000_b");
        assert_eq!(xlsx_text("\u{1}\u{1f}"), "_x0001__x001F_");
        // Tab, LF and CR are legal XML and stay themselves.
        assert_eq!(xlsx_text("a\tb\nc\rd"), "a\tb\nc\rd");
        // A literal `_x0041_` must not round-trip as `A`.
        assert_eq!(xlsx_text("_x0041_"), "_x005F_x0041_");
        // A bare underscore, and one not followed by an escape, are untouched.
        assert_eq!(xlsx_text("snake_case"), "snake_case");
        assert_eq!(xlsx_text("_xZZZZ_"), "_xZZZZ_");
    }

    /// The failure mode this whole module is careful about: a numeric-looking
    /// string stays a string, so an account number keeps its leading zero.
    #[test]
    fn cell_type_follows_the_value_variant_never_the_text() {
        let cell = |v: &Value| {
            let mut s = String::new();
            push_cell(&mut s, "A", 1, v);
            s
        };
        assert_eq!(cell(&Value::Integer(42)), "<c r=\"A1\"><v>42</v></c>");
        assert_eq!(cell(&Value::Real(1.5)), "<c r=\"A1\"><v>1.5</v></c>");
        // A numeric *string* is an inline string, not a number.
        assert!(cell(&Value::Text("007".into())).contains("t=\"inlineStr\""));
        assert!(cell(&Value::Text("007".into())).contains(">007<"));
        // A NULL is an absent cell, not the word NULL.
        assert_eq!(cell(&Value::Null), "");
        // A blob is the same length marker CSV and JSON write.
        assert!(cell(&Value::Blob(vec![0; 5])).contains("&lt;5 bytes&gt;"));
        // A non-finite float has no numeric spelling; it rides as text.
        assert!(cell(&Value::Real(f64::NAN)).contains("t=\"inlineStr\""));
    }

    #[test]
    fn sheet_names_are_cleaned_to_excels_rules() {
        assert_eq!(sheet_name("orders"), "orders");
        assert_eq!(sheet_name("a/b:c*d?e[f]g\\h"), "abcdefgh");
        assert_eq!(sheet_name(""), "Sheet1");
        assert_eq!(sheet_name("   "), "Sheet1");
        assert_eq!(sheet_name(&"x".repeat(50)).len(), 31);
    }

    /// Parse the archive back by hand rather than shelling out: the central
    /// directory is where a malformed ZIP shows up, and a test that reads it is
    /// the one that catches an off-by-one in the offsets.
    fn zip_entries(bytes: &[u8]) -> Vec<(String, u32, u32, u32)> {
        let eocd = bytes
            .windows(4)
            .rposition(|w| w == 0x0605_4B50u32.to_le_bytes())
            .expect("end-of-central-directory record");
        let le16 = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let le32 = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let count = le16(eocd + 10) as usize;
        let mut at = le32(eocd + 16) as usize;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            assert_eq!(le32(at), 0x0201_4B50, "central directory header");
            let crc = le32(at + 16);
            let size = le32(at + 20);
            let name_len = le16(at + 28) as usize;
            let offset = le32(at + 42);
            let name = String::from_utf8(bytes[at + 46..at + 46 + name_len].to_vec()).unwrap();
            out.push((name, crc, size, offset));
            at += 46 + name_len;
        }
        out
    }

    fn write_workbook(dir: &Path, rows: &[Vec<Value>]) -> Vec<u8> {
        let dest = dir.join("book.xlsx");
        let mut sheet = XlsxSheet::begin(&dest, &["id".into(), "name".into()]).unwrap();
        for row in rows {
            sheet.write_row(row).unwrap();
        }
        let mut out: Vec<u8> = Vec::new();
        sheet.finish(&mut out).unwrap();
        out
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("red_xlsx_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_workbook_parses_back_as_five_stored_entries() {
        let dir = scratch("parse");
        let bytes = write_workbook(
            &dir,
            &[
                vec![Value::Integer(1), Value::Text("Ada".into())],
                vec![Value::Integer(2), Value::Null],
            ],
        );
        let entries = zip_entries(&bytes);
        let names: Vec<&str> = entries.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "[Content_Types].xml",
                "_rels/.rels",
                "xl/workbook.xml",
                "xl/_rels/workbook.xml.rels",
                "xl/worksheets/sheet1.xml",
            ]
        );
        // Every entry's recorded CRC and size match the bytes actually stored at
        // its offset: the check that catches a wrong header length.
        for (name, crc, size, offset) in &entries {
            let at = *offset as usize;
            assert_eq!(
                u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()),
                0x0403_4B50,
                "{name} local header"
            );
            let name_len = u16::from_le_bytes(bytes[at + 26..at + 28].try_into().unwrap()) as usize;
            let body_at = at + 30 + name_len;
            let body = &bytes[body_at..body_at + *size as usize];
            assert_eq!(crc32(body), *crc, "{name} checksum");
        }
        // The sheet holds the header row plus the data, with the null omitted.
        let sheet = entries
            .iter()
            .find(|e| e.0.ends_with("sheet1.xml"))
            .unwrap();
        let at = sheet.3 as usize;
        let name_len = u16::from_le_bytes(bytes[at + 26..at + 28].try_into().unwrap()) as usize;
        let body_at = at + 30 + name_len;
        let xml = std::str::from_utf8(&bytes[body_at..body_at + sheet.2 as usize]).unwrap();
        assert!(xml.contains(">id<") && xml.contains(">name<"), "header row");
        assert!(xml.contains("<c r=\"A2\"><v>1</v></c>"));
        assert!(xml.contains(">Ada<"));
        // Row 3's NULL cell is absent entirely.
        assert!(xml.contains("<row r=\"3\"><c r=\"A3\"><v>2</v></c></row>"));
        assert!(xml.ends_with("</sheetData></worksheet>"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_spool_is_removed_whether_the_export_finishes_or_is_dropped() {
        let dir = scratch("spool");
        let dest = dir.join("book.xlsx");
        let sheet = XlsxSheet::begin(&dest, &["a".into()]).unwrap();
        let spool = sheet.spool_path.clone();
        assert!(spool.exists());
        // A cancelled export drops the writer; nothing may be left behind.
        drop(sheet);
        assert!(!spool.exists(), "a dropped sheet removes its spool");

        let mut sheet = XlsxSheet::begin(&dest, &["a".into()]).unwrap();
        let spool = sheet.spool_path.clone();
        sheet.write_row(&[Value::Integer(1)]).unwrap();
        sheet.finish(&mut Vec::new()).unwrap();
        assert!(!spool.exists(), "a finished sheet removes its spool");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Not a test: a `#[ignore]`d hook that writes a workbook to a caller-named
    /// path so a human can open it in Excel/LibreOffice/Numbers. A format nobody
    /// opened is not a shipped format.
    #[test]
    #[ignore = "writes a file for manual inspection; run with RED_XLSX_PROBE=<path>"]
    fn write_a_workbook_for_manual_inspection() {
        let Ok(dest) = std::env::var("RED_XLSX_PROBE") else {
            return;
        };
        let dest = std::path::PathBuf::from(dest);
        let mut sheet = XlsxSheet::begin(
            &dest,
            &[
                "id".into(),
                "account".into(),
                "amount".into(),
                "note".into(),
                "raw".into(),
                "empty".into(),
            ],
        )
        .unwrap();
        sheet
            .write_row(&[
                Value::Integer(1),
                Value::Text("007".into()),
                Value::Real(12.5),
                Value::Text("a & b <tag> 'q'".into()),
                Value::Blob(vec![0, 255]),
                Value::Null,
            ])
            .unwrap();
        sheet
            .write_row(&[
                Value::Integer(2),
                Value::Text("0080".into()),
                Value::Real(-3.25),
                Value::Text("a\u{0}b\ttab".into()),
                Value::Blob(vec![1, 2, 3, 4, 5]),
                Value::Null,
            ])
            .unwrap();
        let out = std::fs::File::create(&dest).unwrap();
        sheet.finish(std::io::BufWriter::new(out)).unwrap();
    }

    #[test]
    fn a_result_wider_than_excel_errors_rather_than_truncating() {
        let dir = scratch("wide");
        let names: Vec<String> = (0..XLSX_MAX_COLS + 1).map(|i| i.to_string()).collect();
        let err = XlsxSheet::begin(&dir.join("wide.xlsx"), &names).unwrap_err();
        assert!(err.to_string().contains("Excel's limit"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
