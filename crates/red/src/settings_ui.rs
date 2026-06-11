//! The settings panel — a Zed-style left category nav beside a scrolling content
//! pane, ported from Nyx's `settings_modal`. Built as a custom scrim + card (not
//! the shared `Modal`) so the nav stays fixed while only the page scrolls. Pure
//! assembly over Flint components; the state + actions live on [`AppState`]
//! (`app.rs`), persisted through [`crate::settings`].
//!
//! The panel is the convenience surface; the *file* (opened from the footer or the
//! command palette) is the full, documented config — the Zed-spirit primary path.

use flint::prelude::*;
use flint::Theme;
use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, MouseButton, SharedString};

use crate::app::AppState;
use crate::settings::{Density, ThemeMode};

/// The settings categories, in nav order. Editor-font and behavior knobs live in
/// the file (see the footer's "Open settings file"), so they get no panel tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    Appearance,
    Grid,
    Query,
    About,
}

impl SettingsTab {
    pub(crate) const ALL: [SettingsTab; 4] = [
        SettingsTab::Appearance,
        SettingsTab::Grid,
        SettingsTab::Query,
        SettingsTab::About,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Grid => "Result grid",
            SettingsTab::Query => "Query",
            SettingsTab::About => "About",
        }
    }
}

impl AppState {
    /// The settings panel: a scrim over the app, a fixed left nav, an optional
    /// warning banner, the page for the selected category, and a footer.
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

        let banner =
            (!self.settings_warnings.is_empty()).then(|| settings_banner(self, &theme, cx));

        let content = div()
            .flex_1()
            .flex()
            .flex_col()
            .min_w_0()
            .children(banner)
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

/// The non-blocking diagnostics banner: surfaces a bad hand-edit (an unreadable
/// section, a malformed file) so the user gets feedback instead of a silent reset,
/// while last-good defaults stay applied. Dismissible.
fn settings_banner(
    state: &AppState,
    theme: &Theme,
    cx: &mut Context<AppState>,
) -> impl IntoElement {
    let message = state.settings_warnings.join("  •  ");
    div()
        .flex()
        .items_start()
        .gap_2()
        .px_4()
        .py_2()
        .bg(theme.yellow.opacity(0.1))
        .border_b_1()
        .border_color(theme.border_soft)
        .child(crate::icons::icon("lock", px(13.), theme.yellow))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(theme.text_muted)
                .child(SharedString::from(message)),
        )
        .child(
            div()
                .id("settings-warn-dismiss")
                .flex_shrink_0()
                .cursor_pointer()
                .text_xs()
                .text_color(theme.text_faint)
                .hover(|s| s.text_color(theme.text))
                .on_click(cx.listener(|this, _, _, cx| this.dismiss_settings_warnings(cx)))
                .child("Dismiss"),
        )
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
    match tab {
        SettingsTab::Appearance => appearance_page(state, cx),
        SettingsTab::Grid => grid_page(state, cx),
        SettingsTab::Query => query_page(state, cx),
        SettingsTab::About => about_page(cx),
    }
}

fn appearance_page(state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();
    let view = cx.entity();

    // System / Light / Dark — how the theme tracks the OS appearance.
    let mode_sel = match state.theme_mode() {
        ThemeMode::System => 0,
        ThemeMode::Light => 1,
        ThemeMode::Dark => 2,
    };
    let mode_view = view.clone();
    let mode_seg = Segmented::new("set-theme-mode")
        .segment("System")
        .segment("Light")
        .segment("Dark")
        .selected(mode_sel)
        .on_select(move |ix, _, cx| {
            let mode = match ix {
                0 => ThemeMode::System,
                1 => ThemeMode::Light,
                _ => ThemeMode::Dark,
            };
            mode_view.update(cx, |this, cx| this.set_theme_mode(mode, cx));
        });

    settings_page_scaffold(
        "Appearance",
        div()
            .flex()
            .flex_col()
            .child(setting_row(
                "Match appearance",
                "Follow the OS light/dark setting, or pin a single mode.",
                mode_seg,
                &theme,
            ))
            .child(setting_row(
                "Light theme",
                "Used in Light mode, and in System mode on a light OS.",
                theme_picker(state, true, cx),
                &theme,
            ))
            .child(setting_row(
                "Dark theme",
                "Used in Dark mode, and in System mode on a dark OS.",
                theme_picker(state, false, cx),
                &theme,
            ))
            .child(setting_block(
                "Manage themes",
                "Import theme files (.toml), or remove ones you've added.",
                theme_manager(state, cx),
                &theme,
            )),
        &theme,
    )
    .into_any_element()
}

/// A dropdown of the themes of one family (light or dark), selecting the active
/// one for that slot. The panel owns the open flag on [`AppState`].
fn theme_picker(state: &AppState, light: bool, cx: &mut Context<AppState>) -> impl IntoElement {
    let which = if light {
        crate::app::ThemeSelect::Light
    } else {
        crate::app::ThemeSelect::Dark
    };
    let names = state.themes.names(light);
    let current = state.selected_theme(light);
    let selected = names
        .iter()
        .position(|n| *n == current)
        .unwrap_or(usize::MAX);

    let mut select = Select::new(if light { "pick-light" } else { "pick-dark" })
        .placeholder("Select a theme…")
        .selected(selected)
        .open(state.theme_select_open == Some(which));
    for name in &names {
        select = select.option(name.clone());
    }

    let toggle_view = cx.entity();
    let pick_view = cx.entity();
    let pick_names = names.clone();
    select
        .on_toggle(move |_, cx| {
            toggle_view.update(cx, |this, cx| this.toggle_theme_select(which, cx));
        })
        .on_select(move |ix, _, cx| {
            if let Some(name) = pick_names.get(ix).cloned() {
                pick_view.update(cx, |this, cx| {
                    if light {
                        this.set_light_theme(&name, cx)
                    } else {
                        this.set_dark_theme(&name, cx)
                    }
                });
            }
        })
}

/// The theme manager: an Import button, then every theme as a row — built-ins
/// tagged by family, imported ones with a trash button to remove them.
fn theme_manager(state: &AppState, cx: &mut Context<AppState>) -> impl IntoElement {
    let theme = cx.theme().clone();

    let mut list = div().flex().flex_col().gap_0p5().child(
        div().pb_1().child(
            Button::new("theme-import", "Import theme…")
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Sm)
                .on_click(cx.listener(|this, _, _, cx| this.import_theme(cx))),
        ),
    );
    for entry in state.themes.entries() {
        list = list.child(theme_manage_row(
            &entry.name,
            entry.is_light,
            entry.user,
            &theme,
            cx,
        ));
    }
    list
}

/// One row in the theme manager: name + family tag, and a remove button for an
/// imported theme.
fn theme_manage_row(
    name: &str,
    is_light: bool,
    user: bool,
    theme: &Theme,
    cx: &mut Context<AppState>,
) -> impl IntoElement {
    let name_owned = name.to_string();
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded(theme.radius_sm)
        .child(
            div()
                .flex_1()
                .text_sm()
                .text_color(theme.text)
                .child(name_owned.clone()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.text_faint)
                .child(if is_light { "light" } else { "dark" }),
        )
        .child(if user {
            div()
                .id(SharedString::from(format!("theme-rm-{name}")))
                .flex()
                .items_center()
                .justify_center()
                .size(px(20.))
                .rounded(theme.radius_sm)
                .cursor_pointer()
                .text_color(theme.text_faint)
                .hover(|s| s.bg(theme.bg_hover).text_color(theme.red))
                .on_click(cx.listener(move |this, _, _, cx| this.remove_theme(&name_owned, cx)))
                .child(crate::icons::icon("trash", px(13.), theme.text_faint))
                .into_any_element()
        } else {
            div().w(px(20.)).into_any_element()
        })
}

fn grid_page(state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();
    let view = cx.entity();

    let density_view = view.clone();
    let density = Segmented::new("set-density")
        .segment("Compact")
        .segment("Comfortable")
        .segment("Spacious")
        .selected(state.settings.grid.density.index())
        .on_select(move |ix, _, cx| {
            density_view.update(cx, |this, cx| this.set_density(Density::from_index(ix), cx));
        });

    // NULL / ∅ / blank presets. A custom value (set in the file) shows as NULL
    // here; the file is the place for an arbitrary string.
    let null_sel = match state.settings.grid.null_display.as_str() {
        "∅" => 1,
        "" => 2,
        _ => 0,
    };
    let null_view = view.clone();
    let null_display = Segmented::new("set-null")
        .segment("NULL")
        .segment("∅")
        .segment("blank")
        .selected(null_sel)
        .on_select(move |ix, _, cx| {
            let value = match ix {
                1 => "∅",
                2 => "",
                _ => "NULL",
            };
            null_view.update(cx, |this, cx| this.set_null_display(value, cx));
        });

    settings_page_scaffold(
        "Result grid",
        div()
            .flex()
            .flex_col()
            .child(setting_row(
                "Row density",
                "Vertical spacing of rows in the result grid.",
                density,
                &theme,
            ))
            .child(setting_row(
                "Null display",
                "How a SQL NULL renders in a cell.",
                null_display,
                &theme,
            ))
            .child(file_hint(
                "Row numbers, page size, and the fat-cell cap",
                &theme,
            )),
        &theme,
    )
    .into_any_element()
}

fn query_page(state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();
    let view = cx.entity();

    let limit_sel = match state.settings.query.auto_limit {
        0 => 0,
        100 => 1,
        10000 => 3,
        _ => 2,
    };
    let limit_view = view.clone();
    let auto_limit = Segmented::new("set-auto-limit")
        .segment("Off")
        .segment("100")
        .segment("1000")
        .segment("10000")
        .selected(limit_sel)
        .on_select(move |ix, _, cx| {
            let n = [0u32, 100, 1000, 10000][ix.min(3)];
            limit_view.update(cx, |this, cx| this.set_auto_limit(n, cx));
        });

    let confirm = Toggle::new("set-confirm", state.settings.query.confirm_destructive)
        .on_change(cx.listener(|this, on: &bool, _, cx| this.set_confirm_destructive(*on, cx)));

    settings_page_scaffold(
        "Query",
        div()
            .flex()
            .flex_col()
            .child(settings_header("Large-result safety", &theme))
            .child(setting_row(
                "Auto-limit",
                "Append LIMIT to a bare SELECT * so a fat table can't flood the grid.",
                auto_limit,
                &theme,
            ))
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

fn about_page(cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();
    settings_page_scaffold(
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
    .into_any_element()
}

/// A muted note that a knob lives in the settings file (the documented surface),
/// keeping the panel honest about what it doesn't expose.
fn file_hint(what: &str, theme: &Theme) -> impl IntoElement {
    div()
        .py_3()
        .text_xs()
        .text_color(theme.text_faint)
        .child(SharedString::from(format!(
            "{what} are configurable in settings.toml (Open settings file)."
        )))
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

/// The settings panel's footer: "Open settings file" beside the Done button.
fn settings_footer(cx: &mut Context<AppState>) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex()
        .items_center()
        .gap_2p5()
        .px_4()
        .py_3()
        .border_t_1()
        .border_color(theme.border_soft)
        .bg(theme.bg_bar)
        .child(
            Button::new("set-open-file", "Open settings file")
                .variant(ButtonVariant::Ghost)
                .on_click(cx.listener(|this, _, _, cx| this.open_settings_file(cx))),
        )
        .child(div().flex_1())
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
