//! `DocDriver`: the third seam, for document stores (MongoDB today; see
//! `docs/plans/todo/doc-driver.md`). Neither SQL-shaped (`DatabaseDriver`) nor
//! Redis-shaped (`KvDriver`): a `server → databases → collections → documents`
//! hierarchy of nested BSON trees, queried by `find`/`aggregate` rather than SQL
//! or `GET`/`SET`. Object-safe like the other two seams, held as
//! `Arc<dyn DocDriver>`, one impl per engine.
//!
//! This is the D0 read-only subset (catalog + windowed `find` + one-document
//! fetch + count). The streaming server cursor (`next_batch`/`close_cursor`),
//! `infer_schema`/`aggregate`/`indexes`/`explain`, and every write land in the
//! later phases (D1/D2) the plan lays out, added additively to this trait.

use async_trait::async_trait;
use red_core::Result;
use red_core::doc::{
    CollectionInfo, DbInfo, DocPage, DocTopology, DocValue, Document, Filter, FindQuery,
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
