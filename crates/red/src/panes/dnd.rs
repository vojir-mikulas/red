//! The drag-and-drop half of the pane layout: turning a cursor position over a
//! pane into a [`DropZone`], and painting where the dragged tab would land.
//!
//! This is what makes splitting a gesture rather than a mode. Dragging a tab
//! over the middle of a pane moves it into that pane; dragging it near an edge
//! offers a *new* pane on that side, including the very first split of an
//! undivided work area.

use gpui::{AnyElement, Bounds, Hsla, Pixels, Point, div, prelude::*, px, relative};

use super::state::{DropTarget, PaneLayout};
use super::tree::{DropZone, PaneId, split_fits};

/// How much of a pane's width/height each edge band claims. Small enough that
/// the centre stays the easy target (moving a tab between panes is the common
/// gesture), large enough to hit without aiming.
const EDGE_FRACTION: f32 = 0.22;

/// Cap on an edge band, so a very wide pane doesn't turn a third of the screen
/// into a split target.
const EDGE_MAX: f32 = 160.;

/// The pane's geometry limits, as the drop zones need them: the smallest either
/// half of a split may be, and the height of the tab strip along the top (which
/// belongs to the strip's own reorder gesture, not to the split zones).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PaneLimits {
    pub(crate) min_w: f32,
    pub(crate) min_h: f32,
    pub(crate) strip_h: f32,
}

/// The tab being dragged, as the drop zones need to see it: which pane it comes
/// from, and whether it is that pane's only tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DraggedTab {
    pub(crate) from: PaneId,
    pub(crate) alone: bool,
}

impl DraggedTab {
    /// Whether dropping into `zone` of `target` would actually change anything.
    ///
    /// Two drops are no-ops, and the difference matters because a zone that
    /// highlights but does nothing is worse than no highlight at all: the user
    /// aims, commits, and the layout ignores them. A tab dropped into the middle
    /// of the pane it already lives in goes nowhere; and a pane's *only* tab
    /// dropped on that pane's own edge would mint a pane, empty the source, and
    /// have it normalized straight back away.
    pub(crate) fn would_move(self, target: PaneId, zone: DropZone) -> bool {
        if self.from != target {
            return true;
        }
        zone != DropZone::Center && !self.alone
    }
}

/// Where a tab dropped at `p` over a pane occupying `bounds` would land.
///
/// Corners resolve to whichever axis the cursor is deeper into, measured as a
/// fraction of that band: without this, the corner of a short wide pane would
/// always read as vertical simply because its bands are thinner.
pub(crate) fn zone_at(bounds: Bounds<Pixels>, p: Point<Pixels>) -> DropZone {
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let x = f32::from(p.x - bounds.origin.x);
    let y = f32::from(p.y - bounds.origin.y);
    let band_x = (w * EDGE_FRACTION).min(EDGE_MAX);
    let band_y = (h * EDGE_FRACTION).min(EDGE_MAX);

    // Depth into each band as a 0..1 fraction; 0 means "not in that band".
    let left = if band_x > 0. {
        (1. - x / band_x).max(0.)
    } else {
        0.
    };
    let right = if band_x > 0. {
        (1. - (w - x) / band_x).max(0.)
    } else {
        0.
    };
    let top = if band_y > 0. {
        (1. - y / band_y).max(0.)
    } else {
        0.
    };
    let bottom = if band_y > 0. {
        (1. - (h - y) / band_y).max(0.)
    } else {
        0.
    };

    let horizontal = left.max(right);
    let vertical = top.max(bottom);
    if horizontal <= 0. && vertical <= 0. {
        return DropZone::Center;
    }
    if horizontal >= vertical {
        if left >= right {
            DropZone::Left
        } else {
            DropZone::Right
        }
    } else if top >= bottom {
        DropZone::Top
    } else {
        DropZone::Bottom
    }
}

/// Whether the split `zone` implies would leave both halves usable. Always true
/// for [`DropZone::Center`], which splits nothing.
pub(crate) fn zone_fits(bounds: Bounds<Pixels>, zone: DropZone, min_w: f32, min_h: f32) -> bool {
    match zone {
        DropZone::Center => true,
        DropZone::Left | DropZone::Right => split_fits(f32::from(bounds.size.width), min_w),
        DropZone::Top | DropZone::Bottom => split_fits(f32::from(bounds.size.height), min_h),
    }
}

/// Resolve the cursor at `p` over `pane` (occupying `bounds`) into a drop target
/// and record it on `layout`. Returns whether the highlight moved, so the caller
/// repaints only on a real change — this runs on every mouse move of a drag.
///
/// Nothing is highlighted where nothing would happen: over the tab strip (whose
/// own insertion bar owns that band), or on a drop [`DraggedTab::would_move`]
/// rules out. The strip band has to be excluded here rather than by letting the
/// strip stop the event, because `on_drag_move` is dispatched in the capture
/// phase — the pane's handler runs first, so by the time the strip could stop
/// propagation the zone would already be claimed and both indicators would paint.
pub(crate) fn aim(
    layout: &mut PaneLayout,
    pane: PaneId,
    bounds: Bounds<Pixels>,
    p: Point<Pixels>,
    limits: PaneLimits,
    dragged: DraggedTab,
) -> bool {
    // GPUI delivers drag-moves to every registered pane, not just the hovered
    // one, so a pane the cursor has left must not keep claiming the highlight.
    let inside = p.x >= bounds.origin.x
        && p.x < bounds.origin.x + bounds.size.width
        && p.y >= bounds.origin.y
        && p.y < bounds.origin.y + bounds.size.height;
    if !inside {
        return false;
    }
    if f32::from(p.y - bounds.origin.y) < limits.strip_h {
        return layout.clear_drop_target();
    }
    let zone = zone_at(bounds, p);
    if !dragged.would_move(pane, zone) {
        return layout.clear_drop_target();
    }
    layout.set_drop_target(DropTarget {
        pane,
        zone,
        allowed: zone_fits(bounds, zone, limits.min_w, limits.min_h),
    })
}

/// The highlight painted over the pane under the cursor: the region the tab
/// would occupy — half the pane for an edge, all of it for the centre.
///
/// `allowed` false means the split wouldn't fit; the same region is drawn in a
/// muted grey so the gesture reads as refused rather than simply ignored. Purely
/// decorative: it never intercepts the drop, which the pane body handles.
pub(crate) fn drop_overlay(zone: DropZone, allowed: bool, accent: Hsla, muted: Hsla) -> AnyElement {
    let color = if allowed { accent } else { muted };
    let region = div()
        .absolute()
        .border_1()
        .border_color(color)
        .bg(color.opacity(if allowed { 0.10 } else { 0.06 }))
        .rounded(px(4.));
    let region = match zone {
        DropZone::Center => region.inset_0(),
        DropZone::Left => region.left_0().top_0().bottom_0().w(relative(0.5)),
        DropZone::Right => region.right_0().top_0().bottom_0().w(relative(0.5)),
        DropZone::Top => region.top_0().left_0().right_0().h(relative(0.5)),
        DropZone::Bottom => region.bottom_0().left_0().right_0().h(relative(0.5)),
    };
    // The wrapper covers the pane so the region's percentage sizes resolve
    // against it; neither layer takes hits, so the body's own drop still fires.
    div()
        .absolute()
        .inset_0()
        .p(px(4.))
        .child(region)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Size, point};

    fn bounds(w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(100.), px(50.)),
            size: Size {
                width: px(w),
                height: px(h),
            },
        }
    }

    fn at(b: Bounds<Pixels>, x: f32, y: f32) -> DropZone {
        zone_at(b, point(b.origin.x + px(x), b.origin.y + px(y)))
    }

    const LIMITS: PaneLimits = PaneLimits {
        min_w: 320.,
        min_h: 180.,
        strip_h: 35.,
    };

    /// A tab dragged from `from` that is not its pane's only one.
    fn among_others(from: PaneId) -> DraggedTab {
        DraggedTab { from, alone: false }
    }

    #[test]
    fn the_middle_of_a_pane_is_the_centre_zone() {
        let b = bounds(800., 600.);
        assert_eq!(at(b, 400., 300.), DropZone::Center);
        // Just inside the bands on every side is still centre.
        assert_eq!(at(b, 170., 300.), DropZone::Center);
        assert_eq!(at(b, 630., 300.), DropZone::Center);
    }

    #[test]
    fn each_edge_band_names_its_own_side() {
        let b = bounds(800., 600.);
        assert_eq!(at(b, 5., 300.), DropZone::Left);
        assert_eq!(at(b, 795., 300.), DropZone::Right);
        assert_eq!(at(b, 400., 5.), DropZone::Top);
        assert_eq!(at(b, 400., 595.), DropZone::Bottom);
    }

    #[test]
    fn a_corner_resolves_to_the_deeper_axis() {
        // Short and wide: the vertical bands are much thinner than the
        // horizontal ones, so a naive pixel comparison would always pick
        // horizontal. Deep into the top band but barely into the left one must
        // still read as Top.
        let b = bounds(1200., 200.);
        assert_eq!(at(b, 130., 2.), DropZone::Top);
        // ...and the mirror case: deep left, barely below the top band.
        assert_eq!(at(b, 2., 44.), DropZone::Left);
    }

    #[test]
    fn bands_are_capped_on_a_very_wide_pane() {
        // 22% of 2000px would be 440px; the cap holds it at 160px, so a point
        // 300px in is comfortably centre.
        let b = bounds(2000., 600.);
        assert_eq!(at(b, 300., 300.), DropZone::Center);
        assert_eq!(at(b, 100., 300.), DropZone::Left);
    }

    #[test]
    fn a_pane_too_small_to_split_refuses_the_edge_zones() {
        let b = bounds(500., 300.);
        // 500/2 = 250, under a 320 minimum width.
        assert!(!zone_fits(b, DropZone::Right, 320., 180.));
        // 300/2 = 150, under a 180 minimum height.
        assert!(!zone_fits(b, DropZone::Bottom, 320., 180.));
        // A plain move into the pane is always allowed.
        assert!(zone_fits(b, DropZone::Center, 320., 180.));
        // Roomy enough and both fit.
        let big = bounds(900., 700.);
        assert!(zone_fits(big, DropZone::Right, 320., 180.));
        assert!(zone_fits(big, DropZone::Bottom, 320., 180.));
    }

    #[test]
    fn a_drag_over_the_tab_strip_claims_no_zone() {
        let mut layout = PaneLayout::new();
        let pane = layout.focus();
        let b = bounds(800., 600.);
        // An edge of the pane, which a tab with siblings may genuinely split.
        let body = point(b.origin.x + px(795.), b.origin.y + px(300.));
        assert!(aim(&mut layout, pane, b, body, LIMITS, among_others(pane)));
        assert!(layout.drop_target().is_some());
        // Back up into the strip band: the reorder gesture owns it, so the split
        // highlight has to go — otherwise both indicators paint at once.
        let strip = point(b.origin.x + px(795.), b.origin.y + px(10.));
        assert!(aim(&mut layout, pane, b, strip, LIMITS, among_others(pane)));
        assert!(layout.drop_target().is_none());
    }

    #[test]
    fn a_drag_outside_a_pane_leaves_its_zone_alone() {
        let mut layout = PaneLayout::new();
        let pane = layout.focus();
        let b = bounds(800., 600.);
        let outside = point(b.origin.x - px(20.), b.origin.y + px(300.));
        assert!(!aim(
            &mut layout,
            pane,
            b,
            outside,
            LIMITS,
            among_others(pane)
        ));
        assert!(layout.drop_target().is_none());
    }

    #[test]
    fn a_lone_tab_gets_no_zone_over_its_own_pane() {
        let mut layout = PaneLayout::new();
        let pane = layout.focus();
        let lone = DraggedTab {
            from: pane,
            alone: true,
        };
        let b = bounds(800., 600.);
        // Every edge of its own pane would split and immediately collapse, so
        // none of them may offer the affordance.
        for (x, y) in [(5., 300.), (795., 300.), (400., 60.), (400., 595.)] {
            let p = point(b.origin.x + px(x), b.origin.y + px(y));
            aim(&mut layout, pane, b, p, LIMITS, lone);
            assert!(
                layout.drop_target().is_none(),
                "a lone tab was offered a split of its own pane at ({x}, {y})"
            );
        }
    }

    #[test]
    fn a_tab_among_others_may_still_split_its_own_pane() {
        let mut layout = PaneLayout::new();
        let pane = layout.focus();
        let b = bounds(800., 600.);
        let p = point(b.origin.x + px(795.), b.origin.y + px(300.));
        assert!(aim(&mut layout, pane, b, p, LIMITS, among_others(pane)));
        assert_eq!(layout.drop_target().map(|t| t.zone), Some(DropZone::Right));
    }

    #[test]
    fn the_centre_of_a_tabs_own_pane_offers_nothing() {
        let mut layout = PaneLayout::new();
        let pane = layout.focus();
        let b = bounds(800., 600.);
        let p = point(b.origin.x + px(400.), b.origin.y + px(300.));
        // Dropping a tab into the pane it already lives in moves nothing,
        // whether or not it has siblings.
        aim(&mut layout, pane, b, p, LIMITS, among_others(pane));
        assert!(layout.drop_target().is_none());
    }

    #[test]
    fn a_lone_tab_may_still_be_dropped_on_another_pane() {
        let mut layout = PaneLayout::new();
        let home = layout.focus();
        let other = layout.insert(home, DropZone::Right).expect("split");
        let lone = DraggedTab {
            from: home,
            alone: true,
        };
        let b = bounds(800., 600.);
        // Moving it out of its pane is a real change: the source pane collapses
        // and the tab lands where it was aimed.
        let p = point(b.origin.x + px(795.), b.origin.y + px(300.));
        assert!(aim(&mut layout, other, b, p, LIMITS, lone));
        assert_eq!(layout.drop_target().map(|t| t.pane), Some(other));
    }

    #[test]
    fn a_zero_sized_pane_does_not_divide_by_zero() {
        let b = bounds(0., 0.);
        assert_eq!(at(b, 0., 0.), DropZone::Center);
    }
}
