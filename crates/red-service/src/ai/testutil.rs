//! Test doubles and helpers shared by more than one module's tests.
//!
//! A double lives here only when two modules genuinely need it: the MongoDB stub
//! backs the doc-seam tests *and* the cross-seam catalog wiring test, and the
//! `assess_write` shim spares every write-gate test from restating the dialect.

use std::sync::Arc;

use red_ai::CancelToken;
use red_core::doc::{
    CollKind, DocPage, DocPlan, DocSchema, DocUpdate, DocValue, Document, FindQuery, IndexInfo,
    IndexSpec,
};
use red_core::sql::Dialect;
use red_core::{AiPolicy, RedError};
use red_driver::{AbortSignal, DocDriver};
use serde_json::Value as Json;

use super::doc::doc_run_tool;
use super::gate::WriteAssessment;
use super::state::ReportSink;

/// The gate tests probe statement shapes, not dialect lexing, so they run under
/// [`Dialect::Generic`]; the dialect-sensitive cases pass their own.
pub(super) fn assess_write(name: &str, input: &Json, policy: &AiPolicy) -> WriteAssessment {
    super::gate::assess_write(name, input, policy, Dialect::Generic)
}

/// A minimal in-memory `DocDriver` for the doc-seam tools. Purpose-built for
/// what they actually exercise — the catalog, an inferred schema, a windowed
/// `find`, and a **filtered** `count` — because a `count` that ignores its
/// filter would report every reference as fully resolving, which is exactly
/// the failure `doc_reference_map` exists to catch.
struct DocStub {
    colls: Vec<(String, Vec<Document>)>,
}
impl DocStub {
    fn docs(&self, coll: &str) -> &[Document] {
        self.colls
            .iter()
            .find(|(name, _)| name == coll)
            .map(|(_, docs)| docs.as_slice())
            .unwrap_or(&[])
    }
}
#[async_trait::async_trait]
impl DocDriver for DocStub {
    async fn ping(&self) -> red_core::Result<()> {
        Ok(())
    }
    fn server_version(&self) -> String {
        "7.0.0".into()
    }
    fn topology(&self) -> red_core::doc::DocTopology {
        red_core::doc::DocTopology::Standalone
    }
    async fn list_databases(&self) -> red_core::Result<Vec<red_core::doc::DbInfo>> {
        Ok(vec![red_core::doc::DbInfo {
            name: "app".into(),
            size_on_disk: 0,
            empty: false,
        }])
    }
    async fn list_collections(
        &self,
        _db: &str,
    ) -> red_core::Result<Vec<red_core::doc::CollectionInfo>> {
        Ok(self
            .colls
            .iter()
            .map(|(name, docs)| red_core::doc::CollectionInfo {
                name: name.clone(),
                kind: CollKind::Collection,
                est_count: docs.len() as u64,
                size: 0,
                capped: false,
                validator: None,
            })
            .collect())
    }
    async fn find(&self, q: &FindQuery, _abort: &AbortSignal) -> red_core::Result<DocPage> {
        let all = self.docs(&q.coll);
        let take = q.limit.map(|l| l as usize).unwrap_or(q.batch);
        Ok(DocPage {
            docs: all.iter().take(take).cloned().collect(),
            cursor: None,
            exhausted: true,
        })
    }
    async fn find_seek(
        &self,
        _db: &str,
        _coll: &str,
        _filter: Option<&red_core::doc::Filter>,
        _seek: red_core::doc::DocSeek,
        _limit: usize,
        _abort: &AbortSignal,
    ) -> red_core::Result<Vec<Document>> {
        Ok(Vec::new())
    }
    async fn get_document(
        &self,
        _db: &str,
        coll: &str,
        id: &DocValue,
    ) -> red_core::Result<Option<Document>> {
        Ok(self.docs(coll).iter().find(|d| &d.id == id).cloned())
    }
    /// Understands exactly one filter shape: `{_id: {$in: [...]}}`, the probe
    /// `doc_reference_map` issues. Anything else counts everything.
    async fn count(
        &self,
        _db: &str,
        coll: &str,
        filter: Option<&red_core::doc::Filter>,
    ) -> red_core::Result<u64> {
        let docs = self.docs(coll);
        let Some(DocValue::Document(fields)) = filter else {
            return Ok(docs.len() as u64);
        };
        let wanted = fields.iter().find(|(k, _)| k == "_id").and_then(|(_, v)| {
            let DocValue::Document(ops) = v else {
                return None;
            };
            ops.iter().find(|(k, _)| k == "$in").map(|(_, v)| v)
        });
        let Some(DocValue::Array(ids)) = wanted else {
            return Ok(docs.len() as u64);
        };
        Ok(docs.iter().filter(|d| ids.contains(&d.id)).count() as u64)
    }
    async fn infer_schema(
        &self,
        _db: &str,
        coll: &str,
        sample: usize,
        _abort: &AbortSignal,
    ) -> red_core::Result<DocSchema> {
        let docs = self.docs(coll);
        Ok(DocSchema::from_documents(&docs[..docs.len().min(sample)]))
    }
    async fn aggregate(
        &self,
        _db: &str,
        _coll: &str,
        _pipeline: &[DocValue],
        _batch: usize,
        _abort: &AbortSignal,
    ) -> red_core::Result<DocPage> {
        Ok(DocPage {
            docs: Vec::new(),
            cursor: None,
            exhausted: true,
        })
    }
    async fn indexes(&self, _db: &str, _coll: &str) -> red_core::Result<Vec<IndexInfo>> {
        Ok(Vec::new())
    }
    async fn explain(&self, _q: &FindQuery) -> red_core::Result<DocPlan> {
        Ok(DocPlan {
            stages: Vec::new(),
            index_used: None,
            docs_examined: None,
            n_returned: None,
            collscan: true,
        })
    }
    async fn distinct(
        &self,
        _db: &str,
        _coll: &str,
        _field: &str,
        _filter: Option<&red_core::doc::Filter>,
    ) -> red_core::Result<Vec<DocValue>> {
        Ok(Vec::new())
    }
    async fn next_batch(
        &self,
        _cursor: &red_core::doc::DocCursor,
        _batch: usize,
    ) -> red_core::Result<DocPage> {
        Ok(DocPage {
            docs: Vec::new(),
            cursor: None,
            exhausted: true,
        })
    }
    async fn close_cursor(&self, _cursor: &red_core::doc::DocCursor) {}
    /// Enough extended JSON for the tool arguments under test: plain JSON
    /// plus `{"$oid": …}`. The real dialect is the engine's; this only has
    /// to round-trip what the tests pass in.
    fn parse_ext_json(&self, text: &str) -> red_core::Result<DocValue> {
        fn convert(v: &Json) -> DocValue {
            match v {
                Json::Null => DocValue::Null,
                Json::Bool(b) => DocValue::Bool(*b),
                Json::Number(n) => match n.as_i64() {
                    Some(i) if i32::try_from(i).is_ok() => DocValue::Int32(i as i32),
                    Some(i) => DocValue::Int64(i),
                    None => DocValue::Double(n.as_f64().unwrap_or(0.0)),
                },
                Json::String(s) => DocValue::Str(s.clone()),
                Json::Array(items) => DocValue::Array(items.iter().map(convert).collect()),
                Json::Object(map) => {
                    if let Some(Json::String(hex)) = map.get("$oid")
                        && let Ok(bytes) = <[u8; 12]>::try_from(
                            (0..hex.len().min(24))
                                .step_by(2)
                                .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
                                .collect::<Vec<u8>>(),
                        )
                    {
                        return DocValue::ObjectId(bytes);
                    }
                    DocValue::Document(map.iter().map(|(k, v)| (k.clone(), convert(v))).collect())
                }
            }
        }
        serde_json::from_str::<Json>(text)
            .map(|v| convert(&v))
            .map_err(|e| RedError::Query(e.to_string()))
    }
    async fn insert(&self, _db: &str, _coll: &str, _docs: &[Document]) -> red_core::Result<u64> {
        Err(RedError::Driver("read-only stub".into()))
    }
    async fn update(
        &self,
        _db: &str,
        _coll: &str,
        _filter: &red_core::doc::Filter,
        _change: &DocUpdate,
        _many: bool,
    ) -> red_core::Result<u64> {
        Err(RedError::Driver("read-only stub".into()))
    }
    async fn replace(
        &self,
        _db: &str,
        _coll: &str,
        _id: &DocValue,
        _doc: &Document,
    ) -> red_core::Result<()> {
        Err(RedError::Driver("read-only stub".into()))
    }
    async fn delete(
        &self,
        _db: &str,
        _coll: &str,
        _filter: &red_core::doc::Filter,
        _many: bool,
    ) -> red_core::Result<u64> {
        Err(RedError::Driver("read-only stub".into()))
    }
    async fn create_collection(&self, _db: &str, _coll: &str) -> red_core::Result<()> {
        Err(RedError::Driver("read-only stub".into()))
    }
    async fn drop_collection(&self, _db: &str, _coll: &str) -> red_core::Result<()> {
        Err(RedError::Driver("read-only stub".into()))
    }
    async fn create_index(
        &self,
        _db: &str,
        _coll: &str,
        _spec: &IndexSpec,
    ) -> red_core::Result<()> {
        Err(RedError::Driver("read-only stub".into()))
    }
}

/// `customers` holds ids 1..=3. `orders.customer_id` points at two of them
/// and one stranger; `orders.customerRef` points at nothing.
pub(super) fn doc_stub() -> Arc<dyn DocDriver> {
    let customers = (1..=3)
        .map(|id| Document {
            id: DocValue::Int32(id),
            fields: vec![("name".into(), DocValue::Str(format!("c{id}")))],
        })
        .collect();
    let orders = (1..=3)
        .map(|i| Document {
            id: DocValue::Int32(100 + i),
            fields: vec![
                ("customer_id".into(), DocValue::Int32(i)),
                ("customerRef".into(), DocValue::Int32(900 + i)),
            ],
        })
        .collect();
    Arc::new(DocStub {
        colls: vec![
            ("customers".to_string(), customers),
            ("orders".to_string(), orders),
        ],
    })
}
pub(super) async fn doc_tool(
    driver: &Arc<dyn DocDriver>,
    name: &str,
    input: Json,
) -> (String, bool) {
    doc_run_tool(
        driver,
        name,
        &input,
        &AiPolicy::default(),
        &CancelToken::new(),
        &ReportSink::disabled(),
    )
    .await
}
