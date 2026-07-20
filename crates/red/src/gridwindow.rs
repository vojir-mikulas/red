//! Virtual-scroll window arithmetic, shared by every windowed grid (the SQL
//! result grid and the MongoDB document grid).
//!
//! A `uniform_list` places each row at `index * row_height` in `f32`, exact only
//! to about `2^24` px (~16.7M). Past that, positions quantize: rows overlap,
//! double up, and the wheel sticks. So a list over a result of tens of millions
//! of rows never spans the whole result: it lays out at most [`WINDOW`] rows (a
//! `WINDOW * row_height` canvas, well under the ceiling), and a `window_base`
//! that the caller keeps slides that window across the full result. The
//! fraction-mapped scrollbar drives long jumps; wheel scrolling re-centers the
//! window near its edges ([`window_decision`]).
//!
//! Everything here is pure: the gpui pixel-offset compensation that keeps the
//! visible rows still when the window slides stays in each caller (it touches the
//! caller's own scroll handle), but the *decision* to slide, and by how much,
//! lives here so both grids share one tested implementation.

/// Physical rows a windowed list lays out at once. A `WINDOW * row_height`
/// canvas stays well under the `f32` layout ceiling (see the module docs).
pub(crate) const WINDOW: usize = 100_000;

/// When the viewport scrolls within this many rows of a window edge (and more
/// result exists beyond that edge), the window re-centers on the viewport,
/// compensating the list's pixel offset so the visible rows don't move.
const REANCHOR_MARGIN: usize = 5_000;

/// The virtual-scroll window resolved for one render.
pub(crate) struct WindowView {
    /// Absolute ordinal of list-local index 0.
    pub(crate) base: usize,
    /// Physical rows fed to `uniform_list` this frame (`total.min(WINDOW)`).
    pub(crate) len: usize,
    /// Scrollbar thumb position, `0..=1`, over the *whole* result.
    pub(crate) fraction: f32,
    /// Scrollbar thumb size (viewport / total).
    pub(crate) thumb: f32,
}

/// Given the result `total`, the current window `base`, the viewport's top row in
/// list-local coordinates, and the viewport height in rows, returns the base to
/// use this frame and, when it changed, the list-local row the pixel offset must
/// be re-anchored onto so the visible rows don't move.
///
/// The window re-centers on the viewport once it scrolls within
/// [`REANCHOR_MARGIN`] of an edge that has more result beyond it.
pub(crate) fn window_decision(
    total: usize,
    base: usize,
    local_first: usize,
    viewport_rows: usize,
) -> (usize, Option<usize>) {
    if total <= WINDOW {
        return (0, None);
    }
    let max_base = total - WINDOW;
    let base = base.min(max_base);
    let abs_first = base + local_first;
    let near_top = base > 0 && local_first < REANCHOR_MARGIN;
    let near_bottom = base < max_base && local_first + viewport_rows + REANCHOR_MARGIN > WINDOW;
    if near_top || near_bottom {
        let desired = abs_first.saturating_sub(WINDOW / 2).min(max_base);
        if desired != base {
            return (desired, Some(abs_first - desired));
        }
    }
    (base, None)
}

/// The scrollbar's fraction (thumb position, `0..=1`) and thumb size for a
/// viewport whose top absolute row is `abs_first`. Both map over the *whole*
/// result, not the resident window, so the thumb reflects where the viewport
/// sits in all N rows.
pub(crate) fn scrollbar_metrics(
    total: usize,
    abs_first: usize,
    viewport_rows: usize,
) -> (f32, f32) {
    let denom = total.saturating_sub(viewport_rows).max(1) as f32;
    let fraction = (abs_first as f32 / denom).clamp(0.0, 1.0);
    let thumb = if total > 0 {
        (viewport_rows as f32 / total as f32).clamp(0.0, 1.0)
    } else {
        1.0
    };
    (fraction, thumb)
}

/// The window base that centers the viewport on `target`, clamped to the result.
/// Used by an explicit far jump (a scrollbar scrub or go-to-row) to place the
/// window before the run reloads there.
pub(crate) fn centered_base(total: usize, target: usize) -> usize {
    if total > WINDOW {
        target.saturating_sub(WINDOW / 2).min(total - WINDOW)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small result fits in one window: never windowed, never re-anchored.
    #[test]
    fn small_result_is_never_windowed() {
        assert_eq!(window_decision(WINDOW, 0, 0, 30), (0, None));
        assert_eq!(window_decision(WINDOW, 0, WINDOW - 1, 30), (0, None));
        assert_eq!(window_decision(500, 0, 400, 30), (0, None));
    }

    /// At rest in the middle of a window, with margins on both sides, nothing
    /// moves.
    #[test]
    fn mid_window_holds_still() {
        let total = 50_000_000;
        assert_eq!(
            window_decision(total, 1_000_000, WINDOW / 2, 30),
            (1_000_000, None)
        );
    }

    /// Scrolling near the bottom edge re-centers the window forward and reports
    /// the local row to pin the offset to (so the visible rows don't jump).
    #[test]
    fn near_bottom_recenters_forward() {
        let total = 50_000_000;
        let base = 1_000_000;
        let local_first = WINDOW - 100; // viewport top is 100 rows from the edge
        let (new_base, reanchor) = window_decision(total, base, local_first, 30);
        let abs_first = base + local_first;
        assert!(new_base > base);
        assert_eq!(reanchor, Some(abs_first - new_base));
        assert_eq!(new_base + reanchor.unwrap(), abs_first); // same absolute row
        assert_eq!(reanchor.unwrap(), WINDOW / 2);
    }

    /// Scrolling back near the top edge re-centers the window backward.
    #[test]
    fn near_top_recenters_backward() {
        let total = 50_000_000;
        let base = 1_000_000;
        let local_first = 100;
        let (new_base, reanchor) = window_decision(total, base, local_first, 30);
        let abs_first = base + local_first;
        assert!(new_base < base);
        assert_eq!(new_base + reanchor.unwrap(), abs_first);
        assert_eq!(reanchor.unwrap(), WINDOW / 2);
    }

    /// Near the result's true start the window can't slide further: clamps to 0,
    /// and once the viewport is genuinely at the top it stays put.
    #[test]
    fn clamps_at_result_start() {
        let total = 50_000_000;
        let (new_base, _) = window_decision(total, 10_000, 100, 30);
        assert_eq!(new_base, 0);
        assert_eq!(window_decision(total, 0, 0, 30), (0, None));
    }

    /// Near the result's true end the base clamps to `total - WINDOW`.
    #[test]
    fn clamps_at_result_end() {
        let total = 50_000_000;
        let max_base = total - WINDOW;
        assert_eq!(
            window_decision(total, max_base, WINDOW - 50, 30),
            (max_base, None)
        );
    }

    /// The thumb shrinks and the fraction tracks the absolute position.
    #[test]
    fn scrollbar_maps_over_the_whole_result() {
        let (fraction, thumb) = scrollbar_metrics(1_000_000, 0, 40);
        assert_eq!(fraction, 0.0);
        assert!(thumb > 0.0 && thumb < 0.001);
        let (fraction, _) = scrollbar_metrics(1_000_000, 999_960, 40);
        assert!((fraction - 1.0).abs() < 1e-6);
    }

    /// `centered_base` puts a far target near the middle of its window, clamped
    /// to the result's ends.
    #[test]
    fn centered_base_clamps() {
        assert_eq!(centered_base(500, 400), 0); // fits one window
        let total = 50_000_000;
        assert_eq!(centered_base(total, 0), 0);
        assert_eq!(centered_base(total, total - 1), total - WINDOW);
        assert_eq!(centered_base(total, 1_000_000), 1_000_000 - WINDOW / 2);
    }
}
