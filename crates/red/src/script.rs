//! Running a multi-statement script, and the per-statement log it produces.
//!
//! ⌘↵ runs *one* statement: a row-returning batch cannot open as a single result
//! (the paging path wraps the SQL in one subquery), so the caret's statement is
//! what the user means. A migration file, a seed script, or a handful of
//! statements pasted from a ticket wants the other contract, and this is it:
//! every statement in order, each reported by name, and the trailing `SELECT`
//! opened in the grid.
//!
//! Not a transaction. `Command::Execute` already wraps a write batch in one
//! (all-or-nothing, one total reported); a script is deliberately the opposite,
//! because the thing you want from a failed migration is to know which statement
//! failed and what the ones before it did. [`ScriptStop`] chooses whether the
//! rest still run.
//!
//! The trailing read is *not* executed by the backend: it is handed back and
//! opened through the ordinary result path, so it runs exactly once and arrives
//! in the grid with paging, sort and FK affordances intact.

use flint::prelude::*;
use flint::{Button, ButtonSize, ButtonVariant};
use gpui::{AnyElement, Context, ScrollHandle, div, prelude::*, px};
use red_core::{ScriptOutcome, ScriptStep, ScriptStop};
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase, TabWorkspace};

/// One script run, owned by its [`crate::app::QueryTab`]. Occupies the result
/// pane in place of the grid while open, like a query plan.
pub(crate) struct ScriptRun {
    /// How many statements the script has in total, so the log can show
    /// "3 of 12" while it fills in.
    pub total: usize,
    /// Per-statement outcomes, appended as each `ScriptStep` lands.
    pub steps: Vec<ScriptStep>,
    /// `None` while running; `Some` once `ScriptDone` lands.
    pub summary: Option<ScriptSummary>,
    pub scroll: ScrollHandle,
}

/// A finished script's totals.
pub(crate) struct ScriptSummary {
    pub ran: usize,
    pub failed: usize,
}

impl ScriptRun {
    fn new(total: usize) -> Self {
        Self {
            total,
            steps: Vec::with_capacity(total),
            summary: None,
            scroll: ScrollHandle::new(),
        }
    }
}

impl AppState {
    /// Run the active tab's whole buffer (or its selection) as a script.
    ///
    /// Falls through to the ordinary single-statement run when there is only one
    /// statement, so the action is never the wrong thing to press: on a buffer
    /// holding one `SELECT` it behaves exactly like ⌘↵.
    pub(crate) fn run_editor_script(&mut self, cx: &mut Context<Self>) {
        let dialect = self.active_dialect();
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        // Redis and MongoDB have no SQL editor; their `query 1` tab is a phantom.
        if active.kv_view.is_some() || active.doc_view.is_some() {
            return;
        }
        let Some(tab) = active.active().filter(|t| !t.is_view()) else {
            return;
        };
        let editor = tab.editor.read(cx);
        let source = editor.selected_text().unwrap_or_else(|| editor.content());
        let source = crate::sql::normalize_spaces(&source).unwrap_or(source);

        let statements: Vec<String> = red_core::sql::split_statements(&source, dialect)
            .into_iter()
            .filter(|s| !crate::sql::is_blank(s))
            .map(str::to_string)
            .collect();
        match statements.len() {
            0 => return,
            // One statement is not a script; the ordinary run path already knows
            // how to open it as a result, confirm it, or refuse it read-only.
            1 => {
                self.run_editor_query(cx);
                return;
            }
            _ => {}
        }

        let read_only = matches!(&self.phase, Phase::Connected(a) if a.config.read_only);
        // Grade the whole script, not each statement: the confirm has to describe
        // what pressing Run will do in total, and `assess` already takes the max
        // across a batch and flags which member carries the danger.
        let assessment = red_core::sql::assess(&source, dialect);
        if read_only && assessment.level > red_core::sql::RiskLevel::Safe {
            self.notify(
                flint::ToastVariant::Error,
                crate::i18n::tr!(
                    "editor.read_only_blocked",
                    "Connection is read-only; write statements are disabled."
                ),
                cx,
            );
            return;
        }
        if self.confirm_policy().requires(assessment.level) {
            self.open_script_confirm(source, statements, assessment, cx);
            return;
        }
        self.send_script(statements, cx);
    }

    /// Dispatch a graded script to the backend and open its log in the result
    /// pane. The single place a `RunScript` is sent, so the confirm path and the
    /// direct path cannot drift.
    pub(crate) fn send_script(&mut self, statements: Vec<String>, cx: &mut Context<Self>) {
        let Some(session) = self.foreground_session else {
            return;
        };
        let Some(conn) = self.conn_for(Some(session)) else {
            return;
        };
        let namespace = conn.namespace_for_send();
        let conn_id = conn.conn_id.clone();
        // The script goes into history as one entry: it is one thing the user
        // ran, and logging twelve statements separately would bury the rest of
        // the log under a single migration.
        let joined = statements.join(";\n");
        self.query_history
            .update(cx, |store, _| store.record(&conn_id, &joined));

        let total = statements.len();
        if let Phase::Connected(active) = &mut self.phase
            && let Some(index) = active.focused_tab_index()
            && let Some(tab) = active.tabs.get_mut(index)
        {
            // The log takes the result pane, so the grid and any open plan step
            // aside for it, exactly as a plan displaces the grid.
            tab.plan = None;
            tab.script = Some(ScriptRun::new(total));
        }
        if let Some(conn) = self.conn_mut(Some(session)) {
            conn.write_in_flight = true;
        }
        self.service.send_to(
            session,
            Command::RunScript {
                statements,
                namespace,
                stop: ScriptStop::OnError,
            },
        );
        cx.notify();
    }

    /// A `ScriptStep` landed: append it to the open log.
    pub(crate) fn on_script_step(&mut self, step: ScriptStep, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase
            && let Some(index) = active.focused_tab_index()
            && let Some(run) = active.tabs.get_mut(index).and_then(|t| t.script.as_mut())
        {
            run.steps.push(step);
            cx.notify();
        }
    }

    /// A `ScriptDone` landed: close out the log and open the trailing read, if
    /// the script left one to open.
    pub(crate) fn on_script_done(
        &mut self,
        ran: usize,
        failed: usize,
        trailing_read: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.foreground_session
            && let Some(conn) = self.conn_mut(Some(session))
        {
            conn.write_in_flight = false;
        }
        if let Phase::Connected(active) = &mut self.phase
            && let Some(index) = active.focused_tab_index()
            && let Some(run) = active.tabs.get_mut(index).and_then(|t| t.script.as_mut())
        {
            run.summary = Some(ScriptSummary { ran, failed });
        }
        if failed > 0 {
            self.notify(
                flint::ToastVariant::Error,
                format!(
                    "Script stopped: {failed} of {} statements failed",
                    ran + failed
                ),
                cx,
            );
        } else {
            self.notify(
                flint::ToastVariant::Success,
                format!("Script ran {ran} statements"),
                cx,
            );
        }
        // Opening the trailing read replaces the log with its grid: the rows are
        // what the user was reading the script for, and the log's own summary
        // has already been toasted.
        if let Some(sql) = trailing_read {
            let table = self.resolve_browse_table(&sql, cx);
            let sql = crate::sql::auto_limit(&sql, self.settings.sql.auto_limit).unwrap_or(sql);
            if let Phase::Connected(active) = &mut self.phase
                && let Some(index) = active.focused_tab_index()
                && let Some(tab) = active.tabs.get_mut(index)
            {
                tab.script = None;
            }
            self.open_result("script", sql, table, cx);
        }
        cx.notify();
    }

    /// Close the open script log, returning the pane to the grid.
    pub(crate) fn close_script(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase
            && let Some(index) = active.focused_tab_index()
            && let Some(tab) = active.tabs.get_mut(index)
        {
            tab.script = None;
        }
        cx.notify();
    }

    /// The script log, in the result pane's place.
    pub(crate) fn render_script(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(run) = active.tabs.get(tab_idx).and_then(|t| t.script.as_ref()) else {
            return div().into_any_element();
        };
        let done = run.summary.is_some();
        let heading = match &run.summary {
            Some(s) if s.failed > 0 => format!("{} ran, {} failed", s.ran, s.failed),
            Some(s) => format!("{} statements ran", s.ran),
            None => format!("running… {} of {}", run.steps.len(), run.total),
        };
        let rows: Vec<AnyElement> = run
            .steps
            .iter()
            .map(|step| render_step(step, &theme))
            .collect();

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .h(px(30.))
                    .flex_none()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().text_color(theme.text_muted).child(heading))
                    .when(done, |this| {
                        this.child(
                            Button::new("script-close", "Close")
                                .size(ButtonSize::Sm)
                                .variant(ButtonVariant::Ghost)
                                .on_click(cx.listener(|this, _, _, cx| this.close_script(cx))),
                        )
                    }),
            )
            .child(
                div()
                    .id("script-log")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_scroll()
                    .track_scroll(&run.scroll)
                    .children(rows),
            )
            .into_any_element()
    }
}

/// One log line: the statement's position, its outcome, and its text.
fn render_step(step: &ScriptStep, theme: &flint::Theme) -> AnyElement {
    let (marker, colour) = match &step.outcome {
        ScriptOutcome::Ok { .. } => ("ok", theme.green),
        ScriptOutcome::Rows => ("rows", theme.accent),
        ScriptOutcome::Failed { .. } => ("failed", theme.red),
        ScriptOutcome::Skipped => ("skipped", theme.text_faint),
    };
    let detail = match &step.outcome {
        // Only a non-zero count says anything; "0 rows" on a `CREATE TABLE` is
        // noise dressed as information.
        ScriptOutcome::Ok { affected } if *affected > 0 => Some(format!("{affected} rows")),
        ScriptOutcome::Failed { error } => Some(error.clone()),
        _ => None,
    };
    div()
        .flex()
        .flex_col()
        .gap_0p5()
        .px_3()
        .py_1p5()
        .border_b_1()
        .border_color(theme.border_soft)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(28.))
                        .flex_none()
                        .text_color(theme.text_faint)
                        .child(format!("{}", step.index + 1)),
                )
                .child(
                    div()
                        .w(px(56.))
                        .flex_none()
                        .text_color(colour)
                        .child(marker),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(theme.mono_family.clone())
                        .child(step.summary.clone()),
                ),
        )
        .when_some(detail, |this, detail| {
            this.child(
                div()
                    .pl(px(88.))
                    .text_color(if step.outcome.is_failure() {
                        theme.red
                    } else {
                        theme.text_muted
                    })
                    .child(detail),
            )
        })
        .into_any_element()
}
