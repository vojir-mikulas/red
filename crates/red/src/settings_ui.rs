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
use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};

use crate::app::{AppState, FontSelect};
use crate::settings::{Density, ThemeMode};

/// The settings categories, in nav order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    Appearance,
    Grid,
    Query,
    Behavior,
    About,
}

impl SettingsTab {
    pub(crate) const ALL: [SettingsTab; 5] = [
        SettingsTab::Appearance,
        SettingsTab::Grid,
        SettingsTab::Query,
        SettingsTab::Behavior,
        SettingsTab::About,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Grid => "Result grid",
            SettingsTab::Query => "Query",
            SettingsTab::Behavior => "Behavior",
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
                    .text_size(theme.scale(12.))
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

        // The scrim closes on a backdrop click; `occlude` keeps clicks off the
        // app beneath, and the card stops its own clicks from reaching the scrim.
        // `on_scrim_dismiss` ignores the click that ends a window drag, so moving
        // the window from behind the panel doesn't close it.
        let view = cx.entity().downgrade();
        div()
            .id("settings")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.55))
            .occlude()
            .on_scrim_dismiss(move |_, cx| {
                view.update(cx, |this, cx| this.close_settings(cx)).ok();
            })
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
        .child(crate::icons::icon("lock", theme.scale(13.), theme.yellow))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(theme.scale(12.))
                .text_color(theme.text_muted)
                .child(SharedString::from(message)),
        )
        .child(
            div()
                .id("settings-warn-dismiss")
                .flex_shrink_0()
                .cursor_pointer()
                .text_size(theme.scale(12.))
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
        .text_size(theme.scale(14.))
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
        SettingsTab::Behavior => behavior_page(state, cx),
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
                theme_picker(state, true),
                &theme,
            ))
            .child(setting_row(
                "Dark theme",
                "Used in Dark mode, and in System mode on a dark OS.",
                theme_picker(state, false),
                &theme,
            ))
            .child(settings_header("Typography", &theme))
            .child(setting_row(
                "Interface font",
                "The sans font for chrome — toolbars, tabs, sidebars, status bar, menus.",
                font_picker(state, FontSelect::Ui),
                &theme,
            ))
            .child(setting_row(
                "Interface mono font",
                "The font for in-UI data — result grid cells and schema identifiers. \
                 Shares the interface font size; match it to the interface font for a \
                 uniform look.",
                font_picker(state, FontSelect::UiMono),
                &theme,
            ))
            .child(setting_row(
                "Interface font size",
                "Base interface text size, in pixels. Both interface fonts scale from this.",
                state.ui_font_size_input.clone(),
                &theme,
            ))
            .child(setting_row(
                "Editor font",
                "The SQL editor font — independent of the interface. A monospace face is \
                 recommended.",
                font_picker(state, FontSelect::Editor),
                &theme,
            ))
            .child(setting_row(
                "Editor font size",
                "SQL editor text size, in pixels. Independent of the interface size.",
                state.editor_font_size_input.clone(),
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

/// The searchable dropdown for one theme family (light or dark). The combo box is
/// a long-lived entity on [`AppState`]; its options + selection are filled by
/// `AppState::rebuild_settings_pickers` when the panel opens, and it routes the
/// chosen theme name to `set_light_theme` / `set_dark_theme`.
fn theme_picker(state: &AppState, light: bool) -> impl IntoElement {
    if light {
        state.theme_combo_light.clone()
    } else {
        state.theme_combo_dark.clone()
    }
}

/// The searchable dropdown for one font slot (UI sans / UI mono / editor). Mirrors
/// [`theme_picker`]: a long-lived combo box on [`AppState`], filled on panel open.
fn font_picker(state: &AppState, which: FontSelect) -> impl IntoElement {
    match which {
        FontSelect::Ui => state.font_combo_ui.clone(),
        FontSelect::UiMono => state.font_combo_ui_mono.clone(),
        FontSelect::Editor => state.font_combo_editor.clone(),
    }
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
                .text_size(theme.scale(14.))
                .text_color(theme.text)
                .child(name_owned.clone()),
        )
        .child(
            div()
                .text_size(theme.scale(12.))
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
                .child(crate::icons::icon(
                    "trash",
                    theme.scale(13.),
                    theme.text_faint,
                ))
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

    let row_numbers = Toggle::new("set-row-numbers", state.settings.grid.row_numbers)
        .on_change(cx.listener(|this, on: &bool, _, cx| this.set_row_numbers(*on, cx)));

    // Page-size presets; a custom value (set in the file) shows none selected.
    const PAGE_PRESETS: [usize; 4] = [100, 200, 500, 1000];
    let page_sel = PAGE_PRESETS
        .iter()
        .position(|&n| n == state.settings.grid.page_size)
        .unwrap_or(usize::MAX);
    let page_view = view.clone();
    let page_size = Segmented::new("set-page-size")
        .segment("100")
        .segment("200")
        .segment("500")
        .segment("1000")
        .selected(page_sel)
        .on_select(move |ix, _, cx| {
            let n = PAGE_PRESETS[ix.min(PAGE_PRESETS.len() - 1)];
            page_view.update(cx, |this, cx| this.set_page_size(n, cx));
        });

    // Fat-cell cap presets, in bytes; a custom value shows none selected.
    const CELL_PRESETS: [usize; 4] = [1024, 4096, 16384, 65536];
    let cell_sel = CELL_PRESETS
        .iter()
        .position(|&n| n == state.settings.grid.max_cell_chars)
        .unwrap_or(usize::MAX);
    let cell_view = view.clone();
    let max_cell = Segmented::new("set-max-cell")
        .segment("1K")
        .segment("4K")
        .segment("16K")
        .segment("64K")
        .selected(cell_sel)
        .on_select(move |ix, _, cx| {
            let n = CELL_PRESETS[ix.min(CELL_PRESETS.len() - 1)];
            cell_view.update(cx, |this, cx| this.set_max_cell_chars(n, cx));
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
            .child(setting_row(
                "Row numbers",
                "Show the leading row-number gutter.",
                row_numbers,
                &theme,
            ))
            .child(settings_header("Performance", &theme))
            .child(setting_row(
                "Page size",
                "Rows fetched per page as you scroll. Larger means fewer round-trips, \
                 more resident rows.",
                page_size,
                &theme,
            ))
            .child(setting_row(
                "Max cell size",
                "Bytes of a single cell kept resident — the fat-cell memory rail. \
                 Over-cap cells are clipped for display only; export stays full.",
                max_cell,
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

    // Statement-timeout presets, in seconds (0 = off); a custom value shows none.
    const TIMEOUT_PRESETS: [u32; 4] = [0, 10, 30, 60];
    let timeout_sel = TIMEOUT_PRESETS
        .iter()
        .position(|&n| n == state.settings.query.statement_timeout)
        .unwrap_or(usize::MAX);
    let timeout_view = view.clone();
    let statement_timeout = Segmented::new("set-statement-timeout")
        .segment("Off")
        .segment("10s")
        .segment("30s")
        .segment("60s")
        .selected(timeout_sel)
        .on_select(move |ix, _, cx| {
            let n = TIMEOUT_PRESETS[ix.min(TIMEOUT_PRESETS.len() - 1)];
            timeout_view.update(cx, |this, cx| this.set_statement_timeout(n, cx));
        });

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
            .child(setting_row(
                "Statement timeout",
                "Abort a query — and its page/run fetches — that runs longer than this.",
                statement_timeout,
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

fn behavior_page(state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();

    let restore = Toggle::new(
        "set-restore-last-session",
        state.settings.behavior.restore_last_session,
    )
    .on_change(cx.listener(|this, on: &bool, _, cx| this.set_restore_last_session(*on, cx)));

    settings_page_scaffold(
        "Behavior",
        div().flex().flex_col().child(setting_row(
            "Restore last session",
            "Reconnect to the most recently used connection on launch (credentials \
             come from the keychain). Takes effect next launch.",
            restore,
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
                    .text_size(theme.scale(14.))
                    .text_color(theme.text_muted)
                    .child("Red — Roughly Enough Data, a fast, native database explorer."),
            )
            .child(settings_header("Build", &theme))
            .child(setting_row(
                "Version",
                "The installed application version.",
                div()
                    .text_size(theme.scale(14.))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(env!("CARGO_PKG_VERSION"))),
                &theme,
            )),
        &theme,
    )
    .into_any_element()
}

/// A page heading above its sections.
fn settings_page_scaffold(title: &str, body: impl IntoElement, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .pb_2()
                .text_size(theme.scale(18.))
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
        .text_size(theme.scale(12.))
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
                .text_size(theme.scale(14.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(title.into()),
        )
        .child(
            div()
                .text_size(theme.scale(12.))
                .text_color(theme.text_muted)
                .child(description.into()),
        )
}
