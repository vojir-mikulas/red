//! The MongoDB browser (`MongoView`), the document-store shell parallel to the
//! Redis `kvbrowse::RedisView`. D0 read-only surface: a `database -> collection`
//! tree on the left, the selected collection's documents in a sampled-column
//! grid, and a raw-document inspector on the right. It speaks the `Doc*`
//! `Command`/`Event` pair (see `red-service`'s protocol) and never touches the
//! `DocDriver` directly, the same UI/driver separation the SQL and Redis shells
//! keep. See `docs/plans/todo/doc-driver.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use flint::Theme;
use flint::prelude::*;
use gpui::{
    Context, FocusHandle, SharedString, UniformListScrollHandle, WeakEntity, Window, div,
    prelude::*, px,
};
use red_core::doc::{CollKind, CollectionInfo, DbInfo, DocValue, Document};
use red_service::{Command, Epoch, SessionId};

use crate::app::{ActiveConn, AppState, Phase};

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
    scroll: UniformListScrollHandle,
    list_focus: FocusHandle,
}

impl CollView {
    fn new(db: String, coll: String, cx: &mut Context<AppState>) -> Self {
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
            scroll: UniformListScrollHandle::new(),
            list_focus: cx.focus_handle(),
        }
    }
}

impl MongoView {
    /// Build the view for a freshly-connected Mongo session. The first
    /// `DocListDatabases` fires from [`AppState::doc_start_browse`] once the
    /// session is live, not here (this only needs `cx` for future focus state).
    pub(crate) fn new(session: SessionId, _cx: &mut Context<AppState>) -> Self {
        Self {
            session,
            epoch: crate::result::next_kv_epoch(),
            databases: Vec::new(),
            collections: BTreeMap::new(),
            expanded: BTreeSet::new(),
            current: None,
            error: None,
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
        let (db, coll) = (current.db.clone(), current.coll.clone());
        self.service.send_to(
            session,
            Command::DocFetchPage {
                epoch,
                db,
                coll,
                skip: next_skip,
            },
        );
        cx.notify();
    }

    /// Open (or, on the same row, close) the inspector on a document row.
    fn doc_toggle_inspector(&mut self, session: SessionId, row: usize, cx: &mut Context<Self>) {
        let Some(current) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.current.as_mut())
        else {
            return;
        };
        current.inspector = if current.inspector == Some(row) {
            None
        } else {
            Some(row)
        };
        cx.notify();
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

        let body = active
            .doc_view
            .as_ref()
            .map(|v| self.render_doc_body(v, &theme, &view))
            .unwrap_or_else(|| div().into_any_element());

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_app)
            .text_color(theme.text)
            .child(topbar)
            .child(div().flex().flex_1().min_h(px(0.)).child(body))
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

    /// The main area: a toolbar (collection name + pager) over the sampled-column
    /// document grid, with the inspector docked right when a row is open.
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

        let toolbar = self.render_doc_toolbar(v.session, current, theme, view);
        let grid = self.render_doc_grid(v.session, current, theme, view);

        let content = if let Some(sel) = current.inspector.and_then(|i| current.docs.get(i)) {
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
                        .child(self.render_doc_inspector(v.session, sel, theme, view)),
                )
                .into_any_element()
        } else {
            div().flex_1().min_h(px(0.)).child(grid).into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(toolbar)
            .child(content)
            .into_any_element()
    }

    fn render_doc_toolbar(
        &self,
        session: SessionId,
        current: &CollView,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
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

        let prev_view = view.clone();
        let next_view = view.clone();
        let prev = Button::new("doc-prev", "Prev")
            .size(ButtonSize::Sm)
            .variant(ButtonVariant::Secondary)
            .disabled(current.skip == 0 || current.loading)
            .on_click(move |_, _, cx| {
                prev_view
                    .update(cx, |this, cx| this.doc_page(session, false, cx))
                    .ok();
            });
        let next = Button::new("doc-next", "Next")
            .size(ButtonSize::Sm)
            .variant(ButtonVariant::Secondary)
            .disabled(current.exhausted || current.loading)
            .on_click(move |_, _, cx| {
                next_view
                    .update(cx, |this, cx| this.doc_page(session, true, cx))
                    .ok();
            });

        div()
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
                    .child(format!("{}.{}", current.db, current.coll)),
            )
            .child(div().flex_1())
            .child(div().text_color(theme.text_muted).child(status))
            .child(prev)
            .child(next)
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

    /// The raw-document inspector: the selected document as pretty-printed
    /// extended JSON, preserving BSON types (`$oid`, `$date`, ...).
    fn render_doc_inspector(
        &self,
        session: SessionId,
        doc: &Document,
        theme: &Theme,
        view: &WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let close_view = view.clone();
        // Render line-by-line (like the SQL inspector's non-editor fallback) so
        // the pretty-printed newlines/indentation lay out as real lines; a plain
        // multi-line `.child(String)` wouldn't break. Bounded so a pathological
        // document can't lay out unbounded line-divs.
        const MAX_LINES: usize = 5_000;
        let json = pretty_extjson(&doc.to_doc_value());
        let lines: Vec<SharedString> = json
            .lines()
            .take(MAX_LINES)
            .map(|l| SharedString::from(l.to_string()))
            .collect();
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
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
                            .child("Document"),
                    )
                    .child(
                        Button::new("doc-inspector-close", "Close")
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Ghost)
                            .on_click(move |_, _, cx| {
                                close_view
                                    .update(cx, |this, cx| {
                                        if let Phase::Connected(a) = &mut this.phase
                                            && let Some(c) = a
                                                .doc_view
                                                .as_mut()
                                                .filter(|v| v.session == session)
                                                .and_then(|v| v.current.as_mut())
                                        {
                                            c.inspector = None;
                                        }
                                        cx.notify();
                                    })
                                    .ok();
                            }),
                    ),
            )
            .child(
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
                    ),
            )
            .into_any_element()
    }
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
