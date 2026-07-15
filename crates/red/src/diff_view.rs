//! Data-compare (table diff) UI: the trigger's progress toast, the terminal-event
//! handlers, and the full-screen read-only **diff report** overlay (see
//! docs/plans/todo/data-diff.md). The backend (`red-service`) does the merge-walk
//! and streams back a bounded [`DiffFinished`](red_service::Event::DiffFinished)
//! payload; this renders it. Like the ER diagram, the report hangs off the
//! connection (schema-wide, not a result tab) so it survives tab churn.

use gpui::{AnyElement, Context, ScrollHandle, div, prelude::*, px};
use red_core::diff::{DiffColumnPlan, DiffKind, DiffRow, DiffSummary};
use red_core::{TableRef, Value};
use red_service::{Command, OpId, SessionId};

use flint::prelude::*;

use crate::app::{ActiveConn, AppState};

/// Which diff rows the report shows. Unchanged rows are never stored (only
/// counted), so the filter selects among the materialized added/removed/changed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffFilter {
    All,
    Added,
    Removed,
    Changed,
}

/// Max diff rows painted at once. The backend caps stored rows already; this caps
/// the *rendered* ones so a huge diff can't stall a frame (a note shows the rest).
const RENDER_CAP: usize = 500;

/// An open diff report: the column alignment, the summary totals, and the bounded
/// set of differing rows, plus the current row filter and scroll position.
pub(crate) struct DiffReport {
    pub(crate) left: String,
    pub(crate) right: String,
    pub(crate) plan: DiffColumnPlan,
    pub(crate) summary: DiffSummary,
    pub(crate) rows: Vec<DiffRow>,
    pub(crate) truncated: bool,
    pub(crate) filter: DiffFilter,
    pub(crate) scroll: ScrollHandle,
}

impl AppState {
    /// Fire `DiffTables` (same or cross connection) and raise the progress toast.
    /// `key` empty means the backend aligns on the left table's primary key.
    pub(crate) fn start_diff(
        &mut self,
        left_session: SessionId,
        left: TableRef,
        right_session: SessionId,
        right: TableRef,
        cx: &mut Context<Self>,
    ) {
        let id = OpId::new(self.next_export_id);
        self.next_export_id += 1;
        let left_name = left.name.clone();
        let right_name = right.name.clone();
        let label = format!("{left_name} vs {right_name}");
        self.service.send_to(
            left_session,
            Command::DiffTables {
                id,
                left,
                right_session,
                right,
                key: String::new(),
            },
        );
        self.diff_op = Some(id);
        self.diff_labels = Some((left_name, right_name));
        self.diff_notif = Some(self.notify(ToastVariant::Info, format!("Comparing {label}…"), cx));
    }

    /// `DiffProgress`: update the "Comparing…" toast with the scanned-row count.
    pub(crate) fn on_diff_progress(&mut self, id: OpId, scanned: usize, cx: &mut Context<Self>) {
        if self.diff_op != Some(id) {
            return;
        }
        if let Some(nid) = self.diff_notif
            && let Some(n) = self.notifications.iter_mut().find(|n| n.id == nid)
        {
            n.message = format!("Comparing… {scanned} row(s) scanned").into();
            cx.notify();
        }
    }

    /// `DiffFinished`: dismiss the progress toast and open the report overlay on the
    /// active connection.
    pub(crate) fn on_diff_finished(
        &mut self,
        id: OpId,
        plan: DiffColumnPlan,
        summary: DiffSummary,
        rows: Vec<DiffRow>,
        truncated: bool,
        cx: &mut Context<Self>,
    ) {
        if self.diff_op != Some(id) {
            return;
        }
        self.clear_diff_toast(cx);
        let (left, right) = self.diff_labels.take().unwrap_or_default();
        if let crate::app::Phase::Connected(active) = &mut self.phase {
            active.diff = Some(DiffReport {
                left,
                right,
                plan,
                summary,
                rows,
                truncated,
                filter: DiffFilter::All,
                scroll: ScrollHandle::new(),
            });
        }
        // Route Esc/focus through the shared modal handle (see `render_diff`).
        self.focus_modal = true;
        cx.notify();
    }

    /// `DiffFailed`: drop the progress toast and surface the error.
    pub(crate) fn on_diff_failed(&mut self, id: OpId, message: String, cx: &mut Context<Self>) {
        if self.diff_op != Some(id) {
            return;
        }
        self.clear_diff_toast(cx);
        self.notify(
            ToastVariant::Error,
            format!("Compare failed: {message}"),
            cx,
        );
    }

    /// `DiffCancelled`: drop the progress toast.
    pub(crate) fn on_diff_cancelled(&mut self, id: OpId, cx: &mut Context<Self>) {
        if self.diff_op != Some(id) {
            return;
        }
        self.clear_diff_toast(cx);
        self.notify(ToastVariant::Info, "Compare cancelled", cx);
    }

    fn clear_diff_toast(&mut self, cx: &mut Context<Self>) {
        if let Some(nid) = self.diff_notif.take() {
            self.dismiss(nid, cx);
        }
        self.diff_op = None;
    }

    /// Close the diff report overlay.
    pub(crate) fn close_diff(&mut self, cx: &mut Context<Self>) {
        if let crate::app::Phase::Connected(active) = &mut self.phase {
            active.diff = None;
        }
        cx.notify();
    }

    fn set_diff_filter(&mut self, filter: DiffFilter, cx: &mut Context<Self>) {
        if let crate::app::Phase::Connected(active) = &mut self.phase
            && let Some(diff) = active.diff.as_mut()
        {
            diff.filter = filter;
            cx.notify();
        }
    }

    /// The full-screen diff report overlay. Rendered from the root whenever the active
    /// connection has an open report (like the ER diagram).
    pub(crate) fn render_diff(&self, active: &ActiveConn, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(diff) = active.diff.as_ref() else {
            return div().into_any_element();
        };
        let mono = theme.mono_family.clone();
        let s = &diff.summary;

        // --- header: title + summary counts + close ---
        let title = format!("Compare — {} vs {}", diff.left, diff.right);
        let counts = format!(
            "{} added · {} removed · {} changed · {} unchanged · key: {}",
            s.added, s.removed, s.changed, s.unchanged, diff.plan.key,
        );
        let header = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .pl(px(crate::shell::TITLEBAR_LEFT_INSET))
            .pr_3()
            .py_2()
            .bg(theme.bg_panel)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme.text)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(theme.scale(11.))
                            .text_color(theme.text_muted)
                            .child(counts),
                    ),
            )
            .child(
                Button::new("diff-close", "Close")
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.close_diff(cx))),
            );

        // --- filter segmented control ---
        let filter = diff.filter;
        let view = cx.entity().downgrade();
        let filter_bar = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .bg(theme.bg_app)
            .border_b_1()
            .border_color(theme.border)
            .child(
                Segmented::new("diff-filter")
                    .segment("All")
                    .segment("Added")
                    .segment("Removed")
                    .segment("Changed")
                    .selected(match filter {
                        DiffFilter::All => 0,
                        DiffFilter::Added => 1,
                        DiffFilter::Removed => 2,
                        DiffFilter::Changed => 3,
                    })
                    .on_select(move |i, _w, cx| {
                        let f = match i {
                            1 => DiffFilter::Added,
                            2 => DiffFilter::Removed,
                            3 => DiffFilter::Changed,
                            _ => DiffFilter::All,
                        };
                        view.update(cx, |this, cx| this.set_diff_filter(f, cx)).ok();
                    }),
            );

        // One-side-only column warning, if any.
        let mut notes: Vec<String> = Vec::new();
        if !diff.plan.left_only.is_empty() {
            notes.push(format!(
                "left-only columns: {}",
                diff.plan.left_only.join(", ")
            ));
        }
        if !diff.plan.right_only.is_empty() {
            notes.push(format!(
                "right-only columns: {}",
                diff.plan.right_only.join(", ")
            ));
        }

        // --- column header row (compared columns) ---
        let col_header = {
            let mut row = div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .bg(theme.bg_panel)
                .border_b_1()
                .border_color(theme.border)
                .font_family(mono.clone())
                .text_size(theme.scale(10.5))
                .text_color(theme.text_muted);
            // Gutter spacer to align with the row markers below.
            row = row.child(div().w(px(16.)));
            for name in &diff.plan.columns {
                row = row.child(div().flex_1().min_w(px(60.)).child(name.clone()));
            }
            row
        };

        // --- rows (filtered, render-capped) ---
        let visible: Vec<&DiffRow> = diff
            .rows
            .iter()
            .filter(|r| match filter {
                DiffFilter::All => true,
                DiffFilter::Added => r.kind == DiffKind::Added,
                DiffFilter::Removed => r.kind == DiffKind::Removed,
                DiffFilter::Changed => r.kind == DiffKind::Changed,
            })
            .collect();
        let shown = visible.len().min(RENDER_CAP);
        let mut row_els: Vec<AnyElement> = Vec::with_capacity(shown);
        for r in visible.iter().take(RENDER_CAP) {
            row_els.push(diff_row_el(r, &theme, &mono));
        }
        if visible.len() > shown {
            row_els.push(
                div()
                    .px_3()
                    .py_2()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_muted)
                    .child(format!(
                        "…and {} more (showing the first {shown})",
                        visible.len() - shown
                    ))
                    .into_any_element(),
            );
        }
        if diff.truncated {
            row_els.push(
                div()
                    .px_3()
                    .py_2()
                    .text_size(theme.scale(11.))
                    .text_color(theme.yellow)
                    .child("The diff exceeded the stored-row cap; totals are exact but some differing rows aren't listed.")
                    .into_any_element(),
            );
        }
        if visible.is_empty() {
            row_els.push(
                div()
                    .px_3()
                    .py_4()
                    .text_color(theme.text_muted)
                    .child("No differing rows for this filter.")
                    .into_any_element(),
            );
        }

        let body = div()
            .id("diff-rows")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&diff.scroll)
            .flex()
            .flex_col()
            .children(row_els);

        let notes_bar = (!notes.is_empty()).then(|| {
            div()
                .flex_shrink_0()
                .px_3()
                .py_1()
                .bg(theme.bg_app)
                .border_b_1()
                .border_color(theme.border)
                .text_size(theme.scale(10.5))
                .text_color(theme.yellow)
                .child(notes.join(" · "))
        });

        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(theme.bg_app)
            .occlude()
            .track_focus(&self.modal_focus)
            .key_context("Modal")
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if ev.keystroke.key.as_str() == "escape" {
                    this.close_diff(cx);
                    cx.stop_propagation();
                }
            }))
            .child(header)
            .child(filter_bar)
            .children(notes_bar)
            .child(col_header)
            .child(body)
            .into_any_element()
    }
}

/// One diff row: a colored `+`/`-`/`~` gutter marker plus the compared cells, with
/// changed cells rendered as `old → new` in amber.
fn diff_row_el(r: &DiffRow, theme: &flint::Theme, mono: &gpui::SharedString) -> AnyElement {
    let (marker, color, tint) = match r.kind {
        DiffKind::Added => ("+", theme.green, theme.green.opacity(0.08)),
        DiffKind::Removed => ("-", theme.red, theme.red.opacity(0.08)),
        DiffKind::Changed => ("~", theme.yellow, theme.yellow.opacity(0.08)),
    };
    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .bg(tint)
        .border_b_1()
        .border_color(theme.border.opacity(0.5))
        .font_family(mono.clone())
        .text_size(theme.scale(11.))
        .text_color(theme.text)
        .child(
            div()
                .w(px(16.))
                .flex_shrink_0()
                .text_color(color)
                .child(marker),
        );

    // The values to show: the present side (right for added/changed, left for removed).
    let cells = if r.kind == DiffKind::Removed {
        &r.left
    } else {
        &r.right
    };
    for (i, name_cell) in cells.iter().enumerate() {
        let changed = r.changed.get(i).copied().unwrap_or(false);
        let text = if changed {
            // Show old → new for a changed cell.
            let old = r.left.get(i).map(cell_text).unwrap_or_default();
            let new = cell_text(name_cell);
            format!("{old} → {new}")
        } else {
            cell_text(name_cell)
        };
        let mut cell = div().flex_1().min_w(px(60.)).child(text);
        if changed {
            cell = cell.text_color(theme.yellow);
        }
        row = row.child(cell);
    }
    row.into_any_element()
}

/// A short textual rendering of a cell value for the diff grid (long text/blobs
/// are summarized so a row stays one line).
fn cell_text(v: &Value) -> String {
    const MAX: usize = 120;
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => r.to_string(),
        Value::Text(s) => {
            let one_line = s.replace('\n', " ");
            if one_line.chars().count() > MAX {
                let head: String = one_line.chars().take(MAX).collect();
                format!("{head}…")
            } else {
                one_line
            }
        }
        Value::Blob(b) => format!("<{} bytes>", b.len()),
        Value::Capped(_) => "<capped>".to_string(),
    }
}
