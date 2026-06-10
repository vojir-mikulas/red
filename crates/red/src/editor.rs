//! The SQL editor pane (M4): a toolbar (Run · history · read-only badge) over
//! Flint's `CodeEditor`. RED owns the domain bits — the SQL highlighter, the
//! completion candidates fed into the editor's generic completion seam, running
//! the current statement (or selection), and the query history. Results land in
//! the same interim grid the schema preview uses (M5 replaces that renderer).

use flint::prelude::*;
use gpui::{div, prelude::*, px, Context, SharedString};
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase};
use crate::assets::FONT_MONO;
use crate::schema::SchemaState;

/// Completion candidates from the loaded schema: every object + every known
/// column name, plus the upper-cased SQL keywords. Rebuilt as the schema grows.
fn build_candidates(schema: &SchemaState) -> Vec<SharedString> {
    let mut out: Vec<SharedString> = Vec::new();
    for sc in &schema.schemas {
        for obj in &sc.objects {
            out.push(obj.name.clone().into());
        }
    }
    for detail in schema.details.values() {
        for col in &detail.columns {
            out.push(col.name.clone().into());
        }
    }
    for kw in crate::sql::KEYWORDS {
        out.push(kw.to_uppercase().into());
    }
    out.sort();
    out.dedup();
    out
}

/// The provider closure handed to the editor's completion seam: filter the
/// candidate snapshot by the word under the cursor (case-insensitive prefix).
fn completion_provider(
    candidates: Vec<SharedString>,
) -> impl Fn(&str, usize) -> Vec<SharedString> + 'static {
    move |content, cursor| {
        let prefix = crate::sql::word_prefix(content, cursor);
        if prefix.is_empty() {
            return Vec::new();
        }
        let lower = prefix.to_lowercase();
        candidates
            .iter()
            .filter(|c| {
                let cl = c.to_lowercase();
                cl.starts_with(&lower) && cl != lower
            })
            .take(20)
            .cloned()
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
            .h(px(34.))
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
        match crate::sql::classify(&sql) {
            crate::sql::StatementKind::Query => self.open_result("query", sql, None, cx),
            crate::sql::StatementKind::Write => self.execute_sql(sql, cx),
            crate::sql::StatementKind::Destructive => {
                // The safety rail is opt-out in settings; when off, run immediately.
                if self.settings.confirm_destructive {
                    self.confirm_exec = Some(sql);
                    cx.notify();
                } else {
                    self.execute_sql(sql, cx);
                }
            }
        }
    }

    /// Run a write/DDL statement in a transaction; refresh the schema tree after,
    /// since it may have created or dropped objects.
    pub(crate) fn execute_sql(&mut self, sql: String, cx: &mut Context<Self>) {
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
        let (editors, candidates) = match &self.phase {
            Phase::Connected(active) => (
                active
                    .tabs
                    .iter()
                    .map(|t| t.editor.clone())
                    .collect::<Vec<_>>(),
                build_candidates(&active.schema),
            ),
            _ => return,
        };
        for editor in editors {
            let candidates = candidates.clone();
            editor.update(cx, |editor, cx| {
                editor.set_completions(completion_provider(candidates), cx)
            });
        }
    }
}
