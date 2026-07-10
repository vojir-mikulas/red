//! The native macOS menu bar. [`build_menus`] returns the static menu tree that
//! `main.rs` hands to `cx.set_menus` at startup; on macOS GPUI mounts it as the
//! global menu bar at the top of the screen (the seamless in-window titlebar is
//! untouched; see `main.rs::titlebar_options`).
//!
//! Every item references an **action struct that already exists**: the same
//! types `keymap.rs` binds. GPUI looks each action up in the keybinding registry
//! and renders its accelerator automatically, so the menu and the keymap can't
//! drift: `keymap.rs` stays the single source of truth for shortcuts.
//!
//! The Edit menu's clipboard items pair Flint's `TextInput` clipboard actions
//! with [`OsAction`]s. On macOS those route through the standard Edit-menu
//! selectors, so menu Copy/Cut/Paste/Select All drive editing inside the
//! connection form's text fields. (The SQL `CodeEditor` uses a *separate* set of
//! clipboard actions in Flint, so the menu items don't reach it; its own ⌘C/⌘V
//! keystrokes still work. Unifying the two is tracked in `docs/deferred.md`.)
//!
//! The tree is a static snapshot. Dynamic content (Open Recent, `.checked()`
//! state) would need a `refresh_menus` helper that re-calls `set_menus`; both are
//! deferred (see `docs/deferred.md`).

use gpui::{Menu, MenuItem, OsAction, SystemMenuType};

// Flint's text-field clipboard actions. They're bound inside Flint's `TextInput`
// key context (`TextInput::bind_keys`), so dispatching them from the menu reaches
// a focused field. Aliased to keep the Edit-menu items unambiguous.
use flint::components::text_input::{
    Copy as InputCopy, Cut as InputCut, Paste as InputPaste, SelectAll as InputSelectAll,
};

use crate::keymap::{
    About, CloseTab, CycleFocusNext, CycleFocusPrev, FocusEditor, FocusGrid, FocusOtherHalf,
    FocusSchema, FormatSql, NewConnection, NewTab, NextTab, PrevTab, RefreshSchema, ReportBug,
    RunQuery, SearchSchema, Settings, ShowChangelog, ShowErDiagram, ShowShortcuts,
    SwitchConnection, ToggleSidebar, ToggleSplit,
};
use crate::palette::{CopyResult, GoToRow, ToggleCommandPalette};
use crate::Quit;

/// Build the full menu tree. Side-effect-free and cheap, so it can be rebuilt and
/// re-handed to `cx.set_menus` whenever the menu needs to reflect new state.
///
/// macOS forces the bold app-menu title to the process/bundle name. With no
/// `.app` bundle that's the executable filename, so the binary is named `Red`
/// (see `crates/red/Cargo.toml`) to make the top bar read "Red".
pub(crate) fn build_menus() -> Vec<Menu> {
    vec![
        // The app menu. macOS overrides the visible name with the bundle name.
        Menu::new("Red").items([
            MenuItem::action("About Red", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", Settings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit Red", Quit),
        ]),
        Menu::new("Connection").items([
            // Open the ⌘P switcher (active + recent connections), or start a new
            // connection (⌘N on the welcome screen). Both display their
            // accelerators via the keybinding registry.
            MenuItem::action("Switch Connection…", SwitchConnection),
            MenuItem::action("New Connection…", NewConnection),
        ]),
        Menu::new("Edit").items([
            // Clipboard for text fields. Undo/Redo are intentionally omitted;
            // Flint's inputs have no undo stack yet (see `docs/deferred.md`).
            MenuItem::os_action("Cut", InputCut, OsAction::Cut),
            MenuItem::os_action("Copy", InputCopy, OsAction::Copy),
            MenuItem::os_action("Paste", InputPaste, OsAction::Paste),
            MenuItem::os_action("Select All", InputSelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            // Copy the result grid's current selection (RED's own action).
            MenuItem::action("Copy Result", CopyResult),
        ]),
        Menu::new("View").items([
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Toggle Split View", ToggleSplit),
            MenuItem::separator(),
            MenuItem::action("Focus Schema", FocusSchema),
            MenuItem::action("Focus Editor", FocusEditor),
            MenuItem::action("Focus Grid", FocusGrid),
            MenuItem::action("Focus Other Split Half", FocusOtherHalf),
            MenuItem::separator(),
            MenuItem::action("Cycle Focus Next", CycleFocusNext),
            MenuItem::action("Cycle Focus Previous", CycleFocusPrev),
            MenuItem::separator(),
            MenuItem::action("Search Schema", SearchSchema),
            MenuItem::action("Command Palette…", ToggleCommandPalette),
        ]),
        Menu::new("Query").items([
            // ⌘↵ runs the active tab's query, or tests the connection while the
            // connection form is open (the unified `RunQuery` action).
            MenuItem::action("Run Query", RunQuery),
            MenuItem::action("Format SQL", FormatSql),
            MenuItem::action("ER Diagram", ShowErDiagram),
            MenuItem::action("Refresh Schema", RefreshSchema),
            MenuItem::separator(),
            MenuItem::action("Go to Row…", GoToRow),
        ]),
        Menu::new("Tabs").items([
            MenuItem::action("New Tab", NewTab),
            MenuItem::separator(),
            MenuItem::action("Next Tab", NextTab),
            MenuItem::action("Previous Tab", PrevTab),
            MenuItem::separator(),
            MenuItem::action("Close Tab", CloseTab),
        ]),
        Menu::new("Help").items([
            MenuItem::action("What's New", ShowChangelog),
            MenuItem::action("Keyboard Shortcuts", ShowShortcuts),
            MenuItem::separator(),
            MenuItem::action("Report a Bug…", ReportBug),
        ]),
    ]
}
