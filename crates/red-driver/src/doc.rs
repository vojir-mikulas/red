//! `DocDriver`: the third seam, for document stores (MongoDB today; see
//! `docs/plans/todo/doc-driver.md`). Neither SQL-shaped (`DatabaseDriver`) nor
//! Redis-shaped (`KvDriver`): a `server → databases → collections → documents`
//! hierarchy of nested BSON trees, queried by `find`/`aggregate` rather than SQL
//! or `GET`/`SET`. Object-safe like the other two seams, held as
//! `Arc<dyn DocDriver>`, one impl per engine.
//!
//! This is the read path (D0 catalog + windowed `find` + count, plus D1's
//! `infer_schema`/`aggregate`/`indexes`/`explain`/`distinct`, the streaming server
//! cursor, and extended-JSON parsing). Writes land in D2, added additively to this
//! trait.

use async_trait::async_trait;
use red_core::Result;
use red_core::doc::{
    CollectionInfo, DbInfo, DocCursor, DocPage, DocPlan, DocSchema, DocTopology, DocValue,
    Document, Filter, FindQuery, IndexInfo,
};

use crate::AbortSignal;

/// One open document-store session. The parallel seam to [`DatabaseDriver`](crate::DatabaseDriver)
/// and [`KvDriver`](crate::KvDriver) for engines that are document-shaped.
/// Object-safe so the service can hold `Arc<dyn DocDriver>` and swap engines
/// behind it, mirroring how the other two seams are held.
#[async_trait]
pub trait DocDriver: Send + Sync {
    /// Cheap liveness probe: touches the underlying connection.
    async fn ping(&self) -> Result<()>;

    /// Engine version string (e.g. `"7.0.5"`), for the status bar. Cheap and
    /// synchronous; captured once at connect.
    fn server_version(&self) -> String;

    /// The deployment topology detected at connect (standalone / replica set /
    /// sharded), mirroring `KvDriver::topology`.
    fn topology(&self) -> DocTopology;

    /// The databases on the server (`listDatabases`), the top level of the
    /// hierarchy the flat KV seam can't express.
    async fn list_databases(&self) -> Result<Vec<DbInfo>>;

    /// The collections in one database (`listCollections` + cheap `collStats`),
    /// with estimated counts/sizes and `capped`/`view`/`timeseries` kind — the
    /// schema-tree level a table catalog maps onto.
    async fn list_collections(&self, db: &str) -> Result<Vec<CollectionInfo>>;

    /// One window of a collection (`find` with `skip`/`limit`), cancellable via
    /// `abort` and capped by `q.batch`. The browse read: never materializes the
    /// whole collection, the same streaming discipline the SQL cursor holds. In
    /// D0 the window is `skip`/`limit`-addressed (random access); D1 adds the
    /// stateful server cursor (`getMore`) for large forward scans.
    async fn find(&self, q: &FindQuery, abort: &AbortSignal) -> Result<DocPage>;

    /// One document by `_id` (`findOne({_id})`), for the inspector's
    /// full-fidelity raw-document view. `Ok(None)` when no document matches, not
    /// an error.
    async fn get_document(&self, db: &str, coll: &str, id: &DocValue) -> Result<Option<Document>>;

    /// The number of documents matching `filter` (`countDocuments`), or the
    /// estimate (`estimatedDocumentCount`) when `filter` is `None` — the cheap,
    /// O(1) total the grid shows when the whole collection is browsed unfiltered.
    async fn count(&self, db: &str, coll: &str, filter: Option<&Filter>) -> Result<u64>;

    // --- D1 power reads ----------------------------------------------------

    /// Infer a collection's schema by sampling up to `sample` documents
    /// (`$sample`) and rolling their fields into per-path type frequencies and
    /// present-ratios. What makes a schemaless collection legible: the discovered
    /// schema the SQL seam gets for free but a document store must derive.
    /// Cancellable via `abort`.
    async fn infer_schema(
        &self,
        db: &str,
        coll: &str,
        sample: usize,
        abort: &AbortSignal,
    ) -> Result<DocSchema>;

    /// Run a read-only aggregation `pipeline` (an array of stage documents) and
    /// return one window of results, capped by `batch` and cancellable via
    /// `abort`. Mongo's real analytical engine; write stages (`$out`/`$merge`)
    /// are the caller's to reject before this runs.
    async fn aggregate(
        &self,
        db: &str,
        coll: &str,
        pipeline: &[DocValue],
        batch: usize,
        abort: &AbortSignal,
    ) -> Result<DocPage>;

    /// A collection's indexes (`listIndexes`) with keys / unique / sparse / ttl /
    /// partial, for the indexes panel.
    async fn indexes(&self, db: &str, coll: &str) -> Result<Vec<IndexInfo>>;

    /// `explain` a find query: the winning plan, the index used (if any), and the
    /// examined/returned numbers, so the UI can flag a `COLLSCAN` and the "missing
    /// index" case.
    async fn explain(&self, q: &FindQuery) -> Result<DocPlan>;

    /// The distinct values of `field` over documents matching `filter`
    /// (`distinct`), for cheap cardinality without pulling documents.
    async fn distinct(
        &self,
        db: &str,
        coll: &str,
        field: &str,
        filter: Option<&Filter>,
    ) -> Result<Vec<DocValue>>;

    /// Advance a live server cursor (`getMore`), returning the next window and an
    /// updated cursor (`None` once the server exhausts it). Paired with a
    /// `find`/`aggregate` that opened the cursor; `batch` bounds the window.
    async fn next_batch(&self, cursor: &DocCursor, batch: usize) -> Result<DocPage>;

    /// Close a live server cursor early (`killCursors`) when the UI abandons a
    /// scan before exhausting it, so the server doesn't hold it open. Best-effort:
    /// a failed close is not worth surfacing (the cursor times out server-side).
    async fn close_cursor(&self, cursor: &DocCursor);

    /// Parse an extended-JSON string (a filter document, or an aggregation
    /// pipeline array) into a [`DocValue`]. Engine-specific because the
    /// extended-JSON dialect is the engine's, kept off the pure `red-core` types.
    /// A syntax error surfaces as a [`red_core::RedError::Query`].
    fn parse_ext_json(&self, text: &str) -> Result<DocValue>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_core::doc::{CollKind, DocCursor};
    use std::collections::BTreeMap;

    /// An in-memory `DocDriver` over a fixed set of collections, for exercising
    /// the seam without a live mongod. The reusable analog of the Redis `StubKv`
    /// test double; promoted out of `#[cfg(test)]` when the UI/AI phases need it.
    struct FakeDocDriver {
        version: String,
        /// `db → coll → documents`, in insertion order per collection.
        data: BTreeMap<String, BTreeMap<String, Vec<Document>>>,
    }

    impl FakeDocDriver {
        fn docs(&self, db: &str, coll: &str) -> &[Document] {
            self.data
                .get(db)
                .and_then(|c| c.get(coll))
                .map(Vec::as_slice)
                .unwrap_or(&[])
        }
    }

    #[async_trait]
    impl DocDriver for FakeDocDriver {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        fn server_version(&self) -> String {
            self.version.clone()
        }
        fn topology(&self) -> DocTopology {
            DocTopology::Standalone
        }
        async fn list_databases(&self) -> Result<Vec<DbInfo>> {
            Ok(self
                .data
                .keys()
                .map(|name| DbInfo {
                    name: name.clone(),
                    size_on_disk: 0,
                    empty: false,
                })
                .collect())
        }
        async fn list_collections(&self, db: &str) -> Result<Vec<CollectionInfo>> {
            Ok(self
                .data
                .get(db)
                .into_iter()
                .flat_map(|c| c.iter())
                .map(|(name, docs)| CollectionInfo {
                    name: name.clone(),
                    kind: CollKind::Collection,
                    est_count: docs.len() as u64,
                    size: 0,
                    capped: false,
                })
                .collect())
        }
        async fn find(&self, q: &FindQuery, _abort: &AbortSignal) -> Result<DocPage> {
            let all = self.docs(&q.db, &q.coll);
            let skip = q.skip as usize;
            let take = q.limit.map(|l| l as usize).unwrap_or(q.batch).min(q.batch);
            let docs: Vec<Document> = all.iter().skip(skip).take(take).cloned().collect();
            let exhausted = skip + docs.len() >= all.len();
            Ok(DocPage {
                docs,
                cursor: None,
                exhausted,
            })
        }
        async fn get_document(
            &self,
            db: &str,
            coll: &str,
            id: &DocValue,
        ) -> Result<Option<Document>> {
            Ok(self.docs(db, coll).iter().find(|d| &d.id == id).cloned())
        }
        async fn count(&self, db: &str, coll: &str, _filter: Option<&Filter>) -> Result<u64> {
            Ok(self.docs(db, coll).len() as u64)
        }
        async fn infer_schema(
            &self,
            db: &str,
            coll: &str,
            sample: usize,
            _abort: &AbortSignal,
        ) -> Result<DocSchema> {
            let docs = self.docs(db, coll);
            let taken = &docs[..docs.len().min(sample)];
            Ok(DocSchema::from_documents(taken))
        }
        async fn aggregate(
            &self,
            _db: &str,
            _coll: &str,
            _pipeline: &[DocValue],
            _batch: usize,
            _abort: &AbortSignal,
        ) -> Result<DocPage> {
            // The in-memory double doesn't execute pipelines; it exists for the
            // catalog/find/schema paths.
            Ok(DocPage {
                docs: Vec::new(),
                cursor: None,
                exhausted: true,
            })
        }
        async fn indexes(&self, _db: &str, _coll: &str) -> Result<Vec<red_core::doc::IndexInfo>> {
            Ok(Vec::new())
        }
        async fn explain(&self, q: &FindQuery) -> Result<red_core::doc::DocPlan> {
            let n = self.docs(&q.db, &q.coll).len() as u64;
            Ok(red_core::doc::DocPlan {
                stages: vec![red_core::doc::PlanStage {
                    stage: "COLLSCAN".into(),
                    depth: 0,
                    detail: None,
                }],
                index_used: None,
                docs_examined: Some(n),
                n_returned: Some(n),
                collscan: true,
            })
        }
        async fn distinct(
            &self,
            db: &str,
            coll: &str,
            field: &str,
            _filter: Option<&Filter>,
        ) -> Result<Vec<DocValue>> {
            let mut seen: Vec<DocValue> = Vec::new();
            for doc in self.docs(db, coll) {
                let value = if field == "_id" {
                    Some(&doc.id)
                } else {
                    doc.fields.iter().find(|(k, _)| k == field).map(|(_, v)| v)
                };
                if let Some(v) = value
                    && !seen.contains(v)
                {
                    seen.push(v.clone());
                }
            }
            Ok(seen)
        }
        async fn next_batch(&self, _cursor: &DocCursor, _batch: usize) -> Result<DocPage> {
            // The double never hands out a live cursor, so a `getMore` is empty.
            Ok(DocPage {
                docs: Vec::new(),
                cursor: None,
                exhausted: true,
            })
        }
        async fn close_cursor(&self, _cursor: &DocCursor) {}
        fn parse_ext_json(&self, _text: &str) -> Result<DocValue> {
            Err(red_core::RedError::Query(
                "extended-JSON parsing is not supported by the in-memory test driver".into(),
            ))
        }
    }

    fn sample() -> FakeDocDriver {
        let docs = vec![
            Document {
                id: DocValue::Int32(1),
                fields: vec![("name".into(), DocValue::Str("Ada".into()))],
            },
            Document {
                id: DocValue::Int32(2),
                fields: vec![(
                    "user".into(),
                    DocValue::Document(vec![("city".into(), DocValue::Str("London".into()))]),
                )],
            },
            Document {
                id: DocValue::Int32(3),
                fields: vec![(
                    "tags".into(),
                    DocValue::Array(vec![DocValue::Str("x".into())]),
                )],
            },
        ];
        let mut colls = BTreeMap::new();
        colls.insert("people".to_string(), docs);
        let mut data = BTreeMap::new();
        data.insert("app".to_string(), colls);
        FakeDocDriver {
            version: "7.0.0".into(),
            data,
        }
    }

    #[tokio::test]
    async fn catalog_and_count() {
        let d = sample();
        assert_eq!(d.server_version(), "7.0.0");
        assert_eq!(d.topology(), DocTopology::Standalone);
        let dbs = d.list_databases().await.unwrap();
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].name, "app");
        let colls = d.list_collections("app").await.unwrap();
        assert_eq!(colls.len(), 1);
        assert_eq!(colls[0].name, "people");
        assert_eq!(colls[0].est_count, 3);
        assert_eq!(d.count("app", "people", None).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn find_windows_and_exhausts() {
        let d = sample();
        let abort = AbortSignal::new();
        let q = FindQuery {
            db: "app".into(),
            coll: "people".into(),
            filter: None,
            projection: None,
            sort: None,
            skip: 0,
            limit: None,
            batch: 2,
        };
        let page = d.find(&q, &abort).await.unwrap();
        assert_eq!(page.docs.len(), 2);
        assert!(!page.exhausted);

        let q2 = FindQuery { skip: 2, ..q };
        let page2 = d.find(&q2, &abort).await.unwrap();
        assert_eq!(page2.docs.len(), 1);
        assert!(page2.exhausted);
    }

    #[tokio::test]
    async fn get_document_by_id() {
        let d = sample();
        let found = d
            .get_document("app", "people", &DocValue::Int32(2))
            .await
            .unwrap()
            .unwrap();
        // The nested `user.city` document round-trips through the value tree.
        assert_eq!(
            found.to_doc_value().to_extended_json(),
            r#"{"_id":2,"user":{"city":"London"}}"#
        );
        assert!(
            d.get_document("app", "people", &DocValue::Int32(99))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn schema_and_distinct() {
        let d = sample();
        let abort = AbortSignal::new();
        let schema = d.infer_schema("app", "people", 100, &abort).await.unwrap();
        assert_eq!(schema.sampled, 3);
        // `_id` is present in every sampled document.
        let id = schema.fields.iter().find(|f| f.path == "_id").unwrap();
        assert_eq!(id.present_ratio, 1.0);

        let ids = d.distinct("app", "people", "_id", None).await.unwrap();
        assert_eq!(ids.len(), 3);
    }

    // A `DocCursor` is opaque and echoed back to the driver; assert its identity
    // survives a clone the way the D1 `getMore` path will rely on.
    #[test]
    fn cursor_identity() {
        let c = DocCursor {
            id: 42,
            db: "app".into(),
            coll: "people".into(),
        };
        assert_eq!(c.clone(), c);
    }
}
