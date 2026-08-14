//! The MongoDB browser (`MongoView`), the document-store shell parallel to the Redis
//! `kvbrowse::RedisView`. A `database -> collection` tree on the left and a tabbed work
//! area on the right: each open collection is its own tab (with a per-collection filter
//! bar, document grid, schema/indexes panels, aggregation editor, and inspector), and
//! tabs live in an optional side-by-side split — the same `TabWorkspace` plumbing the
//! SQL and Redis shells share. It speaks the `Doc*` `Command`/`Event` pair (see `red-
//! service`'s protocol) and never touches the `DocDriver` directly, the same UI/driver
//! separation the other shells keep.

mod aggregate;
mod form;
mod indexes;
mod render;
mod tabs;
mod transfer;
mod validator;
mod watch;
mod window;

pub(crate) use aggregate::{DocQueryMode, DocStage};
pub(crate) use form::{DocForm, InspectorMode};
pub(crate) use indexes::DocIndexForm;
pub(crate) use transfer::{DocCopyState, DocExportState, DocImportState};
pub(crate) use validator::DocValidatorForm;
pub(crate) use watch::DocWatchState;

use window::{DocWindow, FetchCtx};

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use flint::prelude::*;
use gpui::{
    Context, Entity, FocusHandle, Focusable, ListAlignment, ListState, UniformListScrollHandle,
    Window, prelude::*,
};
use red_core::doc::{
    CollectionInfo, DbInfo, DocPlan, DocSchema, DocSeek, DocValue, DocWrite, Document, IndexInfo,
};
use red_service::{Command, CommandSender, Epoch, SessionId};

use crate::app::{AppState, Pane, Phase, SplitWorkspace, TabWorkspace, WorkspaceTab};
use crate::panes::{PaneId, PaneLayout};

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
    /// The live change stream.
    Watch,
}

impl DocPanel {
    /// The panels in toolbar order, with their segment labels.
    const ALL: [(DocPanel, &'static str); 5] = [
        (DocPanel::Documents, "Documents"),
        (DocPanel::Query, "Query"),
        (DocPanel::Schema, "Schema"),
        (DocPanel::Indexes, "Indexes"),
        (DocPanel::Watch, "Watch"),
    ];
}

/// How the filter box's text is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocFilterMode {
    /// A raw MongoDB filter document in extended JSON.
    Json,
    /// The `field:value age>30 created:last7d` shorthand, compiled to a filter
    /// document (see [`red_core::doc::compile_fast_filter`]).
    Fast,
}

impl DocFilterMode {
    const ALL: [(DocFilterMode, &'static str); 2] =
        [(DocFilterMode::Fast, "Fast"), (DocFilterMode::Json, "JSON")];

    fn placeholder(self) -> &'static str {
        match self {
            DocFilterMode::Fast => "status:active age>30 created:last7d",
            DocFilterMode::Json => "{ \"status\": \"active\" }",
        }
    }
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

/// The persisted `doc.default_view` maps onto the in-tab mode. Two types for one
/// concept, kept apart deliberately: the settings enum owns the `settings.toml`
/// vocabulary and carries no UI, this one carries the toolbar's display labels.
impl From<crate::settings::DocView> for DocViewMode {
    fn from(view: crate::settings::DocView) -> Self {
        match view {
            crate::settings::DocView::Table => DocViewMode::Table,
            crate::settings::DocView::List => DocViewMode::List,
            crate::settings::DocView::Json => DocViewMode::Json,
        }
    }
}

/// The per-kind state a Mongo tab holds. An `Empty` tab shows the "pick a
/// collection" hint; a `Collection` tab holds a whole [`CollView`]. Boxed like
/// the Redis `Browse` variant because `CollView` dwarfs the empty case.
enum MongoTabState {
    /// A blank tab awaiting a collection choice from the sidebar tree.
    Empty,
    Collection(Box<CollView>),
    /// A database's inferred-relationship diagram.
    Relations(Box<RelationsView>),
}

/// One database's relations diagram: the inferred references, and the ER canvas
/// built from them.
///
/// The references are kept beside the canvas because they carry the *evidence*
/// (how many sampled values resolved), which the diagram cannot show and the
/// footer must: an inferred edge is a claim, not a declaration.
pub(crate) struct RelationsView {
    /// The epoch the inference runs under, so a reply lands on the tab that asked.
    epoch: Epoch,
    db: String,
    loading: bool,
    er: Option<crate::er::ErView>,
    references: Vec<red_core::doc::DocReference>,
    /// How many collections the inference sampled, for the footer.
    sampled: usize,
    error: Option<String>,
}

/// One tab in the Mongo shell: a title, a stable id, and its per-kind state.
pub(crate) struct MongoTab {
    /// Stable identity, never reused, assigned from [`MongoView::tab_seq`].
    id: u64,
    title: String,
    state: MongoTabState,
    /// Which pane this tab belongs to.
    pane: PaneId,
    /// Pinned tabs sort ahead of the rest in their pane's strip.
    pinned: bool,
}

/// The per-connection MongoDB browse state, held as `ActiveConn.doc_view` for a
/// `DbKind::Mongo` session (mirrors `kv_view`). The catalog `epoch` scopes the
/// databases/collections replies; each open collection tab carries its own
/// `epoch` so a page/schema/write reply routes to the tab that asked.
pub(crate) struct MongoView {
    session: SessionId,
    /// The shared query log, so a run can record itself without reaching back
    /// through `AppState`.
    history_store: Entity<red_config::history::QueryHistory>,
    /// The left History dock (⌘Y), a view over the shared log; see
    /// [`crate::dochistory::DocHistoryPanel`].
    pub(crate) history_panel: Entity<crate::dochistory::DocHistoryPanel>,
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
    /// A `$out`/`$merge` pipeline awaiting the same confirm as a destructive write:
    /// `(epoch, pipeline, prompt)`. Kept apart from [`Self::pending_write`] because
    /// it re-sends a `DocAggregate`, not a `DocApplyWrite`; both drive the one modal.
    pending_pipeline: Option<(Epoch, String, String)>,
    /// The open tabs (one collection each, plus blank chooser tabs).
    tabs: Vec<MongoTab>,
    /// Monotonic id source for `MongoTab::id`.
    tab_seq: u64,
    /// How the work area is divided, which pane has focus, and the per-pane
    /// state. Shared with the SQL and Redis sides.
    pub(crate) layout: PaneLayout,
    /// The tab whose right-click context menu is open, as `(id, position)`.
    tab_menu: Option<(u64, gpui::Point<gpui::Pixels>)>,
    /// The documents toolbar's "Actions" dropdown, anchored at the trigger while
    /// open (Explain / New / Drop live here to keep the toolbar uncrowded).
    /// Mirrors the Redis `actions_menu` positioned-menu pattern.
    actions_menu: Option<gpui::Point<gpui::Pixels>>,
    /// The collection-tree row whose right-click menu is open, as
    /// `(db, coll, position)`. A `coll` of `None` is a database row.
    coll_menu: Option<(String, Option<String>, gpui::Point<gpui::Pixels>)>,
    /// The open "Export documents" modal, if any.
    pub(crate) export: Option<DocExportState>,
    /// The open "Import documents" modal, if any.
    pub(crate) import: Option<DocImportState>,
    /// The open "Copy collection to…" modal, if any.
    pub(crate) copy: Option<DocCopyState>,
    /// The open "New index" dialog, if any.
    pub(crate) index_form: Option<DocIndexForm>,
    /// The open "Validation rules" dialog, if any.
    pub(crate) validator: Option<DocValidatorForm>,
    /// The `database -> collection` sidebar tree's keyboard focus handle; the
    /// `FocusSchema` action and a tree click plant focus here.
    pub(crate) tree_focus: FocusHandle,
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
/// `flatten_doc_tree`, mirroring the schema sidebar's `flatten`.
struct DocTreeRow {
    item: TreeItem,
    /// The selectable identity, or `None` for a placeholder row.
    sel: Option<DocTreeSel>,
    kind: DocTreeKind,
}

/// The open collection in a tab: its current window of documents plus the sampled
/// columns, the sub-panel selection, and the inspector. Each carries its own
/// `epoch` so a stale page for a since-closed or repointed tab is dropped.
pub(crate) struct CollView {
    /// This collection tab's backend epoch; every collection-scoped `Doc*`
    /// command carries it and replies route back by matching it.
    epoch: Epoch,
    db: String,
    coll: String,
    /// This session's command sender, bound at open so the grid's load-on-scroll
    /// (`DocWindow::ensure`, called from the paint closure) can issue a
    /// `DocFetchRun` without re-entering the view entity.
    sender: CommandSender,
    /// The windowed `_id`-keyset document run behind the grid: the browse's
    /// continuous scroll over a collection of any size (see [`DocWindow`]). Shared
    /// via `Rc<RefCell>` so the grid's paint-time `on_visible_range` closure can
    /// advance it (evict + fetch on scroll) without re-entering the view entity.
    window: Rc<RefCell<DocWindow>>,
    /// Whether a fetch is in flight (shows a loading hint) before the run first
    /// reports its total.
    loading: bool,
    /// The union of top-level field names seen across resident documents (`_id`
    /// first), capped to `doc.max_columns`; the grid's columns. Accumulated so
    /// columns don't flicker as the window scrolls onto documents of other shapes.
    columns: Vec<String>,
    /// How the Documents panel renders each document (table / list / json).
    view_mode: DocViewMode,
    /// The documents expanded in List mode, by absolute ordinal.
    expanded_rows: BTreeSet<usize>,
    /// The document open in the inspector, by absolute ordinal, if any.
    inspector: Option<usize>,
    /// A snapshot of the inspected document, held so the inspector (and its
    /// save/delete) survives the row scrolling out of the resident window.
    inspector_doc: Option<Document>,
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
    /// Always an extended-JSON document, whichever mode wrote it.
    filter: Option<String>,
    /// How [`Self::filter_input`]'s text is read.
    filter_mode: DocFilterMode,
    /// What the current filter text compiles to, recomputed as it is typed. Drives
    /// the inline hint; `Incomplete` deliberately shows nothing.
    filter_status: red_core::doc::FastFilter,
    /// Schema field paths matching the token being typed, and which is
    /// highlighted. Empty when the popup is closed.
    suggestions: Vec<String>,
    suggestion_ix: usize,
    /// The browse order as `(field, ascending)` keys in priority order. Empty is
    /// the `_id` order the keyset scroll assumes; anything else switches the run to
    /// ordinal paging (see [`window::FetchCtx`]).
    sort: Vec<(String, bool)>,
    /// The chosen field subset, or `None` for whole documents. `_id` always rides
    /// along, so a projected row is still addressable.
    projection: Option<Vec<String>>,
    /// [`Self::sort`] / [`Self::projection`] rendered as the extended JSON the
    /// fetch path sends. Cached rather than rebuilt per paint: the grid's
    /// `on_visible_range` closure borrows them every frame.
    sort_doc: Option<String>,
    projection_doc: Option<String>,
    /// The "Fields" projection dropdown's anchor while it is open.
    fields_menu: Option<gpui::Point<gpui::Pixels>>,
    /// The inferred schema, lazily fetched the first time the Schema panel opens.
    schema: Option<DocSchema>,
    /// The collection's storage numbers, fetched with the schema. `None` until
    /// they arrive, and they may never: an under-privileged role gets no
    /// `collStats`, which the panel shows by simply omitting the header.
    stats: Option<red_core::doc::CollStats>,
    /// The collection's indexes, lazily fetched the first time Indexes opens.
    indexes: Option<Vec<IndexInfo>>,
    /// The explain plan for the current filter, shown as a dismissible readout
    /// on the Documents panel; `None` when not requested / dismissed.
    explain: Option<DocPlan>,
    /// The index keys the last explain suggested (empty when it needs none), for
    /// the Indexes panel's suggestion row. Outlives the dismissible plan readout:
    /// the advice is still true after the readout is closed.
    index_advice: Option<Vec<String>>,
    /// The aggregation-pipeline editor (Query panel), holding the whole pipeline
    /// as text. The source of truth in both query modes: the stage list is split
    /// out of it and joined back into it.
    query_editor: Entity<CodeEditor>,
    /// How the Query panel is edited (one editor, or one per stage).
    query_mode: DocQueryMode,
    /// The stage list, when the panel is in Stages mode.
    stages: Vec<DocStage>,
    /// Which stage the next run previews at, if any: the pipeline is truncated
    /// after it and a `$limit` appended.
    preview_stage: Option<usize>,
    /// The "add stage" palette's insertion point and anchor while it is open.
    stage_menu: Option<(usize, gpui::Point<gpui::Pixels>)>,
    /// Why the last mode switch or run was refused, shown above the panel.
    query_error: Option<String>,
    /// The Query panel's last result window, its sampled columns, and whether a
    /// run is in flight.
    query_docs: Vec<Document>,
    query_columns: Vec<String>,
    query_loading: bool,
    query_scroll: UniformListScrollHandle,
    scroll: UniformListScrollHandle,
    /// The grid's fraction-mapped scrollbar drag state (sized over the whole
    /// collection, not the resident window).
    scrollbar: ScrollbarState,
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
    pub(crate) list_focus: FocusHandle,
    /// The change stream's state: whether it is running, what it has seen, and
    /// which operations the viewer shows.
    watch: DocWatchState,
    /// The keyboard row cursor as an absolute ordinal (arrow / vim motions), or
    /// `None` before the grid has been touched. Drives the grid highlight and the
    /// Enter-to-inspect target, falling back to the inspected row.
    cursor: Option<usize>,
}

impl CollView {
    /// `page` (`data.page_size`) and `view` (`doc.default_view`) are captured at
    /// open: a live tab keeps the window it was built with, exactly as a SQL
    /// result keeps its page, and re-picking the view per tab is a toolbar click.
    fn new(
        epoch: Epoch,
        db: String,
        coll: String,
        sender: CommandSender,
        page: usize,
        view: DocViewMode,
        cx: &mut Context<AppState>,
    ) -> Self {
        let filter_input = cx.new(|cx| {
            TextInput::new(cx)
                .with_placeholder(DocFilterMode::Fast.placeholder())
                // Up/Down move the field-name suggestion popup instead of leaving
                // the field, the same opt-in the in-cell FK picker uses.
                .emit_nav()
        });
        cx.subscribe(&filter_input, |this, _input, event: &TextInputEvent, cx| {
            let Some(session) = this.doc_active_session() else {
                return;
            };
            match event {
                // Enter takes the highlighted suggestion when the popup is open,
                // and applies the filter otherwise: the popup is transient, the
                // filter is the field's real job.
                TextInputEvent::Submit => {
                    if !this.doc_accept_suggestion(session, cx) {
                        this.doc_apply_filter(session, cx);
                    }
                }
                TextInputEvent::Change => this.doc_filter_changed(session, cx),
                TextInputEvent::Up => this.doc_move_suggestion(session, -1, cx),
                TextInputEvent::Down => this.doc_move_suggestion(session, 1, cx),
                _ => {}
            }
        })
        .detach();
        let query_editor = cx.new(|cx| {
            CodeEditor::new(cx)
                .soft_wrap(false)
                .placeholder(crate::i18n::tr!(
                    "doc.aggregation_pipeline_e_g_group_runs",
                    "Aggregation pipeline, e.g. [ { \"$group\": … } ]. \u{2318}\u{21b5} runs."
                ))
                .edit_menu_labels(crate::editor::edit_menu_labels())
                .a11y_label(crate::i18n::tr!(
                    "doc.mongodb_aggregation_pipeline",
                    "MongoDB aggregation pipeline"
                ))
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
                .edit_menu_labels(crate::editor::edit_menu_labels())
                .a11y_label(crate::i18n::tr!(
                    "doc.mongodb_document_editor",
                    "MongoDB document editor"
                ))
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
            sender,
            window: Rc::new(RefCell::new(DocWindow::new(page))),
            loading: true,
            columns: Vec::new(),
            view_mode: view,
            expanded_rows: BTreeSet::new(),
            inspector: None,
            inspector_doc: None,
            inspector_insert: false,
            inspector_mode: InspectorMode::Form,
            form: None,
            inspector_editor,
            panel: DocPanel::Documents,
            filter_input,
            filter: None,
            filter_mode: DocFilterMode::Fast,
            filter_status: red_core::doc::FastFilter::Empty,
            suggestions: Vec::new(),
            suggestion_ix: 0,
            sort: Vec::new(),
            projection: None,
            sort_doc: None,
            projection_doc: None,
            fields_menu: None,
            schema: None,
            stats: None,
            indexes: None,
            explain: None,
            index_advice: None,
            query_editor,
            query_mode: DocQueryMode::Text,
            stages: Vec::new(),
            preview_stage: None,
            stage_menu: None,
            query_error: None,
            query_docs: Vec::new(),
            query_columns: Vec::new(),
            query_loading: false,
            query_scroll: UniformListScrollHandle::new(),
            scroll: UniformListScrollHandle::new(),
            scrollbar: ScrollbarState::new(),
            json_labels: Vec::new(),
            json_list: new_doc_list_state(0),
            list_state: new_doc_list_state(0),
            list_labels: BTreeMap::new(),
            selection_group: SelectionGroup::default(),
            list_focus: cx.focus_handle(),
            watch: DocWatchState::default(),
            cursor: None,
        }
    }

    /// (Re)build the JSON render mode's per-document selectable blocks for the
    /// resident window and resize its virtualized list. Cheap to hold (each label
    /// only shapes its text when actually painted). The blocks are keyed by
    /// absolute ordinal so the highlight follows a document as the window slides.
    fn rebuild_json_labels(&mut self, cx: &mut Context<AppState>) {
        let group = self.selection_group.clone();
        let (anchor, docs) = {
            let w = self.window.borrow();
            let (anchor, resident) = w.resident();
            (anchor, resident.iter().cloned().collect::<Vec<_>>())
        };
        self.json_labels = docs
            .iter()
            .enumerate()
            .map(|(i, doc)| {
                let text = pretty_extjson(&doc.to_doc_value());
                let ord = (anchor + i) as u64;
                cx.new(|cx| SelectableLabel::new(text, cx).selection_group(group.clone(), ord))
            })
            .collect();
        self.json_list.reset(self.json_labels.len());
    }

    /// The addressing a fetch needs, borrowed from this collection tab's state.
    fn fetch_ctx(&self) -> FetchCtx<'_> {
        FetchCtx {
            epoch: self.epoch,
            db: &self.db,
            coll: &self.coll,
            filter: self.filter.as_deref(),
            projection: self.projection_doc.as_deref(),
            sort: self.sort_doc.as_deref(),
        }
    }

    /// The pipeline to run: the raw editor's text, or the stage list joined back
    /// into one, truncated after [`Self::preview_stage`] with a `$limit` appended
    /// when a stage preview is pinned.
    ///
    /// An empty pipeline is `[]` rather than nothing, so "Run" on a fresh tab
    /// returns the collection instead of a parse error.
    fn pipeline_text(&self, cx: &gpui::App) -> String {
        let text = match self.query_mode {
            DocQueryMode::Text => self.query_editor.read(cx).content().to_string(),
            DocQueryMode::Stages => {
                let mut stages: Vec<String> = self
                    .stages
                    .iter()
                    .map(|s| s.editor.read(cx).content().to_string())
                    .filter(|s| !s.trim().is_empty())
                    .collect();
                if let Some(upto) = self.preview_stage {
                    stages.truncate(upto + 1);
                    stages.push(format!("{{ \"$limit\": {} }}", aggregate::PREVIEW_LIMIT));
                }
                red_core::doc::join_pipeline_stages(&stages)
            }
        };
        if text.trim().is_empty() {
            "[]".to_string()
        } else {
            text
        }
    }

    /// Re-render the cached `sort` / `projection` documents after either changes.
    fn refresh_query_docs(&mut self) {
        self.sort_doc = (!self.sort.is_empty()).then(|| red_core::doc::sort_json(&self.sort));
        self.projection_doc = self
            .projection
            .as_ref()
            .filter(|fields| !fields.is_empty())
            .map(|fields| red_core::doc::projection_json(fields));
    }

    /// Restart the browse after the query shape changed (filter, sort, projection):
    /// forget the resident run, the accumulated columns and anything addressed by
    /// an ordinal that no longer means the same row.
    fn requery(&mut self) {
        self.loading = true;
        self.inspector = None;
        self.inspector_doc = None;
        self.cursor = None;
        self.expanded_rows.clear();
        self.columns.clear();
        // The plan and its index advice belong to the filter that earned them.
        self.explain = None;
        self.index_advice = None;
        self.seed_browse();
    }

    /// Seed (or re-seed) the browse: reset the run and fetch the first window,
    /// which also asks for the collection count. Used on open, on a filter change,
    /// and after a write refreshes the collection.
    fn seed_browse(&self) {
        let ctx = self.fetch_ctx();
        self.window.borrow_mut().seed(&ctx, &self.sender);
    }

    /// The number of resident documents around the current viewport (what the
    /// List/JSON modes lay out; the grid lays out the full `row_count`).
    fn resident_len(&self) -> usize {
        self.window.borrow().resident().1.len()
    }

    /// Scroll the grid so absolute ordinal `ord` is visible: a local scroll when
    /// it is within the current virtual window, otherwise relocate the window onto
    /// it (the far keyboard jump, e.g. `Last`), the way the scrollbar scrub does.
    fn reveal_ord(&self, ord: usize, row_height: f32) {
        let (base, len, total) = {
            let w = self.window.borrow();
            (
                w.window_base(),
                w.total().unwrap_or(0).min(crate::gridwindow::WINDOW),
                w.total(),
            )
        };
        if ord >= base && ord < base + len {
            self.scroll
                .scroll_to_item(ord - base, gpui::ScrollStrategy::Nearest);
        } else if let Some(total) = total {
            let handle = self.window.borrow().window_base_handle();
            window::place_window(&handle, &self.scroll, total, ord, row_height);
        }
    }
}

/// Field-name suggestions offered at once. A popup taller than this stops being a
/// hint and starts being a list to read.
const SUGGESTION_MAX: usize = 8;

/// The field-name token at the end of `text`, or `None` when the tail is not one
/// (a value, a closing brace, whitespace).
///
/// Completion follows the *end* of the line rather than the caret, because
/// [`TextInput`] does not expose one. That covers typing, which is where
/// completion earns its keep, and does nothing at all when the caret is elsewhere,
/// which is the right kind of wrong.
fn field_prefix(text: &str) -> Option<String> {
    // A field name runs back to whatever introduced it: whitespace, or the JSON
    // punctuation a key sits behind.
    let start = text
        .rfind(|c: char| c.is_whitespace() || matches!(c, '{' | ',' | '"' | ':' | '(' | '['))
        .map_or(0, |i| i + 1);
    let tail = &text[start..];
    if tail.is_empty() {
        return None;
    }
    // A dotted path is one token; anything else non-identifier ends it.
    if !tail
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '$'))
    {
        return None;
    }
    Some(tail.to_string())
}

/// Milliseconds since the Unix epoch, for the fast filter's relative dates. The
/// clock lives here rather than in `red-core` so the compiler stays pure.
fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A fresh top-aligned [`ListState`] for a document window, with enough overdraw
/// that a flick keeps painted rows ahead of the viewport.
fn new_doc_list_state(count: usize) -> ListState {
    ListState::new(count, ListAlignment::Top, gpui::px(600.))
}

impl WorkspaceTab for MongoTab {
    fn pane(&self) -> PaneId {
        self.pane
    }
    fn set_pane(&mut self, pane: PaneId) {
        self.pane = pane;
    }
    fn pinned(&self) -> bool {
        self.pinned
    }
    fn set_pinned(&mut self, pinned: bool) {
        self.pinned = pinned;
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
    fn ws_layout(&self) -> &PaneLayout {
        &self.layout
    }
    fn ws_layout_mut(&mut self) -> &mut PaneLayout {
        &mut self.layout
    }
    /// Like Redis, Mongo has no separate pinned strip section, so pinned tabs
    /// sort ahead within their pane's strip.
    fn pins_sort_first(&self) -> bool {
        true
    }
}

impl SplitWorkspace for MongoView {
    fn push_blank_tab(&mut self, pane: PaneId) -> usize {
        let id = self.tab_seq;
        self.tab_seq += 1;
        self.tabs.push(MongoTab {
            id,
            title: "New tab".to_string(),
            state: MongoTabState::Empty,
            pane,
            pinned: false,
        });
        self.tabs.len() - 1
    }
    fn clear_tab_menu(&mut self) {
        self.tab_menu = None;
    }
    fn tab_idx_of(&self, id: u64) -> Option<usize> {
        self.tab_index_by_id(id)
    }
}

impl MongoView {
    /// Build the view for a freshly-connected Mongo session. The first
    /// `DocListDatabases` fires from [`AppState::doc_start_browse`] once the
    /// session is live. Opens with a single blank tab (the shell always shows
    /// something, and ⌘T / the ＋ button open more).
    pub(crate) fn new(
        session: SessionId,
        conn_id: String,
        read_only: bool,
        history_store: Entity<red_config::history::QueryHistory>,
        cx: &mut Context<AppState>,
    ) -> Self {
        let history_panel = {
            let store = history_store.clone();
            cx.new(|cx| crate::dochistory::DocHistoryPanel::new(conn_id, store, cx))
        };
        cx.subscribe(&history_panel, move |this, _panel, event, cx| {
            this.on_doc_history_event(session, event, cx)
        })
        .detach();
        Self {
            session,
            read_only,
            history_store,
            history_panel,
            epoch: crate::result::next_kv_epoch(),
            databases: Vec::new(),
            collections: BTreeMap::new(),
            expanded: BTreeSet::new(),
            error: None,
            pending_write: None,
            pending_pipeline: None,
            tabs: vec![MongoTab {
                id: 0,
                title: "New tab".to_string(),
                state: MongoTabState::Empty,
                pane: PaneId::FIRST,
                pinned: false,
            }],
            tab_seq: 1,
            layout: PaneLayout::new(),
            tab_menu: None,
            actions_menu: None,
            coll_menu: None,
            export: None,
            import: None,
            copy: None,
            index_form: None,
            validator: None,
            tree_focus: cx.focus_handle(),
            tree_filter: {
                let filter = cx.new(|cx| {
                    TextInput::new(cx).with_placeholder(crate::i18n::tr!(
                        "doc.search_collections",
                        "Search collections…"
                    ))
                });
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
                MongoTabState::Empty | MongoTabState::Relations(_) => None,
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
    pub(crate) fn coll_at(&self, idx: usize) -> Option<&CollView> {
        match self.tabs.get(idx).map(|t| &t.state)? {
            MongoTabState::Collection(c) => Some(&**c),
            MongoTabState::Empty | MongoTabState::Relations(_) => None,
        }
    }

    /// The relations diagram shown by the tab at `idx`, if that tab is one.
    pub(crate) fn relations(&self, idx: usize) -> Option<&crate::er::ErView> {
        match self.tabs.get(idx).map(|t| &t.state)? {
            MongoTabState::Relations(r) => r.er.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn relations_mut(&mut self, idx: usize) -> Option<&mut crate::er::ErView> {
        match self.tabs.get_mut(idx).map(|t| &mut t.state)? {
            MongoTabState::Relations(r) => r.er.as_mut(),
            _ => None,
        }
    }

    /// The relations diagram *state* shown by the tab at `idx` (the canvas plus
    /// the evidence), where [`relations`](Self::relations) hands back only the
    /// canvas the shared ER renderer needs.
    pub(crate) fn relations_at(&self, idx: usize) -> Option<&RelationsView> {
        match self.tabs.get(idx).map(|t| &t.state)? {
            MongoTabState::Relations(r) => Some(&**r),
            _ => None,
        }
    }

    /// The relations tab that owns `epoch`, for routing its inference reply.
    fn relations_by_epoch_mut(&mut self, epoch: Epoch) -> Option<&mut RelationsView> {
        self.tabs.iter_mut().find_map(|t| match &mut t.state {
            MongoTabState::Relations(r) if r.epoch == epoch => Some(&mut **r),
            _ => None,
        })
    }

    /// The focused tab's collection (UI actions target the visible tab).
    fn focused_coll(&self) -> Option<&CollView> {
        self.coll_at(self.focused_tab_index()?)
    }

    fn focused_coll_mut(&mut self) -> Option<&mut CollView> {
        let i = self.focused_tab_index()?;
        match self.tabs.get_mut(i).map(|t| &mut t.state)? {
            MongoTabState::Collection(c) => Some(&mut **c),
            MongoTabState::Empty | MongoTabState::Relations(_) => None,
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

    /// [`coll_by_epoch_mut`](Self::coll_by_epoch_mut) by shared reference.
    fn coll_by_epoch(&self, epoch: Epoch) -> Option<&CollView> {
        self.tabs.iter().find_map(|t| match &t.state {
            MongoTabState::Collection(c) if c.epoch == epoch => Some(&**c),
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
        reason = "the args mirror the DocRunReady event's fields 1:1, like the on_kv_* handlers"
    )]
    pub(crate) fn on_doc_run(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        db: String,
        coll: String,
        seek: DocSeek,
        docs: Vec<Document>,
        seq: u64,
        total: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let max_columns = self.settings.doc.max_columns;
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        let Some(current) = view.coll_by_epoch_mut(epoch) else {
            return;
        };
        // Drop a late window for a collection the tab has since been repointed at.
        if current.db != db || current.coll != coll {
            return;
        }
        // Land the window in the run; a stale/mismatched reply is dropped inside.
        current
            .window
            .borrow_mut()
            .apply(seek, docs, seq, total.map(|t| t as usize));
        current.loading = current.window.borrow().total().is_none();

        // Accumulate columns from the resident documents (so they don't flicker as
        // the window scrolls onto documents of other shapes), and resize the
        // List/JSON virtualized lists to the resident count.
        let resident: Vec<std::rc::Rc<Document>> = current
            .window
            .borrow()
            .resident()
            .1
            .iter()
            .cloned()
            .collect();
        merge_columns(
            &mut current.columns,
            resident.iter().map(std::rc::Rc::as_ref),
            max_columns,
        );
        let n = resident.len();
        current.list_labels.clear();
        current.json_labels.clear();
        current.list_state.reset(n);
        current.json_list.reset(n);

        // Clear an inspector/cursor left past the (now known) end.
        if let Some(total) = current.window.borrow().total() {
            if current.inspector.is_some_and(|s| s >= total) {
                current.inspector = None;
                current.inspector_doc = None;
            }
            if current.cursor.is_some_and(|c| c >= total) {
                current.cursor = None;
            }
        }
        if matches!(current.view_mode, DocViewMode::Json) {
            current.rebuild_json_labels(cx);
        }
        cx.notify();
    }

    /// A keyset window fetch failed: free the run's in-flight slot so a later
    /// scroll can retry. The error message rode a separate `DocError`.
    pub(crate) fn on_doc_run_failed(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        seq: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(current) = self
            .conn_mut(session)
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.coll_by_epoch_mut(epoch))
        {
            current.window.borrow_mut().run_failed(seq);
            current.loading = false;
            cx.notify();
        }
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

    /// Apply the filter box's current text: set the filter (compiling it first in
    /// Fast mode), or clear it when the box is empty, and re-seed the browse from
    /// the first window. The grid's continuous scroll takes over from there.
    ///
    /// A filter that does not compile is not applied: the inline hint already says
    /// why, and replacing the visible result with an error would lose the rows the
    /// user was looking at.
    fn doc_apply_filter(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let now = unix_millis();
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
        current.suggestions.clear();
        let filter = match (current.filter_mode, text.is_empty()) {
            (_, true) => None,
            (DocFilterMode::Json, false) => Some(text),
            (DocFilterMode::Fast, false) => {
                match red_core::doc::compile_fast_filter(&text, now) {
                    red_core::doc::FastFilter::Ready(json) => Some(json),
                    red_core::doc::FastFilter::Empty => None,
                    // Nothing to apply yet; the hint is already on screen.
                    red_core::doc::FastFilter::Incomplete
                    | red_core::doc::FastFilter::Invalid(_) => {
                        cx.notify();
                        return;
                    }
                }
            }
        };
        current.filter = filter.clone();
        current.panel = DocPanel::Documents;
        current.requery();
        // Record what actually ran (the compiled document), not the shorthand
        // that produced it: the log is replayed into the JSON box, and a fast
        // filter reads as nonsense there.
        if let Some(filter) = filter {
            self.doc_record_history(session, &filter, cx);
        }
        cx.notify();
    }

    /// Switch how the filter box reads its text. The box is cleared rather than
    /// reinterpreted: a JSON document is not a fast-filter line and vice versa, and
    /// carrying the text across would produce a confident, wrong filter.
    fn doc_set_filter_mode(
        &mut self,
        session: SessionId,
        mode: DocFilterMode,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        if current.filter_mode == mode {
            return;
        }
        current.filter_mode = mode;
        current.filter_status = red_core::doc::FastFilter::Empty;
        current.suggestions.clear();
        let input = current.filter_input.clone();
        input.update(cx, |input, cx| {
            input.set_content("", cx);
            input.set_placeholder(mode.placeholder(), cx);
        });
        cx.notify();
    }

    /// Recompute the inline hint and the field-name suggestions as the box is typed.
    fn doc_filter_changed(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let now = unix_millis();
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        let text = current.filter_input.read(cx).content().to_string();
        current.filter_status = match current.filter_mode {
            DocFilterMode::Fast => red_core::doc::compile_fast_filter(&text, now),
            // The JSON box's own errors surface from the driver's parser on Run;
            // guessing at half-typed JSON here would only cry wolf.
            DocFilterMode::Json => red_core::doc::FastFilter::Empty,
        };
        let prefix = field_prefix(&text);
        current.suggestions = match (&prefix, &current.schema) {
            (Some(prefix), Some(schema)) => schema
                .fields
                .iter()
                .map(|f| f.path.as_str())
                .filter(|path| path.len() > prefix.len() && path.starts_with(prefix.as_str()))
                .take(SUGGESTION_MAX)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        current.suggestion_ix = 0;
        cx.notify();
    }

    /// Move the suggestion highlight by `delta`, wrapping at both ends.
    fn doc_move_suggestion(&mut self, session: SessionId, delta: i32, cx: &mut Context<Self>) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        let n = current.suggestions.len();
        if n == 0 {
            return;
        }
        let next = current.suggestion_ix as i32 + delta;
        current.suggestion_ix = next.rem_euclid(n as i32) as usize;
        cx.notify();
    }

    /// Replace the token being typed with the highlighted suggestion. Returns
    /// whether a suggestion was taken, so Enter can fall through to applying the
    /// filter when the popup is closed.
    fn doc_accept_suggestion(&mut self, session: SessionId, cx: &mut Context<Self>) -> bool {
        self.doc_take_suggestion(session, None, cx)
    }

    /// Accept `which` (or the highlighted one), rewriting the trailing field token.
    fn doc_take_suggestion(
        &mut self,
        session: SessionId,
        which: Option<usize>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return false;
        };
        let ix = which.unwrap_or(current.suggestion_ix);
        let Some(path) = current.suggestions.get(ix).cloned() else {
            return false;
        };
        let input = current.filter_input.clone();
        let text = input.read(cx).content().to_string();
        let Some(prefix) = field_prefix(&text) else {
            return false;
        };
        let head = &text[..text.len() - prefix.len()];
        let replaced = format!("{head}{path}");
        input.update(cx, |input, cx| input.set_content(replaced, cx));
        current.suggestions.clear();
        // The accepted path may complete a term, so the hint is recomputed.
        self.doc_filter_changed(session, cx);
        true
    }

    /// Dismiss the suggestion popup. The popup belongs to the filter box, so it
    /// closes as soon as attention moves elsewhere (another panel, another pane).
    fn doc_dismiss_suggestions(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(current) = self.doc_focused_coll_mut(session)
            && !current.suggestions.is_empty()
        {
            current.suggestions.clear();
            cx.notify();
        }
    }

    /// Toggle the browse sort on `field`, cycling ascending -> descending -> off.
    /// `additive` (a shift-click) keeps the existing keys and appends this one, so
    /// a multi-key sort is built by shift-clicking headers in priority order.
    fn doc_toggle_sort(
        &mut self,
        session: SessionId,
        field: String,
        additive: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        let existing = current.sort.iter().position(|(f, _)| *f == field);
        match existing {
            // Ascending -> descending -> gone, so a third click on a header is the
            // way back to the collection's natural order.
            Some(i) if current.sort[i].1 => current.sort[i].1 = false,
            Some(i) => {
                current.sort.remove(i);
            }
            None => {
                if !additive {
                    current.sort.clear();
                }
                current.sort.push((field, true));
            }
        }
        current.refresh_query_docs();
        current.requery();
        cx.notify();
    }

    fn doc_clear_sort(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        current.sort.clear();
        current.refresh_query_docs();
        current.requery();
        cx.notify();
    }

    /// Add or remove one field from the projection. The first toggle starts from
    /// the sampled schema's full field list, so unticking one field keeps the rest
    /// rather than projecting down to a single column.
    fn doc_toggle_projection_field(
        &mut self,
        session: SessionId,
        field: String,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        let all: Vec<String> = current
            .schema
            .as_ref()
            .map(|s| {
                s.fields
                    .iter()
                    .map(|f| f.path.clone())
                    .filter(|p| p != "_id")
                    .collect()
            })
            .unwrap_or_default();
        let fields = current.projection.get_or_insert(all);
        match fields.iter().position(|f| *f == field) {
            Some(i) => {
                fields.remove(i);
            }
            None => fields.push(field),
        }
        // An empty projection is no projection: `{}` would return `_id` alone,
        // which reads as a bug rather than as a choice.
        if fields.is_empty() {
            current.projection = None;
        }
        current.refresh_query_docs();
        current.requery();
        cx.notify();
    }

    fn doc_clear_projection(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        current.projection = None;
        current.refresh_query_docs();
        current.requery();
        cx.notify();
    }

    /// Open / close the toolbar's "Fields" projection dropdown. Opening it fetches
    /// the inferred schema when the tab has not needed it yet: the field list is
    /// the menu.
    fn doc_open_fields_menu(
        &mut self,
        session: SessionId,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        current.fields_menu = Some(pos);
        let needs_schema = current.schema.is_none();
        let (epoch, db, coll) = (current.epoch, current.db.clone(), current.coll.clone());
        if needs_schema {
            self.service
                .send_to(session, Command::DocInferSchema { epoch, db, coll });
        }
        cx.notify();
    }

    fn doc_close_fields_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(current) = self.doc_focused_coll_mut(session) {
            current.fields_menu = None;
        }
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
        current.suggestions.clear();
        let (epoch, db, coll) = (current.epoch, current.db.clone(), current.coll.clone());
        let filter = current.filter.clone();
        let needs_indexes = current.indexes.is_none();
        let needs_schema = current.schema.is_none();
        let needs_stats = current.stats.is_none();
        let needs_advice = current.index_advice.is_none() && filter.is_some();
        match panel {
            DocPanel::Schema => {
                if needs_schema {
                    self.service.send_to(
                        session,
                        Command::DocInferSchema {
                            epoch,
                            db: db.clone(),
                            coll: coll.clone(),
                        },
                    );
                }
                if needs_stats {
                    self.service
                        .send_to(session, Command::DocCollStats { epoch, db, coll });
                }
            }
            DocPanel::Indexes => {
                if needs_indexes {
                    self.service.send_to(
                        session,
                        Command::DocListIndexes {
                            epoch,
                            db: db.clone(),
                            coll: coll.clone(),
                        },
                    );
                }
                // Explain the applied filter so the panel can offer the index it
                // wants. Only worth asking when there *is* a filter: an unfiltered
                // browse scans by definition and needs no advice about it.
                if needs_advice {
                    self.service.send_to(
                        session,
                        Command::DocExplain {
                            epoch,
                            db,
                            coll,
                            filter,
                        },
                    );
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Toggle a document's expansion in List mode. Expanding builds the row's
    /// selectable field block (a `SelectableLabel`); collapsing drops it.
    /// Expand/collapse the List-mode card at absolute ordinal `ord`. Its expansion
    /// and selectable block are keyed by ordinal (so they survive the window
    /// sliding); the virtualized list re-measures the resident-local row.
    fn doc_toggle_row(&mut self, session: SessionId, ord: usize, cx: &mut Context<Self>) {
        let cell_cap = self.settings.data.max_cell_chars;
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
            if current.expanded_rows.remove(&ord) {
                current.list_labels.remove(&ord);
                None
            } else {
                current.expanded_rows.insert(ord);
                current.window.borrow().doc_at(ord).map(|doc| {
                    (
                        doc_field_text(doc, cell_cap),
                        current.selection_group.clone(),
                    )
                })
            }
        };
        // Phase B: build the label entity (needs `cx`), then store it back.
        let label = build.map(|(text, group)| {
            cx.new(|cx| SelectableLabel::new(text, cx).selection_group(group, ord as u64))
        });
        if let Some(current) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.focused_coll_mut())
        {
            if let Some(label) = label {
                current.list_labels.insert(ord, label);
            }
            // The toggled card changed height; re-measure just that resident row so
            // the virtualized list lays the rest out correctly.
            let local = ord.saturating_sub(current.window.borrow().anchor());
            current.list_state.remeasure_items(local..local + 1);
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
        // resident window (cleared on every window change).
        if matches!(mode, DocViewMode::Json) && current.json_labels.len() != current.resident_len()
        {
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

    /// `DocStatsReady`: land a collection's storage numbers on the tab that asked.
    pub(crate) fn on_doc_stats(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        db: String,
        coll: String,
        stats: red_core::doc::CollStats,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        if let Some(current) = view.coll_by_epoch_mut(epoch)
            && current.db == db
            && current.coll == coll
        {
            current.stats = Some(stats);
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
        let pipeline = current.pipeline_text(cx);
        current.query_loading = true;
        let (epoch, db, coll) = (current.epoch, current.db.clone(), current.coll.clone());
        self.service.send_to(
            session,
            Command::DocAggregate {
                epoch,
                db,
                coll,
                pipeline: pipeline.clone(),
                confirmed: false,
            },
        );
        // An empty pipeline is the panel's default, not a query worth keeping.
        if pipeline.trim() != "[]" {
            self.doc_record_history(session, &pipeline, cx);
        }
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
        let max_columns = self.settings.doc.max_columns;
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        if let Some(current) = view.coll_by_epoch_mut(epoch)
            && current.db == db
            && current.coll == coll
        {
            current.query_columns = sample_columns(&docs, max_columns);
            current.query_docs = docs;
            current.query_loading = false;
            cx.notify();
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the args mirror the DocPlanReady event's fields 1:1, like the other on_doc_* handlers"
    )]
    pub(crate) fn on_doc_plan(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        db: String,
        coll: String,
        plan: DocPlan,
        advice: Vec<String>,
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
            current.index_advice = Some(advice);
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
        let row_height = f32::from(self.settings.data.density.row_height());
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        // The cursor ranges over all documents (absolute ordinals), not just the
        // resident window; moving onto an off-window row relocates the window.
        let Some(total) = current.window.borrow().total().filter(|&t| t > 0) else {
            return;
        };
        let last = total - 1;
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
        current.reveal_ord(next, row_height);
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
        let Some(total) = current.window.borrow().total().filter(|&t| t > 0) else {
            return true;
        };
        let ord = current.cursor.unwrap_or(0).min(total - 1);
        self.doc_toggle_inspector(session, ord, cx);
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
        // Leaving the filter box takes its suggestion popup with it.
        if pane != Pane::Editor
            && let Some(session) = self.doc_active_session()
        {
            self.doc_dismiss_suggestions(session, cx);
        }
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Open (or, on the same ordinal, close) the inspector on a document, loading
    /// its extended JSON into the editor when it opens. `ord` is an absolute
    /// ordinal; the document is snapshotted so the inspector survives the row
    /// scrolling out of the resident window.
    fn doc_toggle_inspector(&mut self, session: SessionId, ord: usize, cx: &mut Context<Self>) {
        // Phase A: flip the selection and, when opening, clone the target document
        // so the form/editor can be built without holding the borrow.
        let doc = {
            let Some(current) = self.doc_focused_coll_mut(session) else {
                return;
            };
            if current.inspector == Some(ord) && !current.inspector_insert {
                current.inspector = None;
                current.inspector_doc = None;
                current.form = None;
                None
            } else {
                let doc = current.window.borrow().doc_at(ord).cloned();
                current.inspector = Some(ord);
                current.inspector_insert = false;
                current.inspector_doc = doc.clone();
                doc
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
            current.inspector_doc = None;
            current.inspector_insert = true;
            current.form = Some(form);
            current.inspector_editor.clone()
        };
        editor.update(cx, |ed, cx| ed.set_content("{\n  \n}", cx));
        cx.notify();
    }

    /// Clone the document at absolute ordinal `ord` into the compose editor (drops
    /// `_id` so the insert mints a fresh one), the Compass-style "insert a copy"
    /// affordance.
    fn doc_clone_document(&mut self, session: SessionId, ord: usize, cx: &mut Context<Self>) {
        let doc = self
            .doc_focused_coll_mut(session)
            .and_then(|current| current.window.borrow().doc_at(ord).cloned());
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
            current.inspector_doc = None;
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
            let id = current.inspector_doc.as_ref().map(|d| d.id.clone());
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

    /// Queue a delete of the document at absolute ordinal `ord` behind the confirm
    /// modal.
    fn doc_delete_row(&mut self, session: SessionId, ord: usize, cx: &mut Context<Self>) {
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
            let Some(doc) = current.window.borrow().doc_at(ord).cloned() else {
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
        if !self.confirm_policy().confirms_delete() {
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

    /// Propose dropping a collection named from the tree, which need not be open
    /// in a tab. Runs under the catalog epoch for exactly that reason; the
    /// confirm dance and the `DocWriteDone` refresh are the drop-current path's.
    fn doc_drop_collection(
        &mut self,
        session: SessionId,
        db: String,
        coll: String,
        cx: &mut Context<Self>,
    ) {
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
            // A tab already open on the namespace owns the refresh that follows,
            // so the write rides its epoch when there is one.
            let epoch = view
                .tabs
                .iter()
                .find_map(|t| match &t.state {
                    MongoTabState::Collection(c) if c.db == db && c.coll == coll => Some(c.epoch),
                    _ => None,
                })
                .unwrap_or(view.epoch);
            Command::DocApplyWrite {
                epoch,
                write: DocWrite::DropCollection { db, coll },
                confirmed: false,
            }
        };
        self.service.send_to(session, cmd);
        cx.notify();
    }

    /// Open (or focus) the relations diagram for a database and start the
    /// inference behind it.
    ///
    /// One tab per database: reopening focuses the existing diagram rather than
    /// re-running a sampling pass whose answer is already on screen.
    pub(crate) fn doc_open_relations(
        &mut self,
        session: SessionId,
        db: String,
        cx: &mut Context<Self>,
    ) {
        self.doc_close_coll_menu(session, cx);
        let existing = self
            .conn_for(Some(session))
            .and_then(|a| a.doc_view.as_ref())
            .and_then(|v| {
                v.tabs.iter().position(|t| match &t.state {
                    MongoTabState::Relations(r) => r.db == db,
                    _ => false,
                })
            });
        if let Some(idx) = existing {
            self.doc_activate_tab(session, idx, cx);
            return;
        }
        let epoch = crate::result::next_kv_epoch();
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let pane = view.focused_pane();
        let id = view.tab_seq;
        view.tab_seq += 1;
        view.tabs.push(MongoTab {
            id,
            title: format!("{db} relations"),
            state: MongoTabState::Relations(Box::new(RelationsView {
                epoch,
                db: db.clone(),
                loading: true,
                er: None,
                references: Vec::new(),
                sampled: 0,
                error: None,
            })),
            pane,
            pinned: false,
        });
        let idx = view.tabs.len() - 1;
        view.set_pane_active(pane, idx);
        view.scroll_tab_into_view(idx);
        self.service
            .send_to(session, Command::DocReferenceMap { epoch, db });
        cx.notify();
    }

    /// `DocReferencesReady`: build the diagram from the inferred graph.
    pub(crate) fn on_doc_references(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        db: String,
        collections: Vec<(String, usize)>,
        references: Vec<red_core::doc::DocReference>,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        let Some(relations) = view.relations_by_epoch_mut(epoch) else {
            return;
        };
        // Only the references that actually resolved become edges. A weak one is
        // still listed in the footer count, because "we looked and it did not hold"
        // is a finding, but drawing it would assert a relationship that is not there.
        let edges: Vec<(String, String, String)> = references
            .iter()
            .filter(|r| r.is_strong())
            .map(|r| (r.from_coll.clone(), r.field.clone(), r.to_coll.clone()))
            .collect();
        relations.sampled = collections.len();
        relations.er = Some(crate::er::ErView::from_references(db, &collections, &edges));
        relations.references = references;
        relations.loading = false;
        relations.error = None;
        cx.notify();
    }

    /// Handle what the History dock asks the shell to do: seed a past filter or
    /// pipeline back into a tab, clear the log, or hide the dock.
    fn on_doc_history_event(
        &mut self,
        session: SessionId,
        event: &crate::dochistory::DocHistoryPanelEvent,
        cx: &mut Context<Self>,
    ) {
        use crate::dochistory::DocHistoryPanelEvent as E;
        match event {
            E::SeedFilter { namespace, filter } => {
                self.doc_point_at(session, namespace.as_deref(), cx);
                let Some(current) = self.doc_focused_coll_mut(session) else {
                    return;
                };
                // A recorded filter is a filter document, so the box must be in
                // the mode that reads one; seeding JSON into the fast box would
                // produce a confident, wrong query.
                current.filter_mode = DocFilterMode::Json;
                current.panel = DocPanel::Documents;
                let input = current.filter_input.clone();
                let text = filter.clone();
                input.update(cx, |input, cx| {
                    input.set_placeholder(DocFilterMode::Json.placeholder(), cx);
                    input.set_content(text, cx);
                });
                self.doc_apply_filter(session, cx);
            }
            E::SeedPipeline {
                namespace,
                pipeline,
            } => {
                self.doc_point_at(session, namespace.as_deref(), cx);
                let Some(current) = self.doc_focused_coll_mut(session) else {
                    return;
                };
                // Seeding replaces the pipeline, so the stage list built from the
                // old one is gone; the text mode holds what just arrived.
                current.panel = DocPanel::Query;
                current.query_mode = DocQueryMode::Text;
                current.stages.clear();
                current.preview_stage = None;
                let editor = current.query_editor.clone();
                let text = pipeline.clone();
                editor.update(cx, |editor, cx| editor.set_content(text, cx));
                cx.notify();
            }
            E::ClearAll => {
                let conn_id = self
                    .conn_for(Some(session))
                    .map(|a| a.conn_id.clone())
                    .unwrap_or_default();
                if let Some(view) = self
                    .conn_for(Some(session))
                    .and_then(|a| a.doc_view.as_ref())
                {
                    let store = view.history_store.clone();
                    store.update(cx, |store, _| store.clear_conn(&conn_id));
                }
                cx.notify();
            }
            E::Close => {
                if let Phase::Connected(active) = &mut self.phase {
                    active.history_open = false;
                }
                self.refocus_root = true;
                cx.notify();
            }
        }
    }

    /// Point the focused tab at `namespace` (`db.collection`) when it names one
    /// the tab is not already on, so a seeded history entry lands on the
    /// collection it was recorded against rather than whatever is open.
    fn doc_point_at(
        &mut self,
        session: SessionId,
        namespace: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some((db, coll)) = namespace.and_then(|ns| ns.split_once('.')) else {
            return;
        };
        let already = self
            .conn_for(Some(session))
            .and_then(|a| a.doc_view.as_ref())
            .and_then(|v| v.focused_coll())
            .is_some_and(|c| c.db == db && c.coll == coll);
        if !already {
            self.doc_open_collection(session, db.to_string(), coll.to_string(), false, cx);
        }
    }

    /// The query a save should capture on a MongoDB connection: the pipeline when
    /// the Query panel is showing, else the applied filter, with the collection it
    /// belongs to.
    pub(crate) fn doc_savable_query(&self, cx: &gpui::App) -> Option<(String, Option<String>)> {
        let current = self.doc_view().and_then(|v| v.focused_coll())?;
        let namespace = Some(format!("{}.{}", current.db, current.coll));
        match current.panel {
            DocPanel::Query => Some((current.pipeline_text(cx), namespace)),
            // A filter is what the Documents panel *ran*, not what is half-typed
            // in the box: saving a draft nobody has applied would be a surprise.
            _ => current.filter.clone().map(|f| (f, namespace)),
        }
    }

    /// Seed a saved query into the shell: a pipeline into the Query panel, a
    /// filter into the Documents filter box, at the collection it names.
    pub(crate) fn doc_open_saved_query(
        &mut self,
        session: SessionId,
        query: &red_config::queries::SavedQuery,
        cx: &mut Context<Self>,
    ) {
        let text = query.sql.clone();
        let namespace = query.namespace.clone();
        if crate::dochistory::is_pipeline(&text) {
            self.on_doc_history_event(
                session,
                &crate::dochistory::DocHistoryPanelEvent::SeedPipeline {
                    namespace,
                    pipeline: text,
                },
                cx,
            );
        } else {
            self.on_doc_history_event(
                session,
                &crate::dochistory::DocHistoryPanelEvent::SeedFilter {
                    namespace,
                    filter: text,
                },
                cx,
            );
        }
    }

    /// The MongoDB view of the foreground connection.
    fn doc_view(&self) -> Option<&MongoView> {
        match &self.phase {
            Phase::Connected(a) => a.doc_view.as_ref(),
            _ => None,
        }
    }

    /// Record a run in the shared query log, tagged with the namespace it ran
    /// against. The user's own runs only, like the SQL path: history is grounding
    /// precisely because it is human-authored.
    fn doc_record_history(&mut self, session: SessionId, text: &str, cx: &mut Context<Self>) {
        if text.trim().is_empty() {
            return;
        }
        let Some(active) = self.conn_for(Some(session)) else {
            return;
        };
        let conn_id = active.conn_id.clone();
        let Some(view) = active.doc_view.as_ref() else {
            return;
        };
        let namespace = view.focused_coll().map(|c| format!("{}.{}", c.db, c.coll));
        let store = view.history_store.clone();
        let text = text.to_string();
        store.update(cx, |store, _| {
            store.record_scoped(&conn_id, &text, namespace)
        });
    }

    /// Re-fetch one database's collection list (the tree's context-menu refresh).
    fn doc_reload_collections(&mut self, session: SessionId, db: String, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_ref())
        else {
            return;
        };
        let epoch = view.epoch;
        self.service
            .send_to(session, Command::DocListCollections { epoch, db });
        cx.notify();
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
            return;
        }
        // The other thing this modal can be holding: a `$out`/`$merge` pipeline.
        let pipeline = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.pending_pipeline.take());
        if let Some((epoch, pipeline, _)) = pipeline {
            let target = self
                .conn_for(Some(session))
                .and_then(|a| a.doc_view.as_ref())
                .and_then(|v| v.coll_by_epoch(epoch))
                .map(|c| (c.db.clone(), c.coll.clone()));
            if let Some((db, coll)) = target {
                self.service.send_to(
                    session,
                    Command::DocAggregate {
                        epoch,
                        db,
                        coll,
                        pipeline,
                        confirmed: true,
                    },
                );
            }
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
            // The Query panel's spinner was armed by the run that raised this; a
            // declined confirm leaves it spinning otherwise.
            if let Some((epoch, _, _)) = view.pending_pipeline.take()
                && let Some(c) = view.coll_by_epoch_mut(epoch)
            {
                c.query_loading = false;
            }
            cx.notify();
        }
    }

    /// A pipeline write stage needs the destructive confirm before it runs.
    pub(crate) fn on_doc_pipeline_confirm(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        pipeline: String,
        prompt: String,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        view.pending_pipeline = Some((epoch, pipeline, prompt));
        if let Some(c) = view.coll_by_epoch_mut(epoch) {
            c.query_loading = false;
        }
        cx.notify();
    }

    /// Close the inspector (edit or compose).
    fn doc_close_inspector(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(current) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.focused_coll_mut())
        {
            current.inspector = None;
            current.inspector_doc = None;
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
        // Close the inspector on the writing tab, clear any pending confirm, and
        // re-seed the browse (a write shifts ordinals, so the resident window and
        // its expansions are re-derived from a fresh first window).
        let mut reload_indexes = None;
        let mut catalog_refresh = None;
        {
            let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
                return;
            };
            view.pending_write = None;
            let catalog_epoch = view.epoch;
            if let Some(c) = view.coll_by_epoch_mut(epoch) {
                catalog_refresh = Some((catalog_epoch, c.db.clone()));
                c.inspector = None;
                c.inspector_doc = None;
                c.inspector_insert = false;
                c.cursor = None;
                c.expanded_rows.clear();
                c.loading = true;
                c.seed_browse();
                // An index write changes the Indexes panel, not the documents, and
                // the panel only fetches on its first open; re-fetch so a created
                // or dropped index shows without reopening the tab.
                if c.indexes.is_some() {
                    reload_indexes = Some((c.epoch, c.db.clone(), c.coll.clone()));
                }
            }
        }
        if let Some((epoch, db, coll)) = reload_indexes
            && let Some(session) = session
        {
            self.service
                .send_to(session, Command::DocListIndexes { epoch, db, coll });
        }
        // Re-read the catalog too: a write can change a collection's count, its
        // validator, or whether it exists at all, and `listCollections` is cheap
        // metadata. Unconditional rather than guessed at from the summary line,
        // which is prose meant for a human.
        if let Some((session, epoch, db)) =
            session.zip(catalog_refresh).map(|(s, (e, d))| (s, e, d))
        {
            self.service
                .send_to(session, Command::DocListCollections { epoch, db });
        }
        self.notify(ToastVariant::Success, summary, cx);
    }
}

// --- free helpers ------------------------------------------------------------

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
fn push_field_data(
    key: &str,
    value: &DocValue,
    depth: usize,
    cell_cap: usize,
    out: &mut Vec<FieldRow>,
) {
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
                push_field_data(k, v, depth + 1, cell_cap, out);
            }
        }
        DocValue::Array(items) if !items.is_empty() => {
            out.push(row(format!("[ {} ]", items.len()), depth));
            for (i, item) in items.iter().enumerate() {
                push_field_data(&i.to_string(), item, depth + 1, cell_cap, out);
            }
        }
        other => out.push(row(other.to_cell(cell_cap).to_string(), depth)),
    }
}

/// The selectable "key: value" block for one document's List-mode card: `_id`
/// first, then each field, nested objects/arrays indented two spaces per level.
fn doc_field_text(doc: &Document, cell_cap: usize) -> String {
    let mut fields = Vec::new();
    push_field_data("_id", &doc.id, 0, cell_cap, &mut fields);
    for (k, v) in &doc.fields {
        push_field_data(k, v, 0, cell_cap, &mut fields);
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

fn sample_columns(docs: &[Document], max_columns: usize) -> Vec<String> {
    let mut cols = vec!["_id".to_string()];
    merge_columns(&mut cols, docs.iter(), max_columns);
    cols
}

/// Fold the top-level field names of `docs` into `cols` (first-seen order, `_id`
/// always leading), capped at `max_columns` (`doc.max_columns`). Additive, so the grid's columns
/// accumulate as the window scrolls onto documents of other shapes rather than
/// flickering when a field is absent from the current resident set. Seeds `_id`
/// on an empty `cols`.
fn merge_columns<'a>(
    cols: &mut Vec<String>,
    docs: impl IntoIterator<Item = &'a Document>,
    max_columns: usize,
) {
    if cols.is_empty() {
        cols.push("_id".to_string());
    }
    for doc in docs {
        for (name, _) in &doc.fields {
            if cols.len() >= max_columns {
                return;
            }
            if !cols.iter().any(|c| c == name) {
                cols.push(name.clone());
            }
        }
    }
}

/// The display string for one grid cell: the document's value for `col`, or
/// `None` when the field is absent (a schemaless gap). Nested values render as
/// extended JSON capped at `cell_cap` (`data.max_cell_chars`, the same fat-cell
/// rail the SQL grid runs under); scalars map through [`DocValue::to_cell`].
fn cell_string(doc: &Document, col: &str, cell_cap: usize) -> Option<String> {
    let value = if col == "_id" {
        Some(&doc.id)
    } else {
        doc.fields.iter().find(|(k, _)| k == col).map(|(_, v)| v)
    };
    value.map(|v| v.to_cell(cell_cap).to_string())
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
