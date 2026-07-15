//! The query-plan view (Track B4, EXPLAIN). An Explain action runs the engine's
//! `EXPLAIN` for the editor's current query and shows the normalized plan tree in
//! the result pane, in place of the grid. The driver does the engine-specific
//! formatting (`DatabaseDriver::explain` → `red_core::QueryPlan`); this module is
//! purely the UI: the action, the per-tab `PlanView` state, the `PlanReady` /
//! `PlanFailed` handlers, and the rendering.
//!
//! A plan is tiny and bounded, so it's held whole: the "never materialize" budget
//! rule is about row data, not a fixed-size plan. Closing the plan frees it.

use flint::prelude::*;
use flint::{Button, ButtonSize, ButtonVariant, ToastVariant};
use gpui::{AnyElement, ClipboardItem, Context, ScrollHandle, SharedString, div, prelude::*, px};
use red_core::{PlanNode, QueryPlan};
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase};

/// One open query plan, owned by its [`crate::app::QueryTab`]. Replaces the grid
/// in the result pane while open; a fresh query run clears it.
pub(crate) struct PlanView {
    /// Identifies this explain request; a `PlanReady`/`PlanFailed` for a different
    /// epoch (the tab re-explained or switched) is dropped.
    pub epoch: red_service::Epoch,
    /// The SQL that was explained; shown truncated in the header.
    pub sql: String,
    /// Whether this was an `EXPLAIN ANALYZE` (actuals + hot-node tint).
    pub analyze: bool,
    pub state: PlanState,
    /// Scroll position of the plan body, retained across renders.
    pub scroll: ScrollHandle,
}

/// Where a [`PlanView`] is in its lifecycle.
pub(crate) enum PlanState {
    /// The `Explain` command is in flight.
    Loading,
    /// The plan arrived.
    Ready(QueryPlan),
    /// `EXPLAIN` failed (bad SQL, unsupported statement); shown in the pane.
    Failed(String),
}

impl AppState {
    /// Run `EXPLAIN` (or `EXPLAIN ANALYZE` when `analyze`) for the active tab's
    /// current query (its selection if any, else the whole buffer) and open a
    /// plan view in the result pane. `analyze` *executes* the statement, so it's
    /// gated to read queries; plain explain never executes and is unconditional.
    pub(crate) fn explain_query(&mut self, analyze: bool, cx: &mut Context<Self>) {
        let sql = match &self.phase {
            Phase::Connected(active) => match active.active() {
                Some(tab) => {
                    let editor = tab.editor.read(cx);
                    editor.selected_text().unwrap_or_else(|| editor.content())
                }
                None => return,
            },
            _ => return,
        };
        let sql = sql.trim().to_string();
        if sql.is_empty() {
            return;
        }

        // EXPLAIN ANALYZE runs the statement, so refuse it for anything but a read
        // query (a confirmed `EXPLAIN ANALYZE DELETE …` would still delete).
        if analyze && !matches!(crate::sql::classify(&sql), crate::sql::StatementKind::Query) {
            self.notify(
                ToastVariant::Error,
                "Explain Analyze runs the statement, so it's only available for read queries.",
                cx,
            );
            return;
        }

        let epoch = crate::result::new_epoch();
        if let Phase::Connected(active) = &mut self.phase
            && let Some(tab) = active.active_mut()
        {
            tab.plan = Some(PlanView {
                epoch,
                sql: sql.clone(),
                analyze,
                state: PlanState::Loading,
                scroll: ScrollHandle::new(),
            });
        }
        self.send_active(Command::Explain {
            sql,
            analyze,
            epoch,
        });
        cx.notify();
    }

    /// A plan arrived: drop it into the matching tab's plan view (by session,
    /// then epoch), unless it's been superseded.
    pub(crate) fn on_plan_ready(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: red_service::Epoch,
        plan: QueryPlan,
    ) {
        if let Some(active) = self.conn_mut(session)
            && let Some(view) = active.plan_by_epoch(epoch)
        {
            view.state = PlanState::Ready(plan);
        }
    }

    /// An explain failed: show the message in the plan pane (not a global toast).
    pub(crate) fn on_plan_failed(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: red_service::Epoch,
        message: String,
    ) {
        if let Some(active) = self.conn_mut(session)
            && let Some(view) = active.plan_by_epoch(epoch)
        {
            view.state = PlanState::Failed(message);
        }
    }

    /// Close the active tab's plan view, returning to the grid (if any).
    pub(crate) fn close_plan(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase
            && let Some(tab) = active.active_mut()
        {
            tab.plan = None;
        }
        cx.notify();
    }

    /// Render the plan pane: a header (title · explained SQL · actions) over the
    /// plan tree, a loading line, or the error.
    pub(crate) fn render_plan(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (bg, border, text, muted, faint, dim, red) = (
            theme.bg_panel,
            theme.border,
            theme.text,
            theme.text_muted,
            theme.text_faint,
            theme.text_dim,
            theme.red,
        );
        let ui_family = theme.font_family.clone();
        let mono_family = theme.mono_family.clone();
        let (s11, s12) = (theme.scale(11.), theme.scale(12.));
        let body_size = theme.font_size;

        let Some(tab) = active.tabs.get(tab_idx) else {
            return div().into_any_element();
        };
        let Some(view) = tab.plan.as_ref() else {
            return div().into_any_element();
        };
        let has_result = tab.result.is_some();

        // --- header: title, explained SQL, actions ---
        let analyzed = view.analyze;
        let title = if analyzed {
            "Query plan · analyzed"
        } else {
            "Query plan"
        };
        let sql_line = one_line(&view.sql, 96);
        let raw_for_copy: Option<SharedString> = match &view.state {
            PlanState::Ready(plan) => Some(plan.raw.clone().into()),
            _ => None,
        };

        let mut header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py(px(6.))
            .border_b_1()
            .border_color(border)
            .font_family(ui_family.clone())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(div().text_size(s12).text_color(text).child(title))
                    .child(
                        div()
                            .text_size(s11)
                            .text_color(faint)
                            .font_family(mono_family.clone())
                            .child(sql_line),
                    ),
            );
        if let Some(raw) = raw_for_copy {
            header = header.child(
                Button::new("plan-copy", "Copy plan")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(raw.to_string()));
                    })),
            );
        }
        if has_result {
            header = header.child(
                Button::new("plan-back", "← Results")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.close_plan(cx))),
            );
        }
        header = header.child(
            Button::new("plan-close", "✕")
                .variant(ButtonVariant::Ghost)
                .size(ButtonSize::Sm)
                .on_click(cx.listener(|this, _, _, cx| this.close_plan(cx))),
        );

        // --- body by state ---
        let body: AnyElement = match &view.state {
            PlanState::Loading => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(s12)
                .text_color(faint)
                .child("Explaining…")
                .into_any_element(),
            PlanState::Failed(message) => div()
                .id("plan-error")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .p_4()
                .flex()
                .flex_col()
                .gap_2()
                .font_family(mono_family.clone())
                .child(div().text_size(s11).text_color(red).child("EXPLAIN failed"))
                .child(div().text_size(s12).text_color(text).child(message.clone()))
                .into_any_element(),
            PlanState::Ready(plan) if plan.nodes.is_empty() => div()
                .id("plan-raw")
                .flex_1()
                .min_h(px(0.))
                .overflow_scroll()
                .track_scroll(&view.scroll)
                .p_3()
                .font_family(mono_family.clone())
                .text_size(body_size)
                .text_color(text)
                .child(SharedString::from(plan.raw.clone()))
                .into_any_element(),
            PlanState::Ready(plan) => {
                let colors = PlanColors {
                    text,
                    muted,
                    dim,
                    faint,
                    red,
                };
                // Under ANALYZE, the largest actual-time upper bound marks the
                // hottest node for a heat tint.
                let hot = analyzed.then(|| max_actual_time(&plan.nodes)).flatten();
                let mut rows: Vec<AnyElement> = Vec::new();
                for node in &plan.nodes {
                    render_node(node, 0, hot, &colors, &mono_family, body_size, &mut rows);
                }
                div()
                    .id("plan-tree")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_scroll()
                    .track_scroll(&view.scroll)
                    .py_1()
                    .children(rows)
                    .into_any_element()
            }
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(bg)
            .child(header)
            .child(body)
            .into_any_element()
    }
}

/// Colour tokens snapshotted for the `'static`-ish row builders.
#[derive(Clone, Copy)]
struct PlanColors {
    text: gpui::Hsla,
    muted: gpui::Hsla,
    dim: gpui::Hsla,
    faint: gpui::Hsla,
    red: gpui::Hsla,
}

/// Append `node` (and its subtree) as indented rows. Each row: an indent guide,
/// the operation label, its metric pills, and any detail line beneath.
fn render_node(
    node: &PlanNode,
    depth: usize,
    hot: Option<f64>,
    colors: &PlanColors,
    mono: &SharedString,
    size: gpui::Pixels,
    out: &mut Vec<AnyElement>,
) {
    let is_hot = hot
        .zip(node_actual_time(node))
        .is_some_and(|(max, t)| max > 0. && (t - max).abs() < f64::EPSILON);

    let pills = node.metrics.iter().map(|(k, v)| {
        let label = if k.is_empty() {
            v.clone()
        } else {
            format!("{k} {v}")
        };
        div()
            .px(px(5.))
            .py(px(1.))
            .rounded(px(4.))
            .bg(colors.dim.opacity(0.10))
            .text_color(colors.muted)
            .child(label)
    });

    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .pl(px(8. + depth as f32 * 16.))
        .pr(px(8.))
        .py(px(2.))
        .font_family(mono.clone())
        .text_size(size)
        .when(is_hot, |d| d.bg(colors.red.opacity(0.12)))
        .child(
            div()
                .flex_shrink_0()
                .text_color(if is_hot { colors.red } else { colors.text })
                .child(node.label.clone()),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_1()
                .text_size(size * 0.92)
                .children(pills),
        );
    // Disclosure-ish caret for a node with children, so the nesting reads.
    if !node.children.is_empty() {
        row = row.child(
            div()
                .text_color(colors.faint)
                .child(format!("· {} sub", node.children.len())),
        );
    }
    out.push(row.into_any_element());

    if let Some(detail) = &node.detail {
        out.push(
            div()
                .pl(px(8. + depth as f32 * 16. + 14.))
                .pr(px(8.))
                .pb(px(2.))
                .font_family(mono.clone())
                .text_size(size * 0.92)
                .text_color(colors.faint)
                .child(detail.clone())
                .into_any_element(),
        );
    }

    for child in &node.children {
        render_node(child, depth + 1, hot, colors, mono, size, out);
    }
}

/// The largest `actual time` upper bound across the tree (the bottleneck), or
/// `None` when no node carries one.
fn max_actual_time(nodes: &[PlanNode]) -> Option<f64> {
    let mut max: Option<f64> = None;
    fn walk(node: &PlanNode, max: &mut Option<f64>) {
        if let Some(t) = node_actual_time(node)
            && max.is_none_or(|m| t > m)
        {
            *max = Some(t);
        }
        for c in &node.children {
            walk(c, max);
        }
    }
    for n in nodes {
        walk(n, &mut max);
    }
    max
}

/// A node's `actual time` upper bound (`"0.011..0.013"` → `0.013`), if present.
fn node_actual_time(node: &PlanNode) -> Option<f64> {
    let raw = node
        .metrics
        .iter()
        .find(|(k, _)| k == "actual time")
        .map(|(_, v)| v.as_str())?;
    let upper = raw.rsplit("..").next().unwrap_or(raw);
    upper.trim().parse::<f64>().ok()
}

/// Collapse `sql` to a single trimmed line, truncated to `max` chars with an
/// ellipsis, for the plan header's "what was explained" subtitle.
fn one_line(sql: &str, max: usize) -> String {
    let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let head: String = flat.chars().take(max).collect();
        format!("{head}…")
    } else {
        flat
    }
}
