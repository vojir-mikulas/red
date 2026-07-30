//! The work area's pane layout: N panes arranged in nested rows and columns, in
//! place of the fixed left/right halves the split view used to be.
//!
//! Three layers, so the geometry stays testable without a window:
//!
//! - [`tree`] — the layout tree itself. Pure, no GPUI, no tabs.
//! - [`state`] — `PaneLayout`: the tree plus the per-pane UI state (tab-strip
//!   scroll, focus handle, editor/result ratio, last painted bounds) that has to
//!   be minted and dropped in lockstep with the panes.
//! - [`dnd`] — the drop-zone overlay that turns a tab drag into a new pane. The
//!   splits themselves are drawn with Flint's `SplitStack`, which this module
//!   was the spike for.
//!
//! Shared verbatim by the SQL, Redis and MongoDB workspaces through the
//! `TabWorkspace`/`SplitWorkspace` traits in `crate::app`.

pub(crate) mod dnd;
pub(crate) mod state;
pub(crate) mod tree;

pub(crate) use dnd::{DraggedTab, PaneLimits, aim, drop_overlay};
pub(crate) use state::{MIN_PANE_WEIGHT, PaneLayout, SplitPath, path_id};
pub(crate) use tree::{DropZone, Node, PaneId, SplitAxis};
