//! The Sessions view: one row per server session, longest-running first, with
//! the blocked ones marked and the kill actions where they are allowed.
//!
//! Engine-agnostic by construction. The backend's dispatch adapters flatten a
//! Postgres backend, a Redis client and a Mongo current-op into the same
//! [`ServerSession`], so nothing here knows which seam answered beyond what
//! [`SessionCaps`](red_core::SessionCaps) reports: whether a cancel exists,
//! whether a terminate exists, and whether an empty `blocked_by` means "free"
//! or "this engine has no wait graph".

use flint::prelude::*;
use flint::{Button, ButtonSize, ButtonVariant};
use gpui::{Context, SharedString, div, prelude::*, px};
use red_core::{KillMode, ServerSession, SessionKey};

use super::ServerPanel;

/// How much of a statement a session row shows before clipping. Enough to tell
/// two queries apart; the full text is one click away in the expanded row.
const QUERY_CHARS: usize = 220;

impl ServerPanel {
    pub(super) fn render_sessions(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let panel = self;
        let theme = cx.theme().clone();
        let caps = self.kind.session_caps();
        let writable = !self.read_only;
        let size_11 = theme.scale(11.);

        // A role that cannot see other sessions is a normal, common state on a
        // locked-down server. Explaining it is the difference between "this server
        // is idle" and "you are not allowed to see this".
        let banner = panel.sessions_restricted.then(|| {
            div()
                .px_3()
                .py_2()
                .text_size(size_11)
                .text_color(theme.yellow)
                .child(
                    "This role can only see its own sessions. Grant pg_monitor (Postgres) \
                     or PROCESS (MySQL) to see the rest.",
                )
        });

        // Who is blocking someone, so a blocker can be marked as the root cause
        // rather than looking like any other running query.
        let blockers: std::collections::HashSet<&SessionKey> = self
            .sessions
            .iter()
            .flat_map(|s| s.blocked_by.iter())
            .collect();
        let blocking_count = blockers.len();

        let rows: Vec<gpui::AnyElement> = panel
            .sessions
            .iter()
            .map(|s| self.render_session_row(s, &blockers, writable, caps, cx))
            .collect();

        let list = if panel.sessions.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_2()
                .text_size(size_11)
                .text_color(theme.text_muted)
                .child(if panel.sessions_loading {
                    "loading…"
                } else {
                    "Nothing is running on this server."
                })
                .into_any_element()
        } else {
            div()
                .id("server-sessions-list")
                .flex_1()
                .min_h(px(0.))
                .overflow_scroll()
                .children(rows)
                .into_any_element()
        };

        // The root-cause line: the single most useful sentence in the panel when a
        // lock chain is the reason someone is asking.
        let summary = (blocking_count > 0).then(|| {
            let blocked = self
                .sessions
                .iter()
                .filter(|s| !s.blocked_by.is_empty())
                .count();
            div()
                .flex_shrink_0()
                .px_3()
                .py_1()
                .border_t_1()
                .border_color(theme.border_soft)
                .text_size(theme.scale(10.5))
                .text_color(theme.orange)
                .child(crate::i18n::tr!(
                    "server.sessions_blocking",
                    "{blocking_count} session(s) blocking {blocked} other(s)",
                    blocking_count = blocking_count,
                    blocked = blocked
                ))
        });

        div()
            .size_full()
            .flex()
            .flex_col()
            .children(banner)
            .child(list)
            .children(summary)
            .into_any_element()
    }

    fn render_session_row(
        &self,
        s: &ServerSession,
        blockers: &std::collections::HashSet<&SessionKey>,
        writable: bool,
        caps: red_core::SessionCaps,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let size_11 = theme.scale(11.);
        let blocked = !s.blocked_by.is_empty();
        let is_blocker = blockers.contains(&s.key);
        // Colour says the one thing worth reading at a glance: red is waiting on
        // someone, orange is what everyone else is waiting on, green is fine.
        let dot = if blocked {
            theme.red
        } else if is_blocker {
            theme.orange
        } else {
            theme.green
        };

        let who = [
            s.user.clone(),
            s.database.clone(),
            s.application.clone(),
            s.client_addr.clone(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");

        let query: String = s
            .query
            .clone()
            .unwrap_or_else(|| "(not visible to this role)".to_string())
            .chars()
            .take(QUERY_CHARS)
            .collect();

        // The kill ladder's visible half: nothing for RED's own connection,
        // nothing on a read-only connection, and only the modes the engine says it
        // has. Everything offered here still goes through the confirm modal.
        let can_kill = writable && !s.is_self;
        let (key_cancel, key_term) = (s.key.clone(), s.key.clone());

        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border_soft)
            .text_size(size_11)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(div().size(px(6.)).rounded_full().bg(dot))
                    .child(div().text_color(theme.text).child(who))
                    .child(div().text_color(theme.text_muted).child(format!(
                        "{} · {}",
                        s.state,
                        fmt_elapsed(s.elapsed_secs)
                    )))
                    .when(s.is_self, |d| {
                        d.child(div().text_color(theme.text_faint).child(crate::i18n::tr!(
                            "server.this_connection",
                            "this connection"
                        )))
                    })
                    .when(can_kill && caps.can_cancel, |d| {
                        d.child(
                            div().ml_auto().child(
                                Button::new(
                                    SharedString::from(format!("session-cancel-{}", s.key)),
                                    "Cancel",
                                )
                                .variant(ButtonVariant::Ghost)
                                .size(ButtonSize::Sm)
                                .on_click({
                                    let app = self.app.clone();
                                    move |_, _, cx: &mut gpui::App| {
                                        if let Some(app) = &app {
                                            app.update(cx, |this, cx| {
                                                this.confirm_kill_session(
                                                    key_cancel.clone(),
                                                    KillMode::Cancel,
                                                    cx,
                                                );
                                            })
                                            .ok();
                                        }
                                    }
                                }),
                            ),
                        )
                    })
                    .when(can_kill && caps.can_terminate, |d| {
                        d.child(
                            Button::new(
                                SharedString::from(format!("session-term-{}", s.key)),
                                "Terminate",
                            )
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click({
                                let app = self.app.clone();
                                move |_, _, cx: &mut gpui::App| {
                                    if let Some(app) = &app {
                                        app.update(cx, |this, cx| {
                                            this.confirm_kill_session(
                                                key_term.clone(),
                                                KillMode::Terminate,
                                                cx,
                                            );
                                        })
                                        .ok();
                                    }
                                }
                            }),
                        )
                    }),
            )
            .when(blocked, |d| {
                d.child(div().text_color(theme.red).child(format!(
                        "blocked by {}",
                        s.blocked_by
                            .iter()
                            .map(|k| k.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )))
            })
            .when_some(s.wait.clone(), |d, wait| {
                d.child(div().text_color(theme.text_faint).child(wait))
            })
            .child(
                div()
                    .font_family(theme.mono_family.clone())
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(query),
            )
            .into_any_element()
    }
}

/// A duration a human reads at a glance. Sub-minute stays in seconds because that
/// is the resolution that matters when deciding whether to stop something.
pub(super) fn fmt_elapsed(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 60.0 {
        format!("{s:.1}s")
    } else if s < 3600.0 {
        format!("{}m {}s", (s / 60.0) as u64, (s % 60.0) as u64)
    } else {
        format!("{}h {}m", (s / 3600.0) as u64, ((s % 3600.0) / 60.0) as u64)
    }
}
