// SPDX-License-Identifier: GPL-3.0-or-later

//! The SQL editor pane (M4): a toolbar (Run · history · read-only badge) over
//! Flint's `CodeEditor`. RED owns the domain bits — the SQL highlighter, the
//! completion candidates fed into the editor's generic completion seam, running
//! the current statement (or selection), and the query history. Results land in
//! the same interim grid the schema preview uses (M5 replaces that renderer).

use flint::prelude::*;
use gpui::{div, prelude::*, px, Context, SharedString};

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
        let (bg_app, bg_panel, bg_elevated, bg_hover, border) = (
            theme.bg_app,
            theme.bg_panel,
            theme.bg_elevated,
            theme.bg_hover,
            theme.border,
        );
        let (text, faint, yellow, radius) =
            (theme.text, theme.text_faint, theme.yellow, theme.radius);
        let view = cx.entity().downgrade();

        let toolbar = div()
            .flex_shrink_0()
            .h(px(34.))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .bg(bg_panel)
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(px(11.))
                    .text_color(faint)
                    .child("SQL editor"),
            )
            .child(
                div()
                    .ml_auto()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(active.config.read_only, |d| {
                        d.child(
                            div()
                                .text_size(px(10.))
                                .text_color(yellow)
                                .child("read-only"),
                        )
                    })
                    .child(
                        Button::new("sql-history", "History")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_history(cx))),
                    )
                    .child(
                        Button::new("sql-run", "Run  ⌘↵")
                            .variant(ButtonVariant::Primary)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.run_editor_query(cx))),
                    ),
            );

        let surface = div()
            .flex_1()
            .min_h(px(0.))
            .child(active.editor.clone());

        let history = active.history_open.then(|| {
            let list: Vec<_> = active.history.clone();
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
            div()
                .absolute()
                .top(px(34.))
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
            .child(toolbar)
            .child(surface)
            .children(history)
    }

    /// Run the selection if any, else the whole buffer. Pushes to history and
    /// streams the first window into the results pane.
    pub(crate) fn run_editor_query(&mut self, cx: &mut Context<Self>) {
        let sql = match &self.phase {
            Phase::Connected(active) => {
                let editor = active.editor.read(cx);
                editor.selected_text().unwrap_or_else(|| editor.content())
            }
            _ => return,
        };
        let sql = sql.trim().to_string();
        if sql.is_empty() {
            return;
        }

        if let Phase::Connected(active) = &mut self.phase {
            // De-dupe consecutive identical runs at the head of the history.
            if active.history.first() != Some(&sql) {
                active.history.insert(0, sql.clone());
                active.history.truncate(50);
            }
            active.history_open = false;
        }
        self.open_result("query", sql, cx);
    }

    pub(crate) fn toggle_history(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.history_open = !active.history_open;
        }
        cx.notify();
    }

    pub(crate) fn load_history(&mut self, sql: String, cx: &mut Context<Self>) {
        let editor = match &mut self.phase {
            Phase::Connected(active) => {
                active.history_open = false;
                active.editor.clone()
            }
            _ => return,
        };
        editor.update(cx, |editor, cx| editor.set_content(sql, cx));
        cx.notify();
    }

    /// Rebuild the editor's completion candidates from the current schema. Called
    /// when the skeleton or a table's detail arrives.
    pub(crate) fn refresh_completions(&mut self, cx: &mut Context<Self>) {
        let (editor, candidates) = match &self.phase {
            Phase::Connected(active) => {
                (active.editor.clone(), build_candidates(&active.schema))
            }
            _ => return,
        };
        editor.update(cx, |editor, cx| {
            editor.set_completions(completion_provider(candidates), cx)
        });
    }
}
