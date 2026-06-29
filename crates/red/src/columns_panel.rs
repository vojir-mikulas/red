//! The Columns panel: inline foreign-key expansion (Track B7).
//!
//! A recursive, lazy tree that lets a single-table browse pull columns from
//! *referenced* tables inline. The base table's columns are listed; an FK column
//! expands (chevron) into the referenced table's columns, recursively
//! (`tier → cascade → placement`); checking a column adds it to the grid as an
//! extra, dotted-aliased column via a `LEFT JOIN` (see [`crate::result::build_joins`]
//! and the driver's `fk_join_wrap`). This is dbgate's data-grid column tree, built on
//! RED's windowed cursor — the join decorates one page, never the whole table.
//!
//! The tree's *expanded* nodes and the *checked* columns are both result-grid state
//! (so they're scoped to the open browse); the catalog (a referenced table's columns)
//! comes from the schema panel's eager `describe_table` prefetch, with a lazy fetch
//! on first expand when a large schema overflowed the prefetch cap.

use flint::prelude::*;
use gpui::{div, prelude::*, px, AnyElement, Context, SharedString};
use red_core::FkEdge;
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase};

/// Horizontal indent per tree level.
const INDENT: f32 = 14.0;
/// Recursion-depth cap — deep enough for any real normalized schema, shallow enough
/// that a self-referential / cyclic FK can't loop the renderer forever.
const MAX_DEPTH: usize = 8;

/// The `(ref_schema, ref_table)` a single-column FK on `table.col` points at, if the
/// graph has such an edge. The schema falls back to the source's when the engine
/// omits it (SQLite), keeping the catalog key within the same namespace.
fn fk_target(graph: &[FkEdge], schema: &str, table: &str, col: &str) -> Option<(String, String)> {
    graph
        .iter()
        .find(|e| {
            e.columns.len() == 1
                && e.from_table == table
                && e.from_schema.as_deref() == Some(schema)
                && e.columns[0].0 == col
        })
        .map(|e| {
            (
                e.to_schema.clone().unwrap_or_else(|| schema.to_string()),
                e.to_table.clone(),
            )
        })
}

impl AppState {
    /// Show or hide the Columns panel (status-bar toggle). No-op unless connected.
    pub(crate) fn toggle_columns_panel(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.columns_open = !active.columns_open;
            cx.notify();
        }
    }

    /// Expand / collapse a Columns-panel FK node, describing its referenced table on
    /// first open when the catalog prefetch hasn't reached it (a large schema past
    /// the prefetch cap). The detail arrives as `TableDescribed` and re-renders.
    pub(crate) fn toggle_columns_tree_node(
        &mut self,
        path: Vec<String>,
        ref_schema: String,
        ref_table: String,
        cx: &mut Context<Self>,
    ) {
        let need_describe = match &mut self.phase {
            Phase::Connected(active) => {
                let opened = active
                    .active_result_mut()
                    .map(|g| g.toggle_tree_node(path))
                    .unwrap_or(false);
                opened
                    && !active
                        .schema
                        .details
                        .contains_key(&(ref_schema.clone(), ref_table.clone()))
            }
            _ => false,
        };
        if need_describe {
            self.send_active(Command::DescribeTable {
                schema: ref_schema,
                table: ref_table,
            });
        }
        cx.notify();
    }

    /// The Columns panel: a header + the active browse's column tree (or an empty
    /// state). Shown in the left dock when [`ActiveConn::columns_open`].
    pub(crate) fn render_columns_panel(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let bg_panel = theme.bg_panel;
        let border = theme.border;
        let (text, muted, faint) = (theme.text, theme.text_muted, theme.text_faint);
        let bg_elevated = theme.bg_elevated;
        let ui_family = theme.font_family.clone();
        let (size_11, icon_x) = (theme.scale(11.), theme.scale(11.));

        let close_btn = div()
            .id("columns-hide")
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .size(px(18.))
            .rounded(px(3.))
            .cursor_pointer()
            .text_color(faint)
            .hover(|s| s.bg(bg_elevated).text_color(text))
            .tooltip(Tooltip::text("Hide columns"))
            .child(crate::icons::icon("x", icon_x, faint))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_columns_panel(cx)));

        let clear_btn = active
            .active_result()
            .is_some_and(|g| g.has_expansion())
            .then(|| {
                div()
                    .id("columns-clear")
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(18.))
                    .rounded(px(3.))
                    .cursor_pointer()
                    .text_color(faint)
                    .hover(|s| s.bg(bg_elevated).text_color(text))
                    .tooltip(Tooltip::text("Hide all reference columns"))
                    .child(crate::icons::icon("trash", icon_x, faint))
                    .on_click(cx.listener(|this, _, _, cx| this.clear_reference_columns(cx)))
            });

        let header = div()
            .flex_shrink_0()
            .h(px(28.))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .bg(bg_panel)
            .border_b_1()
            .border_color(border)
            .font_family(ui_family.clone())
            .text_size(size_11)
            .text_color(muted)
            .child(div().flex_1().min_w_0().truncate().child("Columns"))
            .children(clear_btn)
            .child(close_btn);

        // The tree rows, or an empty state when no table browse is in focus.
        let rows = self.columns_tree_rows(active, cx);
        let body = if rows.is_empty() {
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .px_4()
                .text_size(size_11)
                .text_color(faint)
                .child("Open a table to add reference columns")
                .into_any_element()
        } else {
            div()
                .id("columns-tree")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .py_1()
                .font_family(ui_family)
                .children(rows)
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(bg_panel)
            .child(header)
            .child(body)
    }

    /// Build the column-tree rows for the active single-table browse. Empty when the
    /// focused result isn't a table browse (the panel then shows its empty state).
    fn columns_tree_rows(&self, active: &ActiveConn, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let Some((schema, table)) = active.active_result().and_then(|g| g.base_table()).cloned()
        else {
            return out;
        };
        self.push_columns_level(active, &schema, &table, &[], 0, &mut out, cx);
        out
    }

    /// Append one table's columns to `out` and recurse into each *expanded* FK node.
    /// `prefix` is the dotted FK path from the base table to this level (empty at the
    /// base); `depth` is the indent / a recursion guard. Reads the table's columns
    /// from the prefetched catalog — an expanded-but-not-yet-described node shows a
    /// "loading…" line until its `TableDescribed` lands.
    #[allow(clippy::too_many_arguments)]
    fn push_columns_level(
        &self,
        active: &ActiveConn,
        schema: &str,
        table: &str,
        prefix: &[String],
        depth: usize,
        out: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let theme = cx.theme().clone();
        let (text, muted, faint, accent) =
            (theme.text, theme.text_muted, theme.text_faint, theme.accent);
        let (bg_hover, bg_panel) = (theme.bg_hover, theme.bg_panel);
        let (size_12, size_10) = (theme.scale(12.), theme.scale(10.));
        let icon_sm = theme.scale(12.);

        let Some(grid) = active.active_result() else {
            return;
        };
        let Some(detail) = active
            .schema
            .details
            .get(&(schema.to_string(), table.to_string()))
        else {
            // Expanded but not yet described (large-schema lazy fetch in flight).
            out.push(
                div()
                    .pl(px(depth as f32 * INDENT + 8.))
                    .py(px(2.))
                    .text_size(size_10)
                    .text_color(faint)
                    .child("loading…")
                    .into_any_element(),
            );
            return;
        };

        for col in &detail.columns {
            let mut col_path = prefix.to_vec();
            col_path.push(col.name.clone());
            let target = fk_target(&active.fk_graph, schema, table, &col.name);
            let is_fk = target.is_some();
            let expanded = is_fk && grid.is_tree_expanded(&col_path);
            // A reference column (depth >= 1) carries a checkbox to add it; base
            // columns are always shown, so they don't.
            let checkable = depth >= 1;
            let checked = checkable && grid.is_shown(&col_path);

            // Chevron (FK nodes only) — toggles the subtree open.
            let chevron = if is_fk {
                let (s, t) = target.clone().unwrap();
                let node = col_path.clone();
                div()
                    .id(SharedString::from(format!(
                        "col-chev-{}",
                        col_path.join(".")
                    )))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(14.))
                    .cursor_pointer()
                    .text_color(faint)
                    .hover(|x| x.text_color(text))
                    .child(crate::icons::icon(
                        if expanded { "chevron-down" } else { "chevron" },
                        icon_sm,
                        faint,
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_columns_tree_node(node.clone(), s.clone(), t.clone(), cx);
                        cx.stop_propagation();
                    }))
                    .into_any_element()
            } else {
                div().flex_shrink_0().size(px(14.)).into_any_element()
            };

            // Checkbox (reference columns only) — toggles the column into the grid.
            let checkbox = if checkable {
                let path = col_path.clone();
                let mut sq = div()
                    .id(SharedString::from(format!(
                        "col-chk-{}",
                        col_path.join(".")
                    )))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(13.))
                    .rounded(px(3.))
                    .border_1()
                    .border_color(if checked { accent } else { theme.border })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_reference_column(path.clone(), cx);
                        cx.stop_propagation();
                    }));
                if checked {
                    sq = sq.bg(accent).child(crate::icons::icon(
                        "check",
                        theme.scale(10.),
                        bg_panel,
                    ));
                }
                sq.into_any_element()
            } else {
                div().flex_shrink_0().size(px(13.)).into_any_element()
            };

            // The row's primary click: expand an FK, else toggle a reference leaf.
            let row_path = col_path.clone();
            let row_target = target.clone();
            let primary = cx.listener(move |this, _, _, cx| match (&row_target, checkable) {
                (Some((s, t)), _) => {
                    this.toggle_columns_tree_node(row_path.clone(), s.clone(), t.clone(), cx)
                }
                (None, true) => this.toggle_reference_column(row_path.clone(), cx),
                (None, false) => {}
            });

            let type_label = col
                .type_name
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(|t| t.to_lowercase());

            let row = div()
                .id(SharedString::from(format!(
                    "col-row-{}",
                    col_path.join(".")
                )))
                .flex()
                .items_center()
                .gap_1()
                .h(px(22.))
                .pl(px(depth as f32 * INDENT + 6.))
                .pr_2()
                .cursor_pointer()
                .hover(|s| s.bg(bg_hover))
                .child(chevron)
                .child(checkbox)
                .child(crate::icons::icon(
                    if is_fk { "link" } else { "col" },
                    icon_sm,
                    if is_fk { accent } else { faint },
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(size_12)
                        .text_color(if checked { text } else { muted })
                        .child(col.name.clone()),
                )
                .when_some(type_label, |d, t| {
                    d.child(
                        div()
                            .flex_shrink_0()
                            .text_size(size_10)
                            .text_color(faint)
                            .child(t),
                    )
                })
                .on_click(primary);
            out.push(row.into_any_element());

            // Recurse into an expanded FK node.
            if let (true, Some((ref_schema, ref_table))) = (expanded, target) {
                self.push_columns_level(
                    active,
                    &ref_schema,
                    &ref_table,
                    &col_path,
                    depth + 1,
                    out,
                    cx,
                );
            }
        }
    }
}
