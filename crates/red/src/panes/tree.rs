//! The work-area layout: a tree whose leaves are panes and whose interior nodes
//! are splits along one axis. This is the geometry half of the pane model and
//! knows nothing about tabs, GPUI, or what a pane renders — pane *membership*
//! stays a field on each tab, so this file stays pure and unit-testable.
//!
//! Two invariants shape every operation and are restored by [`PaneTree::normalize`]:
//!
//! - **Splits are flat.** A split never holds a same-axis split as a direct
//!   child; splitting the rightmost of three columns to the right appends a
//!   fourth child rather than nesting. Without this, N presses of ⌘\ would build
//!   an N-deep staircase whose dividers resize increasingly small subtrees.
//! - **A split has at least two children.** Removing the second-to-last child
//!   collapses the split into its survivor, which then re-flattens into *its*
//!   parent. This is what makes closing a pane restore the previous layout
//!   exactly, instead of leaving invisible one-child wrappers behind.
//!
//! Weights are fractions of their split (each split's children sum to 1.0), not
//! pixel sizes. Fractions are what let a five-column layout reflow sensibly when
//! the window resizes; a pixel-sized model pins every pane but the last.

/// Stable identity of a pane. Ids are never reused within a tree, so per-pane UI
/// state keyed by id (scroll offsets, focus handles) can't be inherited by a
/// later pane that happens to occupy the same slot.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct PaneId(pub(crate) u32);

impl PaneId {
    /// The pane a fresh [`PaneTree`] starts with. A tab built before its pane is
    /// known (the connection's first tab, or one minted for a push) carries this
    /// until the push assigns the real one.
    pub(crate) const FIRST: PaneId = PaneId(0);
}

/// The axis a split divides along: `Horizontal` puts its children side by side
/// (columns), `Vertical` stacks them (rows).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SplitAxis {
    Horizontal,
    Vertical,
}

/// Where a dragged tab would land relative to the pane under the cursor: into it
/// (`Center`), or into a new pane on one of its four sides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DropZone {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

impl DropZone {
    /// The split this zone asks for: the axis, and whether the new pane goes
    /// *before* the target along it. `None` for `Center`, which moves the tab
    /// into the existing pane and creates nothing.
    pub(crate) fn split(self) -> Option<(SplitAxis, bool)> {
        match self {
            DropZone::Center => None,
            DropZone::Left => Some((SplitAxis::Horizontal, true)),
            DropZone::Right => Some((SplitAxis::Horizontal, false)),
            DropZone::Top => Some((SplitAxis::Vertical, true)),
            DropZone::Bottom => Some((SplitAxis::Vertical, false)),
        }
    }
}

/// One slot of a split: the fraction of the split it occupies, and what's in it.
/// Weight and child live together so the two can't desync the way parallel
/// `children`/`weights` vectors would.
pub(crate) struct Child {
    pub(crate) weight: f32,
    pub(crate) node: Node,
}

impl Child {
    fn new(weight: f32, node: Node) -> Self {
        Self { weight, node }
    }
}

/// A node of the layout tree. Exposed so the renderer can walk it; mutation goes
/// through [`PaneTree`], which restores the invariants afterwards.
pub(crate) enum Node {
    Leaf(PaneId),
    Split {
        axis: SplitAxis,
        children: Vec<Child>,
    },
}

/// The pane layout of one workspace: the tree, which pane has focus, and which
/// pane (if any) is temporarily zoomed to fill the work area.
pub(crate) struct PaneTree {
    root: Node,
    focus: PaneId,
    /// Next id to hand out. Monotonic — see [`PaneId`].
    next: u32,
    zoom: Option<PaneId>,
}

impl Default for PaneTree {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneTree {
    /// A single pane holding focus: the unsplit layout every workspace starts in.
    pub(crate) fn new() -> Self {
        Self {
            root: Node::Leaf(PaneId::FIRST),
            focus: PaneId::FIRST,
            next: 1,
            zoom: None,
        }
    }

    /// Rebuild a tree from a persisted shape, then restore the module's
    /// invariants over it.
    ///
    /// [`normalize`](Self::normalize) is run rather than trusted-as-written
    /// because the caller is a *file*: a hand-edited or truncated `state.json`
    /// can name a one-child split or a same-axis nesting that no in-app
    /// operation would ever produce, and every later operation assumes those are
    /// impossible. Focus falls back to the first surviving pane when the stored
    /// one is not in the tree, so a restored workspace always has a focused pane.
    ///
    /// Never zoomed: zoom is a transient "look at this one pane for a moment"
    /// state, and restoring into it would hide the rest of the workspace with no
    /// hint as to why.
    pub(crate) fn restore(root: Node, focus: PaneId, next: u32) -> Self {
        let mut tree = Self {
            root,
            focus,
            // A `next` at or below a live id would hand out a duplicate on the
            // first split; the max guards a file that under-reports it.
            next: next.max(1),
            zoom: None,
        };
        tree.normalize();
        let panes = tree.panes();
        // `normalize` collapses a one-child split but has no answer for a
        // *zero*-child one, which no in-app operation can build and only a
        // corrupt file can supply. A paneless tree would render an empty work
        // area with focus pointing at nothing, so fall back to a fresh layout.
        if panes.is_empty() {
            return Self::new();
        }
        tree.next = tree
            .next
            .max(panes.iter().map(|p| p.0 + 1).max().unwrap_or(1));
        if !tree.contains(tree.focus) {
            tree.focus = panes.first().copied().unwrap_or(PaneId::FIRST);
        }
        tree
    }

    pub(crate) fn root(&self) -> &Node {
        &self.root
    }

    pub(crate) fn focus(&self) -> PaneId {
        self.focus
    }

    /// The next pane id this tree would hand out, so a persisted layout can carry
    /// the counter and restored panes keep their ids.
    pub(crate) fn next_id(&self) -> u32 {
        self.next
    }

    /// Point focus at `pane`. Returns whether it moved, so callers notify only on
    /// a real change.
    pub(crate) fn set_focus(&mut self, pane: PaneId) -> bool {
        if self.focus != pane && self.contains(pane) {
            self.focus = pane;
            return true;
        }
        false
    }

    /// Every pane in visual order (left to right, top to bottom).
    pub(crate) fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        collect(&self.root, &mut out);
        out
    }

    /// Whether the layout holds more than one pane (the work area is divided).
    pub(crate) fn is_split(&self) -> bool {
        matches!(self.root, Node::Split { .. })
    }

    pub(crate) fn contains(&self, pane: PaneId) -> bool {
        self.panes().contains(&pane)
    }

    pub(crate) fn zoomed(&self) -> Option<PaneId> {
        self.zoom
    }

    /// Zoom `pane` to fill the work area, or restore the full layout when it is
    /// already the zoomed one. Zooming an unknown pane clears the zoom.
    pub(crate) fn zoom_toggle(&mut self, pane: PaneId) {
        self.zoom = match self.zoom {
            Some(z) if z == pane => None,
            _ if self.contains(pane) && self.is_split() => Some(pane),
            _ => None,
        };
    }

    /// Split `at` and return the new pane, or `None` for [`DropZone::Center`] (or
    /// an unknown pane), which creates nothing. The new pane takes half of `at`'s
    /// share, so the rest of the layout doesn't shift.
    pub(crate) fn insert(&mut self, at: PaneId, zone: DropZone) -> Option<PaneId> {
        let (axis, before) = zone.split()?;
        if !self.contains(at) {
            return None;
        }
        let new = PaneId(self.next);
        self.next += 1;
        insert_at(&mut self.root, at, axis, before, new);
        // A layout change makes the zoomed single-pane view stale: showing one
        // pane while the user just created another reads as the split failing.
        self.zoom = None;
        self.normalize();
        Some(new)
    }

    /// Drop `pane` from the layout, returning the pane its tabs and focus should
    /// fold into. `None` (and no change) when it's the last pane or unknown — a
    /// workspace always has at least one pane to render.
    pub(crate) fn remove(&mut self, pane: PaneId) -> Option<PaneId> {
        let order = self.panes();
        let pos = order.iter().position(|&p| p == pane)?;
        if order.len() < 2 {
            return None;
        }
        // The visually adjacent pane inherits: the one before it, or the one
        // after when the removed pane was first.
        let heir = if pos > 0 {
            order[pos - 1]
        } else {
            order[pos + 1]
        };
        remove_at(&mut self.root, pane);
        if self.focus == pane {
            self.focus = heir;
        }
        if self.zoom == Some(pane) {
            self.zoom = None;
        }
        self.normalize();
        Some(heir)
    }

    /// Set the weight of the child *before* the divider after `gutter`, within
    /// the split at `path`; the child after it takes the remainder. Only those two
    /// change, so dragging one divider never disturbs the rest of the row. `min`
    /// is the smallest fraction either side may shrink to (the caller converts its
    /// pixel minimum against the measured container).
    pub(crate) fn set_weight(
        &mut self,
        path: &[usize],
        gutter: usize,
        leading: f32,
        min: f32,
    ) -> bool {
        let Some(Node::Split { children, .. }) = node_at_mut(&mut self.root, path) else {
            return false;
        };
        if gutter + 1 >= children.len() {
            return false;
        }
        let pair = children[gutter].weight + children[gutter + 1].weight;
        // A pair too small to honour `min` on both sides splits evenly instead of
        // clamping to an empty range.
        let min = min.max(0.).min(pair / 2.);
        let first = leading.clamp(min, pair - min);
        children[gutter].weight = first;
        children[gutter + 1].weight = pair - first;
        true
    }

    /// Reset every split to even shares.
    pub(crate) fn equalize(&mut self) {
        equalize_node(&mut self.root);
    }

    /// The next pane in visual order, wrapping. `None` with a single pane.
    pub(crate) fn cycle(&self, from: PaneId, forward: bool) -> Option<PaneId> {
        let order = self.panes();
        if order.len() < 2 {
            return None;
        }
        let pos = order.iter().position(|&p| p == from).unwrap_or(0);
        let n = order.len();
        Some(if forward {
            order[(pos + 1) % n]
        } else {
            order[(pos + n - 1) % n]
        })
    }

    /// Restore the invariants after any mutation: flatten same-axis nesting,
    /// collapse one-child splits, renormalize each split's weights, and re-point
    /// focus and zoom at panes that still exist.
    pub(crate) fn normalize(&mut self) {
        normalize_node(&mut self.root);
        if !self.contains(self.focus) {
            self.focus = self.panes().first().copied().unwrap_or(PaneId(0));
        }
        if self.zoom.is_some_and(|z| !self.contains(z)) || !self.is_split() {
            self.zoom = None;
        }
    }
}

/// Whether a pane `extent` px along an axis can be split in two without either
/// half falling under `min`. The guard behind the muted drop zone: a split that
/// can't fit is refused rather than made unusable.
pub(crate) fn split_fits(extent: f32, min: f32) -> bool {
    extent * 0.5 >= min
}

// --- tree walks ---

fn collect(node: &Node, out: &mut Vec<PaneId>) {
    match node {
        Node::Leaf(id) => out.push(*id),
        Node::Split { children, .. } => {
            for c in children {
                collect(&c.node, out);
            }
        }
    }
}

/// Insert a leaf for `new` beside `at`. Joins an existing same-axis split as a
/// sibling (keeping the row flat); otherwise turns the target leaf into a split.
/// Returns whether `at` was found.
fn insert_at(node: &mut Node, at: PaneId, axis: SplitAxis, before: bool, new: PaneId) -> bool {
    match node {
        Node::Leaf(id) if *id == at => {
            let target = Child::new(0.5, Node::Leaf(at));
            let fresh = Child::new(0.5, Node::Leaf(new));
            let children = if before {
                vec![fresh, target]
            } else {
                vec![target, fresh]
            };
            *node = Node::Split { axis, children };
            true
        }
        Node::Leaf(_) => false,
        Node::Split { axis: a, children } => {
            if *a == axis
                && let Some(i) = children
                    .iter()
                    .position(|c| matches!(c.node, Node::Leaf(id) if id == at))
            {
                // Halve the target's share and give the other half to the new
                // pane, so every other column keeps the width it had.
                let half = children[i].weight * 0.5;
                children[i].weight = half;
                let slot = if before { i } else { i + 1 };
                children.insert(slot, Child::new(half, Node::Leaf(new)));
                return true;
            }
            children
                .iter_mut()
                .any(|c| insert_at(&mut c.node, at, axis, before, new))
        }
    }
}

/// Drop the leaf for `pane`, handing its share back to its siblings in
/// proportion. Leaves possible one-child splits for `normalize` to collapse.
fn remove_at(node: &mut Node, pane: PaneId) -> bool {
    let Node::Split { children, .. } = node else {
        return false;
    };
    if let Some(i) = children
        .iter()
        .position(|c| matches!(c.node, Node::Leaf(id) if id == pane))
    {
        let freed = children.remove(i).weight;
        let total: f32 = children.iter().map(|c| c.weight).sum();
        if total > 0. {
            for c in children.iter_mut() {
                c.weight += freed * c.weight / total;
            }
        }
        return true;
    }
    children.iter_mut().any(|c| remove_at(&mut c.node, pane))
}

fn normalize_node(node: &mut Node) {
    if let Node::Split { axis, children } = node {
        let axis = *axis;
        for c in children.iter_mut() {
            normalize_node(&mut c.node);
        }
        let mut flat: Vec<Child> = Vec::with_capacity(children.len());
        for c in children.drain(..) {
            match c.node {
                // Splice a same-axis child's slots into this split, scaling them
                // by the slot they came from so nothing visibly moves.
                Node::Split {
                    axis: inner_axis,
                    children: inner,
                } if inner_axis == axis => {
                    for mut g in inner {
                        g.weight *= c.weight;
                        flat.push(g);
                    }
                }
                other => flat.push(Child::new(c.weight, other)),
            }
        }
        renormalize(&mut flat);
        *children = flat;
    }
    // A split with one child *is* that child; it takes over the slot (and the
    // weight) the split occupied in its own parent.
    if let Node::Split { children, .. } = node
        && children.len() == 1
    {
        let only = children.remove(0);
        *node = only.node;
    }
}

/// Scale a split's weights to sum to 1. A degenerate total (all zero, NaN from a
/// division that lost precision) falls back to even shares rather than making
/// panes vanish.
fn renormalize(children: &mut [Child]) {
    if children.is_empty() {
        return;
    }
    let even = 1. / children.len() as f32;
    let total: f32 = children.iter().map(|c| c.weight.max(0.)).sum();
    if !total.is_finite() || total <= f32::EPSILON {
        for c in children.iter_mut() {
            c.weight = even;
        }
        return;
    }
    for c in children.iter_mut() {
        c.weight = c.weight.max(0.) / total;
    }
}

fn equalize_node(node: &mut Node) {
    if let Node::Split { children, .. } = node {
        let even = 1. / children.len() as f32;
        for c in children.iter_mut() {
            c.weight = even;
            equalize_node(&mut c.node);
        }
    }
}

fn node_at_mut<'a>(node: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    match path.split_first() {
        None => Some(node),
        Some((&i, rest)) => match node {
            Node::Split { children, .. } => node_at_mut(&mut children.get_mut(i)?.node, rest),
            Node::Leaf(_) => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert every structural invariant. Run after each mutation in the tests
    /// below, so a regression names the operation that broke it.
    fn check(tree: &PaneTree) {
        check_node(tree.root());
        let panes = tree.panes();
        assert!(
            panes.contains(&tree.focus()),
            "focus {:?} is not a live pane",
            tree.focus()
        );
        if let Some(z) = tree.zoomed() {
            assert!(panes.contains(&z), "zoom {z:?} is not a live pane");
        }
        let mut sorted = panes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), panes.len(), "duplicate pane ids: {panes:?}");
    }

    fn check_node(node: &Node) {
        let Node::Split { axis, children } = node else {
            return;
        };
        assert!(children.len() >= 2, "split with {} child", children.len());
        let total: f32 = children.iter().map(|c| c.weight).sum();
        assert!((total - 1.).abs() < 1e-4, "weights sum to {total}");
        for c in children {
            assert!(c.weight > 0., "non-positive weight {}", c.weight);
            if let Node::Split { axis: inner, .. } = &c.node {
                assert_ne!(*inner, *axis, "same-axis nesting was not flattened");
            }
            check_node(&c.node);
        }
    }

    /// Walk to the node at `path`. Only the tests address a node by path; the
    /// renderer already holds one as it recurses, and mutation goes through
    /// `node_at_mut`.
    fn node_at<'a>(node: &'a Node, path: &[usize]) -> Option<&'a Node> {
        match path.split_first() {
            None => Some(node),
            Some((&i, rest)) => match node {
                Node::Split { children, .. } => node_at(&children.get(i)?.node, rest),
                Node::Leaf(_) => None,
            },
        }
    }

    fn weights(tree: &PaneTree, path: &[usize]) -> Vec<f32> {
        match node_at(tree.root(), path) {
            Some(Node::Split { children, .. }) => children.iter().map(|c| c.weight).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn new_tree_is_one_focused_pane() {
        let tree = PaneTree::new();
        assert_eq!(tree.panes(), vec![PaneId(0)]);
        assert_eq!(tree.focus(), PaneId(0));
        assert!(!tree.is_split());
        check(&tree);
    }

    #[test]
    fn split_right_makes_two_even_columns() {
        let mut tree = PaneTree::new();
        let new = tree.insert(PaneId(0), DropZone::Right).expect("split");
        assert_eq!(tree.panes(), vec![PaneId(0), new]);
        assert_eq!(weights(&tree, &[]), vec![0.5, 0.5]);
        assert!(tree.is_split());
        check(&tree);
    }

    #[test]
    fn split_left_puts_the_new_pane_first() {
        let mut tree = PaneTree::new();
        let new = tree.insert(PaneId(0), DropZone::Left).expect("split");
        assert_eq!(tree.panes(), vec![new, PaneId(0)]);
        check(&tree);
    }

    #[test]
    fn repeated_right_splits_stay_flat() {
        let mut tree = PaneTree::new();
        let a = tree.insert(PaneId(0), DropZone::Right).expect("split");
        let b = tree.insert(a, DropZone::Right).expect("split");
        let c = tree.insert(b, DropZone::Right).expect("split");
        assert_eq!(tree.panes(), vec![PaneId(0), a, b, c]);
        // One flat row of four, not a four-deep staircase.
        assert_eq!(weights(&tree, &[]).len(), 4);
        // Only the split pane's share is subdivided; the first column is untouched.
        assert_eq!(weights(&tree, &[]), vec![0.5, 0.25, 0.125, 0.125]);
        check(&tree);
    }

    #[test]
    fn cross_axis_split_nests() {
        let mut tree = PaneTree::new();
        let right = tree.insert(PaneId(0), DropZone::Right).expect("split");
        let below = tree.insert(right, DropZone::Bottom).expect("split");
        assert_eq!(tree.panes(), vec![PaneId(0), right, below]);
        // Root stays two columns; the second column became two rows.
        assert_eq!(weights(&tree, &[]), vec![0.5, 0.5]);
        assert_eq!(weights(&tree, &[1]), vec![0.5, 0.5]);
        check(&tree);
    }

    #[test]
    fn remove_hands_the_share_to_siblings() {
        let mut tree = PaneTree::new();
        let a = tree.insert(PaneId(0), DropZone::Right).expect("split");
        let b = tree.insert(a, DropZone::Right).expect("split");
        // 0.5 / 0.25 / 0.25 — dropping the middle leaves 2:1.
        let heir = tree.remove(a).expect("removed");
        assert_eq!(heir, PaneId(0));
        assert_eq!(tree.panes(), vec![PaneId(0), b]);
        let w = weights(&tree, &[]);
        assert!((w[0] - 2. / 3.).abs() < 1e-5, "{w:?}");
        check(&tree);
    }

    #[test]
    fn removing_down_to_one_collapses_and_reflattens() {
        let mut tree = PaneTree::new();
        let right = tree.insert(PaneId(0), DropZone::Right).expect("split");
        let below = tree.insert(right, DropZone::Bottom).expect("split");
        // Emptying the nested column's second row collapses the row split, and
        // the survivor folds back into the root row.
        tree.remove(below).expect("removed");
        assert_eq!(tree.panes(), vec![PaneId(0), right]);
        assert_eq!(weights(&tree, &[]), vec![0.5, 0.5]);
        check(&tree);
        // And back to a bare leaf.
        tree.remove(right).expect("removed");
        assert!(!tree.is_split());
        assert_eq!(tree.panes(), vec![PaneId(0)]);
        check(&tree);
    }

    #[test]
    fn removing_the_last_pane_is_refused() {
        let mut tree = PaneTree::new();
        assert_eq!(tree.remove(PaneId(0)), None);
        assert_eq!(tree.panes(), vec![PaneId(0)]);
        assert_eq!(tree.remove(PaneId(42)), None);
        check(&tree);
    }

    #[test]
    fn removing_the_focused_pane_moves_focus_to_the_heir() {
        let mut tree = PaneTree::new();
        let right = tree.insert(PaneId(0), DropZone::Right).expect("split");
        tree.set_focus(right);
        let heir = tree.remove(right).expect("removed");
        assert_eq!(heir, PaneId(0));
        assert_eq!(tree.focus(), PaneId(0));
        check(&tree);
    }

    #[test]
    fn ids_are_not_reused_after_removal() {
        let mut tree = PaneTree::new();
        let first = tree.insert(PaneId(0), DropZone::Right).expect("split");
        tree.remove(first);
        let second = tree.insert(PaneId(0), DropZone::Right).expect("split");
        assert_ne!(first, second, "a fresh pane inherited a dead pane's id");
        check(&tree);
    }

    #[test]
    fn cycle_wraps_in_visual_order() {
        let mut tree = PaneTree::new();
        let a = tree.insert(PaneId(0), DropZone::Right).expect("split");
        let b = tree.insert(a, DropZone::Right).expect("split");
        assert_eq!(tree.cycle(PaneId(0), true), Some(a));
        assert_eq!(tree.cycle(b, true), Some(PaneId(0)));
        assert_eq!(tree.cycle(PaneId(0), false), Some(b));
        assert_eq!(PaneTree::new().cycle(PaneId(0), true), None);
    }

    #[test]
    fn resize_moves_one_divider_and_honours_the_minimum() {
        let mut tree = PaneTree::new();
        let a = tree.insert(PaneId(0), DropZone::Right).expect("split");
        tree.insert(a, DropZone::Right).expect("split");
        let before = weights(&tree, &[]);
        assert!(tree.set_weight(&[], 0, before[0] + 0.1, 0.1));
        let after = weights(&tree, &[]);
        assert!((after[0] - (before[0] + 0.1)).abs() < 1e-5, "{after:?}");
        assert!((after[1] - (before[1] - 0.1)).abs() < 1e-5, "{after:?}");
        // The third column is untouched by the first divider.
        assert!((after[2] - before[2]).abs() < 1e-5, "{after:?}");
        check(&tree);
        // A drag far past the end clamps at the minimum instead of zeroing a pane.
        tree.set_weight(&[], 0, -10., 0.1);
        assert!(weights(&tree, &[])[0] >= 0.1 - 1e-5);
        check(&tree);
        assert!(
            !tree.set_weight(&[], 9, 0.5, 0.1),
            "bad gutter index accepted"
        );
        assert!(!tree.set_weight(&[7], 0, 0.5, 0.1), "bad path accepted");
    }

    #[test]
    fn equalize_evens_every_split() {
        let mut tree = PaneTree::new();
        let a = tree.insert(PaneId(0), DropZone::Right).expect("split");
        let b = tree.insert(a, DropZone::Right).expect("split");
        tree.insert(b, DropZone::Bottom).expect("split");
        tree.equalize();
        for w in weights(&tree, &[]) {
            assert!((w - 1. / 3.).abs() < 1e-5);
        }
        assert_eq!(weights(&tree, &[2]), vec![0.5, 0.5]);
        check(&tree);
    }

    #[test]
    fn zoom_round_trips_and_clears_on_layout_change() {
        let mut tree = PaneTree::new();
        // Zoom is meaningless with one pane.
        tree.zoom_toggle(PaneId(0));
        assert_eq!(tree.zoomed(), None);
        let right = tree.insert(PaneId(0), DropZone::Right).expect("split");
        tree.zoom_toggle(right);
        assert_eq!(tree.zoomed(), Some(right));
        tree.zoom_toggle(right);
        assert_eq!(tree.zoomed(), None);
        // A pane created while zoomed would otherwise stay hidden behind it.
        tree.zoom_toggle(right);
        tree.insert(right, DropZone::Bottom).expect("split");
        assert_eq!(tree.zoomed(), None);
        // As does removing the zoomed pane.
        tree.zoom_toggle(right);
        tree.remove(right);
        assert_eq!(tree.zoomed(), None);
        check(&tree);
    }

    #[test]
    fn invariants_survive_a_long_op_sequence() {
        // A deterministic pseudo-random walk (xorshift, fixed seed) over the full
        // operation set: the shapes it reaches are the ones hand-written cases
        // miss, and every step re-checks the invariants.
        let mut seed: u32 = 0x5eed_1234;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let zones = [
            DropZone::Left,
            DropZone::Right,
            DropZone::Top,
            DropZone::Bottom,
        ];
        let mut tree = PaneTree::new();
        for step in 0..500 {
            let panes = tree.panes();
            let target = panes[rand() as usize % panes.len()];
            match rand() % 10 {
                0..=4 => {
                    tree.insert(target, zones[rand() as usize % zones.len()]);
                }
                5..=7 => {
                    tree.remove(target);
                }
                8 => {
                    tree.set_weight(&[], (rand() % 4) as usize, 0.13, 0.05);
                }
                _ => {
                    tree.set_focus(target);
                    tree.zoom_toggle(target);
                }
            }
            assert!(!tree.panes().is_empty(), "step {step} emptied the layout");
            check(&tree);
        }
    }

    /// A restored tree keeps its shape and focus, and the id counter clears every
    /// live pane so the next split cannot mint a duplicate.
    #[test]
    fn restore_round_trips_a_split_layout() {
        let root = Node::Split {
            axis: SplitAxis::Horizontal,
            children: vec![
                Child::new(0.3, Node::Leaf(PaneId(0))),
                Child::new(0.7, Node::Leaf(PaneId(4))),
            ],
        };
        let tree = PaneTree::restore(root, PaneId(4), 5);
        check(&tree);
        assert_eq!(tree.panes(), vec![PaneId(0), PaneId(4)]);
        assert_eq!(tree.focus(), PaneId(4));
        assert_eq!(tree.next_id(), 5);
    }

    /// A file naming a focus pane that is not in the tree still restores, with
    /// focus pulled to a live pane rather than left dangling.
    #[test]
    fn restore_repoints_a_dangling_focus() {
        let tree = PaneTree::restore(Node::Leaf(PaneId(1)), PaneId(9), 2);
        check(&tree);
        assert_eq!(tree.focus(), PaneId(1));
    }

    /// An under-reported `next` cannot hand out an id a restored pane already
    /// holds: the counter is lifted past every live pane.
    #[test]
    fn restore_lifts_the_id_counter_past_live_panes() {
        let tree = PaneTree::restore(Node::Leaf(PaneId(7)), PaneId(7), 1);
        assert!(tree.next_id() > 7, "next {} would collide", tree.next_id());
    }

    /// A corrupt file describing a childless split has no panes at all;
    /// `normalize` cannot collapse it, so restore falls back to a fresh layout
    /// rather than rendering an empty work area.
    #[test]
    fn restore_falls_back_when_the_file_describes_no_panes() {
        let root = Node::Split {
            axis: SplitAxis::Horizontal,
            children: Vec::new(),
        };
        let tree = PaneTree::restore(root, PaneId(3), 4);
        check(&tree);
        assert_eq!(tree.panes(), vec![PaneId::FIRST]);
    }
}
