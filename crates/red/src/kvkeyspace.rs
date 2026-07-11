//! The Redis keyspace-notification watcher (see docs/plans/redis.md's
//! "keyspace-notification live tooling" gap). A live view of key-level events
//! (`set`/`del`/`expired`/`lpush`/…) as they happen, built on the same
//! `PSUBSCRIBE`→`Event::KvMessage` path as the Pub/Sub monitor — just a canned
//! `__keyevent@*__`/`__keyspace@*__` channel pattern and a decode.
//!
//! Keyspace notifications are off by default (`notify-keyspace-events` empty),
//! so the panel surfaces the current setting and, on a writable connection,
//! offers to enable them before nothing would otherwise arrive.

use std::rc::Rc;

use flint::prelude::*;
use gpui::{div, prelude::*, px, Context, Window};
use red_core::kv::{KeyspaceEvent, KeyspaceScope};
use red_service::{Command, SessionId};

use crate::app::{ActiveConn, AppState};

/// Oldest-evicted cap on resident events, mirroring the Pub/Sub monitor's
/// `MAX_MESSAGES`: a busy keyspace is a firehose.
const MAX_EVENTS: usize = 2_000;
/// The flag set used when enabling notifications from the panel: `K` (keyspace),
/// `E` (keyevent), `A` (all event classes) — "everything", the most useful
/// default for a live watcher (matches `redis-cli config set` guidance).
const ENABLE_FLAGS: &str = "KEA";

pub(crate) struct KvKeyspace {
    /// A dedicated subscription epoch (distinct from the Pub/Sub monitor's), so
    /// both can watch at once and each tears down independently via
    /// `CloseResult`.
    pub(crate) epoch: u64,
    pub(crate) scope: KeyspaceScope,
    /// `true` while a subscription is live (Start pressed, not yet Stopped).
    pub(crate) watching: bool,
    pub(crate) events: Vec<KeyspaceEvent>,
    /// The server's `notify-keyspace-events` value; `None` until first fetched.
    /// Empty string means notifications are disabled.
    pub(crate) notify_config: Option<String>,
    /// Set once the config has been fetched at least once (lazy on first open).
    pub(crate) config_loaded: bool,
}

impl KvKeyspace {
    pub(crate) fn new() -> Self {
        Self {
            epoch: crate::result::next_kv_epoch(),
            scope: KeyspaceScope::ByEvent,
            watching: false,
            events: Vec::new(),
            notify_config: None,
            config_loaded: false,
        }
    }

    /// Whether notifications are currently enabled server-side (a non-empty
    /// `notify-keyspace-events`). `false` while the config is still unknown.
    fn enabled(&self) -> bool {
        self.notify_config.as_deref().is_some_and(|v| !v.is_empty())
    }
}

impl AppState {
    /// Fetch the current `notify-keyspace-events` setting (lazy, on first open).
    pub(crate) fn kv_keyspace_load_config(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(ks) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_keyspace_mut())
        else {
            return;
        };
        let epoch = ks.epoch;
        self.service
            .send_to(session, Command::KvNotifyConfig { epoch });
        cx.notify();
    }

    /// Enable keyspace notifications server-side (`CONFIG SET
    /// notify-keyspace-events KEA`). Writable connections only; the fresh value
    /// comes back as a `KvNotifyConfigReady`.
    pub(crate) fn kv_keyspace_enable(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(ks) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_keyspace_mut())
        else {
            return;
        };
        let epoch = ks.epoch;
        self.service.send_to(
            session,
            Command::KvSetNotifyConfig {
                epoch,
                flags: ENABLE_FLAGS.to_string(),
            },
        );
        cx.notify();
    }

    /// Switch between the by-event and by-key channel families; re-subscribes
    /// live if already watching.
    pub(crate) fn kv_keyspace_set_scope(
        &mut self,
        session: SessionId,
        scope: KeyspaceScope,
        cx: &mut Context<Self>,
    ) {
        let restart = {
            let Some(ks) = self
                .conn_mut(Some(session))
                .and_then(|a| a.kv_view.as_mut())
                .and_then(|v| v.active_keyspace_mut())
            else {
                return;
            };
            if ks.scope == scope {
                return;
            }
            ks.scope = scope;
            ks.watching
        };
        if restart {
            self.kv_keyspace_stop(session, cx);
            self.kv_keyspace_start(session, cx);
        }
        cx.notify();
    }

    /// Start watching: `PSUBSCRIBE` to the current scope's channel pattern.
    pub(crate) fn kv_keyspace_start(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(ks) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_keyspace_mut())
        else {
            return;
        };
        if ks.watching {
            return;
        }
        ks.watching = true;
        ks.events.clear();
        let epoch = ks.epoch;
        let pattern = ks.scope.pattern().to_string();
        self.service
            .send_to(session, Command::KvSubscribe { epoch, pattern });
        cx.notify();
    }

    /// Stop watching (tears down the subscription connection service-side).
    pub(crate) fn kv_keyspace_stop(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(ks) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_keyspace_mut())
        else {
            return;
        };
        if !ks.watching {
            return;
        }
        ks.watching = false;
        let epoch = ks.epoch;
        self.service
            .send_to(session, Command::CloseResult { epoch });
        cx.notify();
    }

    pub(crate) fn on_kv_notify_config_ready(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        value: String,
        cx: &mut Context<Self>,
    ) {
        let Some(ks) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.keyspace_by_epoch_mut(epoch))
        else {
            return;
        };
        ks.notify_config = Some(value);
        ks.config_loaded = true;
        cx.notify();
    }

    /// A `KvMessage` whose epoch matches the keyspace watcher (routed here from
    /// `on_kv_message` when it isn't a Pub/Sub-monitor message). Decodes the
    /// notification channel and appends it, capped.
    pub(crate) fn on_kv_keyspace_message(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        channel: String,
        payload: String,
        cx: &mut Context<Self>,
    ) {
        let Some(ks) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.keyspace_by_epoch_mut(epoch))
        else {
            return;
        };
        if !ks.watching {
            return;
        }
        let Some(ev) = red_core::kv::parse_keyspace_channel(&channel, &payload) else {
            return; // not a keyspace notification (shouldn't happen for this pattern)
        };
        ks.events.push(ev);
        if ks.events.len() > MAX_EVENTS {
            let drop = ks.events.len() - MAX_EVENTS;
            ks.events.drain(0..drop);
        }
        cx.notify();
    }

    pub(crate) fn render_kv_keyspace(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let session = active.session;
        let writable = !active.config.read_only;
        let Some(ks) = active.kv_view.as_ref().and_then(|v| v.keyspace_at(tab_idx)) else {
            return div().flex_1();
        };

        // Scope toggle (By event | By key), mirroring the diagnostics tabs.
        let tab = |label: &'static str, this_scope: KeyspaceScope| {
            let selected = ks.scope == this_scope;
            let tab_view = cx.entity().downgrade();
            div()
                .id(label)
                .px_2()
                .py_0p5()
                .cursor_pointer()
                .text_size(theme.scale(11.))
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
                            this.kv_keyspace_set_scope(session, this_scope, cx)
                        })
                        .ok();
                })
        };

        let toggle_view = cx.entity().downgrade();
        let watch_button = if ks.watching {
            Button::new("kv-keyspace-toggle", "Stop")
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Sm)
                .on_click(move |_, _, cx| {
                    toggle_view
                        .update(cx, |this, cx| this.kv_keyspace_stop(session, cx))
                        .ok();
                })
        } else {
            Button::new("kv-keyspace-toggle", "Watch")
                .size(ButtonSize::Sm)
                .on_click(move |_, _, cx| {
                    toggle_view
                        .update(cx, |this, cx| this.kv_keyspace_start(session, cx))
                        .ok();
                })
        };

        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(tab("By event", KeyspaceScope::ByEvent))
            .child(tab("By key", KeyspaceScope::ByKey))
            .child(div().flex_1())
            .child(watch_button);

        // When notifications are known to be off, a banner explains why nothing
        // arrives, with an Enable action on a writable connection.
        let disabled_banner = (ks.config_loaded && !ks.enabled()).then(|| {
            let enable_view = cx.entity().downgrade();
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1p5()
                .bg(theme.yellow.opacity(0.1))
                .border_b_1()
                .border_color(theme.yellow.opacity(0.5))
                .child(
                    div()
                        .flex_1()
                        .text_size(theme.scale(10.5))
                        .text_color(theme.text)
                        .child(if writable {
                            "Keyspace notifications are disabled — enable them to see events."
                        } else {
                            "Keyspace notifications are disabled (read-only: enable them server-side with CONFIG SET notify-keyspace-events)."
                        }),
                )
                .when(writable, |d| {
                    d.child(
                        Button::new("kv-keyspace-enable", "Enable")
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                enable_view
                                    .update(cx, |this, cx| this.kv_keyspace_enable(session, cx))
                                    .ok();
                            }),
                    )
                })
                .into_any_element()
        });

        let status = {
            let cfg = ks
                .notify_config
                .as_deref()
                .filter(|v| !v.is_empty())
                .map(|v| format!("flags \"{v}\""))
                .unwrap_or_else(|| "disabled".to_string());
            if ks.watching {
                format!(
                    "watching {} — {} event(s), {cfg}",
                    ks.scope.pattern(),
                    ks.events.len()
                )
            } else {
                format!("not watching — {cfg}")
            }
        };

        let mono = theme.mono_family.clone();
        let events = Rc::new(ks.events.clone());
        let items: Vec<_> = events
            .iter()
            .rev()
            .take(1_000)
            .map(|e| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .font_family(mono.clone())
                    .text_size(theme.scale(11.))
                    .child(
                        div()
                            .w(px(28.))
                            .flex_shrink_0()
                            .text_size(theme.scale(9.5))
                            .text_color(theme.text_muted)
                            .child(format!("db{}", e.db)),
                    )
                    .child(
                        div()
                            .w(px(110.))
                            .flex_shrink_0()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.accent)
                            .child(e.event.clone()),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(e.key.clone()))
                    .into_any_element()
            })
            .collect();

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(header)
            .children(disabled_banner)
            .child(
                div()
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(status),
            )
            .child(
                div()
                    .id("kv-keyspace-events")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(items),
            )
    }
}
