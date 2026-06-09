// SPDX-License-Identifier: GPL-3.0-or-later

//! The root view. A placeholder welcome screen that proves the wiring —
//! GPUI + the shared Flint components + theme tokens. The real shell (connection
//! manager, schema explorer, SQL editor, result grid) grows from here.

use flint::{ActiveTheme, Button, ButtonVariant};
use gpui::{div, prelude::*, px, Context, FontWeight, Window};

use crate::assets::{FONT_MONO, FONT_UI};

/// The single root entity. Holds no state yet — backend wiring is the next step.
pub struct AppState;

impl AppState {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self
    }
}

impl Render for AppState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(theme.bg_app)
            .text_color(theme.text)
            .font_family(FONT_UI)
            .child(
                div()
                    .text_size(px(34.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.accent)
                    .child("RED"),
            )
            .child(
                div()
                    .text_size(px(15.))
                    .text_color(theme.text_muted)
                    .child("Roughly Enough Data"),
            )
            .child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(px(12.))
                    .text_color(theme.text_faint)
                    .child("sqlite · postgres"),
            )
            .child(
                div().pt_4().child(
                    Button::new("new-connection", "New Connection")
                        .variant(ButtonVariant::Primary)
                        .on_click(|_, _, _| tracing::info!("New Connection clicked")),
                ),
            )
    }
}
