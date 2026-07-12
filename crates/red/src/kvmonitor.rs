//! The Redis diagnostics panel (see docs/plans/redis.md). Three
//! related-but-distinct views behind one panel:
//!
//! - **Slow log** — a one-shot `SLOWLOG GET` of the server's recorded slow
//!   commands (refreshable, resettable on a writable connection).
//! - **Live (MONITOR)** — a `MONITOR` firehose of every command the server
//!   runs, streamed in via `Event::KvMonitorLine` and capped like the Pub/Sub
//!   monitor's message log, so a busy server can't grow it without bound.
//! - **Clients** — a `CLIENT LIST` viewer (id/addr/name/db/age/idle/cmd), with
//!   a per-client `CLIENT KILL` on a writable connection.

use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use flint::prelude::*;
use gpui::{div, prelude::*, px, Context, SharedString, Window};
use red_core::kv::{ClientInfo, SlowlogEntry};
use red_service::{Command, SessionId};

use crate::app::{ActiveConn, AppState};

/// How many slow-log entries to request (`SLOWLOG GET <count>`). The log is a
/// small server-side ring anyway (default `slowlog-max-len` 128), so this is
/// effectively "all of it" on a default config.
const SLOWLOG_COUNT: usize = 128;
/// Oldest-evicted cap on resident MONITOR lines, mirroring the Pub/Sub
/// monitor's `MAX_MESSAGES`: MONITOR on a busy server is a genuine firehose.
const MAX_LINES: usize = 5_000;

/// Which diagnostics view is showing.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum MonitorView {
    #[default]
    Slowlog,
    Live,
    Clients,
}

pub(crate) struct KvMonitor {
    /// A dedicated epoch for the MONITOR subscription's lifecycle (its own,
    /// distinct from the browse/console/pubsub epochs), torn down by
    /// `CloseResult { epoch }` exactly like the Pub/Sub subscription.
    pub(crate) epoch: u64,
    pub(crate) view: MonitorView,
    pub(crate) slowlog: Vec<SlowlogEntry>,
    /// Set once the first `SLOWLOG GET` reply lands, so re-entering the panel
    /// doesn't refetch on every switch (the explicit Refresh button still does).
    pub(crate) slowlog_loaded: bool,
    pub(crate) slowlog_loading: bool,
    /// `true` while a `MONITOR` stream is live (Start pressed, not yet Stopped).
    pub(crate) monitoring: bool,
    /// `true` when the firehose is paused: the stream stays live but incoming
    /// lines are dropped so the view holds still (a tail -f style freeze).
    pub(crate) paused: bool,
    pub(crate) lines: Vec<String>,
    /// The `CLIENT LIST` viewer's rows.
    pub(crate) clients: Vec<ClientInfo>,
    /// Set once the first `CLIENT LIST` reply lands (lazy load on first open).
    pub(crate) clients_loaded: bool,
    pub(crate) clients_loading: bool,
}

impl KvMonitor {
    pub(crate) fn new() -> Self {
        Self {
            epoch: crate::result::next_kv_epoch(),
            view: MonitorView::default(),
            slowlog: Vec::new(),
            slowlog_loaded: false,
            slowlog_loading: false,
            monitoring: false,
            paused: false,
            lines: Vec::new(),
            clients: Vec::new(),
            clients_loaded: false,
            clients_loading: false,
        }
    }
}

impl AppState {
    /// Switch diagnostics views. Opening Slow-log or Clients for the first time
    /// triggers their lazy fetch (`SLOWLOG GET` / `CLIENT LIST`).
    pub(crate) fn kv_set_monitor_view(
        &mut self,
        session: SessionId,
        view: MonitorView,
        cx: &mut Context<Self>,
    ) {
        let load = {
            let Some(mon) = self
                .conn_mut(Some(session))
                .and_then(|a| a.kv_view.as_mut())
                .and_then(|v| v.active_monitor_mut())
            else {
                return;
            };
            mon.view = view;
            match view {
                MonitorView::Slowlog if !mon.slowlog_loaded => Some(true),
                MonitorView::Clients if !mon.clients_loaded => Some(false),
                _ => None,
            }
        };
        match load {
            Some(true) => self.kv_load_slowlog(session, cx),
            Some(false) => self.kv_load_clients(session, cx),
            None => {}
        }
        cx.notify();
    }

    /// Fetch (or refresh) the connected-clients list (`CLIENT LIST`).
    pub(crate) fn kv_load_clients(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(mon) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_monitor_mut())
        else {
            return;
        };
        mon.clients_loading = true;
        let epoch = mon.epoch;
        self.service
            .send_to(session, Command::KvClientList { epoch });
        cx.notify();
    }

    /// Disconnect a client by id (`CLIENT KILL ID <id>`); the refreshed list
    /// comes back as a `KvClientListReady`.
    pub(crate) fn kv_kill_client(&mut self, session: SessionId, id: i64, cx: &mut Context<Self>) {
        let Some(mon) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_monitor_mut())
        else {
            return;
        };
        mon.clients_loading = true;
        let epoch = mon.epoch;
        self.service
            .send_to(session, Command::KvClientKill { epoch, id });
        cx.notify();
    }

    pub(crate) fn on_kv_client_list_ready(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        clients: Vec<ClientInfo>,
        cx: &mut Context<Self>,
    ) {
        let Some(mon) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.monitor_by_epoch_mut(epoch))
        else {
            return;
        };
        mon.clients = clients;
        mon.clients_loaded = true;
        mon.clients_loading = false;
        cx.notify();
    }

    /// Fetch (or refresh) the slow log.
    pub(crate) fn kv_load_slowlog(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(mon) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_monitor_mut())
        else {
            return;
        };
        mon.slowlog_loading = true;
        let epoch = mon.epoch;
        self.service.send_to(
            session,
            Command::KvSlowlog {
                epoch,
                count: SLOWLOG_COUNT,
            },
        );
        cx.notify();
    }

    /// Clear the slow log (`SLOWLOG RESET`); the empty reply refreshes the view.
    pub(crate) fn kv_reset_slowlog(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(mon) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_monitor_mut())
        else {
            return;
        };
        mon.slowlog_loading = true;
        let epoch = mon.epoch;
        self.service
            .send_to(session, Command::KvSlowlogReset { epoch });
        cx.notify();
    }

    pub(crate) fn on_kv_slowlog_ready(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        entries: Vec<SlowlogEntry>,
        cx: &mut Context<Self>,
    ) {
        let Some(mon) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.monitor_by_epoch_mut(epoch))
        else {
            return;
        };
        mon.slowlog = entries;
        mon.slowlog_loaded = true;
        mon.slowlog_loading = false;
        cx.notify();
    }

    /// Start the live `MONITOR` firehose.
    pub(crate) fn kv_start_monitor(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(mon) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_monitor_mut())
        else {
            return;
        };
        if mon.monitoring {
            return;
        }
        mon.monitoring = true;
        mon.paused = false;
        mon.lines.clear();
        let epoch = mon.epoch;
        self.service.send_to(session, Command::KvMonitor { epoch });
        cx.notify();
    }

    /// Freeze / unfreeze the live firehose view without tearing the stream
    /// down. Paused lines are dropped (a tail-style freeze), not buffered.
    pub(crate) fn kv_toggle_monitor_pause(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(mon) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_monitor_mut())
        {
            mon.paused = !mon.paused;
        }
        cx.notify();
    }

    /// Clear the accumulated MONITOR lines.
    pub(crate) fn kv_clear_monitor(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(mon) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_monitor_mut())
        {
            mon.lines.clear();
        }
        cx.notify();
    }

    /// Stop the live `MONITOR` (tears down the dedicated connection service-side
    /// via the generic epoch teardown).
    pub(crate) fn kv_stop_monitor(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(mon) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_monitor_mut())
        else {
            return;
        };
        if !mon.monitoring {
            return;
        }
        mon.monitoring = false;
        mon.paused = false;
        let epoch = mon.epoch;
        self.service
            .send_to(session, Command::CloseResult { epoch });
        cx.notify();
    }

    pub(crate) fn on_kv_monitor_line(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        line: String,
        cx: &mut Context<Self>,
    ) {
        let Some(mon) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.monitor_by_epoch_mut(epoch))
        else {
            return;
        };
        if !mon.monitoring || mon.paused {
            return; // already stopped, or the view is frozen
        }
        mon.lines.push(line);
        if mon.lines.len() > MAX_LINES {
            let drop = mon.lines.len() - MAX_LINES;
            mon.lines.drain(0..drop);
        }
        cx.notify();
    }

    pub(crate) fn render_kv_monitor(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let session = active.session;
        let writable = !active.config.read_only;
        let Some(mon) = active.kv_view.as_ref().and_then(|v| v.monitor_at(tab_idx)) else {
            return div().flex_1();
        };

        // The Slow-log / Live sub-toggle, mirroring the stream inspector's tabs.
        let tab = |label: &'static str, this_view: MonitorView| {
            let selected = mon.view == this_view;
            let tab_view = cx.entity().downgrade();
            div()
                .id(label)
                .px_2()
                .py_0p5()
                .cursor_pointer()
                .text_size(theme.scale(11.5))
                .text_color(if selected {
                    theme.text
                } else {
                    theme.text_muted
                })
                .border_b_2()
                .border_color(if selected {
                    theme.accent
                } else {
                    theme.border.opacity(0.)
                })
                .child(label)
                .on_click(move |_, _, cx| {
                    tab_view
                        .update(cx, |this, cx| {
                            this.kv_set_monitor_view(session, this_view, cx)
                        })
                        .ok();
                })
        };
        let tabs = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(tab("Slow log", MonitorView::Slowlog))
            .child(tab("Live (MONITOR)", MonitorView::Live))
            .child(tab("Clients", MonitorView::Clients));

        let body = match mon.view {
            MonitorView::Slowlog => self.render_slowlog(session, mon, writable, &theme, cx),
            MonitorView::Live => self.render_live_monitor(session, mon, &theme, cx),
            MonitorView::Clients => self.render_clients(session, mon, writable, &theme, cx),
        };

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(tabs)
            .child(body)
    }

    fn render_slowlog(
        &self,
        session: SessionId,
        mon: &KvMonitor,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (refresh_view, reset_view) = (cx.entity().downgrade(), cx.entity().downgrade());
        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(if mon.slowlog_loading {
                        "Loading slow log…".to_string()
                    } else {
                        format!("{} slow command(s)", mon.slowlog.len())
                    }),
            )
            .child(
                Button::new("kv-slowlog-refresh", "Refresh")
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        refresh_view
                            .update(cx, |this, cx| this.kv_load_slowlog(session, cx))
                            .ok();
                    }),
            )
            .when(writable, |d| {
                d.child(
                    Button::new("kv-slowlog-reset", "Reset")
                        .variant(ButtonVariant::Secondary)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            reset_view
                                .update(cx, |this, cx| this.kv_reset_slowlog(session, cx))
                                .ok();
                        }),
                )
            });

        let mono = theme.mono_family.clone();
        let now = unix_now();
        let entries = Rc::new(mon.slowlog.clone());
        let rows: Vec<_> = entries
            .iter()
            .map(|e| {
                let dur = fmt_micros(e.micros);
                // Tint the duration by rough severity so the eye lands on the
                // worst offenders: >100ms red, >10ms yellow, else muted.
                let dur_color = if e.micros >= 100_000 {
                    theme.red
                } else if e.micros >= 10_000 {
                    theme.yellow
                } else {
                    theme.text_muted
                };
                let client = if e.client.is_empty() {
                    String::new()
                } else {
                    format!("  {}", e.client)
                };
                div()
                    .flex()
                    .flex_col()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border.opacity(0.5))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(72.))
                                    .flex_shrink_0()
                                    .text_size(theme.scale(11.))
                                    .text_color(dur_color)
                                    .child(dur),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(mono.clone())
                                    .text_size(theme.scale(11.))
                                    .child(fmt_argv(&e.argv)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(theme.scale(9.5))
                            .text_color(theme.text_muted)
                            .child(format!("{}{client}", fmt_ago(now, e.time_secs))),
                    )
                    .into_any_element()
            })
            .collect();

        let list = if mon.slowlog.is_empty() && !mon.slowlog_loading {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_2()
                .text_size(theme.scale(11.))
                .text_color(theme.text_muted)
                .child("No slow commands logged.")
                .into_any_element()
        } else {
            div()
                .id("kv-slowlog-list")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .children(rows)
                .into_any_element()
        };

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(header)
            .child(list)
            .into_any_element()
    }

    fn render_live_monitor(
        &self,
        session: SessionId,
        mon: &KvMonitor,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let toggle_view = cx.entity().downgrade();
        let toggle = if mon.monitoring {
            Button::new("kv-monitor-toggle", "Stop")
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Sm)
                .on_click(move |_, _, cx| {
                    toggle_view
                        .update(cx, |this, cx| this.kv_stop_monitor(session, cx))
                        .ok();
                })
        } else {
            Button::new("kv-monitor-toggle", "Start")
                .size(ButtonSize::Sm)
                .on_click(move |_, _, cx| {
                    toggle_view
                        .update(cx, |this, cx| this.kv_start_monitor(session, cx))
                        .ok();
                })
        };

        let status = if mon.monitoring {
            if mon.paused {
                format!("paused — {} line(s) held", mon.lines.len())
            } else {
                format!("streaming every command — {} line(s)", mon.lines.len())
            }
        } else {
            "stopped (MONITOR streams every command the server runs)".to_string()
        };

        let (pause_view, clear_view) = (cx.entity().downgrade(), cx.entity().downgrade());
        let pause_button = mon.monitoring.then(|| {
            Button::new(
                "kv-monitor-pause",
                if mon.paused { "Resume" } else { "Pause" },
            )
            .variant(ButtonVariant::Secondary)
            .size(ButtonSize::Sm)
            .on_click(move |_, _, cx| {
                pause_view
                    .update(cx, |this, cx| this.kv_toggle_monitor_pause(session, cx))
                    .ok();
            })
        });
        let clear_button = (!mon.lines.is_empty()).then(|| {
            Button::new("kv-monitor-clear", "Clear")
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Sm)
                .on_click(move |_, _, cx| {
                    clear_view
                        .update(cx, |this, cx| this.kv_clear_monitor(session, cx))
                        .ok();
                })
        });

        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(status),
            )
            .children(clear_button)
            .children(pause_button)
            .child(toggle);

        let mono = theme.mono_family.clone();
        // Iterate the resident buffer directly: cloning the whole thing (up to
        // MAX_LINES) every frame just to read the newest 1_000 was pure waste —
        // the clone was never captured in a closure.
        let items: Vec<_> = mon
            .lines
            .iter()
            .rev()
            .take(1_000)
            .map(|line| {
                div()
                    .px_2()
                    .py_0p5()
                    .font_family(mono.clone())
                    .text_size(theme.scale(11.))
                    .min_w_0()
                    .truncate()
                    .child(line.clone())
                    .into_any_element()
            })
            .collect();

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id("kv-monitor-lines")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(items),
            )
            .into_any_element()
    }

    fn render_clients(
        &self,
        session: SessionId,
        mon: &KvMonitor,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let refresh_view = cx.entity().downgrade();
        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(if mon.clients_loading {
                        "Loading clients…".to_string()
                    } else {
                        format!("{} connected client(s)", mon.clients.len())
                    }),
            )
            .child(
                Button::new("kv-clients-refresh", "Refresh")
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        refresh_view
                            .update(cx, |this, cx| this.kv_load_clients(session, cx))
                            .ok();
                    }),
            );

        let mono = theme.mono_family.clone();
        let rows: Vec<_> = mon
            .clients
            .iter()
            .map(|c| {
                let kill_view = cx.entity().downgrade();
                let id = c.id;
                // `name` is often empty; show a dim placeholder so the row
                // doesn't look truncated.
                let name = if c.name.is_empty() {
                    "—".to_string()
                } else {
                    c.name.clone()
                };
                let resp = if c.resp.is_empty() {
                    String::new()
                } else {
                    format!(" · RESP{}", c.resp)
                };
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border.opacity(0.5))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .font_family(mono.clone())
                                            .text_size(theme.scale(11.))
                                            .child(format!("#{} {}", c.id, c.addr)),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(theme.scale(10.5))
                                            .text_color(theme.text_muted)
                                            .child(name),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(theme.scale(9.5))
                                    .text_color(theme.text_muted)
                                    .child(format!(
                                        "db {} · age {} · idle {} · cmd {}{resp}",
                                        c.db,
                                        fmt_secs(c.age),
                                        fmt_secs(c.idle),
                                        c.cmd,
                                    )),
                            ),
                    )
                    .when(writable, |d| {
                        d.child(
                            Button::new(SharedString::from(format!("kv-client-kill-{id}")), "Kill")
                                .variant(ButtonVariant::Secondary)
                                .size(ButtonSize::Sm)
                                .on_click(move |_, _, cx| {
                                    kill_view
                                        .update(cx, |this, cx| this.kv_kill_client(session, id, cx))
                                        .ok();
                                }),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        let list = if mon.clients.is_empty() && !mon.clients_loading {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_2()
                .text_size(theme.scale(11.))
                .text_color(theme.text_muted)
                .child("No connected clients.")
                .into_any_element()
        } else {
            div()
                .id("kv-clients-list")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .children(rows)
                .into_any_element()
        };

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(header)
            .child(list)
            .into_any_element()
    }
}

/// Seconds since the Unix epoch on the local clock, for the slow log's
/// "N ago" column. Best-effort: a clock before the epoch (never in practice)
/// reads as 0.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A coarse "N ago" for a slow-log entry's timestamp (server clock vs. local,
/// roughly aligned). `just now` under a second, else `Ns/Nm/Nh/Nd ago`.
fn fmt_ago(now: i64, then: i64) -> String {
    let d = (now - then).max(0);
    if d < 1 {
        "just now".to_string()
    } else if d < 60 {
        format!("{d}s ago")
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

/// A compact human duration from whole seconds (`CLIENT LIST` age/idle):
/// `"0s"`, `"45s"`, `"3m"`, `"2h"`, `"1d"`.
fn fmt_secs(s: u64) -> String {
    if s < 60 {
        format!("{s}s")
    } else if s < 3_600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3_600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// A slow command's execution time (`SLOWLOG` micros) in a compact human form:
/// `"820µs"`, `"12.4ms"`, `"1.30s"`.
fn fmt_micros(us: u64) -> String {
    if us < 1_000 {
        format!("{us}µs")
    } else if us < 1_000_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{:.2}s", us as f64 / 1_000_000.0)
    }
}

/// Join a slow-command's argv for display, quoting any argument that contains
/// whitespace so the command reads unambiguously.
fn fmt_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.chars().any(char::is_whitespace) {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
