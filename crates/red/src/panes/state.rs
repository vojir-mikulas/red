//! [`PaneLayout`]: the layout tree plus the per-pane UI state that has to be
//! minted and dropped in lockstep with it.
//!
//! Giving each pane its own state is not cosmetic. The two-half layout shared a
//! single tab-strip scroll handle and a single editor/result ratio between its
//! halves, so scrolling one strip scrolled the other and dragging one pane's
//! divider moved both. At two panes that reads as a quirk; at five it is
//! unusable. Everything a pane owns alone lives in [`PaneUi`], keyed by
//! [`PaneId`] — and because ids are never reused, a fresh pane can never inherit
//! a dead one's scroll offset or focus.
//!
//! The tree is the source of truth for which panes exist; [`PaneLayout::sync`]
//! reconciles the `PaneUi` map after any structural change.

use std::collections::HashMap;

use flint::{DividerDrag, DragAnchor};
use gpui::{App, FocusHandle, Pixels, ScrollHandle, px};

use super::tree::{DropZone, PaneId, PaneTree};

/// Child indices from the tree's root down to a split node — how the renderer
/// names the split a divider belongs to.
pub(crate) type SplitPath = Vec<usize>;

/// Starting height of a pane's editor, above its result grid.
const DEFAULT_EDITOR_H: f32 = 300.;

/// Smallest fraction of its split a pane may be dragged down to. The pixel
/// minimum is enforced by the stack's flex `min_w`/`min_h`; this stops the
/// *weights* from collapsing to zero behind it, which would leave a pane that
/// re-expands to nothing the next time the window is resized.
pub(crate) const MIN_PANE_WEIGHT: f32 = 0.05;

/// A stable element-id fragment for the split at `path` — `"root"`, `"0"`,
/// `"0-1"` — so sibling stacks never collide.
pub(crate) fn path_id(path: &[usize]) -> String {
    if path.is_empty() {
        return "root".to_string();
    }
    path.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("-")
}

/// Where the in-flight tab drag would land: the pane under the cursor and the
/// zone within it. `allowed` is false when the implied split wouldn't fit, which
/// draws the zone muted and makes the drop fall back to a plain move — better
/// than silently producing a pane too narrow to use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DropTarget {
    pub(crate) pane: PaneId,
    pub(crate) zone: DropZone,
    pub(crate) allowed: bool,
}

/// UI state belonging to one pane rather than to the workspace.
pub(crate) struct PaneUi {
    /// The tab this pane shows, as an index into the workspace's `tabs`. Only a
    /// hint: a pane whose stored index no longer names one of its own tabs falls
    /// back to its first (see `TabWorkspace::pane_active`), so an index left
    /// stale by a close or reorder can never render another pane's tab.
    pub(crate) active_tab: usize,
    /// Horizontal scroll of this pane's tab strip.
    pub(crate) tab_scroll: ScrollHandle,
    /// Height of this pane's editor, above its result grid.
    pub(crate) editor_h: Pixels,
    pub(crate) editor_drag: Option<DragAnchor>,
    /// Focus anchor for this pane's result grid, so keyboard focus between grids
    /// is unambiguous. Minted once per frame by
    /// [`ensure_focus_handles`](PaneLayout::ensure_focus_handles) rather than at
    /// pane creation: a handle needs an `App`, and requiring one would thread a
    /// `cx` through every layout mutation — including the pure ones that are
    /// unit-tested without a window. Only the SQL workspace focuses a grid; the
    /// Redis and MongoDB workspaces leave it untouched.
    pub(crate) grid_focus: Option<FocusHandle>,
}

impl Default for PaneUi {
    fn default() -> Self {
        Self {
            active_tab: 0,
            tab_scroll: ScrollHandle::new(),
            editor_h: px(DEFAULT_EDITOR_H),
            editor_drag: None,
            grid_focus: None,
        }
    }
}

/// One workspace's pane layout: the geometry, the per-pane state, and the
/// transient feedback an in-flight tab drag paints.
pub(crate) struct PaneLayout {
    tree: PaneTree,
    ui: HashMap<PaneId, PaneUi>,
    drop: Option<DropTarget>,
    /// While a tab is dragged over a strip, the pane whose strip it is and the
    /// gap (an insertion index `0..=tabs.len()`) it would land in. Held per pane
    /// so two strips can't paint the same insertion bar at once.
    gap: Option<(PaneId, usize)>,
    /// The divider drag in flight, and which split it belongs to. Scoped by path
    /// so a nested stack's overlay can't hijack its parent's drag.
    divider: Option<(SplitPath, DividerDrag)>,
}

impl Default for PaneLayout {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneLayout {
    /// A single pane holding focus: the layout every workspace opens in.
    pub(crate) fn new() -> Self {
        let tree = PaneTree::new();
        let ui = HashMap::from([(tree.focus(), PaneUi::default())]);
        Self {
            tree,
            ui,
            drop: None,
            gap: None,
            divider: None,
        }
    }

    /// Rebuild a layout around a restored [`PaneTree`], minting the per-pane UI
    /// state each surviving pane needs. The transient fields (an in-flight drag,
    /// a drop target, a divider grab) start empty: none of them can be mid-flight
    /// at restore.
    pub(crate) fn restore(tree: PaneTree) -> Self {
        let mut layout = Self {
            tree,
            ui: HashMap::new(),
            drop: None,
            gap: None,
            divider: None,
        };
        layout.sync();
        layout
    }

    // --- geometry (delegated, so callers never reach through to the tree) ---

    pub(crate) fn tree(&self) -> &PaneTree {
        &self.tree
    }

    pub(crate) fn focus(&self) -> PaneId {
        self.tree.focus()
    }

    pub(crate) fn set_focus(&mut self, pane: PaneId) -> bool {
        self.tree.set_focus(pane)
    }

    pub(crate) fn panes(&self) -> Vec<PaneId> {
        self.tree.panes()
    }

    pub(crate) fn is_split(&self) -> bool {
        self.tree.is_split()
    }

    pub(crate) fn zoomed(&self) -> Option<PaneId> {
        self.tree.zoomed()
    }

    pub(crate) fn zoom_toggle(&mut self, pane: PaneId) {
        self.tree.zoom_toggle(pane);
    }

    pub(crate) fn cycle(&self, from: PaneId, forward: bool) -> Option<PaneId> {
        self.tree.cycle(from, forward)
    }

    pub(crate) fn equalize(&mut self) {
        self.tree.equalize();
    }

    pub(crate) fn set_weight(
        &mut self,
        path: &[usize],
        gutter: usize,
        leading: f32,
        min: f32,
    ) -> bool {
        self.tree.set_weight(path, gutter, leading, min)
    }

    /// The divider drag in flight on the split at `path`, if any. The renderer
    /// hands this straight to that split's `SplitStack`.
    pub(crate) fn divider_drag(&self, path: &[usize]) -> Option<DividerDrag> {
        self.divider
            .as_ref()
            .filter(|(p, _)| p.as_slice() == path)
            .map(|(_, d)| *d)
    }

    pub(crate) fn begin_divider_drag(&mut self, path: SplitPath, drag: DividerDrag) {
        self.divider = Some((path, drag));
    }

    pub(crate) fn end_divider_drag(&mut self) -> bool {
        self.divider.take().is_some()
    }

    /// Split `at` and return the new pane, with its UI state minted. `None` when
    /// the zone creates nothing ([`DropZone::Center`]) or `at` is unknown.
    pub(crate) fn insert(&mut self, at: PaneId, zone: DropZone) -> Option<PaneId> {
        let new = self.tree.insert(at, zone)?;
        self.sync();
        Some(new)
    }

    /// Drop `pane`, returning the pane its tabs and focus fold into. `None` (and
    /// no change) when it is the last pane.
    pub(crate) fn remove(&mut self, pane: PaneId) -> Option<PaneId> {
        let heir = self.tree.remove(pane)?;
        self.sync();
        Some(heir)
    }

    /// Reconcile the per-pane state with the tree: mint state for panes that
    /// gained it, drop state for panes that are gone. Called after every
    /// structural change, so no caller has to remember to.
    pub(crate) fn sync(&mut self) {
        let live = self.tree.panes();
        for pane in &live {
            self.ui.entry(*pane).or_default();
        }
        self.ui.retain(|id, _| live.contains(id));
        if self.gap.is_some_and(|(p, _)| !live.contains(&p)) {
            self.gap = None;
        }
        if self.drop.is_some_and(|d| !live.contains(&d.pane)) {
            self.drop = None;
        }
    }

    /// Give every pane a result-grid focus anchor. Called once per frame from the
    /// root render, which is the first point after a layout change that holds an
    /// `App` — see [`PaneUi::grid_focus`].
    pub(crate) fn ensure_focus_handles(&mut self, cx: &mut App) {
        for ui in self.ui.values_mut() {
            if ui.grid_focus.is_none() {
                ui.grid_focus = Some(cx.focus_handle());
            }
        }
    }

    // --- per-pane state ---

    pub(crate) fn ui(&self, pane: PaneId) -> Option<&PaneUi> {
        self.ui.get(&pane)
    }

    pub(crate) fn ui_mut(&mut self, pane: PaneId) -> Option<&mut PaneUi> {
        self.ui.get_mut(&pane)
    }

    /// The tab index `pane` last showed. Callers resolve it against the pane's
    /// actual tabs (see `TabWorkspace::pane_active`) rather than trusting it.
    pub(crate) fn active_tab(&self, pane: PaneId) -> Option<usize> {
        self.ui.get(&pane).map(|u| u.active_tab)
    }

    pub(crate) fn set_active_tab(&mut self, pane: PaneId, index: usize) {
        if let Some(u) = self.ui.get_mut(&pane) {
            u.active_tab = index;
        }
    }

    /// Shift every pane's stored tab index across a `tabs` mutation, so a close
    /// or reorder can't leave a pane pointing at whatever slid into the slot.
    pub(crate) fn remap_active_tabs(&mut self, remap: impl Fn(usize) -> usize) {
        for u in self.ui.values_mut() {
            u.active_tab = remap(u.active_tab);
        }
    }

    pub(crate) fn grid_focus(&self, pane: PaneId) -> Option<&FocusHandle> {
        self.ui.get(&pane).and_then(|u| u.grid_focus.as_ref())
    }

    // --- drag feedback ---

    pub(crate) fn drop_target(&self) -> Option<DropTarget> {
        self.drop
    }

    /// Record where the dragged tab would land. Returns whether it changed, so
    /// the caller repaints only when the highlight actually moves — this fires on
    /// every mouse move during a drag.
    pub(crate) fn set_drop_target(&mut self, target: DropTarget) -> bool {
        if self.drop != Some(target) {
            self.drop = Some(target);
            return true;
        }
        false
    }

    pub(crate) fn clear_drop_target(&mut self) -> bool {
        self.drop.take().is_some()
    }

    /// Release the highlight, but only if `pane` is the one holding it.
    ///
    /// The ownership check is what makes this safe to call from every pane on
    /// every mouse move: GPUI dispatches a drag-move to *all* of them, so an
    /// unconditional clear would have each pane wipe whichever neighbour had
    /// legitimately claimed the cursor, and the highlight would flicker or
    /// vanish depending on dispatch order.
    pub(crate) fn clear_drop_target_of(&mut self, pane: PaneId) -> bool {
        if self.drop.is_some_and(|t| t.pane == pane) {
            self.drop = None;
            return true;
        }
        false
    }

    /// The strip insertion gap, if the drag is over `pane`'s strip.
    pub(crate) fn gap_in(&self, pane: PaneId) -> Option<usize> {
        self.gap.filter(|(p, _)| *p == pane).map(|(_, g)| g)
    }

    pub(crate) fn set_gap(&mut self, pane: PaneId, gap: usize) -> bool {
        if self.gap != Some((pane, gap)) {
            self.gap = Some((pane, gap));
            // A drag over a strip is a reorder, not a split: drop the body
            // highlight so the two never paint at once.
            self.drop = None;
            return true;
        }
        false
    }

    /// Release the strip insertion bar, but only if `pane`'s strip is showing it.
    ///
    /// Same ownership check as [`Self::clear_drop_target_of`], and for the same
    /// reason: with strips side by side, a drag crossing from one to the next
    /// makes both fire on the same move, and an unconditional clear would race
    /// the neighbour's legitimate claim.
    pub(crate) fn clear_gap_of(&mut self, pane: PaneId) -> bool {
        if self.gap.is_some_and(|(p, _)| p == pane) {
            self.gap = None;
            return true;
        }
        false
    }

    /// Drop all transient drag feedback (the drag ended, one way or another).
    pub(crate) fn clear_drag(&mut self) -> bool {
        let had = self.gap.is_some() || self.drop.is_some();
        self.gap = None;
        self.drop = None;
        had
    }
}
