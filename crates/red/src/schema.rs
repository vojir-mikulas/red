// SPDX-License-Identifier: GPL-3.0-or-later

//! The schema explorer (M3): the left-sidebar tree of namespaces → tables/views →
//! columns, plus the interim table preview rendered in the results pane.
//!
//! The generic, virtualized tree lives in Flint; the *domain* logic is here — the
//! schema model fetched over `Command`/`Event`, lazy column loading on expand,
//! the live name filter, and turning a double-clicked table into a read-only
//! `SELECT` preview. State hangs off [`ActiveConn`] so it lives for the connection
//! and dies on disconnect.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use flint::prelude::*;
use gpui::{div, prelude::*, px, App, Context, Entity};
use red_core::{ColumnMeta, DbKind, ObjectKind, SchemaMeta, TableDetail};
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase};
use crate::assets::FONT_MONO;

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
/// RED renders, the node's identity (for toggle/select), and — for an object —
/// the `(schema, table)` a double-click previews.
struct VisibleRow {
    item: TreeItem,
    content: RowContent,
    node: Option<NodeId>,
    preview: Option<(String, String)>,
}

/// Walk the schema model in display order into the currently-visible rows,
/// applying expansion and the name filter. Pure over the in-memory model — no
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
            .child(crate::icons::icon("schema", px(14.), muted))
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(text)
                    .child(name.clone()),
            )
            .child(
                div()
                    .ml_auto()
                    .font_family(FONT_MONO)
                    .text_size(px(10.))
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
                .child(crate::icons::icon(name_icon, px(14.), color))
                .child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(12.))
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
                .child(crate::icons::icon("col", px(13.), faint))
                .child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(11.5))
                        .text_color(muted)
                        .child(meta.name.clone()),
                );
            if let Some(type_name) = &meta.type_name {
                row = row.child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(10.))
                        .text_color(faint)
                        .child(type_name.clone()),
                );
            }
            if meta.primary_key {
                row = row.child(crate::icons::icon("key", px(12.), theme.yellow));
            }
            if *is_fk {
                row = row.child(crate::icons::icon("link", px(12.), theme.accent));
            }
            row.into_any_element()
        }

        RowContent::Loading => div()
            .text_size(px(11.))
            .text_color(faint)
            .child("loading…")
            .into_any_element(),
    }
}

/// Quote an identifier for the preview `SELECT` so a table name can never break
/// out of the SQL. MySQL/MariaDB use backticks (double quotes are string literals
/// there unless `ANSI_QUOTES` is set); SQLite/Postgres use the SQL-standard double
/// quote. Embedded quote chars are doubled either way.
fn quote_ident(ident: &str, kind: DbKind) -> String {
    match kind {
        DbKind::Mysql => format!("`{}`", ident.replace('`', "``")),
        _ => format!("\"{}\"", ident.replace('"', "\"\"")),
    }
}

impl AppState {
    /// The left-sidebar schema explorer: connection pill · filter · tree · footer.
    pub(crate) fn render_schema(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (bg_panel, bg_elevated, border, radius) = (
            theme.bg_panel,
            theme.bg_elevated,
            theme.border,
            theme.radius,
        );
        let (text, faint, green, yellow) =
            (theme.text, theme.text_faint, theme.green, theme.yellow);
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

        let pill = div()
            .flex_shrink_0()
            .mx_2()
            .mt_2()
            .mb_1()
            .h(px(26.))
            .flex()
            .items_center()
            .gap_1p5()
            .px_2()
            .rounded(radius)
            .bg(bg_elevated)
            .border_1()
            .border_color(border)
            .child(div().size(px(7.)).rounded_full().bg(green))
            .child(
                div()
                    .flex_1()
                    .font_family(FONT_MONO)
                    .text_size(px(12.))
                    .text_color(text)
                    .child(active.config.name.clone()),
            )
            .when(active.config.read_only, |d| {
                d.child(crate::icons::icon("lock", px(12.), yellow))
            });

        let filter_row = div().flex_shrink_0().px_2().pb_1().child(s.filter.clone());

        // Capture the flattened rows per handler so each can map its click index
        // back to the node it represents.
        let rows_render = rows.clone();
        let rows_toggle = rows.clone();
        let rows_select = rows.clone();
        let rows_activate = rows.clone();
        let (tv, sv, av) = (view.clone(), view.clone(), view.clone());

        let tree = Tree::new("schema-tree")
            .rows(items)
            .row_height(px(24.))
            .indent(px(14.))
            .selected(selected_ix)
            .render_row(move |ix, _window, cx| render_node(&rows_render[ix], cx))
            .on_toggle(move |ix, _window, cx| {
                if let Some(node) = rows_toggle[ix].node.clone() {
                    tv.update(cx, |this, cx| this.schema_toggle(node, cx)).ok();
                }
            })
            .on_select(move |ix, _event, _window, cx| {
                if let Some(node) = rows_select[ix].node.clone() {
                    sv.update(cx, |this, cx| this.schema_select(node, cx)).ok();
                }
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
            .font_family(FONT_MONO)
            .text_size(px(10.))
            .text_color(faint)
            .child(footer_text);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(bg_panel)
            .child(pill)
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
                if let NodeId::Object { schema, name } = &node {
                    if !s.details.contains_key(&(schema.clone(), name.clone())) {
                        describe = Some((schema.clone(), name.clone()));
                    }
                }
                s.expanded.insert(node);
            }
        }
        if let Some((schema, table)) = describe {
            self.service.send(Command::DescribeTable { schema, table });
        }
        cx.notify();
    }

    pub(crate) fn schema_select(&mut self, node: NodeId, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.schema.selected = Some(node);
        }
        cx.notify();
    }

    /// Preview a table/view: open `SELECT * FROM schema.table` in a **new** query
    /// tab so the user's current query and result are preserved. No `LIMIT` — the
    /// grid pages through it with flat memory. The new tab's editor is pre-filled.
    pub(crate) fn schema_preview(&mut self, schema: String, table: String, cx: &mut Context<Self>) {
        let (sql, label) = match &mut self.phase {
            Phase::Connected(active) => {
                let kind = active.config.kind;
                let sql = format!(
                    "SELECT * FROM {}.{}",
                    quote_ident(&schema, kind),
                    quote_ident(&table, kind)
                );
                let label = format!("{schema}.{table}");
                active.schema.selected = Some(NodeId::Object {
                    schema,
                    name: table,
                });
                (sql, label)
            }
            _ => return,
        };
        // Reuse the focused tab only if it's untouched; otherwise open a new one
        // so the user's current query and result are preserved.
        let reuse = matches!(&self.phase, Phase::Connected(a) if a.active().is_pristine(cx));
        if reuse {
            if let Phase::Connected(active) = &mut self.phase {
                active.active_mut().title = label.clone();
            }
        } else {
            let tab = crate::app::QueryTab::new(label.clone(), cx);
            self.push_tab(tab, cx);
        }
        let editor = match &self.phase {
            Phase::Connected(active) => active.active().editor.clone(),
            _ => return,
        };
        editor.update(cx, |editor, cx| editor.set_content(sql.clone(), cx));
        self.open_result(label, sql, cx);
    }
}
