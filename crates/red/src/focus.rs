//! The focus-target registry: the one list of places keyboard focus can land.
//!
//! Focus routing used to be three parallel half-implementations. The SQL shell
//! had a three-arm `Pane` (schema / editor / grid) with its own cycle order;
//! MongoDB punned its tree and document grid onto the same three arms; Redis had
//! nothing at all, so its focus jumps landed on SQL handles the Redis shell never
//! renders — which left keyboard focus outside the rendered frame and silently
//! killed every `RedRoot`-scoped binding (see
//! [`ensure_focus_anchored`](crate::app::AppState::ensure_focus_anchored)).
//!
//! The cause was that nothing enumerated the surfaces. Each feature that needed
//! to move focus re-derived "what is on screen" from a different angle and got a
//! different answer. [`AppState::focus_targets`] is that enumeration, built fresh
//! per frame from whichever shell is up: a collapsed dock or a closed inspector
//! simply is not in the list, so no consumer has to test visibility itself.
//!
//! Four consumers share it, which is what makes it worth its weight:
//!
//! - the hold-to-reveal hint overlay ([`crate::focus_overlay`]);
//! - `CycleFocusNext` / `CycleFocusPrev` (F6), now uniform across all three
//!   seams rather than SQL-only;
//! - focus *restore*, so a surface that closes hands focus to a real neighbour
//!   instead of dropping it;
//! - the command palette's `focus: …` entries, so every target is reachable with
//!   no binding at all.
//!
//! **Order is the contract.** Targets run docks → sidebar → pane bodies →
//! transient overlays → assistant. That keeps the low hint digits on the
//! surfaces a user jumps to constantly: on a plain SQL shell `1` is the sidebar,
//! `2` the editor and `3` the grid, which is what ⌥⌘1/2/3 meant before this
//! existed. Hints are positional, so they only move when the layout does.
//!
//! Individual *tabs* are deliberately not targets yet. They are reachable by
//! ⌃Tab and by click, and a four-pane split with full strips would put thirty
//! badges on screen for surfaces that all lead to the same few bodies.

use gpui::{App, FocusHandle, Focusable, SharedString};

use crate::app::{ActiveConn, AppState, Phase, TabWorkspace};
use crate::i18n::tr;
use crate::panes::PaneId;

/// A pane's primary work surface. Named for what the user sees rather than for
/// the seam it belongs to, so the cycle order reads the same in all three shells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BodyArea {
    /// The SQL editor.
    Editor,
    /// The SQL result grid.
    Grid,
    /// A Redis key list or a MongoDB document list.
    List,
    /// The Redis blank-tab kind chooser (a tab with no kind picked yet).
    Chooser,
}

/// Identity of a focus target: stable across frames for as long as the surface
/// is on screen, so a hint or a cycle position does not move under the user.
///
/// Keyed by [`PaneId`] rather than by pane index because ids are never reused —
/// a split or close can therefore never alias one pane's target onto another's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FocusTargetId {
    /// The query-history dock (⌘Y), shared by the SQL and Redis shells.
    History,
    /// The schema tree (SQL) or the database/collection tree (MongoDB).
    Sidebar,
    Body {
        pane: PaneId,
        area: BodyArea,
    },
    Inspector,
    FilterBar,
    FindBar,
    Assistant,
}

/// One place keyboard focus can land: how to reach it and what to call it.
///
/// Holds no bounds. Where a target *is* on screen is captured during paint and
/// belongs to the overlay that draws hints ([`crate::focus_overlay`]); a
/// consumer that only moves focus should not have to wait a frame for geometry.
pub(crate) struct FocusTarget {
    pub(crate) id: FocusTargetId,
    /// The name shown on a hint badge and in the palette, already localized.
    pub(crate) label: SharedString,
    pub(crate) handle: FocusHandle,
}

impl FocusTarget {
    fn new(id: FocusTargetId, label: SharedString, handle: FocusHandle) -> Self {
        Self { id, label, handle }
    }
}

/// The hint alphabet, in the order hints are handed out.
///
/// Digits lead because the common case — one pane with a sidebar, an editor and
/// a grid — is then labelled exactly `1`, `2`, `3`: what a user expects to see,
/// and what the retired ⌥⌘1/2/3 jumps meant. The three letter rows extend the run
/// to 36, comfortably more than can be on screen at once, so a hint is always a
/// single keypress — no Vimium-style two-character prefixes and no paging.
///
/// Left to right along each row, so the hints a user reaches for most are also
/// the ones nearest the home position.
const HINTS: &[char] = &[
    '1', '2', '3', '4', '5', '6', '7', '8', '9', '0', 'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o',
    'p', 'a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', 'z', 'x', 'c', 'v', 'b', 'n', 'm',
];

/// The whole hint alphabet, for the keymap to bind one action per slot.
pub(crate) fn hint_keys() -> &'static [char] {
    HINTS
}

/// The hint painted on the target at `index`, or `None` past the alphabet's end.
///
/// A target with no hint is not unreachable: it keeps its place in the F6 cycle
/// and its palette entry. Only the one-keypress jump runs out.
pub(crate) fn hint_for(index: usize) -> Option<char> {
    HINTS.get(index).copied()
}

impl AppState {
    /// Every surface that can take keyboard focus right now, in cycle order.
    ///
    /// Empty outside the connected shell: the welcome screen has its own
    /// keyboard story (⌘P, the search box) and nothing here to jump between.
    pub(crate) fn focus_targets(&self, cx: &App) -> Vec<FocusTarget> {
        let Phase::Connected(active) = &self.phase else {
            return Vec::new();
        };
        let mut out = Vec::new();

        // Docks first: they sit leftmost on screen, and reading order is the
        // least surprising basis for a hint order the user has to predict.
        if active.history_open {
            out.push(FocusTarget::new(
                FocusTargetId::History,
                tr!("focus.history", "History"),
                active.history_focus.clone(),
            ));
        }

        // Then the seam's own surfaces. Each pushes its sidebar (if it has one)
        // followed by its pane bodies in the layout's visual order.
        if active.kv_view.is_some() {
            self.kv_focus_targets(active, cx, &mut out);
        } else if active.doc_view.is_some() {
            self.doc_focus_targets(active, cx, &mut out);
        } else {
            self.sql_focus_targets(active, cx, &mut out);
        }

        // Overlay surfaces: transient, so they trail the structural ones and
        // never shift a body's hint while they come and go.
        if let Some(handle) = self.inspector_edit_focus(cx) {
            out.push(FocusTarget::new(
                FocusTargetId::Inspector,
                tr!("focus.inspector", "Inspector"),
                handle,
            ));
        }
        if let Some(bar) = &self.filter_bar {
            out.push(FocusTarget::new(
                FocusTargetId::FilterBar,
                tr!("focus.filter_bar", "Filter"),
                bar.focus_handle(cx),
            ));
        }
        if let Some(bar) = &self.find_bar {
            out.push(FocusTarget::new(
                FocusTargetId::FindBar,
                tr!("focus.find_bar", "Find"),
                bar.input.focus_handle(cx),
            ));
        }
        if let Some(panel) = &self.assistant {
            out.push(FocusTarget::new(
                FocusTargetId::Assistant,
                tr!("focus.assistant", "AI agent"),
                panel.input.focus_handle(cx),
            ));
        }
        out
    }

    /// SQL: the schema tree, then each pane's editor and result grid.
    fn sql_focus_targets(&self, active: &ActiveConn, cx: &App, out: &mut Vec<FocusTarget>) {
        if !active.sidebar_collapsed {
            out.push(FocusTarget::new(
                FocusTargetId::Sidebar,
                tr!("focus.schema", "Schema"),
                active.schema_focus.clone(),
            ));
        }
        for pane in active.layout.panes() {
            // Only the pane's *active* tab renders an editor, so only that one
            // has a live handle; a background tab is reached through its strip
            // entry instead (see `tab_focus_targets`).
            if let Some(tab) = active.pane_active(pane).and_then(|i| active.tabs.get(i)) {
                out.push(FocusTarget::new(
                    FocusTargetId::Body {
                        pane,
                        area: BodyArea::Editor,
                    },
                    tr!("focus.editor", "Editor"),
                    tab.editor.focus_handle(cx),
                ));
            }
            if let Some(handle) = active.grid_focus_for(pane) {
                out.push(FocusTarget::new(
                    FocusTargetId::Body {
                        pane,
                        area: BodyArea::Grid,
                    },
                    tr!("focus.grid", "Results"),
                    handle.clone(),
                ));
            }
        }
    }

    /// Redis: no sidebar of its own (the key tree lives inside a browse tab), so
    /// each pane contributes the surface its active tab actually renders.
    fn kv_focus_targets(&self, active: &ActiveConn, _cx: &App, out: &mut Vec<FocusTarget>) {
        let Some(view) = active.kv_view.as_ref() else {
            return;
        };
        for pane in view.layout.panes() {
            let Some(tab) = view.pane_active(pane).and_then(|i| view.tabs.get(i)) else {
                continue;
            };
            // Only the browse and blank-tab kinds own a focus handle today; the
            // console / pub-sub / monitor / keyspace / analysis panels are
            // click-driven and contribute no target rather than a dead one.
            let entry = match &tab.state {
                crate::kvbrowse::RedisTabState::Browse(browse) => Some((
                    BodyArea::List,
                    tr!("focus.keys", "Keys"),
                    browse.list_focus.clone(),
                )),
                crate::kvbrowse::RedisTabState::Empty => Some((
                    BodyArea::Chooser,
                    tr!("focus.new_tab", "New tab"),
                    view.new_tab_focus.clone(),
                )),
                _ => None,
            };
            if let Some((area, label, handle)) = entry {
                out.push(FocusTarget::new(
                    FocusTargetId::Body { pane, area },
                    label,
                    handle,
                ));
            }
        }
    }

    /// MongoDB: the collection tree, then each pane's document list.
    fn doc_focus_targets(&self, active: &ActiveConn, _cx: &App, out: &mut Vec<FocusTarget>) {
        let Some(view) = active.doc_view.as_ref() else {
            return;
        };
        if !active.sidebar_collapsed {
            out.push(FocusTarget::new(
                FocusTargetId::Sidebar,
                tr!("focus.collections", "Collections"),
                view.tree_focus.clone(),
            ));
        }
        for pane in view.layout.panes() {
            if let Some(coll) = view.pane_active(pane).and_then(|i| view.coll_at(i)) {
                out.push(FocusTarget::new(
                    FocusTargetId::Body {
                        pane,
                        area: BodyArea::List,
                    },
                    tr!("focus.documents", "Documents"),
                    coll.list_focus.clone(),
                ));
            }
        }
    }

    /// The target holding focus right now, as an index into [`Self::focus_targets`].
    ///
    /// Matches on containment, not identity, so a caret inside a target's text
    /// field still reports that target: the SQL editor's `CodeEditor` and the
    /// schema tree's filter box both sit under their surface's handle.
    pub(crate) fn focused_target(
        &self,
        targets: &[FocusTarget],
        window: &gpui::Window,
        cx: &App,
    ) -> Option<usize> {
        targets
            .iter()
            .position(|t| t.handle.contains_focused(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digits lead, so the surfaces a user jumps to constantly keep the keys
    /// the retired ⌥⌘1/2/3 jumps used to mean.
    #[test]
    fn hints_start_with_digits() {
        assert_eq!(hint_for(0), Some('1'));
        assert_eq!(hint_for(1), Some('2'));
        assert_eq!(hint_for(2), Some('3'));
        assert_eq!(hint_for(9), Some('0'));
    }

    /// Every hint is a single character. The keymap builds a binding string by
    /// concatenating a modifier prefix with the hint (`alt-` + `q`), which only
    /// parses for a one-character key.
    #[test]
    fn hints_are_single_characters() {
        for &h in hint_keys() {
            assert_eq!(h.len_utf8(), 1, "{h:?} is not a single-byte key");
            assert!(h.is_ascii_alphanumeric(), "{h:?} is not a bindable key");
        }
    }

    /// Past the digits the alphabet keeps going rather than repeating, so two
    /// surfaces can never be labelled the same key.
    #[test]
    fn hints_are_unique_and_run_out_cleanly() {
        let all: Vec<char> = (0..HINTS.len()).filter_map(hint_for).collect();
        assert_eq!(all.len(), HINTS.len());
        let mut sorted = all.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "hint alphabet has a duplicate");
        assert_eq!(hint_for(HINTS.len()), None);
        assert_eq!(hint_for(usize::MAX), None);
    }
}
