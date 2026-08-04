//! The SQL editor pane: a toolbar (Run · history · read-only badge) over
//! Flint's `CodeEditor`. RED owns the domain bits: the SQL highlighter, the
//! completion candidates fed into the editor's generic completion seam, running
//! the current statement (or selection), and the query history. Results land in
//! the result grid.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use flint::prelude::*;
use gpui::{
    App, Context, Hsla, MouseButton, Pixels, Point, SharedString, Window, div, prelude::*, px,
};
use red_core::{DbKind, FkEdge, ObjectKind, SchemaMeta, TableDetail};
use red_service::Command;

use crate::app::PreflightCount;
use crate::app::{ActiveConn, AppState, Phase, TabCloseScope, TabWorkspace};
use crate::app::{AiReview, AiReviewState};
use crate::sql::CompletionContext;
use crate::tabstrip::{StripTab, TabStrip};
use red_core::ConnEnv;
use red_core::sql::RiskLevel;

use crate::app::ConfirmInput;

/// How many candidates the popup ever shows; the editor renders at most 8, but
/// we hand it a few more so prefix-narrowing has headroom.
const MAX_CANDIDATES: usize = 20;

/// In-app drag payload for the tab strip: the source tab's index. A drop on a
/// strip reorders via [`AppState::drop_tab`]; a drop on a pane *body* moves the
/// tab there — or splits off a new pane — via [`AppState::drop_tab_on_pane`].
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
    /// The engine's lexical dialect, so every scan this index feeds
    /// (diagnostics, completion scoping, hover) splits statements exactly the
    /// way the engine will.
    dialect: crate::sql::Dialect,
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
            // Only relations complete in a `FROM`. A function or a trigger is
            // not a table name, and offering one here would make completion
            // actively wrong the moment the lazy object groups are loaded.
            if !obj.kind.is_relation() {
                continue;
            }
            tables.push(TableCand {
                name: obj.name.clone().into(),
                // "Not a plain table", i.e. not writable through the grid: a
                // materialized view is as read-only as a view for this hint.
                is_view: !matches!(obj.kind, ObjectKind::Table),
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
        dialect: crate::sql::Dialect::of(kind),
    }
}

/// The synthetic statement a bare filter expression is completed (and diagnosed)
/// inside, so the clause-aware passes see a real `WHERE`. `_red` stands in when
/// the result isn't a table browse: an unknown table contributes no columns, and
/// the schema-wide candidates still come through.
fn filter_wrapper(table: Option<&str>) -> String {
    format!("SELECT * FROM {} WHERE ", table.unwrap_or("_red"))
}

/// Completions for the result filter bar's `WHERE` box. The *result's own*
/// columns lead (they are what the predicate can actually name, including
/// inline-expanded reference columns like `"tier_id.name"`), then the editor's
/// own schema-aware candidates, reached by wrapping the expression in
/// [`filter_wrapper`] and shifting the cursor past the prefix.
fn filter_completion_provider(
    index: Rc<CompletionIndex>,
    table: Option<String>,
    columns: Vec<String>,
) -> impl Fn(&str, usize) -> Vec<CompletionItem> + 'static {
    let prefix = filter_wrapper(table.as_deref());
    let shift = prefix.len();
    let inner = completion_provider(index);
    move |content, cursor| {
        let word = crate::sql::word_prefix(content, cursor).to_lowercase();
        let mut out: Vec<CompletionItem> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        if !word.is_empty() {
            for name in &columns {
                let quoted = quote_column(name);
                if name.to_lowercase().starts_with(&word)
                    || quoted.to_lowercase().starts_with(&word)
                {
                    seen.insert(name.to_lowercase());
                    out.push(
                        CompletionItem::new(quoted, CompletionKind::Field).documentation(
                            crate::i18n::tr!(
                                "editor.completion_doc_result_column",
                                "result column"
                            ),
                        ),
                    );
                }
            }
        }
        let cursor = cursor.min(content.len());
        for item in inner(&format!("{prefix}{content}"), cursor + shift) {
            if seen.insert(item.label.to_lowercase()) {
                out.push(item);
            }
        }
        out.truncate(MAX_CANDIDATES);
        out
    }
}

/// The diagnostics provider for the filter box: the same schema-aware pass the
/// editor runs, over the wrapped statement, with the findings mapped back onto
/// the box's own offsets (anything landing in the synthetic prefix is dropped).
fn filter_decoration_provider(
    index: Rc<CompletionIndex>,
    table: Option<String>,
) -> impl Fn(&str) -> Vec<flint::Decoration> + 'static {
    let prefix = filter_wrapper(table.as_deref());
    let shift = prefix.len();
    move |content| {
        crate::sql::diagnostics(&format!("{prefix}{content}"), index.as_ref(), index.dialect)
            .into_iter()
            .filter_map(|d| {
                let start = d.range.start.checked_sub(shift)?;
                let end = d.range.end.checked_sub(shift)?;
                Some(flint::Decoration {
                    range: start..end.min(content.len()),
                    style: flint::DecorationStyle::Error,
                })
            })
            .collect()
    }
}

/// A result column as it must be written in a predicate: bare when it's a plain
/// identifier, double-quoted otherwise (an inline-expanded reference column is
/// aliased `tier_id.name`, so it only resolves quoted).
fn quote_column(name: &str) -> SharedString {
    let plain = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if plain {
        SharedString::from(name.to_string())
    } else {
        SharedString::from(format!("\"{}\"", name.replace('"', "\"\"")))
    }
}

/// Memoize a per-paint buffer analysis on a hash of the buffer text, so a repaint
/// that didn't change the content (cursor blink, grid scroll, a resize) reuses the
/// prior result instead of re-tokenizing and re-parsing the whole buffer. The
/// hash is one linear pass; the analyses it guards are each linear-or-worse plus a
/// full parse, and they run on the GPUI thread every frame. Single-slot: the
/// editor only ever asks about its current content, so one cached (hash, result)
/// pair is all that's needed.
fn memoize_by_content<T: Clone + 'static>(
    f: impl Fn(&str) -> T + 'static,
) -> impl Fn(&str) -> T + 'static {
    use std::cell::RefCell;
    use std::hash::{Hash, Hasher};
    let cache: RefCell<Option<(u64, T)>> = RefCell::new(None);
    move |content: &str| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        let key = hasher.finish();
        if let Some((cached_key, value)) = cache.borrow().as_ref()
            && *cached_key == key
        {
            return value.clone();
        }
        let value = f(content);
        *cache.borrow_mut() = Some((key, value.clone()));
        value
    }
}

/// The SQL highlighter for the editor, memoized on buffer content so a repaint
/// that changed nothing doesn't re-tokenize the whole buffer.
pub(crate) fn memoized_highlighter() -> impl Fn(&str) -> Vec<(std::ops::Range<usize>, TokenStyle)> {
    memoize_by_content(crate::sql::tokenize)
}

/// The gutter run-marker lines for the editor, memoized on buffer content: the
/// underlying [`crate::sql::statement_start_lines`] splits the whole buffer.
pub(crate) fn memoized_gutter_markers(dialect: crate::sql::Dialect) -> impl Fn(&str) -> Vec<usize> {
    memoize_by_content(move |content| crate::sql::statement_start_lines(content, dialect))
}

/// The diagnostics provider handed to the editor's decoration seam: it runs the
/// schema-aware [`crate::sql::diagnostics`] pass against the live buffer each paint
/// and maps each finding to an error-styled wavy underline. Memoized on buffer
/// content so a repaint that changed nothing doesn't re-run the whole pass.
fn decoration_provider(
    index: Rc<CompletionIndex>,
) -> impl Fn(&str) -> Vec<flint::Decoration> + 'static {
    memoize_by_content(move |content| {
        crate::sql::diagnostics(content, index.as_ref(), index.dialect)
            .into_iter()
            .map(|d| flint::Decoration {
                range: d.range,
                style: flint::DecorationStyle::Error,
            })
            .collect()
    })
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
        if let Some(d) = crate::sql::diagnostics(content, index.as_ref(), index.dialect)
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
        for (_, table) in crate::sql::referenced_tables_at(content, offset, index.dialect) {
            if let Some(cols) = index.columns_by_table.get(&table.to_lowercase())
                && let Some(c) = cols.iter().find(|c| c.name.to_lowercase() == wl)
            {
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
        None
    }
}

/// Build a column completion: a `Field` badge, the type as detail, a short guide.
fn column_item(col: &ColumnCand) -> CompletionItem {
    let item = CompletionItem::new(col.name.clone(), CompletionKind::Field);
    if col.ty.is_empty() {
        item.documentation(crate::i18n::tr!(
            "editor.completion_doc_bare_column",
            "column"
        ))
    } else {
        item.detail(col.ty.clone()).documentation(crate::i18n::tr!(
            "editor.completion_doc_column",
            "{ty} column",
            ty = col.ty
        ))
    }
}

/// Build a table/view completion: an `Object` badge plus table-vs-view text.
fn table_item(t: &TableCand) -> CompletionItem {
    let (detail, doc) = if t.is_view {
        (
            crate::i18n::tr!("editor.completion_kind_view", "view"),
            crate::i18n::tr!("editor.completion_doc_view", "Database view."),
        )
    } else {
        (
            crate::i18n::tr!("editor.completion_kind_table", "table"),
            crate::i18n::tr!("editor.completion_doc_table", "Database table."),
        )
    };
    CompletionItem::new(t.name.clone(), CompletionKind::Object)
        .detail(detail)
        .documentation(doc)
}

/// Build a keyword completion: a `Keyword` badge plus any one-line guide.
fn keyword_item(kw: &SharedString) -> CompletionItem {
    let item = CompletionItem::new(kw.clone(), CompletionKind::Keyword).detail(crate::i18n::tr!(
        "editor.completion_kind_keyword",
        "keyword"
    ));
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
    let referenced = crate::sql::referenced_tables_at(content, cursor, index.dialect);
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
                    .detail(crate::i18n::tr!("editor.completion_kind_join", "join"))
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
        let context = crate::sql::analyze(content, cursor, index.dialect);

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
                let real = crate::sql::referenced_tables_at(content, cursor, index.dialect)
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
                for (_, table) in crate::sql::referenced_tables_at(content, cursor, index.dialect) {
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
    /// This pane's tab strip: one tab per open query in the pane, plus the ＋.
    ///
    /// Split out of [`AppState::render_editor`] because an ER tab replaces the
    /// editor entirely (see `render_pane`) yet still needs its strip — a diagram
    /// you cannot tab away from, and whose own tab you cannot see, is a trap.
    pub(crate) fn render_sql_tab_strip(
        &self,
        active: &ActiveConn,
        pane: crate::app::PaneId,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(ui) = active.layout.ui(pane) else {
            return div().into_any_element();
        };
        let active_idx = active.pane_active(pane);
        let tabs: Vec<StripTab> = active
            .pane_tab_indices(pane)
            .into_iter()
            .map(|i| StripTab {
                index: i,
                title: active.tabs[i].title.clone().into(),
                pinned: active.tabs[i].pinned,
                active: Some(i) == active_idx,
            })
            .collect();
        let strip = TabStrip::new(
            "sql",
            pane,
            ui.tab_scroll.clone(),
            move |this, i, cx| {
                // Clicking a tab in this pane's strip aims actions at this pane.
                this.set_split_focus(pane, cx);
                this.set_active_tab(i, cx);
            },
            |this, i, cx| this.request_close_tab(i, cx),
            move |this, cx| {
                this.set_split_focus(pane, cx);
                this.new_query(cx);
            },
            move |this, from, cx| this.drop_tab(from, pane, cx),
            move |this, slot, cx| this.set_tab_drop_target(pane, slot, cx),
            move |this, cx| this.clear_tab_drop_target(pane, cx),
        )
        .tabs(tabs)
        .gap(active.layout.gap_in(pane))
        // SQL keeps pinned tabs in their own fixed section rather than sorting
        // them first inline.
        .pinned_section(true)
        .new_tab_tooltip(crate::keymap::localize_hint(&format!(
            "{}  ⌘T",
            crate::i18n::tr!("editor.new_tab", "New tab")
        )))
        .on_menu(|this, i, position, cx| {
            this.tab_context_menu = Some((i, position));
            cx.notify();
        });
        self.render_tab_strip(strip, cx)
    }

    /// The editor area for the tab at `tab_idx`, shown in pane `pane`: the tab
    /// strip + breadcrumb + the `CodeEditor` surface + run bar. `is_focused` is
    /// whether that pane holds focus; the (single-instance) find bar renders only
    /// in the focused pane.
    pub(crate) fn render_editor(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        pane: crate::app::PaneId,
        is_focused: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // Owned (not borrowed from `cx`) so the agent-tab branch below can call a
        // `&mut cx` render method without clashing with the theme tokens this fn
        // snapshots throughout.
        let theme = cx.theme().clone();
        let (bg_app, bg_panel) = (theme.bg_app, theme.bg_panel);
        let (border, border_soft) = (theme.border, theme.border_soft);
        let (text, muted, faint, dim) = (
            theme.text,
            theme.text_muted,
            theme.text_faint,
            theme.text_dim,
        );
        let (yellow, on_accent) = (theme.yellow, theme.on_accent);
        // UI font + chrome sizes snapshotted (SharedString clones / Copy `Pixels`)
        // so the breadcrumb chrome tracks the UI font. The editor *surface* keeps
        // its own mono font.
        let ui_family = theme.font_family.clone();
        let (size_11, size_12) = (theme.scale(11.), theme.scale(12.));

        // The strip is shared with the ER-diagram path, which has no editor.
        let tabstrip = self.render_sql_tab_strip(active, pane, cx);

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
                .child(crate::i18n::tr!(
                    "editor.no_tab_open",
                    "No query tab open. Press ＋ to start"
                ));
            return div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .bg(bg_app)
                .child(tabstrip)
                .child(empty);
        };

        // --- breadcrumb: connection / database / query ---
        // The database crumb is the *interactive* namespace picker: this bar is
        // where the eye goes to answer "where am I", so the answer and the control
        // that changes it are the same widget. (It used to be a separate chip down
        // in the run bar, which put the answer among the action buttons and let it
        // disagree with the crumb next to it.)
        let ns_segment = self.render_namespace_picker(active, tab_idx, pane, cx);
        // A browse tab is titled "db.table"; with the database as its own crumb the
        // prefix is redundant, so drop it when the two agree. When they don't, the
        // qualified title is the honest thing to show and it stays.
        let crumb_title = match active.namespace_for_tab(tab_idx) {
            Some(ns) => tab
                .title
                .strip_prefix(&format!("{ns}."))
                .unwrap_or(&tab.title)
                .to_string(),
            None => tab.title.clone(),
        };
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
            // The deployment marker rides next to the connection name, where the eye
            // already goes to answer "where am I". Knowing you are on production
            // before typing is worth more than any dialog after.
            .when(active.config.env != ConnEnv::Unset, |bar| {
                bar.child(
                    Badge::new(active.config.env.label())
                        .variant(crate::connect::env_badge(active.config.env)),
                )
            })
            .when_some(ns_segment, |d, seg| {
                d.child(div().text_color(dim).child("/")).child(seg)
            })
            .child(div().text_color(dim).child("/"))
            .child(div().text_color(text).child(crumb_title));

        // The editor's own typography, applied here: the `CodeEditor` shapes its
        // text with `window.text_style()` / `window.line_height()`, both inherited
        // from this container, so setting them here drives the editor font without
        // it (or Flint) knowing about settings.
        let ed = &self.settings.editor;
        let surface = div()
            .flex_1()
            .min_h(px(0.))
            .relative()
            .font_family(ed.font_family.clone())
            .text_size(px(ed.font_size))
            .line_height(px(ed.font_size * ed.line_height))
            .child(tab.editor.clone())
            .children(
                self.focus_hint(crate::focus::FocusTargetId::Body {
                    pane,
                    area: crate::focus::BodyArea::Editor,
                })
                .map(|h| crate::focus_overlay::badge(h, cx)),
            );

        // --- bottom run bar: Run · Explain · Save · ……… · watch · read-only ---
        // (Query history now lives in the left dock, toggled with ⌘Y.)
        let ro_chip = active.config.read_only.then(|| {
            div()
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
                .child(crate::i18n::tr!("editor.read_only_badge", "read-only"))
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
                Button::new(
                    "sql-run",
                    crate::keymap::localize_hint(&format!(
                        "{}  ⌘↵",
                        crate::i18n::tr!("editor.run", "Run")
                    )),
                )
                .variant(ButtonVariant::Primary)
                .size(ButtonSize::Sm)
                .icon(crate::icons::icon("play", theme.scale(11.), on_accent))
                // Aim the action at the half this bar lives in, not whichever
                // half currently holds focus (mirrors the tab/＋ handlers).
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_split_focus(pane, cx);
                    this.run_editor_query(cx);
                })),
            )
            .child(
                Button::new("sql-explain", crate::i18n::tr!("editor.explain", "Explain"))
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_split_focus(pane, cx);
                        this.explain_query(false, cx);
                    })),
            )
            .child(
                Button::new("sql-save", crate::i18n::tr!("editor.save", "Save"))
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_split_focus(pane, cx);
                        this.open_save_prompt(cx);
                    })),
            )
            // Watch and read-only report state rather than invite a click, so
            // they sit at the far right, opposite the actions. One trailing
            // group holds them: two separate `ml_auto` children would split the
            // free space between them instead of pushing both to the edge.
            .child(
                div()
                    .ml_auto()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(self.render_watch_pill(active, tab_idx, pane, cx))
                    .children(ro_chip),
            );

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(bg_app)
            .child(tabstrip)
            .child(breadcrumb)
            // The find bar sits above the editor when ⌘F opened
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

    /// The breadcrumb's database crumb: which database (MySQL/ClickHouse) an
    /// unqualified `FROM users` in this tab resolves against, *and* the control
    /// that changes it.
    ///
    /// This is the affordance the feature exists for — before it, nothing on
    /// screen said what a bare table name would bind to, so a MySQL connection
    /// dialled without a database failed with the engine's bare "No database
    /// selected" and no way to act on it.
    ///
    /// It lives in the breadcrumb rather than the run bar because the breadcrumb
    /// is already the "where am I" line: `connection / database / table`. Split
    /// across two places the two could visibly disagree — the crumb reading one
    /// database while the picker read another — which is exactly the confusion a
    /// context indicator is supposed to remove.
    ///
    /// Hidden on engines whose namespace is fixed at connect (SQLite, Postgres),
    /// where a picker would be a lie, and the breadcrumb falls back to its plain
    /// `connection / title` form. Shows the tab's override when it has one, else
    /// the connection's; picking writes the *tab* override, so two tabs in a split
    /// can sit on two databases.
    ///
    /// Everything here is scoped to (`tab_idx`, `pane`) rather than to whatever
    /// holds focus: in a split each pane draws its own crumb, so a focus-scoped
    /// one would read the other half's database, and a connection-wide open flag
    /// would drop a menu into every half at once.
    fn render_namespace_picker(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        pane: crate::app::PaneId,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let caps = active.config.kind.namespace_caps();
        if !caps.settable {
            return None;
        }
        let theme = cx.theme();
        let (muted, text, yellow) = (theme.text_muted, theme.text, theme.yellow);
        let hover_bg = theme.bg_panel_2;
        let (radius, size_11, icon_sz) = (theme.radius_sm, theme.scale(11.), theme.scale(10.));
        let current = active.namespace_for_tab(tab_idx);
        let names: Vec<String> = active
            .schema
            .read(cx)
            .schemas
            .iter()
            .map(|s| s.name.clone())
            .collect();
        // Unset on an engine that *errors* without one (MySQL) is the state worth
        // shouting about, so the crumb goes warning-tinted rather than muted.
        let unset_is_a_problem = current.is_none() && caps.required;
        let tint = if unset_is_a_problem { yellow } else { text };
        let label = current
            .clone()
            .unwrap_or_else(|| format!("no {}", caps.label.to_lowercase()));

        // Deliberately hand-built rather than a Flint `Select`, even a `seamless`
        // one: that trigger hardcodes the base font size, medium weight, `px_2`
        // and a 24px height, and paints its focus/hover states in the *accent*
        // colour. In a 26px breadcrumb of 11px muted text that reads as a red
        // control dropped into a trail of labels. A crumb has to inherit the
        // trail's typography, so the affordance here is only a hover tint plus a
        // chevron. (If this shape recurs, it's the spike for a Flint breadcrumb
        // component — see CONTRIBUTING's "spike in RED first".)
        let menu = (active.namespace_menu_pane == Some(pane)).then(|| {
            // Ids are per-pane: two panes rendering the same id would collide in
            // GPUI's element tree.
            let mut menu =
                ContextMenu::new(SharedString::from(format!("sql-namespace-menu-{}", pane.0)));
            for name in &names {
                let is_current = current.as_deref() == Some(name.as_str());
                let mut item = ContextMenuItem::new(
                    SharedString::from(format!("ns-opt-{}-{name}", pane.0)),
                    name.clone(),
                )
                .on_click(cx.listener({
                    let name = name.clone();
                    move |this, _, _, cx| {
                        this.close_namespace_menu(cx);
                        // Picking in a half aims at that half, like the run bar's
                        // buttons and the tab strip.
                        this.set_split_focus(pane, cx);
                        this.set_tab_namespace(pane, Some(name.clone()), cx);
                    }
                }));
                // The trailing slot right-aligns the mark, so the names stay flush
                // left instead of being indented by a checkmark column.
                if is_current {
                    item = item.shortcut("✓");
                }
                menu = menu.item(item);
            }
            // Dismissal rides the surface's own outside-click (what Flint's
            // `Select`/`ComboBox` do) rather than a full-bleed catcher, which in
            // this bar would only have covered the breadcrumb strip.
            floating(
                div()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_namespace_menu(cx)))
                    .child(menu),
            )
            .offset(gpui::point(px(0.), px(20.)))
        });

        Some(
            div()
                .relative()
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "sql-namespace-crumb-{}",
                            pane.0
                        )))
                        .flex()
                        .items_center()
                        .gap_1()
                        // Tight, text-scale padding: enough for the hover tint to
                        // read as a target without the crumb growing a control-sized
                        // box inside a 26px bar.
                        .px_1()
                        .py(px(1.))
                        .rounded(radius)
                        .text_size(size_11)
                        .text_color(tint)
                        .cursor_pointer()
                        // Neutral, not accent: this is a label you can click, not a
                        // primary action.
                        .hover(|s| s.bg(hover_bg))
                        .child(crate::icons::icon("database", icon_sz, muted))
                        .child(label)
                        .child(crate::icons::icon("chevron-down", icon_sz, muted))
                        .when(unset_is_a_problem, |d| {
                            d.tooltip(Tooltip::text(format!(
                                "Unqualified table names have no target {}. \
                                 Pick one, or write them as db.table.",
                                caps.label.to_lowercase()
                            )))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_split_focus(pane, cx);
                            this.toggle_namespace_menu(pane, cx);
                        })),
                )
                .children(menu),
        )
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
    fn resolve_browse_table(&self, sql: &str, cx: &App) -> Option<(String, String)> {
        let (schema_hint, table) = crate::sql::single_table_star(sql)?;
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let resolved = resolve_in_catalog(
            &active.schema.read(cx).schemas,
            schema_hint.as_deref(),
            &table,
        );
        tracing::debug!(
            ?schema_hint, %table, ns = active.schema.read(cx).schemas.len(), ?resolved,
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

    /// The SQL lexical dialect of the active connection's engine, for every
    /// scanner call made on the run path; [`crate::sql::Dialect::Generic`] when
    /// nothing is connected (those paths bail before running anything).
    pub(crate) fn active_dialect(&self) -> crate::sql::Dialect {
        match &self.phase {
            Phase::Connected(active) => crate::sql::Dialect::of(active.config.kind),
            _ => crate::sql::Dialect::Generic,
        }
    }

    /// The dialect of a *specific* session's engine (foreground or parked), for a
    /// background reply that must be scanned against the connection that asked
    /// for it rather than whatever is on screen. `None` when that session isn't
    /// live.
    pub(crate) fn conn_dialect(
        &self,
        session: Option<red_service::SessionId>,
    ) -> Option<crate::sql::Dialect> {
        let id = session?;
        if self.foreground_session == Some(id)
            && let Phase::Connected(active) = &self.phase
        {
            return Some(crate::sql::Dialect::of(active.config.kind));
        }
        self.parked
            .get(&id)
            .map(|a| crate::sql::Dialect::of(a.config.kind))
    }

    fn run_editor_query_impl(&mut self, force_offset: Option<usize>, cx: &mut Context<Self>) {
        // A Redis session has no SQL editor — its `query 1` tab is a phantom that
        // is never rendered (the Redis shell replaces the editor). ⌘↵ / gutter-run
        // must not construct and send a SQL statement against it. Same for an ER
        // diagram tab: its editor exists but is never shown, so running it would
        // fire the untouched placeholder SQL at the server.
        if matches!(&self.phase, Phase::Connected(a) if a.kv_view.is_some()) {
            return;
        }
        if matches!(&self.phase, Phase::Connected(a) if a.active().is_some_and(|t| t.is_er())) {
            return;
        }
        let dialect = self.active_dialect();
        let sql = match &self.phase {
            Phase::Connected(active) => match active.active() {
                Some(tab) => {
                    let editor = tab.editor.read(cx);
                    match force_offset {
                        // A clicked gutter marker runs exactly its statement.
                        Some(off) => {
                            let content = editor.content();
                            crate::sql::statement_at(&content, off, dialect).to_string()
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
                                crate::sql::statement_at(&content, editor.cursor_offset(), dialect)
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
            self.query_history
                .update(cx, |store, _| store.record(&conn_id, &sql));
        }

        // Grade the statement: `Safe` streams into the grid, everything else executes
        // in a transaction, and the configured threshold decides which of those first
        // stop to ask.
        let assessment = red_core::sql::assess(&sql, dialect);

        // On a read-only connection, refuse writes up front instead of letting
        // them round-trip to the engine and bounce back as a cryptic error. The
        // engine still rejects writes as a backstop; this is the friendly gate.
        let read_only = matches!(&self.phase, Phase::Connected(active) if active.config.read_only);
        if read_only && assessment.level > RiskLevel::Safe {
            self.notify(
                ToastVariant::Error,
                crate::i18n::tr!(
                    "editor.read_only_blocked",
                    "Connection is read-only; write statements are disabled."
                ),
                cx,
            );
            return;
        }

        if assessment.level == RiskLevel::Safe {
            // A row-returning batch can't open as one result: the paging path
            // wraps the SQL in a single `SELECT * FROM (<sql>) AS _red`, which a
            // `;`-separated batch makes a syntax error. Only an explicit
            // multi-statement selection reaches here (a no-selection run already
            // narrowed to the caret's statement); say so plainly.
            if crate::sql::statement_count(&sql, dialect) > 1 {
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
            let table = self.resolve_browse_table(&sql, cx);
            // Guard a bare `SELECT *` against flooding the grid: append the
            // configured `LIMIT` unless the user wrote their own.
            let sql = crate::sql::auto_limit(&sql, self.settings.sql.auto_limit).unwrap_or(sql);
            self.open_result("query", sql, table, cx);
            return;
        }

        if self.confirm_policy().requires(assessment.level) {
            self.open_confirm(sql, assessment, cx);
        } else {
            self.execute_sql(sql, cx);
        }
    }

    /// Raise the confirmation modal for a graded statement, arming the type-to-confirm
    /// box when the grade is [`RiskLevel::Critical`] and the doomed object could be
    /// named. Everything below `Critical`, and anything critical whose target could
    /// not be extracted, gets the ordinary Cancel/Run modal.
    fn open_confirm(
        &mut self,
        sql: String,
        assessment: red_core::sql::Assessment,
        cx: &mut Context<Self>,
    ) {
        self.confirm_input = self
            .confirm_policy()
            .requires_typing(assessment.level)
            .then(|| assessment.confirm_target())
            .flatten()
            .map(|target| ConfirmInput::new(target.to_string(), cx));
        // Ask the engine how much this touches. Fired here rather than awaited, so
        // the dialog opens on the grading immediately and the number fills in.
        self.confirm_count_token = self.confirm_count_token.wrapping_add(1);
        let namespace = match &self.phase {
            Phase::Connected(active) => active.namespace_for_send(),
            _ => None,
        };
        let dialect = self.active_dialect();
        self.confirm_count = red_core::sql::count_preflight(&sql, dialect).map(|count_sql| {
            self.send_active(Command::CountMatching {
                sql: count_sql,
                namespace,
                token: self.confirm_count_token,
            });
            PreflightCount::Pending
        });
        // The advisory review, when the user opted in. Gated on the grading rather
        // than fired on every run: a round-trip per statement would contradict the
        // point of the app, and the deterministic guards are what actually decide
        // whether to stop. This only ever adds a line to a dialog already open.
        self.confirm_review =
            (self.settings.safety.ai_review && assessment.level >= RiskLevel::Risky).then(|| {
                // Which agent runs this isn't a separate setting: it follows the
                // assistant panel. Carry its display name so the line can say who
                // answered rather than leaving the user to guess.
                let agent = self.assistant_agent_id();
                let name = self.agent_name(&agent);
                self.send_active(Command::AssessSql {
                    sql: sql.clone(),
                    agent,
                    schema_summary: self.review_schema_context(assessment.table.as_deref(), cx),
                    token: self.confirm_count_token,
                });
                AiReview {
                    agent: name,
                    state: AiReviewState::Pending,
                }
            });
        self.confirm_exec =
            self.pending_confirm(crate::app::PendingWrite::EditorSql { sql, assessment });
        // Focus the modal (or, when there is one, its type-to-confirm box) so its
        // Enter/Esc handling is heard.
        self.focus_modal = true;
        cx.notify();
    }

    /// Beautify the active editor's SQL in place (⌥⌘F / palette / Query menu):
    /// re-indent, upper-case keywords, and break clauses onto their own lines. It
    /// reformats the whole buffer (a single undo step) and never touches the
    /// database. A blank or already-formatted buffer is a no-op.
    pub(crate) fn format_active_sql(&mut self, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        // `is_er`: the diagram's editor is a never-rendered placeholder, so
        // formatting it would rewrite a buffer the user can't see.
        let Some(tab) = active.active().filter(|t| !t.is_er()) else {
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
        let Some(session) = self.foreground_session else {
            return;
        };
        self.execute_sql_on(session, sql, cx);
    }

    /// [`execute_sql`](Self::execute_sql) against a named connection rather than the
    /// foreground one.
    ///
    /// The confirm modal takes this route: the connection can change between raising
    /// a destructive confirm and clicking Run (⌘P / ⌘1-9 are root globals), and both
    /// the read-only gate and the namespace have to be read from the connection the
    /// statement will actually run on.
    pub(crate) fn execute_sql_on(
        &mut self,
        session: red_service::SessionId,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.conn_for(Some(session)) else {
            return;
        };
        if conn.config.read_only {
            self.notify(
                ToastVariant::Error,
                crate::i18n::tr!(
                    "editor.read_only_blocked",
                    "Connection is read-only; write statements are disabled."
                ),
                cx,
            );
            return;
        }
        let namespace = conn.namespace_for_send();
        if let Some(conn) = self.conn_mut(Some(session)) {
            conn.write_in_flight = true;
        }
        self.service
            .send_to(session, Command::Execute { sql, namespace });
        cx.notify();
    }

    /// Show or hide the History panel in the left dock (status-bar toggle, ⌘Y, or
    /// palette). Opening focuses its list; closing returns focus to the editor.
    pub(crate) fn toggle_history(&mut self, cx: &mut Context<Self>) {
        let opened = if let Phase::Connected(active) = &mut self.phase {
            active.history_open = !active.history_open;
            Some(active.history_open)
        } else {
            None
        };
        // Reset the keyboard highlight to the top whenever it opens. Done
        // outside the borrow above so the panel can be updated through `cx`.
        if opened == Some(true)
            && let Phase::Connected(active) = &self.phase
        {
            active
                .history_panel
                .update(cx, |panel, cx| panel.reset_selection(cx));
        }
        match opened {
            // Focus the panel's list so its arrow keys work; closing returns focus
            // to the editor.
            Some(true) => self.focus_history = true,
            Some(false) => self.pending_focus = Some(crate::app::Pane::Editor),
            None => {}
        }
        cx.notify();
    }

    /// React to the SQL History dock. Bucket collapse, keyboard selection,
    /// per-row delete and clear all live on the panel now (it owns that state
    /// and holds the store), so what reaches here is only what needs the shell:
    /// seeding an editor, and moving focus.
    pub(crate) fn on_history_event(
        &mut self,
        _panel: gpui::Entity<crate::history::HistoryPanel>,
        event: &crate::history::HistoryPanelEvent,
        cx: &mut Context<Self>,
    ) {
        use crate::history::HistoryPanelEvent as E;
        match event {
            E::Open { sql, replace } => self.open_history(sql.clone(), *replace, cx),
            E::Accept { sql } => {
                // Fill the editor; the panel stays open so the user can keep
                // browsing.
                self.load_history(sql.clone(), cx);
                self.pending_focus = Some(crate::app::Pane::Editor);
            }
            E::Close => self.toggle_history(cx),
            E::LeaveToEditor => self.pending_focus = Some(crate::app::Pane::Editor),
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
            self.query_history
                .update(cx, |store, _| store.clear_conn(&conn_id));
        }
        if let Phase::Connected(active) = &self.phase {
            active
                .history_panel
                .update(cx, |panel, cx| panel.reset_selection(cx));
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
            let tab = crate::app::QueryTab::new(history_label(&sql), self.active_dialect(), cx);
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
                    &active.schema.read(cx).schemas,
                    &active.schema.read(cx).details,
                    &active.schema.read(cx).fk_graph,
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
        self.refresh_filter_completions(cx);
    }

    /// (Re)install completion + diagnostics on the filter bar's `WHERE` box, from
    /// the active connection's schema and the *active result's* columns. Called
    /// when the bar opens, when the active result changes, and as the schema grows.
    /// A no-op with the bar closed or before a connection is up.
    pub(crate) fn refresh_filter_completions(&mut self, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let Some(bar) = &self.filter_bar else {
            return;
        };
        let index = Rc::new(build_index(
            &active.schema.read(cx).schemas,
            &active.schema.read(cx).details,
            &active.schema.read(cx).fk_graph,
            active.config.kind,
        ));
        let (table, columns) = match active.active_result() {
            Some(grid) => (grid.browsed_table(), grid.column_names()),
            None => (None, Vec::new()),
        };
        let expr = bar.expr.clone();
        expr.update(cx, |editor, cx| {
            editor.set_rich_completions(
                filter_completion_provider(index.clone(), table.clone(), columns),
                cx,
            );
            editor.set_decorations(filter_decoration_provider(index, table), cx);
        });
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
    ) -> impl IntoElement + use<> {
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
        let pin_label = if pinned {
            crate::i18n::tr!("editor.unpin_tab", "Unpin tab")
        } else {
            crate::i18n::tr!("editor.pin_tab", "Pin tab")
        };
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
                ContextMenuItem::new("tab-close", crate::i18n::tr!("editor.tab_close", "Close"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab_context_menu = None;
                        this.close_tab_group(index, TabCloseScope::One, cx);
                    })),
            )
            .item(
                ContextMenuItem::new(
                    "tab-close-others",
                    crate::i18n::tr!("editor.tab_close_others", "Close Others"),
                )
                .disabled(!has_others)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab_context_menu = None;
                    this.close_tab_group(index, TabCloseScope::Others, cx);
                })),
            )
            .item(
                ContextMenuItem::new(
                    "tab-close-left",
                    crate::i18n::tr!("editor.tab_close_left", "Close Left"),
                )
                .disabled(!has_left)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab_context_menu = None;
                    this.close_tab_group(index, TabCloseScope::Left, cx);
                })),
            )
            .item(
                ContextMenuItem::new(
                    "tab-close-right",
                    crate::i18n::tr!("editor.tab_close_right", "Close Right"),
                )
                .disabled(!has_right)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab_context_menu = None;
                    this.close_tab_group(index, TabCloseScope::Right, cx);
                })),
            )
            .item(
                ContextMenuItem::new(
                    "tab-close-all",
                    crate::i18n::tr!("editor.tab_close_all", "Close All"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab_context_menu = None;
                    this.close_tab_group(index, TabCloseScope::All, cx);
                })),
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
        CompletionIndex, build_index, completion_provider, hover_provider, join_items,
        resolve_in_catalog,
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
