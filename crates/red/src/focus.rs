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
//! transient overlays → assistant. That keeps the easiest hints on the surfaces
//! a user jumps to constantly: on a plain SQL shell the sidebar, editor and grid
//! take the first three keys of the alphabet. Hints are positional, so they only
//! move when the layout does.
//!
//! Individual *tabs* are deliberately not targets yet. They are reachable by
//! ⌃Tab and by click, and a four-pane split with full strips would put thirty
//! badges on screen for surfaces that all lead to the same few bodies.

use gpui::{App, FocusHandle, Focusable, SharedString, Window};

use crate::app::{ActiveConn, AppState, Phase, TabWorkspace};
use crate::i18n::tr;
use crate::panes::PaneId;
use crate::settings::HintAlphabet;

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

/// The default hint alphabet: letters, home row outward.
///
/// **Letters, not digits, because a hint has to be typable without Shift.** The
/// digits are not on the base level of every keyboard: a Czech layout puts `+ ě
/// š č ř ž ý á í é` on the number row and needs Shift for `1`–`0`, and French
/// AZERTY is the same. gpui reports the *unshifted* character in
/// `Keystroke::key` (see the layout table in its macOS `parse_keystroke`, which
/// names Czech explicitly), so a digit hint there either cannot be typed at all
/// or forces a second modifier onto a gesture defined by holding exactly one.
///
/// Letters have no such problem. Every Latin layout types all 26 unshifted, and
/// gpui normalizes a non-Latin layout to its ASCII equivalent in `key` (`q` for
/// `ๆ`), so a letter hint resolves on Russian and Thai keyboards too. Where a
/// layout moves a letter (QWERTZ swaps Y and Z, AZERTY moves A and Q) the badge
/// still names the character to press, so it stays self-consistent.
///
/// Home row first, then the top row, then the bottom: the shortest reach goes to
/// the surfaces that come first.
const LETTER_HINTS: &[char] = &[
    'a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', 'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p',
    'z', 'x', 'c', 'v', 'b', 'n', 'm',
];

/// The opt-in digit alphabet (`keymap.focus_overlay_hints = "digits"`), with the
/// letters trailing as overflow.
///
/// Only sound on a layout whose digits sit on the base level — see
/// [`LETTER_HINTS`] for why that is not a safe assumption to bake in.
const DIGIT_HINTS: &[char] = &[
    '1', '2', '3', '4', '5', '6', '7', '8', '9', '0', 'a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l',
    'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p', 'z', 'x', 'c', 'v', 'b', 'n', 'm',
];

/// The alphabet `style` hands out.
pub(crate) fn hint_alphabet(style: HintAlphabet) -> &'static [char] {
    match style {
        HintAlphabet::Letters => LETTER_HINTS,
        HintAlphabet::Digits => DIGIT_HINTS,
    }
}

/// Every character any alphabet can use, for the keymap to bind once.
///
/// The keymap binds *characters*, not positions, precisely so it does not have
/// to be reinstalled when the alphabet setting changes: the slot a character
/// names is resolved against the live alphabet when the key is pressed.
pub(crate) fn all_hint_keys() -> impl Iterator<Item = char> {
    DIGIT_HINTS.iter().copied()
}

/// The Flint key contexts that mean "this surface turns printable keys into
/// text". Every one of them either *is* a text field or embeds one.
///
/// `TextInput` alone would cover most of these (the palette, the switcher and a
/// combo box all focus an inner field), but a surface can also park focus on its
/// own wrapper — an open `ComboBox` popover holds the shared handle before its
/// field takes it — so each is named.
const TEXT_ENTRY_CONTEXTS: &[&str] = &[
    "TextInput",
    "ComboBox",
    "CodeEditor",
    "MarkdownEditor",
    "Palette",
    "Switcher",
];

/// Whether a text-entry surface currently owns the keyboard.
///
/// The guard for bare-key shortcuts (`e` edits the highlighted connection, `/`
/// jumps to the search box). GPUI dispatches a `on_key_down` listener to *every*
/// node from the focused element up to the root, so a root-level listener sees
/// keys that were meant for a field several levels down: typing "redis" into the
/// welcome screen's engine filter used to fire the `e` shortcut mid-word and open
/// the edit form. A binding cannot have this problem — the deeper context wins —
/// so only hand-written key listeners need to ask.
///
/// Reads the focused element's live key-context stack rather than a list of
/// focus handles the caller happens to know about, so a field added later is
/// covered without anyone remembering to add it here.
pub(crate) fn text_entry_focused(window: &Window) -> bool {
    window
        .context_stack()
        .iter()
        .any(|ctx| TEXT_ENTRY_CONTEXTS.iter().any(|name| ctx.contains(name)))
}

/// Whether an open `ComboBox` popover holds the keyboard (the context exists only
/// while the dropdown is up — a closed trigger carries none).
///
/// Narrower than [`text_entry_focused`], and for the other half of the same
/// problem: Esc. The dropdown is a layer over whatever opened it, so Esc there
/// means "close the dropdown" — but the field's own Esc binding doesn't stop the
/// key from bubbling, so a panel's hand-written Esc listener would close the panel
/// out from under it too.
pub(crate) fn combo_popover_focused(window: &Window) -> bool {
    window
        .context_stack()
        .iter()
        .any(|ctx| ctx.contains("ComboBox"))
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
                active.history_panel.focus_handle(cx),
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
                active.schema.read(cx).focus.clone(),
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

    /// The default alphabet must be typable without Shift on every layout, which
    /// is the whole reason it is letters: Czech and French put the digits on the
    /// shifted level, so a digit hint there needs a second modifier or cannot be
    /// reached at all.
    #[test]
    fn the_default_alphabet_is_all_letters() {
        assert!(
            hint_alphabet(HintAlphabet::Letters)
                .iter()
                .all(char::is_ascii_lowercase),
            "a non-letter in the default alphabet is unreachable on a Czech keyboard"
        );
    }

    /// Home row first, so the surfaces that come first are the shortest reach.
    #[test]
    fn letters_start_on_the_home_row() {
        assert_eq!(
            &hint_alphabet(HintAlphabet::Letters)[..4],
            &['a', 's', 'd', 'f']
        );
    }

    /// Digits are opt-in and run out into the letters rather than stopping at ten.
    #[test]
    fn the_digit_alphabet_leads_with_digits_then_overflows() {
        let digits = hint_alphabet(HintAlphabet::Digits);
        assert_eq!(&digits[..3], &['1', '2', '3']);
        assert_eq!(digits[9], '0');
        assert!(digits[10].is_ascii_lowercase());
    }

    /// Both alphabets are single ASCII characters. The keymap builds a binding
    /// string by concatenating a modifier prefix with the hint (`alt-` + `q`),
    /// which only parses for a one-character ASCII key.
    #[test]
    fn every_hint_is_one_bindable_character() {
        for style in [HintAlphabet::Letters, HintAlphabet::Digits] {
            for &h in hint_alphabet(style) {
                assert!(h.is_ascii_alphanumeric(), "{h:?} is not a bindable key");
                assert_eq!(h.len_utf8(), 1);
            }
        }
    }

    /// No alphabet may label two surfaces with the same key.
    #[test]
    fn alphabets_have_no_duplicates() {
        for style in [HintAlphabet::Letters, HintAlphabet::Digits] {
            let mut seen: Vec<char> = hint_alphabet(style).to_vec();
            let total = seen.len();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), total, "{style:?} alphabet has a duplicate");
        }
    }

    /// The keymap binds one action per character across *both* alphabets, so
    /// switching the setting never needs a keymap reinstall. If a letter were
    /// missing from the bound set, that hint would silently do nothing.
    #[test]
    fn the_bound_key_set_covers_every_alphabet() {
        let bound: Vec<char> = all_hint_keys().collect();
        for style in [HintAlphabet::Letters, HintAlphabet::Digits] {
            for &h in hint_alphabet(style) {
                assert!(bound.contains(&h), "{h:?} is never bound");
            }
        }
    }

    /// Three surfaces, laid out like the real thing: a grid that types nothing, a
    /// bare field, and a field inside an open combo popover. Enough tree for the
    /// guards to read a real context stack.
    struct Probe {
        grid: FocusHandle,
        field: FocusHandle,
        combo_field: FocusHandle,
    }

    impl gpui::Render for Probe {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            use gpui::{InteractiveElement, ParentElement, div};
            div()
                .child(div().key_context("Table").track_focus(&self.grid))
                .child(div().key_context("TextInput").track_focus(&self.field))
                .child(
                    div().key_context("ComboBox").child(
                        div()
                            .key_context("TextInput")
                            .track_focus(&self.combo_field),
                    ),
                )
        }
    }

    /// The guards read the *focused* element's context stack, which is the whole
    /// point: a root-level key listener asks one question and gets the right
    /// answer for a field nested any number of levels down.
    #[gpui::test]
    fn the_guards_follow_focus(cx: &mut gpui::TestAppContext) {
        let (grid, field, combo_field) =
            cx.update(|cx| (cx.focus_handle(), cx.focus_handle(), cx.focus_handle()));
        let window = cx.add_window({
            let (grid, field, combo_field) = (grid.clone(), field.clone(), combo_field.clone());
            move |_window, _cx| Probe {
                grid,
                field,
                combo_field,
            }
        });
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        let ask = |cx: &mut gpui::VisualTestContext, handle: &FocusHandle| {
            let handle = handle.clone();
            cx.update(move |window, cx| {
                window.focus(&handle, cx);
                (text_entry_focused(window), combo_popover_focused(window))
            })
        };

        assert_eq!(
            ask(cx, &grid),
            (false, false),
            "a focused grid types nothing: bare-key shortcuts stay live"
        );
        assert_eq!(
            ask(cx, &field),
            (true, false),
            "a focused field owns every printable key, but not Esc"
        );
        assert_eq!(
            ask(cx, &combo_field),
            (true, true),
            "an open dropdown's search field owns the keyboard and Esc"
        );
    }
}
