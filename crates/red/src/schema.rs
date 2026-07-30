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
    /// One object-kind group inside a namespace ("Tables", "Functions"). The tier
    /// between a namespace and its objects; expanding a *lazy* group is what
    /// fetches it (see `Command::LoadObjectGroup`).
    Group {
        schema: String,
        kind: ObjectKind,
    },
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
    /// Relations only: the columnless kinds live in [`Self::groups`].
    pub schemas: Vec<SchemaMeta>,
    /// Which object kinds this engine has, in tree order
    /// (`DbKind::object_kinds`). Held here so `flatten` stays a pure function of
    /// the schema state and does not need the connection config threaded in.
    pub kinds: &'static [ObjectKind],
    /// Lazily-loaded programmatic objects, keyed by (namespace, kind). Absent =
    /// never expanded; present-and-empty = the engine reported none, which is a
    /// real answer and renders as an empty group rather than a spinner.
    pub groups: HashMap<(String, ObjectKind), Vec<red_core::ObjectMeta>>,
    /// Groups with a `LoadObjectGroup` in flight, so a second expand does not
    /// fire a second fetch and the row can show "loading…".
    pub groups_loading: HashSet<(String, ObjectKind)>,
    /// How many objects each lazy kind holds per namespace, from one query at
    /// connect (`Event::ObjectCountsReady`). Absent while `counts_loaded` is
    /// false; absent *after* it is true means zero.
    counts: HashMap<(String, ObjectKind), usize>,
    /// Whether the engine answered the count query at all. Without it a missing
    /// entry would be ambiguous between "none" and "not asked", and the tree
    /// would hide groups it has no business hiding.
    counts_loaded: bool,
    /// Namespaces whose default group expansion has already been applied (see
    /// [`Self::apply_objects`]). A refresh re-runs `apply_objects`, and without
    /// this it would re-open a group the user had deliberately collapsed.
    seeded: HashSet<String>,
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
    /// The open right-click menu (which node, and where to draw it), or `None`.
    pub menu: Option<SchemaMenu>,
}

/// An open schema-tree context menu: the node it acts on and the screen position
/// to anchor it at. Mirrors the Redis key menu's shape (`kvbrowse`'s `KeyMenu`).
pub(crate) struct SchemaMenu {
    pub node: NodeId,
    pub pos: gpui::Point<gpui::Pixels>,
}

impl SchemaState {
    /// `kind` fixes which object groups this tree can ever draw
    /// (`DbKind::object_kinds`), read once at connect because a connection's
    /// engine never changes under it.
    pub fn new(kind: DbKind, cx: &mut Context<AppState>) -> Self {
        let filter = cx.new(|cx| {
            TextInput::new(cx).with_placeholder(crate::i18n::tr!("schema.filter", "Filter schema…"))
        });
        // Re-render so the filter narrows the tree live as the user types.
        cx.subscribe(&filter, |_this, _input, _evt: &TextInputEvent, cx| {
            cx.notify()
        })
        .detach();
        Self {
            schemas: Vec::new(),
            kinds: kind.object_kinds(),
            groups: HashMap::new(),
            groups_loading: HashSet::new(),
            counts: HashMap::new(),
            counts_loaded: false,
            seeded: HashSet::new(),
            details: HashMap::new(),
            expanded: HashSet::new(),
            selected: None,
            filter,
            tree_scroll: UniformListScrollHandle::new(),
            loading: true,
            menu: None,
        }
    }

    /// Install the loaded skeleton. A lone namespace auto-expands so the user
    /// lands directly on the table list (the common SQLite `main` case), and a
    /// namespace whose only populated relation group is Tables opens that group,
    /// so grouping costs the common case no extra click.
    ///
    /// Both are **seeded defaults, not invariants**: they write into `expanded`
    /// once and `flatten` never re-asserts them, so a group the user collapses
    /// stays collapsed. Seeding the group default straight into the open test was
    /// the bug that made Tables uncollapsable; `seeded` is what keeps a refresh
    /// (which calls this again) from re-opening it.
    pub fn apply_objects(&mut self, schemas: Vec<SchemaMeta>) {
        if schemas.len() == 1 {
            self.expanded
                .insert(NodeId::Schema(schemas[0].name.clone()));
        }
        for meta in &schemas {
            // `insert` returns false for a namespace already defaulted, which is
            // every namespace on every refresh after the first.
            if !self.seeded.insert(meta.name.clone()) {
                continue;
            }
            if let Some(kind) = default_open_group(self.kinds, meta) {
                self.expanded.insert(NodeId::Group {
                    schema: meta.name.clone(),
                    kind,
                });
            }
        }
        self.schemas = schemas;
        self.loading = false;
    }

    /// Install the per-namespace object counts.
    pub fn apply_object_counts(&mut self, counts: Vec<(String, ObjectKind, usize)>) {
        self.counts = counts
            .into_iter()
            .map(|(ns, kind, n)| ((ns, kind), n))
            .collect();
        self.counts_loaded = true;
    }

    /// How many objects of `kind` the namespace holds, when that is known: from
    /// the fetched contents if they are in hand, otherwise from the connect-time
    /// count. `None` means genuinely unknown, which only happens on an engine
    /// that could not answer the count query.
    ///
    /// The contents win when both exist: they are newer, and asserting the two
    /// agree would fail for a routine dropped between connect and expand.
    fn known_count(&self, namespace: &str, kind: ObjectKind) -> Option<usize> {
        let key = (namespace.to_string(), kind);
        if let Some(objects) = self.groups.get(&key) {
            return Some(objects.len());
        }
        self.counts_loaded
            .then(|| self.counts.get(&key).copied().unwrap_or(0))
    }

    /// Install one expanded group's objects. An empty list is stored, not
    /// discarded: "loaded and empty" is what stops the row spinning.
    pub fn apply_object_group(
        &mut self,
        namespace: String,
        kind: ObjectKind,
        objects: Vec<red_core::ObjectMeta>,
    ) {
        self.groups_loading.remove(&(namespace.clone(), kind));
        self.groups.insert((namespace, kind), objects);
    }

    /// The relations of one namespace that belong to `kind`. Relations live in
    /// the skeleton, so this is a filter rather than a lookup.
    fn relations<'a>(
        &self,
        schema: &'a SchemaMeta,
        kind: ObjectKind,
    ) -> Vec<&'a red_core::ObjectMeta> {
        schema.objects.iter().filter(|o| o.kind == kind).collect()
    }

    /// Every object of `kind` in `schema`, from whichever tier holds it.
    fn objects_of<'a>(
        &'a self,
        schema: &'a SchemaMeta,
        kind: ObjectKind,
    ) -> Vec<&'a red_core::ObjectMeta> {
        if kind.is_relation() {
            self.relations(schema, kind)
        } else {
            self.groups
                .get(&(schema.name.clone(), kind))
                .map(|v| v.iter().collect())
                .unwrap_or_default()
        }
    }
}

/// The relation group a namespace should open by default: the only populated
/// one, when there is exactly one.
///
/// `None` when a namespace has several populated groups (opening one would be an
/// arbitrary choice) or none at all. A free function over plain data, so the rule
/// is unit-testable without a GPUI context, which `SchemaState` needs.
fn default_open_group(kinds: &[ObjectKind], meta: &SchemaMeta) -> Option<ObjectKind> {
    let mut populated = kinds
        .iter()
        .copied()
        .filter(|k| k.is_relation() && meta.objects.iter().any(|o| o.kind == *k));
    match (populated.next(), populated.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// What opening `obj`'s row does. A relation has rows, so it browses; everything
/// else — a trigger, routine, sequence or type — has only a definition, so that is
/// what the row opens, and it is the only thing its menu offers either.
///
/// A free function over plain data for the same reason as [`default_open_group`]:
/// `flatten` needs a `SchemaState`, which needs a GPUI context, so the rule is only
/// testable out here.
fn row_open(namespace: &str, obj: &red_core::ObjectMeta) -> RowOpen {
    let (schema, name) = (namespace.to_string(), obj.name.clone());
    if obj.kind.is_relation() {
        RowOpen::Browse { schema, name }
    } else {
        RowOpen::Ddl {
            schema,
            name,
            kind: obj.kind,
        }
    }
}

/// What a flattened visible row carries for rendering.
enum RowContent {
    Schema {
        name: String,
        count: usize,
    },
    /// An object-kind group row ("Tables · 42"). `count` is `None` for a lazy
    /// group that has not been fetched: showing one would mean counting at
    /// connect, which is exactly the cost the lazy tier exists to avoid.
    Group {
        kind: ObjectKind,
        count: Option<usize>,
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
/// RED renders, the node's identity (for toggle/select), and what opening the row
/// does.
struct VisibleRow {
    item: TreeItem,
    content: RowContent,
    node: Option<NodeId>,
    open: Option<RowOpen>,
}

/// What opening a row does — a body click, Enter, or a double-click. Only an object
/// row has anything to open, and *which* thing depends on the kind: a relation has
/// rows to browse, while a routine or trigger has only its definition to read.
/// Carried on the row so the click sites don't re-derive the kind.
#[derive(Clone)]
enum RowOpen {
    /// `SELECT * FROM schema.name` in a new tab.
    Browse { schema: String, name: String },
    /// The object's `CREATE` statement in a read-only tab.
    Ddl {
        schema: String,
        name: String,
        kind: ObjectKind,
    },
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

        // Does anything under this namespace survive the filter? Checked across
        // both tiers (skeleton relations and any group already fetched), so a
        // namespace whose only match is a loaded function still shows.
        let any_match = !filtering
            || schema_match
            || s.kinds.iter().any(|&kind| {
                s.objects_of(schema, kind).iter().any(|obj| {
                    hit(&obj.name)
                        || s.details
                            .get(&(schema.name.clone(), obj.name.clone()))
                            .is_some_and(|d| d.columns.iter().any(|c| hit(&c.name)))
                })
            });
        if !any_match {
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
            open: None,
        });
        if !schema_open {
            continue;
        }

        for &kind in s.kinds {
            let members = s.objects_of(schema, kind);
            let loaded = kind.is_relation() || s.groups.contains_key(&(schema.name.clone(), kind));
            // What the namespace holds, when it is known: the fetched contents,
            // else the connect-time count. Relations are always known, since
            // their names are in the skeleton.
            let known = if kind.is_relation() {
                Some(members.len())
            } else {
                s.known_count(&schema.name, kind)
            };
            // Empty and known so *in advance*: never drawn. This is the whole
            // point of the counts. A row that advertises content it does not have
            // and then deflates under the cursor is worse than no row.
            if known == Some(0) && !loaded {
                continue;
            }
            // A relation group's emptiness is always known in advance, since its
            // names are in the skeleton.
            if kind.is_relation() && members.is_empty() {
                continue;
            }
            // Emptiness discovered by *expanding* (an engine that answered no
            // counts) keeps its row for the rest of the session, dimmed and
            // labelled "none". Removing it here would make the row vanish under
            // the cursor that just clicked it, which is its own small betrayal.

            let group_node = NodeId::Group {
                schema: schema.name.clone(),
                kind,
            };
            // A filter reveals through groups, so a match deep in a loaded group
            // is visible without hand-expanding each tier. Nothing else forces a
            // group open: the default expansion is seeded once into `expanded`
            // (see `apply_objects`), so collapsing one sticks.
            let group_open = (filtering && loaded) || s.expanded.contains(&group_node);

            // The group's own rows after the filter, so an all-miss group can be
            // skipped rather than drawn as an empty expanded folder.
            //
            // Built only when it will actually be read. A collapsed, unfiltered
            // group emits no member rows, and unfiltered the "filtered" list is
            // every member anyway — so materializing it was a `Vec` plus a tuple per
            // object, for every group in every namespace, on every frame. Flint's
            // `Tree` is `uniform_list`-backed, so rendering is already O(visible);
            // this is what kept the *flatten* O(total).
            let needs_rows = group_open || filtering;
            let group_rows: Vec<_> = if needs_rows {
                members
                    .iter()
                    .filter_map(|obj| {
                        let obj_match = !filtering || hit(&obj.name);
                        let col_hit = filtering
                            && s.details
                                .get(&(schema.name.clone(), obj.name.clone()))
                                .is_some_and(|d| d.columns.iter().any(|c| hit(&c.name)));
                        (!filtering || schema_match || obj_match || col_hit)
                            .then_some((*obj, obj_match, col_hit))
                    })
                    .collect()
            } else {
                Vec::new()
            };
            // Unfiltered, every member survives, so "nothing left after the filter"
            // is exactly "no members".
            let empty_after_filter = if needs_rows {
                group_rows.is_empty()
            } else {
                members.is_empty()
            };
            if filtering && !schema_match && empty_after_filter {
                continue;
            }

            // A loaded group that filtered down to nothing is still a leaf: it has
            // no rows to disclose *right now*, and a chevron over nothing reads as
            // a group that closed itself.
            let no_rows = loaded && empty_after_filter;
            out.push(VisibleRow {
                item: if no_rows {
                    TreeItem::leaf(1)
                } else {
                    TreeItem::new(1, true, group_open)
                },
                content: RowContent::Group { kind, count: known },
                node: Some(group_node),
                open: None,
            });
            if !group_open || no_rows {
                continue;
            }
            // Expanded but still in flight: one placeholder row, same shape the
            // column list uses while `DescribeTable` is outstanding.
            if !loaded {
                out.push(VisibleRow {
                    item: TreeItem::leaf(2),
                    content: RowContent::Loading,
                    node: None,
                    open: None,
                });
                continue;
            }

            for (obj, obj_match, col_hit) in group_rows {
                let obj_node = NodeId::Object {
                    schema: schema.name.clone(),
                    name: obj.name.clone(),
                };
                // Force open to reveal the matching columns when only a column hit.
                let force = filtering && !schema_match && !obj_match && col_hit;
                let obj_open = s.expanded.contains(&obj_node) || force;
                // Only a relation has columns to expand into, and only a relation
                // can be browsed: opening a trigger row would build a
                // `SELECT * FROM <trigger>`. What a routine or trigger has instead
                // is its definition, so that is what opening one does.
                let expandable = obj.kind.is_relation();
                out.push(VisibleRow {
                    item: TreeItem::new(2, expandable, obj_open && expandable),
                    content: RowContent::Object {
                        kind: obj.kind,
                        name: obj.name.clone(),
                    },
                    node: Some(obj_node),
                    open: Some(row_open(&schema.name, obj)),
                });
                if !obj_open || !expandable {
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
                                item: TreeItem::leaf(3),
                                content: RowContent::Column {
                                    meta: col.clone(),
                                    is_fk,
                                },
                                node: Some(NodeId::Column {
                                    schema: schema.name.clone(),
                                    table: obj.name.clone(),
                                    name: col.name.clone(),
                                }),
                                open: None,
                            });
                        }
                    }
                    None => out.push(VisibleRow {
                        item: TreeItem::leaf(3),
                        content: RowContent::Loading,
                        node: None,
                        open: None,
                    }),
                }
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

/// The icon name + colour one [`ObjectKind`] is drawn with, shared by the tree,
/// the group nodes, and any other surface that lists objects, so a function is
/// never a table in one place and a routine in another.
pub(crate) fn object_icon(kind: ObjectKind, cx: &App) -> (&'static str, gpui::Hsla) {
    let theme = cx.theme();
    match kind {
        ObjectKind::Table => ("table", theme.text_muted),
        ObjectKind::View => ("view", theme.cyan),
        ObjectKind::MaterializedView => ("matview", theme.blue),
        ObjectKind::Function => ("function", theme.purple),
        ObjectKind::Procedure => ("procedure", theme.purple),
        ObjectKind::Trigger => ("trigger", theme.orange),
        ObjectKind::Sequence => ("sequence", theme.green),
        ObjectKind::Type => ("udt", theme.yellow),
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

        RowContent::Group { kind, count } => {
            let (name_icon, color) = object_icon(*kind, cx);
            // A group known to hold nothing dims itself, icon included, so a
            // namespace's empty kinds recede instead of reading as unexplored.
            let empty = *count == Some(0);
            let (label_color, icon_color) = if empty {
                (faint, faint)
            } else {
                (muted, color)
            };
            let mut row = div()
                .flex()
                .flex_1()
                .items_center()
                .gap_1p5()
                .child(crate::icons::icon(name_icon, theme.scale(13.), icon_color))
                .child(
                    div()
                        .text_size(theme.scale(11.5))
                        .text_color(label_color)
                        .child(kind.group_label()),
                );
            if let Some(n) = count {
                row = row.child(
                    div()
                        .ml_auto()
                        .font_family(theme.font_family.clone())
                        .text_size(theme.scale(10.))
                        .text_color(faint)
                        // "0" next to a folder invites a click that does nothing;
                        // "none" says the question has already been answered.
                        .child(if empty {
                            "none".to_string()
                        } else {
                            n.to_string()
                        }),
                );
            }
            row.into_any_element()
        }

        RowContent::Object { kind, name } => {
            let (name_icon, color) = object_icon(*kind, cx);
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
            .child(crate::i18n::tr!("common.loading", "loading…"))
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

        // Just the filter. The ER diagram used to have an icon button here, from
        // before the tree had a right-click menu; the menu's per-database item is
        // both more discoverable and correctly scoped, so the button was a second
        // way to do a worse version of the same thing.
        let filter_row = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .pt_2()
            .pb_1()
            .child(div().flex_1().child(s.filter.clone()));

        // Capture the flattened rows per handler so each can map its click index
        // back to the node it represents.
        let rows_render = rows.clone();
        let rows_toggle = rows.clone();
        let rows_select = rows.clone();
        let rows_activate = rows.clone();
        let rows_secondary = rows.clone();
        let (tv, sv, av, nv) = (view.clone(), view.clone(), view.clone(), view.clone());
        let cv = view.clone();

        let tree = Tree::new("schema-tree")
            .rows(items)
            .row_height(px(24.))
            .indent(px(14.))
            .track_scroll(&s.tree_scroll)
            // Keyboard navigation: the sidebar's focus handle lives on the tree,
            // and ↑/↓ / ←/→ / Enter intents drive selection, expansion, and preview.
            .focus_handle(active.schema_focus.clone())
            .vim_nav(self.vim_mode())
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
            // tab, a routine or trigger opens its definition, a namespace folder
            // expands/collapses, a column just highlights. (The chevron owns
            // expansion for tables, so revealing a table's columns doesn't open it
            // — see the Flint `Tree` chevron hit target.)
            .on_select(move |ix, _event, _window, cx| {
                let node = rows_select[ix].node.clone();
                let open = rows_select[ix].open.clone();
                sv.update(cx, |this, cx| match node {
                    Some(NodeId::Object { .. }) => {
                        if let Some(open) = open {
                            this.schema_open_row(open, cx);
                        }
                    }
                    // A folder row (namespace or object group) has nothing to
                    // open, so a body click is the expand it looks like.
                    Some(node @ (NodeId::Schema(_) | NodeId::Group { .. })) => {
                        this.schema_select(node.clone(), cx);
                        this.schema_toggle(node, cx);
                    }
                    Some(node) => this.schema_select(node, cx),
                    None => {}
                })
                .ok();
            })
            .on_activate(move |ix, _window, cx| {
                if let Some(open) = rows_activate[ix].open.clone() {
                    av.update(cx, |this, cx| this.schema_open_row(open, cx))
                        .ok();
                }
            })
            // Right-click opens the node's action menu. A column row has no
            // actions of its own, so it falls through and opens nothing.
            .on_secondary(move |ix, pos, _window, cx| {
                if let Some(node) = rows_secondary[ix].node.clone()
                    && !matches!(node, NodeId::Column { .. })
                {
                    cv.update(cx, |this, cx| this.schema_open_menu(node, pos, cx))
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
        // The right-click menu is deliberately NOT a child here: it renders at
        // the shell root (see `render_schema_menu`), because this pane is the
        // narrow sidebar and would both clip the menu and offset its position.
    }

    /// The right-click menu for one tree node. A namespace row offers the
    /// namespace actions (the reason this menu exists: pointing a query at a
    /// database without hand-qualifying every name); an object row offers the
    /// browse/copy actions that were previously reachable only by clicking.
    ///
    /// Rendered from the **shell root**, not from the schema pane. `SchemaMenu.pos`
    /// is a window coordinate (Flint's `Tree::on_secondary` hands over
    /// `event.position`), so anchoring it inside the sidebar offset it by the
    /// pane's origin *and* let the narrow pane clip it. `floating` resolves both:
    /// it defers the surface (escaping the pane's clip) and fits it to the window.
    pub(crate) fn render_schema_menu(
        &self,
        active: &ActiveConn,
        menu: &SchemaMenu,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let pos = menu.pos;
        let caps = active.config.kind.namespace_caps();
        let mut items = ContextMenu::new("schema-context-menu");

        match &menu.node {
            NodeId::Schema(name) => {
                let ns = name.clone();
                items = items.item(
                    ContextMenuItem::new("schema-new-query", "New query here").on_click(
                        cx.listener({
                            let ns = ns.clone();
                            move |this, _, window, cx| {
                                this.schema_close_menu(cx);
                                this.schema_new_query_in(ns.clone(), window, cx);
                            }
                        }),
                    ),
                );
                // Only offered where RED can actually rebind the namespace; on
                // SQLite/Postgres the item would be a lie (see `namespace_caps`).
                if caps.settable {
                    let is_active = active.namespace.as_deref() == Some(ns.as_str());
                    let label = if is_active {
                        format!("✓ Active {}", caps.label.to_lowercase())
                    } else {
                        format!("Set as active {}", caps.label.to_lowercase())
                    };
                    items = items.item(
                        ContextMenuItem::new("schema-set-namespace", label).on_click(cx.listener(
                            {
                                let ns = ns.clone();
                                move |this, _, _, cx| {
                                    this.schema_close_menu(cx);
                                    this.set_active_namespace(Some(ns.clone()), cx);
                                }
                            },
                        )),
                    );
                }
                // Scoped to *this* database: the diagram maps what was right-clicked,
                // not every database on the connection.
                items = items.separator().item(
                    ContextMenuItem::new("schema-er-diagram", "ER diagram").on_click(cx.listener(
                        {
                            let ns = ns.clone();
                            move |this, _, _, cx| {
                                this.schema_close_menu(cx);
                                this.open_er_diagram(Some(ns.clone()), cx);
                            }
                        },
                    )),
                );
                items = items.item(
                    ContextMenuItem::new("schema-copy-name", "Copy name").on_click(cx.listener({
                        let ns = ns.clone();
                        move |this, _, _, cx| {
                            this.schema_close_menu(cx);
                            this.copy_to_clipboard(ns.clone(), "Name copied", cx);
                        }
                    })),
                );
            }
            NodeId::Object { schema, name } => {
                let (sc, nm) = (schema.clone(), name.clone());
                // The kind decides which actions make sense: a routine cannot be
                // browsed, and its definition is the only thing there is to see.
                let kind = active
                    .schema
                    .schemas
                    .iter()
                    .find(|ns| ns.name == *schema)
                    .and_then(|ns| ns.objects.iter().find(|o| o.name == *name))
                    .map(|o| o.kind)
                    .or_else(|| {
                        active.schema.groups.iter().find_map(|((ns, k), objs)| {
                            (ns == schema && objs.iter().any(|o| o.name == *name)).then_some(*k)
                        })
                    })
                    .unwrap_or(ObjectKind::Table);
                if kind.is_relation() {
                    items = items.item(ContextMenuItem::new("schema-browse", "Browse").on_click(
                        cx.listener({
                            let (sc, nm) = (sc.clone(), nm.clone());
                            move |this, _, _, cx| {
                                this.schema_close_menu(cx);
                                this.schema_preview(sc.clone(), nm.clone(), cx);
                            }
                        }),
                    ));
                }
                items = items.item(ContextMenuItem::new("schema-ddl", "Show DDL").on_click(
                    cx.listener({
                        let (sc, nm) = (sc.clone(), nm.clone());
                        move |this, _, _, cx| {
                            this.schema_close_menu(cx);
                            this.open_object_ddl(sc.clone(), nm.clone(), kind, cx);
                        }
                    }),
                ));
                // "New query here" is a *relation's* affordance: on a table it reads
                // as "query this", which is what the user wants next. On a trigger or
                // routine the same item opens a blank tab on the namespace, which
                // isn't what the row is about — the definition is, and Show DDL above
                // covers that. Same split the group rows already make.
                if kind.is_relation() {
                    items = items.item(
                        ContextMenuItem::new("schema-obj-new-query", "New query here").on_click(
                            cx.listener({
                                let sc = sc.clone();
                                move |this, _, window, cx| {
                                    this.schema_close_menu(cx);
                                    this.schema_new_query_in(sc.clone(), window, cx);
                                }
                            }),
                        ),
                    );
                }
                items = items.separator().item(
                    ContextMenuItem::new("schema-copy-qualified", "Copy qualified name").on_click(
                        cx.listener({
                            let (sc, nm) = (sc.clone(), nm.clone());
                            move |this, _, _, cx| {
                                this.schema_close_menu(cx);
                                let qualified = format!("{sc}.{nm}");
                                this.copy_to_clipboard(qualified, "Name copied", cx);
                            }
                        }),
                    ),
                );
            }
            NodeId::Group { schema, kind } => {
                // A lazy group is a cached fetch, so it is the one tree node with
                // something to refresh: a routine created since the group was
                // expanded is otherwise invisible until reconnect.
                let (sc, k) = (schema.clone(), *kind);
                if k.is_lazy() {
                    items = items.item(
                        ContextMenuItem::new("schema-group-refresh", "Refresh").on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.schema_close_menu(cx);
                                this.schema_reload_group(sc.clone(), k, cx);
                            }),
                        ),
                    );
                } else {
                    let ns = sc.clone();
                    items = items.item(
                        ContextMenuItem::new("schema-group-new-query", "New query here").on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.schema_close_menu(cx);
                                this.schema_new_query_in(ns.clone(), window, cx);
                            }),
                        ),
                    );
                }
            }
            // Filtered out before the menu opens.
            NodeId::Column { .. } => {}
        }

        // Full-bleed catcher dismisses on any outside click; `occlude()` keeps a
        // press on the menu itself from reaching it (see the Redis key menu).
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| this.schema_close_menu(cx)),
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _, _, cx| this.schema_close_menu(cx)),
            )
            .child(floating(div().occlude().child(items)).at(pos))
    }

    // --- tree interactions ---

    /// Toggle a node's expansion. Expanding an object whose detail isn't cached
    /// fires a lazy `DescribeTable`.
    pub(crate) fn schema_toggle(&mut self, node: NodeId, cx: &mut Context<Self>) {
        let mut describe = None;
        let mut load_group = None;
        if let Phase::Connected(active) = &mut self.phase {
            let s = &mut active.schema;
            if !s.expanded.remove(&node) {
                match &node {
                    NodeId::Object { schema, name }
                        if !s.details.contains_key(&(schema.clone(), name.clone())) =>
                    {
                        describe = Some((schema.clone(), name.clone()));
                    }
                    // Expanding a lazy group is what fetches it. Guarded on both
                    // the cache and the in-flight set, so collapsing and
                    // re-expanding does not re-query, and a slow catalog does not
                    // collect one request per impatient click.
                    NodeId::Group { schema, kind }
                        if kind.is_lazy()
                            && !s.groups.contains_key(&(schema.clone(), *kind))
                            && s.groups_loading.insert((schema.clone(), *kind)) =>
                    {
                        load_group = Some((schema.clone(), *kind));
                    }
                    _ => {}
                }
                s.expanded.insert(node);
            }
        }
        if let Some((schema, table)) = describe {
            self.send_active(Command::DescribeTable { schema, table });
        }
        if let Some((namespace, kind)) = load_group {
            self.send_active(Command::LoadObjectGroup { namespace, kind });
        }
        cx.notify();
    }

    /// Drop one lazy group's cache and re-fetch it, keeping the node expanded so
    /// the refresh reads as a reload rather than a collapse.
    pub(crate) fn schema_reload_group(
        &mut self,
        namespace: String,
        kind: ObjectKind,
        cx: &mut Context<Self>,
    ) {
        if let Phase::Connected(active) = &mut self.phase {
            let s = &mut active.schema;
            s.groups.remove(&(namespace.clone(), kind));
            s.groups_loading.insert((namespace.clone(), kind));
            s.expanded.insert(NodeId::Group {
                schema: namespace.clone(),
                kind,
            });
        }
        self.send_active(Command::LoadObjectGroup { namespace, kind });
        cx.notify();
    }

    pub(crate) fn schema_select(&mut self, node: NodeId, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.schema.selected = Some(node);
        }
        cx.notify();
    }

    /// Open the right-click menu on `node`, anchored at the click position. Also
    /// selects the row, so the menu visibly belongs to what was clicked.
    pub(crate) fn schema_open_menu(
        &mut self,
        node: NodeId,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Phase::Connected(active) = &mut self.phase {
            active.schema.selected = Some(node.clone());
            active.schema.menu = Some(SchemaMenu { node, pos });
        }
        cx.notify();
    }

    pub(crate) fn schema_close_menu(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase
            && active.schema.menu.take().is_some()
        {
            cx.notify();
        }
    }

    /// Set the connection-level namespace every new tab (and any tab without its
    /// own override) resolves unqualified names against.
    ///
    /// Re-runs nothing: an already-open result keeps the namespace it was opened
    /// with, which is what the backend stores per result. The change takes effect
    /// on the next query, so switching can't silently repoint rows on screen.
    pub(crate) fn set_active_namespace(
        &mut self,
        namespace: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Phase::Connected(active) = &mut self.phase else {
            return;
        };
        if active.namespace == namespace {
            return;
        }
        active.namespace = namespace.clone();
        // A tab-level override would mask the change and make the chip look
        // ignored, so adopting a connection namespace clears the overrides.
        for tab in &mut active.tabs {
            tab.namespace = None;
        }
        let label = active.config.kind.namespace_caps().label.to_lowercase();
        if let Some(ns) = namespace {
            self.notify(ToastVariant::Success, format!("Active {label}: {ns}"), cx);
        }
        cx.notify();
    }

    /// Open/close the breadcrumb's database dropdown.
    pub(crate) fn toggle_namespace_menu(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.namespace_menu_open = !active.namespace_menu_open;
            cx.notify();
        }
    }

    pub(crate) fn close_namespace_menu(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase
            && std::mem::take(&mut active.namespace_menu_open)
        {
            cx.notify();
        }
    }

    /// Point the *focused tab* at `namespace` (the breadcrumb picker). Scoped to the
    /// tab rather than the connection so a split view can hold two databases; the
    /// tree's "Set as active" is the connection-wide counterpart.
    pub(crate) fn set_tab_namespace(&mut self, namespace: Option<String>, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase
            && active.config.kind.namespace_caps().settable
            && let Some(tab) = active.active_mut()
        {
            tab.namespace = namespace;
        }
        cx.notify();
    }

    /// Open a blank query tab bound to `namespace` — the tree's "New query here".
    /// The tab carries its own override, so two tabs can sit on two databases.
    pub(crate) fn schema_new_query_in(
        &mut self,
        namespace: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_query(cx);
        if let Phase::Connected(active) = &mut self.phase
            && active.config.kind.namespace_caps().settable
            && let Some(tab) = active.active_mut()
        {
            tab.namespace = Some(namespace);
        }
        cx.notify();
    }

    /// Copy `text` and confirm with a toast; the tree's copy-name actions.
    pub(crate) fn copy_to_clipboard(
        &mut self,
        text: String,
        message: &'static str,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.notify(ToastVariant::Success, message, cx);
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
                if let Some(open) = row.open.clone() {
                    self.schema_open_row(open, cx);
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

    /// Open a tree row, whichever of the two that means for it (see [`RowOpen`]).
    /// The one place a click, an Enter, and a double-click agree on what opening a
    /// row does.
    fn schema_open_row(&mut self, open: RowOpen, cx: &mut Context<Self>) {
        match open {
            RowOpen::Browse { schema, name } => self.schema_preview(schema, name, cx),
            RowOpen::Ddl { schema, name, kind } => {
                // Highlight it like a browse does, so the tree agrees with the tab
                // that just opened.
                if let Phase::Connected(active) = &mut self.phase {
                    active.schema.selected = Some(NodeId::Object {
                        schema: schema.clone(),
                        name: name.clone(),
                    });
                }
                self.open_object_ddl(schema, name, kind, cx);
            }
        }
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
    /// FK click-through. The editor is pre-filled with the base SQL; a
    /// `filter` narrows the result without changing the shown query.
    pub(crate) fn open_table_browse(
        &mut self,
        schema: String,
        table: String,
        filter: Option<ResultFilter>,
        cx: &mut Context<Self>,
    ) {
        let (sql, label, table_ref, browsed_schema) = match &self.phase {
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
                (sql, label, (schema.clone(), table), schema)
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
            let tab = crate::app::QueryTab::new(label.clone(), self.active_dialect(), cx);
            self.push_tab(tab, cx);
        }
        // Browsing a table moves the tab into that table's database, so the
        // breadcrumb (`connection / database / table`) is true by construction and
        // a follow-up unqualified query lands where the user is visibly looking.
        // The generated SQL above is fully qualified either way; this is about the
        // *next* query the user types into this tab.
        if let Phase::Connected(active) = &mut self.phase
            && active.config.kind.namespace_caps().settable
            && let Some(tab) = active.active_mut()
        {
            tab.namespace = Some(browsed_schema);
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

#[cfg(test)]
mod group_default_tests {
    use super::*;
    use red_core::ObjectMeta;

    fn meta(name: &str, objects: &[(&str, ObjectKind)]) -> SchemaMeta {
        SchemaMeta {
            name: name.to_string(),
            objects: objects
                .iter()
                .map(|(n, k)| ObjectMeta {
                    name: n.to_string(),
                    kind: *k,
                })
                .collect(),
        }
    }

    const PG: &[ObjectKind] = &[
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Function,
    ];

    /// The common case: a namespace of plain tables opens its Tables group, so
    /// grouping costs nobody an extra click on the list RED always showed.
    #[test]
    fn a_namespace_of_only_tables_defaults_that_group_open() {
        let m = meta(
            "public",
            &[("a", ObjectKind::Table), ("b", ObjectKind::Table)],
        );
        assert_eq!(default_open_group(PG, &m), Some(ObjectKind::Table));
    }

    /// With more than one populated group, opening either would be an arbitrary
    /// choice, so the namespace opens nothing and the user picks.
    #[test]
    fn several_populated_groups_default_to_none_open() {
        let m = meta(
            "public",
            &[("a", ObjectKind::Table), ("v", ObjectKind::View)],
        );
        assert_eq!(default_open_group(PG, &m), None);
        let m = meta("public", &[("v", ObjectKind::View)]);
        assert_eq!(default_open_group(PG, &m), Some(ObjectKind::View));
        assert_eq!(default_open_group(PG, &meta("empty", &[])), None);
    }

    /// The lazily-loaded kinds are not counted: their contents are unknown at
    /// this point, so treating one as "the only populated group" would open a
    /// group RED cannot yet fill.
    #[test]
    fn lazy_kinds_never_win_the_default() {
        // A function in the skeleton is not a thing that happens, but the rule
        // must not depend on that: only relations are candidates.
        let m = meta("public", &[("f", ObjectKind::Function)]);
        assert_eq!(default_open_group(PG, &m), None);
    }

    /// Opening a row means the one thing the object has: rows for a relation, a
    /// definition for everything else. A trigger used to open nothing at all.
    #[test]
    fn opening_a_row_browses_a_relation_and_reads_everything_else() {
        let obj = |name: &str, kind| ObjectMeta {
            name: name.to_string(),
            kind,
        };
        for kind in [
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
        ] {
            assert!(
                matches!(row_open("public", &obj("t", kind)), RowOpen::Browse { .. }),
                "{kind:?} should browse"
            );
        }
        for kind in [
            ObjectKind::Trigger,
            ObjectKind::Function,
            ObjectKind::Procedure,
            ObjectKind::Sequence,
            ObjectKind::Type,
        ] {
            match row_open("public", &obj("x", kind)) {
                RowOpen::Ddl {
                    schema,
                    name,
                    kind: k,
                } => {
                    assert_eq!((schema.as_str(), name.as_str(), k), ("public", "x", kind));
                }
                RowOpen::Browse { .. } => panic!("{kind:?} has no rows to browse"),
            }
        }
    }
}
