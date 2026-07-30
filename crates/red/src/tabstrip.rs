//! The workspace tab strip: one implementation for the SQL, Redis and MongoDB
//! shells, which previously carried three copies of it that differed only in an
//! id prefix and which `AppState` method each callback reached for.
//!
//! A strip belongs to a pane and shows only that pane's tabs. It owns the whole
//! tab gesture set — click to focus, middle-click to close, right-click for the
//! menu, drag to reorder within the strip or across to another pane, and the
//! trailing ＋ — while the caller supplies the tabs and the handlers.
//!
//! The strip stays in RED rather than moving down into Flint because its
//! callbacks are `AppState`-shaped: it is a shared *call site*, not a
//! domain-free component.

use std::rc::Rc;

use flint::prelude::*;
use gpui::{
    AnyElement, Context, MouseButton, MouseDownEvent, Pixels, Point, ScrollHandle, SharedString,
    div, prelude::*, px,
};

use crate::app::AppState;
use crate::editor::{TabDrag, TabDragPreview};
use crate::panes::PaneId;

/// Height of a pane's tab strip. Public because a pane has to exclude this band
/// from its drop zones: GPUI dispatches `on_drag_move` in the *capture* phase, so
/// the pane's handler runs before the strip's and a `stop_propagation` inside the
/// strip cannot hold it back — the pane has to know where its own strip is.
pub(crate) const STRIP_H: f32 = 35.;

/// One tab as the strip needs to draw it.
pub(crate) struct StripTab {
    /// Index into the workspace's whole tab list — the drag payload and the key
    /// every callback is given, so callers never translate between the pane's
    /// order and the global one.
    pub(crate) index: usize,
    pub(crate) title: SharedString,
    pub(crate) pinned: bool,
    pub(crate) active: bool,
}

type TabHandler = Rc<dyn Fn(&mut AppState, usize, &mut Context<AppState>)>;
type MenuHandler = Rc<dyn Fn(&mut AppState, usize, Point<Pixels>, &mut Context<AppState>)>;
type PlainHandler = Rc<dyn Fn(&mut AppState, &mut Context<AppState>)>;

/// Everything one pane's strip needs: its tabs, its scroll and drop state, and
/// the handlers that turn a gesture into a state change.
pub(crate) struct TabStrip {
    /// Salts this seam's element ids apart from the others' (`"sql"`, `"kv"`,
    /// `"doc"`).
    prefix: &'static str,
    pane: PaneId,
    tabs: Vec<StripTab>,
    scroll: ScrollHandle,
    /// The insertion gap a dragged tab would land in, if the drag is over *this*
    /// pane's strip.
    gap: Option<usize>,
    /// Whether pinned tabs get their own fixed section ahead of the scrolling
    /// strip (SQL) rather than simply sorting first within it (Redis, Mongo).
    pinned_section: bool,
    /// Tooltip for the trailing ＋, already localized and key-hinted.
    new_tab_tooltip: SharedString,
    on_activate: TabHandler,
    on_close: TabHandler,
    on_menu: Option<MenuHandler>,
    on_new: PlainHandler,
    on_drop: TabHandler,
    on_gap: TabHandler,
    on_gap_clear: PlainHandler,
}

impl TabStrip {
    #[allow(
        clippy::too_many_arguments,
        reason = "the gesture set is the strip's contract; splitting it into \
                  optional builder methods would let a caller silently forget one"
    )]
    pub(crate) fn new(
        prefix: &'static str,
        pane: PaneId,
        scroll: ScrollHandle,
        on_activate: impl Fn(&mut AppState, usize, &mut Context<AppState>) + 'static,
        on_close: impl Fn(&mut AppState, usize, &mut Context<AppState>) + 'static,
        on_new: impl Fn(&mut AppState, &mut Context<AppState>) + 'static,
        on_drop: impl Fn(&mut AppState, usize, &mut Context<AppState>) + 'static,
        on_gap: impl Fn(&mut AppState, usize, &mut Context<AppState>) + 'static,
        on_gap_clear: impl Fn(&mut AppState, &mut Context<AppState>) + 'static,
    ) -> Self {
        Self {
            prefix,
            pane,
            tabs: Vec::new(),
            scroll,
            gap: None,
            pinned_section: false,
            new_tab_tooltip: SharedString::default(),
            on_activate: Rc::new(on_activate),
            on_close: Rc::new(on_close),
            on_menu: None,
            on_new: Rc::new(on_new),
            on_drop: Rc::new(on_drop),
            on_gap: Rc::new(on_gap),
            on_gap_clear: Rc::new(on_gap_clear),
        }
    }

    pub(crate) fn tabs(mut self, tabs: Vec<StripTab>) -> Self {
        self.tabs = tabs;
        self
    }

    pub(crate) fn gap(mut self, gap: Option<usize>) -> Self {
        self.gap = gap;
        self
    }

    pub(crate) fn pinned_section(mut self, separate: bool) -> Self {
        self.pinned_section = separate;
        self
    }

    pub(crate) fn new_tab_tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.new_tab_tooltip = tooltip.into();
        self
    }

    pub(crate) fn on_menu(
        mut self,
        handler: impl Fn(&mut AppState, usize, Point<Pixels>, &mut Context<AppState>) + 'static,
    ) -> Self {
        self.on_menu = Some(Rc::new(handler));
        self
    }
}

impl AppState {
    /// Render `strip` — the tab bar at the top of one pane.
    pub(crate) fn render_tab_strip(&self, strip: TabStrip, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (bg_app, bg_panel, bg_elevated, bg_hover) = (
            theme.bg_app,
            theme.bg_panel,
            theme.bg_elevated,
            theme.bg_hover,
        );
        let border = theme.border;
        let (text, muted, faint) = (theme.text, theme.text_muted, theme.text_faint);
        let accent = theme.accent;
        let icon_close = theme.scale(9.);
        let ui_family = theme.font_family.clone();
        let size_12 = theme.scale(12.);

        let TabStrip {
            prefix,
            pane,
            tabs,
            scroll,
            gap,
            pinned_section,
            new_tab_tooltip,
            on_activate,
            on_close,
            on_menu,
            on_new,
            on_drop,
            on_gap,
            on_gap_clear,
        } = strip;

        // Drop-indicator state, gated on an actual drag so a stale gap never
        // paints once the drag ends.
        let dragging = cx.has_active_drag();
        let last = tabs.last().map(|t| t.index);

        let render_tab = |t: &StripTab| {
            let i = t.index;
            let (tab_bg, tab_text) = if t.active {
                (bg_app, text)
            } else {
                (bg_panel, muted)
            };
            let drag_title = t.title.clone();
            let pinned = t.pinned;
            // Group so the close button reveals only on this tab's hover.
            let group = SharedString::from(format!("{prefix}-tab-{i}"));
            // The dragged tab lands before this tab (gap == i) or after it
            // (gap == i+1); the bar paints on whichever edge the gap names. The
            // after-bar shows only on this pane's last tab.
            let bar_before = dragging && gap == Some(i);
            let bar_after = dragging && Some(i) == last && gap == Some(i + 1);
            let (activate, close, menu) = (on_activate.clone(), on_close.clone(), on_menu.clone());
            let (commit, aim) = (on_drop.clone(), on_gap.clone());
            div()
                .id((SharedString::from(format!("{prefix}-tab")), i))
                .group(group.clone())
                .relative()
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                // Stretch with the title between a comfortable min and a cap;
                // past the cap the label ellipsizes (see the title's `truncate`).
                .min_w(px(96.))
                .max_w(px(200.))
                // Symmetric horizontal room: the hover close button lives in the
                // right inset (right: 4px + 15px wide); mirror it on the left so
                // the centered title clears the button and stays balanced.
                .px(px(23.))
                .bg(tab_bg)
                .border_r_1()
                .border_color(border)
                .cursor_pointer()
                .when(!t.active, |d| d.hover(|s| s.bg(bg_elevated)))
                .on_click(cx.listener(move |this, _, _, cx| activate(this, i, cx)))
                // Middle-click closes the tab, like a browser tab strip. Pinned
                // tabs are protected: unpin (or use the menu) to close.
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _, _, cx| {
                        if !pinned {
                            close(this, i, cx);
                        }
                    }),
                )
                .when_some(menu, |d, handler| {
                    // Right-click opens the Close/Pin context menu at the cursor.
                    d.on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            handler(this, i, event.position, cx);
                        }),
                    )
                })
                // Drag this tab to reorder; the chip below tracks the cursor.
                .on_drag(TabDrag(i), move |_, offset, _window, cx| {
                    let title = drag_title.clone();
                    cx.new(move |_| TabDragPreview {
                        title,
                        offset,
                        bg: bg_elevated,
                        border,
                        text,
                    })
                })
                // Track the cursor across this tab to aim the drop gap at the
                // nearer edge, then commit on release. GPUI fires this on *every*
                // tab per mouse move (capture phase, no hover gate), so we ignore
                // moves whose cursor isn't over this tab; only the hovered tab
                // gets to set the gap.
                .on_drag_move::<TabDrag>(cx.listener(
                    move |this, e: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                        let b = e.bounds;
                        let p = e.event.position;
                        // Must be over this tab in *both* axes: checking x alone would
                        // keep re-setting the gap while dragging straight down off the
                        // strip, leaving a stale indicator behind.
                        if p.x < b.origin.x
                            || p.x >= b.origin.x + b.size.width
                            || p.y < b.origin.y
                            || p.y >= b.origin.y + b.size.height
                        {
                            return;
                        }
                        let slot = if p.x < b.origin.x + b.size.width / 2. {
                            i
                        } else {
                            i + 1
                        };
                        aim(this, slot, cx);
                    },
                ))
                .on_drop::<TabDrag>(cx.listener(move |this, drag: &TabDrag, _, cx| {
                    // Handled here so it doesn't also bubble to the pane body's
                    // drop (which would double-move). A drop on a strip is always
                    // a reorder into that pane, never a split.
                    cx.stop_propagation();
                    commit(this, drag.0, cx);
                }))
                .when(bar_before, |d| d.child(insertion_bar(accent, true)))
                .when(bar_after, |d| d.child(insertion_bar(accent, false)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .min_w_0()
                        .when(pinned, |d| {
                            d.child(crate::icons::icon("pin", icon_close, faint))
                        })
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family(ui_family.clone())
                                .text_size(size_12)
                                .text_color(tab_text)
                                .child(t.title.clone()),
                        ),
                )
                // Close button: pinned to the right, revealed only on tab hover
                // so it never crowds the centered title at rest. The outer div
                // positions + vertically centers; the inner one is the hitbox.
                .child(
                    div()
                        .absolute()
                        .right(px(4.))
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .invisible()
                        .group_hover(group, |s| s.visible())
                        .child(
                            div()
                                .id((SharedString::from(format!("{prefix}-tab-close")), i))
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(15.))
                                .rounded(px(3.))
                                .text_color(faint)
                                .hover(|s| s.bg(bg_hover).text_color(text))
                                .on_click(cx.listener({
                                    let close = on_close.clone();
                                    move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        close(this, i, cx);
                                    }
                                }))
                                .child(crate::icons::icon("close", icon_close, faint)),
                        ),
                )
        };

        let (pinned_tabs, scrolling_tabs): (Vec<_>, Vec<_>) = if pinned_section {
            let (p, u): (Vec<_>, Vec<_>) = tabs.iter().partition(|t| t.pinned);
            (
                p.into_iter().map(&render_tab).collect(),
                u.into_iter().map(&render_tab).collect(),
            )
        } else {
            (Vec::new(), tabs.iter().map(&render_tab).collect())
        };

        // The tabs live in a horizontally scrollable viewport, so a crowded strip
        // scrolls instead of squashing the tabs. `min_w(0)` lets the flex child
        // shrink below its content width so the overflow engages.
        let viewport = div()
            .id((
                SharedString::from(format!("{prefix}-tabstrip")),
                pane.0 as usize,
            ))
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .items_stretch()
            .overflow_x_scroll()
            .track_scroll(&scroll)
            // Clear the gap indicator when the drag leaves the strip *vertically*
            // (down into the pane body, where the drop-zone overlay takes over).
            // Horizontal exit is deliberately ignored: with strips side by side, a
            // drag crossing to the next one stays at the same Y, and clearing on
            // horizontal exit would race that strip's gap.
            .on_drag_move::<TabDrag>(cx.listener(
                move |this, e: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                    let b = e.bounds;
                    let p = e.event.position;
                    if p.y < b.origin.y || p.y >= b.origin.y + b.size.height {
                        on_gap_clear(this, cx);
                    }
                },
            ))
            // Release anywhere in the strip (including the trailing space) commits
            // using the gap the hovered tab last set, landing the tab in this pane.
            .on_drop::<TabDrag>(cx.listener(move |this, drag: &TabDrag, _, cx| {
                cx.stop_propagation();
                on_drop(this, drag.0, cx);
            }))
            .children(scrolling_tabs);

        // Pinned tabs sit in their own fixed section ahead of the scrolling strip,
        // so they never leave view however far the rest is scrolled.
        let pinned_strip = (!pinned_tabs.is_empty()).then(|| {
            div()
                .id((
                    SharedString::from(format!("{prefix}-tabstrip-pinned")),
                    pane.0 as usize,
                ))
                .flex_shrink_0()
                .h_full()
                .flex()
                .items_stretch()
                .children(pinned_tabs)
        });

        div()
            .flex_shrink_0()
            .h(px(STRIP_H))
            .flex()
            .items_stretch()
            .bg(bg_panel)
            .border_b_1()
            .border_color(border)
            .children(pinned_strip)
            .child(viewport)
            // The ＋ stays pinned right of the scrolling tabs, always reachable.
            .child(
                div()
                    .id((SharedString::from(format!("{prefix}-new")), pane.0 as usize))
                    .flex_shrink_0()
                    .w(px(34.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_l_1()
                    .border_color(border)
                    .cursor_pointer()
                    .tooltip(Tooltip::text(new_tab_tooltip))
                    .text_color(faint)
                    .hover(|s| s.bg(bg_elevated).text_color(text))
                    .on_click(cx.listener(move |this, _, _, cx| on_new(this, cx)))
                    .child(crate::icons::icon("plus", theme.scale(13.), faint)),
            )
            .into_any_element()
    }
}

/// The 2px accent bar marking where a dragged tab would be inserted.
fn insertion_bar(accent: gpui::Hsla, before: bool) -> impl IntoElement {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .w(px(2.))
        .bg(accent)
        .when(before, |d| d.left_0())
        .when(!before, |d| d.right_0())
}
