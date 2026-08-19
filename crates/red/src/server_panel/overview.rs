//! The Overview view: one sample of the server's live state, grouped.
//!
//! Reads a [`ServerSnapshot`] and nothing else, so it renders a Postgres, a
//! Redis and a MongoDB identically and gains a fourth engine for free. Three
//! rules carry it:
//!
//! - **A bounded value gets a bar, and the bar carries the colour.** "1.9 GiB of
//!   2 GiB" is a sentence you have to read; a bar at 95% in red is not.
//! - **A cumulative total shows the rate it is moving at**, derived from the
//!   previous sample. A raw `total_commands_processed` is a number nobody can
//!   act on; "1,204/s since the last refresh" is the actual answer.
//! - **What this role could not see is shown, not dropped.** A panel that omits
//!   a metric reads as zero, and "no replicas" and "you may not ask about
//!   replicas" are opposite conclusions.
//!
//! No history, no sparklines: the panel shows now, plus one derived rate. A
//! metrics database inside a database explorer is a different product.

use flint::prelude::*;
use gpui::{Context, Hsla, div, prelude::*, px};
use red_core::server::{MetricGroup, MetricValue, ServerMetric, ServerSnapshot};

use crate::app::{ActiveConn, AppState};

/// Where a bar stops being informational and starts being a warning, then a
/// problem. A connection pool at 80% is worth noticing; at 95% the next client
/// is refused.
const WARN_AT: f32 = 0.8;
const BAD_AT: f32 = 0.95;

impl AppState {
    pub(super) fn render_overview(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // Read once for the frame: holding this across the listeners below would
        // freeze the context they need.
        let panel = active.server.read(cx);
        let theme = cx.theme().clone();
        let size_11 = theme.scale(11.);

        let Some(snap) = &panel.metrics else {
            let message = match (&panel.metrics_error, panel.metrics_loading) {
                (Some(e), _) => e.clone(),
                (None, true) => "sampling…".to_string(),
                (None, false) => "No sample yet.".to_string(),
            };
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_3()
                .text_size(size_11)
                .text_color(if panel.metrics_error.is_some() {
                    theme.red
                } else {
                    theme.text_muted
                })
                .child(message)
                .into_any_element();
        };

        let groups: Vec<gpui::AnyElement> = MetricGroup::ORDER
            .into_iter()
            .filter_map(|group| {
                let metrics: Vec<&ServerMetric> = snap.group(group).collect();
                if metrics.is_empty() {
                    return None;
                }
                let rows: Vec<gpui::AnyElement> = metrics
                    .into_iter()
                    .map(|m| self.render_metric(m, snap, panel.metrics_prev.as_ref(), cx))
                    .collect();
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .px_3()
                                .pt_2()
                                .pb_1()
                                .text_size(theme.scale(10.))
                                .text_color(theme.text_faint)
                                .child(group.heading().to_uppercase()),
                        )
                        .children(rows)
                        .into_any_element(),
                )
            })
            .collect();

        // A stale sample must not read as a live one. The "as of" line is the
        // only thing distinguishing a paused panel from a healthy server.
        let taken = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(theme.border_soft)
            .text_size(theme.scale(10.))
            .text_color(theme.text_faint)
            .child(crate::i18n::tr!(
                "server.sampled_at",
                "as of {when}",
                when = crate::fmt::fmt_ago_secs(crate::health::now_unix() - snap.taken_at)
            ))
            .when(panel.metrics_loading, |d| {
                d.child(div().child("refreshing…"))
            });

        let unavailable = (!snap.unavailable.is_empty()).then(|| {
            div()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(theme.border_soft)
                .text_size(theme.scale(10.))
                .text_color(theme.yellow)
                .child(crate::i18n::tr!(
                    "server.not_visible",
                    "Not visible to this connection:"
                ))
                .children(
                    snap.unavailable
                        .iter()
                        .map(|u| div().text_color(theme.text_muted).child(format!("· {u}"))),
                )
        });

        // The pointer across to the Redis-only diagnostics, which deliberately
        // stay out of the shared panel: SLOWLOG and MONITOR have no SQL or Mongo
        // analogue, and folding them in would make this a union of three
        // engines' quirks.
        let monitor_hint = (snap.engine == red_core::DbKind::Redis).then(|| {
            div()
                .flex_shrink_0()
                .px_3()
                .py_1()
                .border_t_1()
                .border_color(theme.border_soft)
                .text_size(theme.scale(10.))
                .text_color(theme.text_faint)
                .child(crate::i18n::tr!(
                    "server.redis_monitor_hint",
                    "Slow log, MONITOR and per-client detail are in the Monitor tab."
                ))
        });

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(taken)
            .child(
                div()
                    .id("server-overview-list")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_scroll()
                    .children(groups),
            )
            .children(unavailable)
            .children(monitor_hint)
            .into_any_element()
    }

    fn render_metric(
        &self,
        m: &ServerMetric,
        snap: &ServerSnapshot,
        prev: Option<&ServerSnapshot>,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let fill = m.value.fraction();
        // Only a *capacity* fills toward something bad. A 99% cache hit rate
        // fills the same bar and is the best possible news, so the warning scale
        // is applied to bounded-against-a-ceiling values and nothing else.
        let capacity = matches!(m.value, MetricValue::Ratio { total, .. } if total > 0);
        let hue = match fill {
            Some(f) if capacity => bar_color(f, &theme),
            Some(_) => theme.accent,
            None => theme.text,
        };

        // A cumulative total on its own is unreadable; the rate is the answer.
        // Absent until there are two samples to derive it from, and absent again
        // if the server restarted between them -- never rendered as zero.
        let rate = prev
            .and_then(|p| snap.rate_since(p, m.key))
            .map(|r| MetricValue::Rate(r).render());

        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .px_3()
            .py_1()
            .text_size(theme.scale(11.))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(div().text_color(theme.text_muted).child(m.label.clone()))
                    .child(
                        div()
                            .ml_auto()
                            .font_family(theme.mono_family.clone())
                            .text_color(hue)
                            .child(m.value.render()),
                    )
                    .children(rate.map(|r| {
                        div()
                            .font_family(theme.mono_family.clone())
                            .text_size(theme.scale(10.))
                            .text_color(theme.accent)
                            .child(r)
                    })),
            )
            .children(fill.map(|f| {
                div()
                    .h(px(3.))
                    .w_full()
                    .rounded(px(2.))
                    .bg(theme.bg_input)
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(f.max(0.01)))
                            .rounded(px(2.))
                            .bg(hue),
                    )
            }))
            .when_some(m.detail.clone(), |d, detail| {
                d.child(
                    div()
                        .text_size(theme.scale(10.))
                        .text_color(theme.text_faint)
                        .child(detail),
                )
            })
            .into_any_element()
    }
}

/// Colour a capacity bar by how full it is. The one judgement the panel makes,
/// and it is the judgement every engine's ceilings share: near the limit is bad
/// whether the limit is `maxmemory` or `max_connections`. Callers apply it only
/// to a value bounded against a real ceiling (see `capacity` above).
fn bar_color(fill: f32, theme: &flint::Theme) -> Hsla {
    if fill >= BAD_AT {
        theme.red
    } else if fill >= WARN_AT {
        theme.orange
    } else {
        theme.green
    }
}
