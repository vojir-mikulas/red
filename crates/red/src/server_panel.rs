//! The Server panel: what the database server is doing right now, and how to
//! stop it.
//!
//! Redis has had this since `kvmonitor.rs` (SLOWLOG / MONITOR / CLIENT LIST with
//! `CLIENT KILL`); ClickHouse had half of it in the Mutations panel; the SQL
//! engines had none of it. Pointing RED at a production database and asking
//! "what is holding this lock" meant leaving RED.
//!
//! **One dock, two views.** Mutations was already a dock panel about the
//! connection rather than the focused result, and Sessions is the same kind of
//! thing, so they share one dock with a segmented switch rather than becoming two
//! adjacent docks that each claim a column of the window.
//!
//! The kill ladder is deliberate and lives mostly outside this file:
//! `is_self` sessions offer nothing, a read-only connection offers nothing, the
//! engine must advertise the mode in `session_caps`, and the confirm rides the
//! shared [`PendingWrite`] modal so a production connection cannot silence it.
//! `kill_session` is not, and should not become, an AI or MCP tool.

use flint::prelude::*;
use flint::{Button, ButtonSize, ButtonVariant};
use gpui::{Context, SharedString, div, prelude::*, px};
use red_core::{KillMode, ServerSession, SessionKey};
use red_service::Command;

use crate::app::{ActiveConn, AppState, PendingWrite, Phase};

/// How much of a statement a session row shows before clipping. Enough to tell
/// two queries apart; the full text is one click away in the expanded row.
const QUERY_CHARS: usize = 220;

/// Which view the Server dock is showing.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ServerView {
    #[default]
    Sessions,
    Mutations,
}

impl AppState {
    /// Whether the active connection has a server worth inspecting: other
    /// sessions, background mutations, or both. Gates the dock and its toggle.
    pub(crate) fn has_server_panel(&self) -> bool {
        match &self.phase {
            Phase::Connected(a) => {
                a.config.kind.session_caps().supported || a.config.kind.tracks_mutations()
            }
            _ => false,
        }
    }

    /// Show or hide the Server dock, refreshing on the way in so it never opens
    /// on a stale list.
    pub(crate) fn toggle_server_panel(&mut self, cx: &mut Context<Self>) {
        if !self.has_server_panel() {
            return;
        }
        let opened = match &mut self.phase {
            Phase::Connected(active) => {
                active.server_open = !active.server_open;
                // Land on the view the engine actually has, so a ClickHouse-only
                // user is not shown an empty Sessions tab and a Postgres user is
                // not shown a Mutations tab that can never populate.
                if !active.config.kind.session_caps().supported {
                    active.server_view = ServerView::Mutations;
                }
                active.server_open
            }
            _ => false,
        };
        if opened {
            self.refresh_server_panel(cx);
        }
        cx.notify();
    }

    pub(crate) fn set_server_view(&mut self, view: ServerView, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.server_view = view;
        }
        self.refresh_server_panel(cx);
        cx.notify();
    }

    /// Refresh whichever view is showing.
    pub(crate) fn refresh_server_panel(&mut self, cx: &mut Context<Self>) {
        let view = match &self.phase {
            Phase::Connected(a) => a.server_view,
            _ => return,
        };
        match view {
            ServerView::Mutations => self.refresh_mutations(cx),
            ServerView::Sessions => {
                if let Phase::Connected(active) = &mut self.phase {
                    active.sessions_loading = true;
                }
                self.send_active(Command::ListServerSessions);
                cx.notify();
            }
        }
    }

    /// A session listing arrived.
    pub(crate) fn on_server_sessions(
        &mut self,
        session: Option<red_service::SessionId>,
        sessions: Vec<ServerSession>,
        restricted: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.conn_mut(session) {
            active.sessions = sessions;
            active.sessions_restricted = restricted;
            active.sessions_loading = false;
        }
        cx.notify();
    }

    /// A kill was accepted: re-list rather than mutating the local copy, so what
    /// the panel shows is always what the server reports.
    pub(crate) fn on_server_session_killed(&mut self, cx: &mut Context<Self>) {
        self.notify(flint::ToastVariant::Success, "The server accepted it.", cx);
        self.refresh_server_panel(cx);
    }

    /// Raise the confirm for stopping one session. The ladder's UI half; the
    /// backend re-checks read-only, and the driver refuses again beneath that.
    fn confirm_kill_session(&mut self, key: SessionKey, mode: KillMode, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let Some(s) = active.sessions.iter().find(|s| s.key == key) else {
            return;
        };
        // Never RED's own connection: stopping it produces a reconnect dance and
        // no useful outcome. Guarded here as well as in the render, because a
        // stale list could still hand this path a self row.
        if s.is_self {
            return;
        }
        let who = match (&s.user, &s.database) {
            (Some(u), Some(d)) => format!("{u} on {d}"),
            (Some(u), None) => u.clone(),
            (None, Some(d)) => format!("a session on {d}"),
            (None, None) => format!("session {key}"),
        };
        let who = format!("{who} ({})", fmt_elapsed(s.elapsed_secs));
        let query = s
            .query
            .clone()
            .unwrap_or_else(|| "(this role cannot see the statement)".to_string());
        self.confirm_exec = self.pending_confirm(PendingWrite::KillSession {
            key,
            mode,
            who,
            query,
        });
        cx.notify();
    }

    /// Fire the confirmed kill against the connection it was raised on. A server
    /// session key means nothing on another server, so routing this to whichever
    /// connection is foreground at click time could kill an unrelated session — or,
    /// where keys are small integers, an arbitrary one.
    pub(crate) fn run_kill_session(
        &mut self,
        session: red_service::SessionId,
        key: SessionKey,
        mode: KillMode,
    ) {
        self.service
            .send_to(session, Command::KillServerSession { key, mode });
    }

    /// The Server dock.
    pub(crate) fn render_server_panel(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let caps = active.config.kind.session_caps();
        let view = active.server_view;

        // The view switch, offered only where both views exist. On an engine with
        // one of them the dock is just that panel, with no control that does
        // nothing.
        let switch = (caps.supported && active.config.kind.tracks_mutations()).then(|| {
            let mut row = div().flex().items_center().gap_1();
            for (label, which) in [
                ("Sessions", ServerView::Sessions),
                ("Mutations", ServerView::Mutations),
            ] {
                row = row.child(
                    Button::new(SharedString::from(format!("server-view-{label}")), label)
                        .variant(if view == which {
                            ButtonVariant::Secondary
                        } else {
                            ButtonVariant::Ghost
                        })
                        .size(ButtonSize::Sm)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.set_server_view(which, cx)),
                        ),
                );
            }
            row
        });

        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(34.))
            .border_b_1()
            .border_color(theme.border_soft)
            .child(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(crate::i18n::tr!("server.title", "Server")),
            )
            .children(switch)
            .child(
                div().ml_auto().child(
                    Button::new("server-refresh", "Refresh")
                        .variant(ButtonVariant::Ghost)
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_server_panel(cx))),
                ),
            );

        let body = match view {
            ServerView::Mutations => self.render_mutations(active, cx).into_any_element(),
            ServerView::Sessions => self.render_sessions(active, cx),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_panel)
            .child(header)
            .child(div().flex_1().min_h(px(0.)).child(body))
    }

    /// The Sessions view: one row per server session, longest-running first,
    /// with the blocked ones marked and the kill actions where they are allowed.
    fn render_sessions(&self, active: &ActiveConn, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let caps = active.config.kind.session_caps();
        let writable = !active.config.read_only;
        let size_11 = theme.scale(11.);

        // A role that cannot see other sessions is a normal, common state on a
        // locked-down server. Explaining it is the difference between "this server
        // is idle" and "you are not allowed to see this".
        let banner = active.sessions_restricted.then(|| {
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
        let blockers: std::collections::HashSet<&SessionKey> = active
            .sessions
            .iter()
            .flat_map(|s| s.blocked_by.iter())
            .collect();
        let blocking_count = blockers.len();

        let rows: Vec<gpui::AnyElement> = active
            .sessions
            .iter()
            .map(|s| self.render_session_row(s, &blockers, writable, caps, cx))
            .collect();

        let list = if active.sessions.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_2()
                .text_size(size_11)
                .text_color(theme.text_muted)
                .child(if active.sessions_loading {
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
            let blocked = active
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
        cx: &mut Context<Self>,
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
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.confirm_kill_session(
                                            key_cancel.clone(),
                                            KillMode::Cancel,
                                            cx,
                                        );
                                    },
                                )),
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
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.confirm_kill_session(
                                        key_term.clone(),
                                        KillMode::Terminate,
                                        cx,
                                    );
                                },
                            )),
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
fn fmt_elapsed(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 60.0 {
        format!("{s:.1}s")
    } else if s < 3600.0 {
        format!("{}m {}s", (s / 60.0) as u64, (s % 60.0) as u64)
    } else {
        format!("{}h {}m", (s / 3600.0) as u64, ((s % 3600.0) / 60.0) as u64)
    }
}
