//! The connection health report view (see
//! `docs/plans/todo/connection-health-report.md`).
//!
//! Two of RED's three driver seams could already answer "what is wrong in here":
//! Redis has the keyspace analysis panel, Mongo has `audit_collection`. The seam
//! most people actually connect to could not. This closes that.
//!
//! The report hangs off the **connection**, not a query, so it is a whole-half tab
//! body ([`TabView::Health`]) like the ER diagram rather than something in the
//! result pane. It is persisted per connection by [`crate::health_store`], so
//! re-opening it after a restart shows the last run with an honest "as of" line
//! instead of a blank panel.
//!
//! **Nothing here runs anything.** A finding's `suggested_sql` gets a Copy button
//! and an "Open as query" that pastes into an ordinary tab. `CREATE INDEX` on a
//! large production table is a locking event; the decision belongs to the
//! operator, taken through the editor where every existing guard applies.

use flint::prelude::*;
use flint::{Button, ButtonSize, ButtonVariant, ToastVariant};
use gpui::{Context, ScrollHandle, SharedString, div, prelude::*, px};
use red_core::health::{Finding, HealthReport, Severity};
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase, TabView};

/// One open health report tab.
pub(crate) struct HealthView {
    pub state: HealthState,
    pub scroll: ScrollHandle,
}

pub(crate) enum HealthState {
    /// A build is in flight. Carries the previously-saved report, if there is
    /// one, so a refresh shows the old numbers rather than an empty pane.
    Loading(Option<HealthReport>),
    Ready(HealthReport),
    Failed(String),
}

/// How old a saved report may be before the header marks it stale (24h).
const STALE_SECS: i64 = 24 * 60 * 60;

impl AppState {
    /// Open (or re-focus) the health report for the active connection, showing the
    /// saved one immediately and refreshing behind it.
    pub(crate) fn open_health_report(&mut self, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        if !active.config.kind.session_caps().supported
            && !matches!(active.config.kind, red_core::DbKind::Sqlite)
        {
            return;
        }
        if let Some(i) = active.tabs.iter().position(|t| t.health().is_some()) {
            self.set_active_tab(i, cx);
            self.refresh_health_report(cx);
            return;
        }
        let saved = self.health_store.get(&active.conn_id).cloned();
        let mut tab = crate::app::QueryTab::new("Health".to_string(), self.active_dialect(), cx);
        tab.view = Some(TabView::Health(HealthView {
            state: HealthState::Loading(saved),
            scroll: ScrollHandle::new(),
        }));
        self.push_tab(tab, cx);
        self.send_active(Command::BuildHealthReport);
        cx.notify();
    }

    pub(crate) fn refresh_health_report(&mut self, cx: &mut Context<Self>) {
        let previous = match &self.phase {
            Phase::Connected(a) => a.tabs.iter().find_map(|t| match t.health() {
                Some(v) => match &v.state {
                    HealthState::Ready(r) => Some(Some(r.clone())),
                    _ => Some(None),
                },
                None => None,
            }),
            _ => None,
        };
        let Some(previous) = previous else { return };
        if let Phase::Connected(active) = &mut self.phase
            && let Some(view) = active.tabs.iter_mut().find_map(|t| t.health_mut())
        {
            view.state = HealthState::Loading(previous);
        }
        self.send_active(Command::BuildHealthReport);
        cx.notify();
    }

    /// A report arrived: show it and persist it for this connection.
    pub(crate) fn on_health_report(
        &mut self,
        session: Option<red_service::SessionId>,
        report: HealthReport,
        cx: &mut Context<Self>,
    ) {
        let conn_id = self.conn_mut(session).map(|a| a.conn_id.clone());
        if let Some(conn_id) = conn_id {
            self.health_store.set(&conn_id, report.clone());
        }
        if let Some(active) = self.conn_mut(session)
            && let Some(view) = active.tabs.iter_mut().find_map(|t| t.health_mut())
        {
            view.state = HealthState::Ready(report);
        }
        cx.notify();
    }

    pub(crate) fn on_health_report_failed(
        &mut self,
        session: Option<red_service::SessionId>,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.conn_mut(session)
            && let Some(view) = active.tabs.iter_mut().find_map(|t| t.health_mut())
        {
            view.state = HealthState::Failed(message);
        }
        cx.notify();
    }

    /// The health report tab body.
    pub(crate) fn render_health(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let Some(view) = active.tabs.get(tab_idx).and_then(|t| t.health()) else {
            return div().into_any_element();
        };

        // A loading refresh keeps the previous numbers on screen: a report that
        // blanks itself every time it is refreshed is unreadable.
        let (report, loading) = match &view.state {
            HealthState::Ready(r) => (Some(r), false),
            HealthState::Loading(prev) => (prev.as_ref(), true),
            HealthState::Failed(_) => (None, false),
        };

        let header = div()
            .flex_shrink_0()
            .h(px(34.))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(SharedString::from(format!(
                        "Health · {}",
                        active.config.name
                    ))),
            )
            .children(report.map(|r| {
                let age = crate::health::now_unix() - r.generated_at;
                let stale = age > STALE_SECS;
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(if stale {
                        theme.yellow
                    } else {
                        theme.text_faint
                    })
                    .child(format!("as of {}", fmt_age(age)))
            }))
            .when(loading, |d| {
                d.child(
                    div()
                        .text_size(theme.scale(10.5))
                        .text_color(theme.text_faint)
                        .child(crate::i18n::tr!("health.refreshing", "refreshing…")),
                )
            })
            .child(
                div().ml_auto().child(
                    Button::new("health-refresh", "Refresh")
                        .variant(ButtonVariant::Ghost)
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_health_report(cx))),
                ),
            );

        let body = match (&view.state, report) {
            (HealthState::Failed(message), _) => div()
                .p_3()
                .text_size(theme.scale(11.5))
                .text_color(theme.red)
                .child(message.clone())
                .into_any_element(),
            (_, None) => div()
                .p_3()
                .text_size(theme.scale(11.5))
                .text_color(theme.text_faint)
                .child(crate::i18n::tr!("health.building", "building the report…"))
                .into_any_element(),
            (_, Some(r)) => self.render_health_body(r, view, cx),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_panel)
            .child(header)
            .child(div().flex_1().min_h(px(0.)).child(body))
            .into_any_element()
    }

    fn render_health_body(
        &self,
        r: &HealthReport,
        view: &HealthView,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let size_11 = theme.scale(11.);

        let totals = div()
            .flex()
            .items_center()
            .gap_4()
            .px_3()
            .py_2()
            .text_size(size_11)
            .text_color(theme.text_muted)
            .child(format!("{} total", human_bytes(r.totals.bytes)))
            .child(format!("{} tables", r.totals.table_count))
            .when(r.totals.index_bytes > 0, |d| {
                d.child(format!("{} in indexes", human_bytes(r.totals.index_bytes)))
            });

        let findings: Vec<gpui::AnyElement> = r
            .sorted_findings()
            .into_iter()
            .map(|f| self.render_finding(f, cx))
            .collect();

        let findings_section = if findings.is_empty() {
            div()
                .px_3()
                .py_2()
                .text_size(size_11)
                .text_color(theme.green)
                .child(crate::i18n::tr!("health.no_findings", "No findings."))
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .children(findings)
                .into_any_element()
        };

        // What could not be checked, stated plainly. Without this an empty
        // findings list on a MariaDB (no `sys` schema) reads as a clean bill of
        // health when half the checks never ran.
        let unavailable = (!r.unavailable.is_empty()).then(|| {
            let items: Vec<gpui::AnyElement> = r
                .unavailable
                .iter()
                .map(|u| {
                    div()
                        .px_3()
                        .py_1()
                        .text_size(theme.scale(10.5))
                        .text_color(theme.text_faint)
                        .child(format!("{:?}: {}", u.kind, u.reason))
                        .into_any_element()
                })
                .collect();
            div()
                .flex()
                .flex_col()
                .child(section_label("Not checked here", &theme))
                .children(items)
        });

        let biggest: Vec<gpui::AnyElement> = r
            .tables
            .iter()
            .take(25)
            .map(|t| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .text_size(size_11)
                    .child(
                        div()
                            .font_family(theme.mono_family.clone())
                            .text_color(theme.text)
                            .child(t.table.name.clone()),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .text_color(theme.text_muted)
                            .child(if t.bytes > 0 {
                                human_bytes(t.bytes)
                            } else {
                                format!("~{} rows", t.estimated_rows)
                            }),
                    )
                    .into_any_element()
            })
            .collect();

        div()
            .id("health-body")
            .size_full()
            .overflow_scroll()
            .track_scroll(&view.scroll)
            .child(totals)
            .child(section_label("Findings", &theme))
            .child(findings_section)
            .children(unavailable)
            .child(section_label("Largest objects", &theme))
            .children(biggest)
            .into_any_element()
    }

    fn render_finding(&self, f: &Finding, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let dot = match f.severity {
            Severity::Bad => theme.red,
            Severity::Warn => theme.yellow,
            Severity::Info => theme.text_faint,
        };
        let sql = f.suggested_sql.clone();

        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border_soft)
            .text_size(theme.scale(11.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(div().size(px(6.)).rounded_full().bg(dot))
                    .child(div().text_color(theme.text).child(f.title.clone()))
                    .when_some(sql.clone(), |d, sql| {
                        let copy = sql.clone();
                        d.child(
                            div()
                                .ml_auto()
                                .flex()
                                .gap_1()
                                .child(
                                    Button::new(
                                        SharedString::from(format!("health-copy-{}", f.title)),
                                        "Copy SQL",
                                    )
                                    .variant(ButtonVariant::Ghost)
                                    .size(ButtonSize::Sm)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.copy_to_clipboard(copy.clone(), "SQL copied", cx);
                                        },
                                    )),
                                )
                                .child(
                                    Button::new(
                                        SharedString::from(format!("health-open-{}", f.title)),
                                        "Open as query",
                                    )
                                    .variant(ButtonVariant::Ghost)
                                    .size(ButtonSize::Sm)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.health_open_as_query(sql.clone(), cx);
                                        },
                                    )),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(f.detail.clone()),
            )
            .into_any_element()
    }

    /// Paste a suggestion into a fresh query tab. Never runs it: an index build or
    /// a VACUUM is the operator's call, taken where the guards are.
    fn health_open_as_query(&mut self, sql: String, cx: &mut Context<Self>) {
        self.new_query(cx);
        let editor = match &self.phase {
            Phase::Connected(active) => match active.active() {
                Some(tab) => tab.editor.clone(),
                None => return,
            },
            _ => return,
        };
        editor.update(cx, |editor, cx| editor.set_content(sql, cx));
        self.notify(
            ToastVariant::Info,
            "Suggestion opened in a query tab. Nothing has run.",
            cx,
        );
        cx.notify();
    }
}

fn section_label(text: &'static str, theme: &flint::Theme) -> gpui::AnyElement {
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_size(theme.scale(10.))
        .text_color(theme.text_faint)
        .child(text)
        .into_any_element()
}

/// Wall-clock Unix seconds; the report's own timestamps come from the driver.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

use crate::fmt::{fmt_ago_secs as fmt_age, human_bytes};
