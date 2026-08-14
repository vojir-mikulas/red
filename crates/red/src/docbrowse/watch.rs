//! The change-stream viewer: a collection's writes as they happen.
//!
//! The panel shape is the Redis keyspace-notification panel's (a live feed with a
//! pause and a per-kind filter), and the reasoning behind the caps is the same: a
//! feed that grows without bound stops being a viewer and becomes a memory leak,
//! so the buffer is a ring.
//!
//! Two things a change stream has that a keyspace notification does not, and both
//! are surfaced deliberately: it needs a replica set (a standalone keeps no oplog),
//! and it carries a **resume token** naming the exact point a consumer would
//! restart from. The token is what makes this more than a tail.

use std::collections::VecDeque;

use flint::prelude::*;
use gpui::{Context, div, prelude::*, px};
use red_core::doc::{DocChange, DocChangeOp};
use red_service::{Command, Epoch, SessionId};

use crate::app::AppState;

/// Changes the viewer keeps. Beyond this the oldest are dropped: a live feed is
/// read from the top, and holding a session's worth of writes would be a leak
/// wearing a feature's clothes.
const WATCH_BUFFER: usize = 500;

/// The Watch panel's state, per collection tab.
#[derive(Default)]
pub(crate) struct DocWatchState {
    /// Whether a stream is running on the backend.
    pub(crate) running: bool,
    /// Whether new changes are being appended. Pausing keeps the stream open (so
    /// no change is missed) and stops the list moving under the reader.
    pub(crate) paused: bool,
    /// The changes seen, newest last. Bounded by [`WATCH_BUFFER`].
    pub(crate) changes: VecDeque<DocChange>,
    /// Changes that arrived while paused, so the resume button can say how many.
    pub(crate) held: usize,
    /// Which operations are shown; empty means all of them.
    pub(crate) filter: Vec<DocChangeOp>,
    /// The most recent resume token, for the footer.
    pub(crate) resume_token: Option<String>,
    /// Why the stream stopped, when it stopped on its own.
    pub(crate) ended: Option<String>,
    /// Total changes seen, including those the ring has since dropped.
    pub(crate) seen: u64,
}

impl DocWatchState {
    /// Whether `change` passes the viewer's operation filter.
    fn shows(&self, change: &DocChange) -> bool {
        self.filter.is_empty() || self.filter.contains(&change.op)
    }
}

impl AppState {
    /// Start (or restart) the change stream for the focused collection.
    pub(crate) fn doc_watch_start(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        let (epoch, db, coll) = (current.epoch, current.db.clone(), current.coll.clone());
        // Resume from the last token when there is one: reopening the panel after
        // a stop should not lose the writes that happened in between.
        let resume_after = current.watch.resume_token.clone();
        current.watch.running = true;
        current.watch.paused = false;
        current.watch.ended = None;
        self.service.send_to(
            session,
            Command::DocWatch {
                epoch,
                db,
                coll,
                resume_after,
            },
        );
        cx.notify();
    }

    /// Stop the stream. The buffer stays: what it already showed is still worth
    /// reading.
    pub(crate) fn doc_watch_stop(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        let epoch = current.epoch;
        current.watch.running = false;
        self.service
            .send_to(session, Command::DocWatchStop { epoch });
        cx.notify();
    }

    /// Pause or resume appending. The stream keeps running either way, so nothing
    /// is missed while the list is held still.
    pub(crate) fn doc_watch_toggle_pause(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(current) = self.doc_focused_coll_mut(session) {
            current.watch.paused = !current.watch.paused;
            if !current.watch.paused {
                current.watch.held = 0;
            }
        }
        cx.notify();
    }

    /// Toggle one operation in the viewer's filter.
    pub(crate) fn doc_watch_toggle_op(
        &mut self,
        session: SessionId,
        op: DocChangeOp,
        cx: &mut Context<Self>,
    ) {
        if let Some(current) = self.doc_focused_coll_mut(session) {
            match current.watch.filter.iter().position(|o| *o == op) {
                Some(i) => {
                    current.watch.filter.remove(i);
                }
                None => current.watch.filter.push(op),
            }
        }
        cx.notify();
    }

    pub(crate) fn doc_watch_clear(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(current) = self.doc_focused_coll_mut(session) {
            current.watch.changes.clear();
            current.watch.held = 0;
        }
        cx.notify();
    }

    /// `DocChanged`: append one change to the tab that is watching.
    pub(crate) fn on_doc_changed(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        change: DocChange,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        let Some(current) = view.coll_by_epoch_mut(epoch) else {
            return;
        };
        current.watch.seen += 1;
        // The token is recorded even while paused and even for a filtered-out
        // change: it marks the stream's position, not the viewer's.
        if let Some(token) = &change.resume_token {
            current.watch.resume_token = Some(token.clone());
        }
        if current.watch.paused {
            current.watch.held += 1;
            cx.notify();
            return;
        }
        if !current.watch.shows(&change) {
            return;
        }
        current.watch.changes.push_back(change);
        while current.watch.changes.len() > WATCH_BUFFER {
            current.watch.changes.pop_front();
        }
        cx.notify();
    }

    /// `DocWatchEnded`: the stream closed; stop looking live.
    pub(crate) fn on_doc_watch_ended(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.conn_mut(session).and_then(|a| a.doc_view.as_mut()) else {
            return;
        };
        if let Some(current) = view.coll_by_epoch_mut(epoch) {
            current.watch.running = false;
            current.watch.ended =
                Some(message.unwrap_or_else(|| "the change stream closed".to_string()));
        }
        cx.notify();
    }

    /// The Watch panel: a toolbar (start/stop, pause, filters, clear) over the
    /// feed, with the resume token in the footer.
    pub(crate) fn render_doc_watch(
        &self,
        session: SessionId,
        current: &super::CollView,
        theme: &Theme,
        view: &gpui::WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        let watch = &current.watch;
        let (run_view, pause_view, clear_view) = (view.clone(), view.clone(), view.clone());
        let running = watch.running;
        let mut toolbar = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("doc-watch-run", if running { "Stop" } else { "Watch" })
                    .size(ButtonSize::Sm)
                    .variant(if running {
                        ButtonVariant::Secondary
                    } else {
                        ButtonVariant::Primary
                    })
                    .on_click(move |_, _, cx| {
                        run_view
                            .update(cx, |this, cx| {
                                if running {
                                    this.doc_watch_stop(session, cx);
                                } else {
                                    this.doc_watch_start(session, cx);
                                }
                            })
                            .ok();
                    }),
            )
            .child(
                Button::new(
                    "doc-watch-pause",
                    if watch.paused {
                        if watch.held == 0 {
                            "Resume".to_string()
                        } else {
                            format!("Resume ({} held)", watch.held)
                        }
                    } else {
                        "Pause".to_string()
                    },
                )
                .size(ButtonSize::Sm)
                .variant(ButtonVariant::Ghost)
                .disabled(!running)
                .on_click(move |_, _, cx| {
                    pause_view
                        .update(cx, |this, cx| this.doc_watch_toggle_pause(session, cx))
                        .ok();
                }),
            );

        for op in DocChangeOp::FILTERABLE {
            let on = watch.filter.is_empty() || watch.filter.contains(&op);
            let op_view = view.clone();
            toolbar = toolbar.child(
                div()
                    .id(gpui::SharedString::from(format!(
                        "doc-watch-op-{}",
                        op.label()
                    )))
                    .px(px(6.))
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(if on { theme.accent } else { theme.border })
                    .text_size(theme.scale(11.))
                    .text_color(if on { theme.text } else { theme.text_faint })
                    .cursor_pointer()
                    .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                        op_view
                            .update(cx, |this, cx| this.doc_watch_toggle_op(session, op, cx))
                            .ok();
                    })
                    .child(op.label()),
            );
        }
        toolbar = toolbar.child(div().flex_1()).child(
            Button::new("doc-watch-clear", "Clear")
                .size(ButtonSize::Sm)
                .variant(ButtonVariant::Ghost)
                .on_click(move |_, _, cx| {
                    clear_view
                        .update(cx, |this, cx| this.doc_watch_clear(session, cx))
                        .ok();
                }),
        );

        let body = if watch.changes.is_empty() {
            super::render::doc_hint(
                if running {
                    "Watching. Writes to this collection will appear here."
                } else {
                    "Not watching. Press Watch to follow this collection's writes live."
                },
                theme,
            )
        } else {
            let rows = watch
                .changes
                .iter()
                .rev()
                .map(|c| render_change(c, theme))
                .collect::<Vec<_>>();
            div()
                .id("doc-watch-feed")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .children(rows)
                .into_any_element()
        };

        let ended = watch.ended.as_ref().map(|why| {
            div()
                .px_3()
                .py_1()
                .flex_shrink_0()
                .text_size(theme.scale(11.))
                .text_color(theme.yellow)
                .child(why.clone())
        });
        let footer = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_3()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .text_size(theme.scale(11.))
            .text_color(theme.text_faint)
            .child(format!("{} change(s) seen", watch.seen))
            .children(watch.resume_token.as_ref().map(|token| {
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .child(format!("resume token: {token}"))
            }));

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(toolbar)
            .children(ended)
            .child(body)
            .child(footer)
            .into_any_element()
    }
}

/// One change as a row: what happened, to which document, and what it touched.
fn render_change(change: &DocChange, theme: &Theme) -> gpui::AnyElement {
    let color = match change.op {
        DocChangeOp::Insert => theme.green,
        DocChangeOp::Delete | DocChangeOp::Drop | DocChangeOp::Invalidate => theme.red,
        DocChangeOp::Update | DocChangeOp::Replace => theme.accent,
        _ => theme.text_muted,
    };
    let id = change
        .id
        .as_ref()
        .map(|v| v.to_extended_json())
        .unwrap_or_default();
    let mut detail = Vec::new();
    if !change.updated.is_empty() {
        detail.push(format!("set {}", change.updated.join(", ")));
    }
    if !change.removed.is_empty() {
        detail.push(format!("unset {}", change.removed.join(", ")));
    }
    // A full document is worth more than a field list when there is one, but it
    // is also the long thing, so it trails.
    if detail.is_empty()
        && let Some(full) = &change.full
    {
        detail.push(full.to_doc_value().to_extended_json());
    }

    div()
        .flex()
        .items_start()
        .gap_2()
        .px_3()
        .py(px(3.))
        .text_size(theme.scale(11.))
        .child(
            div()
                .w(px(64.))
                .flex_shrink_0()
                .text_color(color)
                .child(change.op.label()),
        )
        .child(
            div()
                .w(px(220.))
                .flex_shrink_0()
                .truncate()
                .text_color(theme.text)
                .child(id),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_color(theme.text_muted)
                .child(detail.join("  \u{b7}  ")),
        )
        .into_any_element()
}
