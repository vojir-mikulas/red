// SPDX-License-Identifier: GPL-3.0-or-later

//! The connection manager — the disconnected landing screen: a card listing
//! saved connections (click to connect, edit/delete actions) plus the add/edit
//! modal form. Pure assembly over Flint components; all state + actions live on
//! [`AppState`] (`app.rs`).

use flint::prelude::*;
use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};
use red_core::DbKind;

use crate::app::{AppState, FormState};
use crate::assets::{FONT_MONO, FONT_UI};

impl AppState {
    pub(crate) fn render_connect(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Build the cards + modal first — they reborrow `cx` mutably — then read
        // the theme for the surrounding chrome (it holds an immutable borrow).
        let cards: Vec<AnyElement> = self
            .connections
            .iter()
            .enumerate()
            .map(|(ix, stored)| {
                self.connection_card(ix, &stored.config, cx)
                    .into_any_element()
            })
            .collect();
        let modal = self.form.as_ref().map(|form| self.render_form(form, cx));

        let theme = cx.theme();

        let header = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(
                div()
                    .text_color(theme.red)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(24.))
                    .child("RED"),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.text_faint)
                    .child("Roughly Enough Data"),
            );

        let label = div()
            .text_size(px(10.5))
            .text_color(theme.text_faint)
            .child("CONNECTIONS");

        let list: AnyElement = if self.connections.is_empty() {
            div()
                .py_4()
                .text_size(px(12.))
                .text_color(theme.text_faint)
                .child("No saved connections yet.")
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .children(cards)
                .into_any_element()
        };

        let card = div()
            .w(px(480.))
            .p_5()
            .rounded(px(14.))
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border_soft)
            .flex()
            .flex_col()
            .gap_4()
            .child(header)
            .child(label)
            .child(list)
            .child(
                Button::new("new-connection", "+ New connection")
                    .variant(ButtonVariant::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.open_new_form(cx))),
            );

        let screen = div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.bg_app)
            .font_family(FONT_UI)
            .child(card);

        div().size_full().child(screen).children(modal)
    }

    fn connection_card(
        &self,
        index: usize,
        config: &red_core::ConnectionConfig,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let glyph = match config.kind {
            DbKind::Sqlite => "S",
            DbKind::Postgres => "P",
        };

        let main = div()
            .id(SharedString::from(format!("connect-{index}")))
            .flex_1()
            .flex()
            .items_center()
            .gap_3()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.connect(index, cx)))
            .child(
                div()
                    .flex_shrink_0()
                    .size(px(30.))
                    .rounded(px(8.))
                    .bg(theme.bg_active)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.text_muted)
                    .child(glyph),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .font_family(FONT_MONO)
                                    .text_color(theme.text)
                                    .child(config.name.clone()),
                            )
                            .when(config.read_only, |row| {
                                row.child(Badge::new("read-only").variant(BadgeVariant::Warning))
                            }),
                    )
                    .child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(px(11.))
                            .text_color(theme.text_faint)
                            .child(format!("{} • {}", config.kind, config.dsn)),
                    ),
            );

        let actions = div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                Button::new(SharedString::from(format!("edit-{index}")), "Edit")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |this, _, _, cx| this.open_edit_form(index, cx))),
            )
            .child(
                Button::new(SharedString::from(format!("delete-{index}")), "Delete")
                    .variant(ButtonVariant::Danger)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |this, _, _, cx| this.delete_connection(index, cx))),
            );

        div()
            .flex()
            .items_center()
            .gap_2()
            .p_2p5()
            .rounded(px(9.))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border)
            .child(main)
            .child(actions)
    }

    fn render_form(&self, form: &FormState, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = cx.entity().downgrade();
        let kind_ix = match form.kind {
            DbKind::Sqlite => 0,
            DbKind::Postgres => 1,
        };
        let dsn_label = match form.kind {
            DbKind::Sqlite => "Database file",
            DbKind::Postgres => "Connection URL",
        };
        let title = if form.editing.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };

        let field_label =
            |text: &'static str| div().text_xs().text_color(cx.theme().text_dim).child(text);

        let engine_select = {
            let toggle_view = view.clone();
            let select_view = view.clone();
            Select::new("engine")
                .option("SQLite")
                .option("PostgreSQL")
                .selected(kind_ix)
                .open(form.kind_open)
                .on_toggle(move |_, cx| {
                    toggle_view
                        .update(cx, |this, cx| this.toggle_form_kind_open(cx))
                        .ok();
                })
                .on_select(move |ix, _, cx| {
                    let kind = if ix == 0 {
                        DbKind::Sqlite
                    } else {
                        DbKind::Postgres
                    };
                    select_view
                        .update(cx, |this, cx| this.set_form_kind(kind, cx))
                        .ok();
                })
        };

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(field_label("Name"))
                    .child(self.name_input.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(field_label("Engine"))
                    .child(engine_select),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(field_label(dsn_label))
                    .child(self.dsn_input.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Toggle::new("read-only", form.read_only).on_change(cx.listener(
                            |this, checked: &bool, _, cx| this.set_form_read_only(*checked, cx),
                        )),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child("Read-only connection"),
                    ),
            );

        let footer = div()
            .flex()
            .gap_2()
            .child(
                Button::new("form-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.close_form(cx))),
            )
            .child(
                Button::new("form-save", "Save")
                    .variant(ButtonVariant::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.save_form(cx))),
            );

        let close_view = view.clone();
        Modal::new("connection-form")
            .title(title)
            .width(px(440.))
            .footer(footer)
            .on_close(move |_, cx| {
                close_view.update(cx, |this, cx| this.close_form(cx)).ok();
            })
            .child(body)
    }
}
