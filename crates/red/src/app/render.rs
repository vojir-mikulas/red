//! Root rendering: the `Render` impl that picks the top-level screen, the
//! connecting splash, and the two confirmation modals (destructive statement,
//! close-with-unsaved-work).

use flint::prelude::*;
use gpui::{div, prelude::*, px, Render, Window};

use super::{AppState, ConnectStatus, Connecting, Phase};
use crate::palette::{GoToRow, ToggleCommandPalette};

impl AppState {
    /// The connecting splash: an indeterminate progress bar while an attempt is
    /// in flight, the error plus a backoff countdown between retries, and always
    /// a Cancel (quit-the-load) button — with "Retry now" while backing off.
    fn render_connecting(&self, conn: &Connecting, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let name = conn.config.name.clone();

        let status = div().flex().flex_col().items_center().gap_2().w(px(360.));
        let status = match &conn.status {
            ConnectStatus::InProgress => {
                let label = if conn.attempt > 1 {
                    format!("Connecting to {name}… (attempt {})", conn.attempt)
                } else {
                    format!("Connecting to {name}…")
                };
                status
                    .child(div().text_color(theme.text).child(label))
                    .child(ProgressBar::new("connect-progress", 0.0).indeterminate(true))
            }
            ConnectStatus::Backoff { error, delay } => status
                .child(
                    div()
                        .text_color(theme.text)
                        .child(format!("Couldn't connect to {name}")),
                )
                .child(
                    div()
                        .text_color(theme.text_muted)
                        .text_size(theme.scale(12.))
                        .child(error.clone()),
                )
                .child(
                    div()
                        .text_color(theme.text_muted)
                        .text_size(theme.scale(12.))
                        .child(format!("Retrying in {}s…", delay.as_secs())),
                )
                .child(ProgressBar::new("connect-progress", 0.0).indeterminate(true)),
        };

        let mut actions = div().flex().justify_center().gap_2();
        if matches!(conn.status, ConnectStatus::Backoff { .. }) {
            actions = actions.child(
                Button::new("connect-retry", "Retry now")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.retry_now(cx))),
            );
        }
        actions = actions.child(
            Button::new("connect-cancel", "Cancel")
                .variant(ButtonVariant::Secondary)
                .on_click(cx.listener(|this, _, _, cx| this.cancel_connect(cx))),
        );

        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(theme.bg_app)
            .font_family(theme.font_family.clone())
            .child(status)
            .child(actions)
    }
}

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Dev perf HUD: time this render and tally its allocation churn. No-op
        // (compiled out) in a normal build.
        #[cfg(feature = "dev-stats")]
        self.dev_stats.begin_frame();

        // An overlay just closed (or we're starting up): reclaim focus so the
        // global ⌘K binding has a live dispatch target again.
        if self.refocus_root {
            self.refocus_root = false;
            window.focus(&self.root_focus, cx);
        }

        // First paint: install the OS-appearance observer and the settings
        // file-watcher (both need a live `Window`).
        self.ensure_observers(window, cx);

        let screen = match &self.phase {
            Phase::Disconnected => self.render_connect(cx).into_any_element(),
            Phase::Connecting(conn) => self.render_connecting(conn, cx).into_any_element(),
            Phase::Connected(active) => self.render_shell(active, cx).into_any_element(),
        };

        // The notification stack, anchored bottom-right and growing upward:
        // oldest first in the column, so the newest sits nearest the corner. At
        // most `MAX_VISIBLE` show; the rest collapse into a "+N more" line on top.
        let toast = (!self.notifications.is_empty()).then(|| self.render_notifications(cx));

        let confirm = self
            .confirm_exec
            .clone()
            .map(|sql| self.render_confirm(sql, cx));

        let confirm_close = self
            .confirm_close_tab
            .and_then(|i| self.tab_title(i))
            .map(|title| self.render_confirm_close(title, cx));

        let settings = self
            .settings_open
            .then(|| self.render_settings(cx).into_any_element());

        let theme = cx.theme();
        let root = div()
            .size_full()
            .relative()
            // Anchor focus + the global ⌘K binding here so the palette toggles
            // from any phase, even when no field or editor is focused.
            .key_context("RedRoot")
            .track_focus(&self.root_focus)
            .on_action(cx.listener(|this, _: &ToggleCommandPalette, _, cx| this.toggle_palette(cx)))
            .on_action(cx.listener(|this, _: &GoToRow, _, cx| this.open_goto_prompt(cx)))
            .bg(theme.bg_app)
            .text_color(theme.text)
            // The UI font + size from settings, set once at the root so any unsized
            // text inherits the right family/scale (GPUI otherwise defaults to 16px
            // Helvetica). The editor overrides both on its own surface.
            .font_family(self.settings.appearance.ui_font_family.clone())
            .text_size(px(self.settings.appearance.ui_font_size))
            .child(screen)
            .children(toast)
            .children(confirm)
            .children(confirm_close)
            .children(settings)
            // The palette renders its own full-screen overlay; last = on top.
            .children(self.palette.clone());

        // Dev perf HUD: register its toggle, overlay the panel last (on top), and
        // close the frame so the rings capture this render's cost.
        #[cfg(feature = "dev-stats")]
        let root = {
            let root = root.on_action(
                cx.listener(|this, _: &crate::ToggleDevStats, _, cx| this.toggle_dev_stats(cx)),
            );
            let panel = self.render_dev_panel(cx);
            self.dev_stats.end_frame();
            root.children(panel)
        };

        root
    }
}

/// How many toasts show at once; older ones beyond this collapse to "+N more".
const MAX_VISIBLE_TOASTS: usize = 5;

impl AppState {
    /// The bottom-right notification stack: oldest first (top), newest last
    /// (nearest the corner). Each toast carries a close `✕` wired to
    /// [`AppState::close_notification`]; the export toast also shows its progress.
    fn render_notifications(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let total = self.notifications.len();
        let hidden = total.saturating_sub(MAX_VISIBLE_TOASTS);

        let mut col = div()
            .absolute()
            .bottom_4()
            .right_4()
            .flex()
            .flex_col()
            .items_end()
            .gap_2();

        if hidden > 0 {
            col = col.child(
                div()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_muted)
                    .child(format!("+{hidden} more")),
            );
        }

        for n in self.notifications.iter().skip(hidden) {
            let id = n.id;
            let mut toast = Toast::new(n.message.clone()).variant(n.variant);
            if let Some(export) = &n.export {
                let fraction = if export.total > 0 {
                    export.rows as f32 / export.total as f32
                } else {
                    0.0
                };
                toast = toast.progress(fraction);
            }
            let weak = cx.entity().downgrade();
            col = col.child(toast.on_close(move |_, cx| {
                weak.update(cx, |this, cx| this.close_notification(id, cx))
                    .ok();
            }));
        }

        col
    }

    /// The title of tab `index`, if it exists — for the close-confirm prompt.
    fn tab_title(&self, index: usize) -> Option<String> {
        match &self.phase {
            Phase::Connected(active) => active.tabs.get(index).map(|t| t.title.clone()),
            _ => None,
        }
    }

    /// Confirmation before closing a tab that holds real work. Mirrors the
    /// destructive-statement modal's shape.
    fn render_confirm_close(&self, title: String, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let body = div().text_color(theme.text_muted).child(format!(
            "“{title}” has a query or result that will be lost. Close it?"
        ));
        let footer = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("close-cancel", "Keep tab")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_close(cx))),
            )
            .child(
                Button::new("close-confirm", "Close tab")
                    .variant(ButtonVariant::Danger)
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_close(cx))),
            );
        Modal::new("confirm-close-tab")
            .title("Close tab")
            .width(px(420.))
            .footer(footer)
            .on_close(move |_, cx| {
                close_view.update(cx, |this, cx| this.cancel_close(cx)).ok();
            })
            .child(body)
    }

    /// The destructive-statement confirmation modal — the write safety rail.
    fn render_confirm(&self, sql: String, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let preview: String = sql.chars().take(200).collect();
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child("This statement modifies data and can't be undone. Run it?"),
            )
            .child(
                div()
                    .p_2()
                    .rounded(theme.radius_sm)
                    .bg(theme.bg_input)
                    .font_family(theme.mono_family.clone())
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(preview),
            );
        let footer = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("confirm-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_destructive(cx))),
            )
            .child(
                Button::new("confirm-run", "Run statement")
                    .variant(ButtonVariant::Danger)
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_destructive(cx))),
            );
        Modal::new("confirm-destructive")
            .title("Confirm destructive statement")
            .width(px(440.))
            .footer(footer)
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.cancel_destructive(cx))
                    .ok();
            })
            .child(body)
    }
}

#[cfg(feature = "dev-stats")]
impl AppState {
    /// The dev perf HUD overlay: a small bottom-right mono panel with the budget
    /// readouts (build time, allocs/frame, live + RSS bytes, the grid footprint).
    /// `None` while toggled off. Kept deliberately trivial — building it allocates
    /// and takes time, so it lightly perturbs its own reading (see the plan).
    fn render_dev_panel(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.dev_stats.visible() {
            return None;
        }
        let theme = cx.theme();
        let ds = &self.dev_stats;
        let mb = |bytes: usize| format!("{} MB", bytes / (1024 * 1024));
        let rss = ds.rss().map(&mb).unwrap_or_else(|| "—".into());
        // `gap` is the interval between renders — the repaint cadence during
        // interaction. Idle is notify-gated (no frame stream), so a large gap at
        // rest is correct, not a stall (see the plan's fps caveat).
        let line1 = format!(
            "build {:.2} ms · gap {:.0} ms · {:.0} allocs/f · live {} · rss {}",
            ds.build_ms(),
            ds.interval_ms(),
            ds.allocs_per_frame(),
            mb(ds.live_bytes()),
            rss,
        );

        let grid = match &self.phase {
            Phase::Connected(active) => active.active_result().map(|g| g.dev_snapshot()),
            _ => None,
        };
        let line2 = match grid {
            Some(g) => format!(
                "grid {} rows · {} · {} in-flight · q {:.0} ms",
                crate::result::group_digits(g.resident_rows),
                g.mode,
                g.in_flight,
                g.last_query_ms,
            ),
            None => "grid —".to_string(),
        };

        Some(
            div()
                .absolute()
                .bottom_2()
                .right_2()
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(theme.radius_sm)
                .bg(theme.bg_panel)
                .border_1()
                .border_color(theme.border)
                .font_family(theme.mono_family.clone())
                .text_size(theme.scale(10.))
                .text_color(theme.text_muted)
                .child(div().child(line1))
                .child(div().text_color(theme.text_faint).child(line2))
                .into_any_element(),
        )
    }
}
