//! Root rendering: the `Render` impl that picks the top-level screen, the
//! connecting splash, and the two confirmation modals (destructive statement,
//! close-with-unsaved-work).

use flint::prelude::*;
use gpui::{div, prelude::*, px, Render, SharedString, Window};

use super::{AppState, Phase};
use crate::assets::{FONT_MONO, FONT_UI};

impl AppState {
    fn render_connecting(&self, name: SharedString, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .bg(theme.bg_app)
            .font_family(FONT_UI)
            .child(
                div()
                    .text_color(theme.text)
                    .child(format!("Connecting to {name}…")),
            )
    }
}

impl Render for AppState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let screen = match &self.phase {
            Phase::Disconnected => self.render_connect(cx).into_any_element(),
            Phase::Connecting { config } => self
                .render_connecting(config.name.clone().into(), cx)
                .into_any_element(),
            Phase::Connected(active) => self.render_shell(active, cx).into_any_element(),
        };

        // Dismissible error toast, anchored bottom-center over whatever screen.
        let toast = self.toast.clone().map(|(message, variant)| {
            div()
                .absolute()
                .bottom_4()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("toast-dismiss")
                        .cursor_pointer()
                        .child(Toast::new(message).variant(variant))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toast = None;
                            cx.notify();
                        })),
                )
        });

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
        div()
            .size_full()
            .relative()
            .bg(theme.bg_app)
            .text_color(theme.text)
            .font_family(FONT_UI)
            // The design's base font size is 13px; GPUI defaults to 16px, so set
            // it once at the root and any unsized text inherits the right scale.
            .text_size(px(13.))
            .child(screen)
            .children(toast)
            .children(confirm)
            .children(confirm_close)
            .children(settings)
    }
}

impl AppState {
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
                    .font_family(FONT_MONO)
                    .text_size(px(12.))
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
