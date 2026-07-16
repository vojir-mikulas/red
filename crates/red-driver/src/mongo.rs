//! `MongoDriver`: the first [`DocDriver`](crate::DocDriver) implementation, over
//! the official `mongodb` crate. This is the *only* place `mongodb`/`bson` are
//! visible — the conversion between BSON and the [`DocValue`] mirror
//! (`bson_to_doc`/`doc_to_bson`) is the version firewall the plan calls for, so
//! the UI, `red-core`, and the eventual plugin API never see either crate.
//!
//! Covers the whole `DocDriver` surface: connect + identity/topology, the
//! `db → collection` catalog, windowed `find` and the streaming server cursor,
//! count/distinct, schema inference, aggregation, indexes, explain, and the
//! read-only-guarded writes.

use async_trait::async_trait;
use futures_util::StreamExt;
use mongodb::bson::{Bson, Document as BsonDocument, doc};
use mongodb::error::{Error as MongoError, ErrorKind};
use mongodb::options::{ClientOptions, IndexOptions};
use mongodb::results::CollectionType;
use mongodb::{Client, Cursor, IndexModel};
use red_core::doc::{
    CollKind, CollectionInfo, DbInfo, DocCursor, DocPage, DocPlan, DocSchema, DocTopology,
    DocUpdate, DocValue, Document, Filter, FindQuery, IndexInfo, IndexSpec, PlanStage,
};
use red_core::{RedError, Result};

use crate::{AbortSignal, DocDriver};

/// One open MongoDB session. `Client` is cheap to clone (an `Arc` internally)
/// and multiplexes over a pooled connection, so the driver holds one.
pub struct MongoDriver {
    client: Client,
    /// The server version, captured once at connect (`buildInfo.version`).
    version: String,
    /// The deployment topology, detected at connect from `hello`.
    topology: DocTopology,
    /// The connection's read-only posture. The write methods consult it via
    /// [`MongoDriver::ensure_writable`] to refuse writes at the driver, exactly
    /// like `KvDriver`.
    read_only: bool,
}

impl MongoDriver {
    /// Dial a MongoDB deployment from a `mongodb://`/`mongodb+srv://` URI and probe
    /// it (`hello` + `buildInfo`) so a bad host/credential surfaces here rather than
    /// on the first browse. `Client::with_uri_str` connects lazily, so the probe is
    /// what actually forces the connection.
    pub async fn connect(dsn: &str, read_only: bool) -> Result<Self> {
        let options = ClientOptions::parse(dsn).await.map_err(connect_err)?;
        let client = Client::with_options(options).map_err(connect_err)?;

        let admin = client.database("admin");
        // `hello` doubles as the liveness probe and the topology source.
        let hello = admin
            .run_command(doc! { "hello": 1 })
            .await
            .map_err(connect_err)?;
        let topology = topology_from_hello(&hello);
        // Version is best-effort: a locked-down deployment may refuse `buildInfo`,
        // in which case the status bar simply shows an empty version.
        let version = admin
            .run_command(doc! { "buildInfo": 1 })
            .await
            .ok()
            .and_then(|d| d.get_str("version").ok().map(str::to_owned))
            .unwrap_or_default();

        Ok(Self {
            client,
            version,
            topology,
            read_only,
        })
    }

    /// Refuse a write on a read-only connection, the driver-level guard the
    /// human and AI write paths both pass through (defense in depth beside the
    /// service's own read-only check).
    fn ensure_writable(&self) -> Result<()> {
        crate::refuse_if_read_only(self.read_only)
    }

    /// Drain up to `cap` documents from a find cursor into a page, cooperatively
    /// bailing on `abort`. Shared by `find` (and, later, the server-cursor path).
    async fn collect_page(
        cursor: &mut Cursor<BsonDocument>,
        cap: usize,
        abort: &AbortSignal,
    ) -> Result<Vec<Document>> {
        let mut docs = Vec::new();
        while docs.len() < cap {
            if abort.is_aborted() {
                return Err(RedError::Interrupted);
            }
            match cursor.next().await {
                Some(Ok(bdoc)) => docs.push(split_document(bdoc)),
                Some(Err(e)) => return Err(query_err(e)),
                None => break,
            }
        }
        Ok(docs)
    }
}

#[async_trait]
impl DocDriver for MongoDriver {
    async fn ping(&self) -> Result<()> {
        self.client
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .await
            .map(|_| ())
            .map_err(query_err)
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    fn topology(&self) -> DocTopology {
        self.topology
    }

    async fn list_databases(&self) -> Result<Vec<DbInfo>> {
        let specs = self.client.list_databases().await.map_err(query_err)?;
        Ok(specs
            .into_iter()
            .map(|s| DbInfo {
                name: s.name,
                size_on_disk: s.size_on_disk,
                empty: s.empty,
            })
            .collect())
    }

    async fn list_collections(&self, db: &str) -> Result<Vec<CollectionInfo>> {
        let database = self.client.database(db);
        let mut cursor = database.list_collections().await.map_err(query_err)?;
        let mut out = Vec::new();
        while let Some(spec) = cursor.next().await {
            let spec = spec.map_err(query_err)?;
            let kind = match spec.collection_type {
                CollectionType::View => CollKind::View,
                CollectionType::Timeseries => CollKind::Timeseries,
                // `Collection` and any future/unknown type render as a plain
                // collection rather than failing the whole catalog listing.
                _ => CollKind::Collection,
            };
            let capped = spec.options.capped.unwrap_or(false);
            // A cheap O(1) estimate; only meaningful for real collections (a view
            // has no count of its own), and best-effort so one erroring namespace
            // doesn't sink the listing.
            let est_count = if kind == CollKind::Collection {
                database
                    .collection::<BsonDocument>(&spec.name)
                    .estimated_document_count()
                    .await
                    .unwrap_or(0)
            } else {
                0
            };
            out.push(CollectionInfo {
                name: spec.name,
                kind,
                est_count,
                size: 0,
                capped,
            });
        }
        Ok(out)
    }

    async fn find(&self, q: &FindQuery, abort: &AbortSignal) -> Result<DocPage> {
        let filter = q
            .filter
            .as_ref()
            .map(doc_to_bson_document)
            .unwrap_or_default();
        // The window size: the page batch, tightened by an explicit `limit` when
        // the caller set one smaller.
        let cap = q
            .limit
            .map(|l| (l as usize).min(q.batch))
            .unwrap_or(q.batch)
            .max(1);

        let coll = self
            .client
            .database(&q.db)
            .collection::<BsonDocument>(&q.coll);
        let mut find = coll.find(filter).skip(q.skip).limit(cap as i64);
        if let Some(p) = &q.projection {
            find = find.projection(doc_to_bson_document(p));
        }
        if let Some(s) = &q.sort {
            find = find.sort(doc_to_bson_document(s));
        }
        if abort.is_aborted() {
            return Err(RedError::Interrupted);
        }
        let mut cursor = find.await.map_err(query_err)?;
        let docs = Self::collect_page(&mut cursor, cap, abort).await?;
        // This windowed find hands back no live cursor; a short page means the
        // collection is exhausted at this offset, and the service pages the rest
        // by `skip`.
        let exhausted = docs.len() < cap;
        Ok(DocPage {
            docs,
            cursor: None,
            exhausted,
        })
    }

    async fn get_document(&self, db: &str, coll: &str, id: &DocValue) -> Result<Option<Document>> {
        let filter = doc! { "_id": doc_to_bson(id) };
        let found = self
            .client
            .database(db)
            .collection::<BsonDocument>(coll)
            .find_one(filter)
            .await
            .map_err(query_err)?;
        Ok(found.map(split_document))
    }

    async fn count(&self, db: &str, coll: &str, filter: Option<&Filter>) -> Result<u64> {
        let collection = self.client.database(db).collection::<BsonDocument>(coll);
        match filter {
            // An unfiltered total is the O(1) metadata estimate; a filtered one
            // must actually match, so it pays for `countDocuments`.
            None => collection
                .estimated_document_count()
                .await
                .map_err(query_err),
            Some(f) => collection
                .count_documents(doc_to_bson_document(f))
                .await
                .map_err(query_err),
        }
    }

    async fn infer_schema(
        &self,
        db: &str,
        coll: &str,
        sample: usize,
        abort: &AbortSignal,
    ) -> Result<DocSchema> {
        // `$sample` pulls a bounded random subset server-side, so this stays cheap
        // even on a huge collection; the rollup into per-field type stats is the
        // pure `red-core` helper both drivers share.
        let pipeline = vec![doc! { "$sample": { "size": (sample.max(1)) as i64 } }];
        let mut cursor = self
            .client
            .database(db)
            .collection::<BsonDocument>(coll)
            .aggregate(pipeline)
            .await
            .map_err(query_err)?;
        let docs = Self::collect_page(&mut cursor, sample.max(1), abort).await?;
        Ok(DocSchema::from_documents(&docs))
    }

    async fn aggregate(
        &self,
        db: &str,
        coll: &str,
        pipeline: &[DocValue],
        batch: usize,
        abort: &AbortSignal,
    ) -> Result<DocPage> {
        let stages: Vec<BsonDocument> = pipeline.iter().map(doc_to_bson_document).collect();
        if abort.is_aborted() {
            return Err(RedError::Interrupted);
        }
        let mut cursor = self
            .client
            .database(db)
            .collection::<BsonDocument>(coll)
            .aggregate(stages)
            .await
            .map_err(query_err)?;
        let docs = Self::collect_page(&mut cursor, batch, abort).await?;
        let exhausted = docs.len() < batch;
        Ok(DocPage {
            docs,
            cursor: None,
            exhausted,
        })
    }

    async fn indexes(&self, db: &str, coll: &str) -> Result<Vec<IndexInfo>> {
        let mut cursor = self
            .client
            .database(db)
            .collection::<BsonDocument>(coll)
            .list_indexes()
            .await
            .map_err(query_err)?;
        let mut out = Vec::new();
        while let Some(model) = cursor.next().await {
            let model = model.map_err(query_err)?;
            let keys = model
                .keys
                .iter()
                .map(|(field, order)| (field.clone(), index_order(order)))
                .collect();
            let opts = model.options;
            out.push(IndexInfo {
                name: opts
                    .as_ref()
                    .and_then(|o| o.name.clone())
                    .unwrap_or_default(),
                keys,
                unique: opts.as_ref().and_then(|o| o.unique).unwrap_or(false),
                sparse: opts.as_ref().and_then(|o| o.sparse).unwrap_or(false),
                ttl: opts
                    .as_ref()
                    .and_then(|o| o.expire_after)
                    .map(|d| d.as_secs() as i64),
                partial: opts
                    .as_ref()
                    .is_some_and(|o| o.partial_filter_expression.is_some()),
            });
        }
        Ok(out)
    }

    async fn explain(&self, q: &FindQuery) -> Result<DocPlan> {
        let filter = q
            .filter
            .as_ref()
            .map(doc_to_bson_document)
            .unwrap_or_default();
        // `explain` isn't a first-class typed call, so run it as a command with
        // execution stats (which fills in the examined/returned numbers).
        let reply = self
            .client
            .database(&q.db)
            .run_command(doc! {
                "explain": { "find": &q.coll, "filter": filter },
                "verbosity": "executionStats",
            })
            .await
            .map_err(query_err)?;

        let planner = reply
            .get_document("queryPlanner")
            .map_err(|_| RedError::Query("explain returned no queryPlanner".into()))?;
        let winning = planner
            .get_document("winningPlan")
            .map_err(|_| RedError::Query("explain returned no winningPlan".into()))?;
        // Mongo 5+ (SBE) nests the classic stage tree under `queryPlan`.
        let plan_root = winning.get_document("queryPlan").unwrap_or(winning);

        let mut stages = Vec::new();
        let mut index_used = None;
        let mut collscan = false;
        flatten_plan(plan_root, 0, &mut stages, &mut index_used, &mut collscan);

        let exec = reply.get_document("executionStats").ok();
        Ok(DocPlan {
            stages,
            index_used,
            docs_examined: exec.and_then(|e| get_num(e, "totalDocsExamined")),
            n_returned: exec.and_then(|e| get_num(e, "nReturned")),
            collscan,
        })
    }

    async fn distinct(
        &self,
        db: &str,
        coll: &str,
        field: &str,
        filter: Option<&Filter>,
    ) -> Result<Vec<DocValue>> {
        let filter = filter.map(doc_to_bson_document).unwrap_or_default();
        let values = self
            .client
            .database(db)
            .collection::<BsonDocument>(coll)
            .distinct(field, filter)
            .await
            .map_err(query_err)?;
        Ok(values.into_iter().map(bson_to_doc).collect())
    }

    async fn next_batch(&self, cursor: &DocCursor, batch: usize) -> Result<DocPage> {
        // A live server cursor is advanced with the low-level `getMore` command
        // (the typed `Cursor` object can't be reconstructed from just its id).
        let reply = self
            .client
            .database(&cursor.db)
            .run_command(doc! {
                "getMore": cursor.id,
                "collection": &cursor.coll,
                "batchSize": batch as i64,
            })
            .await
            .map_err(query_err)?;
        parse_cursor_reply(&reply, &cursor.db, &cursor.coll, "nextBatch")
    }

    async fn close_cursor(&self, cursor: &DocCursor) {
        // Best-effort: a failed kill just lets the cursor time out server-side.
        let _ = self
            .client
            .database(&cursor.db)
            .run_command(doc! { "killCursors": &cursor.coll, "cursors": [cursor.id] })
            .await;
    }

    fn parse_ext_json(&self, text: &str) -> Result<DocValue> {
        let json: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| RedError::Query(format!("invalid JSON: {e}")))?;
        // `Bson::try_from` interprets extended-JSON `$`-tags (`$oid`, `$date`, …),
        // so a typed filter keeps its BSON types rather than degrading to strings.
        let bson = Bson::try_from(json)
            .map_err(|e| RedError::Query(format!("invalid extended JSON: {e}")))?;
        Ok(bson_to_doc(bson))
    }

    async fn insert(&self, db: &str, coll: &str, docs: &[Document]) -> Result<u64> {
        self.ensure_writable()?;
        let bson: Vec<BsonDocument> = docs.iter().map(document_to_bson).collect();
        let result = self
            .client
            .database(db)
            .collection::<BsonDocument>(coll)
            .insert_many(bson)
            .await
            .map_err(query_err)?;
        Ok(result.inserted_ids.len() as u64)
    }

    async fn update(
        &self,
        db: &str,
        coll: &str,
        filter: &Filter,
        change: &DocUpdate,
        many: bool,
    ) -> Result<u64> {
        self.ensure_writable()?;
        let coll = self.client.database(db).collection::<BsonDocument>(coll);
        let filter = doc_to_bson_document(filter);
        let modified = match change {
            // A `$set` patch merges the given fields; a `Replace` swaps the whole
            // document (single-match only — `replaceMany` doesn't exist).
            DocUpdate::Patch(patch) => {
                let update = doc! { "$set": doc_to_bson_document(patch) };
                if many {
                    coll.update_many(filter, update)
                        .await
                        .map_err(query_err)?
                        .modified_count
                } else {
                    coll.update_one(filter, update)
                        .await
                        .map_err(query_err)?
                        .modified_count
                }
            }
            DocUpdate::Replace(doc) => {
                coll.replace_one(filter, document_to_bson(doc))
                    .await
                    .map_err(query_err)?
                    .modified_count
            }
        };
        Ok(modified)
    }

    async fn replace(&self, db: &str, coll: &str, id: &DocValue, doc: &Document) -> Result<()> {
        self.ensure_writable()?;
        self.client
            .database(db)
            .collection::<BsonDocument>(coll)
            .replace_one(doc! { "_id": doc_to_bson(id) }, document_to_bson(doc))
            .await
            .map(|_| ())
            .map_err(query_err)
    }

    async fn delete(&self, db: &str, coll: &str, filter: &Filter, many: bool) -> Result<u64> {
        self.ensure_writable()?;
        let coll = self.client.database(db).collection::<BsonDocument>(coll);
        let filter = doc_to_bson_document(filter);
        let deleted = if many {
            coll.delete_many(filter)
                .await
                .map_err(query_err)?
                .deleted_count
        } else {
            coll.delete_one(filter)
                .await
                .map_err(query_err)?
                .deleted_count
        };
        Ok(deleted)
    }

    async fn create_collection(&self, db: &str, coll: &str) -> Result<()> {
        self.ensure_writable()?;
        self.client
            .database(db)
            .create_collection(coll)
            .await
            .map_err(query_err)
    }

    async fn drop_collection(&self, db: &str, coll: &str) -> Result<()> {
        self.ensure_writable()?;
        self.client
            .database(db)
            .collection::<BsonDocument>(coll)
            .drop()
            .await
            .map_err(query_err)
    }

    async fn create_index(&self, db: &str, coll: &str, spec: &IndexSpec) -> Result<()> {
        self.ensure_writable()?;
        let keys: BsonDocument = spec
            .keys
            .iter()
            .map(|(field, dir)| (field.clone(), Bson::Int32(*dir)))
            .collect();
        let options = IndexOptions::builder()
            .unique(spec.unique)
            .name(spec.name.clone())
            .build();
        let model = IndexModel::builder().keys(keys).options(options).build();
        self.client
            .database(db)
            .collection::<BsonDocument>(coll)
            .create_index(model)
            .await
            .map(|_| ())
            .map_err(query_err)
    }
}

// --- topology / error mapping ------------------------------------------------

/// Read the deployment topology off a `hello` reply: a mongos router announces
/// itself via `msg: "isdbgrid"`, a replica-set member carries a `setName`, and
/// anything else is a standalone.
fn topology_from_hello(hello: &BsonDocument) -> DocTopology {
    if hello.get_str("msg") == Ok("isdbgrid") {
        DocTopology::Sharded
    } else if hello.contains_key("setName") {
        DocTopology::ReplicaSet
    } else {
        DocTopology::Standalone
    }
}

/// A connect-time error: an authentication failure is user-correctable (bad
/// credentials), so it maps to [`RedError::Auth`] and stops the UI's retry loop;
/// anything else is a transient/network [`RedError::Connect`].
fn connect_err(e: MongoError) -> RedError {
    if matches!(*e.kind, ErrorKind::Authentication { .. }) {
        RedError::Auth(e.to_string())
    } else {
        RedError::Connect(e.to_string())
    }
}

/// A query-time error (browse/find/count): a failed operation, not a connect.
fn query_err(e: MongoError) -> RedError {
    RedError::Query(e.to_string())
}

/// Render an index key's order value as the string the panel shows: a b-tree
/// direction (`1`/`-1`) or a special-index tag (`text`/`2dsphere`/`hashed`).
fn index_order(order: &Bson) -> String {
    match order {
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(x) => x.to_string(),
        Bson::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Read a numeric explain stat as `u64`, tolerating either BSON int width or a
/// double (explain reports these inconsistently across server versions).
fn get_num(doc: &BsonDocument, key: &str) -> Option<u64> {
    match doc.get(key) {
        Some(Bson::Int64(n)) => Some(*n as u64),
        Some(Bson::Int32(n)) => Some(*n as u64),
        Some(Bson::Double(x)) => Some(*x as u64),
        _ => None,
    }
}

/// Flatten an `explain` winning-plan tree (depth-first) into the panel's stage
/// list, recording the index an `IXSCAN` used and whether any stage is a
/// `COLLSCAN`. Recurses through both the single `inputStage` and the fan-out
/// `inputStages` (an `OR`/merge).
fn flatten_plan(
    node: &BsonDocument,
    depth: usize,
    out: &mut Vec<PlanStage>,
    index_used: &mut Option<String>,
    collscan: &mut bool,
) {
    let stage = node.get_str("stage").unwrap_or_default().to_string();
    if stage == "COLLSCAN" {
        *collscan = true;
    }
    let detail = node.get_str("indexName").ok().map(|name| {
        if index_used.is_none() {
            *index_used = Some(name.to_string());
        }
        name.to_string()
    });
    out.push(PlanStage {
        stage,
        depth,
        detail,
    });
    if let Ok(input) = node.get_document("inputStage") {
        flatten_plan(input, depth + 1, out, index_used, collscan);
    }
    if let Some(Bson::Array(stages)) = node.get("inputStages") {
        for child in stages {
            if let Bson::Document(d) = child {
                flatten_plan(d, depth + 1, out, index_used, collscan);
            }
        }
    }
}

/// Turn a `find`/`getMore` command's `cursor` sub-document into a [`DocPage`].
/// `batch_field` is `"firstBatch"` for a fresh cursor or `"nextBatch"` for a
/// `getMore`. A returned cursor `id` of `0` means the server exhausted it.
fn parse_cursor_reply(
    reply: &BsonDocument,
    db: &str,
    coll: &str,
    batch_field: &str,
) -> Result<DocPage> {
    let cursor = reply
        .get_document("cursor")
        .map_err(|_| RedError::Query("cursor reply had no cursor document".into()))?;
    let id = cursor.get_i64("id").unwrap_or(0);
    let docs = match cursor.get_array(batch_field) {
        Ok(batch) => batch
            .iter()
            .filter_map(|b| match b {
                Bson::Document(d) => Some(split_document(d.clone())),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    let live = (id != 0).then(|| DocCursor {
        id,
        db: db.to_string(),
        coll: coll.to_string(),
    });
    Ok(DocPage {
        docs,
        exhausted: live.is_none(),
        cursor: live,
    })
}

// --- BSON <-> DocValue conversion (the firewall) -----------------------------

/// Split a wire document into a [`Document`], pulling `_id` out from the rest
/// while preserving the stored order of the remaining fields.
fn split_document(bdoc: BsonDocument) -> Document {
    let mut id = DocValue::Null;
    let mut fields = Vec::with_capacity(bdoc.len());
    for (k, v) in bdoc {
        if k == "_id" {
            id = bson_to_doc(v);
        } else {
            fields.push((k, bson_to_doc(v)));
        }
    }
    Document { id, fields }
}

/// Convert a BSON value into the [`DocValue`] mirror. Every type that survives is
/// covered explicitly; the JSON-lossy ones (`ObjectId`/`DateTime`/`Decimal128`/
/// `Binary`/`Timestamp`/`Regex`) keep their identity rather than degrading to a
/// string. The rare sentinel/legacy types (`MinKey`/`MaxKey`/code/`DbPointer`)
/// have no `DocValue` arm and fold to the nearest representable value.
fn bson_to_doc(b: Bson) -> DocValue {
    match b {
        Bson::Null | Bson::Undefined => DocValue::Null,
        Bson::Boolean(v) => DocValue::Bool(v),
        Bson::Int32(n) => DocValue::Int32(n),
        Bson::Int64(n) => DocValue::Int64(n),
        Bson::Double(x) => DocValue::Double(x),
        Bson::Decimal128(d) => DocValue::Decimal128(d.to_string()),
        Bson::String(s) => DocValue::Str(s),
        Bson::ObjectId(oid) => DocValue::ObjectId(oid.bytes()),
        Bson::DateTime(dt) => DocValue::DateTime(dt.timestamp_millis()),
        Bson::Timestamp(ts) => DocValue::Timestamp(((ts.time as u64) << 32) | ts.increment as u64),
        Bson::Binary(bin) => DocValue::Binary {
            subtype: u8::from(bin.subtype),
            bytes: bin.bytes,
        },
        Bson::RegularExpression(re) => DocValue::Regex {
            pattern: re.pattern,
            options: re.options,
        },
        Bson::Array(items) => DocValue::Array(items.into_iter().map(bson_to_doc).collect()),
        Bson::Document(d) => {
            DocValue::Document(d.into_iter().map(|(k, v)| (k, bson_to_doc(v))).collect())
        }
        // Legacy/edge types with no dedicated mirror arm.
        Bson::Symbol(s) | Bson::JavaScriptCode(s) => DocValue::Str(s),
        Bson::JavaScriptCodeWithScope(c) => DocValue::Str(c.code),
        Bson::MinKey => DocValue::Str("MinKey".into()),
        Bson::MaxKey => DocValue::Str("MaxKey".into()),
        Bson::DbPointer(_) => DocValue::Null,
    }
}

/// Convert a [`DocValue`] back into a BSON value (for `_id` filters and, later,
/// writes). The inverse of [`bson_to_doc`]; a `Decimal128` that fails to reparse
/// falls back to a string so a malformed value can't panic the driver.
fn doc_to_bson(v: &DocValue) -> Bson {
    match v {
        DocValue::Null => Bson::Null,
        DocValue::Bool(b) => Bson::Boolean(*b),
        DocValue::Int32(n) => Bson::Int32(*n),
        DocValue::Int64(n) => Bson::Int64(*n),
        DocValue::Double(x) => Bson::Double(*x),
        DocValue::Decimal128(s) => s
            .parse::<mongodb::bson::Decimal128>()
            .map(Bson::Decimal128)
            .unwrap_or_else(|_| Bson::String(s.clone())),
        DocValue::Str(s) => Bson::String(s.clone()),
        DocValue::ObjectId(bytes) => {
            Bson::ObjectId(mongodb::bson::oid::ObjectId::from_bytes(*bytes))
        }
        DocValue::DateTime(ms) => Bson::DateTime(mongodb::bson::DateTime::from_millis(*ms)),
        DocValue::Timestamp(ts) => Bson::Timestamp(mongodb::bson::Timestamp {
            time: (ts >> 32) as u32,
            increment: (*ts & 0xffff_ffff) as u32,
        }),
        DocValue::Binary { subtype, bytes } => Bson::Binary(mongodb::bson::Binary {
            subtype: (*subtype).into(),
            bytes: bytes.clone(),
        }),
        DocValue::Regex { pattern, options } => Bson::RegularExpression(mongodb::bson::Regex {
            pattern: pattern.clone(),
            options: options.clone(),
        }),
        DocValue::Array(items) => Bson::Array(items.iter().map(doc_to_bson).collect()),
        DocValue::Document(fields) => Bson::Document(
            fields
                .iter()
                .map(|(k, val)| (k.clone(), doc_to_bson(val)))
                .collect(),
        ),
    }
}

/// Convert a [`DocValue`] to a BSON document for a filter/projection/sort. A
/// non-document value (a malformed filter) degrades to an empty document — match
/// everything — rather than erroring the browse.
fn doc_to_bson_document(v: &DocValue) -> BsonDocument {
    match doc_to_bson(v) {
        Bson::Document(d) => d,
        _ => BsonDocument::new(),
    }
}

/// Convert a whole [`Document`] (with its `_id`) to a wire BSON document, for an
/// insert/replace. The inverse of [`split_document`].
fn document_to_bson(doc: &Document) -> BsonDocument {
    doc_to_bson_document(&doc.to_doc_value())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::{Binary, spec::BinarySubtype};

    #[test]
    fn bson_docvalue_roundtrip_lossy_types() {
        let cases = vec![
            Bson::Null,
            Bson::Boolean(true),
            Bson::Int32(7),
            Bson::Int64(-9),
            Bson::Double(1.5),
            Bson::String("hi".into()),
            Bson::ObjectId(mongodb::bson::oid::ObjectId::from_bytes([1; 12])),
            Bson::DateTime(mongodb::bson::DateTime::from_millis(1_609_459_200_000)),
            Bson::Timestamp(mongodb::bson::Timestamp {
                time: 100,
                increment: 3,
            }),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![1, 2, 3],
            }),
            Bson::RegularExpression(mongodb::bson::Regex {
                pattern: "^a".into(),
                options: "i".into(),
            }),
        ];
        for original in cases {
            let mirrored = bson_to_doc(original.clone());
            let back = doc_to_bson(&mirrored);
            assert_eq!(back, original, "round-trip lost fidelity");
        }
    }

    #[test]
    fn split_document_pulls_id_and_keeps_order() {
        let bdoc = doc! { "b": 2, "_id": 5, "a": 1 };
        let d = split_document(bdoc);
        assert_eq!(d.id, DocValue::Int32(5));
        // `_id` removed; the remaining fields keep their stored order.
        assert_eq!(
            d.fields,
            vec![
                ("b".to_string(), DocValue::Int32(2)),
                ("a".to_string(), DocValue::Int32(1)),
            ]
        );
    }

    #[test]
    fn topology_detection() {
        assert_eq!(
            topology_from_hello(&doc! { "msg": "isdbgrid" }),
            DocTopology::Sharded
        );
        assert_eq!(
            topology_from_hello(&doc! { "setName": "rs0" }),
            DocTopology::ReplicaSet
        );
        assert_eq!(
            topology_from_hello(&doc! { "ok": 1.0 }),
            DocTopology::Standalone
        );
    }
}
