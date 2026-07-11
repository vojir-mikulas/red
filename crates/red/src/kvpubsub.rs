//! The Redis Pub/Sub live monitor (see docs/plans/redis.md's live phase).
//! One pattern subscription at a time; messages stream in via
//! `Event::KvMessage` for as long as the subscription is open, capped so a
//! chatty channel can't grow the log forever.

use std::rc::Rc;

use flint::prelude::*;
use gpui::{div, prelude::*, px, Context, Entity, Window};
use red_core::kv::KvMessage;
use red_service::{Command, SessionId};

use crate::app::{ActiveConn, AppState};

/// Oldest-evicted cap on resident messages, mirroring the keyspace browser's
/// `MAX_RESIDENT_ROWS` idea: a live monitor left open for a while on a busy
/// channel shouldn't grow its log without bound.
const MAX_MESSAGES: usize = 2_000;

pub(crate) struct KvPubSub {
    pub(crate) epoch: u64,
    pub(crate) pattern_input: Entity<TextInput>,
    /// `Some(pattern)` once `KvSubscribe` has been sent for it; cleared on
    /// unsubscribe. Distinct from "has this pattern ever received a message"
    /// so the panel can show "listening on foo*, no messages yet".
    pub(crate) subscribed: Option<String>,
    /// Received messages, each stamped with the local Unix second it arrived
    /// (for the relative "N ago" column — Pub/Sub messages carry no time of
    /// their own).
    pub(crate) messages: Vec<(i64, KvMessage)>,
}

impl KvPubSub {
    pub(crate) fn new(cx: &mut Context<AppState>) -> Self {
        let pattern_input =
            cx.new(|cx| TextInput::new(cx).with_placeholder("Channel pattern, e.g. news.*"));
        Self {
            epoch: crate::result::next_kv_epoch(),
            pattern_input,
            subscribed: None,
            messages: Vec::new(),
        }
    }
}

impl AppState {
    pub(crate) fn kv_subscribe(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(pubsub) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_pubsub_mut())
        else {
            return;
        };
        let pattern = pubsub.pattern_input.read(cx).content().to_string();
        if pattern.is_empty() {
            return;
        }
        pubsub.subscribed = Some(pattern.clone());
        pubsub.messages.clear();
        let epoch = pubsub.epoch;
        self.service
            .send_to(session, Command::KvSubscribe { epoch, pattern });
        cx.notify();
    }

    pub(crate) fn kv_unsubscribe(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(pubsub) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_pubsub_mut())
        else {
            return;
        };
        if pubsub.subscribed.take().is_none() {
            return;
        }
        let epoch = pubsub.epoch;
        self.service
            .send_to(session, Command::CloseResult { epoch });
        cx.notify();
    }

    pub(crate) fn on_kv_message(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        channel: String,
        payload: String,
        cx: &mut Context<Self>,
    ) {
        // Route by epoch across tabs: a message can target a background
        // Pub/Sub tab. If no Pub/Sub tab owns this epoch, it's a keyspace
        // watcher message (that rides the same `KvMessage` path on its own
        // epoch) — hand it off.
        let is_pubsub = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.pubsub_by_epoch_mut(epoch))
            .is_some_and(|p| p.subscribed.is_some());
        if !is_pubsub {
            self.on_kv_keyspace_message(session, epoch, channel, payload, cx);
            return;
        }
        let Some(pubsub) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.pubsub_by_epoch_mut(epoch))
        else {
            return;
        };
        pubsub
            .messages
            .push((now_unix(), KvMessage { channel, payload }));
        if pubsub.messages.len() > MAX_MESSAGES {
            let drop = pubsub.messages.len() - MAX_MESSAGES;
            pubsub.messages.drain(0..drop);
        }
        cx.notify();
    }

    /// Clear the received-message log without unsubscribing.
    pub(crate) fn kv_clear_pubsub(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(pubsub) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_pubsub_mut())
        {
            pubsub.messages.clear();
        }
        cx.notify();
    }

    pub(crate) fn render_kv_pubsub(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let session = active.session;
        let view = cx.entity().downgrade();
        let Some(pubsub) = active.kv_view.as_ref().and_then(|v| v.pubsub_at(tab_idx)) else {
            return div().flex_1();
        };

        let subscribed = pubsub.subscribed.is_some();
        let toggle = if subscribed {
            Button::new("kv-pubsub-toggle", "Unsubscribe")
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Sm)
                .on_click(move |_, _, cx| {
                    view.update(cx, |this, cx| this.kv_unsubscribe(session, cx))
                        .ok();
                })
        } else {
            Button::new("kv-pubsub-toggle", "Subscribe")
                .size(ButtonSize::Sm)
                .on_click(move |_, _, cx| {
                    view.update(cx, |this, cx| this.kv_subscribe(session, cx))
                        .ok();
                })
        };

        let clear_view = cx.entity().downgrade();
        let clear_button = (!pubsub.messages.is_empty()).then(|| {
            Button::new("kv-pubsub-clear", "Clear")
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Sm)
                .on_click(move |_, _, cx| {
                    clear_view
                        .update(cx, |this, cx| this.kv_clear_pubsub(session, cx))
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
            .child(div().flex_1().child(pubsub.pattern_input.clone()))
            .children(clear_button)
            .child(toggle);

        let status = match &pubsub.subscribed {
            Some(pattern) => format!(
                "listening on \"{pattern}\", {} message(s)",
                pubsub.messages.len()
            ),
            None => "not subscribed".to_string(),
        };

        let mono = theme.mono_family.clone();
        let text_size = theme.scale(11.5);
        let now = now_unix();
        let messages = Rc::new(pubsub.messages.clone());
        let items: Vec<_> = messages
            .iter()
            .rev()
            .take(500)
            .map(|(at, m)| {
                div()
                    .flex()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .font_family(mono.clone())
                    .text_size(text_size)
                    .child(
                        div()
                            .w(px(52.))
                            .flex_shrink_0()
                            .text_size(theme.scale(10.))
                            .text_color(theme.text_faint)
                            .child(fmt_ago(now, *at)),
                    )
                    .child(
                        div()
                            .w(px(160.))
                            .min_w_0()
                            .truncate()
                            .text_color(theme.blue)
                            .child(m.channel.clone()),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(m.payload.clone()))
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
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_muted)
                    .child(status),
            )
            .child(
                div()
                    .id("kv-pubsub-messages")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(items),
            )
    }
}

/// Local Unix seconds now (for stamping message arrival).
fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A compact, timezone-free "N ago" for a message's arrival: `now`, `Ns`, `Nm`,
/// `Nh`. Timezone-free (relative) so it reads right regardless of the user's
/// clock offset.
fn fmt_ago(now: i64, then: i64) -> String {
    let d = (now - then).max(0);
    if d < 1 {
        "now".to_string()
    } else if d < 60 {
        format!("{d}s")
    } else if d < 3_600 {
        format!("{}m", d / 60)
    } else {
        format!("{}h", d / 3_600)
    }
}
