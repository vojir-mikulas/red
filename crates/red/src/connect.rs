//! The connection manager — the disconnected landing screen: a card listing
//! saved connections (click to connect, edit/delete actions) plus the add/edit
//! modal form. Pure assembly over Flint components; all state + actions live on
//! [`AppState`] (`app.rs`).

use flint::prelude::*;
use flint::Theme;
use gpui::{
    div, prelude::*, px, AnyElement, Context, FontWeight, Hsla, SharedString, WindowControlArea,
};
use red_core::DbKind;

use crate::app::{AppState, FormState, TestState};

/// The six label colors a connection can be tagged with, mapped onto semantic
/// theme tokens so they track the active theme. A connection stores the index.
pub(crate) fn label_color(index: u8, theme: &Theme) -> Hsla {
    let palette = [
        theme.red,
        theme.yellow,
        theme.green,
        theme.blue,
        theme.purple,
        theme.text_muted,
    ];
    palette[(index as usize).min(palette.len() - 1)]
}

/// The accent tint for an engine, used on the engine picker's dots and cards.
fn engine_tint(kind: DbKind, theme: &Theme) -> Hsla {
    match kind {
        DbKind::Postgres => theme.blue,
        DbKind::Sqlite => theme.cyan,
        DbKind::Mysql => theme.orange,
    }
}

/// A connection's last-used time as a coarse relative label ("just now", "5m
/// ago", "never") — the per-card recency the design shows on the right.
pub(crate) fn fmt_ago(secs: Option<u64>) -> String {
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
                .text_size(theme.scale(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_dim)
                .child(label),
        )
        .child(div().flex_1().h(px(1.)).bg(theme.border_soft))
}

impl AppState {
    pub(crate) fn render_connect(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Build the cards first — they reborrow `cx` mutably — then read the theme
        // for the surrounding chrome (it holds an immutable borrow). The form modal
        // is rendered at the root (see `render.rs`) so it works in any phase.
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
        let new_button = self.new_button(cx);
        let settings_gear = IconButton::new(
            "connect-settings",
            crate::icons::icon("settings", cx.theme().scale(16.), cx.theme().text_muted),
        )
        .size(IconButtonSize::Sm)
        .tooltip("Settings  ⌘,")
        .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)));

        let theme = cx.theme();

        // RED's wordmark sits in Nyx's logo slot — the red mark, the tagline as the
        // title tier, then a one-line descriptor, matching the welcome rhythm.
        let header = div()
            .child(
                div()
                    .text_color(theme.red)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.scale(34.))
                    .child("RED"),
            )
            .child(
                div()
                    .text_size(theme.scale(20.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .mt_3()
                    .child("Roughly Enough Data"),
            )
            .child(div().text_size(theme.scale(14.)).text_color(theme.text_faint).mt_1().child(
                "A fast, native database explorer. Pick a connection below, or create a new one.",
            ));

        let saved: AnyElement = if self.connections.is_empty() {
            div()
                .py_2()
                .text_size(theme.scale(12.))
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
            .on_click(|event, window, _| {
                tracing::info!(count = event.click_count(), "connect-screen click");
                if event.click_count() == 2 {
                    tracing::info!("connect-screen double-click -> titlebar_double_click");
                    #[cfg(target_os = "macos")]
                    window.titlebar_double_click();
                    #[cfg(not(target_os = "macos"))]
                    window.zoom_window();
                }
            })
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_center()
            .bg(theme.bg_app)
            .text_color(theme.text)
            .font_family(theme.font_family.clone())
            .child(column);

        // Settings gear floats top-right (the disconnected screen has no top bar).
        let gear = div()
            .absolute()
            .top(px(14.))
            .right(px(16.))
            .child(settings_gear);

        div()
            .size_full()
            .relative()
            .child(screen)
            .child(gear)
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
            .text_size(theme.scale(14.))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| {
                s.border_color(theme.accent)
                    .text_color(theme.text)
                    .bg(theme.accent_ghost)
            })
            .child(crate::icons::icon(
                "plus",
                theme.scale(15.),
                theme.text_muted,
            ))
            .child("New connection")
            .child(
                div()
                    .ml_1()
                    .px_1p5()
                    .rounded(px(4.))
                    .text_size(theme.scale(12.))
                    .text_color(theme.text_faint)
                    .bg(theme.bg_input)
                    .border_1()
                    .border_color(theme.border)
                    .font_family(theme.mono_family.clone())
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
        // The tinted icon square is colored with the connection's label color (the
        // engine is conveyed by the badge beside the name, not by the icon color).
        let accent = label_color(config.color, theme);
        let (badge_variant, badge_label) = match config.kind {
            DbKind::Sqlite => (BadgeVariant::Info, "SQLite"),
            DbKind::Postgres => (BadgeVariant::Special, "Postgres"),
            DbKind::Mysql => (BadgeVariant::Warning, "MySQL"),
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
            // The keyboard-highlighted card (↑/↓ on the welcome screen) gets the
            // accent border; the rest sit on the neutral border until hovered.
            .border_color(if index == self.connect_sel {
                theme.accent
            } else {
                theme.border
            })
            .cursor_pointer()
            .hover(|s| s.border_color(theme.border_strong).bg(theme.bg_active))
            .child(
                div()
                    .w(px(3.))
                    .h(px(30.))
                    .rounded_full()
                    .flex_none()
                    .bg(label_color(config.color, theme)),
            )
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
                    .child(crate::icons::icon("db", theme.scale(17.), accent)),
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
                                    .text_size(theme.scale(14.))
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
                                        .text_size(theme.scale(10.))
                                        .text_color(theme.yellow)
                                        .child(crate::icons::icon(
                                            "lock",
                                            theme.scale(10.),
                                            theme.yellow,
                                        ))
                                        .child("read-only"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(theme.scale(12.))
                            .text_color(theme.text_faint)
                            .font_family(theme.mono_family.clone())
                            .truncate()
                            .child(config.display_target()),
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
                    .child(div().group_hover(group.clone(), |s| s.invisible()).child(
                        crate::icons::icon("chevron", theme.scale(16.), theme.text_dim),
                    ))
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
                                    crate::icons::icon("edit", theme.scale(14.), theme.text_muted),
                                )
                                .size(IconButtonSize::Sm)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.open_edit_form(index, cx);
                                    },
                                )),
                            )
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("delete-{index}")),
                                    crate::icons::icon("trash", theme.scale(14.), theme.red),
                                )
                                .size(IconButtonSize::Sm)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.request_delete_connection(index, cx);
                                    },
                                )),
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
                    .child(crate::icons::icon(
                        "clock",
                        theme.scale(14.),
                        theme.text_faint,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(theme.scale(14.))
                            .truncate()
                            .child(config.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(theme.scale(12.))
                            .text_color(theme.text_faint)
                            .child(fmt_ago(last_accessed)),
                    ),
            )
            .child(crate::icons::icon(
                "chevron",
                theme.scale(15.),
                theme.text_dim,
            ))
            .on_click(cx.listener(move |this, _, _, cx| this.connect(index, cx)))
    }

    pub(crate) fn render_form(&self, form: &FormState, cx: &mut Context<Self>) -> impl IntoElement {
        // Owned clone so the theme doesn't hold an immutable borrow of `cx` across
        // the `&mut cx` helper calls below.
        let theme = cx.theme().clone();
        let theme = &theme;
        let view = cx.entity().downgrade();
        let is_file = form.kind.is_file();
        let valid = self
            .form_config(cx)
            .is_some_and(|c| AppState::form_valid(&c));
        let title = if form.editing.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };

        // Network engines get a live connection-string field that mirrors — and is
        // mirrored by — the structured fields. File engines have only a path.
        let conn_str_field = (!is_file)
            .then(|| labeled_field("Connection string", theme).child(self.conn_str_input.clone()));

        let footer = self.render_form_footer(form, valid, cx);

        let close_view = view.clone();
        Modal::new("connection-form")
            .title(title)
            .width(px(520.))
            // The shared modal focus handle is this form's ancestor, so the
            // focus-trap listener keeps Tab inside the dialog. The name field is
            // focused on open (see `focus_name_field`), not the handle itself.
            .focus_handle(self.modal_focus.clone())
            .footer(footer)
            .on_close(move |_, cx| {
                close_view.update(cx, |this, cx| this.close_form(cx)).ok();
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(labeled_field("Name", theme).child(self.name_input.clone()))
                    .child(
                        labeled_field("Engine", theme)
                            .child(self.render_engine_picker(form.kind, theme, cx)),
                    )
                    .children(conn_str_field)
                    .child(self.render_connection_fields(form, is_file, theme))
                    .child(self.render_label_access_row(form, theme, cx)),
            )
    }

    /// The per-engine connection fields: a file path, or host/port/database/
    /// user/password for a network engine.
    fn render_connection_fields(
        &self,
        form: &FormState,
        is_file: bool,
        theme: &Theme,
    ) -> AnyElement {
        if is_file {
            return labeled_field("Database file", theme)
                .child(self.database_input.clone())
                .into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        labeled_field("Host", theme)
                            .flex_1()
                            .child(self.host_input.clone()),
                    )
                    .child(
                        labeled_field("Port", theme)
                            .w(px(88.))
                            .flex_none()
                            .child(self.port_input.clone()),
                    ),
            )
            .child(
                labeled_field(
                    // MySQL can browse the whole server, so its database is
                    // optional — blank shows every database.
                    if form.kind == DbKind::Mysql {
                        "Database (optional)"
                    } else {
                        "Database"
                    },
                    theme,
                )
                .child(self.database_input.clone()),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        labeled_field("User", theme)
                            .flex_1()
                            .child(self.user_input.clone()),
                    )
                    .child(
                        labeled_field("Password", theme)
                            .flex_1()
                            .child(self.password_input.clone()),
                    ),
            )
            .into_any_element()
    }

    /// The engine segmented control — one cell per `DbKind`, with the engine tint
    /// dot. Data-driven off `DbKind::all`, so a new driver appears automatically.
    fn render_engine_picker(
        &self,
        selected: DbKind,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let cells = DbKind::all().iter().map(|&kind| {
            let on = kind == selected;
            let (bg, border, text) = if on {
                (theme.accent_ghost, theme.accent, theme.text)
            } else {
                (theme.bg_input, theme.border_soft, theme.text_muted)
            };
            div()
                .id(SharedString::from(format!("engine-{}", kind.url_scheme())))
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .gap_1p5()
                .h(px(32.))
                .rounded(theme.radius)
                .bg(bg)
                .border_1()
                .border_color(border)
                .text_size(theme.scale(12.))
                .text_color(text)
                .cursor_pointer()
                .hover(|s| s.text_color(theme.text))
                .child(
                    div()
                        .size(px(8.))
                        .rounded_full()
                        .flex_none()
                        .bg(engine_tint(kind, theme)),
                )
                .child(kind.to_string())
                .on_click(cx.listener(move |this, _, _, cx| this.set_form_kind(kind, cx)))
        });
        div().flex().gap_1p5().children(cells)
    }

    /// The label-color swatches + the read-only access toggle, sharing one row.
    fn render_label_access_row(
        &self,
        form: &FormState,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let swatches = (0..6u8).map(|i| {
            let color = label_color(i, theme);
            let on = i == form.color;
            div()
                .id(SharedString::from(format!("swatch-{i}")))
                .size(px(20.))
                .rounded_full()
                .bg(color)
                .cursor_pointer()
                .border_2()
                .border_color(if on { color } else { theme.bg_elevated })
                .when(on, |s| s.shadow_sm())
                .on_click(cx.listener(move |this, _, _, cx| this.set_form_color(i, cx)))
        });

        div()
            .flex()
            .items_start()
            .gap_3()
            .child(
                labeled_field("Label", theme).child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(7.))
                        .h(px(32.))
                        .children(swatches),
                ),
            )
            .child(
                labeled_field("Access", theme).ml_auto().flex_none().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .h(px(32.))
                        .child(
                            Toggle::new("read-only", form.read_only).on_change(cx.listener(
                                |this, checked: &bool, _, cx| this.set_form_read_only(*checked, cx),
                            )),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(theme.scale(12.5))
                                .text_color(if form.read_only {
                                    theme.accent
                                } else {
                                    theme.text_muted
                                })
                                .child(crate::icons::icon(
                                    "lock",
                                    theme.scale(12.),
                                    if form.read_only {
                                        theme.accent
                                    } else {
                                        theme.text_muted
                                    },
                                ))
                                .child("Read-only"),
                        ),
                ),
            )
    }

    /// The modal footer: a Test-connection action with its result on the left, and
    /// Cancel / Save / Connect on the right.
    fn render_form_footer(
        &self,
        form: &FormState,
        valid: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let testing = matches!(form.test, TestState::Testing);
        let result = match &form.test {
            TestState::Idle | TestState::Testing => None,
            TestState::Ok(msg) => Some((msg.clone(), theme.green)),
            TestState::Fail(msg) => Some((msg.clone(), theme.red)),
        };

        let test_side = div()
            .flex()
            .items_center()
            .gap_2p5()
            .flex_1()
            .min_w_0()
            .child(
                Button::new(
                    "test-conn",
                    if testing {
                        "Testing…"
                    } else {
                        "Test connection"
                    },
                )
                .variant(ButtonVariant::Ghost)
                .icon(crate::icons::icon(
                    "play",
                    theme.scale(13.),
                    theme.text_muted,
                ))
                .disabled(testing)
                .on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))),
            )
            .when_some(result, |row, (msg, color)| {
                row.child(
                    div()
                        .min_w_0()
                        .text_size(theme.scale(11.5))
                        .text_color(color)
                        .font_family(theme.mono_family.clone())
                        .truncate()
                        .child(msg),
                )
            });

        div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .child(test_side)
            .child(
                Button::new("form-cancel", "Cancel")
                    .variant(ButtonVariant::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| this.close_form(cx))),
            )
            .child(
                Button::new("form-save", "Save")
                    .variant(ButtonVariant::Secondary)
                    .disabled(!valid)
                    .on_click(cx.listener(|this, _, _, cx| this.save_form(false, cx))),
            )
            .child(
                Button::new("form-connect", "Connect")
                    .variant(ButtonVariant::Primary)
                    .icon(crate::icons::icon(
                        "power",
                        theme.scale(14.),
                        theme.on_accent,
                    ))
                    .disabled(!valid)
                    .on_click(cx.listener(|this, _, _, cx| this.save_form(true, cx))),
            )
    }
}

/// A small uppercase field caption, matching the design's form labels.
fn field_label(text: impl Into<String>, theme: &Theme) -> impl IntoElement {
    div()
        .text_size(theme.scale(10.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_faint)
        .child(text.into().to_uppercase())
}

/// A labeled form field column: the caption above its control (passed as a child).
fn labeled_field(label: impl Into<String>, theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1p5()
        .child(field_label(label, theme))
}
