//! The central keymap — the single place every global `actions!` declaration and
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
//! - no context — true globals that work from any phase (`⌘K`, `⌘Q`, …);
//! - `RedRoot` — app-chrome actions (tabs, sidebar, copy) that should fire from
//!   any focus *within* the app, since `RedRoot` is an ancestor of every pane.
//!
//! The bindings use `cmd-*` unconditionally, matching the rest of the app's
//! macOS-first chrome; per-platform `ctrl-*` splitting is a follow-up.
//!
//! **Re-applying.** [`apply`] is total: it `clear_key_bindings`, re-installs the
//! Flint component keymaps, the defaults, and the overrides, every time. That is
//! how a live `keymap.toml` edit takes effect with no restart — and why the
//! Flint keymaps must be re-bound here, not once at startup: a clear wipes them.

use flint::{CodeEditor, ComboBox, Modal, Palette, Switcher, TextInput};
use gpui::{actions, App, KeyBinding, KeyBindingContextPredicate, Keystroke, NoAction};

use crate::keymap_config::KeymapBlock;
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
        /// Reveal the schema sidebar and focus its filter field (search schema).
        SearchSchema,
        /// Cycle focus to the next / previous pane (schema → editor → grid).
        CycleFocusNext,
        CycleFocusPrev,
        /// Open the keyboard-shortcuts reference overlay.
        ShowShortcuts,
        /// ⌘↵ from anywhere: run the active tab's query — or, while the connection
        /// form is open, test the connection.
        RunQuery,
        /// Open a new-connection form (the disconnected screen's ⌘N).
        NewConnection,
        /// Open the settings panel (⌘,). Also reachable from the gear and palette.
        Settings,
        /// Open the settings panel on its About tab (RED → About RED in the menu).
        About,
        /// Open the connection switcher popover (⌘P).
        SwitchConnection,
        /// Open or close the cell detail inspector (⌘I).
        ToggleInspector,
        /// Close the cell detail inspector (Esc) — a no-op when it's shut.
        CloseInspector,
        /// Open or close the result filter bar (⌘⇧F) — Track B2.
        ToggleFilter,
        /// Save the active tab's query as a named snippet (⇧⌘S) — Track B3.
        SaveQuery,
        /// Open the saved-query picker (⇧⌘O) — Track B3.
        OpenSavedQueries,
        /// Explain the active tab's query — open the plan view (⇧⌘E) — Track B4.
        Explain,
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
                ("⌘P", "Switch connection"),
                ("⌘/", "Keyboard shortcuts"),
                ("⌘,", "Settings"),
                ("⌘N", "New connection (welcome screen)"),
                ("⌘Q", "Quit"),
            ],
        ),
        (
            "Panes",
            vec![
                ("⌥⌘1 / ⌥⌘2 / ⌥⌘3", "Focus schema / editor / grid"),
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
                ("⇧⌘E", "Explain query (plan)"),
                ("⇧⌘S", "Save query"),
                ("⇧⌘O", "Open saved query…"),
                ("Esc", "Leave the editor for the result grid"),
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
                ("⌘I", "Inspect cell"),
                ("⌘⇧F", "Filter rows…"),
            ],
        ),
        (
            "Schema tree",
            vec![
                ("↑ / ↓", "Move selection"),
                ("← / →", "Collapse / expand"),
                ("↵", "Open table preview"),
                ("⌘F", "Search schema (focus filter)"),
                ("⌘R", "Refresh schema"),
            ],
        ),
        (
            "Dialogs",
            vec![
                ("↵", "Confirm / connect"),
                ("Esc", "Cancel / close"),
                ("Tab / ⇧Tab", "Cycle controls (trapped)"),
            ],
        ),
        (
            "Welcome screen",
            vec![
                ("↑ / ↓", "Move between saved connections"),
                ("↵", "Connect to the highlighted one"),
                ("E", "Edit the highlighted connection"),
                ("⌫", "Remove the highlighted connection"),
                ("⌘N", "New connection"),
            ],
        ),
    ]
}

/// One default binding: `(keystroke, action name, context)`. The action name is
/// the same short name a user writes in `keymap.toml`, so this table doubles as
/// the list of bindable actions — every name here is one [`bind_named`] resolves.
/// `None` context = a true global; `Some("RedRoot")` = app chrome (see the module
/// doc for why each lives where it does).
type Entry = (&'static str, &'static str, Option<&'static str>);

/// RED's built-in keybindings — the source of truth a user's `keymap.toml`
/// overlays. Grouped to mirror the keyboard reference; the per-binding rationale
/// (why a key is global vs. `RedRoot`-scoped) is in the comments.
const DEFAULTS: &[Entry] = &[
    // --- true globals (work from any phase) ---
    // ⌘K toggles the command palette; ⌘P the connection switcher; ⌃G opens "go to
    // row"; ⌘Q quits (we render a seamless titlebar with no native app menu, so
    // quit is ours).
    ("cmd-k", "ToggleCommandPalette", None),
    ("cmd-p", "SwitchConnection", None),
    ("ctrl-g", "GoToRow", None),
    ("cmd-q", "Quit", None),
    // --- RedRoot: app chrome, fires from any focus within the app ---
    // ⌘C copies the result grid's selection, scoped to `RedRoot` so a focused text
    // field or the SQL editor keeps its own ⌘C (their context sits deeper in the
    // focus path and wins); it only reaches here when neither is focused.
    ("cmd-c", "CopyResult", Some("RedRoot")),
    // ⌘I toggles the cell detail inspector; Esc closes it. `RedRoot`-scoped so the
    // editor / a field / a modal (deeper contexts) keep their own ⌘I / Esc — this
    // fires only from the grid, schema, or root, where Esc was otherwise unbound.
    ("cmd-i", "ToggleInspector", Some("RedRoot")),
    ("escape", "CloseInspector", Some("RedRoot")),
    // Result filter bar. ⌘⇧F to keep plain ⌘F as schema search.
    ("cmd-shift-f", "ToggleFilter", Some("RedRoot")),
    // Saved queries: ⇧⌘S saves the active query, ⇧⌘O opens the picker. Both reach
    // `RedRoot` from the editor (the `CodeEditor` context binds neither).
    ("cmd-shift-s", "SaveQuery", Some("RedRoot")),
    ("cmd-shift-o", "OpenSavedQueries", Some("RedRoot")),
    // EXPLAIN the active query. ⇧⌘E pairs with the Run idiom; the analyze variant
    // is palette / run-bar only (it executes the statement).
    ("cmd-shift-e", "Explain", Some("RedRoot")),
    // Tab management. `RedRoot` is an ancestor of the editor, so these still fire
    // while it's focused — none collide with the editor's keys (it binds plain
    // `tab`, not `ctrl-tab`).
    ("cmd-t", "NewTab", Some("RedRoot")),
    ("cmd-w", "CloseTab", Some("RedRoot")),
    ("ctrl-tab", "NextTab", Some("RedRoot")),
    ("ctrl-shift-tab", "PrevTab", Some("RedRoot")),
    // Schema sidebar + reload + filter. ⌘F reaches `RedRoot` from the editor (the
    // `CodeEditor` context doesn't bind it), so it always opens search.
    ("cmd-b", "ToggleSidebar", Some("RedRoot")),
    ("cmd-r", "RefreshSchema", Some("RedRoot")),
    ("cmd-f", "SearchSchema", Some("RedRoot")),
    // Pane focus: direct jumps (⌥⌘1/2/3) and a cycle (F6 / ⇧F6). ⌥⌘ avoids the
    // bare ⌘+digit keys (macOS / screenshot tools bind several of those).
    ("cmd-alt-1", "FocusSchema", Some("RedRoot")),
    ("cmd-alt-2", "FocusEditor", Some("RedRoot")),
    ("cmd-alt-3", "FocusGrid", Some("RedRoot")),
    ("f6", "CycleFocusNext", Some("RedRoot")),
    ("shift-f6", "CycleFocusPrev", Some("RedRoot")),
    // Discoverability. `⌘/` (not `?`) so typing `?` into the editor or a field
    // still inserts the character — a global `?` binding would swallow it.
    ("cmd-/", "ShowShortcuts", Some("RedRoot")),
    // ⌘↵ runs the active tab's query from any pane. The editor's deeper
    // `CodeEditor` context keeps its own ⌘↵ (so a focused editor runs through its
    // Run event); this covers every other focus — grid, schema, root — and tests
    // the connection while the form is open.
    ("cmd-enter", "RunQuery", Some("RedRoot")),
    // ⌘N opens a new-connection form on the welcome screen (no-op elsewhere).
    ("cmd-n", "NewConnection", Some("RedRoot")),
    // Settings. `⌘,` is the macOS-standard binding; the menu's RED → Settings…
    // item displays this accelerator by looking the action up here. About has no
    // shortcut — it's reachable only from the menu.
    ("cmd-,", "Settings", Some("RedRoot")),
];

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
    // Dev-only perf HUD toggle (⌥⌘P). Re-bound here so a keymap reload's clear
    // doesn't drop it; the action itself is declared in `main` under the feature.
    #[cfg(feature = "dev-stats")]
    cx.bind_keys([KeyBinding::new("cmd-alt-p", crate::ToggleDevStats, None)]);

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
    Palette::bind_keys(cx);
    Modal::bind_keys(cx);
    Switcher::bind_keys(cx);
    ComboBox::bind_keys(cx);
}

/// Build the default bindings from [`DEFAULTS`]. The names are known-good, so a
/// failure here is a programmer error in the table, not user input.
fn default_bindings() -> Vec<KeyBinding> {
    DEFAULTS
        .iter()
        .map(|(keystroke, name, context)| {
            bind_named(keystroke, name, *context).expect("DEFAULTS holds a valid binding")
        })
        .collect()
}

/// Compile a user's override blocks into bindings, pushing a warning for each
/// entry it has to skip (bad context, bad keystroke, unknown action) so one typo
/// never drops the rest — mirroring how `settings.toml` degrades per section.
fn user_bindings(blocks: &[KeymapBlock], warnings: &mut Vec<String>) -> Vec<KeyBinding> {
    let mut out = Vec::new();
    for block in blocks {
        let context = block.context.as_deref();
        // Validate the block's context once; a bad predicate skips the whole block
        // (every binding in it would fail identically).
        if let Some(c) = context {
            if let Err(e) = KeyBindingContextPredicate::parse(c) {
                warnings.push(format!(
                    "keymap.toml: bad context “{c}” ({e}) — skipping its bindings"
                ));
                continue;
            }
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
        return Err("empty keystroke — skipping".to_string());
    }
    for token in keystroke.split_whitespace() {
        Keystroke::parse(token)
            .map_err(|e| format!("can't parse keystroke “{keystroke}” ({e}) — skipping"))?;
    }
    bind_named(keystroke, action, context)
}

/// Resolve a short action name to a `KeyBinding`. The match is the bindable-action
/// allowlist: a name not here (and not an unbind word) is rejected. `KeyBinding::
/// new` panics on a bad keystroke/context, so callers binding user input must
/// validate those first (see [`make_binding`]); [`DEFAULTS`] is known-good.
fn bind_named(keystroke: &str, action: &str, context: Option<&str>) -> Result<KeyBinding, String> {
    macro_rules! kb {
        ($action:expr) => {
            KeyBinding::new(keystroke, $action, context)
        };
    }
    if UNBIND_NAMES.contains(&action) {
        return Ok(kb!(NoAction));
    }
    Ok(match action {
        "ToggleCommandPalette" => kb!(ToggleCommandPalette),
        "SwitchConnection" => kb!(SwitchConnection),
        "GoToRow" => kb!(GoToRow),
        "Quit" => kb!(Quit),
        "CopyResult" => kb!(CopyResult),
        "ToggleInspector" => kb!(ToggleInspector),
        "CloseInspector" => kb!(CloseInspector),
        "ToggleFilter" => kb!(ToggleFilter),
        "SaveQuery" => kb!(SaveQuery),
        "OpenSavedQueries" => kb!(OpenSavedQueries),
        "Explain" => kb!(Explain),
        "NewTab" => kb!(NewTab),
        "CloseTab" => kb!(CloseTab),
        "NextTab" => kb!(NextTab),
        "PrevTab" => kb!(PrevTab),
        "ToggleSidebar" => kb!(ToggleSidebar),
        "RefreshSchema" => kb!(RefreshSchema),
        "SearchSchema" => kb!(SearchSchema),
        "FocusSchema" => kb!(FocusSchema),
        "FocusEditor" => kb!(FocusEditor),
        "FocusGrid" => kb!(FocusGrid),
        "CycleFocusNext" => kb!(CycleFocusNext),
        "CycleFocusPrev" => kb!(CycleFocusPrev),
        "ShowShortcuts" => kb!(ShowShortcuts),
        "RunQuery" => kb!(RunQuery),
        "NewConnection" => kb!(NewConnection),
        "Settings" => kb!(Settings),
        other => return Err(format!("unknown action “{other}” — skipping")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every default the table holds resolves to a binding (catches a typo'd name
    /// or an invalid keystroke/context in `DEFAULTS` at test time, not on launch).
    #[test]
    fn all_defaults_resolve() {
        assert_eq!(default_bindings().len(), DEFAULTS.len());
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
            // (the same one `KeyBinding::new` would panic on — caught here instead).
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
}
