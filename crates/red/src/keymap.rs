//! The central keymap: the single place every global `actions!` declaration and
//! `KeyBinding` registration lives, grouped by context behind one [`apply`]. It is
//! one source of truth for "what is bound", and the seam for the user-configurable
//! keymap: [`apply`] installs the [`DEFAULTS`] table, then layers the overrides a
//! user writes in `keymap.toml` (see [`crate::keymap_config`]) on top, last-wins.
//!
//! Two layers back the keyboard story: a direct `KeyBinding` here for the common
//! actions, and a command-palette entry (see [`crate::palette`]) for everything.
//! The palette is the floor; these bindings are the fast path.
//!
//! Bindings are scoped to a `key_context` so they fire only where they make sense
//! and never collide with the editing keys that Flint's `TextInput` / `CodeEditor`
//! / `Palette` contexts swallow deeper in the focus path:
//!
//! - no context: true globals that work from any phase (`⌘K`, `⌘Q`, …);
//! - `RedRoot`: app-chrome actions (tabs, sidebar, copy) that should fire from
//!   any focus *within* the app, since `RedRoot` is an ancestor of every pane.
//!
//! The table is written in canonical `cmd-*` form, but [`platform_chord`] rewrites
//! the `cmd` modifier to GPUI's `secondary` at bind time, so each key fires as Cmd
//! on macOS and Ctrl on Windows/Linux. Hints follow suit via [`localize_hint`].
//!
//! **Re-applying.** [`apply`] is total: it `clear_key_bindings`, re-installs the
//! Flint component keymaps, the defaults, and the overrides, every time. That is
//! how a live `keymap.toml` edit takes effect with no restart, and why the
//! Flint keymaps must be re-bound here, not once at startup: a clear wipes them.

use std::collections::BTreeMap;

use flint::{
    CodeEditor, ComboBox, MarkdownEditor, Modal, Palette, SelectableLabel, Switcher, TextInput,
};
use gpui::{
    App, KeyBinding, KeyBindingContextPredicate, Keystroke, NoAction, SharedString, actions,
};

use crate::Quit;
use crate::keymap_config::KeymapBlock;
use crate::palette::{CopyResult, GoToObject, GoToRow, ToggleCommandPalette};

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
        /// Show or hide the query-history panel in the left dock.
        ToggleHistory,
        /// Show or hide the reference-columns panel (inline FK expansion) in the
        /// left dock.
        ToggleColumnsPanel,
        /// Reload the schema tree from the backend.
        RefreshSchema,
        /// Reveal the schema sidebar and focus its filter field (search schema).
        SearchSchema,
        /// Cycle focus to the next / previous surface, in focus-target registry
        /// order. Uniform across the SQL, Redis and MongoDB shells.
        CycleFocusNext,
        CycleFocusPrev,
        /// Open the keyboard-shortcuts reference overlay.
        ShowShortcuts,
        /// Open the "What's New" changelog overlay. No default binding; reached
        /// from the Help menu and the `help: what's new` palette command.
        ShowChangelog,
        /// Open the read-only schema ER diagram. No default binding; reached from the
        /// Query menu, the schema-panel button, and the `schema: ER diagram` palette
        /// command.
        ShowErDiagram,
        /// Open the transfer wizard on whatever the schema tree has selected: a
        /// table, or the whole namespace. F5, matching the muscle memory of every
        /// other tool's "copy this somewhere".
        OpenTransfer,
        /// ⌘↵ from anywhere: run the active tab's query; or, while the connection
        /// form is open, test the connection.
        RunQuery,
        /// ⌥⌘↵: run every statement in the active tab's buffer (or its selection)
        /// in order, reporting each. Falls through to `RunQuery` on a buffer
        /// holding a single statement.
        RunScript,
        /// Open a new-connection form (the disconnected screen's ⌘N).
        NewConnection,
        /// Open the settings panel (⌘,). Also reachable from the gear and palette.
        Settings,
        /// Open the settings panel on its About tab (RED → About RED in the menu).
        About,
        /// Open the GitHub issue tracker in the browser (Help → Report a Bug…).
        /// Menu-only, like `About`: no default shortcut, so it's absent from
        /// `DEFAULTS`/the keymap editor.
        ReportBug,
        /// Open the connection switcher popover (⌘P).
        SwitchConnection,
        /// Switch to the previously-used connection (⌘⇧P): foreground the
        /// most-recently-used warm parked session. Toggles between the last two.
        SwitchToPreviousConnection,
        /// Open or close the cell detail inspector (⌘I).
        ToggleInspector,
        /// Close the cell detail inspector (Esc); a no-op when it's shut.
        CloseInspector,
        /// Open or close the AI assistant chat panel (⌘L).
        ToggleAssistant,
        /// Open or close the result filter bar (⌘⇧F).
        ToggleFilter,
        /// Open or close the find-in-result bar (⌘F when the grid is focused).
        FindInResult,
        /// Save the active tab's query as a named snippet (⇧⌘S).
        SaveQuery,
        /// Open the saved-query picker (⇧⌘O).
        OpenSavedQueries,
        /// Explain the active tab's query: open the plan view (⇧⌘E).
        Explain,
        /// Beautify the active editor's SQL in place (⌥⌘F).
        FormatSql,
        /// Begin editing the focused result cell in place (Enter / F2).
        BeginEdit,
        /// Submit the staged grid edits as one batch (⌘↵ in the grid).
        /// Falls back to running the query when nothing is staged.
        SubmitChanges,
        /// Discard the staged grid edits (⌘⌥Z).
        RevertChanges,
        /// Toggle deletion of the selected result row(s) (⌘⌫).
        DeleteRow,
        /// Pin or unpin the selected result row(s), holding them under the header
        /// while the grid scrolls (⌥⌘P).
        PinRow,
        /// Append a new draft (insert) row to the result (⌘⌥N).
        AddRow,
        /// Set the focused result cell to NULL (⌘⌥0).
        SetNull,
        /// Select the whole result: every row and data column (⌘A in the grid).
        SelectAll,
        /// Split the focused pane to the right (⌘\), putting a blank tab in the
        /// new pane. Repeatable: four presses give four columns.
        ///
        /// Named `ToggleSplit` for the two-pane split it used to be, and kept that
        /// way deliberately — the id is what a user's `keymap.toml` binds, so
        /// renaming it would silently drop their binding.
        ToggleSplit,
        /// Split the focused pane downward (⌘⇧\), stacking a new pane under it.
        SplitDown,
        /// Move focus to the next pane in visual order, wrapping (⌥⌘\).
        FocusOtherHalf,
        /// Close the focused pane, folding its tabs into the pane that takes its
        /// space (⌥⌘W).
        ClosePane,
        /// Zoom the focused pane to fill the work area, or restore the layout
        /// (⌘⇧↩).
        MaximizePane,
        /// Reset every pane divider to even shares (⌥⌘0).
        EqualizePanes,
    ]
);

/// ⌘1–⌘9: jump straight to the n-th connection in the switcher's order. The
/// payload is the 0-based slot. Carries data, so it can't sit in the unit-only
/// [`actions!`] table; it's bound programmatically in [`apply`] (like the
/// OS-shortcut Alt+F4) and so stays out of the rebind editor. `no_json` keeps it
/// off the JSON-keymap path it's never built from.
#[derive(Clone, PartialEq, Eq, Default, Debug, gpui::Action)]
#[action(namespace = red, no_json)]
pub(crate) struct SwitchToConnectionSlot(pub usize);

/// A focus-hint key pressed while the hint overlay is up.
///
/// This has to be an *action* rather than a plain key listener on the hint layer,
/// because gpui dispatches keymap bindings before key listeners and returns as
/// soon as one consumes the event. A hint key that collides with an app shortcut
/// would never reach a listener: with the trigger set to Cmd/Ctrl, ⌘1–⌘9 are
/// `SwitchToConnectionSlot`, so pressing hint `1` would switch *connection*
/// instead of moving focus. Bound in the `FocusHints` context, which only exists
/// while the layer holds focus and sits deeper than `RedRoot`, so it wins.
///
/// Carries the *character*, not the target's position. The alphabet is a live
/// setting, and characters keep the keymap out of it: the slot a character names
/// is resolved against the active alphabet when the key arrives, so changing the
/// setting needs no keymap reinstall and no restart.
#[derive(Clone, PartialEq, Eq, Default, Debug, gpui::Action)]
#[action(namespace = red, no_json)]
pub(crate) struct FocusHintKey(pub char);

/// The keyboard reference, grouped, for the shortcuts overlay (`⌘/`) and the
/// docs. Kept beside the bindings above so the two don't drift; the overlay is
/// built from this rather than hand-maintained in the view.
///
/// Each row carries a stable id alongside its English text, and the catalog key
/// is built from it (`shortcuts.<group>.<row>`). Deriving the key from the
/// description instead would orphan every translation the moment someone reworded
/// a line, which is exactly the copy edit a reference table invites.
pub(crate) fn shortcuts() -> Vec<(SharedString, Vec<(&'static str, SharedString)>)> {
    SHORTCUTS
        .iter()
        .map(|(gid, gen_label, rows)| {
            let title = crate::i18n::tr_or(&format!("shortcuts.{gid}.title"), gen_label);
            let rows = rows
                .iter()
                .map(|(rid, keys, desc)| {
                    (
                        *keys,
                        crate::i18n::tr_or(&format!("shortcuts.{gid}.{rid}"), desc),
                    )
                })
                .collect();
            (title, rows)
        })
        .collect()
}

/// The English source for [`shortcuts`]: `(group id, group name, rows)`, each row
/// `(row id, keystroke, description)`. Keystrokes are symbols and never
/// translated; `localize_hint` already rewrites the modifier per platform.
type ShortcutGroup = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str, &'static str)],
);

static SHORTCUTS: &[ShortcutGroup] = &[
    (
        "global",
        "Global",
        &[
            ("command_palette", "⌘K", "Command palette"),
            ("switch_connection", "⌘P", "Switch connection"),
            (
                "switch_to_previous_connection",
                "⌘⇧P",
                "Switch to previous connection",
            ),
            (
                "jump_to_connection_by_position",
                "⌘1–9",
                "Jump to connection by position",
            ),
            ("keyboard_shortcuts", "⌘/", "Keyboard shortcuts"),
            ("settings", "⌘,", "Settings"),
            (
                "new_connection_welcome_screen",
                "⌘N",
                "New connection (welcome screen)",
            ),
            ("quit", "⌘Q", "Quit"),
        ],
    ),
    (
        "panes",
        "Panes",
        &[
            (
                "focus_hints",
                "hold ⌥",
                "Show a jump key on every panel, then press it",
            ),
            (
                "cycle_focus_forward_back",
                "F6 / ⇧F6",
                "Cycle focus forward / back",
            ),
            ("toggle_schema_sidebar", "⌘B", "Toggle schema sidebar"),
            ("split_pane_right", "⌘\\", "Split pane right"),
            ("split_pane_down", "⌘⇧\\", "Split pane down"),
            ("focus_next_pane", "⌥⌘\\", "Focus next pane"),
            ("close_pane", "⌥⌘W", "Close pane"),
            ("maximize_pane", "⌘⇧↩", "Maximize / restore pane"),
            ("equalize_panes", "⌥⌘0", "Equalize pane sizes"),
        ],
    ),
    (
        "query_tabs",
        "Query tabs",
        &[
            ("new_tab", "⌘T", "New tab"),
            ("close_tab", "⌘W", "Close tab"),
            ("next_previous_tab", "⌃Tab / ⌃⇧Tab", "Next / previous tab"),
            ("run_query", "⌘↵", "Run query"),
            ("format_sql", "⌥⌘F", "Format SQL"),
            ("find_in_query", "⌘F", "Find in query…"),
            ("explain_query_plan", "⇧⌘E", "Explain query (plan)"),
            ("save_query", "⇧⌘S", "Save query"),
            ("open_saved_query", "⇧⌘O", "Open saved query…"),
            (
                "leave_the_editor_for_the_result_grid",
                "Esc",
                "Leave the editor for the result grid",
            ),
        ],
    ),
    (
        "result_grid",
        "Result grid",
        &[
            ("move_cell_cursor", "↑ ↓ ← →", "Move cell cursor"),
            ("extend_selection", "⇧ + arrows", "Extend selection"),
            ("row_start_end", "⌘← / ⌘→", "Row start / end"),
            ("first_last_row", "⌘↑ / ⌘↓", "First / last row"),
            ("page_up_down", "PgUp / PgDn", "Page up / down"),
            ("select_all", "⌘A", "Select all"),
            ("go_to_row", "⌃G", "Go to row…"),
            ("copy_selection", "⌘C", "Copy selection"),
            ("inspect_cell", "⌘I", "Inspect cell"),
            ("find_in_loaded_rows", "⌘F", "Find in loaded rows…"),
            ("filter_rows", "⌘⇧F", "Filter rows…"),
        ],
    ),
    (
        "editing_data",
        "Editing data",
        &[
            ("edit_the_focused_cell", "↵ / F2", "Edit the focused cell"),
            ("submit_staged_changes", "⌘↵", "Submit staged changes"),
            ("revert_staged_changes", "⌥⌘Z", "Revert staged changes"),
            ("mark_row_s_for_deletion", "⌘⌫", "Mark row(s) for deletion"),
            ("add_a_new_row", "⌥⌘N", "Add a new row"),
            ("set_cell_to_null", "⌥⌘0", "Set cell to NULL"),
        ],
    ),
    (
        "schema_tree",
        "Schema tree",
        &[
            ("move_selection", "↑ / ↓", "Move selection"),
            ("collapse_expand", "← / →", "Collapse / expand"),
            ("open_table_preview", "↵", "Open table preview"),
            (
                "search_schema_focus_filter",
                "⌘F",
                "Search schema (focus filter)",
            ),
            ("refresh_schema", "⌘R", "Refresh schema"),
            ("transfer_selection", "F5", "Transfer table / database to…"),
        ],
    ),
    (
        "mongodb_browser",
        "MongoDB browser",
        &[
            (
                "move_selection_collection_tree_document_grid",
                "↑ / ↓",
                "Move selection (collection tree, document grid)",
            ),
            (
                "collapse_expand_a_database",
                "← / →",
                "Collapse / expand a database",
            ),
            (
                "open_the_highlighted_collection_document",
                "↵ / F2",
                "Open the highlighted collection / document",
            ),
            (
                "focus_collection_tree_document_grid",
                "⌥⌘1 / ⌥⌘3",
                "Focus collection tree / document grid",
            ),
            (
                "search_collections_tree_filter_documents_grid",
                "⌘F",
                "Search collections (tree) / filter documents (grid)",
            ),
        ],
    ),
    (
        "dialogs",
        "Dialogs",
        &[
            ("confirm_connect", "↵", "Confirm / connect"),
            ("cancel_close", "Esc", "Cancel / close"),
            (
                "cycle_controls_trapped",
                "Tab / ⇧Tab",
                "Cycle controls (trapped)",
            ),
        ],
    ),
    (
        "welcome_screen",
        "Welcome screen",
        &[
            (
                "move_between_saved_connections",
                "↑ / ↓",
                "Move between saved connections",
            ),
            ("page_the_connection_list", "← / →", "Page the list"),
            (
                "jump_to_the_first_last_connection",
                "Home / End",
                "Jump to the first / last connection",
            ),
            (
                "connect_to_the_highlighted_one",
                "↵",
                "Connect to the highlighted one",
            ),
            ("search_connections", "/", "Search connections"),
            (
                "edit_the_highlighted_connection",
                "E",
                "Edit the highlighted connection",
            ),
            (
                "remove_the_highlighted_connection",
                "⌫",
                "Remove the highlighted connection",
            ),
            ("new_connection", "⌘N", "New connection"),
        ],
    ),
];

/// Rewrite RED's canonical `cmd-*` chords onto the key GPUI actually matches per
/// platform. `cmd` is the primary modifier (the Cmd key) only on macOS; on
/// Windows/Linux a bare `cmd` token binds the Win/Super key, which the OS owns,
/// so every default would be dead there. GPUI's `secondary` token resolves to
/// Cmd on macOS and Ctrl elsewhere, so swapping the `cmd` component for it makes
/// the bindings fire as the user expects on every platform. `ctrl`, `alt`,
/// `shift` and literal-`ctrl` chords (`ctrl-tab`, `ctrl-g`) are left alone; they
/// mean the same physical key everywhere. Applied at the single bind chokepoint
/// ([`bind_named`]) so defaults *and* `keymap.toml` overrides get the same
/// treatment; the canonical `cmd-*` form is what the editor and file still use.
fn platform_chord(chord: &str) -> String {
    chord
        .split(' ')
        .map(|step| {
            step.split('-')
                .map(|part| {
                    if part.eq_ignore_ascii_case("cmd") {
                        "secondary"
                    } else {
                        part
                    }
                })
                .collect::<Vec<_>>()
                .join("-")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Localize a pre-rendered macOS-glyph shortcut hint (`⌘⇧F`, `⌥⌘1`, `⌃Tab`) to
/// the host platform. A no-op on macOS; on Windows/Linux it spells the modifier
/// glyphs (`Ctrl`/`Alt`/`Shift`, with the `⌘` primary folded onto `Ctrl` to match
/// what [`platform_chord`] binds) and `+`-joins them, e.g. `⌘⇧F` → `Ctrl+Shift+F`.
/// Non-modifier glyphs (arrows, `↵`) and plain text pass through unchanged, so it
/// is safe to run over any hint string, including ones with no shortcut at all.
pub(crate) fn localize_hint(hint: &str) -> String {
    if cfg!(target_os = "macos") {
        return hint.to_string();
    }
    hint.split(' ')
        .map(localize_token)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Localize one whitespace-delimited token of a hint: peel any leading modifier
/// glyphs, then spell + `+`-join them ahead of the remaining key text. A token
/// with no leading modifier (a separator, an arrow, plain text) is returned as is.
fn localize_token(token: &str) -> String {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut rest = token;
    loop {
        let mut chars = rest.chars();
        match chars.next() {
            // `⌘` (primary) and `⌃` (literal ctrl) both render as Ctrl off macOS.
            Some('⌘') | Some('⌃') => ctrl = true,
            Some('⌥') => alt = true,
            Some('⇧') => shift = true,
            _ => break,
        }
        rest = chars.as_str();
    }
    if !(ctrl || alt || shift) {
        return token.to_string();
    }
    // A stable, conventional order regardless of how the source glyphs were written.
    let mods = [(ctrl, "Ctrl"), (alt, "Alt"), (shift, "Shift")]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect::<Vec<_>>()
        .join("+");
    if rest.is_empty() {
        mods
    } else {
        format!("{mods}+{rest}")
    }
}

/// One default binding and the metadata the keymap editor needs to present it.
/// `action` is the short name a user writes in `keymap.toml`, so this table
/// doubles as the bindable-action allowlist: every name here is one
/// [`bind_named`] resolves; `label` is the human title the editor shows.
/// `context = None` is a true global; `Some("RedRoot")` is app chrome (see the
/// module doc for why each lives where it does). An action with two default keys
/// (e.g. `BeginEdit`: Enter and F2) appears once per key; the editor lists each
/// as its own rebindable row.
pub(crate) struct ActionDef {
    /// The default keystroke, in `keymap.toml`'s canonical form (`cmd-shift-f`).
    pub keystroke: &'static str,
    /// The action name: the allowlist key and what `keymap.toml` writes. Also the
    /// catalog namespace this row's label is translated under, for the same
    /// reason the settings registry keys on its `settings.toml` path: the name a
    /// row already answers to in its config file is the one identity that will
    /// not move when someone rewords the label.
    pub action: &'static str,
    /// The English source text. Named for the language it is in because rendering
    /// it directly would pin the UI to English; [`label`](ActionDef::label) is
    /// what the editor draws.
    pub en_label: &'static str,
    /// The key-context, or `None` for a true global.
    pub context: Option<&'static str>,
}

impl ActionDef {
    /// This action's title in the active locale.
    pub(crate) fn label(&self) -> gpui::SharedString {
        crate::i18n::tr_or(&format!("keymap.{}.label", self.action), self.en_label)
    }
}

/// The full bindable-action registry: RED's built-in keybindings and the source
/// of truth a user's `keymap.toml` overlays. One row per default binding; read by
/// [`apply`] (to install the defaults) and by the keymap editor (to list every
/// rebindable action). Grouped to mirror the keyboard reference; the per-binding
/// rationale (why a key is global vs. `RedRoot`-scoped) is in the comments.
const DEFAULTS: &[ActionDef] = &[
    // --- true globals (work from any phase) ---
    // ⌘K toggles the command palette; ⌘P the connection switcher; ⌃G opens "go to
    // row"; ⌘Q quits (we render a seamless titlebar with no native app menu, so
    // quit is ours).
    def("cmd-k", "ToggleCommandPalette", "Command palette", None),
    def("cmd-p", "SwitchConnection", "Switch connection", None),
    // ⌘⇧P mirrors ⌘P: flip to the previously-used connection (the MRU warm
    // session). A true global like ⌘P so it fires from any focus.
    def(
        "cmd-shift-p",
        "SwitchToPreviousConnection",
        "Switch to previous connection",
        None,
    ),
    def("ctrl-g", "GoToRow", "Go to row", None),
    // ⌘O jumps to a schema object. `RedRoot`-scoped rather than a true global so
    // a focused text field keeps any ⌘O of its own; nothing binds it today, and
    // the editor's `CodeEditor` context does not, so it fires from the editor too.
    def("cmd-o", "GoToObject", "Go to table", Some("RedRoot")),
    // F5 opens the transfer wizard on the tree's selection. `RedRoot`-scoped so a
    // focused field or editor keeps any F5 of its own; nothing binds it today, so
    // in practice it fires from anywhere in the app.
    def("f5", "OpenTransfer", "Transfer to…", Some("RedRoot")),
    def("cmd-q", "Quit", "Quit", None),
    // --- RedRoot: app chrome, fires from any focus within the app ---
    // ⌘C copies the result grid's selection, scoped to `RedRoot` so a focused text
    // field or the SQL editor keeps its own ⌘C (their context sits deeper in the
    // focus path and wins); it only reaches here when neither is focused.
    def("cmd-c", "CopyResult", "Copy selection", Some("RedRoot")),
    // ⌘I toggles the cell detail inspector; Esc closes it. `RedRoot`-scoped so the
    // editor / a field / a modal (deeper contexts) keep their own ⌘I / Esc; this
    // fires only from the grid, schema, or root, where Esc was otherwise unbound.
    def(
        "cmd-i",
        "ToggleInspector",
        "Toggle cell inspector",
        Some("RedRoot"),
    ),
    def(
        "escape",
        "CloseInspector",
        "Close cell inspector",
        Some("RedRoot"),
    ),
    // ⌘L toggles the AI assistant panel. `RedRoot`-scoped like the inspector so a
    // focused editor / field keeps its own ⌘L; it fires from the grid, schema, or
    // root. The panel has a close button for when its own input has focus.
    def(
        "cmd-l",
        "ToggleAssistant",
        "Toggle AI agent",
        Some("RedRoot"),
    ),
    // Result filter bar. ⌘⇧F to keep plain ⌘F as schema search.
    def(
        "cmd-shift-f",
        "ToggleFilter",
        "Toggle filter bar",
        Some("RedRoot"),
    ),
    // Saved queries: ⇧⌘S saves the active query, ⇧⌘O opens the picker. Both reach
    // `RedRoot` from the editor (the `CodeEditor` context binds neither).
    def("cmd-shift-s", "SaveQuery", "Save query", Some("RedRoot")),
    def(
        "cmd-shift-o",
        "OpenSavedQueries",
        "Open saved query",
        Some("RedRoot"),
    ),
    // EXPLAIN the active query. ⇧⌘E pairs with the Run idiom; the analyze variant
    // is palette / run-bar only (it executes the statement).
    def("cmd-shift-e", "Explain", "Explain query", Some("RedRoot")),
    // Beautify the editor's SQL. ⌥⌘F is the common "format" idiom and is free
    // (⇧⌘F is the result filter). RedRoot-scoped so it fires while the editor is
    // focused, like Explain; the handler no-ops when there's nothing to format.
    def("cmd-alt-f", "FormatSql", "Format SQL", Some("RedRoot")),
    // Tab management. `RedRoot` is an ancestor of the editor, so these still fire
    // while it's focused; none collide with the editor's keys (it binds plain
    // `tab`, not `ctrl-tab`).
    def("cmd-t", "NewTab", "New tab", Some("RedRoot")),
    def("cmd-w", "CloseTab", "Close tab", Some("RedRoot")),
    def("ctrl-tab", "NextTab", "Next tab", Some("RedRoot")),
    def("ctrl-shift-tab", "PrevTab", "Previous tab", Some("RedRoot")),
    // Schema sidebar + reload + filter. ⌘F reaches `RedRoot` from the editor (the
    // `CodeEditor` context doesn't bind it), so it always opens search.
    def("cmd-b", "ToggleSidebar", "Toggle schema", Some("RedRoot")),
    def("cmd-y", "ToggleHistory", "Toggle history", Some("RedRoot")),
    def(
        "cmd-shift-c",
        "ToggleColumnsPanel",
        "Toggle columns panel",
        Some("RedRoot"),
    ),
    def("cmd-r", "RefreshSchema", "Refresh schema", Some("RedRoot")),
    def("cmd-f", "SearchSchema", "Search schema", Some("RedRoot")),
    // Focus movement. The ⌥⌘1/2/3 jumps are retired: they named the SQL shell's
    // three panes, which Redis and MongoDB had to pun onto or ignore, and the
    // hold-to-reveal hint overlay reaches every surface in every seam with one
    // gesture. The two cycle keys keep their names and keys and are re-pointed at
    // the focus-target registry — renaming them would silently drop the binding
    // out of any user's `keymap.toml` for no gain.
    def(
        "f6",
        "CycleFocusNext",
        "Cycle focus forward",
        Some("RedRoot"),
    ),
    def(
        "shift-f6",
        "CycleFocusPrev",
        "Cycle focus back",
        Some("RedRoot"),
    ),
    // Discoverability. `⌘/` (not `?`) so typing `?` into the editor or a field
    // still inserts the character (a global `?` binding would swallow it).
    def(
        "cmd-/",
        "ShowShortcuts",
        "Keyboard shortcuts",
        Some("RedRoot"),
    ),
    // ⌘↵ runs the active tab's query from any pane. The editor's deeper
    // `CodeEditor` context keeps its own ⌘↵ (so a focused editor runs through its
    // Run event); this covers every other focus (grid, schema, root) and tests
    // the connection while the form is open.
    def("cmd-enter", "RunQuery", "Run query", Some("RedRoot")),
    // ⌥⌘↵ runs the whole buffer as a script. Beside ⌘↵ so the pair reads as one
    // idea; ⇧⌘↵ was taken (maximize pane) and ⌥ is the "more of this" modifier.
    def("cmd-alt-enter", "RunScript", "Run script", Some("RedRoot")),
    // ⌘N opens a new-connection form on the welcome screen (no-op elsewhere).
    def("cmd-n", "NewConnection", "New connection", Some("RedRoot")),
    // Settings. `⌘,` is the macOS-standard binding; the menu's RED → Settings…
    // item displays this accelerator by looking the action up here. About has no
    // shortcut; it's reachable only from the menu.
    def("cmd-,", "Settings", "Settings", Some("RedRoot")),
    // Panes: ⌘\ splits the focused pane to the right and ⌘⇧\ splits it downward
    // (the Zed idiom), both repeatable; ⌥⌘\ cycles focus. `RedRoot`-scoped so they
    // fire from any pane's focus.
    def("cmd-\\", "ToggleSplit", "Split pane right", Some("RedRoot")),
    def(
        "cmd-shift-\\",
        "SplitDown",
        "Split pane down",
        Some("RedRoot"),
    ),
    def(
        "cmd-alt-\\",
        "FocusOtherHalf",
        "Focus next pane",
        Some("RedRoot"),
    ),
    def("cmd-alt-w", "ClosePane", "Close pane", Some("RedRoot")),
    def(
        "cmd-shift-enter",
        "MaximizePane",
        "Maximize / restore pane",
        Some("RedRoot"),
    ),
    def(
        "cmd-alt-0",
        "EqualizePanes",
        "Equalize pane sizes",
        Some("RedRoot"),
    ),
    // --- staged grid editing ---
    // Scoped to the `Table` context (the result grid's focus context, set by
    // Flint's `Table`) so they fire only with the grid focused and never touch the
    // editor / schema tree. The `Table` context sits below `RedRoot`, so its
    // `cmd-enter` (Submit) wins over `RedRoot`'s Run while editing data; with
    // nothing staged the handler falls through to running the query.
    def("enter", "BeginEdit", "Edit cell", Some("Table")),
    def("f2", "BeginEdit", "Edit cell", Some("Table")),
    def(
        "cmd-enter",
        "SubmitChanges",
        "Submit changes",
        Some("Table"),
    ),
    def(
        "cmd-alt-z",
        "RevertChanges",
        "Revert changes",
        Some("Table"),
    ),
    def(
        "cmd-backspace",
        "DeleteRow",
        "Mark row for deletion",
        Some("Table"),
    ),
    // ⌥⌘P pins the selected row(s) under the header. `Table`-scoped, so with the
    // grid focused it wins over the dev-only perf-HUD toggle bound on the same
    // chord with no context; anywhere else that toggle still fires.
    def("cmd-alt-p", "PinRow", "Pin row", Some("Table")),
    def("cmd-alt-n", "AddRow", "Add row", Some("Table")),
    def("cmd-alt-0", "SetNull", "Set cell to NULL", Some("Table")),
    // ⌘A selects the whole result. `Table`-scoped so it fires only with the grid
    // focused; a focused text field / SQL editor (deeper contexts) keeps its own
    // ⌘A (select all text).
    def("cmd-a", "SelectAll", "Select all", Some("Table")),
    // ⌘F finds within the focused pane: loaded rows in the grid, text in the SQL
    // editor. Bound in `Table` and `CodeEditor` (both below `RedRoot`), so it wins
    // over `RedRoot`'s `SearchSchema` only while one of those is focused;
    // elsewhere ⌘F still focuses the schema filter. The single `FindInResult`
    // handler picks the target from which pane holds focus.
    def("cmd-f", "FindInResult", "Find", Some("Table")),
    def("cmd-f", "FindInResult", "Find", Some("CodeEditor")),
];

/// A `const fn` shorthand so [`DEFAULTS`] reads as a compact table rather than a
/// wall of struct-literal field names; every row is one `def(key, action, label,
/// context)`.
const fn def(
    keystroke: &'static str,
    action: &'static str,
    en_label: &'static str,
    context: Option<&'static str>,
) -> ActionDef {
    ActionDef {
        keystroke,
        action,
        en_label,
        context,
    }
}

/// The bindable-action registry: every default binding, for the keymap editor to
/// list and rebind. One row per default keystroke (an action with two default
/// keys appears twice).
pub(crate) fn action_defs() -> &'static [ActionDef] {
    DEFAULTS
}

/// The editor's per-row "effective keystroke" model: a slot for each [`ActionDef`]
/// (same length and order as [`action_defs`]), holding the keystroke that row is
/// currently bound to: `Some(k)` bound, `None` unbound. The pure bridge between
/// the per-keystroke `keymap.toml` and the per-action editor; [`effective_slots`]
/// reads it, [`diff_blocks`] writes it back.
pub(crate) type Slots = Vec<Option<String>>;

/// Build the effective per-row keystrokes by overlaying a user's override blocks
/// on the defaults; the editor's read model. Each row starts at its default
/// keystroke; an `keystroke = action` override moves that action's row onto the
/// keystroke, and an `"unbind"`/`"none"` clears whichever row currently sits on
/// the keystroke. Mirrors GPUI's own last-wins resolution closely enough for the
/// editor to present the live keymap, then round-trip it through [`diff_blocks`].
pub(crate) fn effective_slots(blocks: &[KeymapBlock]) -> Slots {
    let mut slots: Slots = DEFAULTS
        .iter()
        .map(|d| Some(d.keystroke.to_string()))
        .collect();
    // Track which rows an assignment has already claimed this load, so a second
    // override for a two-key action (BeginEdit) lands on its other row rather than
    // overwriting the first.
    let mut claimed = vec![false; DEFAULTS.len()];

    for block in blocks {
        let ctx = block.context.as_deref();
        for (keystroke, target) in &block.bindings {
            if UNBIND_NAMES.contains(&target.as_str()) {
                // Clear whichever row in this context currently sits on the key.
                for (i, slot) in slots.iter_mut().enumerate() {
                    if DEFAULTS[i].context == ctx && slot.as_deref() == Some(keystroke.as_str()) {
                        *slot = None;
                    }
                }
                continue;
            }
            // An assignment: move the named action's row onto this keystroke.
            // Prefer a row still on its own default (and unclaimed), so the two
            // BeginEdit rows stay distinct; else the first matching row.
            let pick = (0..DEFAULTS.len())
                .find(|&i| {
                    DEFAULTS[i].action == target
                        && DEFAULTS[i].context == ctx
                        && !claimed[i]
                        && slots[i].as_deref() == Some(DEFAULTS[i].keystroke)
                })
                .or_else(|| {
                    (0..DEFAULTS.len()).find(|&i| {
                        DEFAULTS[i].action == target && DEFAULTS[i].context == ctx && !claimed[i]
                    })
                });
            if let Some(i) = pick {
                slots[i] = Some(keystroke.clone());
                claimed[i] = true;
            }
        }
    }
    slots
}

/// Translate the editor's per-row model back into the *minimal* `keymap.toml`
/// override blocks, the inverse of [`effective_slots`]. A row still on its
/// default emits nothing; a moved row emits its new `keystroke = action`; and a
/// default keystroke that no row occupies any more is emitted as `"unbind"` so the
/// freed default stops firing its old action. This minimality is what keeps a
/// GUI-written file small and interchangeable with a hand-edited one.
pub(crate) fn diff_blocks(slots: &Slots) -> Vec<KeymapBlock> {
    // Every keystroke a row currently occupies, per context, used to decide
    // whether a freed default needs an explicit unbind (it doesn't if another
    // action's override already shadows it).
    let occupied: Vec<Option<&str>> = slots.iter().map(Option::as_deref).collect();
    let is_occupied = |ctx: Option<&str>, key: &str| -> bool {
        (0..DEFAULTS.len()).any(|i| DEFAULTS[i].context == ctx && occupied[i] == Some(key))
    };

    // Group emitted entries by context. `Option<String>` sorts `None` (globals)
    // first, giving a stable, readable file order.
    let mut by_ctx: BTreeMap<Option<String>, BTreeMap<String, String>> = BTreeMap::new();

    for (i, d) in DEFAULTS.iter().enumerate() {
        let ctx_owned = d.context.map(str::to_string);
        match slots[i].as_deref() {
            // On its default: nothing to emit.
            Some(k) if k == d.keystroke => {}
            // Moved to a new key: bind it; the freed default is handled below.
            Some(k) => {
                by_ctx
                    .entry(ctx_owned)
                    .or_default()
                    .insert(k.to_string(), d.action.to_string());
            }
            // Unbound: nothing positive to emit; the freed default is handled below.
            None => {}
        }
        // The row left its default key. If no other row took that key, the default
        // would still fire its old action, so suppress it with an explicit unbind.
        if slots[i].as_deref() != Some(d.keystroke) && !is_occupied(d.context, d.keystroke) {
            by_ctx
                .entry(d.context.map(str::to_string))
                .or_default()
                .insert(d.keystroke.to_string(), "unbind".to_string());
        }
    }

    by_ctx
        .into_iter()
        .map(|(context, bindings)| KeymapBlock { context, bindings })
        .collect()
}

/// The row, if any, that already binds `keystroke` in the same context as `row`,
/// i.e. a collision a rebind to `keystroke` would create. The editor surfaces this
/// before committing so a duplicate is never a silent shadow.
pub(crate) fn conflict_for(slots: &Slots, row: usize, keystroke: &str) -> Option<usize> {
    let ctx = DEFAULTS[row].context;
    (0..slots.len())
        .find(|&j| j != row && DEFAULTS[j].context == ctx && slots[j].as_deref() == Some(keystroke))
}

/// The reserved action names that mean "remove the default for this keystroke"
/// (TOML has no `null`, so an explicit word stands in). Bound to GPUI's
/// [`NoAction`], which unbinds when it is the highest-precedence match.
const UNBIND_NAMES: [&str; 2] = ["unbind", "none"];

/// Install the full keymap from scratch and return any per-binding warnings from
/// the user overrides (an unknown action, an unparseable keystroke). Total and
/// idempotent: safe to call at startup and again on every `keymap.toml` edit.
pub(crate) fn apply(cx: &mut App, overrides: &[KeymapBlock]) -> Vec<String> {
    // A clear wipes *everything*, so re-install the Flint component keymaps first
    // (their contexts must win for keys typed inside the editor / fields), then
    // RED's defaults, then the user's overrides last so they take precedence.
    cx.clear_key_bindings();
    bind_components(cx);
    cx.bind_keys(default_bindings());
    // ⌘1–⌘9 jump to the first nine connections in the switcher's order. Bound here
    // (not in DEFAULTS) so the nine slots stay out of the rebind editor (like the
    // OS-shortcut Alt+F4 below), and as true globals (no context) so they fire from
    // any focus. `platform_chord` makes them Cmd on macOS / Ctrl elsewhere.
    cx.bind_keys((1..=9u8).map(|n| {
        KeyBinding::new(
            &platform_chord(&format!("cmd-{n}")),
            SwitchToConnectionSlot((n - 1) as usize),
            None,
        )
    }));
    // Alt+F4 closes the window, the Windows and (most) Linux convention. It
    // usually arrives from the OS / compositor, but not every Wayland compositor
    // binds it, so wire it explicitly. Bound here rather than in `DEFAULTS` so it
    // stays out of the rebind editor (it's an OS shortcut, not app chrome), and
    // skipped on macOS, where ⌘Q is the convention and F4 isn't a close key.
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([KeyBinding::new("alt-f4", crate::Quit, None)]);
    // Focus hints: one binding per hint character, in the `FocusHints` context
    // the hint layer declares while it holds focus. Bound for every modifier a
    // trigger could be, plus the bare key and the shifted forms, because the
    // trigger is a live setting and the keymap is installed once — and because
    // the whole point is to out-rank the `RedRoot` bindings some of these keys
    // collide with. Programmatic, like `SwitchToConnectionSlot`, so they stay out
    // of the rebind editor.
    //
    // The shifted forms are not padding. On a layout whose digits sit on the
    // shifted level (Czech, French), the keystroke that types `1` is Shift plus
    // that key, and gpui reports it as `alt-1` with the shift folded away — but
    // only for keys whose unshifted character is not a lowercase letter. Binding
    // both forms covers the two cases without having to know the layout.
    for hint in crate::focus::all_hint_keys() {
        for prefix in [
            "",
            "alt-",
            "secondary-",
            "shift-",
            "ctrl-",
            "alt-shift-",
            "secondary-shift-",
            "ctrl-shift-",
        ] {
            cx.bind_keys([KeyBinding::new(
                &format!("{prefix}{hint}"),
                FocusHintKey(hint),
                Some("FocusHints"),
            )]);
        }
    }
    // Dev-only perf HUD toggle (⌥⌘P). Re-bound here so a keymap reload's clear
    // doesn't drop it; the action itself is declared in `main` under the feature.
    #[cfg(feature = "dev-stats")]
    cx.bind_keys([KeyBinding::new(
        &platform_chord("cmd-alt-p"),
        crate::ToggleDevStats,
        None,
    )]);

    let mut warnings = Vec::new();
    let user = user_bindings(overrides, &mut warnings);
    if !user.is_empty() {
        cx.bind_keys(user);
    }
    warnings
}

/// Install only the defaults (no overrides). The startup baseline `main` calls
/// before settings load, so the app is never unbound; `AppState` re-applies with
/// the loaded overrides once it has read `keymap.toml`.
pub(crate) fn bind_all(cx: &mut App) {
    apply(cx, &[]);
}

/// The Flint component keymaps (editing keys, palette/switcher/combobox/modal
/// navigation). Installed before RED's own so their deeper contexts win.
fn bind_components(cx: &mut App) {
    TextInput::bind_keys(cx);
    CodeEditor::bind_keys(cx);
    MarkdownEditor::bind_keys(cx);
    Palette::bind_keys(cx);
    Modal::bind_keys(cx);
    Switcher::bind_keys(cx);
    ComboBox::bind_keys(cx);
    SelectableLabel::bind_keys(cx);
}

/// Build the default bindings from [`DEFAULTS`]. The names are known-good, so a
/// failure here is a programmer error in the table, not user input.
fn default_bindings() -> Vec<KeyBinding> {
    DEFAULTS
        .iter()
        .map(|d| {
            #[allow(
                clippy::expect_used,
                reason = "DEFAULTS is a compile-time table of valid bindings"
            )]
            let bound = bind_named(d.keystroke, d.action, d.context)
                .expect("DEFAULTS holds a valid binding");
            bound
        })
        .collect()
}

/// Compile a user's override blocks into bindings, pushing a warning for each
/// entry it has to skip (bad context, bad keystroke, unknown action) so one typo
/// never drops the rest, mirroring how `settings.toml` degrades per section.
fn user_bindings(blocks: &[KeymapBlock], warnings: &mut Vec<String>) -> Vec<KeyBinding> {
    let mut out = Vec::new();
    for block in blocks {
        let context = block.context.as_deref();
        // Validate the block's context once; a bad predicate skips the whole block
        // (every binding in it would fail identically).
        if let Some(c) = context
            && let Err(e) = KeyBindingContextPredicate::parse(c)
        {
            warnings.push(format!(
                "keymap.toml: bad context “{c}” ({e}); skipping its bindings"
            ));
            continue;
        }
        for (keystroke, action) in &block.bindings {
            match make_binding(keystroke, action, context) {
                Ok(binding) => out.push(binding),
                Err(e) => warnings.push(format!("keymap.toml: {e}")),
            }
        }
    }
    out
}

/// Build one user binding, validating the keystroke and action *before* the
/// (panicking) `KeyBinding::new` so malformed input becomes a warning, never a
/// crash. The context is assumed already validated by the caller.
fn make_binding(
    keystroke: &str,
    action: &str,
    context: Option<&str>,
) -> Result<KeyBinding, String> {
    if keystroke.split_whitespace().next().is_none() {
        return Err("empty keystroke; skipping".to_string());
    }
    for token in keystroke.split_whitespace() {
        Keystroke::parse(token)
            .map_err(|e| format!("can't parse keystroke “{keystroke}” ({e}); skipping"))?;
    }
    bind_named(keystroke, action, context)
}

/// Resolve a short action name to a `KeyBinding`. The match is the bindable-action
/// allowlist: a name not here (and not an unbind word) is rejected. `KeyBinding::
/// new` panics on a bad keystroke/context, so callers binding user input must
/// validate those first (see [`make_binding`]); [`DEFAULTS`] is known-good.
fn bind_named(keystroke: &str, action: &str, context: Option<&str>) -> Result<KeyBinding, String> {
    // Bind the platform-resolved chord (`cmd` → `secondary`), not the canonical
    // form, so the key fires as Cmd on macOS and Ctrl on Windows/Linux.
    let keystroke = platform_chord(keystroke);
    let keystroke = keystroke.as_str();
    macro_rules! kb {
        ($action:expr_2021) => {
            KeyBinding::new(keystroke, $action, context)
        };
    }
    if UNBIND_NAMES.contains(&action) {
        return Ok(kb!(NoAction));
    }
    Ok(match action {
        "ToggleCommandPalette" => kb!(ToggleCommandPalette),
        "SwitchConnection" => kb!(SwitchConnection),
        "SwitchToPreviousConnection" => kb!(SwitchToPreviousConnection),
        "GoToRow" => kb!(GoToRow),
        "GoToObject" => kb!(GoToObject),
        "Quit" => kb!(Quit),
        "CopyResult" => kb!(CopyResult),
        "ToggleInspector" => kb!(ToggleInspector),
        "CloseInspector" => kb!(CloseInspector),
        "ToggleAssistant" => kb!(ToggleAssistant),
        "ToggleFilter" => kb!(ToggleFilter),
        "FindInResult" => kb!(FindInResult),
        "SaveQuery" => kb!(SaveQuery),
        "OpenSavedQueries" => kb!(OpenSavedQueries),
        "Explain" => kb!(Explain),
        "FormatSql" => kb!(FormatSql),
        "NewTab" => kb!(NewTab),
        "CloseTab" => kb!(CloseTab),
        "NextTab" => kb!(NextTab),
        "PrevTab" => kb!(PrevTab),
        "ToggleSidebar" => kb!(ToggleSidebar),
        "ToggleHistory" => kb!(ToggleHistory),
        "ToggleColumnsPanel" => kb!(ToggleColumnsPanel),
        "RefreshSchema" => kb!(RefreshSchema),
        "SearchSchema" => kb!(SearchSchema),
        // Retired in favour of the hold-to-reveal hint overlay, but still
        // resolvable: `bind_named` doubles as the allowlist for `keymap.toml`, so
        // dropping the names outright would turn an existing user's binding into
        // a load warning and a dead key. They now cycle instead of jumping to a
        // fixed pane, which is the nearest surviving behaviour. Remove a release
        // after the overlay ships.
        "FocusSchema" | "FocusEditor" | "FocusGrid" => kb!(CycleFocusNext),
        "CycleFocusNext" => kb!(CycleFocusNext),
        "CycleFocusPrev" => kb!(CycleFocusPrev),
        "ShowShortcuts" => kb!(ShowShortcuts),
        "OpenTransfer" => kb!(OpenTransfer),
        "RunQuery" => kb!(RunQuery),
        "RunScript" => kb!(RunScript),
        "NewConnection" => kb!(NewConnection),
        "Settings" => kb!(Settings),
        "BeginEdit" => kb!(BeginEdit),
        "SubmitChanges" => kb!(SubmitChanges),
        "RevertChanges" => kb!(RevertChanges),
        "DeleteRow" => kb!(DeleteRow),
        "PinRow" => kb!(PinRow),
        "AddRow" => kb!(AddRow),
        "SetNull" => kb!(SetNull),
        "SelectAll" => kb!(SelectAll),
        "ToggleSplit" => kb!(ToggleSplit),
        "SplitDown" => kb!(SplitDown),
        "FocusOtherHalf" => kb!(FocusOtherHalf),
        "ClosePane" => kb!(ClosePane),
        "MaximizePane" => kb!(MaximizePane),
        "EqualizePanes" => kb!(EqualizePanes),
        other => return Err(format!("unknown action “{other}”; skipping")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The English catalog has to reproduce this table's source text exactly.
    ///
    /// `assets/i18n/keymap/en.ftl` is generated from `DEFAULTS`, so the two drift
    /// the moment someone edits a label without re-running the extractor. Looks
    /// the key up raw rather than through [`ActionDef::label`], which falls back
    /// to the English source and would hide exactly that.
    #[test]
    fn every_action_label_is_in_the_english_catalog() {
        crate::i18n::apply(crate::i18n::DEFAULT);

        let stale: Vec<_> = DEFAULTS
            .iter()
            .filter_map(|d| {
                let key = format!("keymap.{}.label", d.action);
                let got = crate::i18n::lookup(&key);
                (got.as_ref() != d.en_label)
                    .then(|| format!("  {key}\n    catalog: {got}\n    code:    {}", d.en_label))
            })
            .collect();

        assert!(
            stale.is_empty(),
            "assets/i18n/keymap/en.ftl is out of date with keymap.rs:\n{}\n\n\
             Re-run: python3 scripts/i18n-extract.py",
            stale.join("\n")
        );
    }

    /// The English catalog has to reproduce the keyboard reference exactly.
    ///
    /// Counts the rows as well as checking them: the extractor reads this table
    /// with a regex, and rustfmt is free to wrap a long row across lines. A
    /// pattern that stops matching the wrapped ones under-extracts silently,
    /// which is how nine rows went missing the first time.
    #[test]
    fn every_shortcut_is_in_the_english_catalog() {
        crate::i18n::apply(crate::i18n::DEFAULT);

        let mut checked = 0;
        let mut stale = Vec::new();
        let mut check = |key: String, want: &str| {
            let got = crate::i18n::lookup(&key);
            if got.as_ref() != want {
                stale.push(format!("  {key}\n    catalog: {got}\n    code:    {want}"));
            }
        };

        for (gid, gname, rows) in SHORTCUTS {
            checked += 1;
            check(format!("shortcuts.{gid}.title"), gname);
            for (rid, _keys, desc) in *rows {
                checked += 1;
                check(format!("shortcuts.{gid}.{rid}"), desc);
            }
        }

        assert!(
            stale.is_empty(),
            "assets/i18n/shortcuts/en.ftl is out of date with keymap.rs:\n{}\n\n\
             Re-run: python3 scripts/i18n-extract.py",
            stale.join("\n")
        );
        assert_eq!(
            checked,
            SHORTCUTS.len() + SHORTCUTS.iter().map(|(_, _, r)| r.len()).sum::<usize>(),
            "the walk above missed rows"
        );
    }

    /// An action bound to two default keystrokes appears twice, and both rows must
    /// carry the same label: they collapse to one catalog key, so a mismatch would
    /// silently relabel one of them.
    #[test]
    fn an_action_has_one_label() {
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for d in DEFAULTS {
            let first = seen.entry(d.action).or_insert(d.en_label);
            assert_eq!(
                *first, d.en_label,
                "{} is labelled two ways: {:?} and {:?}",
                d.action, first, d.en_label
            );
        }
    }

    /// Every default the table holds resolves to a binding (catches a typo'd name
    /// or an invalid keystroke/context in `DEFAULTS` at test time, not on launch).
    #[test]
    fn all_defaults_resolve() {
        assert_eq!(default_bindings().len(), DEFAULTS.len());
    }

    #[test]
    fn platform_chord_rewrites_only_cmd() {
        // `cmd` becomes GPUI's platform-resolving `secondary`; other modifiers and
        // literal-`ctrl` chords are untouched.
        assert_eq!(platform_chord("cmd-k"), "secondary-k");
        assert_eq!(platform_chord("cmd-shift-f"), "secondary-shift-f");
        assert_eq!(platform_chord("cmd-alt-1"), "secondary-alt-1");
        assert_eq!(platform_chord("cmd-,"), "secondary-,");
        assert_eq!(platform_chord("ctrl-tab"), "ctrl-tab");
        assert_eq!(platform_chord("ctrl-shift-tab"), "ctrl-shift-tab");
        assert_eq!(platform_chord("f2"), "f2");
    }

    #[test]
    fn platform_chord_every_default_parses() {
        // The rewritten form must still be a keystroke GPUI accepts, on any host.
        for d in DEFAULTS {
            for token in platform_chord(d.keystroke).split_whitespace() {
                assert!(
                    Keystroke::parse(token).is_ok(),
                    "rewritten default {:?} → {:?} is unparseable",
                    d.keystroke,
                    token
                );
            }
        }
    }

    #[test]
    fn localize_token_spells_modifiers() {
        // `localize_token` is the off-macOS path: glyph runs become `+`-joined words
        // in a stable Ctrl/Alt/Shift order, key glyphs pass through, `⌘`/`⌃` fold.
        assert_eq!(localize_token("⌘⇧F"), "Ctrl+Shift+F");
        assert_eq!(localize_token("⌥⌘1"), "Ctrl+Alt+1");
        assert_eq!(localize_token("⇧⌘E"), "Ctrl+Shift+E");
        assert_eq!(localize_token("⌃Tab"), "Ctrl+Tab");
        assert_eq!(localize_token("⌘↵"), "Ctrl+↵");
        // No leading modifier: plain text and arrows are returned verbatim.
        assert_eq!(localize_token("Settings"), "Settings");
        assert_eq!(localize_token("→"), "→");
        assert_eq!(localize_token("/"), "/");
    }

    #[test]
    fn unknown_action_is_skipped_with_warning() {
        let mut warnings = Vec::new();
        let block = KeymapBlock {
            context: Some("RedRoot".into()),
            bindings: [("cmd-l".to_string(), "DoesNotExist".to_string())]
                .into_iter()
                .collect(),
        };
        let bindings = user_bindings(&[block], &mut warnings);
        assert!(bindings.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("DoesNotExist"));
    }

    #[test]
    fn bad_keystroke_is_skipped_with_warning() {
        let mut warnings = Vec::new();
        let block = KeymapBlock {
            // A trailing key component after the key is a structural parse error
            // (the same one `KeyBinding::new` would panic on, caught here instead).
            context: None,
            bindings: [("cmd-a-b".to_string(), "Quit".to_string())]
                .into_iter()
                .collect(),
        };
        let bindings = user_bindings(&[block], &mut warnings);
        assert!(bindings.is_empty());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn bad_context_skips_whole_block() {
        let mut warnings = Vec::new();
        let block = KeymapBlock {
            // An unbalanced predicate fails to parse.
            context: Some("RedRoot &&".into()),
            bindings: [
                ("cmd-l".to_string(), "ToggleFilter".to_string()),
                ("cmd-j".to_string(), "RunQuery".to_string()),
            ]
            .into_iter()
            .collect(),
        };
        let bindings = user_bindings(&[block], &mut warnings);
        assert!(bindings.is_empty());
        // One warning for the block, not one per binding.
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn valid_override_and_unbind_resolve() {
        let mut warnings = Vec::new();
        let block = KeymapBlock {
            context: Some("RedRoot".into()),
            bindings: [
                ("cmd-l".to_string(), "ToggleFilter".to_string()),
                ("cmd-shift-f".to_string(), "unbind".to_string()),
            ]
            .into_iter()
            .collect(),
        };
        let bindings = user_bindings(&[block], &mut warnings);
        assert_eq!(bindings.len(), 2);
        assert!(warnings.is_empty());
    }

    /// The slot for a given action+context, for the diff tests.
    fn row_of(action: &str, context: Option<&str>) -> usize {
        DEFAULTS
            .iter()
            .position(|d| d.action == action && d.context == context)
            .expect("action exists")
    }

    #[test]
    fn no_overrides_round_trip_to_no_blocks() {
        // Defaults in, nothing changed → an empty override set (a clean file).
        let slots = effective_slots(&[]);
        assert_eq!(slots.len(), DEFAULTS.len());
        assert!(diff_blocks(&slots).is_empty());
    }

    #[test]
    fn moving_an_action_unbinds_its_freed_default() {
        // Move ToggleFilter (cmd-shift-f, RedRoot) to cmd-l. The new key binds and
        // the freed default is unbound so it stops toggling the filter.
        let mut slots = effective_slots(&[]);
        slots[row_of("ToggleFilter", Some("RedRoot"))] = Some("cmd-l".to_string());
        let blocks = effective_slots(&diff_blocks(&slots));
        assert_eq!(blocks, slots, "diff → reload is identity");

        let out = diff_blocks(&slots);
        let block = out
            .iter()
            .find(|b| b.context.as_deref() == Some("RedRoot"))
            .expect("RedRoot block");
        assert_eq!(
            block.bindings.get("cmd-l").map(String::as_str),
            Some("ToggleFilter")
        );
        assert_eq!(
            block.bindings.get("cmd-shift-f").map(String::as_str),
            Some("unbind")
        );
    }

    #[test]
    fn swapping_two_keys_needs_no_unbind() {
        // A and B trade keys: each override shadows the other's default, so neither
        // default needs an explicit unbind.
        let a = row_of("NewTab", Some("RedRoot")); // cmd-t
        let b = row_of("CloseTab", Some("RedRoot")); // cmd-w
        let mut slots = effective_slots(&[]);
        slots[a] = Some("cmd-w".to_string());
        slots[b] = Some("cmd-t".to_string());

        let out = diff_blocks(&slots);
        let block = out
            .iter()
            .find(|bl| bl.context.as_deref() == Some("RedRoot"))
            .expect("RedRoot block");
        assert_eq!(
            block.bindings.get("cmd-w").map(String::as_str),
            Some("NewTab")
        );
        assert_eq!(
            block.bindings.get("cmd-t").map(String::as_str),
            Some("CloseTab")
        );
        // No stray unbinds; both defaults are still occupied (by the other action).
        assert!(!block.bindings.values().any(|v| v == "unbind"));
        assert_eq!(effective_slots(&out), slots, "swap round-trips");
    }

    #[test]
    fn unbinding_a_row_emits_unbind_only() {
        let mut slots = effective_slots(&[]);
        let row = row_of("Explain", Some("RedRoot")); // cmd-shift-e
        slots[row] = None;
        let out = diff_blocks(&slots);
        let block = out
            .iter()
            .find(|b| b.context.as_deref() == Some("RedRoot"))
            .expect("RedRoot block");
        assert_eq!(
            block.bindings.get("cmd-shift-e").map(String::as_str),
            Some("unbind")
        );
        // No positive binding for the now-keyless action.
        assert!(!block.bindings.values().any(|v| v == "Explain"));
        assert_eq!(effective_slots(&out), slots);
    }

    #[test]
    fn resetting_one_row_drops_its_entries() {
        // Two changes; resetting one back to default leaves only the other's entries.
        let mut slots = effective_slots(&[]);
        let filter = row_of("ToggleFilter", Some("RedRoot"));
        let save = row_of("SaveQuery", Some("RedRoot"));
        slots[filter] = Some("cmd-l".to_string());
        slots[save] = Some("cmd-j".to_string());
        // Reset the filter row to its default.
        slots[filter] = Some(DEFAULTS[filter].keystroke.to_string());

        let out = diff_blocks(&slots);
        let block = out
            .iter()
            .find(|b| b.context.as_deref() == Some("RedRoot"))
            .expect("RedRoot block");
        // SaveQuery's move survives; the filter row contributes nothing.
        assert_eq!(
            block.bindings.get("cmd-j").map(String::as_str),
            Some("SaveQuery")
        );
        assert!(!block.bindings.values().any(|v| v == "ToggleFilter"));
        assert!(!block.bindings.contains_key("cmd-l"));
    }

    #[test]
    fn two_key_action_rows_stay_distinct() {
        // BeginEdit has two default rows (enter, f2). Moving the enter row to cmd-e
        // must leave the f2 row alone, and round-trip cleanly.
        let mut slots = effective_slots(&[]);
        let enter_row = DEFAULTS
            .iter()
            .position(|d| d.action == "BeginEdit" && d.keystroke == "enter")
            .unwrap();
        slots[enter_row] = Some("cmd-e".to_string());
        let out = diff_blocks(&slots);
        assert_eq!(effective_slots(&out), slots, "two-key move round-trips");
        let block = out
            .iter()
            .find(|b| b.context.as_deref() == Some("Table"))
            .expect("Table block");
        assert_eq!(
            block.bindings.get("cmd-e").map(String::as_str),
            Some("BeginEdit")
        );
        assert_eq!(
            block.bindings.get("enter").map(String::as_str),
            Some("unbind")
        );
        // f2 is untouched, so it must not appear.
        assert!(!block.bindings.contains_key("f2"));
    }

    #[test]
    fn conflict_detects_same_context_duplicate() {
        let slots = effective_slots(&[]);
        let filter = row_of("ToggleFilter", Some("RedRoot")); // cmd-shift-f
        // cmd-t already binds NewTab in RedRoot.
        assert_eq!(
            conflict_for(&slots, filter, "cmd-t"),
            Some(row_of("NewTab", Some("RedRoot")))
        );
        // A free key collides with nothing.
        assert_eq!(conflict_for(&slots, filter, "cmd-d"), None);
        // The same key in a *different* context (Table vs RedRoot) is not a conflict.
        let begin = DEFAULTS
            .iter()
            .position(|d| d.action == "BeginEdit" && d.keystroke == "enter")
            .unwrap();
        assert_eq!(conflict_for(&slots, begin, "cmd-shift-f"), None);
    }
}
