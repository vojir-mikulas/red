//! The settings panel: a Zed-style left category nav beside a scrolling content
//! pane, ported from Nyx's `settings_modal`. Built as a custom scrim + card (not
//! the shared `Modal`) so the nav stays fixed while only the page scrolls. Pure
//! assembly over Flint components; the state + actions live on [`AppState`]
//! (`app.rs`), persisted through [`crate::settings`].
//!
//! The panel is the convenience surface; the *file* (opened from the footer or the
//! command palette) is the full, documented config, the Zed-spirit primary path.

use flint::Theme;
use flint::prelude::*;
use gpui::{AnyElement, Context, FontWeight, SharedString, canvas, div, prelude::*, px};

use crate::app::{AppState, FontSelect};
use crate::settings::ThemeMode;
use crate::settings_reg::{self, Control, SettingDef, SettingsTab, Value};

// --- type scale ---------------------------------------------------------------
//
// Six steps, and nothing between them. The panel had drifted to nine sizes
// (10 / 10.5 / 11 / 11.5 / 12 / 12.5 / 13 / 14 / 18) with half-steps used
// interchangeably for the same role — a status line at 10.5 beside one at 11 —
// which reads as sloppy rather than as hierarchy. Every size below is one of
// these, and `theme.scale` still applies the user's UI font size on top.

/// Page titles.
const TEXT_TITLE: f32 = 18.;
/// A settings row's label, a nav category, a read-only value: the panel's
/// primary text.
const TEXT_ROW: f32 = 14.;
/// Inputs and small controls, matching the app's text-field scale.
const TEXT_CONTROL: f32 = 13.;
/// Descriptions, section and band headers, and any explanatory prose.
const TEXT_BODY: f32 = 12.;
/// Tertiary detail: file keys, paths, status lines, inline notes.
const TEXT_MINOR: f32 = 11.;
/// Uppercase chips (CUSTOM, the seam badges).
const TEXT_BADGE: f32 = 10.;

/// The Appearance-tab controls that can sit below the fold and so are scrolled
/// into view when Tab focuses them: the five dropdowns and the two font-size
/// inputs (the controls with a queryable focus handle). See
/// [`AppState::update_settings_scroll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevealTarget {
    ThemeLight,
    ThemeDark,
    FontUi,
    FontUiMono,
    FontEditor,
    UiSize,
    EditorSize,
}

impl AppState {
    /// The settings panel: a scrim over the app, a fixed left nav, an optional
    /// warning banner, the page for the selected category, and a footer.
    pub(crate) fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();

        // A live search over every registered setting wins over the selected
        // page: with eleven pages, "where is that option" is answered by typing,
        // not by remembering which category it was filed under.
        let query = self
            .settings_search
            .read(cx)
            .content()
            .trim()
            .to_lowercase();
        let page = if query.is_empty() {
            settings_page(self.settings_tab, self, cx)
        } else {
            search_results(&query, self, cx)
        };

        // The category list scrolls independently of the search box above it: at
        // eleven pages it already outgrows a short window, and a clipped last
        // category (About) is unreachable rather than merely cut off.
        let mut list = div()
            .id("settings-nav")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_0p5();
        // Bands ("App", "Data", "Policy", "System") are emitted as each one's
        // first page comes up, so the order lives in `SettingsTab::ALL` alone.
        let mut band = "";
        for tab in SettingsTab::ALL {
            if tab.band() != band {
                band = tab.band();
                list = list.child(nav_band_header(band, &theme));
            }
            list = list.child(settings_nav_item(tab, self.settings_tab, &theme, cx));
        }

        let nav = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(184.))
            .p_2()
            .gap_1()
            .bg(theme.bg_panel)
            // The nav's own fill spans the card's rounded left edge; round its
            // left corners to match, or its square corners paint through them
            // (the card's `overflow_hidden` clips content, not child bg corners).
            .rounded_tl(px(10.))
            .rounded_bl(px(10.))
            .border_r_1()
            .border_color(theme.border_soft)
            .child(settings_search_box(self, &theme))
            .child(list);

        let banner = (!self.settings_warnings.is_empty() || !self.keymap_warnings.is_empty())
            .then(|| settings_banner(self, &theme, cx));

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
                    .track_scroll(&self.settings_scroll)
                    .px_6()
                    .py_5()
                    .child(page),
            )
            .child(settings_footer(cx));

        // The scrim closes on a backdrop click; `occlude` keeps clicks off the
        // app beneath, and the card stops its own clicks from reaching the scrim.
        // `on_scrim_dismiss` ignores the click that ends a window drag, so moving
        // the window from behind the panel doesn't close it.
        //
        // Keyboard handling rides on the scrim (the ancestor of every control), so
        // it fires whether the panel root or a focused child holds focus: Esc
        // closes, and Tab/Shift-Tab cycle the panel's controls (the `Modal` context
        // is shared with Flint's `Modal`; its bindings are registered once at
        // startup). The focus trap on `modal_focus` keeps Tab from escaping to the
        // chrome behind the scrim (see `app::render`).
        let view = cx.entity().downgrade();
        let modal_focus = self.modal_focus.clone();
        div()
            .id("settings")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.55))
            .occlude()
            .track_focus(&modal_focus)
            .key_context("Modal")
            .on_action(|_: &flint::components::modal::FocusNext, window, cx| window.focus_next(cx))
            .on_action(|_: &flint::components::modal::FocusPrev, window, cx| window.focus_prev(cx))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                // While the keymap recorder is live it owns the keyboard (its
                // interceptor ran first and stopped propagation, but this listener
                // fires regardless), so stand down; Esc/Enter there cancel/confirm
                // the capture, not close the panel.
                if this.keymap_intercept.is_some() {
                    cx.stop_propagation();
                    return;
                }
                // Likewise while a confirmation is layered over the panel (the
                // Behavior page's "Remove all RED data"): Esc there means back out
                // of the confirmation, not close the panel underneath it, and the
                // panel has to still be there to go back to.
                if this.confirm_reset {
                    return;
                }
                match event.keystroke.key.as_str() {
                    // Esc is one of the two ways out (the other is the Done button).
                    "escape" => {
                        this.close_settings(cx);
                        cx.stop_propagation();
                    }
                    // Swallow a bare Enter so it can't fall through to a background
                    // action (e.g. the welcome screen's Enter-to-connect) or read as
                    // a "confirm" that dismisses the panel; closing stays explicit.
                    // A focused control still activates on Enter (it handles the key
                    // first, before it bubbles here).
                    "enter" => cx.stop_propagation(),
                    _ => {}
                }
            }))
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
) -> impl IntoElement + use<> {
    let message = state
        .settings_warnings
        .iter()
        .chain(&state.keymap_warnings)
        .cloned()
        .collect::<Vec<_>>()
        .join("  •  ");
    let focus_ring = theme.accent;
    div()
        .flex()
        .items_start()
        .gap_2()
        .px_4()
        .py_2()
        .bg(theme.yellow.opacity(0.1))
        // Sits at the card's top-right (the nav covers the top-left); round it so
        // its tinted fill doesn't square off that corner.
        .rounded_tr(px(10.))
        .border_b_1()
        .border_color(theme.border_soft)
        .child(crate::icons::icon(
            "lock",
            theme.scale(TEXT_CONTROL),
            theme.yellow,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(theme.scale(TEXT_BODY))
                .text_color(theme.text_muted)
                .child(SharedString::from(message)),
        )
        .child(
            div()
                .id("settings-warn-dismiss")
                .flex_shrink_0()
                .px_1()
                .rounded(theme.radius_sm)
                .cursor_pointer()
                .text_size(theme.scale(TEXT_BODY))
                .text_color(theme.text_faint)
                .hover(|s| s.text_color(theme.text))
                // Focusable so Tab reaches it; Enter/Space fires the dismiss click.
                // Hand-rolled div, so name it for assistive tech like Flint's buttons.
                .role(gpui::Role::Button)
                .aria_label("Dismiss")
                .border_1()
                .border_color(gpui::transparent_black())
                .tab_index(0)
                .focus(move |s| s.border_color(focus_ring))
                .on_click(cx.listener(|this, _, _, cx| this.dismiss_settings_warnings(cx)))
                .child(crate::i18n::tr!("settings.ui_dismiss", "Dismiss")),
        )
}

/// The nav's search field. Filters every registered setting across every page,
/// so a user who remembers the *name* of a setting never has to guess which
/// category it lives under.
fn settings_search_box(state: &AppState, theme: &Theme) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_center()
        .flex_shrink_0()
        .gap_1p5()
        .h(px(30.))
        .px_2()
        .rounded(theme.radius_sm)
        .bg(theme.bg_input)
        .border_1()
        .border_color(theme.border)
        .text_size(theme.scale(TEXT_CONTROL))
        .text_color(theme.text)
        .child(crate::icons::icon(
            "search",
            theme.scale(TEXT_CONTROL),
            theme.text_faint,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(state.settings_search.clone()),
        )
}

/// A band label above a run of nav rows.
fn nav_band_header(band: &'static str, theme: &Theme) -> impl IntoElement + use<> {
    div()
        .px_2()
        .pt_2()
        .pb_1()
        .text_size(theme.scale(TEXT_MINOR))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text_faint)
        .child(band.to_uppercase())
}

/// One left-nav row in the settings panel.
fn settings_nav_item(
    tab: SettingsTab,
    active: SettingsTab,
    theme: &Theme,
    cx: &mut Context<AppState>,
) -> impl IntoElement + use<> {
    let is_active = tab == active;
    let focus_ring = theme.accent;
    div()
        .id(SharedString::from(format!("settings-nav-{}", tab.label())))
        // A category selector; expose it as a tab to assistive tech.
        .role(gpui::Role::Tab)
        .aria_label(tab.title())
        .flex()
        .items_center()
        .px_3()
        .py_1p5()
        .rounded(theme.radius_sm)
        .text_size(theme.scale(TEXT_ROW))
        .cursor_pointer()
        // A transparent border reserved in every state, so the focus ring colours
        // it in without nudging the row's size.
        .border_1()
        .border_color(gpui::transparent_black())
        .when(is_active, |d| d.bg(theme.bg_active).text_color(theme.text))
        .when(!is_active, |d| {
            d.text_color(theme.text_muted)
                .hover(|s| s.bg(theme.bg_hover).text_color(theme.text))
        })
        // Focusable so Tab reaches each category; GPUI fires the click on
        // Enter/Space, so the focused row becomes the selected tab.
        .tab_index(0)
        .focus(move |s| s.border_color(focus_ring))
        .on_click(cx.listener(move |this, _, _, cx| this.set_settings_tab(tab, cx)))
        .child(tab.title())
}

/// The content page for the selected settings category.
fn settings_page(tab: SettingsTab, state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    match tab {
        SettingsTab::Appearance => appearance_page(state, cx),
        SettingsTab::Editor => editor_page(state, cx),
        SettingsTab::Keymap => keymap_page(state, cx),
        SettingsTab::Behavior => behavior_page(state, cx),
        SettingsTab::Ai => ai_page(state, cx),
        SettingsTab::About => about_page(state, cx),
        // The seam + data + safety pages are pure registry: every row on them is
        // a value edit, so there's nothing to hand-assemble.
        tab => registry_page(tab, state, cx),
    }
}

// --- registry-driven rows ----------------------------------------------------

/// A whole page rendered from the registry: its groups in registry order, each
/// under its header.
fn registry_page(tab: SettingsTab, state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();
    let mut body = div().flex().flex_col();
    let mut group = "";
    let mut first = true;
    for def in settings_reg::for_tab(tab) {
        if def.group != group {
            group = def.group;
            // The first group's header is redundant under the page title unless
            // the page has more than one.
            if !(first
                && settings_reg::for_tab(tab)
                    .filter(|d| d.group != group)
                    .count()
                    == 0)
            {
                body = body.child(settings_header(&def.group_label(), &theme));
            }
        }
        first = false;
        body = body.child(registry_row(def, tab, state, &theme, cx));
    }
    page_scaffold(tab, body, &theme).into_any_element()
}

/// The registry rows of one group, for a page that also has hand-written parts.
fn registry_group(
    tab: SettingsTab,
    group: &'static str,
    state: &AppState,
    theme: &Theme,
    cx: &mut Context<AppState>,
) -> impl IntoElement + use<> {
    let mut body = div().flex().flex_col();
    for def in settings_reg::for_group(tab, group) {
        body = body.child(registry_row(def, tab, state, theme, cx));
    }
    body
}

/// One registry row: label + help on the left, the generated control on the
/// right, with the seam badge, the risk warning, and a Reset when it differs
/// from the shipped default.
fn registry_row(
    def: &'static SettingDef,
    tab: SettingsTab,
    state: &AppState,
    theme: &Theme,
    cx: &mut Context<AppState>,
) -> impl IntoElement + use<> {
    let modified = def.is_modified(&state.settings);
    let warn = def
        .warn
        .and_then(|f| f(&state.settings))
        .map(SharedString::from);
    let control = risky_control(def.key, warn, registry_control(def, state, cx), theme);

    // The badge is only informative where a page mixes scopes: on a seam page
    // every row is that seam, and repeating it would be noise. In search results
    // the page name is shown instead, which locates the row better.
    let badge = (tab == SettingsTab::Data)
        .then(|| def.applies.badge())
        .flatten();

    let label = labelled(def, badge, modified, theme);
    let reset = modified.then(|| reset_button(def, theme, cx));
    let row = div()
        .flex()
        .py_3()
        .border_b_1()
        .border_color(theme.border_soft);

    // A toggle is narrow enough to sit opposite its label; a row of segments is
    // not. Four segments ("Any write / Risky / Destructive / Never") eat most of
    // the content width, and a `justify_between` row answers that by crushing the
    // description into a column a few words wide. So: switches on the right,
    // choices on their own line under a full-width description. It's one rule
    // rather than a width threshold, which means it also holds at a larger UI
    // font size, where a threshold tuned at 13px would quietly stop working.
    match def.control {
        Control::Toggle => row
            .items_center()
            .justify_between()
            .gap_6()
            .child(label)
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(reset)
                    .child(control),
            ),
        Control::Segments(_) => row.flex_col().gap_2p5().child(label).child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(control)
                .children(reset),
        ),
    }
}

/// The left-hand column of a registry row: the label (with its optional scope
/// badge and Modified dot), the description, and the file key it maps to.
fn labelled(
    def: &'static SettingDef,
    badge: Option<&'static str>,
    modified: bool,
    theme: &Theme,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_col()
        .gap_0p5()
        .min_w_0()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_size(theme.scale(TEXT_ROW))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(def.label()),
                )
                .children(badge.map(|b| {
                    div()
                        .px_1p5()
                        .rounded(theme.radius_sm)
                        .bg(theme.bg_active)
                        .text_size(theme.scale(TEXT_BADGE))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text_faint)
                        .child(b)
                }))
                // A quiet dot, not a word: "this one isn't the default" is useful
                // at a glance and shouldn't compete with the label.
                .when(modified, |d| {
                    d.child(
                        div()
                            .id(SharedString::from(format!("modified-{}", def.key)))
                            .size(px(5.))
                            .rounded_full()
                            .bg(theme.accent)
                            .tooltip(flint::Tooltip::text("Changed from the default")),
                    )
                }),
        )
        .child(
            div()
                .text_size(theme.scale(TEXT_BODY))
                .text_color(theme.text_muted)
                .child(def.help()),
        )
        .child(
            div()
                .text_size(theme.scale(TEXT_MINOR))
                .text_color(theme.text_faint)
                .child(def.key),
        )
}

/// The per-row Reset: put this setting back to what RED ships with.
fn reset_button(
    def: &'static SettingDef,
    _theme: &Theme,
    cx: &mut Context<AppState>,
) -> impl IntoElement + use<> {
    Button::new(SharedString::from(format!("reset-{}", def.key)), "Reset")
        .variant(ButtonVariant::Ghost)
        .size(ButtonSize::Sm)
        .on_click(cx.listener(move |this, _, _, cx| {
            let value = def.default_value();
            this.edit_settings(cx, |s| (def.set)(s, &value));
        }))
}

/// Build the control a [`SettingDef`] asks for, wired to
/// [`AppState::edit_settings`] so every generated row goes through the one
/// mutation path (and so inherits its clamping, persistence, and effects).
fn registry_control(
    def: &'static SettingDef,
    state: &AppState,
    cx: &mut Context<AppState>,
) -> AnyElement {
    let current = (def.get)(&state.settings);
    match &def.control {
        Control::Toggle => Toggle::new(SharedString::from(def.key), current.as_bool())
            .on_change(cx.listener(move |this, on: &bool, _, cx| {
                let value = Value::Bool(*on);
                this.edit_settings(cx, |s| (def.set)(s, &value));
            }))
            .into_any_element(),
        Control::Segments(segments) => {
            // A value matching no preset (only reachable by hand-editing the file)
            // selects nothing rather than snapping to a neighbour.
            let selected = segments
                .iter()
                .position(|s| s.value == current)
                .unwrap_or(usize::MAX);
            let view = cx.entity();
            let mut control = Segmented::new(SharedString::from(def.key));
            for segment in *segments {
                control = control.segment(def.segment_label(segment));
            }
            control
                .selected(selected)
                .on_select(move |ix, _, cx| {
                    let Some(segment) = segments.get(ix) else {
                        return;
                    };
                    let value = segment.value.clone();
                    view.update(cx, |this, cx| {
                        this.edit_settings(cx, |s| (def.set)(s, &value));
                    });
                })
                .into_any_element()
        }
    }
}

/// The search view: every registered setting matching the query, grouped by the
/// page it lives on so a hit is also a direction ("it's under Key-value").
fn search_results(query: &str, state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();
    let hits: Vec<&'static SettingDef> = settings_reg::defs()
        .iter()
        .filter(|d| d.matches(query))
        .collect();

    let mut body = div().flex().flex_col();
    if hits.is_empty() {
        body = body.child(
            div()
                .py_4()
                .text_size(theme.scale(TEXT_BODY))
                .text_color(theme.text_faint)
                .child(crate::i18n::tr!(
                    "settings.ui_no_settings_match_your_search",
                    "No settings match your search."
                )),
        );
    }
    let mut tab = None;
    for def in hits {
        if tab != Some(def.tab) {
            tab = Some(def.tab);
            body = body.child(settings_header(&def.tab.title(), &theme));
        }
        // Always badge in search results: a row lifted out of its page needs to
        // say which seam it affects.
        body = body.child(registry_row(def, SettingsTab::Data, state, &theme, cx));
    }

    div()
        .flex()
        .flex_col()
        .child(
            div()
                .pb_2()
                .text_size(theme.scale(TEXT_TITLE))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(crate::i18n::tr!(
                    "settings.ui_search_results",
                    "Search results"
                )),
        )
        .child(body)
        .into_any_element()
}

fn appearance_page(state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();
    let view = cx.entity();

    // System / Light / Dark: how the theme tracks the OS appearance.
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
                "The sans font for chrome: toolbars, tabs, sidebars, status bar, menus.",
                font_picker(state, FontSelect::Ui),
                &theme,
            ))
            .child(setting_row(
                "Interface mono font",
                "The font for in-UI data: result grid cells and schema identifiers. \
                 Shares the interface font size; match it to the interface font for a \
                 uniform look.",
                font_picker(state, FontSelect::UiMono),
                &theme,
            ))
            .child(setting_row(
                "Interface font size",
                "Base interface text size, in pixels. Both interface fonts scale from this.",
                reveal_wrap(
                    state,
                    RevealTarget::UiSize,
                    state.ui_font_size_input.clone(),
                ),
                &theme,
            ))
            .child(settings_header("Motion", &theme))
            .child(registry_group(
                SettingsTab::Appearance,
                "Motion",
                state,
                &theme,
                cx,
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

/// The Editor tab: the code-editor's own typography (its font is independent of
/// the interface font, which is why it was confusing under Appearance) plus the
/// registry's layout rows.
fn editor_page(state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();
    settings_page_scaffold(
        "Editor",
        div()
            .flex()
            .flex_col()
            .child(setting_row(
                "Editor font",
                "The font for the SQL editor, the JSON editor, and the Redis console, \
                 independent of the interface. A monospace face is recommended.",
                font_picker(state, FontSelect::Editor),
                &theme,
            ))
            .child(setting_row(
                "Editor font size",
                "Editor text size, in pixels. Independent of the interface size.",
                reveal_wrap(
                    state,
                    RevealTarget::EditorSize,
                    state.editor_font_size_input.clone(),
                ),
                &theme,
            ))
            .child(settings_header("Layout", &theme))
            .child(registry_group(
                SettingsTab::Editor,
                "Layout",
                state,
                &theme,
                cx,
            )),
        &theme,
    )
    .into_any_element()
}

/// The searchable dropdown for one theme family (light or dark). The combo box is
/// a long-lived entity on [`AppState`]; its options + selection are filled by
/// `AppState::rebuild_settings_pickers` when the panel opens, and it routes the
/// chosen theme name to `set_light_theme` / `set_dark_theme`.
fn theme_picker(state: &AppState, light: bool) -> AnyElement {
    if light {
        reveal_wrap(
            state,
            RevealTarget::ThemeLight,
            state.theme_combo_light.clone(),
        )
    } else {
        reveal_wrap(
            state,
            RevealTarget::ThemeDark,
            state.theme_combo_dark.clone(),
        )
    }
}

/// The searchable dropdown for one font slot (UI sans / UI mono / editor). Mirrors
/// [`theme_picker`]: a long-lived combo box on [`AppState`], filled on panel open.
fn font_picker(state: &AppState, which: FontSelect) -> AnyElement {
    let (combo, target) = match which {
        FontSelect::Ui => (state.font_combo_ui.clone(), RevealTarget::FontUi),
        FontSelect::UiMono => (state.font_combo_ui_mono.clone(), RevealTarget::FontUiMono),
        FontSelect::Editor => (state.font_combo_editor.clone(), RevealTarget::FontEditor),
    };
    reveal_wrap(state, target, combo)
}

/// Wrap a reveal-able control so that, while it holds focus, an invisible canvas
/// overlay records its window-space bounds into [`AppState::settings_focus_box`].
/// The next render reads that to scroll the control into view (see
/// [`AppState::update_settings_scroll`]). Untracked controls render unwrapped, so
/// only the focused one pays for the overlay.
fn reveal_wrap(state: &AppState, target: RevealTarget, control: impl IntoElement) -> AnyElement {
    if state.settings_focused_reveal != Some(target) {
        return control.into_any_element();
    }
    let cell = state.settings_focus_box.clone();
    div()
        .relative()
        .child(control)
        .child(
            canvas(
                move |bounds, _, _| *cell.borrow_mut() = Some((target, bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .into_any_element()
}

/// The theme manager: an Import button, then every theme as a row (built-ins
/// tagged by family, imported ones with a trash button to remove them).
fn theme_manager(state: &AppState, cx: &mut Context<AppState>) -> impl IntoElement + use<> {
    let theme = cx.theme().clone();

    let mut list = div().flex().flex_col().gap_0p5().child(
        div().pb_1().child(
            Button::new(
                "theme-import",
                crate::i18n::tr!("settings.ui_import_theme", "Import theme…"),
            )
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
) -> impl IntoElement + use<> {
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
                .text_size(theme.scale(TEXT_ROW))
                .text_color(theme.text)
                .child(name_owned.clone()),
        )
        .child(
            div()
                .text_size(theme.scale(TEXT_BODY))
                .text_color(theme.text_faint)
                .child(if is_light { "light" } else { "dark" }),
        )
        .child(if user {
            let focus_ring = theme.accent;
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
                // Focusable so Tab reaches it; Enter/Space fires the remove click.
                .border_1()
                .border_color(gpui::transparent_black())
                .tab_index(0)
                .focus(move |s| s.border_color(focus_ring))
                .on_click(cx.listener(move |this, _, _, cx| this.remove_theme(&name_owned, cx)))
                .child(crate::icons::icon(
                    "trash",
                    theme.scale(TEXT_CONTROL),
                    theme.text_faint,
                ))
                .into_any_element()
        } else {
            div().w(px(20.)).into_any_element()
        })
}

/// The Keymap tab: a searchable list of every bindable action with its effective
/// shortcut, a per-row Rebind (capture the next chord) / Reset, and a Reset-all.
/// The effective bindings are read fresh from `keymap.toml` each render (see
/// [`AppState::keymap_slots`]) so a hand-edit and the tab stay in sync.
fn keymap_page(state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();
    let defs = crate::keymap::action_defs();
    let slots = state.keymap_slots();
    let query = state.keymap_search.read(cx).content().trim().to_lowercase();

    // The search box, styled like the welcome screen's connection filter.
    let search = div()
        .flex_1()
        .flex()
        .items_center()
        .gap_2()
        .h(px(34.))
        .px_2p5()
        .rounded(theme.radius)
        .bg(theme.bg_input)
        .border_1()
        .border_color(theme.border)
        .text_size(theme.scale(TEXT_CONTROL))
        .text_color(theme.text)
        .child(crate::icons::icon(
            "search",
            theme.scale(TEXT_CONTROL),
            theme.text_faint,
        ))
        .child(div().flex_1().min_w_0().child(state.keymap_search.clone()));

    let toolbar = div()
        .flex()
        .items_center()
        .gap_2()
        .mb_1()
        .child(search)
        .child(
            Button::new(
                "keymap-open-file",
                crate::i18n::tr!("settings.ui_open_keymap_file", "Open keymap file"),
            )
            .variant(ButtonVariant::Secondary)
            .size(ButtonSize::Sm)
            .on_click(cx.listener(|this, _, _, cx| this.open_keymap_file(cx))),
        )
        .child(
            Button::new(
                "keymap-reset-all",
                crate::i18n::tr!("settings.ui_reset_all", "Reset all"),
            )
            .variant(ButtonVariant::Ghost)
            .size(ButtonSize::Sm)
            .on_click(cx.listener(|this, _, _, cx| this.reset_all_keymap(cx))),
        );

    let mut list = div().flex().flex_col();
    let mut shown = 0usize;
    for (i, d) in defs.iter().enumerate() {
        let eff = slots[i].as_deref();
        // Filter by label or effective keystroke.
        if !query.is_empty() {
            let hit = d.label().to_lowercase().contains(&query)
                || eff.is_some_and(|k| k.to_lowercase().contains(&query));
            if !hit {
                continue;
            }
        }
        shown += 1;
        list = list.child(keymap_row(state, i, d, eff, &theme, cx));
    }
    if shown == 0 {
        list = list.child(
            div()
                .py_3()
                .text_size(theme.scale(TEXT_BODY))
                .text_color(theme.text_faint)
                .child(crate::i18n::tr!(
                    "settings.ui_no_actions_match_your_search",
                    "No actions match your search."
                )),
        );
    }

    settings_page_scaffold(
        "Keymap",
        div()
            .flex()
            .flex_col()
            .child(registry_group(
                SettingsTab::Keymap,
                "Navigation",
                state,
                &theme,
                cx,
            ))
            .child(
                div()
                    .pt_3()
                    .pb_2()
                    .text_size(theme.scale(TEXT_BODY))
                    .text_color(theme.text_muted)
                    .child(
                        "Rebind captures the next shortcut you press, even one already in use. \
                         The SQL editor, text-field, and dialog keys stay fixed.",
                    ),
            )
            .child(toolbar)
            .child(list),
        &theme,
    )
    .into_any_element()
}

/// One row of the Keymap tab: the action label + context on the left, and on the
/// right one of three states: the idle chip with Rebind/Reset, the live
/// "press a shortcut" affordance, or the captured-chord confirm (with a conflict
/// note when the chord is taken).
fn keymap_row(
    state: &AppState,
    row: usize,
    def: &crate::keymap::ActionDef,
    effective: Option<&str>,
    theme: &Theme,
    cx: &mut Context<AppState>,
) -> impl IntoElement + use<> {
    let recording = state.keymap_recording == Some(row);
    let pending = state.keymap_capture.as_ref().filter(|c| c.row == row);
    let is_customized = effective != Some(def.keystroke);

    // The right-hand control swaps with the row's state.
    let control = if recording {
        recording_affordance(theme, cx).into_any_element()
    } else if let Some(cap) = pending {
        capture_confirm(cap, theme, cx).into_any_element()
    } else {
        idle_control(row, effective, is_customized, theme, cx).into_any_element()
    };

    // The context a binding lives in, as a faint sub-label so a user knows where
    // it fires (globals show nothing; they fire everywhere).
    let context_note = def.context.map(|c| match c {
        "RedRoot" => "App",
        "Table" => "Result grid",
        other => other,
    });
    let mut label = div().flex().flex_col().gap_0p5().min_w_0().child(
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_size(theme.scale(TEXT_ROW))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(def.label()),
            )
            .when(is_customized && pending.is_none() && !recording, |d| {
                d.child(
                    div()
                        .px_1p5()
                        .rounded(theme.radius_sm)
                        .bg(theme.accent.opacity(0.14))
                        .text_size(theme.scale(TEXT_BADGE))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.accent)
                        .child(crate::i18n::tr!("settings.ui_custom", "CUSTOM")),
                )
            }),
    );
    if let Some(note) = context_note {
        label = label.child(
            div()
                .text_size(theme.scale(TEXT_BODY))
                .text_color(theme.text_faint)
                .child(note),
        );
    }

    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_6()
        .py_2()
        .border_b_1()
        .border_color(theme.border_soft)
        .child(label)
        .child(div().flex_shrink_0().child(control))
}

/// The idle right-hand control: the effective-shortcut chip (or "Unset"), a Rebind
/// button, and a Reset button shown only when the row differs from its default.
fn idle_control(
    row: usize,
    effective: Option<&str>,
    is_customized: bool,
    theme: &Theme,
    cx: &mut Context<AppState>,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(match effective {
            Some(k) => shortcut_chip(k, theme).into_any_element(),
            None => div()
                .text_size(theme.scale(TEXT_BODY))
                .text_color(theme.text_faint)
                .child(crate::i18n::tr!("settings.ui_unset", "Unset"))
                .into_any_element(),
        })
        .child(
            Button::new(("keymap-rebind", row), "Rebind")
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Sm)
                .on_click(cx.listener(move |this, _, _, cx| this.begin_keymap_record(row, cx))),
        )
        .when(is_customized, |d| {
            d.child(
                Button::new(("keymap-reset", row), "Reset")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |this, _, _, cx| this.reset_keymap_row(row, cx))),
            )
        })
}

/// The "now recording" affordance: a pulsing prompt to press a shortcut, with a
/// Cancel that ends capture (Esc does the same from the keyboard).
fn recording_affordance(theme: &Theme, cx: &mut Context<AppState>) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .px_2p5()
                .py_1()
                .rounded(theme.radius_sm)
                .border_1()
                .border_color(theme.accent)
                .bg(theme.accent.opacity(0.1))
                .text_size(theme.scale(TEXT_BODY))
                .text_color(theme.accent)
                .child(crate::i18n::tr!(
                    "settings.ui_press_a_shortcut_esc_to_cancel",
                    "Press a shortcut… · Esc to cancel"
                )),
        )
        .child(
            Button::new(
                "keymap-record-cancel",
                crate::i18n::tr!("settings.ui_cancel", "Cancel"),
            )
            .variant(ButtonVariant::Ghost)
            .size(ButtonSize::Sm)
            .on_click(cx.listener(|this, _, _, cx| this.cancel_keymap_record(cx))),
        )
}

/// The captured-chord confirmation: the chord chip, a conflict note when the
/// chord is already bound in the same context, and Confirm / Cancel.
fn capture_confirm(
    cap: &crate::app::KeymapCapture,
    theme: &Theme,
    cx: &mut Context<AppState>,
) -> impl IntoElement + use<> {
    let conflict_label = cap
        .conflict
        .map(|j| crate::keymap::action_defs()[j].label().to_string());

    let confirm_label = if conflict_label.is_some() {
        "Rebind anyway"
    } else {
        "Confirm"
    };

    div()
        .flex()
        .flex_col()
        .items_end()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(shortcut_chip(&cap.chord, theme))
                .child(
                    Button::new("keymap-confirm", confirm_label)
                        .variant(ButtonVariant::Primary)
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_keymap_rebind(cx))),
                )
                .child(
                    Button::new(
                        "keymap-confirm-cancel",
                        crate::i18n::tr!("settings.ui_cancel", "Cancel"),
                    )
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_keymap_record(cx))),
                ),
        )
        .children(conflict_label.map(|other| {
            div()
                .max_w(px(260.))
                .text_size(theme.scale(TEXT_MINOR))
                .text_color(theme.yellow)
                .child(SharedString::from(format!(
                    "Already bound to “{other}”; rebinding unbinds it."
                )))
        }))
}

/// A small monospace chip rendering a keystroke as macOS glyphs (`cmd-shift-f` →
/// `⌘⇧F`), matching the app's keyboard-shortcut chrome.
fn shortcut_chip(keystroke: &str, theme: &Theme) -> impl IntoElement + use<> {
    div()
        .px_2()
        .py_0p5()
        .rounded(theme.radius_sm)
        .bg(theme.bg_active)
        .border_1()
        .border_color(theme.border_soft)
        .text_size(theme.scale(TEXT_BODY))
        .text_color(theme.text_muted)
        .child(SharedString::from(keystroke_glyphs(keystroke)))
}

/// Render a `keymap.toml` keystroke string as readable glyphs. Space-separated
/// chords (a sequence) are kept space-separated. On macOS the modifier glyphs run
/// together (`⌘⇧F`); on Windows/Linux the modifiers are spelled and `+`-joined
/// (`Ctrl+Shift+F`), with `cmd` folded onto Ctrl to match what actually binds
/// there (see [`crate::keymap`]).
fn keystroke_glyphs(keystroke: &str) -> String {
    let mac = cfg!(target_os = "macos");
    keystroke
        .split(' ')
        .map(|chord| {
            let parts = chord
                .split('-')
                .map(|part| match part {
                    "cmd" => if mac { "⌘" } else { "Ctrl" }.to_string(),
                    "shift" => if mac { "⇧" } else { "Shift" }.to_string(),
                    "alt" => if mac { "⌥" } else { "Alt" }.to_string(),
                    "ctrl" => if mac { "⌃" } else { "Ctrl" }.to_string(),
                    "fn" => if mac { "fn" } else { "Fn" }.to_string(),
                    "enter" => "↵".to_string(),
                    "escape" => "Esc".to_string(),
                    "backspace" => "⌫".to_string(),
                    "delete" => "⌦".to_string(),
                    "tab" => "⇥".to_string(),
                    "space" => "Space".to_string(),
                    "up" => "↑".to_string(),
                    "down" => "↓".to_string(),
                    "left" => "←".to_string(),
                    "right" => "→".to_string(),
                    other => other.to_uppercase(),
                })
                .collect::<Vec<_>>();
            // Mac glyphs read as a run; spelled-out modifiers need a separator.
            if mac { parts.concat() } else { parts.join("+") }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The Behavior tab: startup behaviour, then the danger zone. The Redis
/// auto-refresh row that used to sit here (the only engine setting in the whole
/// panel, filed next to "Remove all RED data") now lives on the Key-value page.
fn behavior_page(state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();

    settings_page_scaffold(
        "Behavior",
        div()
            .flex()
            .flex_col()
            .child(registry_group(
                SettingsTab::Behavior,
                "Startup",
                state,
                &theme,
                cx,
            ))
            .child(settings_header("Danger zone", &theme))
            .child(setting_row(
                "Remove all RED data",
                "Permanently delete RED's config and cached-data directories and every \
                 keychain secret (connection passwords, SSH keys, AI keys). Irreversible; \
                 does not touch the RED binary. RED quits when it's done.",
                Button::new(
                    "reset-open",
                    crate::i18n::tr!("settings.ui_remove_all_red_data", "Remove all RED data…"),
                )
                .variant(ButtonVariant::Danger)
                .size(ButtonSize::Sm)
                .on_click(cx.listener(|this, _, _, cx| this.open_reset_confirm(cx))),
                &theme,
            )),
        &theme,
    )
    .into_any_element()
}

/// The AI tab. The value rows (kill switch, access tier, resource guards, the
/// thinking affordance) come from the registry; what stays hand-written is the
/// prose that qualifies them, the folder picker, and the Accounts list. Provider,
/// model, and the agent command stay in the file: the convenience surface holds
/// the safety knobs, the file holds the plumbing.
fn ai_page(state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();
    let ai = &state.settings.ai;

    // Where `generate_report` writes finished HTML files. Empty falls back to the
    // system temp dir (the historical behavior); a chosen folder keeps reports
    // somewhere the user can find them. The path is picked with a native folder
    // dialog; "Reset" clears it back to the temp dir.
    let report_dir = ai.report_dir.trim();
    let has_report_dir = !report_dir.is_empty();
    let report_dir_label: SharedString = if has_report_dir {
        report_dir.to_string().into()
    } else {
        "System temp folder".into()
    };
    let report_dir_color = if has_report_dir {
        theme.text
    } else {
        theme.text_faint
    };
    let report_folder = div()
        .flex()
        .flex_col()
        .items_end()
        .gap_1p5()
        .child(
            div()
                .max_w(px(280.))
                .truncate()
                .text_size(theme.scale(TEXT_MINOR))
                .text_color(report_dir_color)
                .child(report_dir_label),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .child(
                    Button::new(
                        "ai-report-dir-pick",
                        crate::i18n::tr!("settings.ui_choose_folder", "Choose folder…"),
                    )
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.pick_ai_report_dir(cx))),
                )
                .when(has_report_dir, |row| {
                    row.child(
                        Button::new(
                            "ai-report-dir-reset",
                            crate::i18n::tr!("settings.ui_reset", "Reset"),
                        )
                        .variant(ButtonVariant::Secondary)
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| this.clear_ai_report_dir(cx))),
                    )
                }),
        );

    settings_page_scaffold(
        "AI agent",
        div()
            .flex()
            .flex_col()
            .child(registry_group(SettingsTab::Ai, "", state, &theme, cx))
            .child(settings_header("Database access", &theme))
            .child(registry_group(
                SettingsTab::Ai,
                "Database access",
                state,
                &theme,
                cx,
            ))
            .child(
                div()
                    .pt_2()
                    .pb_1()
                    .text_size(theme.scale(TEXT_BODY))
                    .text_color(theme.text_muted)
                    .child(
                        "Writes never run DDL or an unqualified UPDATE/DELETE, and are \
                         blocked on read-only connections. Override per connection with \
                         ai_enabled / ai_tier in connections.toml.",
                    ),
            )
            .child(settings_header("Read-tier resource guards", &theme))
            .child(registry_group(
                SettingsTab::Ai,
                "Read-tier resource guards",
                state,
                &theme,
                cx,
            ))
            .child(settings_header("Display", &theme))
            .child(registry_group(
                SettingsTab::Ai,
                "Display",
                state,
                &theme,
                cx,
            ))
            .child(settings_header("Reports", &theme))
            .child(setting_row(
                "Report folder",
                "Where the agent saves generated HTML reports. Unset uses the system \
                 temp folder.",
                report_folder,
                &theme,
            ))
            .child(settings_header("Accounts", &theme))
            .child(ai_agents_section(state, &theme, cx)),
        &theme,
    )
    .into_any_element()
}

/// The Accounts list: every configured agent (the synthesized legacy built-ins,
/// or an explicit `[[ai.agents]]` set) with its login/key controls. An API agent
/// shows whether its key is set (the keyring) and offers Add/Change/Remove key; an
/// ACP agent shows who's signed in and offers Sign in / switch / Sign out. Which
/// agent runs a chat is chosen in the chat's own agent dropdown, not here.
/// Add/remove agents and edit their fields (command / base_url / model) in the
/// settings file; the "Edit in settings file" button opens it.
fn ai_agents_section(state: &AppState, theme: &Theme, cx: &mut Context<AppState>) -> AnyElement {
    let view = cx.entity();
    let agents = state.settings.ai.resolved_agents();
    let usable: std::collections::HashSet<&str> =
        state.usable_agents.iter().map(|a| a.id.as_str()).collect();
    // The *resolved* default, not the raw setting: an empty or stale `default_agent`
    // still resolves to a real agent, and that is the one a new chat would open on,
    // so it is the one to mark.
    let default_agent = state.settings.ai.resolved_default_agent();

    let mut list = div().flex().flex_col().gap_1();
    for a in &agents {
        let is_acp = a.kind.eq_ignore_ascii_case("acp");
        let is_default = a.id == default_agent;
        // The subscription agent's last-known sign-in, once checked (the AI tab asks
        // on open). API agents have no sign-in; they carry a key.
        let acp_auth = is_acp.then(|| state.ai_auth.get(a.id.as_str())).flatten();
        let signed_in = acp_auth.is_some_and(|s| s.logged_in);
        // Status line: a subscription agent shows who's signed in (or that it isn't);
        // an API agent shows whether its key is in the keyring.
        let (status, status_color): (SharedString, gpui::Hsla) = if is_acp {
            match acp_auth {
                Some(s) if s.logged_in => (acp_identity(s).into(), theme.accent),
                Some(_) => ("not signed in".into(), theme.text_faint),
                None => ("ready".into(), theme.text_muted),
            }
        } else if usable.contains(a.id.as_str()) {
            ("key set".into(), theme.accent)
        } else {
            ("no key".into(), theme.text_faint)
        };
        let kind_badge = div()
            .flex_none()
            .px_1p5()
            .rounded(px(4.))
            .bg(theme.bg_elevated)
            .text_size(theme.scale(TEXT_BADGE))
            .text_color(theme.text_muted)
            .child(if is_acp { "ACP" } else { "API" });
        // Which agent is the default was previously settable only by hand-editing
        // `[ai] default_agent`, which made it invisible: nothing on screen said
        // which agent a new chat (or a confirm-dialog review) would use.
        let default_control: AnyElement = if is_default {
            div()
                .flex_none()
                .px_1p5()
                .rounded(px(4.))
                .bg(theme.accent_ghost)
                .text_size(theme.scale(TEXT_BADGE))
                .text_color(theme.accent)
                .child(crate::i18n::tr!("settings.ui_default", "DEFAULT"))
                .into_any_element()
        } else {
            let id = a.id.clone();
            Button::new(
                SharedString::from(format!("ai-default-{}", a.id)),
                "Make default",
            )
            .variant(ButtonVariant::Ghost)
            .size(ButtonSize::Sm)
            .on_click(cx.listener(move |this, _, _, cx| this.set_default_agent(&id, cx)))
            .into_any_element()
        };
        // Re-auth / switch account (M-S4) lives here, not in the chat panel: an ACP
        // agent owns its own `/login`, so this asks the backend to spawn it for a
        // fresh handshake (it pops its own browser when signed out). API agents
        // carry a key, not a login, so they get no button.
        let signin = is_acp.then(|| {
            let signin_id = a.id.clone();
            let signin_view = view.clone();
            div()
                .id(SharedString::from(format!("agent-signin-{}", a.id)))
                .role(gpui::Role::Button)
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(22.))
                .rounded(px(4.))
                .cursor_pointer()
                .tooltip(flint::Tooltip::text("Sign in / switch account"))
                .hover(|s| s.bg(theme.border))
                .child(crate::icons::icon(
                    "key-round",
                    theme.scale(TEXT_BODY),
                    theme.text_muted,
                ))
                .on_click(move |_, _, cx| {
                    // Keep the click on the button; the row itself isn't clickable.
                    cx.stop_propagation();
                    let id = signin_id.clone();
                    signin_view.update(cx, |this, cx| this.reauthenticate_agent(&id, cx));
                })
        });
        // Sign out, shown only once we know the ACP agent is signed in. (A second
        // sign-in switches account without signing out first.)
        let signout = (is_acp && signed_in).then(|| {
            let signout_id = a.id.clone();
            let signout_view = view.clone();
            div()
                .id(SharedString::from(format!("agent-signout-{}", a.id)))
                .role(gpui::Role::Button)
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .h(px(22.))
                .whitespace_nowrap()
                .px_2()
                .rounded(px(4.))
                .border_1()
                .border_color(theme.border)
                .text_size(theme.scale(TEXT_MINOR))
                .text_color(theme.text_muted)
                .cursor_pointer()
                .tooltip(flint::Tooltip::text("Sign out of this subscription"))
                .hover(|s| s.bg(theme.bg_elevated))
                .child(crate::i18n::tr!("settings.ui_sign_out", "Sign out"))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    let id = signout_id.clone();
                    signout_view.update(cx, |this, cx| this.sign_out_agent(&id, cx));
                })
        });
        // API agents authenticate with a key (kept in the OS keyring, never the
        // settings file). This toggles an inline editor row below; the label tracks
        // whether a key is already stored and whether this row is open.
        let editing = state.ai_key_editing.as_deref() == Some(a.id.as_str());
        let has_key = !is_acp && usable.contains(a.id.as_str());
        let key_btn = (!is_acp).then(|| {
            let key_id = a.id.clone();
            let key_view = view.clone();
            let label = if editing {
                "Cancel"
            } else if has_key {
                "Change key"
            } else {
                "Add key"
            };
            div()
                .id(SharedString::from(format!("agent-key-{}", a.id)))
                .role(gpui::Role::Button)
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .h(px(22.))
                .whitespace_nowrap()
                .px_2()
                .rounded(px(4.))
                .border_1()
                .border_color(theme.border)
                .text_size(theme.scale(TEXT_MINOR))
                .text_color(theme.text_muted)
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_elevated))
                .child(label)
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    let id = key_id.clone();
                    key_view.update(cx, |this, cx| this.edit_agent_key(&id, cx));
                })
        });
        list = list.child(
            div()
                .id(SharedString::from(format!("agent-row-{}", a.id)))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .min_h(px(40.))
                .rounded(theme.radius)
                // Name on top, the status/identity line beneath it, so a long
                // sign-in identity (email · tier) never pushes the name into a wrap.
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(theme.scale(TEXT_BODY))
                                .text_color(theme.text)
                                .child(SharedString::from(a.name.clone())),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(theme.scale(TEXT_MINOR))
                                .text_color(status_color)
                                .child(status),
                        ),
                )
                .child(kind_badge)
                .child(default_control)
                .when_some(key_btn, |row, b| row.child(b))
                .when_some(signout, |row, b| row.child(b))
                .when_some(signin, |row, b| row.child(b)),
        );

        // Inline subscription sign-in (paste-code OAuth) for this agent, when active.
        if let Some(flow) = state.ai_login.as_ref().filter(|f| f.agent_id == a.id) {
            list = list.child(login_panel(state, flow, theme, cx));
        }

        // The inline key editor, shown under the row while this API agent is open.
        // Enter (or Save) stores the key; Esc (or Cancel) closes it; Remove clears a
        // stored key. The field is the shared `ai_key_input`; only one row at a time.
        if editing {
            let id_remove = a.id.clone();
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .pb_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(state.ai_key_input.clone()),
                    )
                    .child(
                        Button::new("ai-key-save", crate::i18n::tr!("settings.ui_save", "Save"))
                            .variant(ButtonVariant::Primary)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.save_agent_key(cx))),
                    )
                    .when(has_key, |row| {
                        row.child(
                            Button::new(
                                "ai-key-remove",
                                crate::i18n::tr!("settings.ui_remove", "Remove"),
                            )
                            .variant(ButtonVariant::Danger)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.clear_agent_key(&id_remove, cx),
                            )),
                        )
                    }),
            );
        }
    }

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(theme.scale(TEXT_BODY))
                .text_color(theme.text_muted)
                .child(
                    "Sign in or add API keys for the agents you use. An API agent needs a \
                     key; use “Add key” (stored in the OS keyring, never in the file). A \
                     subscription (ACP) agent signs in through your browser; use the key \
                     button to sign in or switch account, and “Sign out” to disconnect; the \
                     row shows who's signed in. The DEFAULT agent is the one a new chat \
                     opens on, and the one that reviews a statement in the confirmation \
                     dialog; a chat can still be switched to another from its own agent \
                     dropdown. Add or remove agents and edit their command / endpoint / \
                     model in the settings file.",
                ),
        )
        .child(list)
        .child(
            Button::new(
                "ai-edit-agents-file",
                crate::i18n::tr!("settings.ui_edit_in_settings_file", "Edit in settings file"),
            )
            .variant(ButtonVariant::Secondary)
            .size(ButtonSize::Sm)
            .on_click(cx.listener(|this, _, _, cx| this.open_settings_file(cx))),
        )
        .into_any_element()
}

/// A one-line "who's signed in" label for a subscription agent: the email, plus the
/// subscription tier when known (e.g. `you@example.com · Max`).
fn acp_identity(status: &red_service::AiAuthStatus) -> String {
    let mut label = status
        .email
        .clone()
        .unwrap_or_else(|| "signed in".to_string());
    if let Some(sub) = status.subscription.as_deref().filter(|s| !s.is_empty()) {
        label.push_str(" · ");
        let mut chars = sub.chars();
        if let Some(first) = chars.next() {
            label.extend(first.to_uppercase());
            label.push_str(chars.as_str());
        }
    }
    label
}

/// The inline sign-in panel shown under an agent's row while a sign-in is in flight.
/// The common path completes in the browser on its own (no code needed); the code
/// field is a fallback for when the CLI shows a code to paste. Also: the manual
/// "open the page" link and any error from a prior attempt.
fn login_panel(
    state: &AppState,
    flow: &crate::app::AiLoginFlow,
    theme: &Theme,
    cx: &mut Context<AppState>,
) -> AnyElement {
    let mut panel = div().flex().flex_col().gap_1p5().px_2().pt_1().pb_2();
    match &flow.url {
        // The browser hasn't been pointed yet; the backend is starting the CLI.
        None => {
            panel = panel.child(
                div()
                    .text_size(theme.scale(TEXT_MINOR))
                    .text_color(theme.text_muted)
                    .child(crate::i18n::tr!(
                        "settings.ui_starting_sign_in_a_browser_window_will_open_shor",
                        "Starting sign-in… a browser window will open shortly."
                    )),
            );
        }
        Some(url) => {
            let url_open = url.clone();
            panel = panel
                .child(
                    div()
                        .text_size(theme.scale(TEXT_MINOR))
                        .text_color(theme.text_muted)
                        .child(
                            "A browser window opened; authorize there to finish signing in. \
                             This closes on its own when you're done.",
                        ),
                )
                .child(
                    div()
                        .id("ai-login-open")
                        .role(gpui::Role::Button)
                        .cursor_pointer()
                        .text_size(theme.scale(TEXT_MINOR))
                        .text_color(theme.accent)
                        .child(crate::i18n::tr!(
                            "settings.ui_didn_t_open_open_the_sign_in_page",
                            "Didn't open? Open the sign-in page"
                        ))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.open_external(&url_open, cx)),
                        ),
                )
                // Fallback: only some flows show a code to paste back.
                .child(
                    div()
                        .pt_1()
                        .text_size(theme.scale(TEXT_MINOR))
                        .text_color(theme.text_faint)
                        .child(crate::i18n::tr!(
                            "settings.ui_if_the_browser_shows_a_code_instead_paste_it_her",
                            "If the browser shows a code instead, paste it here:"
                        )),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .child(state.ai_login_code.clone()),
                        )
                        .child(
                            Button::new(
                                "ai-login-submit",
                                crate::i18n::tr!("settings.ui_submit", "Submit"),
                            )
                            .variant(ButtonVariant::Secondary)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.submit_login_code(cx))),
                        )
                        .child(
                            Button::new(
                                "ai-login-cancel",
                                crate::i18n::tr!("settings.ui_cancel", "Cancel"),
                            )
                            .variant(ButtonVariant::Secondary)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))),
                        ),
                );
            if flow.submitting {
                panel = panel.child(
                    div()
                        .text_size(theme.scale(TEXT_MINOR))
                        .text_color(theme.text_muted)
                        .child(crate::i18n::tr!(
                            "settings.ui_finishing_sign_in",
                            "Finishing sign-in…"
                        )),
                );
            }
        }
    }
    if let Some(error) = &flow.error {
        panel = panel.child(
            div()
                .text_size(theme.scale(TEXT_MINOR))
                .text_color(theme.red)
                .child(error.clone()),
        );
    }
    panel.into_any_element()
}

fn about_page(state: &AppState, cx: &mut Context<AppState>) -> AnyElement {
    let theme = cx.theme().clone();

    // Build provenance: version, plus the git SHA + UTC build date embedded by
    // build.rs (Phase 2) so a build is unambiguous in a bug report.
    let build = format!("{} · {}", env!("RED_GIT_SHA"), env!("RED_BUILD_DATE"));

    settings_page_scaffold(
        "About",
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .pb_2()
                    .text_size(theme.scale(TEXT_ROW))
                    .text_color(theme.text_muted)
                    .child(crate::i18n::tr!(
                        "settings.ui_red_roughly_enough_data_a_fast_native_database_e",
                        "RED: Roughly Enough Data, a fast, native database explorer."
                    )),
            )
            .child(settings_header("Build", &theme))
            .child(setting_row(
                "Version",
                "The installed application version.",
                muted_value(env!("CARGO_PKG_VERSION"), &theme),
                &theme,
            ))
            .child(setting_row(
                "Build",
                "The commit and date this build was produced from.",
                muted_value(build, &theme),
                &theme,
            ))
            .child(setting_row(
                "What's new",
                "Open the changelog for this and past releases.",
                Button::new(
                    "about-changelog",
                    crate::i18n::tr!("settings.ui_open_changelog", "Open changelog…"),
                )
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Sm)
                .on_click(cx.listener(|this, _, _, cx| {
                    // Close Settings first so the What's New overlay isn't hidden
                    // behind it; the focus flags resolve in the right order.
                    this.close_settings(cx);
                    this.open_whats_new(cx);
                })),
                &theme,
            ))
            .child(settings_header("Updates", &theme))
            .child(registry_group(
                SettingsTab::About,
                "Updates",
                state,
                &theme,
                cx,
            ))
            .child(update_status_row(state, &theme, cx))
            .child(settings_header("Feedback", &theme))
            .child(setting_row(
                "Report a bug",
                "RED is in early development. Found something wrong? Open an \
                 issue on GitHub.",
                Button::new(
                    "report-bug",
                    crate::i18n::tr!("settings.ui_report_a_bug", "Report a bug…"),
                )
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Sm)
                .on_click(
                    cx.listener(|this, _, _, cx| this.open_external(crate::app::ISSUES_URL, cx)),
                ),
                &theme,
            )),
        &theme,
    )
    .into_any_element()
}

/// The update-status row: a live status line from the updater state, plus the
/// "Check for updates" action (when enabled) or a "Download" link (when a build
/// is available but can't be self-applied).
fn update_status_row<'a>(
    state: &'a AppState,
    theme: &'a Theme,
    cx: &mut Context<AppState>,
) -> impl IntoElement + use<'a> {
    use red_core::UpdateState;

    let enabled = state.settings.update.auto_update;
    let status = match &state.update {
        _ if !enabled => "Automatic updates are off.".to_string(),
        UpdateState::Unknown => "Not checked yet this session.".to_string(),
        UpdateState::Checking => "Checking for updates…".to_string(),
        UpdateState::UpToDate { .. } => "You're on the latest version.".to_string(),
        UpdateState::Downloading { version, .. } => format!("Downloading {version}…"),
        UpdateState::ReadyToRestart { version } => {
            format!("{version} is staged; restart to apply.")
        }
        UpdateState::Failed { reason } => format!("Last check failed: {reason}"),
        UpdateState::Unsupported { version, .. } => {
            format!("{version} is available, but this install can't self-update.")
        }
    };

    // A manual check only makes sense when updates are on (the backend ignores
    // `CheckNow` while disabled). A non-self-updatable build offers a download
    // link to the GitHub release instead.
    let action = match &state.update {
        UpdateState::Unsupported { url, .. } => {
            let url = url.clone();
            Some(
                Button::new(
                    "update-download",
                    crate::i18n::tr!("settings.ui_download", "Download…"),
                )
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Sm)
                .on_click(cx.listener(move |this, _, _, cx| this.open_external(&url, cx))),
            )
        }
        _ if enabled => Some(
            Button::new(
                "update-check",
                crate::i18n::tr!("settings.ui_check_for_updates", "Check for updates"),
            )
            .variant(ButtonVariant::Secondary)
            .size(ButtonSize::Sm)
            .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(cx))),
        ),
        _ => None,
    };

    setting_row("Status", status, div().children(action), theme)
}

/// A right-aligned muted value, the common read-only control shape on the About
/// page.
fn muted_value(text: impl Into<SharedString>, theme: &Theme) -> impl IntoElement {
    div()
        .text_size(theme.scale(TEXT_ROW))
        .text_color(theme.text_muted)
        .child(text.into())
}

/// A page heading above its sections.
fn settings_page_scaffold(title: &str, body: impl IntoElement, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .pb_2()
                .text_size(theme.scale(TEXT_TITLE))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(SharedString::from(title.to_string())),
        )
        .child(body)
}

/// [`settings_page_scaffold`] for a registry page, adding the tab's subtitle: the
/// line that says who the page is for, so a seam page reads as global defaults
/// rather than as something about the connection currently open.
fn page_scaffold(tab: SettingsTab, body: impl IntoElement, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .text_size(theme.scale(TEXT_TITLE))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(tab.title()),
        )
        .children(tab.subtitle_text().map(|s| {
            div()
                .pt_0p5()
                .text_size(theme.scale(TEXT_BODY))
                .text_color(theme.text_muted)
                .child(s)
        }))
        .child(div().pb_2())
        .child(body)
}

/// The settings panel's footer: "Open settings file" beside the Done button.
fn settings_footer(cx: &mut Context<AppState>) -> impl IntoElement + use<> {
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
        // Match the card's rounded bottom-right so the bar's fill doesn't paint a
        // sharp corner into it (the nav rounds the bottom-left).
        .rounded_br(px(10.))
        .child(
            Button::new(
                "set-open-file",
                crate::i18n::tr!("settings.ui_open_settings_file", "Open settings file"),
            )
            .variant(ButtonVariant::Ghost)
            .on_click(cx.listener(|this, _, _, cx| this.open_settings_file(cx))),
        )
        .child(div().flex_1())
        .child(
            Button::new("set-done", crate::i18n::tr!("settings.ui_done", "Done"))
                .variant(ButtonVariant::Primary)
                .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx))),
        )
}

/// A small uppercase section header above a group of settings rows.
fn settings_header(title: &str, theme: &Theme) -> impl IntoElement + use<> {
    div()
        .pt_4()
        .pb_1()
        .text_size(theme.scale(TEXT_BODY))
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

/// A settings control paired with an optional "at your own risk" warning. When
/// `warn` is `Some`, a red ⚠ sits left of the control and hovering it shows a
/// danger tooltip; when `None`, just the control. Used by the AI resource guards
/// so a value above the safe default (or an outright-disabled cap) is flagged
/// without cluttering the common, safe case.
fn risky_control(
    id: &'static str,
    warn: Option<SharedString>,
    control: impl IntoElement,
    theme: &Theme,
) -> AnyElement {
    let row = div().flex().items_center().gap_2();
    match warn {
        Some(msg) => row
            .child(
                div()
                    .id(id)
                    .flex_none()
                    .text_size(theme.scale(TEXT_CONTROL))
                    .text_color(theme.red)
                    .cursor_default()
                    .tooltip(flint::Tooltip::danger(msg))
                    .child("⚠"),
            )
            .child(control)
            .into_any_element(),
        None => row.child(control).into_any_element(),
    }
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
                .text_size(theme.scale(TEXT_ROW))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(title.into()),
        )
        .child(
            div()
                .text_size(theme.scale(TEXT_BODY))
                .text_color(theme.text_muted)
                .child(description.into()),
        )
}
