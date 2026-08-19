//! The Mutations panel: what the engine is still doing after a submit returned.
//!
//! A best-effort edit doesn't end when Submit does. ClickHouse applies an
//! `ALTER TABLE … UPDATE` by rewriting every part the predicate can touch, and on a
//! production-sized table that runs long after the dialog closed. Without somewhere
//! to watch it, the only feedback is a grid that hasn't changed yet -- which reads as
//! "it didn't work" and invites a retry, and a retried mutation is a second full part
//! rewrite.
//!
//! So: a left-dock panel listing `system.mutations` (unfinished first) with each
//! one's command, parts remaining, and failure reason, and a per-row cancel. Offered
//! only where [`DbKind::tracks_mutations`](red_core::DbKind::tracks_mutations) says
//! there is asynchronous work to track.

use flint::prelude::*;
use gpui::{App, Context, SharedString, div, prelude::*, px};
use red_core::{MutationInfo, TableRef};
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase};

/// How much of a mutation's statement the row shows before clipping. Enough to tell
/// two edits of the same table apart; the full text is a `KILL`-worthy detail, not a
/// panel one.
const COMMAND_CHARS: usize = 160;

impl AppState {
    /// Whether the active connection has background mutations to track at all.
    /// Gates the panel and its status-bar toggle.
    pub(crate) fn tracks_mutations(&self) -> bool {
        matches!(
            &self.phase,
            Phase::Connected(active) if active.config.kind.tracks_mutations()
        )
    }

    /// How many of the active connection's mutations are still running. Drives the
    /// status-bar indicator, so an edit that outlived its submit stays visible
    /// without the panel being open.
    pub(crate) fn running_mutations(&self, cx: &App) -> usize {
        match &self.phase {
            Phase::Connected(active) => active
                .server
                .read(cx)
                .mutations
                .iter()
                .filter(|m| !m.done)
                .count(),
            _ => 0,
        }
    }

    /// Ask the backend for the current listing. Cheap (one catalog query), so this is
    /// also fired after every submit: a mutation that is still running is exactly
    /// what the user needs to see next.
    pub(crate) fn refresh_mutations(&mut self, cx: &mut Context<Self>) {
        if !self.tracks_mutations() {
            return;
        }
        if let Phase::Connected(active) = &self.phase {
            active.server.update(cx, |panel, cx| {
                panel.mutations_loading = true;
                cx.notify();
            });
        }
        self.send_active(Command::ListMutations);
        cx.notify();
    }

    /// A listing arrived.
    pub(crate) fn on_mutations_loaded(
        &mut self,
        session: Option<red_service::SessionId>,
        mutations: Vec<MutationInfo>,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.conn_for(session).map(|a| a.server.clone()) {
            panel.update(cx, |panel, cx| {
                panel.mutations = mutations;
                panel.mutations_loading = false;
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Cancel one mutation. Further part rewrites stop; the parts already rewritten
    /// stay rewritten, because there is no transaction to undo them -- so this is a
    /// "stop spending" button, not an undo, and the panel says so.
    pub(crate) fn kill_mutation(&mut self, database: String, table: String, id: String) {
        self.send_active(Command::KillMutation {
            table: TableRef {
                schema: Some(database),
                name: table,
            },
            id,
        });
    }

    /// The Mutations dock panel.
    pub(crate) fn render_mutations(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let (text, muted, faint, border_soft, red, yellow, green) = (
            theme.text,
            theme.text_muted,
            theme.text_faint,
            theme.border_soft,
            theme.red,
            theme.yellow,
            theme.green,
        );
        let size_11 = theme.scale(11.);
        let running = active
            .server
            .read(cx)
            .mutations
            .iter()
            .filter(|m| !m.done)
            .count();

        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(34.))
            .border_b_1()
            .border_color(border_soft)
            .child(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(text)
                    .child(crate::i18n::tr!("server.mutations_title", "Mutations")),
            )
            .child(
                div()
                    .text_size(size_11)
                    .text_color(muted)
                    .child(if running == 0 {
                        "all done".to_string()
                    } else {
                        format!("{running} running")
                    }),
            )
            .child(
                div().ml_auto().child(
                    Button::new("mutations-refresh", "Refresh")
                        .variant(ButtonVariant::Ghost)
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_mutations(cx))),
                ),
            );

        let rows: Vec<gpui::AnyElement> = active
            .server
            .read(cx)
            .mutations
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let (dot, status) = match (m.done, m.fail_reason.is_some()) {
                    (_, true) => (red, "failed".to_string()),
                    (true, _) => (green, "done".to_string()),
                    // Parts, not rows: a one-cell edit can still have many to rewrite,
                    // and saying "parts" is what makes that number make sense.
                    (false, _) => (yellow, format!("{} parts to go", m.parts_to_do)),
                };
                let command: String = m.command.chars().take(COMMAND_CHARS).collect();
                let (database, table, id) = (m.database.clone(), m.table.clone(), m.id.clone());
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(border_soft)
                    .text_size(size_11)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .child(div().size(px(6.)).rounded_full().bg(dot))
                            .child(
                                div()
                                    .text_color(text)
                                    .child(format!("{}.{}", m.database, m.table)),
                            )
                            .child(div().text_color(muted).child(status))
                            .when(!m.done, |d| {
                                d.child(
                                    div().ml_auto().child(
                                        Button::new(
                                            SharedString::from(format!("mutation-kill-{i}")),
                                            "Cancel",
                                        )
                                        .variant(ButtonVariant::Ghost)
                                        .size(ButtonSize::Sm)
                                        .tooltip(
                                            "Stop further part rewrites. Parts already \
                                             rewritten stay changed.",
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, _| {
                                                this.kill_mutation(
                                                    database.clone(),
                                                    table.clone(),
                                                    id.clone(),
                                                )
                                            }),
                                        ),
                                    ),
                                )
                            }),
                    )
                    .child(
                        div()
                            .font_family(theme.mono_family.clone())
                            .text_color(muted)
                            .child(command),
                    )
                    .child(div().text_color(faint).child(m.created.clone()))
                    .children(m.fail_reason.clone().map(|reason| {
                        div()
                            .text_color(red)
                            .child(format!("last failure: {reason}"))
                    }))
                    .into_any_element()
            })
            .collect();

        let body = if rows.is_empty() {
            div()
                .flex_1()
                .p_3()
                .text_size(size_11)
                .text_color(faint)
                .child(if active.server.read(cx).mutations_loading {
                    "Loading…"
                } else {
                    "No mutations. Updates and deletes appear here while the engine applies them."
                })
                .into_any_element()
        } else {
            div()
                .id("mutations-list")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .children(rows)
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_panel)
            .border_r_1()
            .border_color(theme.border)
            .font_family(theme.font_family.clone())
            .child(header)
            .child(body)
    }
}
