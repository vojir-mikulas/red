//! Client-side window decorations (Linux/Wayland).
//!
//! On macOS and Windows the OS draws the window frame (titlebar, drag region,
//! min/max/close buttons, resize borders) and GPUI reports
//! [`Decorations::Server`]. There is nothing for us to do.
//!
//! On Linux, GNOME/Wayland (and X11 with Motif hints) hand the application
//! [`Decorations::Client`]: the compositor paints *nothing*, so the app must
//! draw its own window controls, run the interactive move/resize grabs, and
//! paint the corner rounding + shadow. GPUI's Wayland backend ignores the
//! `WindowControlArea` hit-test entirely (see `on_hit_test_window_control`), so
//! dragging only works through an explicit [`Window::start_window_move`]; the
//! same is true of resize and the min/max/close actions. Without this, the
//! window has no titlebar at all and can't be moved, resized, or closed.
//!
//! Everything here is gated at *runtime* on `Decorations::Client`, so the code
//! compiles on every platform and simply no-ops where the OS supplies a native
//! frame. Mirrors Zed's `PlatformTitleBar`.

use flint::Theme;
use gpui::{
    canvas, div, prelude::*, px, AnyElement, Bounds, CursorStyle, Decorations, HitboxBehavior,
    Hsla, MouseButton, Pixels, Point, ResizeEdge, Size, StatefulInteractiveElement, Tiling,
    WeakEntity, Window,
};
// Only referenced when we build our own drag region; never on Windows, where the
// native caption bar owns window-move (see `draggable`).
#[cfg(not(target_os = "windows"))]
use gpui::WindowControlArea;

use crate::app::AppState;

/// Width of the invisible resize border / drop-shadow margin around a
/// client-decorated window. Matches GPUI's `window_shadow` reference.
const SHADOW: f32 = 10.0;

/// Wrap the application root so a client-decorated window gets its resize
/// borders and drop shadow. The corners are kept square (native-looking on
/// Linux/Windows, where rounded app-drawn corners read as out of place). On
/// server-decorated windows
/// (macOS/Windows, and Linux compositors that draw their own frame) the content
/// is returned untouched.
pub(crate) fn frame(
    window: &mut Window,
    border_color: Hsla,
    content: impl IntoElement,
) -> AnyElement {
    let Decorations::Client { tiling } = window.window_decorations() else {
        return content.into_any_element();
    };

    // Tell GPUI how wide our shadow margin is so input coordinates and the
    // compositor's notion of the window edge line up.
    window.set_client_inset(px(SHADOW));

    let shadow = px(SHADOW);
    let border = px(1.0);

    div()
        .size_full()
        .bg(gpui::transparent_black())
        // A full-window hitbox so the cursor turns into a resize arrow near any
        // free edge; the mouse-down below starts the matching compositor grab.
        .child(
            canvas(
                |_bounds, window, _cx| {
                    window.insert_hitbox(
                        Bounds::new(
                            gpui::point(px(0.), px(0.)),
                            window.window_bounds().get_bounds().size,
                        ),
                        HitboxBehavior::Normal,
                    )
                },
                move |_bounds, hitbox, window, _cx| {
                    let pos = window.mouse_position();
                    let size = window.window_bounds().get_bounds().size;
                    if let Some(edge) = resize_edge(pos, shadow, size, tiling) {
                        window.set_cursor_style(cursor_for(edge), &hitbox);
                    }
                },
            )
            .size_full()
            .absolute(),
        )
        // Reserve the shadow margin on every non-tiled side.
        .when(!tiling.top, |d| d.pt(shadow))
        .when(!tiling.bottom, |d| d.pb(shadow))
        .when(!tiling.left, |d| d.pl(shadow))
        .when(!tiling.right, |d| d.pr(shadow))
        // Repaint as the pointer moves so the resize cursor tracks the edges.
        .on_mouse_move(|_e, window, _cx| window.refresh())
        .on_mouse_down(MouseButton::Left, move |e, window, _cx| {
            let size = window.window_bounds().get_bounds().size;
            if let Some(edge) = resize_edge(e.position, shadow, size, tiling) {
                window.start_window_resize(edge);
            }
        })
        // The actual app surface: bordered and shadowed, with square corners.
        .child(
            div()
                .size_full()
                .overflow_hidden()
                .border_color(border_color)
                .when(!tiling.top, |d| d.border_t(border))
                .when(!tiling.bottom, |d| d.border_b(border))
                .when(!tiling.left, |d| d.border_l(border))
                .when(!tiling.right, |d| d.border_r(border))
                .when(!tiling.is_tiled(), |d| {
                    d.shadow(vec![gpui::BoxShadow {
                        color: gpui::hsla(0., 0., 0., 0.4),
                        blur_radius: shadow / 2.,
                        spread_radius: px(0.),
                        offset: gpui::point(px(0.), px(0.)),
                        inset: false,
                    }])
                })
                .child(content),
        )
        .into_any_element()
}

/// Make `el` (a titlebar / drag strip) move the window when dragged.
///
/// On macOS the `WindowControlArea::Drag` hit-test does the work (and the
/// double-click zooms/minimises per System Settings). On Linux the hit-test is
/// ignored, so we run an explicit interactive move: arm on mouse-down, and fire
/// [`Window::start_window_move`] on the first drag motion (a plain click never
/// moves, so it still reaches the controls underneath).
///
/// **Not on Windows.** Windows keeps its native caption bar (see
/// `main::titlebar_options`), so this strip must *not* also be a drag region:
/// GPUI translates `WindowControlArea::Drag` into an `HTCAPTION` hit-test, and
/// because the hit-test resolver matches the strip's hitbox anywhere in the
/// cursor stack (not just the topmost), the whole bar reads as caption, which
/// both moves the window on drag *and* swallows clicks on the controls sitting
/// in it (the Settings gear, the connection switcher, …). The native caption
/// already handles window-move there, so we leave the strip as plain content.
pub(crate) fn draggable<E>(el: E, window: &Window, view: WeakEntity<AppState>) -> E
where
    E: StatefulInteractiveElement,
{
    #[cfg(target_os = "windows")]
    {
        let _ = (window, view);
        el
    }
    #[cfg(not(target_os = "windows"))]
    {
        let el = el
            .window_control_area(WindowControlArea::Drag)
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    #[cfg(target_os = "macos")]
                    window.titlebar_double_click();
                    #[cfg(not(target_os = "macos"))]
                    window.zoom_window();
                }
            });

        if !matches!(window.window_decorations(), Decorations::Client { .. }) {
            return el;
        }

        let arm = view.clone();
        let disarm = view.clone();
        let go = view.clone();
        el.on_mouse_down(MouseButton::Left, move |_e, _window, cx| {
            arm.update(cx, |this, _| this.titlebar_drag = true).ok();
        })
        .on_mouse_up(MouseButton::Left, move |_e, _window, cx| {
            disarm.update(cx, |this, _| this.titlebar_drag = false).ok();
        })
        .on_mouse_move(move |_e, window, cx| {
            let armed = go
                .update(cx, |this, _| std::mem::take(&mut this.titlebar_drag))
                .unwrap_or(false);
            if armed {
                window.start_window_move();
            }
        })
    }
}

/// The minimize / maximize / close cluster, drawn only when the window uses
/// client-side decorations. Returns `None` on macOS/Windows, where the OS draws
/// these. Sits at the right end of the titlebar.
pub(crate) fn window_controls(window: &Window, theme: &Theme) -> Option<impl IntoElement> {
    if !matches!(window.window_decorations(), Decorations::Client { .. }) {
        return None;
    }

    let supported = window.window_controls();
    let restore = window.is_maximized();

    Some(
        div()
            .flex()
            .items_center()
            .gap_0p5()
            .pl_1()
            .children(supported.minimize.then(|| {
                control("window-minimize", "minimize", theme, |window, _cx| {
                    window.minimize_window()
                })
            }))
            .children(supported.maximize.then(|| {
                control(
                    "window-maximize",
                    if restore { "restore" } else { "maximize" },
                    theme,
                    |window, _cx| window.zoom_window(),
                )
            }))
            .child(control("window-close", "close", theme, |window, cx| {
                // Single-window app: closing the window quits (the
                // `on_window_closed` handler in `main` does this when the last
                // window goes away), so dispatch our `Quit` action directly.
                window.dispatch_action(Box::new(crate::Quit), cx);
            })),
    )
}

/// One window-control button. Stops propagation on press so it never arms the
/// titlebar drag underneath it. The icon masks to `currentColor`, so it rests at
/// the muted text color and brightens on hover via the button's `group`.
fn control(
    id: &'static str,
    icon: &'static str,
    theme: &Theme,
    on_click: impl Fn(&mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let is_close = id == "window-close";
    let hover_bg = if is_close { theme.red } else { theme.bg_hover };
    let hover_fg = if is_close {
        theme.on_accent
    } else {
        theme.text
    };

    div()
        .id(id)
        .group(id)
        .size(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|s| s.bg(hover_bg))
        .child(
            crate::icons::icon(icon, theme.scale(14.), theme.text_muted)
                .group_hover(id, |s| s.text_color(hover_fg)),
        )
        .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
        .on_click(move |_e, window, cx| {
            cx.stop_propagation();
            on_click(window, cx);
        })
}

/// The resize edge the pointer is over, or `None` if it's in the interior or on
/// a tiled (snapped) edge that can't be dragged.
fn resize_edge(
    pos: Point<Pixels>,
    border: Pixels,
    size: Size<Pixels>,
    tiling: Tiling,
) -> Option<ResizeEdge> {
    let left = pos.x < border && !tiling.left;
    let right = pos.x > size.width - border && !tiling.right;
    let top = pos.y < border && !tiling.top;
    let bottom = pos.y > size.height - border && !tiling.bottom;

    let edge = if top && left {
        ResizeEdge::TopLeft
    } else if top && right {
        ResizeEdge::TopRight
    } else if bottom && left {
        ResizeEdge::BottomLeft
    } else if bottom && right {
        ResizeEdge::BottomRight
    } else if top {
        ResizeEdge::Top
    } else if bottom {
        ResizeEdge::Bottom
    } else if left {
        ResizeEdge::Left
    } else if right {
        ResizeEdge::Right
    } else {
        return None;
    };
    Some(edge)
}

fn cursor_for(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
    }
}
