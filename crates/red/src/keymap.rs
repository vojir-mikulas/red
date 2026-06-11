//! The central keymap — the single place every global `actions!` declaration and
//! `KeyBinding` registration lives, grouped by context behind one [`bind_all`]
//! called once at startup. Keeping it here (rather than scattered through
//! `main.rs` and the views) gives one source of truth for "what is bound", and is
//! the future seam for a user-configurable keymap fed from `settings.toml`.
//!
//! Two layers of reach back the keyboard story: a direct `KeyBinding` here for the
//! common actions, and a command-palette entry (see [`crate::palette`]) for
//! everything. The palette is the floor; these bindings are the fast path.
//!
//! Bindings are scoped to a `key_context` so they fire only where they make sense
//! and never collide with the editing keys that Flint's `TextInput` / `CodeEditor`
//! / `Palette` contexts swallow deeper in the focus path:
//!
//! - no context — true globals that work from any phase (`⌘K`, `⌘Q`, …);
//! - `RedRoot` — app-chrome actions (tabs, sidebar, copy) that should fire from
//!   any focus *within* the app, since `RedRoot` is an ancestor of every pane.
//!
//! The bindings use `cmd-*` unconditionally, matching the rest of the app's
//! macOS-first chrome; per-platform `ctrl-*` splitting is a follow-up.

use flint::{CodeEditor, Palette, TextInput};
use gpui::{actions, App, KeyBinding};

use crate::palette::{CopyResult, GoToRow, ToggleCommandPalette};
use crate::Quit;

// App-chrome actions reachable by keyboard. Editing actions come from Flint's
// `TextInput` / `CodeEditor`; the grid/tree navigation actions live with their
// own panes once those land.
actions!(
    red,
    [
        /// Open a fresh, blank query tab.
        NewTab,
        /// Close the focused query tab (confirming if it holds real work).
        CloseTab,
        /// Focus the next query tab (wraps).
        NextTab,
        /// Focus the previous query tab (wraps).
        PrevTab,
        /// Show or hide the schema sidebar.
        ToggleSidebar,
        /// Reload the schema tree from the backend.
        RefreshSchema,
        /// Move keyboard focus to the schema sidebar / editor / result grid.
        FocusSchema,
        FocusEditor,
        FocusGrid,
        /// Cycle focus to the next / previous pane (schema → editor → grid).
        CycleFocusNext,
        CycleFocusPrev,
        /// Open the keyboard-shortcuts reference overlay.
        ShowShortcuts,
    ]
);

/// The keyboard reference, grouped, for the shortcuts overlay (`⌘/`) and the
/// docs. Kept beside the bindings above so the two don't drift; the overlay is
/// built from this rather than hand-maintained in the view.
pub(crate) fn shortcuts() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    vec![
        (
            "Global",
            vec![
                ("⌘K", "Command palette"),
                ("⌘/", "Keyboard shortcuts"),
                ("⌘,", "Settings"),
                ("⌘Q", "Quit"),
            ],
        ),
        (
            "Panes",
            vec![
                ("⌘1 / ⌘2 / ⌘3", "Focus schema / editor / grid"),
                ("F6 / ⇧F6", "Cycle focus forward / back"),
                ("⌘B", "Toggle schema sidebar"),
            ],
        ),
        (
            "Query tabs",
            vec![
                ("⌘T", "New tab"),
                ("⌘W", "Close tab"),
                ("⌃Tab / ⌃⇧Tab", "Next / previous tab"),
                ("⌘↵", "Run query"),
            ],
        ),
        (
            "Result grid",
            vec![
                ("↑ ↓ ← →", "Move cell cursor"),
                ("⇧ + arrows", "Extend selection"),
                ("⌘← / ⌘→", "Row start / end"),
                ("⌘↑ / ⌘↓", "First / last row"),
                ("PgUp / PgDn", "Page up / down"),
                ("⌃G", "Go to row…"),
                ("⌘C", "Copy selection"),
            ],
        ),
        (
            "Schema tree",
            vec![
                ("↑ / ↓", "Move selection"),
                ("← / →", "Collapse / expand"),
                ("↵", "Open table preview"),
                ("⌘R", "Refresh schema"),
            ],
        ),
    ]
}

/// Install every key binding the app uses, once, at startup. Groups the Flint
/// component keymaps, the true globals, and RED's app-chrome bindings.
pub(crate) fn bind_all(cx: &mut App) {
    // The connection form's text fields, the SQL editor, and the command palette
    // each bring their own editing keymap; install them first so their contexts
    // win for keys typed while one of them is focused.
    TextInput::bind_keys(cx);
    CodeEditor::bind_keys(cx);
    Palette::bind_keys(cx);

    cx.bind_keys([
        // --- true globals (work from any phase) ---
        // ⌘K toggles the command palette; ⌃G opens "go to row"; ⌘Q quits (we
        // render a seamless titlebar with no native app menu, so quit is ours).
        KeyBinding::new("cmd-k", ToggleCommandPalette, None),
        KeyBinding::new("ctrl-g", GoToRow, None),
        KeyBinding::new("cmd-q", Quit, None),
        // --- RedRoot: app chrome, fires from any focus within the app ---
        // ⌘C copies the result grid's selection. Scoped to `RedRoot` so a focused
        // text field or the SQL editor keeps its own ⌘C (their context sits deeper
        // in the focus path and wins); it only reaches here when neither is focused.
        KeyBinding::new("cmd-c", CopyResult, Some("RedRoot")),
        // Tab management. `RedRoot` is an ancestor of the editor, so these still
        // fire while the editor is focused — none collide with the editor's keys
        // (it binds plain `tab`, not `ctrl-tab`).
        KeyBinding::new("cmd-t", NewTab, Some("RedRoot")),
        KeyBinding::new("cmd-w", CloseTab, Some("RedRoot")),
        KeyBinding::new("ctrl-tab", NextTab, Some("RedRoot")),
        KeyBinding::new("ctrl-shift-tab", PrevTab, Some("RedRoot")),
        // Schema sidebar + reload.
        KeyBinding::new("cmd-b", ToggleSidebar, Some("RedRoot")),
        KeyBinding::new("cmd-r", RefreshSchema, Some("RedRoot")),
        // Pane focus: direct jumps (⌘1/2/3) and a cycle (F6 / ⇧F6).
        KeyBinding::new("cmd-1", FocusSchema, Some("RedRoot")),
        KeyBinding::new("cmd-2", FocusEditor, Some("RedRoot")),
        KeyBinding::new("cmd-3", FocusGrid, Some("RedRoot")),
        KeyBinding::new("f6", CycleFocusNext, Some("RedRoot")),
        KeyBinding::new("shift-f6", CycleFocusPrev, Some("RedRoot")),
        // Discoverability. `⌘/` (not `?`) so typing `?` into the editor or a field
        // still inserts the character — a global `?` binding would swallow it.
        KeyBinding::new("cmd-/", ShowShortcuts, Some("RedRoot")),
    ]);
}
