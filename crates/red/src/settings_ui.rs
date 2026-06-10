//! The settings panel — a Zed-style left category nav beside a scrolling content
//! pane, ported from Nyx's `settings_modal`. Built as a custom scrim + card (not
//! the shared `Modal`) so the nav stays fixed while only the page scrolls. Pure
//! assembly over Flint components; the state + actions live on [`AppState`]
//! (`app.rs`), persisted through [`crate::settings`].

use flint::prelude::*;
use flint::Theme;
use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, MouseButton, SharedString};

use crate::app::AppState;
use crate::settings::Density;

/// The settings categories, in nav order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    Appearance,
    Behavior,
    About,
}

impl SettingsTab {
    pub(crate) const ALL: [SettingsTab; 3] = [
        SettingsTab::Appearance,
        SettingsTab::Behavior,
        SettingsTab::About,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Behavior => "Behavior",
            SettingsTab::About => "About",
        }
    }
}

impl AppState {
    /// The settings panel: a scrim over the app, a fixed left nav, and the page
    /// for the selected category with a Done footer.
    pub(crate) fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let page = settings_page(self.settings_tab, self, cx);

        let theme = cx.theme().clone();

        let mut nav = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(184.))
            .p_2()
            .gap_0p5()
            .bg(theme.bg_panel)
            .border_r_1()
            .border_color(theme.border_soft)
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .pb_2()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_faint)
                    .child("SETTINGS"),
            );
        for tab in SettingsTab::ALL {
            nav = nav.child(settings_nav_item(tab, self.settings_tab, &theme, cx));
        }

        let content = div()
            .flex_1()
            .flex()
            .flex_col()
            .min_w_0()
            .child(
                div()
                    .id("settings-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_6()
                    .py_5()
                    .child(page),
            )
            .child(settings_footer(cx));

        // The scrim closes on a backdrop click; `occlude` keeps clicks off the app
        // beneath, and the card stops its own clicks from reaching the scrim.
        div()
            .id("settings")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.55))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_settings(cx)),
            )
            .child(
                div()
                    .occlude()
                    .flex()
                    .w(px(720.))
                    .h(gpui::relative(0.82))
                    .bg(theme.bg_elevated)
                    .border_1()
                    .border_color(theme.border_strong)
                    .rounded(px(10.))
                    .shadow_lg()
                    .overflow_hidden()
                    .child(nav)
                    .child(content),
            )
    }
}

/// One left-nav row in the settings panel.
fn settings_nav_item(
    tab: SettingsTab,
    active: SettingsTab,
    theme: &Theme,
    cx: &mut Context<AppState>,
) -> impl IntoElement {
    let is_active = tab == active;
    div()
        .id(SharedString::from(format!("settings-nav-{}", tab.label())))
        .flex()
        .items_center()
        .px_3()
        .py_1p5()
        .rounded(theme.radius_sm)
        .text_sm()
        .cursor_pointer()
        .when(is_active, |d| d.bg(theme.bg_active).text_color(theme.text))
        .when(!is_active, |d| {
            d.text_color(theme.text_muted)
                .hover(|s| s.bg(theme.bg_hover).text_color(theme.text))
        })
        .on_click(cx.listener(move |this, _, _, cx| this.set_settings_tab(tab, cx)))
        .child(tab.label())
}

/// The content page for the selected settings category.
fn settings_page(tab: SettingsTab, state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();
    match tab {
        SettingsTab::Appearance => {
            // `Segmented::on_select` takes a plain `App` closure (not a listener),
            // so route through the entity handle like the rest of Flint's controls.
            let view = cx.entity();
            let density = Segmented::new("set-density")
                .segment("Compact")
                .segment("Comfortable")
                .segment("Spacious")
                .selected(state.settings.density().index())
                .on_select(move |ix, _, cx| {
                    view.update(cx, |this, cx| this.set_density(Density::ALL[ix.min(2)], cx));
                });
            settings_page_scaffold(
                "Appearance",
                div()
                    .flex()
                    .flex_col()
                    .child(setting_block(
                        "Theme",
                        "Switch the color theme.",
                        theme_manager(cx),
                        &theme,
                    ))
                    .child(settings_header("Result grid", &theme))
                    .child(setting_row(
                        "Row density",
                        "Vertical spacing of rows in the result grid.",
                        density,
                        &theme,
                    )),
                &theme,
            )
            .into_any_element()
        }
        SettingsTab::Behavior => {
            let confirm = Toggle::new("set-confirm", state.settings.confirm_destructive).on_change(
                cx.listener(|this, on: &bool, _, cx| this.set_confirm_destructive(*on, cx)),
            );
            settings_page_scaffold(
                "Behavior",
                div()
                    .flex()
                    .flex_col()
                    .child(settings_header("Safety", &theme))
                    .child(setting_row(
                        "Confirm destructive statements",
                        "Ask before running DROP / TRUNCATE / DELETE-without-WHERE and similar.",
                        confirm,
                        &theme,
                    )),
                &theme,
            )
            .into_any_element()
        }
        SettingsTab::About => settings_page_scaffold(
            "About",
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .pb_2()
                        .text_sm()
                        .text_color(theme.text_muted)
                        .child("RED — Roughly Enough Data, a fast, native database explorer."),
                )
                .child(settings_header("Build", &theme))
                .child(setting_row(
                    "Version",
                    "The installed application version.",
                    div()
                        .text_sm()
                        .text_color(theme.text_muted)
                        .child(SharedString::from(env!("CARGO_PKG_VERSION"))),
                    &theme,
                )),
            &theme,
        )
        .into_any_element(),
    }
}

/// The theme picker: each built-in theme as a clickable row, the active one ticked.
fn theme_manager(cx: &mut Context<AppState>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let active = cx.theme().name.clone();

    let mut list = div().flex().flex_col().gap_0p5();
    for (ix, name) in crate::theme::THEMES.iter().enumerate() {
        list = list.child(theme_row(ix, name, &active, &theme, cx));
    }
    list
}

/// One row in the theme picker: click the name to activate it.
fn theme_row(
    ix: usize,
    name: &'static str,
    active: &str,
    theme: &Theme,
    cx: &mut Context<AppState>,
) -> impl IntoElement {
    let is_active = name == active;
    let check: AnyElement = if is_active {
        crate::icons::icon("check", px(14.), theme.accent).into_any_element()
    } else {
        div().size(px(14.)).into_any_element()
    };

    div()
        .id(SharedString::from(format!("theme-pick-{ix}")))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded(theme.radius_sm)
        .cursor_pointer()
        .when(is_active, |d| d.bg(theme.bg_active))
        .when(!is_active, |d| d.hover(|s| s.bg(theme.bg_hover)))
        .on_click(cx.listener(move |this, _, _, cx| this.select_theme(name, cx)))
        .child(check)
        .child(div().text_sm().text_color(theme.text).child(name))
}

/// A page heading above its sections.
fn settings_page_scaffold(title: &str, body: impl IntoElement, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .pb_2()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(SharedString::from(title.to_string())),
        )
        .child(body)
}

/// The settings panel's footer: the Done button that closes it.
fn settings_footer(cx: &mut Context<AppState>) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex()
        .items_center()
        .justify_end()
        .gap_2p5()
        .px_4()
        .py_3()
        .border_t_1()
        .border_color(theme.border_soft)
        .bg(theme.bg_bar)
        .child(
            Button::new("set-done", "Done")
                .variant(ButtonVariant::Primary)
                .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx))),
        )
}

/// A small uppercase section header above a group of settings rows.
fn settings_header(title: &str, theme: &Theme) -> impl IntoElement {
    div()
        .pt_4()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text_faint)
        .child(SharedString::from(title.to_uppercase()))
}

/// One settings row: a title + description on the left, a control on the right,
/// with a hairline divider beneath.
fn setting_row(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: impl IntoElement,
    theme: &Theme,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_6()
        .py_3()
        .border_b_1()
        .border_color(theme.border_soft)
        .child(setting_label(title, description, theme))
        .child(div().flex_shrink_0().child(control))
}

/// A full-width settings entry: title/description, then a block of content below
/// (for controls too large to sit on the right, like the theme list).
fn setting_block(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    body: impl IntoElement,
    theme: &Theme,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .py_3()
        .border_b_1()
        .border_color(theme.border_soft)
        .child(setting_label(title, description, theme))
        .child(body)
}

/// The stacked title + description used by both settings-row shapes.
fn setting_label(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    theme: &Theme,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_0p5()
        .min_w_0()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(title.into()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.text_muted)
                .child(description.into()),
        )
}
