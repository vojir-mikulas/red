//! The SQL editor pane: a toolbar (Run · history · read-only badge) over
//! Flint's `CodeEditor`. RED owns the domain bits: the SQL highlighter, the
//! completion candidates fed into the editor's generic completion seam, running
//! the current statement (or selection), and the query history. Results land in
//! the result grid.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use flint::prelude::*;
use gpui::{div, prelude::*, px, Context, Hsla, MouseButton, Pixels, Point, SharedString, Window};
use red_core::{DbKind, FkEdge, ObjectKind, SchemaMeta, TableDetail};
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase, TabCloseScope};
use crate::sql::CompletionContext;

/// How many candidates the popup ever shows; the editor renders at most 8, but
/// we hand it a few more so prefix-narrowing has headroom.
const MAX_CANDIDATES: usize = 20;

/// In-app drag payload for the tab strip: the source tab's index. A strip drop
/// reorders via [`AppState::drop_tab`]; a drop onto the *other* split half moves
/// the tab across via [`AppState::move_tab_to_half`].
#[derive(Clone, Copy)]
pub(crate) struct TabDrag(pub usize);

/// The floating chip rendered under the cursor while a tab is being dragged.
/// GPUI's `on_drag` wants an `Entity<impl Render>`, so the tab strip mints one
/// of these with the dragged tab's label.
pub(crate) struct TabDragPreview {
    pub(crate) title: SharedString,
    /// Grab offset within the tab, so the chip tracks the pointer (not the
    /// tab's top-left, where GPUI anchors the preview).
    pub(crate) offset: Point<Pixels>,
    pub(crate) bg: Hsla,
    pub(crate) border: Hsla,
    pub(crate) text: Hsla,
}

impl Render for TabDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div().pl(self.offset.x).pt(self.offset.y).child(
            div()
                .flex()
                .items_center()
                .h(px(28.))
                .px_2p5()
                .bg(self.bg)
                .border_1()
                .border_color(self.border)
                .rounded(px(4.))
                .font_family(theme.font_family.clone())
                .text_size(theme.scale(12.))
                .text_color(self.text)
                .child(self.title.clone()),
        )
    }
}

/// A schema column candidate: its `name` and declared `type` (empty when the
/// driver reports none), used to label and document column completions.
#[derive(Clone)]
struct ColumnCand {
    name: SharedString,
    ty: SharedString,
}

/// A schema object candidate: its `name` and whether it's a view (vs a table),
/// which picks the completion's detail/guide text.
#[derive(Clone)]
struct TableCand {
    name: SharedString,
    is_view: bool,
}

/// One join relationship available from a table: the `other` table it connects
/// to and the `(this_col, other_col)` pairs, oriented from *this* table's side so
/// the completion can spell `this.this_col = other.other_col`. Both directions of
/// every FK edge are recorded (a table finds relations whether it holds the key or
/// is pointed at); a composite key carries more than one pair.
#[derive(Clone)]
struct JoinRel {
    other: SharedString,
    pairs: Vec<(SharedString, SharedString)>,
}

/// The completion candidates derived from the loaded schema, grouped so the
/// provider can rank them by the cursor's context. Rebuilt as the schema grows.
struct CompletionIndex {
    /// Every object (table/view), sorted + deduped by name.
    tables: Vec<TableCand>,
    /// Columns keyed by lower-cased table name, for `table.`/`alias.` completion.
    columns_by_table: HashMap<String, Vec<ColumnCand>>,
    /// Every column across the schema, sorted + deduped by name.
    all_columns: Vec<ColumnCand>,
    /// Join relations keyed by lower-cased table name, from the connection's FK
    /// graph. Drives auto-`JOIN` completions and column relationship hints.
    joins_by_table: HashMap<String, Vec<JoinRel>>,
    /// Lower-cased names of every table/view — the always-loaded skeleton the
    /// diagnostics pass checks table existence against.
    table_names: HashSet<String>,
    /// Lower-cased names of every namespace (database/schema), so a table qualified
    /// by an unknown schema (a cross-database ref) is left unvalidated, not flagged.
    schema_names: HashSet<String>,
    /// Lower-cased column-name sets keyed by lower-cased table name, for tables
    /// whose detail is loaded. An absent entry means "not loaded yet", so column
    /// diagnostics for that table are skipped rather than firing false unknowns.
    columns_lower: HashMap<String, HashSet<String>>,
    /// The SQL functions available on *this connection's engine* (name, signature,
    /// guide), for completion + hover — already filtered by `DbKind`.
    functions: Vec<(&'static str, &'static str, &'static str)>,
    /// The upper-cased SQL keywords.
    keywords: Vec<SharedString>,
}

impl crate::sql::SchemaView for CompletionIndex {
    fn has_table(&self, table_lower: &str) -> bool {
        self.table_names.contains(table_lower)
    }
    fn columns(&self, table_lower: &str) -> Option<&HashSet<String>> {
        self.columns_lower.get(table_lower)
    }
    fn has_schema(&self, schema_lower: &str) -> bool {
        self.schema_names.contains(schema_lower)
    }
}

fn build_index(
    schemas: &[SchemaMeta],
    details: &HashMap<(String, String), TableDetail>,
    fks: &[FkEdge],
    kind: DbKind,
) -> CompletionIndex {
    let mut tables: Vec<TableCand> = Vec::new();
    for sc in schemas {
        for obj in &sc.objects {
            tables.push(TableCand {
                name: obj.name.clone().into(),
                is_view: matches!(obj.kind, ObjectKind::View),
            });
        }
    }

    let mut columns_by_table: HashMap<String, Vec<ColumnCand>> = HashMap::new();
    let mut all_columns: Vec<ColumnCand> = Vec::new();
    for ((_, table), detail) in details {
        let entry = columns_by_table.entry(table.to_lowercase()).or_default();
        for col in &detail.columns {
            let cand = ColumnCand {
                name: col.name.clone().into(),
                ty: col.type_name.clone().unwrap_or_default().into(),
            };
            entry.push(cand.clone());
            all_columns.push(cand);
        }
    }

    // Index every FK edge under both endpoints, orienting the column pairs from
    // that endpoint's side so `join_items` can spell the `ON` clause directly.
    let mut joins_by_table: HashMap<String, Vec<JoinRel>> = HashMap::new();
    for edge in fks {
        joins_by_table
            .entry(edge.from_table.to_lowercase())
            .or_default()
            .push(JoinRel {
                other: edge.to_table.clone().into(),
                pairs: edge
                    .columns
                    .iter()
                    .map(|(f, t)| (f.clone().into(), t.clone().into()))
                    .collect(),
            });
        joins_by_table
            .entry(edge.to_table.to_lowercase())
            .or_default()
            .push(JoinRel {
                other: edge.from_table.clone().into(),
                pairs: edge
                    .columns
                    .iter()
                    .map(|(f, t)| (t.clone().into(), f.clone().into()))
                    .collect(),
            });
    }

    tables.sort_by(|a, b| a.name.cmp(&b.name));
    tables.dedup_by(|a, b| a.name == b.name);
    all_columns.sort_by(|a, b| a.name.cmp(&b.name));
    all_columns.dedup_by(|a, b| a.name == b.name);
    for cols in columns_by_table.values_mut() {
        cols.sort_by(|a, b| a.name.cmp(&b.name));
        cols.dedup_by(|a, b| a.name == b.name);
    }

    let keywords = crate::sql::KEYWORDS
        .iter()
        .map(|kw| SharedString::from(kw.to_uppercase()))
        .collect();

    // Diagnostics lookups: the table skeleton (always loaded) and per-loaded-table
    // column-name sets, both lower-cased for case-insensitive checks.
    let table_names: HashSet<String> = tables.iter().map(|t| t.name.to_lowercase()).collect();
    let schema_names: HashSet<String> = schemas.iter().map(|s| s.name.to_lowercase()).collect();
    let columns_lower: HashMap<String, HashSet<String>> = columns_by_table
        .iter()
        .map(|(table, cols)| {
            (
                table.clone(),
                cols.iter().map(|c| c.name.to_lowercase()).collect(),
            )
        })
        .collect();

    CompletionIndex {
        tables,
        columns_by_table,
        all_columns,
        joins_by_table,
        table_names,
        schema_names,
        columns_lower,
        functions: crate::sql::functions_for(kind),
        keywords,
    }
}

/// The diagnostics provider handed to the editor's decoration seam: it runs the
/// schema-aware [`crate::sql::diagnostics`] pass against the live buffer each paint
/// and maps each finding to an error-styled wavy underline.
fn decoration_provider(
    index: Rc<CompletionIndex>,
) -> impl Fn(&str) -> Vec<flint::Decoration> + 'static {
    move |content| {
        crate::sql::diagnostics(content, index.as_ref())
            .into_iter()
            .map(|d| flint::Decoration {
                range: d.range,
                style: flint::DecorationStyle::Error,
            })
            .collect()
    }
}

/// The token covering byte `offset`: its text and style — the thing a hover peeks
/// at. `None` when the offset sits in whitespace or punctuation.
fn token_at(content: &str, offset: usize) -> Option<(String, flint::TokenStyle)> {
    crate::sql::tokenize(content)
        .into_iter()
        .find_map(|(r, style)| {
            r.contains(&offset)
                .then(|| (content[r.clone()].to_string(), style))
        })
}

/// The hover-peek provider: hovering an error shows its message; a function shows
/// its signature; a table shows its columns; a column of a referenced table shows
/// its type. Reuses the resident schema + function catalog — no fetch.
fn hover_provider(
    index: Rc<CompletionIndex>,
) -> impl Fn(&str, usize) -> Option<SharedString> + 'static {
    move |content, offset| {
        // A diagnostic under the pointer wins — surface its message.
        if let Some(d) = crate::sql::diagnostics(content, index.as_ref())
            .into_iter()
            .find(|d| d.range.contains(&offset))
        {
            return Some(SharedString::from(d.message));
        }

        let (word, style) = token_at(content, offset)?;
        let wl = word.to_lowercase();

        // A function call → its signature and one-line guide (known functions only,
        // so an engine-specific or user-defined function simply shows nothing).
        if style == flint::TokenStyle::Function {
            return index
                .functions
                .iter()
                .find(|(name, _, _)| *name == wl)
                .map(|(_, sig, doc)| SharedString::from(format!("{sig}\n{doc}")));
        }
        if style != flint::TokenStyle::Identifier {
            return None;
        }

        // A table name → its column list (with types when the detail is loaded).
        if index.table_names.contains(&wl) {
            let mut peek = word.clone();
            match index.columns_by_table.get(&wl) {
                Some(cols) => {
                    for c in cols.iter().take(14) {
                        peek.push('\n');
                        if c.ty.is_empty() {
                            peek.push_str(&format!("  {}", c.name));
                        } else {
                            peek.push_str(&format!("  {}  {}", c.name, c.ty));
                        }
                    }
                    if cols.len() > 14 {
                        peek.push_str(&format!("\n  … {} more", cols.len() - 14));
                    }
                }
                None => peek.push_str("\n  (columns not loaded)"),
            }
            return Some(SharedString::from(peek));
        }

        // A column of a table the statement references → its type.
        for (_, table) in crate::sql::referenced_tables_at(content, offset) {
            if let Some(cols) = index.columns_by_table.get(&table.to_lowercase()) {
                if let Some(c) = cols.iter().find(|c| c.name.to_lowercase() == wl) {
                    let ty = if c.ty.is_empty() {
                        "column".to_string()
                    } else {
                        c.ty.to_string()
                    };
                    return Some(SharedString::from(format!(
                        "{}  {}\nin {}",
                        c.name, ty, table
                    )));
                }
            }
        }
        None
    }
}

/// Build a column completion: a `Field` badge, the type as detail, a short guide.
fn column_item(col: &ColumnCand) -> CompletionItem {
    let item = CompletionItem::new(col.name.clone(), CompletionKind::Field);
    if col.ty.is_empty() {
        item.documentation("column")
    } else {
        item.detail(col.ty.clone())
            .documentation(SharedString::from(format!("{} column", col.ty)))
    }
}

/// Build a table/view completion: an `Object` badge plus table-vs-view text.
fn table_item(t: &TableCand) -> CompletionItem {
    let (detail, doc) = if t.is_view {
        ("view", "Database view.")
    } else {
        ("table", "Database table.")
    };
    CompletionItem::new(t.name.clone(), CompletionKind::Object)
        .detail(detail)
        .documentation(doc)
}

/// Build a keyword completion: a `Keyword` badge plus any one-line guide.
fn keyword_item(kw: &SharedString) -> CompletionItem {
    let item = CompletionItem::new(kw.clone(), CompletionKind::Keyword).detail("keyword");
    match crate::sql::keyword_doc(&kw.to_lowercase()) {
        Some(doc) => item.documentation(doc),
        None => item,
    }
}

/// Build a function completion: a `Function` badge, its signature, and a guide.
fn function_item(name: &str, sig: &str, doc: &str) -> CompletionItem {
    CompletionItem::new(SharedString::from(name), CompletionKind::Function)
        .detail(SharedString::from(sig))
        .documentation(SharedString::from(doc))
}

/// A column completion enriched, when the column is a foreign key into a table
/// referenced by the statement, with a `→ target.col` relationship hint in place
/// of the generic doc line — so a join key reads as one at a glance.
fn column_item_hinted(col: &ColumnCand, rel: Option<&String>) -> CompletionItem {
    match rel {
        Some(target) => {
            let item = CompletionItem::new(col.name.clone(), CompletionKind::Field)
                .documentation(SharedString::from(format!("→ {target}")));
            if col.ty.is_empty() {
                item
            } else {
                item.detail(col.ty.clone())
            }
        }
        None => column_item(col),
    }
}

/// A short alias for a table in a synthesised JOIN: its first letter, or the whole
/// (lower-cased) name when that letter is already taken by another table in the
/// statement, so the `ON` clause never references an ambiguous alias.
fn suggest_alias(table: &str, taken: &HashSet<String>) -> String {
    if let Some(c) = table.chars().find(|c| c.is_ascii_alphabetic()) {
        let a = c.to_ascii_lowercase().to_string();
        if !taken.contains(&a) {
            return a;
        }
    }
    table.to_lowercase()
}

/// For a table referenced by the statement, map each of its foreign-key columns
/// (lower-cased) to a `target_table.target_col` string, so column completions can
/// show where the key points.
fn fk_hints(index: &CompletionIndex, table_key: &str) -> HashMap<String, String> {
    let mut hints = HashMap::new();
    if let Some(rels) = index.joins_by_table.get(table_key) {
        for rel in rels {
            for (mine, theirs) in &rel.pairs {
                hints
                    .entry(mine.to_lowercase())
                    .or_insert_with(|| format!("{}.{}", rel.other, theirs));
            }
        }
    }
    hints
}

/// The auto-`JOIN` completions for a post-`JOIN` cursor: for each schema table
/// related (by the FK graph) to a table already in the statement, one completion
/// that inserts `<table> <alias> ON <a>.<col> = <b>.<col>`, pre-filled from the
/// relation. Composite keys join their pairs with `AND`. Tables with no relation
/// to the current statement contribute nothing here (the caller still appends the
/// plain table list as a fallback).
fn join_items(index: &CompletionIndex, content: &str, cursor: usize) -> Vec<CompletionItem> {
    let referenced = crate::sql::referenced_tables_at(content, cursor);
    let taken: HashSet<String> = referenced.iter().map(|(a, _)| a.clone()).collect();
    let mut out = Vec::new();
    for t in &index.tables {
        let Some(rels) = index.joins_by_table.get(&t.name.to_lowercase()) else {
            continue;
        };
        for rel in rels {
            // The other endpoint must already be in the statement; use its alias.
            let Some((base_alias, _)) = referenced
                .iter()
                .find(|(_, tbl)| tbl.eq_ignore_ascii_case(&rel.other))
            else {
                continue;
            };
            let alias = suggest_alias(&t.name, &taken);
            let on = rel
                .pairs
                .iter()
                .map(|(mine, theirs)| format!("{alias}.{mine} = {base_alias}.{theirs}"))
                .collect::<Vec<_>>()
                .join(" AND ");
            let label = format!("{} {} ON {}", t.name, alias, on);
            out.push(
                CompletionItem::new(SharedString::from(label), CompletionKind::Object)
                    .detail("join")
                    .documentation(SharedString::from(format!("joins {}", rel.other))),
            );
        }
    }
    out
}

/// The provider closure handed to the editor's completion seam. It reads the
/// cursor's context (member access, a table position, a column expression, or a
/// statement start) and offers the matching candidates, most-relevant first.
fn completion_provider(
    index: Rc<CompletionIndex>,
) -> impl Fn(&str, usize) -> Vec<CompletionItem> + 'static {
    move |content, cursor| {
        let prefix = crate::sql::word_prefix(content, cursor).to_lowercase();
        let context = crate::sql::analyze(content, cursor);

        // Only member access (`table.`) suggests with nothing typed; elsewhere we
        // wait for a prefix so the popup doesn't open on every space.
        if prefix.is_empty() && !matches!(context, CompletionContext::Dot { .. }) {
            return Vec::new();
        }

        // Candidate sources in priority order: earlier groups win ties. Each
        // carries a kind badge, a detail (type/signature), and a doc-panel guide.
        let mut ordered: Vec<CompletionItem> = Vec::new();
        match &context {
            CompletionContext::Dot { qualifier } => {
                let q = qualifier.to_lowercase();
                let real = crate::sql::referenced_tables_at(content, cursor)
                    .into_iter()
                    .find(|(alias, _)| *alias == q)
                    .map(|(_, table)| table.to_lowercase())
                    .or_else(|| {
                        index
                            .tables
                            .iter()
                            .find(|t| t.name.to_lowercase() == q)
                            .map(|t| t.name.to_lowercase())
                    });
                if let Some(cols) = real.and_then(|r| index.columns_by_table.get(&r)) {
                    ordered.extend(cols.iter().map(column_item));
                }
            }
            CompletionContext::Table => ordered.extend(index.tables.iter().map(table_item)),
            CompletionContext::Join => {
                // Auto-JOIN completions (relation-aware) lead; the plain table
                // list follows so an unrelated table is still reachable here.
                ordered.extend(join_items(&index, content, cursor));
                ordered.extend(index.tables.iter().map(table_item));
            }
            CompletionContext::Column => {
                // Columns of the tables this statement actually references rank
                // first, then the rest of the schema, then functions, tables, keywords.
                for (_, table) in crate::sql::referenced_tables_at(content, cursor) {
                    let key = table.to_lowercase();
                    let hints = fk_hints(&index, &key);
                    if let Some(cols) = index.columns_by_table.get(&key) {
                        ordered.extend(
                            cols.iter()
                                .map(|c| column_item_hinted(c, hints.get(&c.name.to_lowercase()))),
                        );
                    }
                }
                ordered.extend(index.all_columns.iter().map(column_item));
                ordered.extend(
                    index
                        .functions
                        .iter()
                        .map(|(n, sig, doc)| function_item(n, sig, doc)),
                );
                ordered.extend(index.tables.iter().map(table_item));
                ordered.extend(index.keywords.iter().map(keyword_item));
            }
            CompletionContext::Keyword => {
                ordered.extend(index.keywords.iter().map(keyword_item));
                ordered.extend(index.tables.iter().map(table_item));
            }
        }

        let mut seen: HashSet<String> = HashSet::new();
        ordered
            .into_iter()
            .filter(|c| {
                let cl = c.label.to_lowercase();
                if !cl.starts_with(&prefix) || (!prefix.is_empty() && cl == prefix) {
                    return false;
                }
                seen.insert(cl)
            })
            .take(MAX_CANDIDATES)
            .collect()
    }
}

/// First non-empty, non-comment line of a query, truncated: the history label,
/// and the suggested name when saving a query (B3).
pub(crate) fn history_label(sql: &str) -> String {
    let line = sql
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or("");
    let truncated: String = line.chars().take(72).collect();
    if line.chars().count() > 72 {
        format!("{truncated}…")
    } else {
        truncated
    }
}

impl AppState {
    /// The editor pane for the tab at `tab_idx`, shown in split half `half`: the tab
    /// strip + breadcrumb + the `CodeEditor` surface + run bar. `is_focused` is
    /// whether this half holds focus; the (single-instance) find bar renders only in
    /// the focused half.
    pub(crate) fn render_editor(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        half: crate::app::SplitHalf,
        is_focused: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Owned (not borrowed from `cx`) so the agent-tab branch below can call a
        // `&mut cx` render method without clashing with the theme tokens this fn
        // snapshots throughout.
        let theme = cx.theme().clone();
        let (bg_app, bg_panel, bg_elevated, bg_hover) = (
            theme.bg_app,
            theme.bg_panel,
            theme.bg_elevated,
            theme.bg_hover,
        );
        let (border, border_soft) = (theme.border, theme.border_soft);
        let (text, muted, faint, dim) = (
            theme.text,
            theme.text_muted,
            theme.text_faint,
            theme.text_dim,
        );
        let (yellow, on_accent) = (theme.yellow, theme.on_accent);
        let accent = theme.accent;
        // UI-chrome icon size scaled with the general UI font (not the editor
        // mono font). Snapshotted here (Pixels is Copy) so it survives into the
        // lazy per-tab `.map` closure without re-borrowing `theme`.
        let icon_close = theme.scale(9.);
        // UI font + chrome sizes snapshotted (SharedString clones / Copy `Pixels`)
        // so the tab + breadcrumb chrome tracks the UI font even inside the lazy
        // per-tab `.map` closure. The editor *surface* keeps its own mono font.
        let ui_family = theme.font_family.clone();
        let (size_11, size_12) = (theme.scale(11.), theme.scale(12.));
        let view = cx.entity().downgrade();

        // --- tab strip: one tab per open query + a "new query" affordance ---
        // This half's strip shows only the tabs that belong to it (Zed-style); the
        // other half (when split) has its own strip. The highlighted one is the
        // pane's active tab (which is the `tab_idx` the body renders).
        let active_idx = active.pane_active(half);
        let pane_indices = active.pane_tab_indices(half);
        let last_in_pane = pane_indices.last().copied();
        // Drop-indicator state: the gap a dragged tab would land in, gated on an
        // actual drag so a stale target never paints once the drag ends.
        let drop_target = active.tab_drop_target;
        let dragging = cx.has_active_drag();
        // Pinned tabs render in their own fixed (non-scrolling) section ahead of
        // the scrollable strip, so they stay on screen no matter how far the rest
        // of the strip is scrolled; they keep their relative order otherwise.
        let (pinned_indices, unpinned_indices): (Vec<usize>, Vec<usize>) = pane_indices
            .iter()
            .copied()
            .partition(|&i| active.tabs[i].pinned);
        let render_tab = |i: usize| {
            let t = &active.tabs[i];
            let is_active = Some(i) == active_idx;
            let pinned = t.pinned;
            let (tab_bg, tab_text) = if is_active {
                (bg_app, text)
            } else {
                (bg_panel, muted)
            };
            let drag_title: SharedString = t.title.clone().into();
            let drop_view = view.clone();
            let move_view = view.clone();
            // Group so the close button reveals only on this tab's hover.
            let group = SharedString::from(format!("sql-tab-{i}"));
            // The dragged tab lands before this tab (gap == i) or after it
            // (gap == i+1); the bar paints on whichever edge the gap names. The
            // after-bar shows only on this pane's last tab.
            let bar_before = dragging && drop_target == Some(i);
            let bar_after = dragging && Some(i) == last_in_pane && drop_target == Some(i + 1);
            div()
                .id(("sql-tab", i))
                .group(group.clone())
                .relative()
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                // Stretch with the title between a comfortable min and a cap;
                // past the cap the label ellipsizes (see the title's `truncate`).
                .min_w(px(96.))
                .max_w(px(200.))
                // Symmetric horizontal room: the hover close button lives in the
                // right inset (right: 4px + 15px wide); mirror it on the left so
                // the centered title clears the button and stays balanced.
                .px(px(23.))
                .bg(tab_bg)
                .border_r_1()
                .border_color(border)
                .cursor_pointer()
                .when(!is_active, |d| d.hover(|s| s.bg(bg_elevated)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    // Clicking a tab in this half's strip aims it at this half.
                    this.set_split_focus(half, cx);
                    this.set_active_tab(i, cx);
                }))
                // Middle-click closes the tab, like a browser tab strip. Pinned
                // tabs are protected: unpin (or use the context menu) to close.
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _, _, cx| {
                        if !pinned {
                            this.request_close_tab(i, cx);
                        }
                    }),
                )
                // Right-click opens the Close/Pin context menu at the cursor.
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        this.tab_context_menu = Some((i, event.position));
                        cx.notify();
                    }),
                )
                // Drag this tab to reorder; the chip below tracks the cursor.
                .on_drag(TabDrag(i), move |_, offset, _window, cx| {
                    let title = drag_title.clone();
                    cx.new(move |_| TabDragPreview {
                        title,
                        offset,
                        bg: bg_elevated,
                        border,
                        text,
                    })
                })
                // Track the cursor across this tab to aim the drop gap at the
                // nearer edge, then commit the reorder on release. GPUI fires
                // this on *every* tab per mouse move (capture phase, no hover
                // gate), so we ignore moves whose cursor isn't over this tab;
                // only the hovered tab gets to set the gap.
                .on_drag_move::<TabDrag>(move |e, _window, cx| {
                    let b = e.bounds;
                    let p = e.event.position;
                    // Must be over this tab in *both* axes, since checking x alone
                    // would keep re-setting the gap while dragging straight down
                    // off the strip, leaving a stale indicator.
                    if p.x < b.origin.x
                        || p.x >= b.origin.x + b.size.width
                        || p.y < b.origin.y
                        || p.y >= b.origin.y + b.size.height
                    {
                        return;
                    }
                    let gap = if p.x < b.origin.x + b.size.width / 2. {
                        i
                    } else {
                        i + 1
                    };
                    move_view
                        .update(cx, |this, cx| this.set_tab_drop_target(gap, cx))
                        .ok();
                })
                .on_drop::<TabDrag>(move |drag, _window, cx| {
                    let from = drag.0;
                    // Handle it here so it doesn't also bubble to the half wrapper's
                    // cross-half drop (which would double-move). Dropping on a half's
                    // strip lands the tab in that half, then reorders to the gap.
                    cx.stop_propagation();
                    drop_view
                        .update(cx, |this, cx| this.drop_tab(from, half, cx))
                        .ok();
                })
                .when(bar_before, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .bottom_0()
                            .w(px(2.))
                            .bg(accent),
                    )
                })
                .when(bar_after, |d| {
                    d.child(
                        div()
                            .absolute()
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .w(px(2.))
                            .bg(accent),
                    )
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .min_w_0()
                        .when(pinned, |d| {
                            d.child(crate::icons::icon("pin", icon_close, faint))
                        })
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family(ui_family.clone())
                                .text_size(size_12)
                                .text_color(tab_text)
                                .child(t.title.clone()),
                        ),
                )
                // Close button: pinned to the right, revealed only on tab hover
                // so it never crowds the centered title at rest. The outer div
                // positions + vertically centers; the inner one is the hitbox.
                .child(
                    div()
                        .absolute()
                        .right(px(4.))
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .invisible()
                        .group_hover(group, |s| s.visible())
                        .child(
                            div()
                                .id(("sql-tab-close", i))
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(15.))
                                .rounded(px(3.))
                                .text_color(faint)
                                .hover(|s| s.bg(bg_hover).text_color(text))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.request_close_tab(i, cx);
                                }))
                                .child(crate::icons::icon("close", icon_close, faint)),
                        ),
                )
        };
        let pinned_tabs: Vec<_> = pinned_indices.iter().map(|&i| render_tab(i)).collect();
        let unpinned_tabs: Vec<_> = unpinned_indices.iter().map(|&i| render_tab(i)).collect();
        let strip_drop_view = view.clone();
        let strip_move_view = view.clone();
        // The tabs live in a horizontally scrollable viewport, so a crowded
        // strip scrolls instead of squashing the tabs. `min_w(0)` lets the
        // flex child shrink below its content width so the overflow engages.
        let tab_viewport = div()
            .id("sql-tabstrip")
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .items_stretch()
            .overflow_x_scroll()
            .track_scroll(&active.tab_scroll)
            // Clear the gap indicator when the drag leaves the strip *vertically*
            // (down into the editor/result body, where the cross-half drop takes
            // over). We deliberately ignore horizontal exit: with two side-by-side
            // strips, a drag crossing to the other strip stays at the same Y, and
            // clearing on horizontal exit would race the other strip's gap set.
            .on_drag_move::<TabDrag>(move |e, _window, cx| {
                let b = e.bounds;
                let p = e.event.position;
                let outside = p.y < b.origin.y || p.y >= b.origin.y + b.size.height;
                if outside {
                    strip_move_view
                        .update(cx, |this, cx| this.clear_tab_drop_target(cx))
                        .ok();
                }
            })
            // Release anywhere in the strip (incl. the trailing space) commits
            // using the gap the hovered tab last set, landing the tab in this half.
            // `stop_propagation` keeps it off the half wrapper's cross-half drop.
            .on_drop::<TabDrag>(move |drag, _window, cx| {
                let from = drag.0;
                cx.stop_propagation();
                strip_drop_view
                    .update(cx, |this, cx| this.drop_tab(from, half, cx))
                    .ok();
            })
            .children(unpinned_tabs);
        // Pinned tabs sit in their own fixed section ahead of the scrollable
        // strip (not `overflow_x_scroll`), so they never leave view.
        let pinned_strip = (!pinned_tabs.is_empty()).then(|| {
            div()
                .id("sql-tabstrip-pinned")
                .flex_shrink_0()
                .h_full()
                .flex()
                .items_stretch()
                .children(pinned_tabs)
        });
        // The "＋" stays pinned right of the scrolling tabs, always reachable.
        let tabstrip = div()
            .flex_shrink_0()
            .h(px(35.))
            .flex()
            .items_stretch()
            .bg(bg_panel)
            .border_b_1()
            .border_color(border)
            .children(pinned_strip)
            .child(tab_viewport)
            .child(
                div()
                    .id("sql-new")
                    .flex_shrink_0()
                    .w(px(34.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_l_1()
                    .border_color(border)
                    .cursor_pointer()
                    .tooltip(Tooltip::text(crate::keymap::localize_hint("New tab  ⌘T")))
                    .text_color(faint)
                    .hover(|s| s.bg(bg_elevated).text_color(text))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // Open the new tab in this half.
                        this.set_split_focus(half, cx);
                        this.new_query(cx);
                    }))
                    .child(crate::icons::icon("plus", theme.scale(13.), faint)),
            );

        // No open tab (user closed the last one): keep the strip (its ＋ opens
        // a new query) over an empty pane, and skip the editor/run/breadcrumb.
        let Some(tab) = active.tabs.get(tab_idx) else {
            let empty = div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(size_12)
                .text_color(faint)
                .child("No query tab open. Press ＋ to start");
            return div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .bg(bg_app)
                .child(tabstrip)
                .child(empty);
        };

        // --- breadcrumb: connection › query ---
        let breadcrumb = div()
            .flex_shrink_0()
            .h(px(26.))
            .flex()
            .items_center()
            .gap_1p5()
            .px_3p5()
            .bg(bg_app)
            .border_b_1()
            .border_color(border_soft)
            .font_family(ui_family.clone())
            .text_size(size_11)
            .text_color(muted)
            .child(active.config.name.clone())
            .child(div().text_color(dim).child("/"))
            .child(div().text_color(text).child(tab.title.clone()));

        // The editor's own typography, applied here: the `CodeEditor` shapes its
        // text with `window.text_style()` / `window.line_height()`, both inherited
        // from this container, so setting them here drives the editor font without
        // it (or Flint) knowing about settings.
        let ed = &self.settings.editor;
        let surface = div()
            .flex_1()
            .min_h(px(0.))
            .font_family(ed.font_family.clone())
            .text_size(px(ed.font_size))
            .line_height(px(ed.font_size * ed.line_height))
            .child(tab.editor.clone());

        // --- bottom run bar: Run · Explain · Save · ……… · read-only ---
        // (Query history now lives in the left dock, toggled with ⌘Y.)
        let ro_chip = active.config.read_only.then(|| {
            div()
                .ml_auto()
                .flex()
                .items_center()
                .px_2()
                .py(px(2.))
                .gap_1()
                .rounded(theme.radius_sm)
                .bg(yellow.opacity(0.1))
                .text_size(size_11)
                .text_color(yellow)
                .child(crate::icons::icon("lock", theme.scale(11.), yellow))
                .child("read-only")
        });
        let run_bar = div()
            .flex_shrink_0()
            // No fixed height: the 24px buttons define the strip and the equal
            // padding brackets them evenly. A fixed height taller than the
            // buttons left slack that GPUI distributed unevenly, sinking the
            // buttons off-center.
            .py(px(5.))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .bg(bg_panel)
            .border_t_1()
            .border_color(border)
            .child(
                Button::new("sql-run", crate::keymap::localize_hint("Run  ⌘↵"))
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Sm)
                    .icon(crate::icons::icon("play", theme.scale(11.), on_accent))
                    // Aim the action at the half this bar lives in, not whichever
                    // half currently holds focus (mirrors the tab/＋ handlers).
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_split_focus(half, cx);
                        this.run_editor_query(cx);
                    })),
            )
            .child(
                Button::new("sql-explain", "Explain")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_split_focus(half, cx);
                        this.explain_query(false, cx);
                    })),
            )
            .child(
                Button::new("sql-save", "Save")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_split_focus(half, cx);
                        this.open_save_prompt(cx);
                    })),
            )
            .children(ro_chip);

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(bg_app)
            .child(tabstrip)
            .child(breadcrumb)
            // The find bar (Track B2, Tier 1) sits above the editor when ⌘F opened
            // it against the query; it selects matches in place, so the editor just
            // repaints. Single-instance, so only the focused half renders it.
            .when_some(
                is_focused
                    .then(|| self.render_find_bar(crate::find::FindTarget::Editor, cx))
                    .flatten(),
                |c, bar| c.child(bar),
            )
            .child(surface)
            .child(run_bar)
    }

    /// The base `(schema, table)` a hand-typed `SELECT * FROM <table>` browses, when
    /// one can be resolved against the connection catalog, so that the editor result
    /// gets the schema-tree browse's FK affordances and keyset paging. `None` for any
    /// query that isn't a plain single-table star select ([`crate::sql::single_table_star`]),
    /// or whose table can't be pinned to exactly one namespace in the catalog.
    ///
    /// The resolved schema string has to match what the driver's FK graph and tree
    /// use, so it's taken from the catalog (`SchemaMeta.name`) rather than a guess: a
    /// bare name resolves only when a single namespace holds it; an explicit
    /// qualifier must name a real object. An ambiguous bare name (same table in two
    /// namespaces) stays `None`: the engine picks by search-path, which we don't
    /// track, so guessing could tag the wrong table.
    fn resolve_browse_table(&self, sql: &str) -> Option<(String, String)> {
        let (schema_hint, table) = crate::sql::single_table_star(sql)?;
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let resolved = resolve_in_catalog(&active.schema.schemas, schema_hint.as_deref(), &table);
        tracing::debug!(
            ?schema_hint, %table, ns = active.schema.schemas.len(), ?resolved,
            "resolve_browse_table for FK affordances"
        );
        resolved
    }

    /// Run the selection if any, else the statement under the caret. Pushes to
    /// history and streams the first window into the results pane.
    pub(crate) fn run_editor_query(&mut self, cx: &mut Context<Self>) {
        self.run_editor_query_impl(None, cx);
    }

    /// Run the statement whose gutter run marker (▶) on 0-based `line` was clicked
    /// (Phase D). Resolves the marker's byte offset in the active tab, then runs the
    /// statement there through the same path as ⌘↵.
    pub(crate) fn run_editor_line(&mut self, line: usize, cx: &mut Context<Self>) {
        let offset = match &self.phase {
            Phase::Connected(active) => match active.active() {
                Some(tab) => {
                    let content = tab.editor.read(cx).content();
                    crate::sql::line_start_offset(&content, line)
                }
                None => return,
            },
            _ => return,
        };
        self.run_editor_query_impl(Some(offset), cx);
    }

    fn run_editor_query_impl(&mut self, force_offset: Option<usize>, cx: &mut Context<Self>) {
        let sql = match &self.phase {
            Phase::Connected(active) => match active.active() {
                Some(tab) => {
                    let editor = tab.editor.read(cx);
                    match force_offset {
                        // A clicked gutter marker runs exactly its statement.
                        Some(off) => {
                            let content = editor.content();
                            crate::sql::statement_at(&content, off).to_string()
                        }
                        // An explicit selection runs verbatim; otherwise run just the
                        // statement under the caret, not the whole buffer: a buffer of
                        // several statements can't open as one result (the paging wrap
                        // is a single subquery), so running the caret's statement is
                        // what the user means and avoids a cryptic engine error.
                        None => match editor.selected_text() {
                            Some(sel) => sel,
                            None => {
                                let content = editor.content();
                                crate::sql::statement_at(&content, editor.cursor_offset())
                                    .to_string()
                            }
                        },
                    }
                }
                None => return,
            },
            _ => return,
        };
        let sql = sql.trim().to_string();
        // An editor can slip in a non-breaking space (macOS Option+Space) that the
        // engine rejects as an invalid token rather than whitespace; scrub those to
        // plain spaces (outside literals/comments) so a valid-looking query runs
        // instead of bouncing back a cryptic `syntax error at or near " FROM"`.
        let sql = crate::sql::normalize_spaces(&sql).unwrap_or(sql);
        // Nothing runnable (empty, or only comments/`;`), so skip it rather than let
        // the empty `SELECT * FROM (<sql>)` paging wrap bounce back a bare "db error".
        if crate::sql::is_blank(&sql) {
            return;
        }

        // Record into the persistent, connection-scoped history. `record` de-dupes
        // consecutive identical runs and caps/persists itself. Pull `conn_id` out
        // first so the borrow of `self.phase` is released before touching
        // `self.query_history`.
        let conn_id = match &self.phase {
            Phase::Connected(active) => Some(active.conn_id.clone()),
            _ => None,
        };
        if let Some(conn_id) = conn_id {
            self.query_history.record(&conn_id, &sql);
        }

        // Row-returning statements stream into the grid; writes execute in a
        // transaction; destructive writes ask for confirmation first.
        let kind = crate::sql::classify(&sql);

        // On a read-only connection, refuse writes up front instead of letting
        // them round-trip to the engine and bounce back as a cryptic error. The
        // engine still rejects writes as a backstop; this is the friendly gate.
        let read_only = matches!(&self.phase, Phase::Connected(active) if active.config.read_only);
        if read_only && !matches!(kind, crate::sql::StatementKind::Query) {
            self.notify(
                ToastVariant::Error,
                "Connection is read-only; write statements are disabled.",
                cx,
            );
            return;
        }

        match kind {
            crate::sql::StatementKind::Query => {
                // A row-returning batch can't open as one result: the paging path
                // wraps the SQL in a single `SELECT * FROM (<sql>) AS _red`, which a
                // `;`-separated batch makes a syntax error. Only an explicit
                // multi-statement selection reaches here (a no-selection run already
                // narrowed to the caret's statement); say so plainly.
                if crate::sql::statement_count(&sql) > 1 {
                    self.notify(
                        ToastVariant::Error,
                        "Select a single statement to run; \
                         a multi-statement query can't open as a result.",
                        cx,
                    );
                    return;
                }
                // When the query is a plain `SELECT * FROM <table>`, tag the result
                // with that base table so it gets the same FK affordances (accent,
                // click-through, reference-column tree) and keyset paging as a browse
                // opened from the schema tree. Resolve before the auto-limit shadows
                // `sql` (the sniffer accepts a trailing LIMIT either way).
                let table = self.resolve_browse_table(&sql);
                // Guard a bare `SELECT *` against flooding the grid: append the
                // configured `LIMIT` unless the user wrote their own.
                let sql =
                    crate::sql::auto_limit(&sql, self.settings.query.auto_limit).unwrap_or(sql);
                self.open_result("query", sql, table, cx)
            }
            crate::sql::StatementKind::Write => self.execute_sql(sql, cx),
            crate::sql::StatementKind::Destructive => {
                // The safety rail is opt-out in settings; when off, run immediately.
                if self.settings.query.confirm_destructive {
                    self.confirm_exec = Some(crate::app::PendingWrite::EditorSql(sql));
                    // Focus the modal so its own Enter/Esc handling is heard.
                    self.focus_modal = true;
                    cx.notify();
                } else {
                    self.execute_sql(sql, cx);
                }
            }
        }
    }

    /// Beautify the active editor's SQL in place (⌥⌘F / palette / Query menu):
    /// re-indent, upper-case keywords, and break clauses onto their own lines. It
    /// reformats the whole buffer (a single undo step) and never touches the
    /// database. A blank or already-formatted buffer is a no-op.
    pub(crate) fn format_active_sql(&mut self, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let Some(tab) = active.active() else {
            return;
        };
        let editor = tab.editor.clone();
        let content = editor.read(cx).content();
        if content.trim().is_empty() {
            return;
        }
        let formatted = crate::sql::format_sql(&content);
        if formatted != content {
            editor.update(cx, |editor, cx| editor.set_content(formatted, cx));
            cx.notify();
        }
    }

    /// Run a write/DDL statement in a transaction; refresh the schema tree after,
    /// since it may have created or dropped objects. The single seam through which
    /// writes leave the UI, so it also enforces the read-only gate, catching any
    /// caller that didn't pre-check (e.g. future inline-edit paths).
    pub(crate) fn execute_sql(&mut self, sql: String, cx: &mut Context<Self>) {
        if matches!(&self.phase, Phase::Connected(active) if active.config.read_only) {
            self.notify(
                ToastVariant::Error,
                "Connection is read-only; write statements are disabled.",
                cx,
            );
            return;
        }
        self.send_active(Command::Execute { sql });
        cx.notify();
    }

    /// Show or hide the History panel in the left dock (status-bar toggle, ⌘Y, or
    /// palette). Opening focuses its list; closing returns focus to the editor.
    pub(crate) fn toggle_history(&mut self, cx: &mut Context<Self>) {
        let opened = if let Phase::Connected(active) = &mut self.phase {
            active.history_open = !active.history_open;
            // Reset the keyboard highlight to the top whenever it opens.
            if active.history_open {
                active.history_sel = 0;
            }
            Some(active.history_open)
        } else {
            None
        };
        match opened {
            // Focus the panel's list so its arrow keys work; closing returns focus
            // to the editor.
            Some(true) => self.focus_history = true,
            Some(false) => self.pending_focus = Some(crate::app::Pane::Editor),
            None => {}
        }
        cx.notify();
    }

    /// Move the History panel's keyboard highlight (↑/↓). No-op with empty history.
    pub(crate) fn history_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = match &self.phase {
            Phase::Connected(active) => self.query_history.count_for_conn(&active.conn_id),
            _ => return,
        };
        if len == 0 {
            return;
        }
        if let Phase::Connected(active) = &mut self.phase {
            let sel = active.history_sel as isize + delta;
            active.history_sel = sel.clamp(0, len as isize - 1) as usize;
            cx.notify();
        }
    }

    /// Load the highlighted history entry into the editor (Enter in the panel).
    pub(crate) fn history_accept(&mut self, cx: &mut Context<Self>) {
        let sql = match &self.phase {
            Phase::Connected(active) => self
                .query_history
                .for_conn(&active.conn_id)
                .get(active.history_sel)
                .map(|e| e.sql.clone()),
            _ => None,
        };
        if let Some(sql) = sql {
            // Fill the editor; the panel stays open so the user can keep browsing.
            self.load_history(sql, cx);
        }
        self.pending_focus = Some(crate::app::Pane::Editor);
    }

    /// Remove one history entry by id (the panel's per-row ✕), keeping the
    /// keyboard highlight in range after the row vanishes.
    pub(crate) fn delete_history(&mut self, id: u64, cx: &mut Context<Self>) {
        self.query_history.delete(id);
        let len = match &self.phase {
            Phase::Connected(active) => self.query_history.count_for_conn(&active.conn_id),
            _ => 0,
        };
        if let Phase::Connected(active) = &mut self.phase {
            if active.history_sel >= len {
                active.history_sel = len.saturating_sub(1);
            }
        }
        cx.notify();
    }

    /// Clear the active connection's entire history (the palette command).
    pub(crate) fn clear_history(&mut self, cx: &mut Context<Self>) {
        let conn_id = match &self.phase {
            Phase::Connected(active) => Some(active.conn_id.clone()),
            _ => None,
        };
        if let Some(conn_id) = conn_id {
            self.query_history.clear_conn(&conn_id);
        }
        if let Phase::Connected(active) = &mut self.phase {
            active.history_sel = 0;
        }
        cx.notify();
    }

    /// Open a history entry from the panel: a plain click opens it in a **new**
    /// query tab (titled from the SQL), a ⌘/Ctrl-click **replaces** the current
    /// tab's editor in place. With no open tab, both open a fresh one. The panel
    /// stays open either way so the user can keep browsing. Nothing runs — the SQL
    /// is only seeded, so a past write is never re-executed by a stray click.
    pub(crate) fn open_history(
        &mut self,
        sql: String,
        replace_current: bool,
        cx: &mut Context<Self>,
    ) {
        let has_tab = matches!(&self.phase, Phase::Connected(a) if a.active().is_some());
        if !replace_current || !has_tab {
            let tab = crate::app::QueryTab::new(history_label(&sql), cx);
            self.push_tab(tab, cx);
            self.pending_focus = Some(crate::app::Pane::Editor);
        }
        self.load_history(sql, cx);
    }

    /// Load a history entry's SQL into the active tab's editor. The dock panel
    /// stays open (unlike the old transient popover) so the user can keep browsing.
    pub(crate) fn load_history(&mut self, sql: String, cx: &mut Context<Self>) {
        let editor = match &mut self.phase {
            Phase::Connected(active) => match active.active_mut() {
                Some(tab) => tab.editor.clone(),
                None => return,
            },
            _ => return,
        };
        editor.update(cx, |editor, cx| editor.set_content(sql, cx));
        cx.notify();
    }

    /// Rebuild every tab's editor completion candidates from the current schema.
    /// Called when the skeleton or a table's detail arrives, or a tab is opened.
    pub(crate) fn refresh_completions(&mut self, cx: &mut Context<Self>) {
        let (editors, index) = match &self.phase {
            Phase::Connected(active) => (
                active
                    .tabs
                    .iter()
                    .map(|t| t.editor.clone())
                    .collect::<Vec<_>>(),
                Rc::new(build_index(
                    &active.schema.schemas,
                    &active.schema.details,
                    &active.fk_graph,
                    active.config.kind,
                )),
            ),
            _ => return,
        };
        for editor in editors {
            let index = index.clone();
            editor.update(cx, |editor, cx| {
                editor.set_rich_completions(completion_provider(index.clone()), cx);
                editor.set_hover(hover_provider(index.clone()), cx);
                editor.set_decorations(decoration_provider(index), cx);
            });
        }
    }

    /// The tab strip's right-click menu: Pin/Unpin, then Close / Close Others /
    /// Close Left / Close Right / Close All, resolved against `index`'s own
    /// pane. Anchored at `pos` (the cursor); a full-cover backdrop dismisses it
    /// on an outside click, mirroring `ResultGrid::render_cell_menu`.
    pub(crate) fn render_tab_menu(
        &self,
        index: usize,
        pos: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (pinned, has_left, has_right, has_others) = match &self.phase {
            Phase::Connected(active) => match active.tabs.get(index) {
                Some(t) => {
                    let siblings = active.pane_tab_indices(t.pane);
                    let p = siblings.iter().position(|&i| i == index).unwrap_or(0);
                    (t.pinned, p > 0, p + 1 < siblings.len(), siblings.len() > 1)
                }
                None => (false, false, false, false),
            },
            _ => (false, false, false, false),
        };
        let pin_label = if pinned { "Unpin tab" } else { "Pin tab" };
        let menu = ContextMenu::new("tab-context-menu")
            .item(
                ContextMenuItem::new("tab-pin", pin_label).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.tab_context_menu = None;
                        this.toggle_tab_pin(index, cx);
                    },
                )),
            )
            .separator()
            .item(
                ContextMenuItem::new("tab-close", "Close").on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.tab_context_menu = None;
                        this.close_tab_group(index, TabCloseScope::One, cx);
                    },
                )),
            )
            .item(
                ContextMenuItem::new("tab-close-others", "Close Others")
                    .disabled(!has_others)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab_context_menu = None;
                        this.close_tab_group(index, TabCloseScope::Others, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("tab-close-left", "Close Left")
                    .disabled(!has_left)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab_context_menu = None;
                        this.close_tab_group(index, TabCloseScope::Left, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("tab-close-right", "Close Right")
                    .disabled(!has_right)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab_context_menu = None;
                        this.close_tab_group(index, TabCloseScope::Right, cx);
                    })),
            )
            .item(
                ContextMenuItem::new("tab-close-all", "Close All").on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.tab_context_menu = None;
                        this.close_tab_group(index, TabCloseScope::All, cx);
                    },
                )),
            );
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.tab_context_menu = None;
                    cx.notify();
                }),
            )
            .child(floating(div().occlude().child(menu)).at(pos))
    }
}

/// Resolve a sniffed `(schema_hint, table)` against the connection's namespace
/// catalog to the canonical `(schema, table)`: the pair the FK graph and browse
/// paths key off. Split out from [`AppState::resolve_browse_table`] so the matching
/// rules are unit-testable without a live connection.
///
/// The returned schema/table strings carry the catalog's canonical casing (the same
/// `list_objects` source the FK graph is built from), so the exact `==` match in
/// `ResultGrid::set_fk_cols` lines up. An explicit qualifier must confirm a real
/// object; a bare name resolves only when exactly one namespace holds it (ambiguous
/// → `None`, since the engine would pick by search-path, which RED doesn't track).
fn resolve_in_catalog(
    schemas: &[red_core::SchemaMeta],
    schema_hint: Option<&str>,
    table: &str,
) -> Option<(String, String)> {
    let object_in = |ns: &red_core::SchemaMeta| {
        ns.objects
            .iter()
            .find(|o| o.name.eq_ignore_ascii_case(table))
            .map(|o| o.name.clone())
    };
    match schema_hint {
        Some(schema) => schemas
            .iter()
            .find(|ns| ns.name.eq_ignore_ascii_case(schema))
            .and_then(|ns| object_in(ns).map(|name| (ns.name.clone(), name))),
        None => {
            let mut hits = schemas
                .iter()
                .filter_map(|ns| object_in(ns).map(|name| (ns.name.clone(), name)));
            let first = hits.next()?;
            hits.next().is_none().then_some(first)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_index, completion_provider, hover_provider, join_items, resolve_in_catalog,
        CompletionIndex,
    };
    use red_core::{ColumnMeta, DbKind, FkEdge, ObjectKind, ObjectMeta, SchemaMeta, TableDetail};
    use std::collections::HashMap;
    use std::rc::Rc;

    fn ns(name: &str, objects: &[&str]) -> SchemaMeta {
        SchemaMeta {
            name: name.into(),
            objects: objects
                .iter()
                .map(|o| ObjectMeta {
                    name: (*o).into(),
                    kind: ObjectKind::Table,
                })
                .collect(),
        }
    }

    #[test]
    fn resolves_bare_name_in_single_namespace() {
        let cat = [ns("main", &["users", "tiers"])];
        assert_eq!(
            resolve_in_catalog(&cat, None, "users"),
            Some(("main".into(), "users".into()))
        );
        // Canonical casing comes from the catalog, not the typed name.
        assert_eq!(
            resolve_in_catalog(&cat, None, "USERS"),
            Some(("main".into(), "users".into()))
        );
        // Unknown table → no tag.
        assert_eq!(resolve_in_catalog(&cat, None, "ghost"), None);
    }

    #[test]
    fn bare_name_in_two_namespaces_is_ambiguous() {
        let cat = [ns("public", &["users"]), ns("audit", &["users"])];
        // Same name in two schemas; the engine would pick by search-path, so we don't.
        assert_eq!(resolve_in_catalog(&cat, None, "users"), None);
    }

    #[test]
    fn explicit_qualifier_must_confirm_in_catalog() {
        let cat = [ns("public", &["users"]), ns("audit", &["events"])];
        assert_eq!(
            resolve_in_catalog(&cat, Some("public"), "users"),
            Some(("public".into(), "users".into()))
        );
        // The qualifier disambiguates a name that would otherwise be ambiguous.
        let dup = [ns("public", &["users"]), ns("audit", &["users"])];
        assert_eq!(
            resolve_in_catalog(&dup, Some("audit"), "users"),
            Some(("audit".into(), "users".into()))
        );
        // A qualifier naming a table the schema doesn't hold → no tag.
        assert_eq!(resolve_in_catalog(&cat, Some("public"), "events"), None);
        assert_eq!(resolve_in_catalog(&cat, Some("nope"), "users"), None);
    }

    // --- FK-aware completion (Phase A) ---

    fn col(name: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: Some("int".into()),
            not_null: false,
            primary_key: false,
            default: None,
            auto_increment: false,
        }
    }

    /// Two tables with `orders.customer_id → customers.id`.
    fn fk_fixture() -> CompletionIndex {
        let schemas = vec![SchemaMeta {
            name: "main".into(),
            objects: vec![
                ObjectMeta {
                    name: "customers".into(),
                    kind: ObjectKind::Table,
                },
                ObjectMeta {
                    name: "orders".into(),
                    kind: ObjectKind::Table,
                },
            ],
        }];
        let mut details = HashMap::new();
        details.insert(
            ("main".into(), "customers".into()),
            TableDetail {
                columns: vec![col("id"), col("name")],
                ..Default::default()
            },
        );
        details.insert(
            ("main".into(), "orders".into()),
            TableDetail {
                columns: vec![col("id"), col("customer_id")],
                ..Default::default()
            },
        );
        let fks = vec![FkEdge {
            from_schema: Some("main".into()),
            from_table: "orders".into(),
            to_schema: Some("main".into()),
            to_table: "customers".into(),
            columns: vec![("customer_id".into(), "id".into())],
        }];
        build_index(&schemas, &details, &fks, DbKind::Postgres)
    }

    /// Split a `|`-marked string into (content, cursor byte offset).
    fn at(s: &str) -> (String, usize) {
        let cursor = s.find('|').expect("cursor marker");
        (s.replace('|', ""), cursor)
    }

    fn pairs_of(rel: &super::JoinRel) -> Vec<(String, String)> {
        rel.pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn build_index_records_both_fk_directions() {
        let index = fk_fixture();
        let from_orders = &index.joins_by_table["orders"][0];
        assert_eq!(from_orders.other, "customers");
        assert_eq!(
            pairs_of(from_orders),
            vec![("customer_id".into(), "id".into())]
        );
        // The pointed-at side records the same edge with reversed orientation.
        let from_customers = &index.joins_by_table["customers"][0];
        assert_eq!(from_customers.other, "orders");
        assert_eq!(
            pairs_of(from_customers),
            vec![("id".into(), "customer_id".into())]
        );
    }

    #[test]
    fn join_completion_prefilled_from_fk() {
        let index = fk_fixture();
        let (content, cursor) = at("SELECT * FROM orders o JOIN cu|");
        let items = join_items(&index, &content, cursor);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "customers c ON c.id = o.customer_id");
        assert_eq!(items[0].detail.as_deref(), Some("join"));
    }

    #[test]
    fn join_completion_falls_back_to_table_name_without_alias() {
        let index = fk_fixture();
        let (content, cursor) = at("SELECT * FROM orders JOIN cu|");
        let items = join_items(&index, &content, cursor);
        assert_eq!(items[0].label, "customers c ON c.id = orders.customer_id");
    }

    #[test]
    fn provider_leads_with_join_then_plain_table() {
        let provider = completion_provider(Rc::new(fk_fixture()));
        let (content, cursor) = at("SELECT * FROM orders o JOIN cu|");
        let items = provider(&content, cursor);
        assert_eq!(items[0].label, "customers c ON c.id = o.customer_id");
        // The plain "customers" table is still offered as a fallback.
        assert!(items.iter().any(|i| i.label == "customers"));
    }

    #[test]
    fn column_completion_hints_fk_target() {
        let provider = completion_provider(Rc::new(fk_fixture()));
        let (content, cursor) = at("SELECT customer| FROM orders o");
        let items = provider(&content, cursor);
        let fk_col = items
            .iter()
            .find(|i| i.label == "customer_id")
            .expect("customer_id column offered");
        assert_eq!(fk_col.documentation.as_deref(), Some("→ customers.id"));
    }

    #[test]
    fn hover_shows_function_signature() {
        let provider = hover_provider(Rc::new(fk_fixture()));
        // Hovering a known function call surfaces its signature + guide.
        let (content, cursor) = at("SELECT conc|at(name, id) FROM customers");
        let text = provider(&content, cursor).expect("function peek");
        assert!(text.contains("concat("), "{text}");
        // An unknown (engine-specific / user-defined) function shows nothing —
        // recognised as a function by shape, but never falsely annotated.
        let (content, cursor) = at("SELECT zz|zz(name) FROM customers");
        assert_eq!(provider(&content, cursor), None);
    }
}
