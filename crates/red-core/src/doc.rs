//! Domain types for the `DocDriver` seam. The third seam, parallel to the SQL-shaped
//! types in `lib.rs` and the Redis-shaped ones in `kv.rs`, for engines that are
//! neither: a `server → databases → collections → documents` hierarchy of nested BSON
//! trees. Nothing here knows about UI, a runtime, or the `mongodb`/`bson` crates:
//! [`DocValue`] is a *mirror* of the BSON value tree, and the conversion between the
//! two lives entirely in `red-driver` (the version firewall, exactly like the SQL and
//! KV families). Extended-JSON rendering is hand-rolled below so this crate stays
//! dependency-light (no `serde_json`).

mod fastfilter;

pub use fastfilter::{FastFilter, compile_fast_filter};

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

    /// The value at a dotted path (`user.addr.city`), or `None` when a segment is
    /// missing or its parent is not a sub-document. `_id` addresses the split-out
    /// identity. An array is a leaf here (no `tags.0` indexing), matching how
    /// [`DocSchema::from_documents`] records one.
    pub fn value_at(&self, path: &str) -> Option<&DocValue> {
        let mut segments = path.split('.');
        let head = segments.next()?;
        let mut current = if head == "_id" {
            &self.id
        } else {
            self.fields
                .iter()
                .find(|(k, _)| k == head)
                .map(|(_, v)| v)?
        };
        for segment in segments {
            let DocValue::Document(fields) = current else {
                return None;
            };
            current = fields.iter().find(|(k, _)| k == segment).map(|(_, v)| v)?;
        }
        Some(current)
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
    /// The collection's JSON-schema validator (`options.validator`) as extended
    /// JSON, when one is declared. The closest thing MongoDB has to a
    /// constraint, so anything proposing a write wants to know it exists before
    /// the write bounces off it.
    pub validator: Option<String>,
}

/// A collection's storage numbers (`collStats`): what it costs, as opposed to
/// what it holds.
///
/// Every field is `Option` because `collStats` is *truncated* rather than refused
/// for an under-privileged user, exactly like `serverStatus`: a missing number
/// means "not reported", which the panel must show as such instead of as a zero.
#[derive(Debug, Clone, PartialEq)]
pub struct CollStats {
    /// Documents, exactly (`collStats.count`).
    pub count: Option<u64>,
    /// Uncompressed size of the documents in bytes (`size`).
    pub size: Option<u64>,
    /// Bytes actually allocated on disk (`storageSize`), which compression can
    /// make much smaller than `size`.
    pub storage_size: Option<u64>,
    /// Mean document size in bytes (`avgObjSize`).
    pub avg_obj_size: Option<u64>,
    /// Total bytes across all indexes (`totalIndexSize`).
    pub total_index_size: Option<u64>,
    /// Per-index bytes (`indexSizes`), largest first.
    pub index_sizes: Vec<(String, u64)>,
    /// Whether the collection is sharded, and across how many shards.
    pub shards: Option<usize>,
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

/// Render a browse sort as an extended-JSON document (`{"age":-1,"name":1}`), the
/// spelling [`FindQuery::sort`] wants. Keys ride in the order given: a document
/// store's sort is ordered, so the first key is the primary one.
///
/// `_id` is appended as a final tiebreaker unless it is already a key, so a sort on
/// a field with repeats has a *total* order. Without it, two windows of a paged
/// browse can disagree about which of two equal-keyed documents comes first, and the
/// grid shows a document twice while another never appears.
pub fn sort_json(keys: &[(String, bool)]) -> String {
    let mut out = String::from("{");
    for (i, (field, ascending)) in keys.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(&mut out, field);
        out.push_str(if *ascending { ":1" } else { ":-1" });
    }
    if !keys.iter().any(|(f, _)| f == "_id") {
        if !keys.is_empty() {
            out.push(',');
        }
        out.push_str("\"_id\":1");
    }
    out.push('}');
    out
}

/// Render a projection over `fields` as extended JSON (`{"name":1,"user.city":1}`).
///
/// `_id` is never listed: MongoDB includes it unless explicitly excluded, and every
/// RED surface addresses a document by it, so a projection that dropped it would
/// leave rows the inspector, the editor and the delete path cannot identify.
pub fn projection_json(fields: &[String]) -> String {
    let mut out = String::from("{");
    let mut first = true;
    for field in fields.iter().filter(|f| f.as_str() != "_id") {
        if !first {
            out.push(',');
        }
        first = false;
        write_json_string(&mut out, field);
        out.push_str(":1");
    }
    out.push('}');
    out
}

/// A streamed document-export target format.
///
/// Two shapes, and the split is load-bearing: the JSON pair writes each document
/// as extended JSON, so every BSON type survives the round trip; the tabular pair
/// flattens documents onto a fixed column list, which is what a spreadsheet needs
/// and what costs the types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocExportFormat {
    /// One JSON array of extended-JSON documents.
    Json,
    /// One extended-JSON document per line (newline-delimited JSON), the format a
    /// downstream tool can read without holding the whole file.
    Ndjson,
    /// Flattened rows against dotted column paths.
    Csv,
    /// Flattened rows into an Excel workbook.
    Xlsx,
}

impl DocExportFormat {
    /// The destination file's extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            DocExportFormat::Json => "json",
            DocExportFormat::Ndjson => "ndjson",
            DocExportFormat::Csv => "csv",
            DocExportFormat::Xlsx => "xlsx",
        }
    }

    /// Whether the format writes rows against a fixed column list, and therefore
    /// needs one computed before the first document is written.
    pub fn is_tabular(self) -> bool {
        matches!(self, DocExportFormat::Csv | DocExportFormat::Xlsx)
    }
}

/// A streamed document-import source format, the read-side mirror of
/// [`DocExportFormat`]. Narrower than the export set on purpose: XLSX is a write
/// target, not a source RED parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocImportFormat {
    /// One top-level JSON array of objects.
    Json,
    /// One JSON object per line.
    Ndjson,
    /// A header of dotted field paths over flat records.
    Csv,
}

impl DocImportFormat {
    /// The format a file name suggests, or `None` when the extension says nothing.
    /// A guess the import dialog pre-selects and the user can override.
    pub fn from_extension(name: &str) -> Option<DocImportFormat> {
        let ext = name.rsplit('.').next()?.to_ascii_lowercase();
        match ext.as_str() {
            "json" => Some(DocImportFormat::Json),
            "ndjson" | "jsonl" => Some(DocImportFormat::Ndjson),
            "csv" => Some(DocImportFormat::Csv),
            _ => None,
        }
    }
}

/// How a collection copy writes into its target.
///
/// The document analogue of [`CopyMode`](crate::CopyMode), with one arm that has no
/// SQL counterpart: a document store has no schema to preserve, so replacing a
/// collection wholesale is a drop, not a truncate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocCopyMode {
    /// Insert the source documents; the target keeps what it already had. A
    /// duplicate `_id` fails the chunk it is in.
    Append,
    /// Replace a target document with the same `_id`, insert when there is none:
    /// re-runnable without collisions.
    UpsertOnId,
    /// Drop the target collection first, then insert: a full refresh. Destructive,
    /// and gated by the same confirm a drop is.
    DropAndInsert,
}

/// How an import writes each document into the target collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocImportMode {
    /// Insert every document. A duplicate `_id` fails that document.
    Insert,
    /// Replace the document with the same `_id`, inserting when there is none
    /// (`replaceOne` with `upsert`). What makes re-importing an export idempotent.
    UpsertOnId,
}

/// The column list a tabular export writes, derived from an inferred schema:
/// every field path that ever held something other than a sub-document, `_id`
/// first and the rest in the schema's (sorted) order, capped at `max`.
///
/// A path that is *only* ever a sub-document is dropped, because its children
/// already carry the data. A path with type drift (a string in some documents, a
/// sub-document in others) is kept, because dropping it would lose the string
/// case — a schemaless store's whole hazard.
pub fn tabular_columns(schema: &DocSchema, max: usize) -> Vec<String> {
    let mut columns = Vec::with_capacity(schema.fields.len().min(max));
    let scalar = |f: &FieldStat| f.types.iter().any(|(t, _)| *t != DocType::Object);
    if schema.fields.iter().any(|f| f.path == "_id") {
        columns.push("_id".to_string());
    }
    for field in schema.fields.iter().filter(|f| f.path != "_id") {
        if columns.len() >= max {
            break;
        }
        if scalar(field) {
            columns.push(field.path.clone());
        }
    }
    columns
}

/// Whether a tabular export of `doc` against `columns` would drop something: any
/// leaf the column list does not address. The signal behind the export's
/// "documents had fields outside the sampled columns" shortfall, because a
/// column list sampled from part of a collection cannot cover a schemaless whole.
pub fn has_unmapped_fields(doc: &Document, columns: &[String]) -> bool {
    let mut unmapped = false;
    let mut check = |path: &str| {
        if !unmapped && !columns.iter().any(|c| c == path) {
            unmapped = true;
        }
    };
    leaf_paths("_id", &doc.id, &mut check);
    for (name, value) in &doc.fields {
        leaf_paths(name, value, &mut check);
    }
    unmapped
}

/// Emit the dotted path of every leaf under `value`: a non-document value, or an
/// empty sub-document (which has no children to carry it).
fn leaf_paths(path: &str, value: &DocValue, emit: &mut impl FnMut(&str)) {
    match value {
        DocValue::Document(fields) if !fields.is_empty() => {
            for (name, child) in fields {
                leaf_paths(&format!("{path}.{name}"), child, emit);
            }
        }
        _ => emit(path),
    }
}

/// A keyset seek into an `_id`-ordered browse: which boundary to page from and
/// in which direction. The browse grid's continuous scroll reads windows this
/// way rather than by `skip`, so a deep window costs O(window) on the always
/// present `_id` index instead of O(skip). The `_id` sort is implied and total
/// (BSON orders `_id` across mixed types), so the three arms cover the whole
/// scroll: seed at the start, extend either way, or land at an exact ordinal.
#[derive(Debug, Clone, PartialEq)]
pub enum DocSeek {
    /// The first window (`after` is `None`), or the window strictly after the
    /// boundary `_id` (`{_id: {$gt: after}}`). Rows come back ascending.
    Forward { after: Option<DocValue> },
    /// The window strictly before the boundary `_id` (`{_id: {$lt: before}}`),
    /// returned ascending so it prepends onto the resident run in order.
    Backward { before: DocValue },
    /// Land exactly at ordinal `skip` (`find().sort(_id).skip(skip)`), the
    /// scrollbar's far jump. Exact (not interpolated), so ordinals stay precise.
    Jump { skip: u64 },
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

/// One operation the deployment is running right now (`$currentOp`). The
/// document-store analogue of [`ServerSession`](crate::ServerSession): same
/// question ("what is it doing, and what is stuck"), different shape, because
/// Mongo addresses an operation by an opid rather than a connection and reports
/// a namespace rather than a database plus a statement.
#[derive(Debug, Clone, PartialEq)]
pub struct DocOp {
    /// What `killOp` addresses this operation by.
    pub opid: i64,
    /// The operation kind as the server words it (`query`, `insert`, `getmore`,
    /// `command`), verbatim.
    pub op: String,
    /// `db.collection` the operation runs against.
    pub namespace: String,
    /// Seconds the operation has been running, computed server-side.
    pub secs_running: f64,
    pub client: Option<String>,
    /// The command document as text, when the connected role may see it.
    pub command: Option<String>,
    /// Whether the operation is blocked waiting for a lock, the signature of a
    /// stall that is somebody else's fault.
    pub waiting_for_lock: bool,
    /// This is the `$currentOp` aggregation that produced this very listing.
    /// Never offered a kill, for the same reason
    /// [`ServerSession::is_self`](crate::ServerSession::is_self) is not: killing
    /// it stops the read the panel is showing and achieves nothing else.
    pub is_self: bool,
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

/// A collection's inferred schema from `DocDriver::infer_schema`: one
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

/// A query's `explain` rollup from `DocDriver::explain`: the winning-plan
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

/// How one index key is spelled: a b-tree direction, or the index type that
/// replaces a direction for a special index.
///
/// A **wildcard** index has no variant here because it is not a key type: it is a
/// key *path* (`"$**"`, or `"user.$**"`) with an ordinary ascending direction, so
/// it is expressed by the field name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKey {
    Asc,
    Desc,
    /// A text index over the field (`"text"`). At most one per collection, which
    /// is the server's rule, not RED's.
    Text,
    /// A hashed index (`"hashed"`), the shard-key shape.
    Hashed,
    /// A 2dsphere geospatial index (`"2dsphere"`).
    Sphere2d,
}

impl IndexKey {
    /// The value the key takes in a `createIndex` key document, as Mongo spells
    /// it: a number for a b-tree direction, a string for a special type.
    pub fn spec_value(self) -> &'static str {
        match self {
            IndexKey::Asc => "1",
            IndexKey::Desc => "-1",
            IndexKey::Text => "text",
            IndexKey::Hashed => "hashed",
            IndexKey::Sphere2d => "2dsphere",
        }
    }

    /// Whether the key is a b-tree direction rather than a special type. Only a
    /// b-tree key can carry a sort, so this is what decides whether an index can
    /// serve one.
    pub fn is_btree(self) -> bool {
        matches!(self, IndexKey::Asc | IndexKey::Desc)
    }

    /// The label the index dialog shows.
    pub fn label(self) -> &'static str {
        match self {
            IndexKey::Asc => "ascending",
            IndexKey::Desc => "descending",
            IndexKey::Text => "text",
            IndexKey::Hashed => "hashed",
            IndexKey::Sphere2d => "2dsphere",
        }
    }

    /// The kinds the dialog offers, in menu order.
    pub const ALL: [IndexKey; 5] = [
        IndexKey::Asc,
        IndexKey::Desc,
        IndexKey::Text,
        IndexKey::Hashed,
        IndexKey::Sphere2d,
    ];
}

/// A new index to create (`createIndex`).
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSpec {
    /// `(field, kind)` pairs, in key order. Order is load-bearing for a compound
    /// index: only a prefix of the keys can serve a query.
    pub keys: Vec<(String, IndexKey)>,
    pub unique: bool,
    /// Skip documents that lack the indexed field entirely.
    pub sparse: bool,
    /// An explicit index name, or `None` to let the server derive one.
    pub name: Option<String>,
    /// Delete a document this many seconds after its indexed date value: a TTL
    /// index. Only meaningful on a single date-valued key, which is the server's
    /// constraint.
    pub ttl_seconds: Option<i64>,
    /// Index only the documents matching this filter: a partial index.
    ///
    /// Carried as extended-JSON **text**, not a parsed [`Filter`], for the reason
    /// `Command::DocFetchRun`'s filter is: it is typed by a user in a UI that has
    /// no parser, and the driver owns the extended-JSON dialect. The driver parses
    /// it, and a syntax error surfaces as the write failing rather than as a
    /// silently ignored option.
    pub partial_filter: Option<String>,
    /// An ICU locale for a case- and accent-aware index (`"en"`, `"de@collation=phonebook"`).
    pub collation_locale: Option<String>,
}

impl IndexSpec {
    /// A plain b-tree index over `keys`, ascending, with every option off. The
    /// shape most indexes actually are, so the dialog and the agent both start
    /// here and set what they need.
    pub fn ascending(keys: impl IntoIterator<Item = String>) -> IndexSpec {
        IndexSpec {
            keys: keys.into_iter().map(|k| (k, IndexKey::Asc)).collect(),
            unique: false,
            sparse: false,
            name: None,
            ttl_seconds: None,
            partial_filter: None,
            collation_locale: None,
        }
    }

    /// The name Mongo would derive for this spec (`field_1_other_-1`), which the
    /// dialog shows as the placeholder so the user can see what they will get.
    pub fn derived_name(&self) -> String {
        self.keys
            .iter()
            .map(|(field, kind)| format!("{field}_{}", kind.spec_value()))
            .collect::<Vec<_>>()
            .join("_")
    }
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
    DropIndex {
        db: String,
        coll: String,
        name: String,
    },
    /// Set (or, with `None`, remove) a collection's JSON-Schema validator
    /// (`collMod`). Extended-JSON **text**, parsed by the driver, for the reason
    /// [`IndexSpec::partial_filter`] is.
    SetValidator {
        db: String,
        coll: String,
        validator: Option<String>,
    },
}

pub use crate::OpClass;

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
pub fn classify_doc_op(op: &DocWrite) -> OpClass {
    let destructive = match op {
        DocWrite::DropCollection { .. } => true,
        DocWrite::Delete { filter, many, .. } => *many || filter_is_empty(filter),
        DocWrite::Update { filter, many, .. } => *many || filter_is_empty(filter),
        // Dropping an index writes no documents and is still the write most
        // likely to take a production deployment down: the queries that relied on
        // it silently become collection scans.
        DocWrite::DropIndex { .. } => true,
        // A validator writes no documents either, and decides whether every
        // future write is accepted. Removing one is as consequential as adding
        // one, so both confirm.
        DocWrite::SetValidator { .. } => true,
        DocWrite::Insert { .. }
        | DocWrite::Replace { .. }
        | DocWrite::CreateCollection { .. }
        | DocWrite::CreateIndex { .. } => false,
    };
    if destructive {
        OpClass::Destructive
    } else {
        OpClass::Write
    }
}

/// Split an aggregation pipeline's text into its top-level stage texts, keeping
/// each stage exactly as written. `None` when the text is not a bracketed array,
/// which is the caller's cue to leave it alone rather than mangle it.
///
/// A *shallow* split: it tracks bracket depth and string state and nothing else,
/// so it needs no parser and no BSON knowledge. That is enough to move between the
/// raw editor and the stage list without either becoming the other's lossy copy --
/// a stage the split hands back is byte-identical to the one it was given,
/// comments and formatting included.
pub fn split_pipeline_stages(text: &str) -> Option<Vec<String>> {
    let trimmed = text.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    let mut stages = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    let mut start = 0usize;
    for (i, ch) in inner.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth -= 1,
            ',' if depth == 0 => {
                let stage = inner[start..i].trim();
                if !stage.is_empty() {
                    stages.push(stage.to_string());
                }
                start = i + ch.len_utf8();
            }
            _ => {}
        }
    }
    // An unbalanced pipeline is being typed, not described; refuse rather than
    // hand back stages that would silently drop the unclosed tail.
    if depth != 0 || in_str {
        return None;
    }
    let tail = inner[start..].trim();
    if !tail.is_empty() {
        stages.push(tail.to_string());
    }
    Some(stages)
}

/// Join stage texts back into a pipeline, one stage per line. The inverse of
/// [`split_pipeline_stages`] for a stage list that has been reordered or edited.
pub fn join_pipeline_stages(stages: &[String]) -> String {
    if stages.is_empty() {
        return "[]".to_string();
    }
    let body = stages
        .iter()
        .map(|s| format!("  {}", s.trim()))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("[\n{body}\n]")
}

/// The `$`-prefixed operator a stage text leads with (`$match`, `$group`), for the
/// stage list's row label. `None` when the stage names none, which is a stage the
/// server will reject anyway.
pub fn stage_operator(stage: &str) -> Option<&str> {
    let after_brace = stage.trim().strip_prefix('{')?.trim_start();
    let quoted = after_brace.strip_prefix('"')?;
    let end = quoted.find('"')?;
    let key = &quoted[..end];
    key.starts_with('$').then_some(key)
}

/// The index keys a filter would want, when `plan` says the query is scanning the
/// whole collection. Empty when the plan already uses an index, when the filter
/// names no fields, or when it is not a document.
///
/// The same reasoning the `index_advice` agent tool applies, lifted here so the
/// Indexes panel can offer the suggestion without asking a model. Deliberately
/// shallow: the equality fields at the top level, in the order they appear.
/// Ordering a compound key well needs the selectivity numbers neither an `explain`
/// nor a sample supplies, so RED suggests the keys and leaves the ordering to a
/// human who knows the data.
pub fn suggested_index_keys(filter: &Filter, plan: &DocPlan) -> Vec<String> {
    if !plan.collscan {
        return Vec::new();
    }
    let DocValue::Document(fields) = filter else {
        return Vec::new();
    };
    fields
        .iter()
        .map(|(k, _)| k.clone())
        // `$and` / `$or` / `$where` name no field to index.
        .filter(|k| !k.starts_with('$'))
        .collect()
}

/// The first pipeline stage that writes (`$out`/`$merge`), if any. Aggregation is
/// the one *read* surface that can write — `$merge` can even target another
/// database — so both the human `DocAggregate` path and the AI's read-tier
/// `aggregate` tool must refuse these when the connection is read-only.
pub fn pipeline_write_stage(stages: &[DocValue]) -> Option<&'static str> {
    stages.iter().find_map(|stage| match stage {
        DocValue::Document(fields) => fields.iter().find_map(|(k, _)| match k.as_str() {
            "$out" => Some("$out"),
            "$merge" => Some("$merge"),
            _ => None,
        }),
        _ => None,
    })
}

/// One field that looks like it references another collection: which collection
/// it lives in, the dotted path, the collection its name points at, and the type
/// its values actually had.
#[derive(Debug, Clone, PartialEq)]
pub struct RefCandidate {
    pub coll: String,
    pub path: String,
    pub target: String,
    pub doc_type: DocType,
}

/// The reference candidates in one collection's inferred schema: the scalar fields
/// whose name points at a collection in `catalog`.
///
/// Mongo declares no foreign keys, so a reference can only be *guessed* from a
/// name and then tested; this is the guessing half over a whole collection, and
/// [`reference_base`] / [`match_collection`] are the naming rules it applies.
///
/// Container and null fields are excluded deliberately: a document or array field
/// may *contain* a reference, but the field itself is not one, and probing an
/// array against `_id` would report a meaningless zero.
pub fn reference_candidates(
    coll: &str,
    schema: &DocSchema,
    catalog: &[String],
) -> Vec<RefCandidate> {
    schema
        .fields
        .iter()
        .filter_map(|f| {
            let name = f.path.rsplit('.').next().unwrap_or(&f.path);
            let (doc_type, _) = f.types.first()?;
            if matches!(doc_type, DocType::Object | DocType::Array | DocType::Null) {
                return None;
            }
            // `order_id` -> `order`, else the bare name (`customer` -> `customers`).
            let base = reference_base(&f.path).unwrap_or(name);
            let target = match_collection(base, catalog)?;
            Some(RefCandidate {
                coll: coll.to_string(),
                path: f.path.clone(),
                target: target.to_string(),
                doc_type: *doc_type,
            })
        })
        .collect()
}

/// One inferred relationship between two collections, with the evidence for it.
///
/// `resolved` out of `sampled` is the whole point: Mongo declares nothing, so a
/// relationship is a *claim*, and a claim that 3 of 200 sampled values resolve is
/// a coincidence rather than a reference. The UI draws the strong ones and reports
/// the weak ones as what they are.
#[derive(Debug, Clone, PartialEq)]
pub struct DocReference {
    pub from_coll: String,
    pub field: String,
    pub to_coll: String,
    /// Distinct sampled values of the field that matched a document's `_id`.
    pub resolved: u64,
    /// Distinct values sampled.
    pub sampled: u64,
}

impl DocReference {
    /// Whether enough of the sampled values resolved to call this a reference
    /// rather than a naming coincidence.
    ///
    /// A majority, not all: a real reference to a collection that has been pruned
    /// (soft-deleted rows, an archive job) still resolves most of the time, and
    /// insisting on 100% would hide exactly the relationships worth seeing.
    pub fn is_strong(&self) -> bool {
        self.sampled > 0 && self.resolved * 2 > self.sampled
    }
}

/// The name a field is probably a reference *to*, or `None` when the field name
/// suggests nothing. Mongo has no foreign keys, so a reference can only ever be
/// guessed from the name and then *tested*; this is the guessing half, kept pure
/// and next to the types it reads.
///
/// Recognizes the four conventions that actually appear in the wild — `order_id`,
/// `orderId`, `order_ref`, `orderRef` — and returns the base (`order`). A field
/// whose name carries no such suffix returns `None` here; the caller still gets a
/// second chance by matching the bare name against the collection catalog
/// (`customer` -> `customers`), which is [`match_collection`]'s job.
///
/// `_id` itself is excluded: it is the target of references, never one.
pub fn reference_base(field: &str) -> Option<&str> {
    let name = field.rsplit('.').next().unwrap_or(field);
    if name == "_id" || name.is_empty() {
        return None;
    }
    for suffix in ["_id", "_ref"] {
        if let Some(base) = name.strip_suffix(suffix)
            && !base.is_empty()
        {
            return Some(base);
        }
    }
    // camelCase: only when the char before the suffix is lowercase, so `ID` and
    // `UUID` don't read as `U` + `Id`.
    for suffix in ["Id", "Ref"] {
        if let Some(base) = name.strip_suffix(suffix)
            && base.chars().next_back().is_some_and(char::is_lowercase)
        {
            return Some(base);
        }
    }
    None
}

/// Resolve a reference base against a collection catalog, case-insensitively and
/// tolerating the singular/plural mismatch that is the norm (`order` ->
/// `orders`). Returns the catalog's own spelling so the caller can query it.
///
/// Deliberately narrow: only exact, `+s`, and `-s` are tried. An irregular plural
/// (`person`/`people`) simply doesn't resolve, and reporting "unresolved" is the
/// honest answer — inventing a match the probe would then have to disprove is
/// worse than admitting the name told us nothing.
pub fn match_collection<'a>(base: &str, collections: &'a [String]) -> Option<&'a str> {
    let base = base.to_ascii_lowercase();
    let candidates = [
        base.clone(),
        format!("{base}s"),
        base.strip_suffix('s').unwrap_or(&base).to_string(),
    ];
    collections
        .iter()
        .find(|c| {
            let lower = c.to_ascii_lowercase();
            candidates.contains(&lower)
        })
        .map(String::as_str)
}

/// The first operator anywhere in `value` that executes server-side JavaScript
/// (`$where`, `$function`, `$accumulator`), if any. These turn a "read-only"
/// filter into arbitrary code running inside mongod, so the AI tools refuse them
/// in every user- or model-supplied document. Walked with an explicit stack so a
/// hostile deeply-nested tree cannot overflow ours.
pub fn server_js_operator(value: &DocValue) -> Option<&'static str> {
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            DocValue::Document(fields) => {
                for (k, child) in fields {
                    match k.as_str() {
                        "$where" => return Some("$where"),
                        "$function" => return Some("$function"),
                        "$accumulator" => return Some("$accumulator"),
                        _ => {}
                    }
                    stack.push(child);
                }
            }
            DocValue::Array(items) => stack.extend(items.iter()),
            _ => {}
        }
    }
    None
}

// --- hand-rolled extended-JSON helpers (no serde_json) -----------------------

/// Append `s` as a JSON string literal (quotes + minimal escaping), matching
/// `serde_json`'s escaping so the output parses cleanly downstream.
pub(super) fn write_json_string(out: &mut String, s: &str) {
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

    /// A document helper for guard tests: `doc([("k", v)])`.
    fn doc(fields: Vec<(&str, DocValue)>) -> DocValue {
        DocValue::Document(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    #[test]
    fn pipeline_write_stage_spots_out_and_merge() {
        let read = vec![doc(vec![("$match", doc(vec![]))])];
        assert_eq!(pipeline_write_stage(&read), None);
        let out = vec![
            doc(vec![("$match", doc(vec![]))]),
            doc(vec![("$out", DocValue::Str("orders_backup".into()))]),
        ];
        assert_eq!(pipeline_write_stage(&out), Some("$out"));
        let merge = vec![doc(vec![("$merge", doc(vec![]))])];
        assert_eq!(pipeline_write_stage(&merge), Some("$merge"));
        // A field *named* `$out` nested inside another stage's argument is not a
        // stage; only top-level stage keys count.
        let nested = vec![doc(vec![(
            "$match",
            doc(vec![("$out", DocValue::Int32(1))]),
        )])];
        assert_eq!(pipeline_write_stage(&nested), None);
    }

    #[test]
    fn server_js_operator_is_found_at_any_depth() {
        let clean = doc(vec![("status", DocValue::Str("active".into()))]);
        assert_eq!(server_js_operator(&clean), None);
        let top = doc(vec![("$where", DocValue::Str("sleep(1000)".into()))]);
        assert_eq!(server_js_operator(&top), Some("$where"));
        // Buried under $or → array → document, the way an injection would hide it.
        let buried = doc(vec![(
            "$or",
            DocValue::Array(vec![
                doc(vec![("a", DocValue::Int32(1))]),
                doc(vec![("$function", doc(vec![]))]),
            ]),
        )]);
        assert_eq!(server_js_operator(&buried), Some("$function"));
        let group = doc(vec![(
            "$group",
            doc(vec![("total", doc(vec![("$accumulator", doc(vec![]))]))]),
        )]);
        assert_eq!(server_js_operator(&group), Some("$accumulator"));
    }

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
            OpClass::Write
        );
        assert_eq!(
            classify_doc_op(&DocWrite::Replace {
                db: "d".into(),
                coll: "c".into(),
                id: DocValue::Int32(1),
                doc: doc(),
            }),
            OpClass::Write
        );
        // A single, filtered delete is an ordinary write.
        assert_eq!(
            classify_doc_op(&DocWrite::Delete {
                db: "d".into(),
                coll: "c".into(),
                filter: by_id(),
                many: false,
            }),
            OpClass::Write
        );

        // Destructive: drop, many, or an un-filtered mutation.
        assert_eq!(
            classify_doc_op(&DocWrite::DropCollection {
                db: "d".into(),
                coll: "c".into(),
            }),
            OpClass::Destructive
        );
        assert_eq!(
            classify_doc_op(&DocWrite::Delete {
                db: "d".into(),
                coll: "c".into(),
                filter: by_id(),
                many: true,
            }),
            OpClass::Destructive
        );
        assert_eq!(
            classify_doc_op(&DocWrite::Delete {
                db: "d".into(),
                coll: "c".into(),
                filter: empty(),
                many: false,
            }),
            OpClass::Destructive
        );
    }

    #[test]
    fn reference_base_reads_the_four_conventions() {
        assert_eq!(reference_base("order_id"), Some("order"));
        assert_eq!(reference_base("orderId"), Some("order"));
        assert_eq!(reference_base("order_ref"), Some("order"));
        assert_eq!(reference_base("orderRef"), Some("order"));
        // Nested paths are judged by their last segment.
        assert_eq!(reference_base("meta.customer_id"), Some("customer"));
        // `_id` is the target of references, never one; and an all-caps tail is
        // not a camelCase suffix (`UUID` is not `UU` + `ID`).
        assert_eq!(reference_base("_id"), None);
        assert_eq!(reference_base("UUID"), None);
        assert_eq!(reference_base("status"), None);
        assert_eq!(reference_base("_ref"), None);
    }

    #[test]
    fn match_collection_tolerates_case_and_plurality() {
        let catalog = vec!["orders".to_string(), "Customer".to_string()];
        assert_eq!(match_collection("order", &catalog), Some("orders"));
        assert_eq!(match_collection("orders", &catalog), Some("orders"));
        assert_eq!(match_collection("customers", &catalog), Some("Customer"));
        // An irregular plural does not resolve, and saying so beats inventing a
        // match the probe would then have to disprove.
        assert_eq!(match_collection("person", &catalog), None);
        assert_eq!(match_collection("invoice", &catalog), None);
    }

    #[test]
    fn pipeline_stages_round_trip_without_reformatting_a_stage() {
        let text =
            r#"[ { "$match": { "a": 1 } }, { "$group": { "_id": "$k", "n": { "$sum": 1 } } } ]"#;
        let stages = split_pipeline_stages(text).unwrap();
        assert_eq!(stages.len(), 2);
        // A stage comes back exactly as written, commas inside it included.
        assert_eq!(stages[0], r#"{ "$match": { "a": 1 } }"#);
        assert_eq!(stage_operator(&stages[0]), Some("$match"));
        assert_eq!(stage_operator(&stages[1]), Some("$group"));
        // Re-splitting the joined form yields the same stages.
        let joined = join_pipeline_stages(&stages);
        assert_eq!(split_pipeline_stages(&joined).unwrap(), stages);
    }

    #[test]
    fn pipeline_split_refuses_what_it_cannot_read() {
        assert_eq!(split_pipeline_stages("[]"), Some(Vec::new()));
        // A comma inside a string is not a separator.
        let quoted = r#"[ { "$match": { "s": "a,b" } } ]"#;
        assert_eq!(split_pipeline_stages(quoted).unwrap().len(), 1);
        // Not an array, or still being typed: leave it alone.
        assert_eq!(split_pipeline_stages("{ \"$match\": {} }"), None);
        assert_eq!(split_pipeline_stages("[ { \"$match\": {"), None);
        assert_eq!(split_pipeline_stages("[ \"unclosed ]"), None);
        assert_eq!(join_pipeline_stages(&[]), "[]");
        // A stage with no operator is reported as such, not guessed at.
        assert_eq!(stage_operator("{ \"a\": 1 }"), None);
    }

    #[test]
    fn sort_json_keeps_key_order_and_totalizes_with_id() {
        assert_eq!(
            sort_json(&[("age".into(), false), ("name".into(), true)]),
            r#"{"age":-1,"name":1,"_id":1}"#
        );
        // An explicit `_id` key is not doubled.
        assert_eq!(sort_json(&[("_id".into(), false)]), r#"{"_id":-1}"#);
        assert_eq!(sort_json(&[]), r#"{"_id":1}"#);
    }

    #[test]
    fn projection_never_drops_id() {
        assert_eq!(
            projection_json(&["name".into(), "user.city".into()]),
            r#"{"name":1,"user.city":1}"#
        );
        // Listing `_id` changes nothing: it rides along regardless.
        assert_eq!(projection_json(&["_id".into()]), "{}");
    }

    #[test]
    fn value_at_walks_dotted_paths() {
        let doc = Document {
            id: DocValue::Int32(1),
            fields: vec![
                (
                    "user".into(),
                    DocValue::Document(vec![
                        ("city".into(), DocValue::Str("London".into())),
                        ("meta".into(), DocValue::Document(vec![])),
                    ]),
                ),
                (
                    "tags".into(),
                    DocValue::Array(vec![DocValue::Str("x".into())]),
                ),
            ],
        };
        assert_eq!(doc.value_at("_id"), Some(&DocValue::Int32(1)));
        assert_eq!(
            doc.value_at("user.city"),
            Some(&DocValue::Str("London".into()))
        );
        assert_eq!(doc.value_at("user.meta"), Some(&DocValue::Document(vec![])));
        assert_eq!(doc.value_at("user.missing"), None);
        // An array is a leaf: no element indexing, matching the schema rollup.
        assert_eq!(doc.value_at("tags.0"), None);
        // Descending through a scalar is not a path, it is a mistake.
        assert_eq!(doc.value_at("user.city.length"), None);
    }

    #[test]
    fn tabular_columns_drops_pure_containers_and_leads_with_id() {
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
            // `user` is a string here: type drift keeps the column, because
            // dropping it would lose this document's value entirely.
            Document {
                id: DocValue::Int32(2),
                fields: vec![("drift".into(), DocValue::Str("s".into()))],
            },
        ];
        let schema = DocSchema::from_documents(&docs);
        let columns = tabular_columns(&schema, 64);
        assert_eq!(columns, vec!["_id", "drift", "name", "user.age"]);
        // `user` itself is only ever a sub-document, so its children carry it.
        assert!(!columns.iter().any(|c| c == "user"));
        // The cap bounds the header the way `doc.max_columns` bounds the grid.
        assert_eq!(tabular_columns(&schema, 2), vec!["_id", "drift"]);
    }

    #[test]
    fn unmapped_fields_spots_what_a_tabular_export_would_drop() {
        let doc = Document {
            id: DocValue::Int32(1),
            fields: vec![
                ("name".into(), DocValue::Str("a".into())),
                (
                    "user".into(),
                    DocValue::Document(vec![("age".into(), DocValue::Int32(30))]),
                ),
            ],
        };
        let covered = [
            "_id".to_string(),
            "name".to_string(),
            "user.age".to_string(),
        ];
        assert!(!has_unmapped_fields(&doc, &covered));
        // The parent path does not stand in for its leaf.
        let parent_only = ["_id".to_string(), "name".to_string(), "user".to_string()];
        assert!(has_unmapped_fields(&doc, &parent_only));
        assert!(has_unmapped_fields(&doc, &["_id".to_string()]));
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
