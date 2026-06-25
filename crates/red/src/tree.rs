//! The relation-tree view (Track B7 — "perspectives"). "Open row as tree" on a
//! result row opens a read-only, lazily-expanded tree that walks the connection's
//! foreign-key graph: a record → its relations (forward to-one / reverse to-many)
//! → the related records → *their* relations, recursively.
//!
//! It stays inside the performance budget by construction: each relation loads one
//! capped, windowed page only when expanded (collapsed branches issue no query),
//! and memory is bounded by the *expanded* nodes × the page cap, never the row
//! count. There are no JOINs and no eager descent — every level is one
//! `SELECT * FROM child WHERE fk = <value>` (the same typed-literal filter as the
//! Track B7 click-through), fetched on demand. The visual tree mirrors the schema
//! sidebar (a flattened, virtualized list); the data model is here.
//!
//! Deliberately out of scope (see `docs/plans/fk-navigation.md`): a join designer,
//! user-declared virtual FKs, custom/cross-connection joins, grouping. This is the
//! read-only auto-FK tree only.

use std::rc::Rc;

use flint::prelude::*;
use flint::{Button, ButtonSize, ButtonVariant};
use gpui::{
    div, prelude::*, px, AnyElement, ClipboardItem, Context, FocusHandle, MouseButton, Pixels,
    Point, ScrollStrategy, SharedString, UniformListScrollHandle,
};
use red_core::{Column as ResultColumn, ColumnValue, DbKind, FkEdge, ResultFilter, Value};
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase};
use crate::result::new_epoch;
use crate::schema::quote_ident;

/// Rows fetched per relation node before the "Load more" affordance appears.
const TREE_PAGE: usize = 100;

/// One open relation tree, held per [`QueryTab`](crate::app::QueryTab) in place of
/// the grid (like the Track B4 plan view). `epoch` routes async node loads; node
/// ids are minted from `next_id`.
pub(crate) struct RelationTreeView {
    /// Routes `TreeNodeLoaded` replies to this tree (a stale one is dropped).
    pub(crate) epoch: u64,
    next_id: u64,
    root: Node,
    /// The keyboard-selected node (a stable id, so it survives expand/collapse),
    /// resolved to a flat index per render.
    selected: Option<u64>,
    /// Focus for keyboard navigation (Tier 2) — the tree's `on_nav` only fires
    /// while this handle is focused.
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
}

impl RelationTreeView {
    fn find(&self, id: u64) -> Option<&Node> {
        fn go(node: &Node, id: u64) -> Option<&Node> {
            if node.id == id {
                return Some(node);
            }
            node.children.iter().find_map(|c| go(c, id))
        }
        go(&self.root, id)
    }

    fn find_mut(&mut self, id: u64) -> Option<&mut Node> {
        Self::find_in(&mut self.root, id)
    }

    fn find_in(node: &mut Node, id: u64) -> Option<&mut Node> {
        if node.id == id {
            return Some(node);
        }
        node.children.iter_mut().find_map(|c| Self::find_in(c, id))
    }
}

/// One tree node: a record (a single row) or a relation (a fetchable set of rows).
struct Node {
    id: u64,
    expanded: bool,
    /// Load state of a [`NodeKind::Relation`]'s rows; unused for records (their
    /// relation children are built synchronously on expand).
    state: NodeState,
    /// More rows exist beyond the loaded page (relations only) — drives "Load more".
    more: bool,
    /// The page size last requested for this relation, grown by "Load more".
    loaded_limit: usize,
    kind: NodeKind,
    children: Vec<Node>,
}

enum NodeState {
    Unloaded,
    Loading,
    Loaded,
    Failed(SharedString),
}

enum NodeKind {
    /// A single record (row) of `table`. Children are its relation nodes, built
    /// from the FK graph on first expand (no query). `summary` is the precomputed
    /// one-line label; `has_relations` whether any FK edge touches `table`.
    Record {
        schema: String,
        table: String,
        columns: Rc<Vec<ResultColumn>>,
        values: Rc<Vec<Value>>,
        summary: SharedString,
        has_relations: bool,
    },
    /// A set of records from `table` reached by following one FK edge: the rows
    /// where `filter` holds. `base_sql` is the `SELECT * FROM schema.table` the
    /// backend filters and caps. Children are [`NodeKind::Record`]s, fetched lazily.
    Relation {
        label: SharedString,
        schema: String,
        table: String,
        base_sql: String,
        filter: ResultFilter,
    },
}

/// One flattened, render-ready row of the tree (depth carried in `item`).
struct VisibleRow {
    item: TreeItem,
    content: RowContent,
    /// The node this row expands/collapses (records and relations).
    node_id: Option<u64>,
    /// Set when this row is the "Load more" affordance for a relation node.
    more_for: Option<u64>,
}

enum RowContent {
    Record(SharedString),
    Relation {
        label: SharedString,
        count: Option<SharedString>,
    },
    Loading,
    Empty,
    Failed(SharedString),
    More,
}

/// A one-line summary of a record: the first few non-null cells, `col=value`.
fn summarize(columns: &[ResultColumn], values: &[Value]) -> SharedString {
    let parts: Vec<String> = columns
        .iter()
        .zip(values)
        .filter(|(_, v)| !matches!(v, Value::Null))
        .take(4)
        .map(|(c, v)| format!("{}={}", c.name, v))
        .collect();
    if parts.is_empty() {
        "(empty row)".into()
    } else {
        parts.join("  ·  ").into()
    }
}

/// Whether any single-column FK edge touches `table` (forward or reverse) — i.e.
/// a record of `table` has relations worth a disclosure caret.
fn has_relations(graph: &[FkEdge], schema: &str, table: &str) -> bool {
    graph.iter().any(|e| {
        e.columns.len() == 1
            && ((e.from_table == table && e.from_schema.as_deref() == Some(schema))
                || (e.to_table == table && e.to_schema.as_deref() == Some(schema)))
    })
}

/// Build a record node from a fetched row.
fn record_node(
    next_id: &mut u64,
    graph: &[FkEdge],
    schema: String,
    table: String,
    columns: Rc<Vec<ResultColumn>>,
    values: Rc<Vec<Value>>,
) -> Node {
    let summary = summarize(&columns, &values);
    let has = has_relations(graph, &schema, &table);
    let id = *next_id;
    *next_id += 1;
    Node {
        id,
        expanded: false,
        state: NodeState::Loaded,
        more: false,
        loaded_limit: 0,
        kind: NodeKind::Record {
            schema,
            table,
            columns,
            values,
            summary,
            has_relations: has,
        },
        children: Vec::new(),
    }
}

/// Synthesize a record's relation children from the FK graph: one node per
/// single-column edge touching the record's table whose key value is non-null.
fn build_relations(
    next_id: &mut u64,
    graph: &[FkEdge],
    kind: DbKind,
    schema: &str,
    table: &str,
    columns: &[ResultColumn],
    values: &[Value],
) -> Vec<Node> {
    let value_of = |name: &str| -> Option<&Value> {
        columns
            .iter()
            .position(|c| c.name == name)
            .and_then(|i| values.get(i))
    };
    let mut out = Vec::new();
    for e in graph {
        if e.columns.len() != 1 {
            continue;
        }
        let (from_col, to_col) = &e.columns[0];

        // Forward (to-one): this table holds the FK → jump to the referenced row.
        if e.from_table == table && e.from_schema.as_deref() == Some(schema) {
            if let Some(v) = value_of(from_col).filter(|v| !matches!(v, Value::Null)) {
                let tsch = e.to_schema.clone().unwrap_or_else(|| schema.to_string());
                out.push(relation_node(
                    next_id,
                    format!("→ {}", e.to_table).into(),
                    tsch,
                    e.to_table.clone(),
                    to_col.clone(),
                    v.clone(),
                    kind,
                ));
            }
        }
        // Reverse (to-many): rows of another table that reference this record.
        if e.to_table == table && e.to_schema.as_deref() == Some(schema) {
            if let Some(v) = value_of(to_col).filter(|v| !matches!(v, Value::Null)) {
                let csch = e.from_schema.clone().unwrap_or_else(|| schema.to_string());
                out.push(relation_node(
                    next_id,
                    format!("↳ {} ({})", e.from_table, from_col).into(),
                    csch,
                    e.from_table.clone(),
                    from_col.clone(),
                    v.clone(),
                    kind,
                ));
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn relation_node(
    next_id: &mut u64,
    label: SharedString,
    schema: String,
    table: String,
    filter_col: String,
    value: Value,
    kind: DbKind,
) -> Node {
    let base_sql = format!(
        "SELECT * FROM {}.{}",
        quote_ident(&schema, kind),
        quote_ident(&table, kind)
    );
    let filter = ResultFilter::Eq(vec![ColumnValue {
        column: filter_col,
        value,
        decl_type: None,
    }]);
    let id = *next_id;
    *next_id += 1;
    Node {
        id,
        expanded: false,
        state: NodeState::Unloaded,
        more: false,
        loaded_limit: 0,
        kind: NodeKind::Relation {
            label,
            schema,
            table,
            base_sql,
            filter,
        },
        children: Vec::new(),
    }
}

fn flatten(view: &RelationTreeView) -> Vec<VisibleRow> {
    let mut out = Vec::new();
    walk(&view.root, 0, &mut out);
    out
}

fn walk(node: &Node, depth: usize, out: &mut Vec<VisibleRow>) {
    match &node.kind {
        NodeKind::Record {
            summary,
            has_relations,
            ..
        } => {
            out.push(VisibleRow {
                item: TreeItem::new(depth, *has_relations, node.expanded),
                content: RowContent::Record(summary.clone()),
                node_id: Some(node.id),
                more_for: None,
            });
            if node.expanded {
                for c in &node.children {
                    walk(c, depth + 1, out);
                }
            }
        }
        NodeKind::Relation { label, .. } => {
            let count = matches!(node.state, NodeState::Loaded).then(|| {
                if node.more {
                    format!("{}+", node.children.len()).into()
                } else {
                    format!("{}", node.children.len()).into()
                }
            });
            out.push(VisibleRow {
                item: TreeItem::new(depth, true, node.expanded),
                content: RowContent::Relation {
                    label: label.clone(),
                    count,
                },
                node_id: Some(node.id),
                more_for: None,
            });
            if node.expanded {
                match &node.state {
                    NodeState::Loading => out.push(leaf(depth + 1, RowContent::Loading)),
                    NodeState::Failed(e) => {
                        out.push(leaf(depth + 1, RowContent::Failed(e.clone())))
                    }
                    NodeState::Loaded if node.children.is_empty() => {
                        out.push(leaf(depth + 1, RowContent::Empty))
                    }
                    NodeState::Loaded => {
                        for c in &node.children {
                            walk(c, depth + 1, out);
                        }
                        if node.more {
                            out.push(VisibleRow {
                                item: TreeItem::leaf(depth + 1),
                                content: RowContent::More,
                                node_id: None,
                                more_for: Some(node.id),
                            });
                        }
                    }
                    NodeState::Unloaded => {}
                }
            }
        }
    }
}

fn leaf(depth: usize, content: RowContent) -> VisibleRow {
    VisibleRow {
        item: TreeItem::leaf(depth),
        content,
        node_id: None,
        more_for: None,
    }
}

/// The next selectable row (one with a `node_id`) above/below `from` — skips the
/// synthetic loading / empty / load-more rows during keyboard navigation.
fn next_nav(flat: &[VisibleRow], from: Option<usize>, down: bool) -> Option<usize> {
    let sel = |i: usize| flat[i].node_id.is_some();
    match (from, down) {
        (None, true) => (0..flat.len()).find(|&i| sel(i)),
        (None, false) => (0..flat.len()).rev().find(|&i| sel(i)),
        (Some(c), true) => ((c + 1)..flat.len()).find(|&i| sel(i)),
        (Some(c), false) => (0..c).rev().find(|&i| sel(i)),
    }
}

impl AppState {
    /// The focused tab's open relation tree, if any (Track B7).
    pub(crate) fn has_active_tree(&self) -> bool {
        matches!(&self.phase, Phase::Connected(a) if a.active().is_some_and(|t| t.tree.is_some()))
    }

    /// "Open row as tree": fetch the focused row in full (the inspector's `CopyRows`
    /// path), then build the tree rooted at that record (in [`on_tree_root_rows`]).
    /// No-op when the result isn't a single-table browse (no FK context).
    pub(crate) fn open_row_as_tree(&mut self, cx: &mut Context<Self>) {
        let pending = match &self.phase {
            Phase::Connected(active) => {
                let Some(grid) = active.active_result() else {
                    return;
                };
                let Some((schema, table)) = grid.base_table() else {
                    return;
                };
                let Some((row, _)) = grid.cursor_cell(self.gutter()) else {
                    return;
                };
                PendingTreeRoot {
                    id: 0,
                    epoch: grid.epoch,
                    row,
                    schema: schema.clone(),
                    table: table.clone(),
                    columns: grid.columns().to_vec(),
                }
            }
            _ => return,
        };
        let id = self.next_copy_id;
        self.next_copy_id += 1;
        let (epoch, row) = (pending.epoch, pending.row);
        self.pending_tree = Some(PendingTreeRoot { id, ..pending });
        self.send_active(Command::CopyRows {
            offset: row,
            limit: 1,
            epoch,
            id,
        });
        cx.notify();
    }

    /// A `CopyRows` reply claimed by a pending "open as tree": build the root record
    /// node and show the tree in the focused tab. Returns whether it claimed it.
    pub(crate) fn on_tree_root_rows(
        &mut self,
        id: u64,
        rows: &[Vec<Value>],
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(p) = self.pending_tree.take_if(|p| p.id == id) else {
            return false;
        };
        let Some(values) = rows.first().cloned() else {
            return true;
        };
        // The focus handle is minted before the `phase` borrow (it needs `cx`).
        let focus = cx.focus_handle();
        let Phase::Connected(active) = &mut self.phase else {
            return true;
        };
        let mut next_id = 0u64;
        let kind = active.config.kind;
        let columns = Rc::new(p.columns);
        let values = Rc::new(values);
        let mut root = record_node(
            &mut next_id,
            &active.fk_graph,
            p.schema.clone(),
            p.table.clone(),
            columns.clone(),
            values.clone(),
        );
        let root_id = root.id;
        // Auto-expand the root so its relations show immediately (building them is
        // synchronous — no query until a relation itself is expanded).
        if matches!(
            &root.kind,
            NodeKind::Record {
                has_relations: true,
                ..
            }
        ) {
            root.children = build_relations(
                &mut next_id,
                &active.fk_graph,
                kind,
                &p.schema,
                &p.table,
                &columns,
                &values,
            );
            root.expanded = true;
        }
        let view = RelationTreeView {
            epoch: new_epoch(),
            next_id,
            root,
            selected: Some(root_id),
            focus,
            scroll: UniformListScrollHandle::new(),
        };
        if let Some(tab) = active.active_mut() {
            tab.tree = Some(view);
        }
        cx.notify();
        true
    }

    /// Close the tree, returning the tab to its grid.
    pub(crate) fn close_tree(&mut self, cx: &mut Context<Self>) {
        self.tree_menu = None;
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(tab) = active.active_mut() {
                tab.tree = None;
            }
        }
        cx.notify();
    }

    /// Act on a node (double-click / Enter, Tier 1): a relation opens in the grid;
    /// a record re-roots the tree at itself.
    fn tree_activate(&mut self, node_id: u64, cx: &mut Context<Self>) {
        let is_relation = matches!(&self.phase, Phase::Connected(a)
            if a.active().and_then(|t| t.tree.as_ref()).and_then(|v| v.find(node_id))
                .is_some_and(|n| matches!(n.kind, NodeKind::Relation { .. })));
        if is_relation {
            self.open_relation_in_grid(node_id, cx);
        } else {
            self.reroot_tree(node_id, cx);
        }
    }

    /// Open a relation node as a normal filtered browse in a tab — the full grid
    /// (sort / filter / export / edit). The tree is preserved in its own tab.
    fn open_relation_in_grid(&mut self, node_id: u64, cx: &mut Context<Self>) {
        let target = match &self.phase {
            Phase::Connected(active) => active
                .active()
                .and_then(|t| t.tree.as_ref())
                .and_then(|v| v.find(node_id))
                .and_then(|n| match &n.kind {
                    NodeKind::Relation {
                        schema,
                        table,
                        filter,
                        ..
                    } => Some((schema.clone(), table.clone(), filter.clone())),
                    _ => None,
                }),
            _ => None,
        };
        if let Some((schema, table, filter)) = target {
            self.open_table_browse(schema, table, Some(filter), cx);
        }
    }

    /// Re-root the tree at a record node (pivot). The record already holds its
    /// values, so this needs no fetch.
    fn reroot_tree(&mut self, node_id: u64, cx: &mut Context<Self>) {
        let (graph, kind) = match &self.phase {
            Phase::Connected(a) => (a.fk_graph.clone(), a.config.kind),
            _ => return,
        };
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(view) = active.active_mut().and_then(|t| t.tree.as_mut()) {
                let focus = view.focus.clone();
                let data = view.find(node_id).and_then(|n| match &n.kind {
                    NodeKind::Record {
                        schema,
                        table,
                        columns,
                        values,
                        ..
                    } => Some((
                        schema.clone(),
                        table.clone(),
                        columns.clone(),
                        values.clone(),
                    )),
                    _ => None,
                });
                if let Some((schema, table, columns, values)) = data {
                    let mut next_id = 0u64;
                    let mut root = record_node(
                        &mut next_id,
                        &graph,
                        schema.clone(),
                        table.clone(),
                        columns.clone(),
                        values.clone(),
                    );
                    let root_id = root.id;
                    if matches!(
                        &root.kind,
                        NodeKind::Record {
                            has_relations: true,
                            ..
                        }
                    ) {
                        root.children = build_relations(
                            &mut next_id,
                            &graph,
                            kind,
                            &schema,
                            &table,
                            &columns,
                            &values,
                        );
                        root.expanded = true;
                    }
                    *view = RelationTreeView {
                        epoch: new_epoch(),
                        next_id,
                        root,
                        selected: Some(root_id),
                        focus,
                        scroll: UniformListScrollHandle::new(),
                    };
                }
            }
        }
        cx.notify();
    }

    /// Copy a record node's row as tab-separated values (Tier 3).
    fn copy_tree_record(&mut self, node_id: u64, cx: &mut Context<Self>) {
        let tsv = match &self.phase {
            Phase::Connected(active) => active
                .active()
                .and_then(|t| t.tree.as_ref())
                .and_then(|v| v.find(node_id))
                .and_then(|n| match &n.kind {
                    NodeKind::Record { values, .. } => Some(
                        values
                            .iter()
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join("\t"),
                    ),
                    _ => None,
                }),
            _ => None,
        };
        if let Some(tsv) = tsv {
            cx.write_to_clipboard(ClipboardItem::new_string(tsv));
        }
    }

    /// Keyboard navigation over the tree (Tier 2): move selection, expand/collapse,
    /// or activate. Mirrors the schema sidebar's `schema_nav`.
    fn tree_nav(&mut self, nav: TreeNav, cx: &mut Context<Self>) {
        let (flat, sel) = match &self.phase {
            Phase::Connected(active) => {
                let Some(view) = active.active().and_then(|t| t.tree.as_ref()) else {
                    return;
                };
                let flat = flatten(view);
                let sel = view
                    .selected
                    .and_then(|id| flat.iter().position(|r| r.node_id == Some(id)));
                (flat, sel)
            }
            _ => return,
        };
        if flat.is_empty() {
            return;
        }
        match nav {
            TreeNav::Up => {
                if let Some(ix) = next_nav(&flat, sel, false) {
                    self.tree_select_ix(&flat, ix, cx);
                }
            }
            TreeNav::Down => {
                if let Some(ix) = next_nav(&flat, sel, true) {
                    self.tree_select_ix(&flat, ix, cx);
                }
            }
            TreeNav::Expand => {
                let Some(i) = sel else { return };
                let row = &flat[i];
                if row.item.has_children && !row.item.expanded {
                    if let Some(id) = row.node_id {
                        self.tree_toggle(id, cx);
                    }
                } else if row.item.expanded {
                    if let Some(ix) = next_nav(&flat, sel, true) {
                        self.tree_select_ix(&flat, ix, cx);
                    }
                }
            }
            TreeNav::Collapse => {
                let Some(i) = sel else { return };
                let row = &flat[i];
                if row.item.has_children && row.item.expanded {
                    if let Some(id) = row.node_id {
                        self.tree_toggle(id, cx);
                    }
                } else if row.item.depth > 0 {
                    if let Some(p) = (0..i)
                        .rev()
                        .find(|&j| flat[j].item.depth < row.item.depth && flat[j].node_id.is_some())
                    {
                        self.tree_select_ix(&flat, p, cx);
                    }
                }
            }
            TreeNav::Activate => {
                let Some(i) = sel else { return };
                if let Some(id) = flat[i].node_id {
                    self.tree_activate(id, cx);
                }
            }
        }
    }

    fn tree_select_ix(&mut self, flat: &[VisibleRow], ix: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(view) = active.active_mut().and_then(|t| t.tree.as_mut()) {
                view.selected = flat[ix].node_id;
                view.scroll.scroll_to_item(ix, ScrollStrategy::Top);
            }
        }
        cx.notify();
    }

    /// Open the right-click context menu on a tree node (Tier 3).
    fn open_tree_menu(&mut self, node_id: Option<u64>, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(id) = node_id else { return };
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(view) = active.active_mut().and_then(|t| t.tree.as_mut()) {
                view.selected = Some(id);
            }
        }
        self.tree_menu = Some((id, pos));
        cx.notify();
    }

    /// Toggle a node: a record builds its relation children (synchronously) on first
    /// expand; a relation fetches its rows lazily (a capped page) on first expand.
    fn tree_toggle(&mut self, node_id: u64, cx: &mut Context<Self>) {
        let (graph, kind) = match &self.phase {
            Phase::Connected(a) => (a.fk_graph.clone(), a.config.kind),
            _ => return,
        };
        let mut fetch: Option<(u64, u64, String, ResultFilter, usize)> = None;
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(tab) = active.active_mut() {
                if let Some(view) = &mut tab.tree {
                    let epoch = view.epoch;
                    let mut next_id = view.next_id;
                    if let Some(node) = view.find_mut(node_id) {
                        if node.expanded {
                            node.expanded = false;
                        } else {
                            node.expanded = true;
                            // Decide the action without holding `node.kind` borrowed.
                            let action = match &node.kind {
                                NodeKind::Record {
                                    schema,
                                    table,
                                    columns,
                                    values,
                                    ..
                                } => Toggle::Build {
                                    schema: schema.clone(),
                                    table: table.clone(),
                                    columns: columns.clone(),
                                    values: values.clone(),
                                    need: node.children.is_empty(),
                                },
                                NodeKind::Relation {
                                    base_sql, filter, ..
                                } => Toggle::Fetch {
                                    base_sql: base_sql.clone(),
                                    filter: filter.clone(),
                                    need: matches!(node.state, NodeState::Unloaded),
                                },
                            };
                            match action {
                                Toggle::Build {
                                    schema,
                                    table,
                                    columns,
                                    values,
                                    need,
                                } => {
                                    if need {
                                        node.children = build_relations(
                                            &mut next_id,
                                            &graph,
                                            kind,
                                            &schema,
                                            &table,
                                            &columns,
                                            &values,
                                        );
                                    }
                                }
                                Toggle::Fetch {
                                    base_sql,
                                    filter,
                                    need,
                                } => {
                                    if need {
                                        node.state = NodeState::Loading;
                                        node.loaded_limit = TREE_PAGE;
                                        fetch = Some((epoch, node.id, base_sql, filter, TREE_PAGE));
                                    }
                                }
                            }
                        }
                    }
                    view.next_id = next_id;
                }
            }
        }
        if let Some((epoch, node, base_sql, filter, limit)) = fetch {
            self.send_active(Command::FetchTreeNode {
                epoch,
                node,
                base_sql,
                filter: Some(filter),
                limit,
            });
        }
        cx.notify();
    }

    /// Grow a relation node's page and re-fetch (the "Load more" affordance).
    fn tree_load_more(&mut self, node_id: u64, cx: &mut Context<Self>) {
        let mut fetch = None;
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(tab) = active.active_mut() {
                if let Some(view) = &mut tab.tree {
                    let epoch = view.epoch;
                    if let Some(node) = view.find_mut(node_id) {
                        if let NodeKind::Relation {
                            base_sql, filter, ..
                        } = &node.kind
                        {
                            let limit = node.loaded_limit + TREE_PAGE;
                            let (base_sql, filter) = (base_sql.clone(), filter.clone());
                            node.loaded_limit = limit;
                            node.state = NodeState::Loading;
                            fetch = Some((epoch, node.id, base_sql, filter, limit));
                        }
                    }
                }
            }
        }
        if let Some((epoch, node, base_sql, filter, limit)) = fetch {
            self.send_active(Command::FetchTreeNode {
                epoch,
                node,
                base_sql,
                filter: Some(filter),
                limit,
            });
        }
        cx.notify();
    }

    /// A relation node's rows arrived: build its record children (or record the
    /// failure). Routed by tree epoch then node id; a stale reply finds no match.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn on_tree_node_loaded(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: u64,
        node: u64,
        columns: Vec<ResultColumn>,
        rows: Vec<Vec<Value>>,
        more: bool,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let graph = match self.conn_mut(session) {
            Some(active) => active.fk_graph.clone(),
            None => return,
        };
        if let Some(active) = self.conn_mut(session) {
            // The reply may belong to any tab's tree (background load), so scan tabs.
            for tab in &mut active.tabs {
                let Some(view) = &mut tab.tree else { continue };
                if view.epoch != epoch {
                    continue;
                }
                let mut next_id = view.next_id;
                if let Some(n) = view.find_mut(node) {
                    if let Some(message) = error {
                        n.state = NodeState::Failed(message.into());
                        n.children.clear();
                        n.more = false;
                    } else {
                        let cols = Rc::new(columns);
                        let (schema, table) = match &n.kind {
                            NodeKind::Relation { schema, table, .. } => {
                                (schema.clone(), table.clone())
                            }
                            // A record node never receives a fetch reply.
                            NodeKind::Record { .. } => return,
                        };
                        n.children = rows
                            .into_iter()
                            .map(|row| {
                                record_node(
                                    &mut next_id,
                                    &graph,
                                    schema.clone(),
                                    table.clone(),
                                    cols.clone(),
                                    Rc::new(row),
                                )
                            })
                            .collect();
                        n.more = more;
                        n.state = NodeState::Loaded;
                    }
                    view.next_id = next_id;
                }
                break;
            }
        }
        cx.notify();
    }

    /// Render the relation-tree pane: a header (root table · close) over the
    /// flattened, virtualized tree.
    pub(crate) fn render_tree(&self, active: &ActiveConn, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (bg, border, text, muted) =
            (theme.bg_panel, theme.border, theme.text, theme.text_muted);
        let ui_family = theme.font_family.clone();
        let (s11, s12) = (theme.scale(11.), theme.scale(12.));

        let Some(view) = active.active().and_then(|t| t.tree.as_ref()) else {
            return div().into_any_element();
        };

        let title = match &view.root.kind {
            NodeKind::Record { schema, table, .. } => format!("{schema}.{table}"),
            NodeKind::Relation { table, .. } => table.clone(),
        };

        let flat = flatten(view);
        let items: Vec<TreeItem> = flat.iter().map(|r| r.item).collect();
        let rows = Rc::new(flat);
        let selected_ix = view
            .selected
            .and_then(|id| rows.iter().position(|r| r.node_id == Some(id)));
        let (rows_render, rows_toggle, rows_activate, rows_select, rows_sec) = (
            rows.clone(),
            rows.clone(),
            rows.clone(),
            rows.clone(),
            rows.clone(),
        );
        let (tv, av, sv, nv, gv) = (
            cx.entity().downgrade(),
            cx.entity().downgrade(),
            cx.entity().downgrade(),
            cx.entity().downgrade(),
            cx.entity().downgrade(),
        );

        // Tier 3: the right-click context menu, anchored at the cursor. Its actions
        // depend on the node's kind; a full-cover backdrop dismisses it.
        let menu_overlay =
            self.tree_menu.and_then(|(id, pos)| {
                let node = view.find(id)?;
                let mut menu = ContextMenu::new("tree-node-menu");
                match &node.kind {
                    NodeKind::Relation { .. } => {
                        menu = menu.item(
                            ContextMenuItem::new("tree-open-grid", "Open in grid").on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.tree_menu = None;
                                    this.open_relation_in_grid(id, cx);
                                    cx.notify();
                                }),
                            ),
                        );
                    }
                    NodeKind::Record { .. } => {
                        menu = menu
                            .item(
                                ContextMenuItem::new("tree-reroot", "Open row as tree").on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.tree_menu = None;
                                        this.reroot_tree(id, cx);
                                        cx.notify();
                                    }),
                                ),
                            )
                            .item(ContextMenuItem::new("tree-copy", "Copy row").on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.tree_menu = None;
                                    this.copy_tree_record(id, cx);
                                    cx.notify();
                                }),
                            ));
                    }
                }
                Some(
                    div()
                        .absolute()
                        .inset_0()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.tree_menu = None;
                                cx.notify();
                            }),
                        )
                        .child(floating(div().occlude().child(menu)).at(pos)),
                )
            });

        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .h(px(34.))
            .px_3()
            .bg(bg)
            .border_b_1()
            .border_color(border)
            .font_family(ui_family.clone())
            .text_size(s11)
            .child(crate::icons::icon("table", theme.scale(13.), muted))
            .child(div().text_color(text).child(format!("Relations · {title}")))
            .child(div().flex_1())
            .child(
                Button::new("tree-close", "Back to grid")
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| this.close_tree(cx))),
            );

        let tree = Tree::new("relation-tree")
            .rows(items)
            .row_height(px(24.))
            .indent(px(14.))
            .track_scroll(&view.scroll)
            .focus_handle(view.focus.clone())
            .selected(selected_ix)
            .disclosure(|expanded, _window, cx| {
                let name = if expanded { "chevron-down" } else { "chevron" };
                crate::icons::icon(name, cx.theme().scale(12.), cx.theme().text_faint)
                    .into_any_element()
            })
            .render_row(move |ix, _window, cx| render_row(&rows_render[ix], cx))
            .on_select(move |ix, _event, _window, cx| {
                if rows_select[ix].node_id.is_some() {
                    let flat = rows_select.clone();
                    sv.update(cx, |this, cx| this.tree_select_ix(&flat, ix, cx))
                        .ok();
                }
            })
            .on_toggle(move |ix, _window, cx| {
                if let Some(node) = rows_toggle[ix].node_id {
                    tv.update(cx, |this, cx| this.tree_toggle(node, cx)).ok();
                }
            })
            // Double-click / Enter acts (Tier 1): relation → open in grid, record →
            // re-root; the synthetic "Load more" row grows its relation's page.
            .on_activate(move |ix, _window, cx| {
                let row = &rows_activate[ix];
                if let Some(node) = row.more_for {
                    av.update(cx, |this, cx| this.tree_load_more(node, cx)).ok();
                } else if let Some(node) = row.node_id {
                    av.update(cx, |this, cx| this.tree_activate(node, cx)).ok();
                }
            })
            .on_nav(move |nav, _window, cx| {
                nv.update(cx, |this, cx| this.tree_nav(nav, cx)).ok();
            })
            .on_secondary(move |ix, pos, _window, cx| {
                let node = rows_sec[ix].node_id;
                gv.update(cx, |this, cx| this.open_tree_menu(node, pos, cx))
                    .ok();
            });

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(bg)
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .font_family(theme.mono_family.clone())
                    .text_size(s12)
                    .text_color(text)
                    .child(tree),
            )
            .children(menu_overlay)
            .into_any_element()
    }
}

/// Render one flattened tree row by its content kind.
fn render_row(row: &VisibleRow, cx: &mut gpui::App) -> AnyElement {
    let theme = cx.theme();
    let (text, muted, faint, accent, cyan) = (
        theme.text,
        theme.text_muted,
        theme.text_faint,
        theme.accent,
        theme.cyan,
    );
    match &row.content {
        RowContent::Record(summary) => div()
            .flex()
            .items_center()
            .gap_1p5()
            .child(crate::icons::icon("table", theme.scale(13.), muted))
            .child(div().text_color(text).child(summary.clone()))
            .into_any_element(),
        RowContent::Relation { label, count } => {
            let mut r = div()
                .flex()
                .items_center()
                .gap_1p5()
                .child(crate::icons::icon("link", theme.scale(12.), accent))
                .child(div().text_color(cyan).child(label.clone()));
            if let Some(count) = count {
                r = r.child(div().text_color(faint).child(count.clone()));
            }
            r.into_any_element()
        }
        RowContent::Loading => div().text_color(faint).child("loading…").into_any_element(),
        RowContent::Empty => div()
            .text_color(faint)
            .child("(no related rows)")
            .into_any_element(),
        RowContent::Failed(msg) => div()
            .text_color(theme.red)
            .child(msg.clone())
            .into_any_element(),
        RowContent::More => div()
            .text_color(accent)
            .child("Load more…")
            .into_any_element(),
    }
}

/// The two toggle actions, extracted so the node's `kind` borrow is dropped before
/// the node is mutated.
enum Toggle {
    Build {
        schema: String,
        table: String,
        columns: Rc<Vec<ResultColumn>>,
        values: Rc<Vec<Value>>,
        need: bool,
    },
    Fetch {
        base_sql: String,
        filter: ResultFilter,
        need: bool,
    },
}

/// A pending "open row as tree": the focused row's full re-fetch is in flight; on
/// arrival the root record is built (see [`AppState::on_tree_root_rows`]).
pub(crate) struct PendingTreeRoot {
    pub(crate) id: u64,
    epoch: u64,
    row: usize,
    schema: String,
    table: String,
    columns: Vec<ResultColumn>,
}
