//! The schema explorer: the left-sidebar tree of namespaces → tables/views →
//! columns, plus the table preview rendered in the results pane.
//!
//! The generic, virtualized tree lives in Flint; the *domain* logic is here: the
//! schema model fetched over `Command`/`Event`, lazy column loading on expand,
//! the live name filter, and turning a double-clicked table into a read-only
//! `SELECT` preview. State hangs off [`ActiveConn`] so it lives for the connection
//! and dies on disconnect.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use flint::prelude::*;
use gpui::{App, Context, Entity, UniformListScrollHandle, Window, div, prelude::*, px};
use red_core::{ColumnMeta, DbKind, ObjectKind, ResultFilter, SchemaMeta, TableDetail};
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase, TabWorkspace};

/// A stable identity for a tree node, surviving re-render and filtering so
/// expansion + selection track the right node regardless of row position.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum NodeId {
    Schema(String),
    Object {
        schema: String,
        name: String,
    },
    Column {
        schema: String,
        table: String,
        name: String,
    },
}

/// The schema explorer's state for one connection.
pub(crate) struct SchemaState {
    /// The tree skeleton (namespaces + object names), from `ObjectsLoaded`.
    pub schemas: Vec<SchemaMeta>,
    /// Per-object detail (columns / FKs / indexes), filled lazily on expand.
    pub details: HashMap<(String, String), TableDetail>,
    pub expanded: HashSet<NodeId>,
    pub selected: Option<NodeId>,
    pub filter: Entity<TextInput>,
    /// Scroll position of the tree's virtual list, so keyboard navigation can keep
    /// the selected row in view (`scroll_to_item`).
    pub tree_scroll: UniformListScrollHandle,
    /// True while the skeleton load is in flight.
    pub loading: bool,
}

impl SchemaState {
    pub fn new(cx: &mut Context<AppState>) -> Self {
        let filter = cx.new(|cx| TextInput::new(cx).with_placeholder("Filter schema…"));
        // Re-render so the filter narrows the tree live as the user types.
        cx.subscribe(&filter, |_this, _input, _evt: &TextInputEvent, cx| {
            cx.notify()
        })
        .detach();
        Self {
            schemas: Vec::new(),
            details: HashMap::new(),
            expanded: HashSet::new(),
            selected: None,
            filter,
            tree_scroll: UniformListScrollHandle::new(),
            loading: true,
        }
    }

    /// Install the loaded skeleton. A lone namespace auto-expands so the user
    /// lands directly on the table list (the common SQLite `main` case).
    pub fn apply_objects(&mut self, schemas: Vec<SchemaMeta>) {
        if schemas.len() == 1 {
            self.expanded
                .insert(NodeId::Schema(schemas[0].name.clone()));
        }
        self.schemas = schemas;
        self.loading = false;
    }
}

/// What a flattened visible row carries for rendering.
enum RowContent {
    Schema {
        name: String,
        count: usize,
    },
    Object {
        kind: ObjectKind,
        name: String,
    },
    Column {
        meta: ColumnMeta,
        is_fk: bool,
    },
    /// A table is expanded but its detail hasn't arrived yet.
    Loading,
}

/// One visible tree row: the structural `item` Flint's `Tree` draws, the content
/// RED renders, the node's identity (for toggle/select), and (for an object)
/// the `(schema, table)` a double-click previews.
struct VisibleRow {
    item: TreeItem,
    content: RowContent,
    node: Option<NodeId>,
    preview: Option<(String, String)>,
}

/// Walk the schema model in display order into the currently-visible rows,
/// applying expansion and the name filter. Pure over the in-memory model, no
/// backend round-trip. When filtering, matched branches force open and only
/// matching leaves show, so the filter reads as a live reveal.
fn flatten(s: &SchemaState, filter: &str) -> Vec<VisibleRow> {
    let f = filter.trim().to_lowercase();
    let filtering = !f.is_empty();
    let hit = |name: &str| name.to_lowercase().contains(&f);

    let mut out = Vec::new();
    for schema in &s.schemas {
        let schema_match = filtering && hit(&schema.name);

        // Which objects survive the filter, and why (name vs. a column hit).
        let mut visible = Vec::new();
        for obj in &schema.objects {
            let obj_match = !filtering || hit(&obj.name);
            let col_hit = filtering
                && s.details
                    .get(&(schema.name.clone(), obj.name.clone()))
                    .is_some_and(|d| d.columns.iter().any(|c| hit(&c.name)));
            if !filtering || schema_match || obj_match || col_hit {
                visible.push((obj, obj_match, col_hit));
            }
        }
        if filtering && !schema_match && visible.is_empty() {
            continue;
        }

        let schema_node = NodeId::Schema(schema.name.clone());
        let schema_open = filtering || s.expanded.contains(&schema_node);
        out.push(VisibleRow {
            item: TreeItem::new(0, !schema.objects.is_empty(), schema_open),
            content: RowContent::Schema {
                name: schema.name.clone(),
                count: schema.objects.len(),
            },
            node: Some(schema_node),
            preview: None,
        });
        if !schema_open {
            continue;
        }

        for (obj, obj_match, col_hit) in visible {
            let obj_node = NodeId::Object {
                schema: schema.name.clone(),
                name: obj.name.clone(),
            };
            // Force open to reveal the matching columns when only a column hit.
            let force = filtering && !schema_match && !obj_match && col_hit;
            let obj_open = s.expanded.contains(&obj_node) || force;
            out.push(VisibleRow {
                item: TreeItem::new(1, true, obj_open),
                content: RowContent::Object {
                    kind: obj.kind,
                    name: obj.name.clone(),
                },
                node: Some(obj_node),
                preview: Some((schema.name.clone(), obj.name.clone())),
            });
            if !obj_open {
                continue;
            }

            match s.details.get(&(schema.name.clone(), obj.name.clone())) {
                Some(detail) => {
                    for col in &detail.columns {
                        // Narrowing by a column hit shows only matching columns.
                        if filtering && !schema_match && !obj_match && !hit(&col.name) {
                            continue;
                        }
                        let is_fk = detail.foreign_keys.iter().any(|fk| fk.column == col.name);
                        out.push(VisibleRow {
                            item: TreeItem::leaf(2),
                            content: RowContent::Column {
                                meta: col.clone(),
                                is_fk,
                            },
                            node: Some(NodeId::Column {
                                schema: schema.name.clone(),
                                table: obj.name.clone(),
                                name: col.name.clone(),
                            }),
                            preview: None,
                        });
                    }
                }
                None => out.push(VisibleRow {
                    item: TreeItem::leaf(2),
                    content: RowContent::Loading,
                    node: None,
                    preview: None,
                }),
            }
        }
    }
    out
}

/// The next selectable row index in `flat`, stepping `forward` (or back) from
/// `from`. Skips rows that carry no node (the "loading…" placeholder). Returns
/// the first/last selectable row when `from` is `None`.
fn next_navigable(flat: &[VisibleRow], from: Option<usize>, forward: bool) -> Option<usize> {
    let len = flat.len();
    let has_node = |i: usize| flat[i].node.is_some();
    match (from, forward) {
        (None, true) => (0..len).find(|&i| has_node(i)),
        (None, false) => (0..len).rev().find(|&i| has_node(i)),
        (Some(cur), true) => ((cur + 1)..len).find(|&i| has_node(i)),
        (Some(cur), false) => (0..cur).rev().find(|&i| has_node(i)),
    }
}

/// Build the content right of the chevron for one tree row.
fn render_node(row: &VisibleRow, cx: &App) -> gpui::AnyElement {
    let theme = cx.theme();
    let (text, muted, faint) = (theme.text, theme.text_muted, theme.text_faint);

    match &row.content {
        RowContent::Schema { name, count } => div()
            .flex()
            .flex_1()
            .items_center()
            .gap_1p5()
            .child(crate::icons::icon("schema", theme.scale(14.), muted))
            .child(
                div()
                    .text_size(theme.scale(12.5))
                    .text_color(text)
                    .child(name.clone()),
            )
            .child(
                div()
                    .ml_auto()
                    .font_family(theme.font_family.clone())
                    .text_size(theme.scale(10.))
                    .text_color(faint)
                    .child(format!("{count} tables")),
            )
            .into_any_element(),

        RowContent::Object { kind, name } => {
            let (name_icon, color) = match kind {
                ObjectKind::Table => ("table", muted),
                ObjectKind::View => ("view", theme.cyan),
            };
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .child(crate::icons::icon(name_icon, theme.scale(14.), color))
                .child(
                    div()
                        .font_family(theme.mono_family.clone())
                        .text_size(theme.scale(12.))
                        .text_color(text)
                        .child(name.clone()),
                )
                .into_any_element()
        }

        RowContent::Column { meta, is_fk } => {
            let mut row = div()
                .flex()
                .items_center()
                .gap_1()
                .child(crate::icons::icon("col", theme.scale(13.), faint))
                .child(
                    div()
                        .font_family(theme.mono_family.clone())
                        .text_size(theme.scale(11.5))
                        .text_color(muted)
                        .child(meta.name.clone()),
                );
            if let Some(type_name) = &meta.type_name {
                row = row.child(
                    div()
                        .font_family(theme.mono_family.clone())
                        .text_size(theme.scale(10.))
                        .text_color(faint)
                        .child(type_name.clone()),
                );
            }
            if meta.primary_key {
                row = row.child(crate::icons::icon(
                    "key-round",
                    theme.scale(12.),
                    theme.yellow,
                ));
            }
            if *is_fk {
                row = row.child(crate::icons::icon("link", theme.scale(12.), theme.accent));
            }
            row.into_any_element()
        }

        RowContent::Loading => div()
            .text_size(theme.scale(11.))
            .text_color(faint)
            .child("loading…")
            .into_any_element(),
    }
}

/// Quote an identifier for the preview `SELECT` so a table name can never break
/// out of the SQL. MySQL/MariaDB use backticks (double quotes are string literals
/// there unless `ANSI_QUOTES` is set); SQLite/Postgres use the SQL-standard double
/// quote. Embedded quote chars are doubled either way.
///
/// ClickHouse also uses double quotes but, unlike SQLite/Postgres, honors backslash
/// escapes inside them, so its backslashes are doubled too; otherwise a table name
/// ending in `\` escapes the closing quote and breaks out.
pub(crate) fn quote_ident(ident: &str, kind: DbKind) -> String {
    match kind {
        DbKind::Mysql => format!("`{}`", ident.replace('`', "``")),
        DbKind::Clickhouse => {
            format!("\"{}\"", ident.replace('\\', "\\\\").replace('"', "\"\""))
        }
        _ => format!("\"{}\"", ident.replace('"', "\"\"")),
    }
}

impl AppState {
    /// The left-sidebar schema explorer: connection pill · filter · tree · footer.
    pub(crate) fn render_schema(
        &self,
        active: &ActiveConn,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (bg_panel, faint) = (theme.bg_panel, theme.text_faint);
        let footer_size = theme.scale(10.);
        let footer_family = theme.font_family.clone();
        let view = cx.entity().downgrade();
        let s = &active.schema;
        let filter_text = s.filter.read(cx).content().to_string();

        let flat = flatten(s, &filter_text);
        let items: Vec<TreeItem> = flat.iter().map(|r| r.item).collect();
        let selected_ix = s
            .selected
            .as_ref()
            .and_then(|sel| flat.iter().position(|r| r.node.as_ref() == Some(sel)));
        let rows = Rc::new(flat);

        let schema_count = s.schemas.len();
        let object_count: usize = s.schemas.iter().map(|sc| sc.objects.len()).sum();

        let er_icon = crate::icons::icon("link", cx.theme().scale(16.), cx.theme().text_muted);
        let filter_row = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .pt_2()
            .pb_1()
            .child(div().flex_1().child(s.filter.clone()))
            // Open the read-only schema ER diagram.
            .child(
                IconButton::new("schema-er-diagram", er_icon)
                    .size(IconButtonSize::Sm)
                    .tooltip("ER diagram")
                    .a11y_label("ER diagram")
                    .on_click(cx.listener(|this, _, _, cx| this.open_er_diagram(cx))),
            );

        // Capture the flattened rows per handler so each can map its click index
        // back to the node it represents.
        let rows_render = rows.clone();
        let rows_toggle = rows.clone();
        let rows_select = rows.clone();
        let rows_activate = rows.clone();
        let (tv, sv, av, nv) = (view.clone(), view.clone(), view.clone(), view.clone());

        let tree = Tree::new("schema-tree")
            .rows(items)
            .row_height(px(24.))
            .indent(px(14.))
            .track_scroll(&s.tree_scroll)
            // Keyboard navigation: the sidebar's focus handle lives on the tree,
            // and ↑/↓ / ←/→ / Enter intents drive selection, expansion, and preview.
            .focus_handle(active.schema_focus.clone())
            .on_nav(move |nav, _window, cx| {
                nv.update(cx, |this, cx| this.schema_nav(nav, cx)).ok();
            })
            .selected(selected_ix)
            .disclosure(|expanded, _window, cx| {
                let name = if expanded { "chevron-down" } else { "chevron" };
                crate::icons::icon(name, cx.theme().scale(12.), cx.theme().text_faint)
                    .into_any_element()
            })
            .render_row(move |ix, _window, cx| render_node(&rows_render[ix], cx))
            .on_toggle(move |ix, _window, cx| {
                if let Some(node) = rows_toggle[ix].node.clone() {
                    tv.update(cx, |this, cx| this.schema_toggle(node, cx)).ok();
                }
            })
            // A single body click acts on the row: a table/view opens in a query
            // tab, a namespace folder expands/collapses, a column just highlights.
            // (The chevron owns expansion for tables, so revealing a table's
            // columns doesn't open it — see the Flint `Tree` chevron hit target.)
            .on_select(move |ix, _event, _window, cx| {
                let node = rows_select[ix].node.clone();
                let preview = rows_select[ix].preview.clone();
                sv.update(cx, |this, cx| match node {
                    Some(NodeId::Object { .. }) => {
                        if let Some((schema, table)) = preview {
                            this.schema_preview(schema, table, cx);
                        }
                    }
                    Some(node @ NodeId::Schema(_)) => {
                        this.schema_select(node.clone(), cx);
                        this.schema_toggle(node, cx);
                    }
                    Some(node) => this.schema_select(node, cx),
                    None => {}
                })
                .ok();
            })
            .on_activate(move |ix, _window, cx| {
                if let Some((schema, table)) = rows_activate[ix].preview.clone() {
                    av.update(cx, |this, cx| this.schema_preview(schema, table, cx))
                        .ok();
                }
            });

        let footer_text = if s.loading {
            "loading…".to_string()
        } else {
            format!("{schema_count} schemas · {object_count} tables")
        };
        let footer = div()
            .flex_shrink_0()
            .h(px(22.))
            .flex()
            .items_center()
            .px_2()
            .font_family(footer_family)
            .text_size(footer_size)
            .text_color(faint)
            .child(footer_text);

        div()
            .size_full()
            .flex()
            .flex_col()
            // The tree itself owns the focus handle + navigation keys (see its
            // `.focus_handle`/`.on_nav` above); the pane draws no focus ring.
            .bg(bg_panel)
            .child(filter_row)
            .child(div().flex_1().min_h(px(0.)).child(tree))
            .child(footer)
    }

    // --- tree interactions ---

    /// Toggle a node's expansion. Expanding an object whose detail isn't cached
    /// fires a lazy `DescribeTable`.
    pub(crate) fn schema_toggle(&mut self, node: NodeId, cx: &mut Context<Self>) {
        let mut describe = None;
        if let Phase::Connected(active) = &mut self.phase {
            let s = &mut active.schema;
            if !s.expanded.remove(&node) {
                if let NodeId::Object { schema, name } = &node
                    && !s.details.contains_key(&(schema.clone(), name.clone()))
                {
                    describe = Some((schema.clone(), name.clone()));
                }
                s.expanded.insert(node);
            }
        }
        if let Some((schema, table)) = describe {
            self.send_active(Command::DescribeTable { schema, table });
        }
        cx.notify();
    }

    pub(crate) fn schema_select(&mut self, node: NodeId, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.schema.selected = Some(node);
        }
        cx.notify();
    }

    /// Drive the schema tree from the keyboard (the focused sidebar's arrows +
    /// Enter). Recomputes the same flattened, filtered visible list the render
    /// uses, then moves the selection, toggles expansion, or previews, reusing
    /// the existing click/double-click handlers so keyboard and mouse stay in step.
    fn schema_nav(&mut self, nav: TreeNav, cx: &mut Context<Self>) {
        // Snapshot the visible rows (owned, so no borrow of `self` is held while
        // the mutating handlers below run) and the selected row's position.
        let (flat, sel) = match &self.phase {
            Phase::Connected(active) => {
                let s = &active.schema;
                let filter = s.filter.read(cx).content().to_string();
                let flat = flatten(s, &filter);
                let sel = s
                    .selected
                    .as_ref()
                    .and_then(|n| flat.iter().position(|r| r.node.as_ref() == Some(n)));
                (flat, sel)
            }
            _ => return,
        };
        if flat.is_empty() {
            return;
        }

        match nav {
            TreeNav::Up => {
                if let Some(ix) = next_navigable(&flat, sel, false) {
                    self.schema_focus_row(&flat, ix, cx);
                }
            }
            TreeNav::Down => {
                if let Some(ix) = next_navigable(&flat, sel, true) {
                    self.schema_focus_row(&flat, ix, cx);
                }
            }
            TreeNav::Expand => {
                let Some(i) = sel else { return };
                let row = &flat[i];
                if row.item.has_children && !row.item.expanded {
                    if let Some(node) = row.node.clone() {
                        self.schema_toggle(node, cx);
                    }
                } else if row.item.expanded {
                    // Already open: descend to the first child (next row down).
                    if let Some(ix) = next_navigable(&flat, sel, true) {
                        self.schema_focus_row(&flat, ix, cx);
                    }
                }
            }
            TreeNav::Collapse => {
                let Some(i) = sel else { return };
                let row = &flat[i];
                if row.item.has_children && row.item.expanded {
                    if let Some(node) = row.node.clone() {
                        self.schema_toggle(node, cx);
                    }
                } else if row.item.depth > 0 {
                    // A leaf or collapsed node: jump to the parent (nearest row
                    // above at a shallower depth).
                    if let Some(p) = (0..i).rev().find(|&j| flat[j].item.depth < row.item.depth) {
                        self.schema_focus_row(&flat, p, cx);
                    }
                }
            }
            TreeNav::Activate => {
                let Some(i) = sel else { return };
                let row = &flat[i];
                if let Some((schema, table)) = row.preview.clone() {
                    self.schema_preview(schema, table, cx);
                } else if row.item.has_children
                    && let Some(node) = row.node.clone()
                {
                    self.schema_toggle(node, cx);
                }
            }
        }
    }

    /// Select the row at flat index `ix` and scroll it into view.
    fn schema_focus_row(&mut self, flat: &[VisibleRow], ix: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(node) = flat[ix].node.clone() {
                active.schema.selected = Some(node);
            }
            // Non-strict: only scrolls when the row is off-screen, so stepping
            // through visible rows doesn't yank the list on every keypress.
            active
                .schema
                .tree_scroll
                .scroll_to_item(ix, gpui::ScrollStrategy::Top);
        }
        cx.notify();
    }

    /// Preview a table/view: open `SELECT * FROM schema.table` in a **new** query
    /// tab so the user's current query and result are preserved. No `LIMIT`; the
    /// grid pages through it with flat memory. The new tab's editor is pre-filled.
    pub(crate) fn schema_preview(&mut self, schema: String, table: String, cx: &mut Context<Self>) {
        // Highlight the previewed object in the sidebar tree, then open it.
        if let Phase::Connected(active) = &mut self.phase {
            active.schema.selected = Some(NodeId::Object {
                schema: schema.clone(),
                name: table.clone(),
            });
        }
        self.open_table_browse(schema, table, None, cx);
    }

    /// Open `SELECT * FROM schema.table` (optionally pre-filtered) in a reused
    /// pristine tab or a fresh one: the shared path for the sidebar preview and the
    /// FK click-through (Track B7). The editor is pre-filled with the base SQL; a
    /// `filter` narrows the result without changing the shown query.
    pub(crate) fn open_table_browse(
        &mut self,
        schema: String,
        table: String,
        filter: Option<ResultFilter>,
        cx: &mut Context<Self>,
    ) {
        let (sql, label, table_ref) = match &self.phase {
            Phase::Connected(active) => {
                let kind = active.config.kind;
                let sql = format!(
                    "SELECT * FROM {}.{}",
                    quote_ident(&schema, kind),
                    quote_ident(&table, kind)
                );
                let label = format!("{schema}.{table}");
                // The browsed table rides along so the backend can resolve a
                // keyset seek key for it.
                (sql, label, (schema, table))
            }
            _ => return,
        };
        // Reuse the focused tab if it's untouched, or if it's already browsing
        // this exact table (so a single click that opens a table and its
        // trailing double-click, or a re-click, refresh in place instead of
        // stacking duplicate tabs). Otherwise open a new one so the user's
        // current query and result are preserved.
        let reuse = matches!(&self.phase, Phase::Connected(a)
            if a.active().is_some_and(|t| t.is_pristine(cx) || t.title == label));
        if reuse {
            if let Phase::Connected(active) = &mut self.phase {
                // Repurpose the focused half's untouched tab in place (it stays in its
                // pane); just relabel it to the previewed table.
                let from = active.focused_tab_index();
                if let Some(tab) = active.tabs.get_mut(from) {
                    tab.title = label.clone();
                }
                active.tab_scroll.scroll_to_item(from);
            }
        } else {
            // No pristine tab to reuse (incl. the empty-strip case), so open one.
            let tab = crate::app::QueryTab::new(label.clone(), cx);
            self.push_tab(tab, cx);
        }
        let editor = match &self.phase {
            Phase::Connected(active) => match active.active() {
                Some(tab) => tab.editor.clone(),
                None => return,
            },
            _ => return,
        };
        editor.update(cx, |editor, cx| editor.set_content(sql.clone(), cx));
        self.open_result_filtered(label, sql, Some(table_ref), filter, cx);
    }
}
