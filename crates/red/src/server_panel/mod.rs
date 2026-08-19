//! The Server panel: what the database server is using, what it is doing, and
//! who is connected, in one dock that reads the same on all three seams.
//!
//! **One dock, three views.** [`Overview`](ServerView::Overview) is live metrics
//! and the view the panel opens on, because it is what you open the panel to
//! see. [`Sessions`](ServerView::Sessions) is who is connected and what they are
//! running: SQL sessions, Redis clients and Mongo current-ops, all flattened to
//! `ServerSession` in the backend's dispatch adapters, so nothing here branches
//! on the engine beyond what
//! [`session_caps`](red_core::DbKind::session_caps) says.
//! [`Mutations`](ServerView::Mutations) is ClickHouse's background work, shown
//! only where it exists. The switch offers only the views the engine can
//! populate, and the panel lands on the first of them.
//!
//! Redis keeps its Monitor tab. `SLOWLOG` and `MONITOR` are Redis-specific tools
//! with no SQL or Mongo analogue, and folding them in here would make the shared
//! panel a union of three engines' quirks; the Overview links across to them
//! instead.
//!
//! The kill ladder is deliberate and lives mostly outside this file: `is_self`
//! sessions offer nothing, a read-only connection offers nothing, the engine
//! must advertise the mode in `session_caps`, and the confirm rides the shared
//! [`PendingWrite`] modal so a production connection cannot silence it.
//! `kill_session` is not, and should not become, an AI or MCP tool -- on any
//! seam, Mongo's `killOp` included.

mod overview;
mod sessions;

use std::time::Duration;

use flint::prelude::*;
use flint::{Button, ButtonSize, ButtonVariant};
use gpui::{AsyncApp, Context, SharedString, WeakEntity, div, prelude::*, px};
use red_core::{DbKind, KillMode, ServerSession, SessionKey};
use red_service::{Command, Epoch};

use crate::app::{ActiveConn, AppState, PendingWrite, Phase};

/// The Server dock's state, owned by the panel rather than scattered across the
/// connection: what it is showing, its two metric samples, its listings, and the
/// cadence it re-samples at.
///
/// One connection has one of these. It is `Default`-constructed with the panel
/// closed, no samples and no refresh, which is the state a fresh connection is in.
#[derive(Default)]
pub(crate) struct ServerPanel {
    /// The engine behind this connection, for the capability gates: which views
    /// exist, whether sessions can be killed, what the metrics are called. Fixed
    /// for the connection's life, so it is set once at construction.
    pub kind: DbKind,
    /// Whether the connection accepts writes. A read-only one offers no kill.
    pub read_only: bool,
    /// The app, for the actions the dock triggers but does not own: refreshing,
    /// switching view, setting the interval, and the kill ladder — which rides the
    /// shared confirm modal and must keep doing so.
    pub app: Option<gpui::WeakEntity<AppState>>,
    /// Whether the dock is shown. Offered on every engine with a server behind it,
    /// which is all of them but SQLite (see `AppState::has_server_panel`).
    pub open: bool,
    /// Which view the dock shows (Overview / Sessions / Mutations).
    pub view: ServerView,
    /// The latest metrics sample, and the one before it.
    ///
    /// The previous sample is kept for exactly one reason: a monotonic counter such
    /// as MySQL `Questions` or Postgres `xact_commit` is unreadable on its own, and
    /// the *rate* is what the user wants. The panel is the only place that knows the
    /// interval between two refreshes, so it is the only place that can derive one.
    /// It is not a history and must not grow into one.
    pub metrics: Option<red_core::server::ServerSnapshot>,
    pub metrics_prev: Option<red_core::server::ServerSnapshot>,
    /// The in-flight sample's epoch; a reply carrying any other is dropped, so a slow
    /// sample cannot land after a newer one and corrupt the rate pair.
    pub metrics_epoch: Epoch,
    pub metrics_loading: bool,
    /// The sample failed outright, as opposed to individual metrics being invisible
    /// to this role (which the snapshot carries in `unavailable`).
    pub metrics_error: Option<String>,
    /// How often the panel re-samples on its own; `None` (the default) is off.
    /// Floored at [`MIN_REFRESH_SECS`]: polling `CLIENT LIST` or `pg_stat_activity`
    /// against production is real load, so this is opt-in and its interval is shown.
    pub refresh: Option<Duration>,
    /// Bumped whenever the interval changes, so a timer armed under the old one
    /// retires instead of firing once more at the wrong cadence.
    pub refresh_gen: u64,
    /// The last server-session listing, longest-running first.
    pub sessions: Vec<ServerSession>,
    /// The connected role could not see other sessions' SQL, so the panel says so
    /// rather than reading as an idle server.
    pub sessions_restricted: bool,
    /// A listing is in flight.
    pub sessions_loading: bool,
    /// The last `system.mutations` listing, unfinished first. Refreshed on open, on
    /// every submit, and while anything is still running.
    pub mutations: Vec<red_core::MutationInfo>,
    /// A listing is in flight (the panel shows it instead of an empty state).
    pub mutations_loading: bool,
}

/// Which view the Server dock is showing.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ServerView {
    /// Live server metrics: memory, throughput, connections, uptime.
    #[default]
    Overview,
    /// Who is connected and what they are running.
    Sessions,
    /// ClickHouse background mutations.
    Mutations,
}

impl ServerView {
    /// Every view, in the order the switch draws them.
    const ORDER: [ServerView; 3] = [
        ServerView::Overview,
        ServerView::Sessions,
        ServerView::Mutations,
    ];

    /// Whether this engine can populate this view. The whole of the panel's
    /// engine awareness: everything else reads the answer off `DbKind`
    /// descriptors rather than comparing engines.
    fn supported_by(self, kind: DbKind) -> bool {
        match self {
            ServerView::Overview => kind.reports_metrics(),
            ServerView::Sessions => kind.session_caps().supported,
            ServerView::Mutations => kind.tracks_mutations(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            ServerView::Overview => "Overview",
            ServerView::Sessions => "Sessions",
            ServerView::Mutations => "Mutations",
        }
    }

    fn available(kind: DbKind) -> impl Iterator<Item = ServerView> {
        Self::ORDER
            .into_iter()
            .filter(move |v| v.supported_by(kind))
    }
}

/// Floor on the auto-refresh interval. A five-second poll of `CLIENT LIST` or
/// `pg_stat_activity` against a production server is real load; anything faster
/// stops being a monitor and becomes a second workload. Off is still the
/// default, and the chosen interval is visible in the panel rather than buried
/// in settings.
pub(crate) const MIN_REFRESH_SECS: u64 = 2;

/// The intervals the auto-refresh menu offers, `0` meaning off.
pub(crate) const REFRESH_CHOICES: [u64; 5] = [0, 2, 5, 10, 30];

impl AppState {
    /// Whether the active connection has a server worth inspecting: live
    /// metrics, other sessions, background mutations, or any of them. Gates the
    /// dock and its toggle.
    pub(crate) fn has_server_panel(&self) -> bool {
        match &self.phase {
            Phase::Connected(a) => ServerView::available(a.config.kind).next().is_some(),
            _ => false,
        }
    }

    /// Show or hide the Server dock, refreshing on the way in so it never opens
    /// on a stale sample.
    pub(crate) fn toggle_server_panel(&mut self, cx: &mut Context<Self>) {
        if !self.has_server_panel() {
            return;
        }
        let opened = match &self.phase {
            Phase::Connected(active) => {
                let kind = active.config.kind;
                active.server.update(cx, |panel, cx| {
                    panel.open = !panel.open;
                    // Land on a view the engine can actually populate, so a Postgres
                    // user is not shown a Mutations tab that can never fill and a
                    // SQLite-shaped engine is not shown an Overview with nothing in
                    // it. Only re-lands when the retained view has become invalid,
                    // so switching connections does not fight the user's choice.
                    if !panel.view.supported_by(kind) {
                        panel.view = ServerView::available(kind).next().unwrap_or_default();
                    }
                    cx.notify();
                    panel.open
                })
            }
            _ => false,
        };
        if opened {
            self.refresh_server_panel(cx);
            self.arm_server_refresh(cx);
        }
        cx.notify();
    }

    pub(crate) fn set_server_view(&mut self, view: ServerView, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &self.phase {
            active.server.update(cx, |panel, cx| {
                panel.view = view;
                cx.notify();
            });
        }
        self.refresh_server_panel(cx);
        cx.notify();
    }

    /// Refresh whichever view is showing.
    pub(crate) fn refresh_server_panel(&mut self, cx: &mut Context<Self>) {
        let view = match &self.phase {
            Phase::Connected(a) => a.server.read(cx).view,
            _ => return,
        };
        match view {
            ServerView::Mutations => self.refresh_mutations(cx),
            ServerView::Sessions => {
                if let Phase::Connected(active) = &self.phase {
                    active.server.update(cx, |panel, cx| {
                        panel.sessions_loading = true;
                        cx.notify();
                    });
                }
                self.send_active(Command::ListServerSessions);
                cx.notify();
            }
            ServerView::Overview => {
                let epoch = crate::result::new_epoch();
                if let Phase::Connected(active) = &self.phase {
                    active.server.update(cx, |panel, cx| {
                        panel.metrics_epoch = epoch;
                        panel.metrics_loading = true;
                        cx.notify();
                    });
                }
                self.send_active(Command::FetchServerMetrics { epoch });
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
        if let Some(panel) = self.conn_for(session).map(|a| a.server.clone()) {
            panel.update(cx, |panel, cx| {
                panel.sessions = sessions;
                panel.sessions_restricted = restricted;
                panel.sessions_loading = false;
                cx.notify();
            });
        }
        cx.notify();
    }

    /// A metrics sample arrived. The previous sample is kept, and only here:
    /// it is what the panel derives a per-second rate from, and the panel is the
    /// only place that knows the interval between two refreshes.
    pub(crate) fn on_server_metrics(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: Epoch,
        snapshot: red_core::server::ServerSnapshot,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.conn_for(session).map(|a| a.server.clone()) {
            panel.update(cx, |panel, cx| {
                if panel.metrics_epoch != epoch {
                    return;
                }
                // Superseding rather than swapping: a sample that arrived out of
                // order would otherwise become the baseline for the *next* rate.
                panel.metrics_prev = panel.metrics.take();
                panel.metrics = Some(snapshot);
                panel.metrics_error = None;
                panel.metrics_loading = false;
                cx.notify();
            });
        }
        cx.notify();
    }

    pub(crate) fn on_server_metrics_failed(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: Epoch,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.conn_for(session).map(|a| a.server.clone()) {
            panel.update(cx, |panel, cx| {
                if panel.metrics_epoch != epoch {
                    return;
                }
                panel.metrics_loading = false;
                panel.metrics_error = Some(message);
                cx.notify();
            });
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
        let Some(s) = active
            .server
            .read(cx)
            .sessions
            .iter()
            .find(|s| s.key == key)
        else {
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
        let who = format!("{who} ({})", sessions::fmt_elapsed(s.elapsed_secs));
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

    /// Set (or clear) the panel's auto-refresh interval, clamped to
    /// [`MIN_REFRESH_SECS`].
    pub(crate) fn set_server_refresh(&mut self, secs: u64, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &self.phase {
            active.server.update(cx, |panel, cx| {
                panel.refresh = (secs > 0).then(|| Duration::from_secs(secs.max(MIN_REFRESH_SECS)));
                // Bumped on every change so an in-flight timer from the previous
                // interval retires instead of firing once more at the old cadence.
                panel.refresh_gen = panel.refresh_gen.wrapping_add(1);
                cx.notify();
            });
        }
        self.arm_server_refresh(cx);
        cx.notify();
    }

    /// Arm the next auto-refresh tick, if the panel is open and an interval is
    /// set. Re-arms itself; a closed panel, a changed interval, or a switched
    /// connection retires the pending timer rather than refreshing behind the
    /// user's back.
    fn arm_server_refresh(&mut self, cx: &mut Context<Self>) {
        let armed = match &self.phase {
            Phase::Connected(a) if a.server.read(cx).open => a
                .server
                .read(cx)
                .refresh
                .map(|i| (i, a.server.read(cx).refresh_gen, a.session)),
            _ => None,
        };
        let Some((interval, generation, session)) = armed else {
            return;
        };
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(interval).await;
            this.update(cx, |this, cx| {
                let still = match &this.phase {
                    Phase::Connected(a) => {
                        a.server.read(cx).open
                            && a.session == session
                            && a.server.read(cx).refresh == Some(interval)
                            && a.server.read(cx).refresh_gen == generation
                    }
                    _ => false,
                };
                if !still {
                    return;
                }
                this.refresh_server_panel(cx);
                this.arm_server_refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Wrap a shell's work area in the Server dock when it is open.
    ///
    /// One helper rather than the same fifty lines of `SplitPane` wiring in
    /// three shells. The dock hangs off the *connection*, not the query tabs, so
    /// the SQL workspace, the Redis browser and the Mongo browser all get it on
    /// the same terms and at the same width.
    pub(crate) fn with_server_dock(
        &self,
        active: &ActiveConn,
        id: &'static str,
        body: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if !active.server.read(cx).open {
            return body;
        }
        // The dock is its own view: the shell gives it a pane and a width, and the
        // panel draws itself from state it owns. A refresh tick repaints it alone.
        let pane = active.server.clone().into_any_element();
        let view = cx.entity().downgrade();
        let (start, resize, end) = (view.clone(), view.clone(), view);
        flint::SplitPane::new(id, gpui::Axis::Horizontal)
            .size(active.server_w)
            .gutter(px(1.))
            .drag(active.server_drag)
            .min_first(px(240.))
            .max_first(px(560.))
            .on_drag_start(move |anchor, _, cx| {
                start
                    .update(cx, |this, cx| {
                        if let Phase::Connected(a) = &mut this.phase {
                            a.server_drag = Some(anchor);
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .on_resize(move |size, _, cx| {
                resize
                    .update(cx, |this, cx| {
                        if let Phase::Connected(a) = &mut this.phase {
                            a.server_w = size;
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .on_drag_end(move |_, cx| {
                end.update(cx, |this, cx| {
                    if let Phase::Connected(a) = &mut this.phase {
                        a.server_drag = None;
                    }
                    cx.notify();
                })
                .ok();
            })
            .first(pane)
            .second(body)
            .into_any_element()
    }

    /// The status-bar affordance that opens the dock, for the shells whose
    /// status bar names the connection but has no endpoint chip of its own. An
    /// icon rather than a word: the Redis and Mongo status bars are already
    /// full.
    pub(crate) fn render_server_toggle(
        &self,
        active: &ActiveConn,
        id: &'static str,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.has_server_panel() {
            return None;
        }
        Some(
            div()
                .id(id)
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .size(px(20.))
                .rounded(px(4.))
                .cursor_pointer()
                .tooltip(flint::Tooltip::text("Server panel"))
                .hover(|s| s.bg(theme.bg_elevated))
                .child(crate::icons::icon(
                    "activity",
                    theme.scale(14.),
                    if active.server.read(cx).open {
                        theme.accent
                    } else {
                        theme.text_muted
                    },
                ))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_server_panel(cx)))
                .into_any_element(),
        )
    }
}

impl gpui::Render for ServerPanel {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_dock(cx)
    }
}

impl ServerPanel {
    /// The Server dock.
    fn render_dock(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let view = self.view;
        let kind = self.kind;

        // The view switch, offered only where more than one view exists. On an
        // engine with a single view the dock is just that panel, with no control
        // that does nothing.
        let views: Vec<ServerView> = ServerView::available(kind).collect();
        let switch = (views.len() > 1).then(|| {
            let mut row = div().flex().items_center().gap_1();
            for which in views {
                let label = which.label();
                row = row.child(
                    Button::new(SharedString::from(format!("server-view-{label}")), label)
                        .variant(if view == which {
                            ButtonVariant::Secondary
                        } else {
                            ButtonVariant::Ghost
                        })
                        .size(ButtonSize::Sm)
                        .on_click({
                            let app = self.app.clone();
                            move |_, _, cx: &mut gpui::App| {
                                if let Some(app) = &app {
                                    app.update(cx, |this, cx| this.set_server_view(which, cx))
                                        .ok();
                                }
                            }
                        }),
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
                div()
                    .ml_auto()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(self.render_refresh_pill(&theme))
                    .child(
                        Button::new("server-refresh", "Refresh")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click({
                                let app = self.app.clone();
                                move |_, _, cx: &mut gpui::App| {
                                    if let Some(app) = &app {
                                        app.update(cx, |this, cx| this.refresh_server_panel(cx))
                                            .ok();
                                    }
                                }
                            }),
                    ),
            );

        let body = match view {
            ServerView::Mutations => self.render_mutations(cx).into_any_element(),
            ServerView::Sessions => self.render_sessions(cx),
            ServerView::Overview => self.render_overview(cx),
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

    /// The auto-refresh control: a pill naming the live interval, or a muted
    /// clock when off. The interval is on screen rather than in settings, so
    /// nobody discovers a five-second poll of production by reading a config
    /// file.
    fn render_refresh_pill(&self, theme: &flint::Theme) -> gpui::AnyElement {
        let current = self.refresh.map(|d| d.as_secs());
        let label = match current {
            Some(secs) => format!("every {secs}s"),
            None => "auto".to_string(),
        };
        // Cycles rather than opening a menu: five choices, and a dock header has
        // no room for a popover that would cover the numbers being read.
        let next = {
            let i = REFRESH_CHOICES
                .iter()
                .position(|s| *s == current.unwrap_or(0))
                .unwrap_or(0);
            REFRESH_CHOICES[(i + 1) % REFRESH_CHOICES.len()]
        };
        div()
            .id("server-auto-refresh")
            .flex()
            .items_center()
            .gap_1()
            .px_1p5()
            .rounded(px(4.))
            .cursor_pointer()
            .text_size(theme.scale(10.5))
            .text_color(if current.is_some() {
                theme.accent
            } else {
                theme.text_faint
            })
            .hover(|s| s.bg(theme.bg_elevated))
            .tooltip(flint::Tooltip::text(
                "Auto-refresh this panel. Polling a production server has a cost; off by default.",
            ))
            .child(crate::icons::icon(
                "refresh-cw",
                theme.scale(11.),
                if current.is_some() {
                    theme.accent
                } else {
                    theme.text_faint
                },
            ))
            .child(label)
            .on_click({
                let app = self.app.clone();
                move |_, _, cx: &mut gpui::App| {
                    if let Some(app) = &app {
                        app.update(cx, |this, cx| this.set_server_refresh(next, cx))
                            .ok();
                    }
                }
            })
            .into_any_element()
    }
}
