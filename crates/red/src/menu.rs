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

use crate::Quit;
use crate::i18n::tr;
use crate::keymap::{
    About, ClosePane, CloseTab, CycleFocusNext, CycleFocusPrev, EqualizePanes, FocusEditor,
    FocusGrid, FocusOtherHalf, FocusSchema, FormatSql, MaximizePane, NewConnection, NewTab,
    NextTab, PrevTab, RefreshSchema, ReportBug, RunQuery, SearchSchema, Settings, ShowChangelog,
    ShowErDiagram, ShowShortcuts, SplitDown, SwitchConnection, ToggleSidebar, ToggleSplit,
};
use crate::palette::{CopyResult, GoToRow, ToggleCommandPalette};

/// Build the full menu tree. Side-effect-free and cheap, so it can be rebuilt and
/// re-handed to `cx.set_menus` whenever the menu needs to reflect new state.
///
/// macOS forces the bold app-menu title to the process/bundle name. With no
/// `.app` bundle that's the executable filename, so the binary is named `RED`
/// (see `crates/red/Cargo.toml`) to make the top bar read "RED".
pub(crate) fn build_menus() -> Vec<Menu> {
    vec![
        // The app menu. macOS overrides the visible name with the bundle name.
        // Not translated: the product name is the product name in every locale,
        // and macOS overrides it with the bundle name anyway.
        Menu::new("RED").items([
            MenuItem::action(tr!("menu.app.about_red", "About RED"), About),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.app.settings", "Settings…"), Settings),
            MenuItem::separator(),
            MenuItem::os_submenu(
                tr!("menu.app.services", "Services"),
                SystemMenuType::Services,
            ),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.app.quit_red", "Quit RED"), Quit),
        ]),
        Menu::new(tr!("menu.connection.title", "Connection")).items([
            // Open the ⌘P switcher (active + recent connections), or start a new
            // connection (⌘N on the welcome screen). Both display their
            // accelerators via the keybinding registry.
            MenuItem::action(
                tr!("menu.connection.switch_connection", "Switch Connection…"),
                SwitchConnection,
            ),
            MenuItem::action(
                tr!("menu.connection.new_connection", "New Connection…"),
                NewConnection,
            ),
        ]),
        Menu::new(tr!("menu.edit.title", "Edit")).items([
            // Clipboard for text fields. Undo/Redo are intentionally omitted;
            // Flint's inputs have no undo stack yet (see `docs/deferred.md`).
            MenuItem::os_action(tr!("menu.edit.cut", "Cut"), InputCut, OsAction::Cut),
            MenuItem::os_action(tr!("menu.edit.copy", "Copy"), InputCopy, OsAction::Copy),
            MenuItem::os_action(tr!("menu.edit.paste", "Paste"), InputPaste, OsAction::Paste),
            MenuItem::os_action(
                tr!("menu.edit.select_all", "Select All"),
                InputSelectAll,
                OsAction::SelectAll,
            ),
            MenuItem::separator(),
            // Copy the result grid's current selection (RED's own action).
            MenuItem::action(tr!("menu.edit.copy_result", "Copy Result"), CopyResult),
        ]),
        Menu::new(tr!("menu.view.title", "View")).items([
            MenuItem::action(
                tr!("menu.view.toggle_sidebar", "Toggle Sidebar"),
                ToggleSidebar,
            ),
            MenuItem::action(tr!("menu.view.split_right", "Split Right"), ToggleSplit),
            MenuItem::action(tr!("menu.view.split_down", "Split Down"), SplitDown),
            MenuItem::action(tr!("menu.view.close_pane", "Close Pane"), ClosePane),
            MenuItem::action(
                tr!("menu.view.maximize_pane", "Maximize / Restore Pane"),
                MaximizePane,
            ),
            MenuItem::action(
                tr!("menu.view.equalize_panes", "Equalize Pane Sizes"),
                EqualizePanes,
            ),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.view.focus_schema", "Focus Schema"), FocusSchema),
            MenuItem::action(tr!("menu.view.focus_editor", "Focus Editor"), FocusEditor),
            MenuItem::action(tr!("menu.view.focus_grid", "Focus Grid"), FocusGrid),
            MenuItem::action(
                tr!("menu.view.focus_next_pane", "Focus Next Pane"),
                FocusOtherHalf,
            ),
            MenuItem::separator(),
            MenuItem::action(
                tr!("menu.view.cycle_focus_next", "Cycle Focus Next"),
                CycleFocusNext,
            ),
            MenuItem::action(
                tr!("menu.view.cycle_focus_previous", "Cycle Focus Previous"),
                CycleFocusPrev,
            ),
            MenuItem::separator(),
            MenuItem::action(
                tr!("menu.view.search_schema", "Search Schema"),
                SearchSchema,
            ),
            MenuItem::action(
                tr!("menu.view.command_palette", "Command Palette…"),
                ToggleCommandPalette,
            ),
        ]),
        Menu::new(tr!("menu.query.title", "Query")).items([
            // ⌘↵ runs the active tab's query, or tests the connection while the
            // connection form is open (the unified `RunQuery` action).
            MenuItem::action(tr!("menu.query.run_query", "Run Query"), RunQuery),
            MenuItem::action(tr!("menu.query.format_sql", "Format SQL"), FormatSql),
            MenuItem::action(tr!("menu.query.er_diagram", "ER Diagram"), ShowErDiagram),
            MenuItem::action(
                tr!("menu.query.refresh_schema", "Refresh Schema"),
                RefreshSchema,
            ),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.query.go_to_row", "Go to Row…"), GoToRow),
        ]),
        Menu::new(tr!("menu.tabs.title", "Tabs")).items([
            MenuItem::action(tr!("menu.tabs.new_tab", "New Tab"), NewTab),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.tabs.next_tab", "Next Tab"), NextTab),
            MenuItem::action(tr!("menu.tabs.previous_tab", "Previous Tab"), PrevTab),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.tabs.close_tab", "Close Tab"), CloseTab),
        ]),
        Menu::new(tr!("menu.help.title", "Help")).items([
            MenuItem::action(tr!("menu.help.whats_new", "What's New"), ShowChangelog),
            MenuItem::action(
                tr!("menu.help.keyboard_shortcuts", "Keyboard Shortcuts"),
                ShowShortcuts,
            ),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.help.report_a_bug", "Report a Bug…"), ReportBug),
        ]),
    ]
}
