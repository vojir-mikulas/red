//! Domain types for the `DocDriver` seam (MongoDB first; see
//! `docs/plans/todo/doc-driver.md`). The third seam, parallel to the SQL-shaped
//! types in `lib.rs` and the Redis-shaped ones in `kv.rs`, for engines that are
//! neither: a `server → databases → collections → documents` hierarchy of nested
//! BSON trees.
//!
//! Nothing here knows about UI, a runtime, or the `mongodb`/`bson` crates:
//! [`DocValue`] is a *mirror* of the BSON value tree, and the conversion between
//! the two lives entirely in `red-driver` (the version firewall, exactly like the
//! SQL and KV families). Extended-JSON rendering is hand-rolled below so this
//! crate stays dependency-light (no `serde_json`).

use std::fmt::Write as _;

use crate::Value;

/// The BSON value tree. Deliberately **not** `serde_json::Value`: BSON carries
/// types JSON loses (`ObjectId`, `Decimal128`, `DateTime`, `Binary` subtypes,
/// `Timestamp`, `Regex`), and preserving them across the read → render → edit
/// round-trip is the whole point of a document store. A [`DocValue::Document`]
/// keeps field order (like BSON), so a document renders the way it was stored.
#[derive(Debug, Clone, PartialEq)]
pub enum DocValue {
    Null,
    Bool(bool),
    Int32(i32),
    Int64(i64),
    Double(f64),
    /// A 128-bit decimal in its canonical string form (e.g. `"1.50"`). Kept as a
    /// string because there is no native Rust `Decimal128` and the canonical
    /// spelling is what both extended-JSON and the grid want.
    Decimal128(String),
    Str(String),
    /// A 12-byte ObjectId, rendered as 24 lowercase hex chars for display.
    ObjectId([u8; 12]),
    /// A UTC datetime as milliseconds since the Unix epoch (BSON's own
    /// representation; can be negative for pre-1970 dates).
    DateTime(i64),
    /// A BSON internal timestamp: high 32 bits are seconds since epoch, low 32
    /// bits an in-second ordinal. Stored as the raw `u64`.
    Timestamp(u64),
    Binary {
        subtype: u8,
        bytes: Vec<u8>,
    },
    Regex {
        pattern: String,
        options: String,
    },
    Array(Vec<DocValue>),
    /// A sub-document; field order preserved, like BSON.
    Document(Vec<(String, DocValue)>),
}

impl DocValue {
    /// Whether this value is a nested tree (`Array`/`Document`) rather than a
    /// scalar. A nested cell renders as expandable extended JSON in the grid;
    /// a scalar renders directly.
    pub fn is_nested(&self) -> bool {
        matches!(self, DocValue::Array(_) | DocValue::Document(_))
    }

    /// The [`DocType`] tag for this value, for the inferred-schema panel's
    /// per-field type distribution.
    pub fn doc_type(&self) -> DocType {
        match self {
            DocValue::Null => DocType::Null,
            DocValue::Bool(_) => DocType::Bool,
            DocValue::Int32(_) => DocType::Int,
            DocValue::Int64(_) => DocType::Long,
            DocValue::Double(_) => DocType::Double,
            DocValue::Decimal128(_) => DocType::Decimal,
            DocValue::Str(_) => DocType::Str,
            DocValue::ObjectId(_) => DocType::ObjectId,
            DocValue::DateTime(_) => DocType::Date,
            DocValue::Timestamp(_) => DocType::Timestamp,
            DocValue::Binary { .. } => DocType::Binary,
            DocValue::Regex { .. } => DocType::Regex,
            DocValue::Array(_) => DocType::Array,
            DocValue::Document(_) => DocType::Object,
        }
    }

    /// A one-word type label for the inferred-schema panel and type hints
    /// (`"string"`, `"objectId"`, …). Matches Mongo's `$type` aliases.
    pub fn type_name(&self) -> &'static str {
        self.doc_type().label()
    }

    /// Render this value as MongoDB **relaxed** extended JSON v2 (the compact,
    /// human-readable spelling: numbers are bare, dates are ISO-8601, only the
    /// JSON-lossy types wrap in a `$`-tagged object). Round-trippable back to a
    /// `DocValue` by the driver's parser, and what the tree widget / `Json` lens
    /// display. Hand-rolled to avoid a `serde_json` dependency in `red-core`.
    pub fn to_extended_json(&self) -> String {
        let mut out = String::new();
        self.write_extjson(&mut out);
        out
    }

    fn write_extjson(&self, out: &mut String) {
        match self {
            DocValue::Null => out.push_str("null"),
            DocValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            DocValue::Int32(n) => {
                let _ = write!(out, "{n}");
            }
            DocValue::Int64(n) => {
                let _ = write!(out, "{n}");
            }
            DocValue::Double(x) => {
                if x.is_finite() {
                    let _ = write!(out, "{x}");
                } else {
                    // Non-finite doubles have no bare-number JSON form; relaxed
                    // extjson keeps the canonical `$numberDouble` wrapper for them.
                    let label = if x.is_nan() {
                        "NaN"
                    } else if *x > 0.0 {
                        "Infinity"
                    } else {
                        "-Infinity"
                    };
                    let _ = write!(out, "{{\"$numberDouble\":\"{label}\"}}");
                }
            }
            DocValue::Decimal128(s) => {
                let _ = write!(out, "{{\"$numberDecimal\":\"{s}\"}}");
            }
            DocValue::Str(s) => write_json_string(out, s),
            DocValue::ObjectId(bytes) => {
                out.push_str("{\"$oid\":\"");
                for b in bytes {
                    let _ = write!(out, "{b:02x}");
                }
                out.push_str("\"}");
            }
            DocValue::DateTime(ms) => match iso8601_utc(*ms) {
                Some(iso) => {
                    let _ = write!(out, "{{\"$date\":\"{iso}\"}}");
                }
                // Out of the ISO-representable range: canonical `$numberLong` form.
                None => {
                    let _ = write!(out, "{{\"$date\":{{\"$numberLong\":\"{ms}\"}}}}");
                }
            },
            DocValue::Timestamp(ts) => {
                let secs = (ts >> 32) as u32;
                let inc = (*ts & 0xffff_ffff) as u32;
                let _ = write!(out, "{{\"$timestamp\":{{\"t\":{secs},\"i\":{inc}}}}}");
            }
            DocValue::Binary { subtype, bytes } => {
                out.push_str("{\"$binary\":{\"base64\":\"");
                base64_encode(out, bytes);
                let _ = write!(out, "\",\"subType\":\"{subtype:02x}\"}}}}");
            }
            DocValue::Regex { pattern, options } => {
                out.push_str("{\"$regularExpression\":{\"pattern\":");
                write_json_string(out, pattern);
                out.push_str(",\"options\":");
                write_json_string(out, options);
                out.push_str("}}");
            }
            DocValue::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_extjson(out);
                }
                out.push(']');
            }
            DocValue::Document(fields) => {
                out.push('{');
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(out, k);
                    out.push(':');
                    v.write_extjson(out);
                }
                out.push('}');
            }
        }
    }

    /// Bridge one value into a grid cell [`Value`]. Scalars map directly; a
    /// nested `Array`/`Document` becomes capped extended-JSON text, displayed
    /// through the tree widget (and the inspector's `Json` lens). `max_bytes` is
    /// the driver's display cap, passed in so `red-core` needn't know it — a
    /// nested cell is still a `Value`, so the fat-cell cap and "Load full" paths
    /// keep working, the same invariant `KvValue::Str → Value::Capped` honors.
    pub fn to_cell(&self, max_bytes: usize) -> Value {
        match self {
            DocValue::Null => Value::Null,
            DocValue::Bool(b) => Value::Text(if *b { "true".into() } else { "false".into() }),
            DocValue::Int32(n) => Value::Integer(*n as i64),
            DocValue::Int64(n) => Value::Integer(*n),
            DocValue::Double(x) => Value::Real(*x),
            DocValue::Decimal128(s) => Value::capped_text(s, max_bytes),
            DocValue::Str(s) => Value::capped_text(s, max_bytes),
            DocValue::ObjectId(bytes) => {
                let mut hex = String::with_capacity(24);
                for b in bytes {
                    let _ = write!(hex, "{b:02x}");
                }
                Value::Text(hex.into())
            }
            DocValue::DateTime(ms) => match iso8601_utc(*ms) {
                Some(iso) => Value::Text(iso.into()),
                None => Value::capped_text(&self.to_extended_json(), max_bytes),
            },
            DocValue::Timestamp(_) => Value::capped_text(&self.to_extended_json(), max_bytes),
            DocValue::Binary { bytes, .. } => Value::capped_blob(bytes.len()),
            DocValue::Regex { pattern, options } => {
                Value::capped_text(&format!("/{pattern}/{options}"), max_bytes)
            }
            DocValue::Array(_) | DocValue::Document(_) => {
                Value::capped_text(&self.to_extended_json(), max_bytes)
            }
        }
    }
}

/// One document (row). `_id` is split out from the rest because the grid and the
/// inspector treat it specially (it's the stable identity for a get/replace/delete
/// and the leftmost grid column), while `fields` keeps every *other* top-level
/// field in stored order.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub id: DocValue,
    pub fields: Vec<(String, DocValue)>,
}

impl Document {
    /// Reconstitute the whole document (including `_id`) as one `DocValue::Document`,
    /// for extended-JSON rendering in the inspector/tree. `_id` leads, as it does
    /// on the wire.
    pub fn to_doc_value(&self) -> DocValue {
        let mut fields = Vec::with_capacity(self.fields.len() + 1);
        fields.push(("_id".to_string(), self.id.clone()));
        fields.extend(self.fields.iter().cloned());
        DocValue::Document(fields)
    }

    /// Split a parsed [`DocValue::Document`] into a [`Document`], pulling out
    /// `_id` (defaulting to [`DocValue::Null`] when absent, as Mongo does on
    /// insert). `None` when `value` isn't a document. The inverse of
    /// [`to_doc_value`](Self::to_doc_value), for the inspector's edit/insert path.
    pub fn from_doc_value(value: DocValue) -> Option<Document> {
        let DocValue::Document(fields) = value else {
            return None;
        };
        let mut id = DocValue::Null;
        let mut rest = Vec::with_capacity(fields.len());
        for (k, v) in fields {
            if k == "_id" {
                id = v;
            } else {
                rest.push((k, v));
            }
        }
        Some(Document { id, fields: rest })
    }
}

/// A database in the catalog (`listDatabases`).
#[derive(Debug, Clone, PartialEq)]
pub struct DbInfo {
    pub name: String,
    pub size_on_disk: u64,
    pub empty: bool,
}

/// What kind of collection an entry in the catalog is. A `View` is read-only
/// (a stored aggregation), a `Timeseries` is Mongo's time-series collection;
/// both render with a badge distinct from a plain `Collection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollKind {
    Collection,
    View,
    Timeseries,
}

/// A collection in the catalog (`listCollections` + `collStats`). Sizes/counts
/// are the server's own estimates (cheap), not an exact scan.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectionInfo {
    pub name: String,
    pub kind: CollKind,
    /// Estimated document count (`estimatedDocumentCount`), O(1) on the server.
    pub est_count: u64,
    /// Storage size in bytes (`collStats.size`), for the tree badge.
    pub size: u64,
    /// Whether the collection is capped (fixed-size ring).
    pub capped: bool,
}

/// A `find` filter / projection / sort, passed through as an extended-JSON
/// [`DocValue::Document`] in v1 (a typed query builder is a later bet). Aliases,
/// not newtypes, so they read at the call sites without ceremony.
pub type Filter = DocValue;
pub type Projection = DocValue;
pub type Sort = DocValue;

/// One windowed `find` request — the browse read. `filter`/`projection`/`sort`
/// are `None` when unset (an empty filter matches everything). `batch` bounds one
/// window the way the SQL grid's page size does.
#[derive(Debug, Clone)]
pub struct FindQuery {
    pub db: String,
    pub coll: String,
    pub filter: Option<Filter>,
    pub projection: Option<Projection>,
    pub sort: Option<Sort>,
    pub skip: u64,
    pub limit: Option<u64>,
    pub batch: usize,
}

/// One window of documents plus the server cursor to continue from. `cursor` is
/// `None` when the whole result fit in this batch; `exhausted` is the explicit
/// "no more documents" flag (a `Some(cursor)` with `exhausted` never happens).
#[derive(Debug, Clone)]
pub struct DocPage {
    pub docs: Vec<Document>,
    pub cursor: Option<DocCursor>,
    pub exhausted: bool,
}

/// An opaque handle to a live server-side cursor (`find` → `getMore`), echoed
/// back to `next_batch`/`close_cursor`. The `id` is Mongo's own cursor id; `db`
/// and `coll` are needed to address the `getMore` at the right namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocCursor {
    pub id: i64,
    pub db: String,
    pub coll: String,
}

/// A document store's deployment topology, detected at connect. Mirrors
/// `KvTopology`; drives affordances that differ by shape (a changeset is atomic
/// only on a replica set, a sharded cluster fans some reads out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocTopology {
    Standalone,
    ReplicaSet,
    Sharded,
}

/// A BSON type tag, the eq/hashable key the inferred-schema panel groups a
/// field's observed values by. Mirrors the [`DocValue`] arms (collapsing the two
/// container arms to `Array`/`Object`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DocType {
    Null,
    Bool,
    Int,
    Long,
    Double,
    Decimal,
    Str,
    ObjectId,
    Date,
    Timestamp,
    Binary,
    Regex,
    Array,
    Object,
}

impl DocType {
    /// The `$type`-alias label (`"string"`, `"objectId"`, …).
    pub fn label(self) -> &'static str {
        match self {
            DocType::Null => "null",
            DocType::Bool => "bool",
            DocType::Int => "int",
            DocType::Long => "long",
            DocType::Double => "double",
            DocType::Decimal => "decimal",
            DocType::Str => "string",
            DocType::ObjectId => "objectId",
            DocType::Date => "date",
            DocType::Timestamp => "timestamp",
            DocType::Binary => "binData",
            DocType::Regex => "regex",
            DocType::Array => "array",
            DocType::Object => "object",
        }
    }
}

/// One field path's inferred shape across a sample: which types were seen (with
/// their observed frequency) and how often the field was present at all. The
/// row the schema panel renders, and the "this field is a string 82% / int 18%"
/// drift signal a schemaless store needs.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldStat {
    /// The dotted field path (`user.addr.city` for a nested field).
    pub path: String,
    /// Observed types with their counts, most-frequent first.
    pub types: Vec<(DocType, u64)>,
    /// Fraction of sampled documents in which the field was present, `0.0..=1.0`.
    pub present_ratio: f32,
}

/// A collection's inferred schema from [`DocDriver::infer_schema`]: one
/// [`FieldStat`] per discovered field path (sorted), plus how many documents were
/// sampled to produce it.
#[derive(Debug, Clone, PartialEq)]
pub struct DocSchema {
    pub fields: Vec<FieldStat>,
    pub sampled: usize,
}

impl DocSchema {
    /// Roll a sample of documents into a schema: for every field path (dotted for
    /// nested sub-documents, `_id` included), the distribution of BSON types seen
    /// and the fraction of the sample the field appeared in. Deterministic —
    /// paths sort, and each path's types sort by descending count then label — so
    /// the same sample always yields the same schema. Arrays are recorded as the
    /// `array` type without descending into element shapes (a v1 simplification).
    /// Both `MongoDriver` and the test double build their schema through this, so
    /// the rollup lives once, in the pure core.
    pub fn from_documents(docs: &[Document]) -> DocSchema {
        use std::collections::BTreeMap;

        // path -> (present count, type -> count)
        let mut acc: BTreeMap<String, (u64, BTreeMap<DocType, u64>)> = BTreeMap::new();
        for doc in docs {
            let mut record = |path: String, value: &DocValue| {
                let entry = acc.entry(path).or_default();
                entry.0 += 1;
                *entry.1.entry(value.doc_type()).or_insert(0) += 1;
            };
            collect_fields("_id", &doc.id, &mut record);
            for (name, value) in &doc.fields {
                collect_fields(name, value, &mut record);
            }
        }

        let sampled = docs.len();
        let denom = sampled.max(1) as f32;
        let fields = acc
            .into_iter()
            .map(|(path, (present, type_counts))| {
                let mut types: Vec<(DocType, u64)> = type_counts.into_iter().collect();
                // Most-frequent first; ties broken by the stable type label.
                types.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.label().cmp(b.0.label())));
                FieldStat {
                    path,
                    types,
                    present_ratio: present as f32 / denom,
                }
            })
            .collect();
        DocSchema { fields, sampled }
    }
}

/// Record `value` at `path`, descending into a sub-document to emit its dotted
/// child paths too. Arrays and scalars record only themselves.
fn collect_fields(path: &str, value: &DocValue, record: &mut impl FnMut(String, &DocValue)) {
    record(path.to_string(), value);
    if let DocValue::Document(fields) = value {
        for (name, child) in fields {
            collect_fields(&format!("{path}.{name}"), child, record);
        }
    }
}

/// One index on a collection (`listIndexes`), for the indexes panel.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexInfo {
    pub name: String,
    /// The index key spec as `(field, order)` pairs; `order` is the spelling
    /// Mongo returns (`"1"`/`"-1"` for a b-tree direction, or `"text"`/`"2dsphere"`/
    /// `"hashed"` for a special index), kept as a string so non-numeric index
    /// types survive.
    pub keys: Vec<(String, String)>,
    pub unique: bool,
    pub sparse: bool,
    /// The TTL in seconds (`expireAfterSeconds`) for a TTL index, else `None`.
    pub ttl: Option<i64>,
    /// Whether the index is partial (`partialFilterExpression` present).
    pub partial: bool,
}

/// A query's `explain` rollup from [`DocDriver::explain`]: the winning-plan
/// stages plus the numbers that answer "is this query using an index, and how
/// wasteful is it".
#[derive(Debug, Clone, PartialEq)]
pub struct DocPlan {
    /// The winning plan's stages, outermost first (`FETCH` -> `IXSCAN`, …).
    pub stages: Vec<PlanStage>,
    /// The index the winning plan used, if any (`None` for a collection scan).
    pub index_used: Option<String>,
    /// Documents the plan examined vs. returned, when the executor reported them
    /// (an `explain("executionStats")`); the `examined / returned` ratio is the
    /// waste signal.
    pub docs_examined: Option<u64>,
    pub n_returned: Option<u64>,
    /// Whether the winning plan is a full collection scan (`COLLSCAN`) — the
    /// "you're missing an index" flag.
    pub collscan: bool,
}

/// One node in an `explain` winning plan, flattened with its depth so the panel
/// can indent the stage tree.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanStage {
    pub stage: String,
    pub depth: usize,
    /// The index name for an `IXSCAN` stage, or another short detail, if any.
    pub detail: Option<String>,
}

// --- writes ------------------------------------------------------------------

/// How a document changes in an update: a `$set`-style partial patch (merge the
/// given fields) or a full replacement document.
#[derive(Debug, Clone, PartialEq)]
pub enum DocUpdate {
    /// Merge these top-level fields into the matched documents (`$set`).
    Patch(DocValue),
    /// Replace the matched document wholesale.
    Replace(Document),
}

/// A new index to create (`createIndex`). v1 covers b-tree keys with an optional
/// unique constraint and name; ttl/partial creation is a later refinement.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSpec {
    /// `(field, direction)` pairs; direction is `1` (ascending) or `-1`.
    pub keys: Vec<(String, i32)>,
    pub unique: bool,
    /// An explicit index name, or `None` to let the server derive one.
    pub name: Option<String>,
}

/// One document-store write, the unit the classifier ([`classify_doc_op`]) and
/// the confirm prompt reason about, and what the UI proposes to the service. The
/// service matches on it to dispatch the right [`crate`]-level driver call, so a
/// write has exactly one representation from proposal through execution.
#[derive(Debug, Clone, PartialEq)]
pub enum DocWrite {
    Insert {
        db: String,
        coll: String,
        docs: Vec<Document>,
    },
    Update {
        db: String,
        coll: String,
        filter: Filter,
        change: DocUpdate,
        many: bool,
    },
    Replace {
        db: String,
        coll: String,
        id: DocValue,
        doc: Document,
    },
    Delete {
        db: String,
        coll: String,
        filter: Filter,
        many: bool,
    },
    CreateCollection {
        db: String,
        coll: String,
    },
    DropCollection {
        db: String,
        coll: String,
    },
    CreateIndex {
        db: String,
        coll: String,
        spec: IndexSpec,
    },
}

/// How risky a write is. A `Destructive` op needs an explicit confirm even on a
/// writable connection, so neither the console nor the AI can slip one through;
/// mirrors `kv::CommandClass`'s `Destructive` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocOpClass {
    Write,
    Destructive,
}

/// Whether a filter document matches nothing-specific — i.e. it's empty (`{}`),
/// so an `update`/`delete` over it touches the whole collection. An absent or
/// non-document filter is treated as empty (match-all), the conservative reading.
fn filter_is_empty(filter: &Filter) -> bool {
    match filter {
        DocValue::Document(fields) => fields.is_empty(),
        _ => true,
    }
}

/// Classify a proposed write. Destructive covers the document-store footguns the
/// plan calls out: dropping a collection, a multi-document `delete`/`update`, and
/// an *un-filtered* `update`/`delete` (which touches the whole collection even
/// when `many` is false, since Mongo's single-document form still picks an
/// arbitrary match). Everything else is an ordinary `Write`.
pub fn classify_doc_op(op: &DocWrite) -> DocOpClass {
    let destructive = match op {
        DocWrite::DropCollection { .. } => true,
        DocWrite::Delete { filter, many, .. } => *many || filter_is_empty(filter),
        DocWrite::Update { filter, many, .. } => *many || filter_is_empty(filter),
        DocWrite::Insert { .. }
        | DocWrite::Replace { .. }
        | DocWrite::CreateCollection { .. }
        | DocWrite::CreateIndex { .. } => false,
    };
    if destructive {
        DocOpClass::Destructive
    } else {
        DocOpClass::Write
    }
}

// --- hand-rolled extended-JSON helpers (no serde_json) -----------------------

/// Append `s` as a JSON string literal (quotes + minimal escaping), matching
/// `serde_json`'s escaping so the output parses cleanly downstream.
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Standard base64 (with padding), appended to `out`. Small and dependency-free,
/// like the Redis binary decoders.
fn base64_encode(out: &mut String, bytes: &[u8]) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
}

/// Format `ms` since the Unix epoch as an ISO-8601 UTC instant with millisecond
/// precision (`2026-01-02T03:04:05.678Z`), or `None` when the year falls outside
/// the four-digit `0000..=9999` range extended JSON's relaxed date form allows
/// (the caller falls back to the canonical `$numberLong` form). Pure civil-date
/// arithmetic (Howard Hinnant's `days_from_civil` inverse), no `chrono`.
fn iso8601_utc(ms: i64) -> Option<String> {
    let (days, ms_of_day) = {
        // Euclidean division so a negative (pre-1970) instant floors correctly.
        let day = ms.div_euclid(86_400_000);
        let rem = ms.rem_euclid(86_400_000);
        (day, rem)
    };
    let (year, month, dom) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return None;
    }
    let secs_of_day = ms_of_day / 1000;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    let milli = ms_of_day % 1000;
    Some(format!(
        "{year:04}-{month:02}-{dom:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}Z"
    ))
}

/// Convert a day count relative to 1970-01-01 into `(year, month, day)`. The
/// standard branch-free algorithm (Hinnant, "chrono-Compatible Low-Level Date
/// Algorithms"); valid across the whole `i64` day range.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extjson_scalars() {
        assert_eq!(DocValue::Null.to_extended_json(), "null");
        assert_eq!(DocValue::Bool(true).to_extended_json(), "true");
        assert_eq!(DocValue::Int32(42).to_extended_json(), "42");
        assert_eq!(DocValue::Int64(-7).to_extended_json(), "-7");
        assert_eq!(
            DocValue::Str("a\"b\nc".into()).to_extended_json(),
            r#""a\"b\nc""#
        );
        assert_eq!(
            DocValue::Decimal128("1.50".into()).to_extended_json(),
            r#"{"$numberDecimal":"1.50"}"#
        );
    }

    #[test]
    fn extjson_objectid_is_lowercase_hex() {
        let oid = DocValue::ObjectId([
            0x50, 0x7f, 0x1f, 0x77, 0xbc, 0xf8, 0x6c, 0xd7, 0x99, 0x43, 0x90, 0x11,
        ]);
        assert_eq!(
            oid.to_extended_json(),
            r#"{"$oid":"507f1f77bcf86cd799439011"}"#
        );
    }

    #[test]
    fn extjson_datetime_is_iso() {
        // 2021-01-01T00:00:00.000Z
        let dt = DocValue::DateTime(1_609_459_200_000);
        assert_eq!(
            dt.to_extended_json(),
            r#"{"$date":"2021-01-01T00:00:00.000Z"}"#
        );
    }

    #[test]
    fn extjson_epoch_and_negative_dates() {
        assert_eq!(
            DocValue::DateTime(0).to_extended_json(),
            r#"{"$date":"1970-01-01T00:00:00.000Z"}"#
        );
        // 1969-12-31T23:59:59.000Z (one second before epoch).
        assert_eq!(
            DocValue::DateTime(-1000).to_extended_json(),
            r#"{"$date":"1969-12-31T23:59:59.000Z"}"#
        );
    }

    #[test]
    fn extjson_binary_base64() {
        let bin = DocValue::Binary {
            subtype: 0,
            bytes: vec![0x66, 0x6f, 0x6f], // "foo"
        };
        assert_eq!(
            bin.to_extended_json(),
            r#"{"$binary":{"base64":"Zm9v","subType":"00"}}"#
        );
    }

    #[test]
    fn extjson_nested_preserves_order() {
        let doc = DocValue::Document(vec![
            ("b".into(), DocValue::Int32(1)),
            (
                "a".into(),
                DocValue::Array(vec![DocValue::Str("x".into()), DocValue::Null]),
            ),
        ]);
        assert_eq!(doc.to_extended_json(), r#"{"b":1,"a":["x",null]}"#);
    }

    #[test]
    fn cell_bridge_scalars_and_nesting() {
        assert_eq!(DocValue::Int64(5).to_cell(4096), Value::Integer(5));
        assert_eq!(DocValue::Double(1.5).to_cell(4096), Value::Real(1.5));
        assert_eq!(
            DocValue::ObjectId([0; 12]).to_cell(4096),
            Value::Text("000000000000000000000000".into())
        );
        // A nested cell is capped extended-JSON text, still a `Value`.
        let nested = DocValue::Array(vec![DocValue::Int32(1)]).to_cell(4096);
        assert_eq!(nested, Value::Text("[1]".into()));
    }

    #[test]
    fn schema_rollup_is_deterministic_and_nested() {
        let docs = vec![
            Document {
                id: DocValue::Int32(1),
                fields: vec![
                    ("name".into(), DocValue::Str("a".into())),
                    (
                        "user".into(),
                        DocValue::Document(vec![("age".into(), DocValue::Int32(30))]),
                    ),
                ],
            },
            Document {
                id: DocValue::Int32(2),
                // `name` is an int here (type drift); no `user`.
                fields: vec![("name".into(), DocValue::Int64(7))],
            },
        ];
        let schema = DocSchema::from_documents(&docs);
        assert_eq!(schema.sampled, 2);
        // Paths are sorted and include the dotted nested path.
        let paths: Vec<&str> = schema.fields.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["_id", "name", "user", "user.age"]);

        let name = schema.fields.iter().find(|f| f.path == "name").unwrap();
        assert_eq!(name.present_ratio, 1.0);
        // Two distinct types seen, tie broken by label ("long" < "string").
        assert_eq!(name.types, vec![(DocType::Long, 1), (DocType::Str, 1)]);

        let user = schema.fields.iter().find(|f| f.path == "user").unwrap();
        assert_eq!(user.present_ratio, 0.5);
        assert_eq!(user.types, vec![(DocType::Object, 1)]);

        // Same input -> identical schema (determinism).
        assert_eq!(DocSchema::from_documents(&docs), schema);
    }

    #[test]
    fn classify_writes() {
        let doc = || Document {
            id: DocValue::Int32(1),
            fields: vec![],
        };
        let by_id = || DocValue::Document(vec![("_id".into(), DocValue::Int32(1))]);
        let empty = || DocValue::Document(vec![]);

        // Ordinary writes.
        assert_eq!(
            classify_doc_op(&DocWrite::Insert {
                db: "d".into(),
                coll: "c".into(),
                docs: vec![doc()],
            }),
            DocOpClass::Write
        );
        assert_eq!(
            classify_doc_op(&DocWrite::Replace {
                db: "d".into(),
                coll: "c".into(),
                id: DocValue::Int32(1),
                doc: doc(),
            }),
            DocOpClass::Write
        );
        // A single, filtered delete is an ordinary write.
        assert_eq!(
            classify_doc_op(&DocWrite::Delete {
                db: "d".into(),
                coll: "c".into(),
                filter: by_id(),
                many: false,
            }),
            DocOpClass::Write
        );

        // Destructive: drop, many, or an un-filtered mutation.
        assert_eq!(
            classify_doc_op(&DocWrite::DropCollection {
                db: "d".into(),
                coll: "c".into(),
            }),
            DocOpClass::Destructive
        );
        assert_eq!(
            classify_doc_op(&DocWrite::Delete {
                db: "d".into(),
                coll: "c".into(),
                filter: by_id(),
                many: true,
            }),
            DocOpClass::Destructive
        );
        assert_eq!(
            classify_doc_op(&DocWrite::Delete {
                db: "d".into(),
                coll: "c".into(),
                filter: empty(),
                many: false,
            }),
            DocOpClass::Destructive
        );
    }

    #[test]
    fn base64_padding() {
        let mut s = String::new();
        base64_encode(&mut s, b"f");
        assert_eq!(s, "Zg==");
        s.clear();
        base64_encode(&mut s, b"fo");
        assert_eq!(s, "Zm8=");
        s.clear();
        base64_encode(&mut s, b"foobar");
        assert_eq!(s, "Zm9vYmFy");
    }
}
