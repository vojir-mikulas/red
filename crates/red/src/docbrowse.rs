//! The MongoDB browser (`MongoView`), the document-store shell parallel to the
//! Redis `kvbrowse::RedisView`. A `database -> collection` tree on the left and a
//! main area that switches (per-collection) between the sampled-column document
//! grid with an extended-JSON filter bar, the inferred-schema panel, and the
//! indexes panel; a raw-document inspector docks right when a row is open. It
//! speaks the `Doc*` `Command`/`Event` pair (see `red-service`'s protocol) and
//! never touches the `DocDriver` directly, the same UI/driver separation the SQL
//! and Redis shells keep. See `docs/plans/todo/doc-driver.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use flint::Theme;
use flint::prelude::*;
use gpui::{
    Context, Entity, FocusHandle, SharedString, UniformListScrollHandle, WeakEntity, Window, div,
    prelude::*, px,
};
use red_core::doc::{
    CollKind, CollectionInfo, DbInfo, DocPlan, DocSchema, DocValue, DocWrite, Document, IndexInfo,
};
use red_service::{Command, Epoch, SessionId};

use crate::app::{ActiveConn, AppState, Phase};

/// Which view the main area shows for the open collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocPanel {
    /// The document grid (with the filter bar) — the default.
    Documents,
    /// The aggregation-pipeline editor and its results.
    Query,
    /// The inferred-schema table (per-field type distribution + present-ratio).
    Schema,
    /// The collection's indexes.
    Indexes,
}

impl DocPanel {
    /// The panels in toolbar order, with their segment labels.
    const ALL: [(DocPanel, &'static str); 4] = [
        (DocPanel::Documents, "Documents"),
        (DocPanel::Query, "Query"),
        (DocPanel::Schema, "Schema"),
        (DocPanel::Indexes, "Indexes"),
    ];
}

/// Documents fetched per page. Matches the service's `DOC_PAGE_ROWS` so a
/// "next page" advances `skip` by exactly one window.
const PAGE: u64 = 100;

/// Display byte cap for a grid cell's text (nested values render as capped
/// extended JSON; the inspector shows the full document). The red crate doesn't
/// depend on `red-driver`, so this is a local budget rather than the shared
/// `display_cell_cap`.
const CELL_CAP: usize = 512;

/// The most top-level fields the sampled-column grid shows; a wider document is
/// still fully visible in the inspector. Keeps the grid readable on documents
/// with dozens of fields.
const MAX_COLUMNS: usize = 12;

/// The per-connection MongoDB browse state, held as `ActiveConn.doc_view` for a
/// `DbKind::Mongo` session (mirrors `kv_view`). One browse `epoch` scopes every
/// reply so a late one for a superseded request is dropped.
pub(crate) struct MongoView {
    session: SessionId,
    /// The connection's read-only posture, captured at connect. Gates every write
    /// affordance (edit / insert / delete / drop) in the UI.
    read_only: bool,
    /// The browse epoch, minted once; every `Doc*` reply echoes it.
    epoch: Epoch,
    /// The server's databases (`listDatabases`), the tree's top level.
    databases: Vec<DbInfo>,
    /// `db -> its collections`, filled lazily when a database branch expands.
    collections: BTreeMap<String, Vec<CollectionInfo>>,
    /// Which database branches are expanded in the tree.
    expanded: BTreeSet<String>,
    /// The open collection's grid + inspector, or `None` before a selection.
    current: Option<CollView>,
    /// The last browse error (a failed list/find), shown inline in the tree.
    error: Option<String>,
    /// A destructive write awaiting confirmation (drop / delete). Rendered as a
    /// modal over the shell; approving re-sends it as a confirmed `DocApplyWrite`.
    pending_write: Option<(DocWrite, String)>,
}

/// The open collection: its current window of documents plus the sampled columns
/// and the inspector selection. Replaced wholesale when another collection is
/// selected, so a stale page can't bleed into the new one.
struct CollView {
    db: String,
    coll: String,
    /// The resident window of documents (one `PAGE`), or empty while loading.
    docs: Vec<Document>,
    /// The offset of this window into the collection.
    skip: u64,
    /// The collection's total document count, once the first page reported it.
    total: Option<u64>,
    /// Whether this is the last window (no further pages).
    exhausted: bool,
    /// Whether a `DocFetchPage` is in flight (shows a loading hint).
    loading: bool,
    /// The union of top-level field names across the window (`_id` first),
    /// capped to [`MAX_COLUMNS`]; the grid's columns.
    columns: Vec<String>,
    /// The document row open in the inspector, if any.
    inspector: Option<usize>,
    /// Whether the inspector is composing a *new* document (insert mode) rather
    /// than editing the selected row.
    inspector_insert: bool,
    /// The inspector's extended-JSON editor (edit-and-save / compose).
    inspector_editor: Entity<CodeEditor>,
    /// Which main view is shown (documents / schema / indexes).
    panel: DocPanel,
    /// The extended-JSON filter input; its text is applied on Enter or "Run".
    filter_input: Entity<TextInput>,
    /// The applied filter (re-sent when paging), or `None` for the whole collection.
    filter: Option<String>,
    /// The inferred schema, lazily fetched the first time the Schema panel opens.
    schema: Option<DocSchema>,
    /// The collection's indexes, lazily fetched the first time Indexes opens.
    indexes: Option<Vec<IndexInfo>>,
    /// The explain plan for the current filter, shown as a dismissible readout
    /// on the Documents panel; `None` when not requested / dismissed.
    explain: Option<DocPlan>,
    /// The aggregation-pipeline editor (Query panel).
    query_editor: Entity<CodeEditor>,
    /// The Query panel's last result window, its sampled columns, and whether a
    /// run is in flight.
    query_docs: Vec<Document>,
    query_columns: Vec<String>,
    query_loading: bool,
    query_scroll: UniformListScrollHandle,
    scroll: UniformListScrollHandle,
    list_focus: FocusHandle,
}

impl CollView {
    fn new(db: String, coll: String, cx: &mut Context<AppState>) -> Self {
        let filter_input = cx.new(|cx| {
            TextInput::new(cx).with_placeholder("filter, e.g. { \"status\": \"active\" }")
        });
        // Apply the filter on Enter, mirroring the SQL/Redis filter bars.
        cx.subscribe(&filter_input, |this, _input, event: &TextInputEvent, cx| {
            if !matches!(event, TextInputEvent::Submit) {
                return;
            }
            // Resolve the session out from under the `&this.phase` borrow before
            // the mutable `doc_apply_filter` call.
            let session = match &this.phase {
                Phase::Connected(active) => active.doc_view.as_ref().map(|v| v.session),
                _ => None,
            };
            if let Some(session) = session {
                this.doc_apply_filter(session, cx);
            }
        })
        .detach();
        let query_editor = cx.new(|cx| {
            CodeEditor::new(cx)
                .soft_wrap(false)
                .placeholder(
                    "Aggregation pipeline, e.g. [ { \"$group\": … } ]. \u{2318}\u{21b5} runs.",
                )
                .a11y_label("MongoDB aggregation pipeline")
        });
        cx.subscribe(
            &query_editor,
            |this, _editor, event: &CodeEditorEvent, cx| {
                if !matches!(event, CodeEditorEvent::Run) {
                    return;
                }
                let session = match &this.phase {
                    Phase::Connected(active) => active.doc_view.as_ref().map(|v| v.session),
                    _ => None,
                };
                if let Some(session) = session {
                    this.doc_run_aggregate(session, cx);
                }
            },
        )
        .detach();
        let inspector_editor = cx.new(|cx| {
            CodeEditor::new(cx)
                .soft_wrap(false)
                .a11y_label("MongoDB document editor")
        });
        // Cmd+Enter in the inspector saves (edit) or inserts (compose).
        cx.subscribe(
            &inspector_editor,
            |this, _editor, event: &CodeEditorEvent, cx| {
                if !matches!(event, CodeEditorEvent::Run) {
                    return;
                }
                let session = match &this.phase {
                    Phase::Connected(active) => active.doc_view.as_ref().map(|v| v.session),
                    _ => None,
                };
                if let Some(session) = session {
                    this.doc_save_document(session, cx);
                }
            },
        )
        .detach();
        Self {
            db,
            coll,
            docs: Vec::new(),
            skip: 0,
            total: None,
            exhausted: false,
            loading: true,
            columns: Vec::new(),
            inspector: None,
            inspector_insert: false,
            inspector_editor,
            panel: DocPanel::Documents,
            filter_input,
            filter: None,
            schema: None,
            indexes: None,
            explain: None,
            query_editor,
            query_docs: Vec::new(),
            query_columns: Vec::new(),
            query_loading: false,
            query_scroll: UniformListScrollHandle::new(),
            scroll: UniformListScrollHandle::new(),
            list_focus: cx.focus_handle(),
        }
    }
}

impl MongoView {
    /// Build the view for a freshly-connected Mongo session. The first
    /// `DocListDatabases` fires from [`AppState::doc_start_browse`] once the
    /// session is live, not here (this only needs `cx` for future focus state).
    pub(crate) fn new(session: SessionId, read_only: bool, _cx: &mut Context<AppState>) -> Self {
        Self {
            session,
            read_only,
            epoch: crate::result::next_kv_epoch(),
            databases: Vec::new(),
            collections: BTreeMap::new(),
            expanded: BTreeSet::new(),
            current: None,
            error: None,
            pending_write: None,
        }
    }
}

impl AppState {
    /// Kick off the document browser's first load (the databases list), called
    /// from `on_connected` for a Mongo session the way `kv_start_browse` is for
    /// Redis.
    pub(crate) fn doc_start_browse(&mut self, session: SessionId, _cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_ref())
        else {
            return;
        };
        let epoch = view.epoch;
        self.service
            .send_to(session, Command::DocListDatabases { epoch });
    }

    // --- event handlers (Doc* replies) -------------------------------------

    pub(crate) fn on_doc_databases(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        databases: Vec<DbInfo>,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        if view.epoch != epoch {
            return;
        }
        view.error = None;
        view.databases = databases;
        cx.notify();
    }

    pub(crate) fn on_doc_collections(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        db: String,
        collections: Vec<CollectionInfo>,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        if view.epoch != epoch {
            return;
        }
        view.collections.insert(db, collections);
        cx.notify();
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the args mirror the DocPageReady event's fields 1:1, like the on_kv_* handlers"
    )]
    pub(crate) fn on_doc_page(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        db: String,
        coll: String,
        skip: u64,
        docs: Vec<Document>,
        exhausted: bool,
        total: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        if view.epoch != epoch {
            return;
        }
        let Some(current) = view.current.as_mut() else {
            return;
        };
        // Drop a late page for a collection the user has since navigated away from.
        if current.db != db || current.coll != coll {
            return;
        }
        current.columns = sample_columns(&docs);
        current.docs = docs;
        current.skip = skip;
        current.exhausted = exhausted;
        current.loading = false;
        // A count only rides the first page; keep the prior total on later pages.
        if let Some(total) = total {
            current.total = Some(total);
        }
        // A shorter window than before can leave the inspector past the end.
        if let Some(sel) = current.inspector
            && sel >= current.docs.len()
        {
            current.inspector = None;
        }
        cx.notify();
    }

    pub(crate) fn on_doc_error(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        message: String,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        if view.epoch != epoch {
            return;
        }
        if let Some(current) = view.current.as_mut() {
            current.loading = false;
            current.query_loading = false;
        }
        view.error = Some(message);
        cx.notify();
    }

    // --- user actions ------------------------------------------------------

    /// Expand/collapse a database branch; expanding one whose collections aren't
    /// loaded yet fires the `DocListCollections` fetch.
    fn doc_toggle_db(&mut self, session: SessionId, db: String, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let epoch = view.epoch;
        let need_load = if view.expanded.remove(&db) {
            false
        } else {
            view.expanded.insert(db.clone());
            !view.collections.contains_key(&db)
        };
        if need_load {
            self.service
                .send_to(session, Command::DocListCollections { epoch, db });
        }
        cx.notify();
    }

    /// Select a collection: open a fresh grid on it and fetch the first page.
    fn doc_select_collection(
        &mut self,
        session: SessionId,
        db: String,
        coll: String,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        // Re-selecting the open collection is a no-op (don't thrash the grid).
        if view
            .current
            .as_ref()
            .is_some_and(|c| c.db == db && c.coll == coll)
        {
            return;
        }
        let epoch = view.epoch;
        view.current = Some(CollView::new(db.clone(), coll.clone(), cx));
        self.service.send_to(
            session,
            Command::DocFetchPage {
                epoch,
                db,
                coll,
                skip: 0,
                filter: None,
            },
        );
        cx.notify();
    }

    /// Page the open collection by one window. `forward` advances `skip`; the
    /// backward step floors at zero.
    fn doc_page(&mut self, session: SessionId, forward: bool, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let epoch = view.epoch;
        let Some(current) = view.current.as_mut() else {
            return;
        };
        let next_skip = if forward {
            if current.exhausted {
                return;
            }
            current.skip + PAGE
        } else {
            if current.skip == 0 {
                return;
            }
            current.skip.saturating_sub(PAGE)
        };
        current.loading = true;
        current.inspector = None;
        let (db, coll, filter) = (
            current.db.clone(),
            current.coll.clone(),
            current.filter.clone(),
        );
        self.service.send_to(
            session,
            Command::DocFetchPage {
                epoch,
                db,
                coll,
                skip: next_skip,
                filter,
            },
        );
        cx.notify();
    }

    /// Apply the filter box's current text: parse-side happens in the service, so
    /// here just re-fetch from `skip = 0` with the (trimmed) filter, or clear it
    /// when the box is empty.
    fn doc_apply_filter(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let epoch = view.epoch;
        let Some(current) = view.current.as_mut() else {
            return;
        };
        let text = current.filter_input.read(cx).content().trim().to_string();
        current.filter = (!text.is_empty()).then_some(text);
        current.skip = 0;
        current.loading = true;
        current.inspector = None;
        current.panel = DocPanel::Documents;
        let (db, coll, filter) = (
            current.db.clone(),
            current.coll.clone(),
            current.filter.clone(),
        );
        self.service.send_to(
            session,
            Command::DocFetchPage {
                epoch,
                db,
                coll,
                skip: 0,
                filter,
            },
        );
        cx.notify();
    }

    /// Switch the open collection's main panel, lazily fetching the schema or
    /// index list the first time each is shown.
    fn doc_set_panel(&mut self, session: SessionId, panel: DocPanel, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let epoch = view.epoch;
        let Some(current) = view.current.as_mut() else {
            return;
        };
        current.panel = panel;
        let (db, coll) = (current.db.clone(), current.coll.clone());
        match panel {
            DocPanel::Schema if current.schema.is_none() => {
                self.service
                    .send_to(session, Command::DocInferSchema { epoch, db, coll });
            }
            DocPanel::Indexes if current.indexes.is_none() => {
                self.service
                    .send_to(session, Command::DocListIndexes { epoch, db, coll });
            }
            _ => {}
        }
        cx.notify();
    }

    pub(crate) fn on_doc_schema(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        db: String,
        coll: String,
        schema: DocSchema,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        if view.epoch != epoch {
            return;
        }
        if let Some(current) = view.current.as_mut()
            && current.db == db
            && current.coll == coll
        {
            current.schema = Some(schema);
            cx.notify();
        }
    }

    pub(crate) fn on_doc_indexes(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        db: String,
        coll: String,
        indexes: Vec<IndexInfo>,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        if view.epoch != epoch {
            return;
        }
        if let Some(current) = view.current.as_mut()
            && current.db == db
            && current.coll == coll
        {
            current.indexes = Some(indexes);
            cx.notify();
        }
    }

    /// Run the Query panel's pipeline (the `CodeEditor` text) into the results
    /// grid. Parsing/validation happens service-side, so an empty pipeline just
    /// runs the identity aggregation.
    fn doc_run_aggregate(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let epoch = view.epoch;
        let Some(current) = view.current.as_mut() else {
            return;
        };
        let pipeline = current.query_editor.read(cx).content();
        let pipeline = if pipeline.trim().is_empty() {
            "[]".to_string()
        } else {
            pipeline
        };
        current.query_loading = true;
        let (db, coll) = (current.db.clone(), current.coll.clone());
        self.service.send_to(
            session,
            Command::DocAggregate {
                epoch,
                db,
                coll,
                pipeline,
            },
        );
        cx.notify();
    }

    /// Run `explain` on the current filter and show the plan readout.
    fn doc_run_explain(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let epoch = view.epoch;
        let Some(current) = view.current.as_ref() else {
            return;
        };
        let (db, coll, filter) = (
            current.db.clone(),
            current.coll.clone(),
            current.filter.clone(),
        );
        self.service.send_to(
            session,
            Command::DocExplain {
                epoch,
                db,
                coll,
                filter,
            },
        );
        cx.notify();
    }

    /// Dismiss the explain readout.
    fn doc_dismiss_explain(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(current) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.current.as_mut())
        {
            current.explain = None;
            cx.notify();
        }
    }

    pub(crate) fn on_doc_aggregate(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        db: String,
        coll: String,
        docs: Vec<Document>,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        if view.epoch != epoch {
            return;
        }
        if let Some(current) = view.current.as_mut()
            && current.db == db
            && current.coll == coll
        {
            current.query_columns = sample_columns(&docs);
            current.query_docs = docs;
            current.query_loading = false;
            cx.notify();
        }
    }

    pub(crate) fn on_doc_plan(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        db: String,
        coll: String,
        plan: DocPlan,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        if view.epoch != epoch {
            return;
        }
        if let Some(current) = view.current.as_mut()
            && current.db == db
            && current.coll == coll
        {
            current.explain = Some(plan);
            cx.notify();
        }
    }

    /// Open (or, on the same row, close) the inspector on a document row, loading
    /// the row's extended JSON into the editor when it opens.
    fn doc_toggle_inspector(&mut self, session: SessionId, row: usize, cx: &mut Context<Self>) {
        let editor_load = {
            let Some(current) = self
                .conn_mut(Some(session))
                .and_then(|a| a.doc_view.as_mut())
                .and_then(|v| v.current.as_mut())
            else {
                return;
            };
            if current.inspector == Some(row) && !current.inspector_insert {
                current.inspector = None;
                None
            } else {
                current.inspector = Some(row);
                current.inspector_insert = false;
                current.docs.get(row).map(|d| {
                    (
                        current.inspector_editor.clone(),
                        pretty_extjson(&d.to_doc_value()),
                    )
                })
            }
        };
        if let Some((editor, json)) = editor_load {
            editor.update(cx, |ed, cx| ed.set_content(json, cx));
        }
        cx.notify();
    }

    /// Open the inspector in compose mode with a blank document template.
    fn doc_new_document(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let editor = {
            let Some(current) = self
                .conn_mut(Some(session))
                .and_then(|a| a.doc_view.as_mut())
                .and_then(|v| v.current.as_mut())
            else {
                return;
            };
            current.inspector = None;
            current.inspector_insert = true;
            current.inspector_editor.clone()
        };
        editor.update(cx, |ed, cx| ed.set_content("{\n  \n}", cx));
        cx.notify();
    }

    /// Save the inspector's editor: insert a new document (compose mode) or
    /// replace the selected one (edit mode). Parsing happens service-side.
    fn doc_save_document(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let cmd = {
            let Some(view) = self
                .conn_mut(Some(session))
                .and_then(|a| a.doc_view.as_ref())
            else {
                return;
            };
            if view.read_only {
                return;
            }
            let Some(current) = view.current.as_ref() else {
                return;
            };
            let doc_json = current.inspector_editor.read(cx).content();
            let (epoch, db, coll) = (view.epoch, current.db.clone(), current.coll.clone());
            if current.inspector_insert {
                Some(Command::DocInsert {
                    epoch,
                    db,
                    coll,
                    doc_json,
                })
            } else {
                current
                    .inspector
                    .and_then(|row| current.docs.get(row))
                    .map(|d| Command::DocReplace {
                        epoch,
                        db,
                        coll,
                        id: d.id.clone(),
                        doc_json,
                    })
            }
        };
        if let Some(cmd) = cmd {
            self.service.send_to(session, cmd);
        }
    }

    /// Queue a delete of the inspected document behind the confirm modal.
    fn doc_delete_current(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        if view.read_only {
            return;
        }
        let Some(current) = view.current.as_ref() else {
            return;
        };
        let Some(doc) = current.inspector.and_then(|row| current.docs.get(row)) else {
            return;
        };
        let write = DocWrite::Delete {
            db: current.db.clone(),
            coll: current.coll.clone(),
            filter: DocValue::Document(vec![("_id".into(), doc.id.clone())]),
            many: false,
        };
        let prompt = format!(
            "Delete this document from {}.{}? This cannot be undone.",
            current.db, current.coll
        );
        view.pending_write = Some((write, prompt));
        cx.notify();
    }

    /// Propose dropping the open collection; the service's destructive gate
    /// replies with a confirm the modal then shows.
    fn doc_drop_current(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let cmd = {
            let Some(view) = self
                .conn_mut(Some(session))
                .and_then(|a| a.doc_view.as_ref())
            else {
                return;
            };
            if view.read_only {
                return;
            }
            view.current.as_ref().map(|current| Command::DocApplyWrite {
                epoch: view.epoch,
                write: DocWrite::DropCollection {
                    db: current.db.clone(),
                    coll: current.coll.clone(),
                },
                confirmed: false,
            })
        };
        if let Some(cmd) = cmd {
            self.service.send_to(session, cmd);
            cx.notify();
        }
    }

    /// Approve the pending destructive write: re-send it confirmed.
    fn doc_confirm_write(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let sent = {
            let Some(view) = self
                .conn_mut(Some(session))
                .and_then(|a| a.doc_view.as_mut())
            else {
                return;
            };
            view.pending_write
                .take()
                .map(|(write, _)| (view.epoch, write))
        };
        if let Some((epoch, write)) = sent {
            self.service.send_to(
                session,
                Command::DocApplyWrite {
                    epoch,
                    write,
                    confirmed: true,
                },
            );
            cx.notify();
        }
    }

    /// Dismiss the confirm modal without writing.
    fn doc_cancel_write(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.pending_write = None;
            cx.notify();
        }
    }

    /// Close the inspector (edit or compose).
    fn doc_close_inspector(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(current) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.current.as_mut())
        {
            current.inspector = None;
            current.inspector_insert = false;
            cx.notify();
        }
    }

    pub(crate) fn on_doc_write_confirm(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        write: DocWrite,
        prompt: String,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        if view.epoch != epoch {
            return;
        }
        view.pending_write = Some((write, prompt));
        cx.notify();
    }

    pub(crate) fn on_doc_write_done(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        summary: String,
        cx: &mut Context<Self>,
    ) {
        let Some(sid) = session else { return };
        // Close the inspector, clear any pending confirm, and gather what the
        // browse needs to refresh, all before the toast + re-fetch.
        let refresh = {
            let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
                return;
            };
            if view.epoch != epoch {
                return;
            }
            view.pending_write = None;
            let epoch = view.epoch;
            view.current.as_mut().map(|c| {
                c.inspector = None;
                c.inspector_insert = false;
                c.loading = true;
                (
                    epoch,
                    c.db.clone(),
                    c.coll.clone(),
                    c.skip,
                    c.filter.clone(),
                )
            })
        };
        self.notify(ToastVariant::Success, summary, cx);
        if let Some((epoch, db, coll, skip, filter)) = refresh {
            self.service.send_to(
                sid,
                Command::DocFetchPage {
                    epoch,
                    db,
                    coll,
                    skip,
                    filter,
                },
            );
        }
    }
}

// --- rendering ---------------------------------------------------------------

impl AppState {
    pub(crate) fn render_mongo_shell(
        &self,
        active: &ActiveConn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let topbar = self.render_topbar(&theme, &view, window, cx);

        let workspace = active
            .doc_view
            .as_ref()
            .map(|v| self.render_doc_body(v, &theme, &view))
            .unwrap_or_else(|| div().into_any_element());

        // Dock the assistant to the right of the workspace when it's open, the
        // same resizable split the SQL/Redis shells use. `render_assistant` is
        // engine-agnostic (a chat over `AiTurn` events); the doc backend grounds
        // the turn (see the doc AI tools).
        let body = if self.assistant.is_some() {
            let start = view.clone();
            let resize = view.clone();
            let end = view.clone();
            let panel = self.render_assistant(cx);
            div().flex_1().min_h(px(0.)).child(
                SplitPane::new("doc-split-assistant", gpui::Axis::Horizontal)
                    .sized(SplitSide::Trailing)
                    .size(self.assistant_w)
                    .gutter(px(1.))
                    .drag(self.assistant_drag)
                    .min_first(px(320.))
                    .max_first(px(760.))
                    .on_drag_start(move |anchor, _, cx| {
                        start
                            .update(cx, |this, cx| {
                                this.assistant_drag = Some(anchor);
                                cx.notify();
                            })
                            .ok();
                    })
                    .on_resize(move |size, _, cx| {
                        resize
                            .update(cx, |this, cx| {
                                this.assistant_w = size;
                                cx.notify();
                            })
                            .ok();
                    })
                    .on_drag_end(move |_, cx| {
                        end.update(cx, |this, cx| {
                            this.assistant_drag = None;
                            cx.notify();
                        })
                        .ok();
                    })
                    .first(workspace)
                    .second(panel),
            )
        } else {
            div().flex_1().min_h(px(0.)).flex().child(workspace)
        }
        .into_any_element();

        // A destructive write awaiting confirmation overlays everything.
        let confirm = active
            .doc_view
            .as_ref()
            .and_then(|v| {
                v.pending_write
                    .as_ref()
                    .map(|(_, prompt)| (v.session, prompt.clone()))
            })
            .map(|(session, prompt)| self.render_doc_confirm(session, prompt, &theme, &view));

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_app)
            .text_color(theme.text)
            .child(topbar)
            .child(div().flex().flex_1().min_h(px(0.)).child(body))
            .children(confirm)
    }

    /// The destructive-write confirm modal: a scrim over a centered card with the
    /// prompt and Cancel / Confirm.
    fn render_doc_confirm(
        &self,
        session: SessionId,
        prompt: String,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let cancel_view = view.clone();
        let confirm_view = view.clone();
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0., 0., 0., 0.5))
            .child(
                div()
                    .w(px(420.))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .bg(theme.bg_panel)
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .child(div().font_weight(gpui::FontWeight::MEDIUM).child("Confirm"))
                    .child(div().text_color(theme.text_muted).child(prompt))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("doc-confirm-cancel", "Cancel")
                                    .size(ButtonSize::Sm)
                                    .variant(ButtonVariant::Secondary)
                                    .on_click(move |_, _, cx| {
                                        cancel_view
                                            .update(cx, |this, cx| {
                                                this.doc_cancel_write(session, cx)
                                            })
                                            .ok();
                                    }),
                            )
                            .child(
                                Button::new("doc-confirm-ok", "Confirm")
                                    .size(ButtonSize::Sm)
                                    .variant(ButtonVariant::Danger)
                                    .on_click(move |_, _, cx| {
                                        confirm_view
                                            .update(cx, |this, cx| {
                                                this.doc_confirm_write(session, cx)
                                            })
                                            .ok();
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_doc_body(
        &self,
        v: &MongoView,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let tree = self.render_doc_tree(v, theme, view);
        let main = self.render_doc_main(v, theme, view);
        div()
            .flex()
            .size_full()
            .child(
                div()
                    .w(px(260.))
                    .h_full()
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(theme.border)
                    .child(tree),
            )
            .child(div().flex_1().min_w(px(0.)).h_full().child(main))
            .into_any_element()
    }

    /// The `database -> collection` tree (left dock). Databases are click-to-
    /// expand; a collection row selects it into the grid.
    fn render_doc_tree(
        &self,
        v: &MongoView,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let session = v.session;
        let selected = v.current.as_ref().map(|c| (c.db.clone(), c.coll.clone()));
        let icon_size = theme.scale(12.);

        let mut rows = div().flex().flex_col().py_1();
        for db in &v.databases {
            let expanded = v.expanded.contains(&db.name);
            let chevron = if expanded { "chevron-down" } else { "chevron" };
            let db_name = db.name.clone();
            let toggle_view = view.clone();
            rows = rows.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.bg_hover))
                    .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                        let db = db_name.clone();
                        toggle_view
                            .update(cx, |this, cx| this.doc_toggle_db(session, db, cx))
                            .ok();
                    })
                    .child(crate::icons::icon(chevron, icon_size, theme.text_muted))
                    .child(crate::icons::icon("database", icon_size, theme.text_muted))
                    .child(div().min_w_0().truncate().child(db.name.clone())),
            );
            if expanded {
                match v.collections.get(&db.name) {
                    Some(colls) if !colls.is_empty() => {
                        for coll in colls {
                            rows = rows.child(self.render_doc_coll_row(
                                session,
                                &db.name,
                                coll,
                                selected.as_ref(),
                                theme,
                                icon_size,
                                view,
                            ));
                        }
                    }
                    Some(_) => {
                        rows = rows.child(
                            div()
                                .pl(px(40.))
                                .py_1()
                                .text_color(theme.text_faint)
                                .child("(no collections)"),
                        );
                    }
                    None => {
                        rows = rows.child(
                            div()
                                .pl(px(40.))
                                .py_1()
                                .text_color(theme.text_faint)
                                .child("Loading..."),
                        );
                    }
                }
            }
        }

        let error = v
            .error
            .as_ref()
            .map(|e| div().px_2().py_1().text_color(theme.red).child(e.clone()));

        div()
            .id("doc-db-tree")
            .size_full()
            .overflow_y_scroll()
            .text_size(theme.scale(13.))
            .children(error)
            .child(rows)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_doc_coll_row(
        &self,
        session: SessionId,
        db: &str,
        coll: &CollectionInfo,
        selected: Option<&(String, String)>,
        theme: &Theme,
        icon_size: gpui::Pixels,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let is_sel = selected.is_some_and(|(sd, sc)| sd == db && sc == &coll.name);
        let (db_owned, coll_owned) = (db.to_string(), coll.name.clone());
        let select_view = view.clone();
        let badge = coll_kind_badge(coll.kind);
        div()
            .flex()
            .items_center()
            .gap_1()
            .pl(px(28.))
            .pr_2()
            .py_1()
            .cursor_pointer()
            .when(is_sel, |d| d.bg(theme.bg_selected))
            .when(!is_sel, |d| d.hover(|s| s.bg(theme.bg_hover)))
            .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                let (db, coll) = (db_owned.clone(), coll_owned.clone());
                select_view
                    .update(cx, |this, cx| {
                        this.doc_select_collection(session, db, coll, cx)
                    })
                    .ok();
            })
            .child(crate::icons::icon("table", icon_size, theme.text_muted))
            .child(div().min_w_0().flex_1().truncate().child(coll.name.clone()))
            .children(badge.map(|label| {
                div()
                    .text_color(theme.text_faint)
                    .text_size(theme.scale(10.))
                    .child(label)
            }))
            .child(
                div()
                    .text_color(theme.text_faint)
                    .child(fmt_count(coll.est_count)),
            )
            .into_any_element()
    }

    /// The main area: a header (collection name + Documents/Schema/Indexes
    /// picker, plus the filter bar and pager on the Documents panel) over the
    /// panel body.
    fn render_doc_main(
        &self,
        v: &MongoView,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let Some(current) = v.current.as_ref() else {
            return div()
                .flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_color(theme.text_faint)
                .child("Select a collection to browse its documents.")
                .into_any_element();
        };

        let header = self.render_doc_header(v.session, current, v.read_only, theme, view);
        let body = match current.panel {
            DocPanel::Documents => {
                self.render_doc_documents(v.session, current, v.read_only, theme, view)
            }
            DocPanel::Query => self.render_doc_query(v.session, current, theme, view),
            DocPanel::Schema => render_doc_schema_panel(current, theme),
            DocPanel::Indexes => render_doc_indexes_panel(current, theme),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_doc_header(
        &self,
        session: SessionId,
        current: &CollView,
        read_only: bool,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let picker_view = view.clone();
        let selected_ix = DocPanel::ALL
            .iter()
            .position(|(p, _)| *p == current.panel)
            .unwrap_or(0);
        let picker = DocPanel::ALL
            .iter()
            .fold(Segmented::new("doc-panel"), |seg, (_, label)| {
                seg.segment(*label)
            })
            .selected(selected_ix)
            .on_select(move |ix, _, cx| {
                let panel = DocPanel::ALL
                    .get(ix)
                    .map(|(p, _)| *p)
                    .unwrap_or(DocPanel::Documents);
                picker_view
                    .update(cx, |this, cx| this.doc_set_panel(session, panel, cx))
                    .ok();
            });

        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .flex_shrink_0()
                    .child(format!("{}.{}", current.db, current.coll)),
            )
            .child(picker);

        // The filter bar + pager belong to the Documents panel only.
        if current.panel == DocPanel::Documents {
            let start = current.skip + 1;
            let end = current.skip + current.docs.len() as u64;
            let range = if current.docs.is_empty() {
                "0".to_string()
            } else {
                format!("{start}\u{2013}{end}")
            };
            let total = current
                .total
                .map(|t| format!(" of {t}"))
                .unwrap_or_default();
            let status = if current.loading {
                "Loading...".to_string()
            } else {
                format!("{range}{total}")
            };

            let run_view = view.clone();
            let explain_view = view.clone();
            let prev_view = view.clone();
            let next_view = view.clone();
            row = row
                .child(
                    div()
                        .flex_1()
                        .min_w(px(120.))
                        .child(current.filter_input.clone()),
                )
                .child(
                    Button::new("doc-run-filter", "Run")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Secondary)
                        .on_click(move |_, _, cx| {
                            run_view
                                .update(cx, |this, cx| this.doc_apply_filter(session, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new("doc-explain", "Explain")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Ghost)
                        .on_click(move |_, _, cx| {
                            explain_view
                                .update(cx, |this, cx| this.doc_run_explain(session, cx))
                                .ok();
                        }),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.text_muted)
                        .child(status),
                )
                .child(
                    Button::new("doc-prev", "Prev")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Secondary)
                        .disabled(current.skip == 0 || current.loading)
                        .on_click(move |_, _, cx| {
                            prev_view
                                .update(cx, |this, cx| this.doc_page(session, false, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new("doc-next", "Next")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Secondary)
                        .disabled(current.exhausted || current.loading)
                        .on_click(move |_, _, cx| {
                            next_view
                                .update(cx, |this, cx| this.doc_page(session, true, cx))
                                .ok();
                        }),
                );
            // Write affordances — hidden on a read-only connection.
            if !read_only {
                let new_view = view.clone();
                let drop_view = view.clone();
                row = row
                    .child(
                        Button::new("doc-new", "+ New")
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Secondary)
                            .on_click(move |_, _, cx| {
                                new_view
                                    .update(cx, |this, cx| this.doc_new_document(session, cx))
                                    .ok();
                            }),
                    )
                    .child(
                        Button::new("doc-drop", "Drop")
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Danger)
                            .on_click(move |_, _, cx| {
                                drop_view
                                    .update(cx, |this, cx| this.doc_drop_current(session, cx))
                                    .ok();
                            }),
                    );
            }
        } else {
            row = row.child(div().flex_1());
        }
        row.into_any_element()
    }

    /// The Documents panel: the explain readout (when requested) over the
    /// sampled-column grid, with the inspector docked right when a row is open or
    /// a new document is being composed.
    fn render_doc_documents(
        &self,
        session: SessionId,
        current: &CollView,
        read_only: bool,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let grid = self.render_doc_grid(session, current, theme, view);
        let grid_area = if current.inspector.is_some() || current.inspector_insert {
            div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .child(div().flex_1().min_w(px(0.)).child(grid))
                .child(
                    div()
                        .w(px(420.))
                        .h_full()
                        .flex_shrink_0()
                        .border_l_1()
                        .border_color(theme.border)
                        .child(self.render_doc_inspector(session, current, read_only, theme, view)),
                )
                .into_any_element()
        } else {
            div().flex_1().min_h(px(0.)).child(grid).into_any_element()
        };

        let explain = current
            .explain
            .as_ref()
            .map(|plan| render_explain_box(session, plan, theme, view));

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .children(explain)
            .child(grid_area)
            .into_any_element()
    }

    /// The Query panel: the aggregation-pipeline editor over its results grid.
    fn render_doc_query(
        &self,
        session: SessionId,
        current: &CollView,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let run_view = view.clone();
        let toolbar = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .child(
                div()
                    .flex_1()
                    .text_color(theme.text_muted)
                    .child("Aggregation pipeline (extended JSON array of stages)"),
            )
            .child(
                Button::new("doc-run-agg", "Run")
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Primary)
                    .disabled(current.query_loading)
                    .on_click(move |_, _, cx| {
                        run_view
                            .update(cx, |this, cx| this.doc_run_aggregate(session, cx))
                            .ok();
                    }),
            );

        let editor = div()
            .h(px(160.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .child(current.query_editor.clone());

        let results = if current.query_docs.is_empty() {
            doc_centered_hint(
                if current.query_loading {
                    "Running..."
                } else {
                    "Run a pipeline to see results."
                },
                theme,
            )
        } else {
            render_docs_table(
                "doc-query-grid",
                &current.query_docs,
                &current.query_columns,
                &current.query_scroll,
                theme,
            )
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(toolbar)
            .child(editor)
            .child(results)
            .into_any_element()
    }

    fn render_doc_grid(
        &self,
        session: SessionId,
        current: &CollView,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        if current.docs.is_empty() && !current.loading {
            return div()
                .flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_color(theme.text_faint)
                .child("No documents.")
                .into_any_element();
        }

        let columns: Vec<Column> = current
            .columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                if i == 0 {
                    Column::new(name.clone()).width(px(220.))
                } else {
                    Column::new(name.clone()).flex()
                }
            })
            .collect();

        let docs = Rc::new(current.docs.clone());
        let cols = Rc::new(current.columns.clone());
        let render_docs = docs.clone();
        let render_cols = cols.clone();
        let text = theme.text;
        let faint = theme.text_faint;
        let select_view = view.clone();
        let selected = current.inspector;

        Table::<()>::new("doc-grid", columns)
            .row_count(current.docs.len())
            .grid_lines(true)
            .text_size(theme.scale(12.))
            .track_scroll(&current.scroll)
            .focus_handle(current.list_focus.clone())
            .selected(selected)
            .on_select(move |ix, _click, _window, cx| {
                select_view
                    .update(cx, |this, cx| this.doc_toggle_inspector(session, ix, cx))
                    .ok();
            })
            .render_row(move |ix, _window, _cx| {
                let Some(doc) = render_docs.get(ix) else {
                    return Vec::new();
                };
                render_cols
                    .iter()
                    .map(|col| match cell_string(doc, col) {
                        Some(text_val) => div()
                            .min_w_0()
                            .truncate()
                            .text_color(text)
                            .child(text_val)
                            .into_any_element(),
                        // A field absent from this document (schemaless): a faint dash.
                        None => div().text_color(faint).child("\u{2014}").into_any_element(),
                    })
                    .collect()
            })
            .into_any_element()
    }

    /// The raw-document inspector. On a writable connection it's an editable
    /// extended-JSON editor with Save / Delete (⌘↵ saves); on a read-only one it
    /// falls back to the pretty-printed read-only view.
    fn render_doc_inspector(
        &self,
        session: SessionId,
        current: &CollView,
        read_only: bool,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let insert = current.inspector_insert;
        let title = if insert { "New document" } else { "Document" };

        let close_view = view.clone();
        let close = Button::new("doc-inspector-close", "Close")
            .size(ButtonSize::Sm)
            .variant(ButtonVariant::Ghost)
            .on_click(move |_, _, cx| {
                close_view
                    .update(cx, |this, cx| this.doc_close_inspector(session, cx))
                    .ok();
            });

        let mut header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(title),
            );

        if read_only {
            return div()
                .flex()
                .flex_col()
                .size_full()
                .child(header.child(close))
                .child(self.render_doc_readonly_body(current, theme))
                .into_any_element();
        }

        let save_view = view.clone();
        header = header.child(
            Button::new("doc-save", if insert { "Insert" } else { "Save" })
                .size(ButtonSize::Sm)
                .variant(ButtonVariant::Primary)
                .on_click(move |_, _, cx| {
                    save_view
                        .update(cx, |this, cx| this.doc_save_document(session, cx))
                        .ok();
                }),
        );
        if !insert {
            let delete_view = view.clone();
            header = header.child(
                Button::new("doc-delete", "Delete")
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Danger)
                    .on_click(move |_, _, cx| {
                        delete_view
                            .update(cx, |this, cx| this.doc_delete_current(session, cx))
                            .ok();
                    }),
            );
        }
        header = header.child(close);

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .child(current.inspector_editor.clone()),
            )
            .into_any_element()
    }

    /// The read-only pretty-printed document view, shown on read-only
    /// connections in place of the editor.
    fn render_doc_readonly_body(&self, current: &CollView, theme: &Theme) -> gpui::AnyElement {
        // Bounded line-by-line render so the pretty JSON's newlines lay out.
        const MAX_LINES: usize = 5_000;
        let json = current
            .inspector
            .and_then(|row| current.docs.get(row))
            .map(|d| pretty_extjson(&d.to_doc_value()))
            .unwrap_or_default();
        let lines: Vec<SharedString> = json
            .lines()
            .take(MAX_LINES)
            .map(|l| SharedString::from(l.to_string()))
            .collect();
        div()
            .id("doc-inspector-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_scroll()
            .p_3()
            .flex()
            .flex_col()
            .font_family(theme.mono_family.clone())
            .text_size(theme.scale(12.))
            .text_color(theme.text)
            .children(
                lines
                    .into_iter()
                    .map(|line| div().flex_shrink_0().child(line)),
            )
            .into_any_element()
    }
}

// --- panel bodies (free, no `&self` needed) ----------------------------------

/// A centered muted hint filling the panel body (loading / empty states).
fn doc_centered_hint(text: &str, theme: &Theme) -> gpui::AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h(px(0.))
        .items_center()
        .justify_center()
        .text_color(theme.text_faint)
        .child(text.to_string())
        .into_any_element()
}

/// One three-column row (Field | middle | trailing), shared by the schema and
/// index panels. `header` styles it as the muted, bordered column header.
fn doc_row3(
    lead: impl Into<SharedString>,
    middle: impl Into<SharedString>,
    trail: impl Into<SharedString>,
    theme: &Theme,
    header: bool,
) -> gpui::AnyElement {
    let color = if header { theme.text_muted } else { theme.text };
    div()
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py(px(5.))
        .when(header, |d| d.border_b_1().border_color(theme.border))
        .child(
            div()
                .w(px(240.))
                .flex_shrink_0()
                .truncate()
                .text_color(color)
                .child(lead.into()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_color(color)
                .child(middle.into()),
        )
        .child(
            div()
                .w(px(90.))
                .flex_shrink_0()
                .text_color(theme.text_muted)
                .child(trail.into()),
        )
        .into_any_element()
}

/// The Schema panel: one row per inferred field path with its type distribution
/// (`string 82% . int 18%`) and present-ratio, or a hint while the sample loads.
fn render_doc_schema_panel(current: &CollView, theme: &Theme) -> gpui::AnyElement {
    let Some(schema) = current.schema.as_ref() else {
        return doc_centered_hint("Sampling schema...", theme);
    };
    if schema.fields.is_empty() {
        return doc_centered_hint("No fields sampled.", theme);
    }
    let rows = schema.fields.iter().map(|f| {
        let total: u64 = f.types.iter().map(|(_, c)| c).sum();
        let types = f
            .types
            .iter()
            .map(|(t, c)| {
                let pct = if total > 0 {
                    (*c as f64 * 100.0 / total as f64).round() as u64
                } else {
                    0
                };
                format!("{} {pct}%", t.label())
            })
            .collect::<Vec<_>>()
            .join("  \u{b7}  ");
        let present = format!("{:.0}%", f.present_ratio * 100.0);
        doc_row3(f.path.clone(), types, present, theme, false)
    });
    div()
        .id("doc-schema")
        .size_full()
        .overflow_y_scroll()
        .text_size(theme.scale(12.))
        .child(doc_row3("Field", "Types", "Present", theme, true))
        .children(rows)
        .child(
            div()
                .px_3()
                .py_2()
                .text_color(theme.text_faint)
                .child(format!("sampled {} documents", schema.sampled)),
        )
        .into_any_element()
}

/// The Indexes panel: one row per index with its keys and properties, or a hint
/// while the list loads.
fn render_doc_indexes_panel(current: &CollView, theme: &Theme) -> gpui::AnyElement {
    let Some(indexes) = current.indexes.as_ref() else {
        return doc_centered_hint("Loading indexes...", theme);
    };
    if indexes.is_empty() {
        return doc_centered_hint("No indexes.", theme);
    }
    let rows = indexes.iter().map(|idx| {
        let keys = idx
            .keys
            .iter()
            .map(|(field, order)| format!("{field}: {order}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut props = Vec::new();
        if idx.unique {
            props.push("unique".to_string());
        }
        if idx.sparse {
            props.push("sparse".to_string());
        }
        if idx.partial {
            props.push("partial".to_string());
        }
        if let Some(ttl) = idx.ttl {
            props.push(format!("ttl {ttl}s"));
        }
        doc_row3(idx.name.clone(), keys, props.join(", "), theme, false)
    });
    div()
        .id("doc-indexes")
        .size_full()
        .overflow_y_scroll()
        .text_size(theme.scale(12.))
        .child(doc_row3("Index", "Keys", "Properties", theme, true))
        .children(rows)
        .into_any_element()
}

/// A read-only sampled-column table over a document window, used by the Query
/// results panel. (The browse grid is `render_doc_grid`, which additionally
/// drives the inspector selection.)
fn render_docs_table(
    id: &'static str,
    docs: &[Document],
    columns: &[String],
    scroll: &UniformListScrollHandle,
    theme: &Theme,
) -> gpui::AnyElement {
    let cols: Vec<Column> = columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            if i == 0 {
                Column::new(name.clone()).width(px(220.))
            } else {
                Column::new(name.clone()).flex()
            }
        })
        .collect();
    let render_docs = Rc::new(docs.to_vec());
    let render_cols = Rc::new(columns.to_vec());
    let text = theme.text;
    let faint = theme.text_faint;
    Table::<()>::new(id, cols)
        .row_count(docs.len())
        .grid_lines(true)
        .text_size(theme.scale(12.))
        .track_scroll(scroll)
        .render_row(move |ix, _, _| {
            let Some(doc) = render_docs.get(ix) else {
                return Vec::new();
            };
            render_cols
                .iter()
                .map(|col| match cell_string(doc, col) {
                    Some(t) => div()
                        .min_w_0()
                        .truncate()
                        .text_color(text)
                        .child(t)
                        .into_any_element(),
                    None => div().text_color(faint).child("\u{2014}").into_any_element(),
                })
                .collect()
        })
        .into_any_element()
}

/// The explain readout strip: a headline that flags a `COLLSCAN` (red) or names
/// the index used (green), the examined/returned counts, the winning-plan stage
/// chain, and a Close button.
fn render_explain_box(
    session: SessionId,
    plan: &DocPlan,
    theme: &Theme,
    view: &WeakEntity<AppState>,
) -> gpui::AnyElement {
    let (headline, color) = if plan.collscan {
        ("COLLSCAN - no index used".to_string(), theme.red)
    } else if let Some(ix) = &plan.index_used {
        (format!("uses index {ix}"), theme.green)
    } else {
        ("indexed plan".to_string(), theme.text)
    };
    let stats = match (plan.docs_examined, plan.n_returned) {
        (Some(e), Some(r)) => format!("examined {e}, returned {r}"),
        (Some(e), None) => format!("examined {e}"),
        _ => String::new(),
    };
    let stage_line = plan
        .stages
        .iter()
        .map(|s| match &s.detail {
            Some(detail) => format!("{}({detail})", s.stage),
            None => s.stage.clone(),
        })
        .collect::<Vec<_>>()
        .join("  \u{203a}  ");

    let close_view = view.clone();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2()
        .flex_shrink_0()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(color)
                        .child(headline),
                )
                .child(div().flex_1())
                .child(div().text_color(theme.text_muted).child(stats))
                .child(
                    Button::new("doc-explain-close", "Close")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Ghost)
                        .on_click(move |_, _, cx| {
                            close_view
                                .update(cx, |this, cx| this.doc_dismiss_explain(session, cx))
                                .ok();
                        }),
                ),
        )
        .child(
            div()
                .text_color(theme.text_muted)
                .text_size(theme.scale(11.))
                .child(stage_line),
        )
        .into_any_element()
}

// --- free helpers ------------------------------------------------------------

/// The union of top-level field names across a window, `_id` first, capped to
/// [`MAX_COLUMNS`]. Stable order: `_id`, then first-seen order across the docs,
/// so the grid columns don't reshuffle between rows.
fn sample_columns(docs: &[Document]) -> Vec<String> {
    let mut cols = vec!["_id".to_string()];
    for doc in docs {
        for (name, _) in &doc.fields {
            if cols.len() >= MAX_COLUMNS {
                return cols;
            }
            if !cols.iter().any(|c| c == name) {
                cols.push(name.clone());
            }
        }
    }
    cols
}

/// The display string for one grid cell: the document's value for `col`, or
/// `None` when the field is absent (a schemaless gap). Nested values render as
/// capped extended JSON; scalars map directly through [`DocValue::to_cell`].
fn cell_string(doc: &Document, col: &str) -> Option<String> {
    let value = if col == "_id" {
        Some(&doc.id)
    } else {
        doc.fields.iter().find(|(k, _)| k == col).map(|(_, v)| v)
    };
    value.map(|v| v.to_cell(CELL_CAP).to_string())
}

/// A short badge label for a non-plain collection kind (a view or time-series),
/// or `None` for an ordinary collection.
fn coll_kind_badge(kind: CollKind) -> Option<&'static str> {
    match kind {
        CollKind::Collection => None,
        CollKind::View => Some("view"),
        CollKind::Timeseries => Some("ts"),
    }
}

/// Compact document count for the tree (`1.2k`, `3.4M`), like the Redis size
/// badges: an exact small count, an abbreviated large one.
fn fmt_count(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.)
    }
}

/// Pretty-print a document as indented extended JSON for the inspector. Nested
/// documents/arrays lay out multi-line; scalars reuse [`DocValue::to_extended_json`]
/// so the JSON-lossy BSON types keep their `$`-tagged spelling.
fn pretty_extjson(value: &DocValue) -> String {
    let mut out = String::new();
    write_pretty(value, &mut out, 0);
    out
}

fn write_pretty(value: &DocValue, out: &mut String, depth: usize) {
    match value {
        DocValue::Document(fields) if !fields.is_empty() => {
            out.push_str("{\n");
            for (i, (key, val)) in fields.iter().enumerate() {
                indent(out, depth + 1);
                out.push('"');
                out.push_str(key);
                out.push_str("\": ");
                write_pretty(val, out, depth + 1);
                if i + 1 < fields.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            indent(out, depth);
            out.push('}');
        }
        DocValue::Array(items) if !items.is_empty() => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                indent(out, depth + 1);
                write_pretty(item, out, depth + 1);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            indent(out, depth);
            out.push(']');
        }
        // Empty containers and every scalar render inline via the compact form.
        other => out.push_str(&other.to_extended_json()),
    }
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}
