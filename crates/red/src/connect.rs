//! The connection manager — the disconnected landing screen: a card listing
//! saved connections (click to connect, edit/delete actions) plus the add/edit
//! modal form. Pure assembly over Flint components; all state + actions live on
//! [`AppState`] (`app.rs`).

use flint::prelude::*;
use flint::Theme;
use gpui::{
    div, prelude::*, px, AnyElement, Context, FontWeight, Hsla, Role, SharedString,
    WindowControlArea,
};
use red_core::DbKind;

use crate::app::{AppState, ConnectSortField, FormField, FormState, TestState};

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

/// A section heading ("Saved connections") with a hairline rule that fills the
/// rest of the row — the divider the welcome layout uses.
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
        //
        // Cards follow the visible (filtered + sorted) order; each carries its
        // original index into `connections` so connect/edit/delete still address the
        // stored connection regardless of display order, while the keyboard
        // highlight (`connect_sel`) tracks the display position.
        let visible = self.visible_connections(cx);
        let cards: Vec<AnyElement> = visible
            .iter()
            .enumerate()
            .map(|(display_ix, &orig_ix)| {
                let stored = &self.connections[orig_ix];
                self.connection_card(
                    display_ix,
                    orig_ix,
                    &stored.config,
                    stored.last_accessed,
                    cx,
                )
                .into_any_element()
            })
            .collect();
        let toolbar = (!self.connections.is_empty()).then(|| self.connect_toolbar(cx));
        let new_button = self.new_button(cx);
        let settings_gear = IconButton::new(
            "connect-settings",
            crate::icons::icon("settings", cx.theme().scale(16.), cx.theme().text_muted),
        )
        .size(IconButtonSize::Sm)
        .tooltip("Settings  ⌘,")
        .a11y_label("Settings")
        .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)));

        let theme = cx.theme();

        // The brand lockup: the square mark beside the wordmark, centered. Both take
        // the accent so they re-tint with the active theme (`red.svg` renders as an
        // accent-masked square).
        let header = div()
            .flex()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                gpui::svg()
                    .path("red.svg")
                    .size(theme.scale(40.))
                    .flex_none()
                    .text_color(theme.accent),
            )
            .child(
                // The wordmark sits in the primary text color (white on the dark
                // themes), so it reads as a label beside the red brand mark.
                div()
                    .text_color(theme.text)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.scale(34.))
                    .child("Red"),
            );

        let empty_note = |msg: &'static str, theme: &Theme| {
            div()
                .py_2()
                .text_size(theme.scale(12.))
                .text_color(theme.text_faint)
                .child(msg)
                .into_any_element()
        };
        let saved: AnyElement = if self.connections.is_empty() {
            empty_note("No saved connections yet.", theme)
        } else if cards.is_empty() {
            empty_note("No connections match your search.", theme)
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
            .children(toolbar)
            .child(saved)
            .child(new_button);

        let screen = div()
            .id("connect-screen")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_center()
            .bg(theme.bg_app)
            .text_color(theme.text)
            .font_family(theme.font_family.clone())
            .child(column);

        // Only a slim strip at the top drags the window (double-click zooms) —
        // mirroring the connected shell, where dragging is confined to the topbar.
        // The rest of the welcome screen stays clickable without moving the window.
        // It sits in the column's empty top padding, behind the settings gear.
        let drag_strip = div()
            .id("connect-drag")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(38.))
            .window_control_area(WindowControlArea::Drag)
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    #[cfg(target_os = "macos")]
                    window.titlebar_double_click();
                    #[cfg(not(target_os = "macos"))]
                    window.zoom_window();
                }
            });

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
            .child(drag_strip)
            .child(gear)
    }

    /// The saved-connection indices to show, in display order: filtered by the
    /// welcome-screen search box, then ordered by the active sort mode. Returns
    /// indices into `self.connections` so the cards and keyboard actions still
    /// address the stored connection regardless of display order.
    pub(crate) fn visible_connections(&self, cx: &Context<Self>) -> Vec<usize> {
        let query = self.connect_search.read(cx).content().trim().to_lowercase();
        let mut indices: Vec<usize> = self
            .connections
            .iter()
            .enumerate()
            .filter(|(_, stored)| {
                query.is_empty()
                    || stored.config.name.to_lowercase().contains(&query)
                    || stored
                        .config
                        .display_target()
                        .to_lowercase()
                        .contains(&query)
            })
            .map(|(ix, _)| ix)
            .collect();
        // Compare by the active key in its natural (ascending) order, then flip for
        // a descending sort. For `Recent`, `Option<u64>` orders `None` (never used)
        // before any timestamp, so ascending lists never-used/oldest first.
        let asc = self.connect_sort.ascending;
        indices.sort_by(|&a, &b| {
            let ord = match self.connect_sort.field {
                ConnectSortField::Recent => self.connections[a]
                    .last_accessed
                    .cmp(&self.connections[b].last_accessed),
                ConnectSortField::Name => self.connections[a]
                    .config
                    .name
                    .to_lowercase()
                    .cmp(&self.connections[b].config.name.to_lowercase()),
            };
            if asc {
                ord
            } else {
                ord.reverse()
            }
        });
        indices
    }

    /// The toolbar above the saved-connection list: a search box that filters the
    /// list as you type, and the Name / Recent sort toggle.
    fn connect_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Build the sort cells first (they reborrow `cx` for their listeners), then
        // read the theme for the search box's chrome.
        let sort_name = self.sort_cell("Name", ConnectSortField::Name, cx);
        let sort_recent = self.sort_cell("Recent", ConnectSortField::Recent, cx);
        let theme = cx.theme();

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
            .text_size(theme.scale(13.))
            .text_color(theme.text)
            .child(crate::icons::icon(
                "search",
                theme.scale(14.),
                theme.text_faint,
            ))
            .child(div().flex_1().min_w_0().child(self.connect_search.clone()));

        div()
            .flex()
            .items_center()
            .gap_2()
            .mb_2()
            .child(search)
            .child(sort_name)
            .child(sort_recent)
    }

    /// One cell of the Name / Recent sort toggle. The active field takes the accent
    /// treatment and its arrow shows the live direction; an inactive cell shows the
    /// direction a first click would apply. Clicking the active field flips the
    /// direction (see [`ConnectSort::toggle`]). Both share the cards' subtle hover
    /// so the welcome screen's interactive surfaces feel consistent.
    fn sort_cell(
        &self,
        label: &'static str,
        field: ConnectSortField,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let on = self.connect_sort.field == field;
        // The arrow reflects the current direction when active, else the direction
        // this cell would select on a first click.
        let ascending = if on {
            self.connect_sort.ascending
        } else {
            matches!(field, ConnectSortField::Name)
        };
        let icon_name = if ascending { "sort-asc" } else { "sort-desc" };
        let (bg, border, text) = if on {
            (theme.accent_ghost, theme.accent, theme.text)
        } else {
            (theme.bg_input, theme.border, theme.text_muted)
        };
        let icon_tint = if on { theme.accent } else { theme.text_faint };
        div()
            .id(SharedString::from(format!("connect-sort-{label}")))
            .flex()
            .items_center()
            .gap_1p5()
            .h(px(34.))
            .px_2p5()
            .rounded(theme.radius)
            .bg(bg)
            .border_1()
            .border_color(border)
            .text_size(theme.scale(12.5))
            .text_color(text)
            .cursor_pointer()
            // Keep the accent border on the active cell while still giving feedback;
            // the inactive cells strengthen their border like the connection cards.
            .when(on, |d| d.hover(|s| s.bg(theme.bg_active)))
            .when(!on, |d| {
                d.hover(|s| s.border_color(theme.border_strong).bg(theme.bg_active))
            })
            .child(crate::icons::icon(icon_name, theme.scale(13.), icon_tint))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.connect_sort.toggle(field);
                this.connect_sel = 0;
                cx.notify();
            }))
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
        display_ix: usize,
        orig_ix: usize,
        config: &red_core::ConnectionConfig,
        last_accessed: Option<u64>,
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
        let group = SharedString::from(format!("connect-card-{orig_ix}"));
        // Accessible name: the connection's name, engine, and read-only state —
        // the card is the welcome screen's primary action, announced as a button.
        let a11y_name = if config.read_only {
            format!("{}, {}, read-only", config.name, badge_label)
        } else {
            format!("{}, {}", config.name, badge_label)
        };

        div()
            .id(SharedString::from(format!("connect-{orig_ix}")))
            .role(Role::Button)
            .aria_label(a11y_name)
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
            .border_color(if display_ix == self.connect_sel {
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
            // The last-used recency, shown by default and hidden on hover to make
            // room for the actions — so sorting by "Recent" is legible at a glance.
            .child(
                div()
                    .flex_none()
                    .group_hover(group.clone(), |s| s.invisible())
                    .text_size(theme.scale(11.5))
                    .text_color(theme.text_faint)
                    .child(fmt_ago(last_accessed)),
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
                                    SharedString::from(format!("edit-{orig_ix}")),
                                    crate::icons::icon("edit", theme.scale(14.), theme.text_muted),
                                )
                                .size(IconButtonSize::Sm)
                                .tooltip("Edit connection")
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.open_edit_form(orig_ix, cx);
                                    },
                                )),
                            )
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("delete-{orig_ix}")),
                                    crate::icons::icon("trash", theme.scale(14.), theme.red),
                                )
                                .size(IconButtonSize::Sm)
                                .tooltip("Delete connection")
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.request_delete_connection(orig_ix, cx);
                                    },
                                )),
                            ),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.connect(orig_ix, cx)))
    }

    pub(crate) fn render_form(&self, form: &FormState, cx: &mut Context<Self>) -> impl IntoElement {
        // Owned clone so the theme doesn't hold an immutable borrow of `cx` across
        // the `&mut cx` helper calls below.
        let theme = cx.theme().clone();
        let theme = &theme;
        let view = cx.entity().downgrade();
        let is_file = form.kind.is_file();
        // Per-field validation messages, shown only once the user has tried to
        // submit (or test) — so a fresh form isn't pre-littered with red text.
        let errors = if form.submitted {
            self.form_config(cx)
                .map(|c| AppState::form_errors(&c))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let title = if form.editing.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };

        // Network engines get a live connection-string field that mirrors — and is
        // mirrored by — the structured fields. File engines have only a path.
        let conn_str_field = (!is_file)
            .then(|| labeled_field("Connection string", theme).child(self.conn_str_input.clone()));

        let footer = self.render_form_footer(form, cx);

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
                    .child(
                        labeled_field("Name", theme)
                            .child(self.name_input.clone())
                            .children(field_error_line(theme, field_err(&errors, FormField::Name))),
                    )
                    .child(
                        labeled_field("Engine", theme)
                            .child(self.render_engine_picker(form.kind, theme, cx)),
                    )
                    .children(conn_str_field)
                    .child(self.render_connection_fields(form, is_file, &errors, theme))
                    .child(self.render_label_access_row(form, theme, cx))
                    .children(self.render_form_status(form, cx)),
            )
    }

    /// The per-engine connection fields: a file path, or host/port/database/
    /// user/password for a network engine.
    fn render_connection_fields(
        &self,
        form: &FormState,
        is_file: bool,
        errors: &[(FormField, &'static str)],
        theme: &Theme,
    ) -> AnyElement {
        if is_file {
            return labeled_field("Database file", theme)
                .child(self.database_input.clone())
                .children(field_error_line(
                    theme,
                    field_err(errors, FormField::Database),
                ))
                .into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    // Top-align so a validation line under Host doesn't stretch the
                    // Port input next to it.
                    .flex()
                    .items_start()
                    .gap_3()
                    .child(
                        labeled_field("Host", theme)
                            .flex_1()
                            .child(self.host_input.clone())
                            .children(field_error_line(theme, field_err(errors, FormField::Host))),
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
                .child(self.database_input.clone())
                .children(field_error_line(
                    theme,
                    field_err(errors, FormField::Database),
                )),
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
        let focus_ring = theme.text;
        let swatches = (0..6u8).map(|i| {
            let color = label_color(i, theme);
            let on = i == form.color;
            div()
                .id(SharedString::from(format!("swatch-{i}")))
                // A radio in a group, named so the keyboard user knows what each
                // color does; Tab reaches it and Enter/Space selects it.
                .role(Role::RadioButton)
                .aria_label(SharedString::from(format!("Label color {}", i + 1)))
                .tab_index(0)
                .size(px(20.))
                .rounded_full()
                .bg(color)
                .cursor_pointer()
                .border_2()
                .border_color(if on { color } else { theme.bg_elevated })
                .when(on, |s| s.shadow_sm())
                .focus(move |s| s.border_color(focus_ring))
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
                            Toggle::new("read-only", form.read_only)
                                .label("Read-only")
                                .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                    this.set_form_read_only(*checked, cx)
                                })),
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

    /// The modal footer: the Test-connection action on the left, Cancel / Save /
    /// Connect on the right. The test *result* renders in the full-width status row
    /// above (see [`Self::render_form_status`]) so it has room to read.
    fn render_form_footer(&self, form: &FormState, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let testing = matches!(form.test, TestState::Testing);

        div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .child(
                div().flex_1().min_w_0().child(
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
                ),
            )
            .child(
                Button::new("form-cancel", "Cancel")
                    .variant(ButtonVariant::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| this.close_form(cx))),
            )
            .child(
                Button::new("form-save", "Save")
                    .variant(ButtonVariant::Secondary)
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
                    .on_click(cx.listener(|this, _, _, cx| this.save_form(true, cx))),
            )
    }

    /// The full-width status row beneath the form fields: the test-connection
    /// result, in a tinted, wrapping panel so a long engine error stays readable
    /// instead of being truncated next to the buttons. `None` while idle/testing.
    fn render_form_status(&self, form: &FormState, cx: &Context<Self>) -> Option<gpui::Div> {
        let theme = cx.theme();
        let (msg, color, icon_name) = match &form.test {
            TestState::Ok(msg) => (msg.clone(), theme.green, "check"),
            TestState::Fail(msg) => (msg.clone(), theme.red, "close"),
            TestState::Idle | TestState::Testing => return None,
        };
        Some(
            div()
                .flex()
                .items_start()
                .gap_2()
                .w_full()
                .p(px(9.))
                .rounded(theme.radius)
                .bg(color.opacity(0.10))
                .child(div().flex_none().mt(px(1.)).child(crate::icons::icon(
                    icon_name,
                    theme.scale(13.),
                    color,
                )))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(theme.scale(12.))
                        .text_color(theme.text)
                        .child(msg),
                ),
        )
    }
}

/// The first validation message tagged for `field`, if any — the per-input lookup
/// over the form's collected [`AppState::form_errors`].
fn field_err(errors: &[(FormField, &'static str)], field: FormField) -> Option<&'static str> {
    errors
        .iter()
        .find(|(f, _)| *f == field)
        .map(|(_, msg)| *msg)
}

/// A small red validation line, rendered beneath the input it belongs to. `None`
/// when the field is valid, so it slots straight into `.children(...)`.
fn field_error_line(theme: &Theme, message: Option<&str>) -> Option<gpui::Div> {
    message.map(|msg| {
        div()
            .text_size(theme.scale(11.))
            .text_color(theme.red)
            .child(msg.to_string())
    })
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
