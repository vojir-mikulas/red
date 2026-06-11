//! The SQL editor pane: a toolbar (Run · history · read-only badge) over
//! Flint's `CodeEditor`. RED owns the domain bits — the SQL highlighter, the
//! completion candidates fed into the editor's generic completion seam, running
//! the current statement (or selection), and the query history. Results land in
//! the result grid.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use flint::prelude::*;
use gpui::{div, prelude::*, px, Context, SharedString};
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase};
use crate::assets::FONT_MONO;
use crate::schema::SchemaState;
use crate::sql::CompletionContext;

/// How many candidates the popup ever shows — the editor renders at most 8, but
/// we hand it a few more so prefix-narrowing has headroom.
const MAX_CANDIDATES: usize = 20;

/// The completion candidates derived from the loaded schema, grouped so the
/// provider can rank them by the cursor's context. Rebuilt as the schema grows.
struct CompletionIndex {
    /// Every object (table/view) name, sorted + deduped.
    tables: Vec<SharedString>,
    /// Columns keyed by lower-cased table name, for `table.`/`alias.` completion.
    columns_by_table: HashMap<String, Vec<SharedString>>,
    /// Every column name across the schema, sorted + deduped.
    all_columns: Vec<SharedString>,
    /// The upper-cased SQL keywords.
    keywords: Vec<SharedString>,
}

fn build_index(schema: &SchemaState) -> CompletionIndex {
    let mut tables: Vec<SharedString> = Vec::new();
    for sc in &schema.schemas {
        for obj in &sc.objects {
            tables.push(obj.name.clone().into());
        }
    }

    let mut columns_by_table: HashMap<String, Vec<SharedString>> = HashMap::new();
    let mut all_columns: Vec<SharedString> = Vec::new();
    for ((_, table), detail) in &schema.details {
        let entry = columns_by_table.entry(table.to_lowercase()).or_default();
        for col in &detail.columns {
            entry.push(col.name.clone().into());
            all_columns.push(col.name.clone().into());
        }
    }

    tables.sort();
    tables.dedup();
    all_columns.sort();
    all_columns.dedup();
    for cols in columns_by_table.values_mut() {
        cols.sort();
        cols.dedup();
    }

    let keywords = crate::sql::KEYWORDS
        .iter()
        .map(|kw| SharedString::from(kw.to_uppercase()))
        .collect();

    CompletionIndex {
        tables,
        columns_by_table,
        all_columns,
        keywords,
    }
}

/// The provider closure handed to the editor's completion seam. It reads the
/// cursor's context (member access, a table position, a column expression, or a
/// statement start) and offers the matching candidates, most-relevant first.
fn completion_provider(
    index: Rc<CompletionIndex>,
) -> impl Fn(&str, usize) -> Vec<SharedString> + 'static {
    move |content, cursor| {
        let prefix = crate::sql::word_prefix(content, cursor).to_lowercase();
        let context = crate::sql::analyze(content, cursor);

        // Only member access (`table.`) suggests with nothing typed; elsewhere we
        // wait for a prefix so the popup doesn't open on every space.
        if prefix.is_empty() && !matches!(context, CompletionContext::Dot { .. }) {
            return Vec::new();
        }

        // Candidate sources in priority order — earlier groups win ties.
        let mut ordered: Vec<SharedString> = Vec::new();
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
                            .find(|t| t.to_lowercase() == q)
                            .map(|t| t.to_lowercase())
                    });
                if let Some(cols) = real.and_then(|r| index.columns_by_table.get(&r)) {
                    ordered.extend(cols.iter().cloned());
                }
            }
            CompletionContext::Table => ordered.extend(index.tables.iter().cloned()),
            CompletionContext::Column => {
                // Columns of the tables this statement actually references rank
                // first, then the rest of the schema, then tables and keywords.
                for (_, table) in crate::sql::referenced_tables_at(content, cursor) {
                    if let Some(cols) = index.columns_by_table.get(&table.to_lowercase()) {
                        ordered.extend(cols.iter().cloned());
                    }
                }
                ordered.extend(index.all_columns.iter().cloned());
                ordered.extend(index.tables.iter().cloned());
                ordered.extend(index.keywords.iter().cloned());
            }
            CompletionContext::Keyword => {
                ordered.extend(index.keywords.iter().cloned());
                ordered.extend(index.tables.iter().cloned());
            }
        }

        let mut seen: HashSet<String> = HashSet::new();
        ordered
            .into_iter()
            .filter(|c| {
                let cl = c.to_lowercase();
                if !cl.starts_with(&prefix) || (!prefix.is_empty() && cl == prefix) {
                    return false;
                }
                seen.insert(cl)
            })
            .take(MAX_CANDIDATES)
            .collect()
    }
}

/// First non-empty, non-comment line of a query, truncated — the history label.
fn history_label(sql: &str) -> String {
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
    /// The editor pane: toolbar + the `CodeEditor` surface + a history popover.
    pub(crate) fn render_editor(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (bg_app, bg_panel, bg_elevated, bg_hover) = (
            theme.bg_app,
            theme.bg_panel,
            theme.bg_elevated,
            theme.bg_hover,
        );
        let (border, border_soft, radius) = (theme.border, theme.border_soft, theme.radius);
        let (text, muted, faint, dim) = (
            theme.text,
            theme.text_muted,
            theme.text_faint,
            theme.text_dim,
        );
        let (yellow, green, on_accent) = (theme.yellow, theme.green, theme.on_accent);
        let view = cx.entity().downgrade();

        // --- tab strip: one tab per open query + a "new query" affordance ---
        let active_idx = active.active_tab;
        let tabs = active.tabs.iter().enumerate().map(|(i, t)| {
            let is_active = i == active_idx;
            let (tab_bg, tab_text, icon_color) = if is_active {
                (bg_app, text, green)
            } else {
                (bg_panel, muted, dim)
            };
            div()
                .id(("sql-tab", i))
                .flex()
                .items_center()
                .gap_1p5()
                .px_2p5()
                .bg(tab_bg)
                .border_r_1()
                .border_color(border)
                .cursor_pointer()
                .when(!is_active, |d| d.hover(|s| s.bg(bg_elevated)))
                .on_click(cx.listener(move |this, _, _, cx| this.set_active_tab(i, cx)))
                .child(crate::icons::icon("play", px(10.), icon_color))
                .child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(12.))
                        .text_color(tab_text)
                        .child(t.title.clone()),
                )
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
                        .child(crate::icons::icon("close", px(9.), faint)),
                )
        });
        let tabstrip = div()
            .flex_shrink_0()
            .h(px(35.))
            .flex()
            .items_stretch()
            .bg(bg_panel)
            .border_b_1()
            .border_color(border)
            .children(tabs)
            .child(
                div()
                    .id("sql-new")
                    .w(px(34.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_color(faint)
                    .hover(|s| s.bg(bg_elevated).text_color(text))
                    .on_click(cx.listener(|this, _, _, cx| this.new_query(cx)))
                    .child(crate::icons::icon("plus", px(13.), faint)),
            );

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
            .font_family(FONT_MONO)
            .text_size(px(11.))
            .text_color(muted)
            .child(active.config.name.clone())
            .child(div().text_color(dim).child("/"))
            .child(div().text_color(text).child(active.active().title.clone()));

        let surface = div()
            .flex_1()
            .min_h(px(0.))
            .child(active.active().editor.clone());

        // --- bottom run bar: Run · history · hint · read-only · endpoint ---
        let ro_chip = active.config.read_only.then(|| {
            div()
                .flex()
                .items_center()
                .px_2()
                .py(px(2.))
                .gap_1()
                .rounded(theme.radius_sm)
                .bg(yellow.opacity(0.1))
                .text_size(px(11.))
                .text_color(yellow)
                .child(crate::icons::icon("lock", px(11.), yellow))
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
                Button::new("sql-run", "Run  ⌘↵")
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Sm)
                    .icon(crate::icons::icon("play", px(11.), on_accent))
                    .on_click(cx.listener(|this, _, _, cx| this.run_editor_query(cx))),
            )
            .child(
                Button::new("sql-history", "History")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_history(cx))),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(dim)
                    .child("Press ⌘↵ to execute"),
            )
            .children(ro_chip)
            .child(
                div()
                    .ml_auto()
                    .font_family(FONT_MONO)
                    .text_size(px(11.))
                    .text_color(dim)
                    .child(active.config.display_target()),
            );

        let history = active.active().history_open.then(|| {
            let list: Vec<_> = active.active().history.clone();
            let inner = if list.is_empty() {
                div()
                    .px_2()
                    .py_1()
                    .text_size(px(11.))
                    .text_color(faint)
                    .child("No history yet")
                    .into_any_element()
            } else {
                div()
                    .id("sql-history-list")
                    .max_h(px(260.))
                    .overflow_y_scroll()
                    .children(list.into_iter().enumerate().map(|(i, q)| {
                        let v = view.clone();
                        let sql = q.clone();
                        div()
                            .id(("hist", i))
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .font_family(FONT_MONO)
                            .text_size(px(11.))
                            .text_color(text)
                            .hover(move |s| s.bg(bg_hover))
                            .on_click(move |_, _, cx| {
                                let sql = sql.clone();
                                v.update(cx, |this, cx| this.load_history(sql, cx)).ok();
                            })
                            .child(history_label(&q))
                    }))
                    .into_any_element()
            };
            // Anchored to the bottom run bar, opening upward.
            div()
                .absolute()
                .bottom(px(38.))
                .right(px(8.))
                .w(px(380.))
                .bg(bg_elevated)
                .border_1()
                .border_color(border)
                .rounded(radius)
                .overflow_hidden()
                .child(inner)
        });

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(bg_app)
            .child(tabstrip)
            .child(breadcrumb)
            .child(surface)
            .child(run_bar)
            .children(history)
    }

    /// Run the selection if any, else the whole buffer. Pushes to history and
    /// streams the first window into the results pane.
    pub(crate) fn run_editor_query(&mut self, cx: &mut Context<Self>) {
        let sql = match &self.phase {
            Phase::Connected(active) => {
                let editor = active.active().editor.read(cx);
                editor.selected_text().unwrap_or_else(|| editor.content())
            }
            _ => return,
        };
        let sql = sql.trim().to_string();
        if sql.is_empty() {
            return;
        }

        if let Phase::Connected(active) = &mut self.phase {
            let tab = active.active_mut();
            // De-dupe consecutive identical runs at the head of the history.
            if tab.history.first() != Some(&sql) {
                tab.history.insert(0, sql.clone());
                tab.history.truncate(50);
            }
            tab.history_open = false;
        }

        // Row-returning statements stream into the grid; writes execute in a
        // transaction; destructive writes ask for confirmation first.
        let kind = crate::sql::classify(&sql);

        // On a read-only connection, refuse writes up front instead of letting
        // them round-trip to the engine and bounce back as a cryptic error. The
        // engine still rejects writes as a backstop; this is the friendly gate.
        let read_only = matches!(&self.phase, Phase::Connected(active) if active.config.read_only);
        if read_only && !matches!(kind, crate::sql::StatementKind::Query) {
            self.toast = Some((
                "Connection is read-only — write statements are disabled.".into(),
                ToastVariant::Error,
            ));
            cx.notify();
            return;
        }

        match kind {
            crate::sql::StatementKind::Query => {
                // Guard a bare `SELECT *` against flooding the grid: append the
                // configured `LIMIT` unless the user wrote their own.
                let sql =
                    crate::sql::auto_limit(&sql, self.settings.query.auto_limit).unwrap_or(sql);
                self.open_result("query", sql, None, cx)
            }
            crate::sql::StatementKind::Write => self.execute_sql(sql, cx),
            crate::sql::StatementKind::Destructive => {
                // The safety rail is opt-out in settings; when off, run immediately.
                if self.settings.query.confirm_destructive {
                    self.confirm_exec = Some(sql);
                    cx.notify();
                } else {
                    self.execute_sql(sql, cx);
                }
            }
        }
    }

    /// Run a write/DDL statement in a transaction; refresh the schema tree after,
    /// since it may have created or dropped objects. The single seam through which
    /// writes leave the UI — so it also enforces the read-only gate, catching any
    /// caller that didn't pre-check (e.g. future inline-edit paths).
    pub(crate) fn execute_sql(&mut self, sql: String, cx: &mut Context<Self>) {
        if matches!(&self.phase, Phase::Connected(active) if active.config.read_only) {
            self.toast = Some((
                "Connection is read-only — write statements are disabled.".into(),
                ToastVariant::Error,
            ));
            cx.notify();
            return;
        }
        self.service.send(Command::Execute { sql });
        cx.notify();
    }

    pub(crate) fn toggle_history(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            let tab = active.active_mut();
            tab.history_open = !tab.history_open;
        }
        cx.notify();
    }

    pub(crate) fn load_history(&mut self, sql: String, cx: &mut Context<Self>) {
        let editor = match &mut self.phase {
            Phase::Connected(active) => {
                let tab = active.active_mut();
                tab.history_open = false;
                tab.editor.clone()
            }
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
                Rc::new(build_index(&active.schema)),
            ),
            _ => return,
        };
        for editor in editors {
            let index = index.clone();
            editor.update(cx, |editor, cx| {
                editor.set_completions(completion_provider(index), cx)
            });
        }
    }
}
