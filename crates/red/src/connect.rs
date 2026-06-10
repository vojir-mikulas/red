// SPDX-License-Identifier: GPL-3.0-or-later

//! The connection manager — the disconnected landing screen: a card listing
//! saved connections (click to connect, edit/delete actions) plus the add/edit
//! modal form. Pure assembly over Flint components; all state + actions live on
//! [`AppState`] (`app.rs`).

use flint::prelude::*;
use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString, WindowControlArea};
use red_core::DbKind;

use crate::app::{AppState, FormState};
use crate::assets::{FONT_MONO, FONT_UI};

/// A connection's last-used time as a coarse relative label ("just now", "5m
/// ago", "never") — the per-card recency the design shows on the right.
fn fmt_ago(secs: Option<u64>) -> String {
    let Some(secs) = secs else {
        return "never".into();
    };
    let delta = crate::config::now().saturating_sub(secs);
    match delta {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86_399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86_400),
    }
}

/// A section heading ("Saved connections", "Recent") with a hairline rule that
/// fills the rest of the row — the divider the welcome layout uses.
fn section_label(label: &'static str, theme: &flint::Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .mt(px(26.))
        .mb_2()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_dim)
                .child(label),
        )
        .child(div().flex_1().h(px(1.)).bg(theme.border_soft))
}

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
        // "Recent" = the saved connections that have actually been opened, newest
        // first (the list arrives recency-sorted from `config::load`), capped at 3.
        let recents: Vec<AnyElement> = self
            .connections
            .iter()
            .enumerate()
            .filter(|(_, stored)| stored.last_accessed.is_some())
            .take(3)
            .map(|(ix, stored)| {
                self.recent_row(ix, &stored.config, stored.last_accessed, cx)
                    .into_any_element()
            })
            .collect();
        let modal = self.form.as_ref().map(|form| self.render_form(form, cx));
        let new_button = self.new_button(cx);

        let theme = cx.theme();

        // RED's wordmark sits in Nyx's logo slot — the red mark, the tagline as the
        // title tier, then a one-line descriptor, matching the welcome rhythm.
        let header = div()
            .child(
                div()
                    .text_color(theme.red)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(34.))
                    .child("RED"),
            )
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .mt_3()
                    .child("Roughly Enough Data"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.text_faint)
                    .mt_1()
                    .child("A fast, native database explorer. Pick a connection below, or create a new one."),
            );

        let saved: AnyElement = if self.connections.is_empty() {
            div()
                .py_2()
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

        let column = div()
            .w_full()
            .max_w(px(620.))
            .px_8()
            .pt(px(72.))
            .pb(px(60.))
            .child(header)
            .child(section_label("Saved connections", theme))
            .child(saved)
            .child(new_button)
            .when(!recents.is_empty(), |this| {
                this.child(section_label("Recent", theme))
                    .child(div().flex().flex_col().gap_1p5().children(recents))
            });

        // The whole backdrop drags the window (seamless traffic lights float over
        // it); the rows + buttons keep their own hitboxes.
        let screen = div()
            .id("connect-screen")
            .window_control_area(WindowControlArea::Drag)
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_center()
            .bg(theme.bg_app)
            .text_color(theme.text)
            .font_family(FONT_UI)
            .child(column);

        div().size_full().child(screen).children(modal)
    }

    fn new_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id("connect-new")
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .w_full()
            .h(px(42.))
            .mt_2()
            .rounded(theme.radius)
            .border_1()
            .border_dashed()
            .border_color(theme.border_strong)
            .text_sm()
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| {
                s.border_color(theme.accent)
                    .text_color(theme.text)
                    .bg(theme.accent_ghost)
            })
            .child(crate::icons::icon("plus", px(15.), theme.text_muted))
            .child("New connection")
            .child(
                div()
                    .ml_1()
                    .px_1p5()
                    .rounded(px(4.))
                    .text_xs()
                    .text_color(theme.text_faint)
                    .bg(theme.bg_input)
                    .border_1()
                    .border_color(theme.border)
                    .font_family(FONT_MONO)
                    .child("⌘N"),
            )
            .on_click(cx.listener(|this, _, _, cx| this.open_new_form(cx)))
    }

    fn connection_card(
        &self,
        index: usize,
        config: &red_core::ConnectionConfig,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        // Each card carries its engine color on the tinted icon square: SQLite →
        // cyan, Postgres → blue.
        let accent = match config.kind {
            DbKind::Sqlite => theme.cyan,
            DbKind::Postgres => theme.blue,
        };
        let (badge_variant, badge_label) = match config.kind {
            DbKind::Sqlite => (BadgeVariant::Info, "SQLite"),
            DbKind::Postgres => (BadgeVariant::Special, "Postgres"),
        };
        let group = SharedString::from(format!("connect-card-{index}"));

        div()
            .id(SharedString::from(format!("connect-{index}")))
            .group(group.clone())
            .flex()
            .items_center()
            .gap(px(13.))
            .p(px(12.))
            .px(px(14.))
            .rounded(theme.radius)
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|s| s.border_color(theme.border_strong).bg(theme.bg_active))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(34.))
                    .rounded(px(8.))
                    .bg(accent.opacity(0.12))
                    .border_1()
                    .border_color(accent.opacity(0.35))
                    .child(crate::icons::icon("db", px(17.), accent)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(config.name.clone()),
                            )
                            .child(Badge::new(badge_label).variant(badge_variant))
                            .when(config.read_only, |row| {
                                row.child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .px_1p5()
                                        .py(px(1.))
                                        .rounded(theme.radius_sm)
                                        .bg(theme.yellow.opacity(0.1))
                                        .text_size(px(10.))
                                        .text_color(theme.yellow)
                                        .child(crate::icons::icon("lock", px(10.), theme.yellow))
                                        .child("read-only"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_faint)
                            .font_family(FONT_MONO)
                            .truncate()
                            .child(config.dsn.clone()),
                    ),
            )
            // Chevron by default, swapped for Edit / Remove on hover (the buttons
            // stop propagation so they don't open the connection).
            .child(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .justify_end()
                    .w(px(58.))
                    .child(
                        div()
                            .group_hover(group.clone(), |s| s.invisible())
                            .child(crate::icons::icon("chevron", px(16.), theme.text_dim)),
                    )
                    .child(
                        div()
                            .absolute()
                            .right_0()
                            .flex()
                            .items_center()
                            .gap_1()
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("edit-{index}")),
                                    crate::icons::icon("edit", px(14.), theme.text_muted),
                                )
                                .size(IconButtonSize::Sm)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.open_edit_form(index, cx);
                                })),
                            )
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("delete-{index}")),
                                    crate::icons::icon("trash", px(14.), theme.red),
                                )
                                .size(IconButtonSize::Sm)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.delete_connection(index, cx);
                                })),
                            ),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.connect(index, cx)))
    }

    fn recent_row(
        &self,
        index: usize,
        config: &red_core::ConnectionConfig,
        last_accessed: Option<u64>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id(SharedString::from(format!("connect-recent-{index}")))
            .flex()
            .items_center()
            .gap(px(13.))
            .py(px(9.))
            .px(px(14.))
            .rounded(theme.radius)
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|s| s.border_color(theme.border_strong).bg(theme.bg_active))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.))
                    .rounded(px(8.))
                    .bg(theme.bg_input)
                    .border_1()
                    .border_color(theme.border_soft)
                    .child(crate::icons::icon("clock", px(14.), theme.text_faint)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(config.name.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_faint)
                            .child(fmt_ago(last_accessed)),
                    ),
            )
            .child(crate::icons::icon("chevron", px(15.), theme.text_dim))
            .on_click(cx.listener(move |this, _, _, cx| this.connect(index, cx)))
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
