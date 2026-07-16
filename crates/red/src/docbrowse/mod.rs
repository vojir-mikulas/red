//! The MongoDB browser (`MongoView`), the document-store shell parallel to the
//! Redis `kvbrowse::RedisView`. A `database -> collection` tree on the left and a
//! tabbed work area on the right: each open collection is its own tab (with a
//! per-collection filter bar, document grid, schema/indexes panels, aggregation
//! editor, and inspector), and tabs live in an optional side-by-side split — the
//! same `TabWorkspace` plumbing the SQL and Redis shells share. It speaks the
//! `Doc*` `Command`/`Event` pair (see `red-service`'s protocol) and never touches
//! the `DocDriver` directly, the same UI/driver separation the other shells keep.
//! See `docs/plans/mongo-workspace.md`.

mod form;
mod render;
mod tabs;

pub(crate) use form::{DocForm, InspectorMode};

use std::collections::{BTreeMap, BTreeSet};

use flint::prelude::*;
use gpui::{
    Context, Entity, FocusHandle, Focusable, ListAlignment, ListState, ScrollHandle,
    UniformListScrollHandle, Window, prelude::*,
};
use red_core::doc::{
    CollectionInfo, DbInfo, DocPlan, DocSchema, DocValue, DocWrite, Document, IndexInfo,
};
use red_service::{Command, Epoch, SessionId};

use crate::app::{AppState, Pane, Phase, SplitHalf, SplitState, TabWorkspace, WorkspaceTab};

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

/// How the Documents panel renders each document (Compass-style modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocViewMode {
    /// A sampled-column table — the compact, spreadsheet-like default.
    Table,
    /// One expandable card per document: a per-field tree with nested
    /// Object/Array drill-down.
    List,
    /// One pretty extended-JSON block per document.
    Json,
}

impl DocViewMode {
    const ALL: [(DocViewMode, &'static str); 3] = [
        (DocViewMode::Table, "Table"),
        (DocViewMode::List, "List"),
        (DocViewMode::Json, "JSON"),
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
/// still fully visible in the List/JSON modes and the inspector. Keeps the grid
/// readable on documents with dozens of fields.
const MAX_COLUMNS: usize = 12;

/// The per-kind state a Mongo tab holds. An `Empty` tab shows the "pick a
/// collection" hint; a `Collection` tab holds a whole [`CollView`]. Boxed like
/// the Redis `Browse` variant because `CollView` dwarfs the empty case.
enum MongoTabState {
    /// A blank tab awaiting a collection choice from the sidebar tree.
    Empty,
    Collection(Box<CollView>),
}

/// One tab in the Mongo shell: a title, a stable id, and its per-kind state.
pub(crate) struct MongoTab {
    /// Stable identity, never reused, assigned from [`MongoView::tab_seq`].
    id: u64,
    title: String,
    state: MongoTabState,
    /// Which split half this tab belongs to; always `Primary` when unsplit.
    pane: SplitHalf,
    /// Pinned tabs sort ahead of the rest in their half's strip.
    pinned: bool,
}

/// The per-connection MongoDB browse state, held as `ActiveConn.doc_view` for a
/// `DbKind::Mongo` session (mirrors `kv_view`). The catalog `epoch` scopes the
/// databases/collections replies; each open collection tab carries its own
/// `epoch` so a page/schema/write reply routes to the tab that asked.
pub(crate) struct MongoView {
    session: SessionId,
    /// The connection's read-only posture, captured at connect. Gates every write
    /// affordance (edit / insert / delete / drop) in the UI.
    read_only: bool,
    /// The catalog epoch, minted once; the `DocListDatabases`/`DocListCollections`
    /// replies echo it.
    epoch: Epoch,
    /// The server's databases (`listDatabases`), the tree's top level.
    databases: Vec<DbInfo>,
    /// `db -> its collections`, filled lazily when a database branch expands.
    collections: BTreeMap<String, Vec<CollectionInfo>>,
    /// Which database branches are expanded in the tree.
    expanded: BTreeSet<String>,
    /// The last browse error (a failed list/find), shown inline in the tree.
    error: Option<String>,
    /// A destructive write awaiting confirmation (drop / delete), tagged with the
    /// originating collection's epoch so a confirmed re-send lands on it. Rendered
    /// as a modal over the shell.
    pending_write: Option<(Epoch, DocWrite, String)>,
    /// The open tabs (one collection each, plus blank chooser tabs).
    tabs: Vec<MongoTab>,
    /// Index into `tabs` of the Primary half's visible tab.
    active_tab: usize,
    /// Monotonic id source for `MongoTab::id`.
    tab_seq: u64,
    /// Horizontal scroll for the tab strip.
    tab_scroll: ScrollHandle,
    /// The gap a dragged tab would land in during a reorder, or `None`.
    tab_drop_target: Option<usize>,
    /// The side-by-side split (reuses the SQL/Redis [`SplitState`]); `None` is the
    /// ordinary single-pane layout.
    split: Option<SplitState>,
    /// The tab whose right-click context menu is open, as `(id, position)`.
    tab_menu: Option<(u64, gpui::Point<gpui::Pixels>)>,
    /// The documents toolbar's "Actions" dropdown, anchored at the trigger while
    /// open (Explain / New / Drop live here to keep the toolbar uncrowded).
    /// Mirrors the Redis `actions_menu` positioned-menu pattern.
    actions_menu: Option<gpui::Point<gpui::Pixels>>,
    /// The `database -> collection` sidebar tree's keyboard focus handle; the
    /// `FocusSchema` action and a tree click plant focus here.
    tree_focus: FocusHandle,
    /// The sidebar search box: narrows the tree to databases / collections whose
    /// name matches, live as the user types (mirrors the SQL schema filter). ⌘F
    /// from the tree / root focuses it.
    tree_filter: Entity<TextInput>,
    /// The tree's scroll position, so keyboard nav can reveal the selected row.
    tree_scroll: UniformListScrollHandle,
    /// The tree's keyboard selection, as a stable identity so it survives a
    /// re-flatten (databases loading, a branch expanding). Mirrors the schema
    /// sidebar's `NodeId` selection.
    tree_selected: Option<DocTreeSel>,
}

/// A stable identity for a collection-tree row, so the keyboard selection
/// survives a re-flatten. Mirrors the schema sidebar's `NodeId`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DocTreeSel {
    /// A database branch (depth 0).
    Db(String),
    /// A collection leaf (depth 1) under its database.
    Coll { db: String, coll: String },
}

/// What a flattened tree row renders as, alongside its `TreeItem` structure.
enum DocTreeKind {
    /// A database branch: its name.
    Db { name: String },
    /// A collection leaf: its name, kind badge, estimated count, and whether it's
    /// open in a tab.
    Coll {
        name: String,
        kind: red_core::doc::CollKind,
        count: u64,
        open: bool,
    },
    /// A non-navigable hint row ("(no collections)" / "Loading...").
    Placeholder(&'static str),
}

/// One visible row of the collection tree in display order: the `TreeItem`
/// structure Flint needs plus the identity and render data RED acts on. Built by
/// [`flatten_doc_tree`], mirroring the schema sidebar's `flatten`.
struct DocTreeRow {
    item: TreeItem,
    /// The selectable identity, or `None` for a placeholder row.
    sel: Option<DocTreeSel>,
    kind: DocTreeKind,
}

/// The open collection in a tab: its current window of documents plus the sampled
/// columns, the sub-panel selection, and the inspector. Each carries its own
/// `epoch` so a stale page for a since-closed or repointed tab is dropped.
struct CollView {
    /// This collection tab's backend epoch; every collection-scoped `Doc*`
    /// command carries it and replies route back by matching it.
    epoch: Epoch,
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
    /// How the Documents panel renders each document (table / list / json).
    view_mode: DocViewMode,
    /// The documents expanded in List mode, by row index.
    expanded_rows: BTreeSet<usize>,
    /// The document row open in the inspector, if any.
    inspector: Option<usize>,
    /// Whether the inspector is composing a *new* document (insert mode) rather
    /// than editing the selected row.
    inspector_insert: bool,
    /// Which editing surface the inspector shows: the field-by-field `Form` or the
    /// raw extended-JSON `Raw` editor. `Form` is the default; toggling to `Raw`
    /// serializes the current form into the editor.
    inspector_mode: InspectorMode,
    /// The field-by-field editor's model, built when the inspector opens on a
    /// document (edit) or a blank/clone template (insert). `None` on read-only
    /// connections and before the inspector first opens.
    form: Option<DocForm>,
    /// The inspector's extended-JSON editor (edit-and-save / compose).
    inspector_editor: Entity<CodeEditor>,
    /// Which main view is shown (documents / query / schema / indexes).
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
    /// The JSON render mode's per-document selectable blocks: one
    /// [`SelectableLabel`] per document holding its pretty extended JSON, rendered
    /// through a virtualized [`ListState`] so only on-screen documents are laid
    /// out (60fps on a full page). Rebuilt when the window changes while JSON mode
    /// shows; coordinated by [`selection_group`](Self::selection_group).
    json_labels: Vec<Entity<SelectableLabel>>,
    /// Virtualized list state for the JSON render mode (variable-height rows).
    json_list: ListState,
    /// Virtualized list state for the List render mode (variable-height cards).
    list_state: ListState,
    /// The List render mode's per-document selectable field blocks, created when a
    /// document is expanded (keyed by row index) and cleared when the window
    /// changes. Coordinated by [`selection_group`](Self::selection_group) so only
    /// one block holds a highlight at a time.
    list_labels: BTreeMap<usize, Entity<SelectableLabel>>,
    /// Shared "who owns the live selection" cell for the List/JSON blocks.
    selection_group: SelectionGroup,
    list_focus: FocusHandle,
    /// The keyboard row cursor over `docs` (arrow / vim motions), or `None`
    /// before the grid has been touched. Drives the grid highlight and the
    /// Enter-to-inspect target, falling back to the inspected row.
    cursor: Option<usize>,
}

impl CollView {
    fn new(epoch: Epoch, db: String, coll: String, cx: &mut Context<AppState>) -> Self {
        let filter_input = cx.new(|cx| {
            TextInput::new(cx).with_placeholder("filter, e.g. { \"status\": \"active\" }")
        });
        // Apply the filter on Enter, mirroring the SQL/Redis filter bars.
        cx.subscribe(&filter_input, |this, _input, event: &TextInputEvent, cx| {
            if !matches!(event, TextInputEvent::Submit) {
                return;
            }
            if let Some(session) = this.doc_active_session() {
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
                if let Some(session) = this.doc_active_session() {
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
                if let Some(session) = this.doc_active_session() {
                    this.doc_save_document(session, cx);
                }
            },
        )
        .detach();
        Self {
            epoch,
            db,
            coll,
            docs: Vec::new(),
            skip: 0,
            total: None,
            exhausted: false,
            loading: true,
            columns: Vec::new(),
            view_mode: DocViewMode::Table,
            expanded_rows: BTreeSet::new(),
            inspector: None,
            inspector_insert: false,
            inspector_mode: InspectorMode::Form,
            form: None,
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
            json_labels: Vec::new(),
            json_list: new_doc_list_state(0),
            list_state: new_doc_list_state(0),
            list_labels: BTreeMap::new(),
            selection_group: SelectionGroup::default(),
            list_focus: cx.focus_handle(),
            cursor: None,
        }
    }

    /// (Re)build the JSON render mode's per-document selectable blocks for the
    /// current window and resize its virtualized list. Cheap to hold (each label
    /// only shapes its text when actually painted).
    fn rebuild_json_labels(&mut self, cx: &mut Context<AppState>) {
        let group = self.selection_group.clone();
        self.json_labels = self
            .docs
            .iter()
            .enumerate()
            .map(|(i, doc)| {
                let text = pretty_extjson(&doc.to_doc_value());
                cx.new(|cx| SelectableLabel::new(text, cx).selection_group(group.clone(), i as u64))
            })
            .collect();
        self.json_list.reset(self.json_labels.len());
    }
}

/// A fresh top-aligned [`ListState`] for a document window, with enough overdraw
/// that a flick keeps painted rows ahead of the viewport.
fn new_doc_list_state(count: usize) -> ListState {
    ListState::new(count, ListAlignment::Top, gpui::px(600.))
}

impl WorkspaceTab for MongoTab {
    fn pane(&self) -> SplitHalf {
        self.pane
    }
    fn set_pane(&mut self, half: SplitHalf) {
        self.pane = half;
    }
    fn pinned(&self) -> bool {
        self.pinned
    }
}

impl TabWorkspace for MongoView {
    type Tab = MongoTab;
    fn ws_tabs(&self) -> &[MongoTab] {
        &self.tabs
    }
    fn ws_tabs_mut(&mut self) -> &mut Vec<MongoTab> {
        &mut self.tabs
    }
    fn ws_active(&self) -> usize {
        self.active_tab
    }
    fn ws_set_active(&mut self, i: usize) {
        self.active_tab = i;
    }
    fn ws_split(&self) -> Option<&SplitState> {
        self.split.as_ref()
    }
    fn ws_split_mut(&mut self) -> &mut Option<SplitState> {
        &mut self.split
    }
    /// Like Redis, Mongo has no separate pinned strip section, so pinned tabs
    /// sort ahead within their pane's strip.
    fn pins_sort_first(&self) -> bool {
        true
    }
}

impl MongoView {
    /// Build the view for a freshly-connected Mongo session. The first
    /// `DocListDatabases` fires from [`AppState::doc_start_browse`] once the
    /// session is live. Opens with a single blank tab (the shell always shows
    /// something, and ⌘T / the ＋ button open more).
    pub(crate) fn new(session: SessionId, read_only: bool, cx: &mut Context<AppState>) -> Self {
        Self {
            session,
            read_only,
            epoch: crate::result::next_kv_epoch(),
            databases: Vec::new(),
            collections: BTreeMap::new(),
            expanded: BTreeSet::new(),
            error: None,
            pending_write: None,
            tabs: vec![MongoTab {
                id: 0,
                title: "New tab".to_string(),
                state: MongoTabState::Empty,
                pane: SplitHalf::Primary,
                pinned: false,
            }],
            active_tab: 0,
            tab_seq: 1,
            tab_scroll: ScrollHandle::new(),
            tab_drop_target: None,
            split: None,
            tab_menu: None,
            actions_menu: None,
            tree_focus: cx.focus_handle(),
            tree_filter: {
                let filter =
                    cx.new(|cx| TextInput::new(cx).with_placeholder("Search collections…"));
                // Re-render so the filter narrows the tree live as the user types.
                cx.subscribe(&filter, |_this, _input, _evt: &TextInputEvent, cx| {
                    cx.notify()
                })
                .detach();
                filter
            },
            tree_scroll: UniformListScrollHandle::new(),
            tree_selected: None,
        }
    }

    /// Flatten the `database -> collection` tree into visible rows in display
    /// order (the index each Flint `Tree` handler passes back), narrowed to
    /// `filter` (case-insensitive substring; empty matches all). Mirrors the
    /// schema sidebar's `flatten`. Filtering can only see already-loaded
    /// collections, so an unexpanded database stays visible as a browsable anchor
    /// rather than being hidden on the strength of a name it hasn't fetched yet.
    fn flatten_doc_tree(&self, filter: &str) -> Vec<DocTreeRow> {
        let f = filter.trim().to_lowercase();
        let filtering = !f.is_empty();
        let hit = |name: &str| name.to_lowercase().contains(&f);

        let open: Vec<(&str, &str)> = self
            .tabs
            .iter()
            .filter_map(|t| match &t.state {
                MongoTabState::Collection(c) => Some((c.db.as_str(), c.coll.as_str())),
                MongoTabState::Empty => None,
            })
            .collect();
        let mut rows = Vec::new();
        for db in &self.databases {
            let db_match = filtering && hit(&db.name);
            let colls = self.collections.get(&db.name);
            let coll_hit = colls.is_some_and(|cs| cs.iter().any(|c| hit(&c.name)));

            // A loaded database with neither a name match nor a matching
            // collection has definitively nothing to show; drop it. An unloaded
            // one is kept (we can't prove absence without fetching it).
            if filtering && !db_match && !coll_hit && colls.is_some() {
                continue;
            }
            // Force the branch open while filtering so matches are visible without
            // the user expanding each database by hand.
            let expanded = if filtering {
                self.expanded.contains(&db.name) || coll_hit || db_match
            } else {
                self.expanded.contains(&db.name)
            };
            rows.push(DocTreeRow {
                item: TreeItem::new(0, true, expanded),
                sel: Some(DocTreeSel::Db(db.name.clone())),
                kind: DocTreeKind::Db {
                    name: db.name.clone(),
                },
            });
            if !expanded {
                continue;
            }
            match colls {
                Some(colls) if !colls.is_empty() => {
                    // A database whose own name matches shows all its collections;
                    // otherwise only the matching ones.
                    for coll in colls
                        .iter()
                        .filter(|c| !filtering || db_match || hit(&c.name))
                    {
                        let is_open = open.iter().any(|(d, c)| *d == db.name && *c == coll.name);
                        rows.push(DocTreeRow {
                            item: TreeItem::leaf(1),
                            sel: Some(DocTreeSel::Coll {
                                db: db.name.clone(),
                                coll: coll.name.clone(),
                            }),
                            kind: DocTreeKind::Coll {
                                name: coll.name.clone(),
                                kind: coll.kind,
                                count: coll.est_count,
                                open: is_open,
                            },
                        });
                    }
                }
                Some(_) => rows.push(DocTreeRow {
                    item: TreeItem::leaf(1),
                    sel: None,
                    kind: DocTreeKind::Placeholder("(no collections)"),
                }),
                None => rows.push(DocTreeRow {
                    item: TreeItem::leaf(1),
                    sel: None,
                    kind: DocTreeKind::Placeholder("Loading..."),
                }),
            }
        }
        rows
    }

    fn tab_index_by_id(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// The collection shown by the tab at `idx` (render-time, per split half).
    fn coll_at(&self, idx: usize) -> Option<&CollView> {
        match self.tabs.get(idx).map(|t| &t.state)? {
            MongoTabState::Collection(c) => Some(&**c),
            MongoTabState::Empty => None,
        }
    }

    /// The focused tab's collection (UI actions target the visible tab).
    fn focused_coll(&self) -> Option<&CollView> {
        self.coll_at(self.focused_tab_index())
    }

    fn focused_coll_mut(&mut self) -> Option<&mut CollView> {
        let i = self.focused_tab_index();
        match self.tabs.get_mut(i).map(|t| &mut t.state)? {
            MongoTabState::Collection(c) => Some(&mut **c),
            MongoTabState::Empty => None,
        }
    }

    /// The collection tab that owns `epoch` — backend replies route here so a
    /// background tab's in-flight read still lands on the tab that asked (even in
    /// split view, or after focus moved).
    fn coll_by_epoch_mut(&mut self, epoch: Epoch) -> Option<&mut CollView> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            MongoTabState::Collection(c) if c.epoch == epoch => Some(&mut **c),
            _ => None,
        })
    }
}

impl AppState {
    /// The session of the active connection when it's a Mongo one, for the
    /// editor-subscription callbacks and palette commands.
    pub(crate) fn doc_active_session(&self) -> Option<SessionId> {
        match &self.phase {
            Phase::Connected(a) if a.doc_view.is_some() => Some(a.session),
            _ => None,
        }
    }

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
        let Some(current) = view.coll_by_epoch_mut(epoch) else {
            return;
        };
        // Drop a late page for a collection the tab has since been repointed at.
        if current.db != db || current.coll != coll {
            return;
        }
        current.columns = sample_columns(&docs);
        current.docs = docs;
        current.skip = skip;
        current.exhausted = exhausted;
        current.loading = false;
        current.expanded_rows.clear();
        // The List/JSON selectable blocks were built for the old window; drop them
        // and resize the virtualized lists to the new count.
        current.list_labels.clear();
        current.json_labels.clear();
        let n = current.docs.len();
        current.list_state.reset(n);
        current.json_list.reset(n);
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
        // A new window invalidates the keyboard cursor's absolute row.
        if current.cursor.is_some_and(|c| c >= current.docs.len()) {
            current.cursor = None;
        }
        // Rebuild the JSON blocks if that's the visible mode.
        if matches!(current.view_mode, DocViewMode::Json) {
            current.rebuild_json_labels(cx);
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
        // Clear the loading flags on whichever collection tab owns this epoch (a
        // catalog error carries the view epoch and matches no tab — still shown
        // in the tree banner below).
        if let Some(current) = view.coll_by_epoch_mut(epoch) {
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

    /// Move the collection tree's keyboard selection (arrows / Enter, plus the
    /// vim aliases), driven by Flint's [`TreeNav`]. Left collapses a database or
    /// steps to the parent; Right expands or descends; Enter opens a collection or
    /// toggles a database. Mirrors the schema sidebar's `schema_nav`.
    fn doc_tree_nav(&mut self, session: SessionId, nav: TreeNav, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_ref())
        else {
            return;
        };
        // Snapshot the rows (owned, so no borrow of `self` is held while the
        // mutating handlers below run) and the selected row's position. Keyboard
        // nav walks the same filtered rows the tree shows.
        let filter = view.tree_filter.read(cx).content().to_string();
        let rows = view.flatten_doc_tree(&filter);
        if rows.is_empty() {
            return;
        }
        let sel = view
            .tree_selected
            .as_ref()
            .and_then(|s| rows.iter().position(|r| r.sel.as_ref() == Some(s)));

        match nav {
            TreeNav::Up => {
                if let Some(ix) = next_navigable_doc(&rows, sel, false) {
                    self.doc_tree_select(session, &rows, ix, cx);
                }
            }
            TreeNav::Down => {
                if let Some(ix) = next_navigable_doc(&rows, sel, true) {
                    self.doc_tree_select(session, &rows, ix, cx);
                }
            }
            TreeNav::Expand => {
                let Some(i) = sel else { return };
                let row = &rows[i];
                if row.item.has_children && !row.item.expanded {
                    if let Some(DocTreeSel::Db(db)) = row.sel.clone() {
                        self.doc_toggle_db(session, db, cx);
                    }
                } else if row.item.expanded {
                    // Already open: descend to the first child (next row down).
                    if let Some(ix) = next_navigable_doc(&rows, sel, true) {
                        self.doc_tree_select(session, &rows, ix, cx);
                    }
                }
            }
            TreeNav::Collapse => {
                let Some(i) = sel else { return };
                let row = &rows[i];
                if row.item.has_children && row.item.expanded {
                    if let Some(DocTreeSel::Db(db)) = row.sel.clone() {
                        self.doc_toggle_db(session, db, cx);
                    }
                } else if row.item.depth > 0 {
                    // A collection leaf: jump to its parent database (the nearest
                    // row above at a shallower depth).
                    if let Some(p) = (0..i).rev().find(|&j| rows[j].item.depth < row.item.depth) {
                        self.doc_tree_select(session, &rows, p, cx);
                    }
                }
            }
            TreeNav::Activate => {
                let Some(i) = sel else { return };
                match row_sel_owned(&rows, i) {
                    Some(DocTreeSel::Coll { db, coll }) => {
                        self.doc_open_collection(session, db, coll, false, cx);
                    }
                    Some(DocTreeSel::Db(db)) => self.doc_toggle_db(session, db, cx),
                    None => {}
                }
            }
        }
    }

    /// Set the tree's keyboard selection to `rows[ix]` and reveal it. `ix` indexes
    /// the flattened rows (the same index Flint hands back).
    fn doc_tree_select(
        &mut self,
        session: SessionId,
        rows: &[DocTreeRow],
        ix: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.tree_selected = rows[ix].sel.clone();
            view.tree_scroll
                .scroll_to_item(ix, gpui::ScrollStrategy::Nearest);
            cx.notify();
        }
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
        let Some(current) = view.focused_coll_mut() else {
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
        let (epoch, db, coll, filter) = (
            current.epoch,
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

    /// Apply the filter box's current text: parsing happens in the service, so
    /// here just re-fetch from `skip = 0` with the (trimmed) filter, or clear it
    /// when the box is empty.
    fn doc_apply_filter(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let Some(current) = view.focused_coll_mut() else {
            return;
        };
        let text = current.filter_input.read(cx).content().trim().to_string();
        current.filter = (!text.is_empty()).then_some(text);
        current.skip = 0;
        current.loading = true;
        current.inspector = None;
        current.panel = DocPanel::Documents;
        let (epoch, db, coll, filter) = (
            current.epoch,
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
        let Some(current) = view.focused_coll_mut() else {
            return;
        };
        current.panel = panel;
        let (epoch, db, coll) = (current.epoch, current.db.clone(), current.coll.clone());
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

    /// Toggle a document's expansion in List mode. Expanding builds the row's
    /// selectable field block (a `SelectableLabel`); collapsing drops it.
    fn doc_toggle_row(&mut self, session: SessionId, row: usize, cx: &mut Context<Self>) {
        // Phase A: flip the expansion and, when opening, gather what the label
        // needs (its text + the selection group) without holding the borrow.
        let build = {
            let Some(current) = self
                .conn_mut(Some(session))
                .and_then(|a| a.doc_view.as_mut())
                .and_then(|v| v.focused_coll_mut())
            else {
                return;
            };
            if current.expanded_rows.remove(&row) {
                current.list_labels.remove(&row);
                None
            } else {
                current.expanded_rows.insert(row);
                current
                    .docs
                    .get(row)
                    .map(|doc| (doc_field_text(doc), current.selection_group.clone()))
            }
        };
        // Phase B: build the label entity (needs `cx`), then store it back.
        let label = build.map(|(text, group)| {
            cx.new(|cx| SelectableLabel::new(text, cx).selection_group(group, row as u64))
        });
        if let Some(current) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.focused_coll_mut())
        {
            if let Some(label) = label {
                current.list_labels.insert(row, label);
            }
            // The toggled card changed height; re-measure just that row so the
            // virtualized list lays the rest out correctly.
            current.list_state.remeasure_items(row..row + 1);
        }
        cx.notify();
    }

    /// Switch how the Documents panel renders each document (table/list/json).
    fn doc_set_view_mode(&mut self, session: SessionId, mode: DocViewMode, cx: &mut Context<Self>) {
        let Some(current) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.focused_coll_mut())
        else {
            return;
        };
        current.view_mode = mode;
        // Build the JSON blocks lazily the first time JSON mode is shown for this
        // window (cleared on every page load).
        if matches!(mode, DocViewMode::Json) && current.json_labels.len() != current.docs.len() {
            current.rebuild_json_labels(cx);
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
        if let Some(current) = view.coll_by_epoch_mut(epoch)
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
        if let Some(current) = view.coll_by_epoch_mut(epoch)
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
        let Some(current) = view.focused_coll_mut() else {
            return;
        };
        let pipeline = current.query_editor.read(cx).content();
        let pipeline = if pipeline.trim().is_empty() {
            "[]".to_string()
        } else {
            pipeline
        };
        current.query_loading = true;
        let (epoch, db, coll) = (current.epoch, current.db.clone(), current.coll.clone());
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
            .and_then(|a| a.doc_view.as_ref())
        else {
            return;
        };
        let Some(current) = view.focused_coll() else {
            return;
        };
        let (epoch, db, coll, filter) = (
            current.epoch,
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
            .and_then(|v| v.focused_coll_mut())
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
        if let Some(current) = view.coll_by_epoch_mut(epoch)
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
        if let Some(current) = view.coll_by_epoch_mut(epoch)
            && current.db == db
            && current.coll == coll
        {
            current.explain = Some(plan);
            cx.notify();
        }
    }

    /// Move the document grid's keyboard cursor (arrows / Home / End / Page /
    /// ⌘arrows, plus the vim aliases), driven by Flint's [`TableNav`]. The grid is
    /// a single logical column, so Left/Right are inert; the cursor clamps within
    /// the loaded window (paging is skip-based, not append). Mirrors the Redis
    /// `kv_browse_nav`.
    fn doc_grid_nav(&mut self, session: SessionId, nav: TableNav, cx: &mut Context<Self>) {
        if matches!(nav, TableNav::Left | TableNav::Right) {
            return;
        }
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        if current.docs.is_empty() {
            return;
        }
        let last = current.docs.len() - 1;
        let cur = current.cursor.unwrap_or(0).min(last);

        // A page jump moves by a screenful; the grid renders roughly this many
        // rows at the default height, matching the Redis browse list's step.
        const STEP: usize = 12;
        let next = match nav {
            TableNav::Up => cur.saturating_sub(1),
            TableNav::Down => (cur + 1).min(last),
            TableNav::PageUp => cur.saturating_sub(STEP),
            TableNav::PageDown => (cur + STEP).min(last),
            TableNav::First | TableNav::RowStart => 0,
            TableNav::Last | TableNav::RowEnd => last,
            // Left/Right handled above.
            _ => cur,
        };
        current.cursor = Some(next);
        current
            .scroll
            .scroll_to_item(next, gpui::ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Enter / F2 on the document grid: open the inspector on the keyboard cursor's
    /// row. Returns `true` when it handled the key (the doc grid is the focused
    /// table), so the shared `BeginEdit` handler falls through otherwise. Mirrors
    /// the Redis `kv_activate_cursor`.
    pub(crate) fn doc_activate_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Phase::Connected(active) = &self.phase else {
            return false;
        };
        let session = active.session;
        let Some(current) = active.doc_view.as_ref().and_then(|v| v.focused_coll()) else {
            return false;
        };
        // Only when the grid actually holds focus, so Enter in the filter box or
        // the inspector editor isn't hijacked.
        if !current.list_focus.is_focused(window) {
            return false;
        }
        if current.docs.is_empty() {
            return true;
        }
        let row = current.cursor.unwrap_or(0).min(current.docs.len() - 1);
        self.doc_toggle_inspector(session, row, cx);
        true
    }

    /// ⌘F in a Mongo session: jump focus to the open collection's filter box (the
    /// extended-JSON find field) instead of the SQL find/search. Returns `true`
    /// when it handled it (the foreground connection is Mongo with a collection
    /// open), so the caller falls through to the SQL path otherwise. Mirrors the
    /// Redis `kv_focus_filter`.
    pub(crate) fn doc_focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Phase::Connected(active) = &self.phase else {
            return false;
        };
        let Some(current) = active.doc_view.as_ref().and_then(|v| v.focused_coll()) else {
            return false;
        };
        let handle = current.filter_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
        true
    }

    /// ⌘F from the collection tree or the shell root: reveal the sidebar and focus
    /// its collection-search box (the SQL "search schema" idiom). Returns `true`
    /// when the foreground connection is Mongo, so the caller falls through to the
    /// SQL path otherwise.
    pub(crate) fn doc_focus_tree_filter(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let handle = match &self.phase {
            Phase::Connected(active) => match active.doc_view.as_ref() {
                Some(v) => v.tree_filter.read(cx).focus_handle(cx),
                None => return false,
            },
            _ => return false,
        };
        if let Phase::Connected(active) = &mut self.phase {
            active.sidebar_collapsed = false;
        }
        window.focus(&handle, cx);
        cx.notify();
        true
    }

    /// Route the SQL pane-focus vocabulary onto the Mongo shell: `Schema` focuses
    /// the collection tree (revealing the sidebar), `Grid` the document grid, and
    /// `Editor` the filter bar. No-op when the focused tab holds no collection.
    pub(crate) fn doc_focus_pane(
        &mut self,
        pane: Pane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if pane == Pane::Schema
            && let Phase::Connected(active) = &mut self.phase
        {
            active.sidebar_collapsed = false;
        }
        let handle = match &self.phase {
            Phase::Connected(active) => {
                let Some(v) = active.doc_view.as_ref() else {
                    return;
                };
                match pane {
                    Pane::Schema => Some(v.tree_focus.clone()),
                    Pane::Grid => v.focused_coll().map(|c| c.list_focus.clone()),
                    Pane::Editor => v
                        .focused_coll()
                        .map(|c| c.filter_input.read(cx).focus_handle(cx)),
                }
            }
            _ => return,
        };
        let Some(handle) = handle else { return };
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Cycle keyboard focus between the Mongo shell's two stops: the collection
    /// tree and the document grid. The direction is immaterial with two stops.
    pub(crate) fn doc_cycle_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tree_focused = matches!(&self.phase, Phase::Connected(a)
            if a.doc_view.as_ref().is_some_and(|v| v.tree_focus.is_focused(window)));
        let pane = if tree_focused {
            Pane::Grid
        } else {
            Pane::Schema
        };
        self.doc_focus_pane(pane, window, cx);
    }

    /// Open (or, on the same row, close) the inspector on a document row, loading
    /// the row's extended JSON into the editor when it opens.
    fn doc_toggle_inspector(&mut self, session: SessionId, row: usize, cx: &mut Context<Self>) {
        // Phase A: flip the selection and, when opening, clone the target document
        // so the form/editor can be built without holding the borrow.
        let doc = {
            let Some(current) = self.doc_focused_coll_mut(session) else {
                return;
            };
            if current.inspector == Some(row) && !current.inspector_insert {
                current.inspector = None;
                current.form = None;
                None
            } else {
                current.inspector = Some(row);
                current.inspector_insert = false;
                current.docs.get(row).cloned()
            }
        };
        if let Some(d) = doc {
            // Build both surfaces from the same document: the field-by-field form
            // and the raw extended-JSON editor stay a toggle apart.
            let form = DocForm::from_document(&d, session, cx);
            let editor = {
                let Some(current) = self.doc_focused_coll_mut(session) else {
                    return;
                };
                current.form = Some(form);
                current.inspector_editor.clone()
            };
            editor.update(cx, |ed, cx| {
                ed.set_content(pretty_extjson(&d.to_doc_value()), cx)
            });
        }
        cx.notify();
    }

    /// Open the inspector in compose mode with a blank document template.
    fn doc_new_document(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let form = DocForm::blank(session, cx);
        let editor = {
            let Some(current) = self.doc_focused_coll_mut(session) else {
                return;
            };
            current.inspector = None;
            current.inspector_insert = true;
            current.form = Some(form);
            current.inspector_editor.clone()
        };
        editor.update(cx, |ed, cx| ed.set_content("{\n  \n}", cx));
        cx.notify();
    }

    /// Clone the selected document into the compose editor (drops `_id` so the
    /// insert mints a fresh one), the Compass-style "insert a copy" affordance.
    fn doc_clone_document(&mut self, session: SessionId, row: usize, cx: &mut Context<Self>) {
        let doc = self
            .doc_focused_coll_mut(session)
            .and_then(|current| current.docs.get(row).cloned());
        let Some(d) = doc else {
            return;
        };
        // Clone the fields only — a new `_id` is minted on insert.
        let form = DocForm::from_fields(&d.fields, session, cx);
        let editor = {
            let Some(current) = self.doc_focused_coll_mut(session) else {
                return;
            };
            current.inspector = None;
            current.inspector_insert = true;
            current.form = Some(form);
            current.inspector_editor.clone()
        };
        let body = DocValue::Document(d.fields.clone());
        editor.update(cx, |ed, cx| ed.set_content(pretty_extjson(&body), cx));
        cx.notify();
    }

    /// Save the inspector: insert a new document (compose mode) or replace the
    /// selected one (edit mode). The body comes from whichever surface is active —
    /// the field-by-field form (serialized to extended JSON here) or the raw
    /// editor. Final parsing happens service-side.
    fn doc_save_document(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let plan = {
            let Some(view) = self
                .conn_mut(Some(session))
                .and_then(|a| a.doc_view.as_ref())
            else {
                return;
            };
            if view.read_only {
                return;
            }
            let Some(current) = view.focused_coll() else {
                return;
            };
            let json = match current.inspector_mode {
                InspectorMode::Form => match &current.form {
                    Some(form) => form.serialize(cx),
                    None => Ok(current.inspector_editor.read(cx).content().to_string()),
                },
                InspectorMode::Raw => Ok(current.inspector_editor.read(cx).content().to_string()),
            };
            let id = current
                .inspector
                .and_then(|row| current.docs.get(row))
                .map(|d| d.id.clone());
            (
                current.epoch,
                current.db.clone(),
                current.coll.clone(),
                current.inspector_insert,
                id,
                json,
            )
        };
        let (epoch, db, coll, insert, id, json) = plan;
        let doc_json = match json {
            Ok(j) => j,
            Err(err) => {
                self.notify(ToastVariant::Error, err, cx);
                return;
            }
        };
        let cmd = if insert {
            Command::DocInsert {
                epoch,
                db,
                coll,
                doc_json,
            }
        } else if let Some(id) = id {
            Command::DocReplace {
                epoch,
                db,
                coll,
                id,
                doc_json,
            }
        } else {
            return;
        };
        self.service.send_to(session, cmd);
    }

    /// Locate the currently-focused collection view for `session`, mutably.
    /// The shared entry point for the form-editing commands in `form.rs`.
    fn doc_focused_coll_mut(&mut self, session: SessionId) -> Option<&mut CollView> {
        self.conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.focused_coll_mut())
    }

    /// Queue a delete of a document behind the confirm modal.
    fn doc_delete_row(&mut self, session: SessionId, row: usize, cx: &mut Context<Self>) {
        let pending = {
            let Some(view) = self
                .conn_mut(Some(session))
                .and_then(|a| a.doc_view.as_mut())
            else {
                return;
            };
            if view.read_only {
                return;
            }
            let Some(current) = view.focused_coll() else {
                return;
            };
            let Some(doc) = current.docs.get(row) else {
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
            (current.epoch, write, prompt)
        };
        // Opt-out: when delete confirmations are disabled, apply the delete straight
        // away (still gated as a single, filtered write server-side).
        if !self.settings.query.confirm_destructive {
            let (epoch, write, _) = pending;
            self.service.send_to(
                session,
                Command::DocApplyWrite {
                    epoch,
                    write,
                    confirmed: true,
                },
            );
            cx.notify();
            return;
        }
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.pending_write = Some(pending);
            cx.notify();
        }
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
            view.focused_coll().map(|current| Command::DocApplyWrite {
                epoch: current.epoch,
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

    /// Approve the pending destructive write: re-send it confirmed against the
    /// originating collection's epoch.
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
                .map(|(epoch, write, _)| (epoch, write))
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
            .and_then(|v| v.focused_coll_mut())
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
        view.pending_write = Some((epoch, write, prompt));
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
        // Close the inspector on the writing tab, clear any pending confirm, and
        // gather what the browse needs to refresh, all before the toast + re-fetch.
        let refresh = {
            let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
                return;
            };
            view.pending_write = None;
            view.coll_by_epoch_mut(epoch).map(|c| {
                c.inspector = None;
                c.inspector_insert = false;
                c.loading = true;
                (
                    c.epoch,
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

// --- free helpers ------------------------------------------------------------

/// The union of top-level field names across a window, `_id` first, capped to
/// [`MAX_COLUMNS`]. Stable order: `_id`, then first-seen order across the docs,
/// so the grid columns don't reshuffle between rows.
/// The next navigable (non-placeholder) tree row from `from` in `forward`, or the
/// first/last navigable row when nothing is selected yet. Mirrors the schema
/// sidebar's `next_navigable`.
fn next_navigable_doc(rows: &[DocTreeRow], from: Option<usize>, forward: bool) -> Option<usize> {
    let len = rows.len();
    let has_sel = |i: usize| rows[i].sel.is_some();
    match (from, forward) {
        (None, true) => (0..len).find(|&i| has_sel(i)),
        (None, false) => (0..len).rev().find(|&i| has_sel(i)),
        (Some(cur), true) => ((cur + 1)..len).find(|&i| has_sel(i)),
        (Some(cur), false) => (0..cur).rev().find(|&i| has_sel(i)),
    }
}

/// The owned selection identity for `rows[ix]`, so the caller can act on it after
/// dropping the borrow on `rows`.
fn row_sel_owned(rows: &[DocTreeRow], ix: usize) -> Option<DocTreeSel> {
    rows.get(ix).and_then(|r| r.sel.clone())
}

/// Max field rows a List-mode block flattens (deep/wide documents fall back to
/// the inspector for the full picture).
const MAX_FIELD_ROWS: usize = 300;

/// A flattened List-mode field: its label, display value, and nesting depth.
struct FieldRow {
    key: String,
    value: String,
    depth: usize,
}

/// Flatten one field (recursing into objects/arrays) into `FieldRow` data, capped
/// at [`MAX_FIELD_ROWS`].
fn push_field_data(key: &str, value: &DocValue, depth: usize, out: &mut Vec<FieldRow>) {
    if out.len() >= MAX_FIELD_ROWS {
        return;
    }
    let row = |value: String, depth: usize| FieldRow {
        key: key.to_string(),
        value,
        depth,
    };
    match value {
        DocValue::Document(fields) if !fields.is_empty() => {
            out.push(row("{ }".to_string(), depth));
            for (k, v) in fields {
                push_field_data(k, v, depth + 1, out);
            }
        }
        DocValue::Array(items) if !items.is_empty() => {
            out.push(row(format!("[ {} ]", items.len()), depth));
            for (i, item) in items.iter().enumerate() {
                push_field_data(&i.to_string(), item, depth + 1, out);
            }
        }
        other => out.push(row(other.to_cell(CELL_CAP).to_string(), depth)),
    }
}

/// The selectable "key: value" block for one document's List-mode card: `_id`
/// first, then each field, nested objects/arrays indented two spaces per level.
fn doc_field_text(doc: &Document) -> String {
    let mut fields = Vec::new();
    push_field_data("_id", &doc.id, 0, &mut fields);
    for (k, v) in &doc.fields {
        push_field_data(k, v, 0, &mut fields);
    }
    let mut out = String::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        for _ in 0..f.depth {
            out.push_str("  ");
        }
        out.push_str(&f.key);
        out.push_str(": ");
        out.push_str(&f.value);
    }
    out
}

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
