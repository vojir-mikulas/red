//! Middle-click hold-to-autoscroll over a result grid, browser-style: holding
//! the middle mouse button down and moving away from the click point scrolls
//! continuously toward it, at a speed proportional to the distance — a
//! joystick, not a drag. A second middle-click (or any other click) ends it.

use std::time::Duration;

use gpui::{
    AsyncApp, Context, Hsla, Pixels, Point, ScrollHandle, Styled, UniformListScrollHandle,
    WeakEntity, point, prelude::*, px,
};

use crate::app::AppState;

/// Distance from the origin (px) before autoscroll starts moving — small
/// jitter right after the middle-click doesn't nudge the grid.
const DEADZONE: f32 = 12.0;
/// Distance beyond the deadzone (px) at which speed reaches its cap.
const RAMP: f32 = 140.0;
/// Fastest the grid moves per tick (px). At the 16ms [`TICK`] that's ~1250px/s.
const MAX_SPEED: f32 = 20.0;
const TICK: Duration = Duration::from_millis(16);

/// A middle-click-held autoscroll in progress: the click origin, the pointer's
/// live position (updated by mouse-move), and the target grid's scroll
/// handles — cloned at click time, so the loop keeps driving the same grid
/// even if focus moves elsewhere mid-scroll.
pub(crate) struct Autoscroll {
    pub(crate) origin: Point<Pixels>,
    current: Point<Pixels>,
    scroll: ScrollHandle,
    h_scroll: ScrollHandle,
    /// Guards against a superseded session's timer loop still running; see
    /// [`AppState::autoscroll_epoch`].
    epoch: u64,
}

/// Speed at distance `d` from the origin: zero inside the deadzone, ramping
/// linearly to `MAX_SPEED` over `RAMP` pixels beyond it, signed toward `d`.
fn speed(d: f32) -> f32 {
    let mag = d.abs();
    if mag <= DEADZONE {
        return 0.0;
    }
    let t = ((mag - DEADZONE) / RAMP).min(1.0);
    (t * MAX_SPEED).copysign(d)
}

impl AppState {
    /// Middle mouse button pressed over a grid pane: start a new autoscroll
    /// session there, or end the current one if one's already running (a
    /// second middle-click toggles it off, mirroring the browser gesture).
    pub(crate) fn toggle_autoscroll(
        &mut self,
        origin: Point<Pixels>,
        scroll: &UniformListScrollHandle,
        h_scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) {
        if self.autoscroll.take().is_some() {
            cx.notify();
            return;
        }
        self.autoscroll_epoch += 1;
        let epoch = self.autoscroll_epoch;
        self.autoscroll = Some(Autoscroll {
            origin,
            current: origin,
            scroll: scroll.0.borrow().base_handle.clone(),
            h_scroll: h_scroll.clone(),
            epoch,
        });
        cx.notify();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(TICK).await;
                let alive = this
                    .update(cx, |this, cx| this.autoscroll_tick(epoch, cx))
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Ends any running autoscroll session — any click other than the
    /// starting middle-click cancels the gesture.
    pub(crate) fn cancel_autoscroll(&mut self, cx: &mut Context<Self>) {
        if self.autoscroll.take().is_some() {
            cx.notify();
        }
    }

    /// Mouse moved while an autoscroll session is live: track the pointer.
    /// The timer loop reads this each tick to compute scroll velocity.
    pub(crate) fn autoscroll_move(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if let Some(a) = self.autoscroll.as_mut() {
            a.current = position;
            cx.notify();
        }
    }

    /// One tick of a live autoscroll session: nudges the grid's scroll offset
    /// toward the pointer at a speed proportional to its distance from the
    /// origin. Returns whether the session is still live — a stale/superseded
    /// epoch or a cancelled session stops the loop.
    fn autoscroll_tick(&mut self, epoch: u64, cx: &mut Context<Self>) -> bool {
        let Some(a) = self.autoscroll.as_ref() else {
            return false;
        };
        if a.epoch != epoch {
            return false;
        }
        let dx = f32::from(a.current.x - a.origin.x);
        let dy = f32::from(a.current.y - a.origin.y);
        let (vx, vy) = (speed(dx), speed(dy));
        if vx != 0.0 {
            let off = a.h_scroll.offset();
            a.h_scroll.set_offset(point(off.x - px(vx), off.y));
        }
        if vy != 0.0 {
            let off = a.scroll.offset();
            a.scroll.set_offset(point(off.x, off.y - px(vy)));
        }
        if vx != 0.0 || vy != 0.0 {
            cx.notify();
        }
        true
    }
}

/// The floating origin indicator shown while autoscroll is live: a small
/// ringed dot at the click point, like a browser's autoscroll cursor anchor.
pub(crate) fn indicator(bg: Hsla, border: Hsla) -> gpui::AnyElement {
    gpui::div()
        .size(px(14.))
        .rounded_full()
        .bg(bg)
        .border_2()
        .border_color(border)
        .into_any_element()
}
