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

use std::collections::BTreeMap;

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
        /// Open the GitHub issue tracker in the browser (Help → Report a Bug…).
        /// Menu-only, like `About` — no default shortcut, so it's absent from
        /// `DEFAULTS`/the keymap editor.
        ReportBug,
        /// Open the connection switcher popover (⌘P).
        SwitchConnection,
        /// Open or close the cell detail inspector (⌘I).
        ToggleInspector,
        /// Close the cell detail inspector (Esc) — a no-op when it's shut.
        CloseInspector,
        /// Open or close the AI assistant chat panel (⌘L).
        ToggleAssistant,
        /// Open or close the result filter bar (⌘⇧F) — Track B2.
        ToggleFilter,
        /// Save the active tab's query as a named snippet (⇧⌘S) — Track B3.
        SaveQuery,
        /// Open the saved-query picker (⇧⌘O) — Track B3.
        OpenSavedQueries,
        /// Explain the active tab's query — open the plan view (⇧⌘E) — Track B4.
        Explain,
        /// Begin editing the focused result cell in place (Enter / F2) — Track B6.
        BeginEdit,
        /// Submit the staged grid edits as one batch (⌘↵ in the grid) — Track B6.
        /// Falls back to running the query when nothing is staged.
        SubmitChanges,
        /// Discard the staged grid edits (⌘⌥Z) — Track B6.
        RevertChanges,
        /// Toggle deletion of the selected result row(s) (⌘⌫) — Track B6.
        DeleteRow,
        /// Append a new draft (insert) row to the result (⌘⌥N) — Track B6.
        AddRow,
        /// Set the focused result cell to NULL (⌘⌥0) — Track B6.
        SetNull,
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
            "Editing data",
            vec![
                ("↵ / F2", "Edit the focused cell"),
                ("⌘↵", "Submit staged changes"),
                ("⌥⌘Z", "Revert staged changes"),
                ("⌘⌫", "Mark row(s) for deletion"),
                ("⌥⌘N", "Add a new row"),
                ("⌥⌘0", "Set cell to NULL"),
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

/// One default binding and the metadata the keymap editor needs to present it.
/// `action` is the short name a user writes in `keymap.toml`, so this table
/// doubles as the bindable-action allowlist — every name here is one
/// [`bind_named`] resolves; `label` is the human title the editor shows.
/// `context = None` is a true global; `Some("RedRoot")` is app chrome (see the
/// module doc for why each lives where it does). An action with two default keys
/// (e.g. `BeginEdit`: Enter and F2) appears once per key — the editor lists each
/// as its own rebindable row.
pub(crate) struct ActionDef {
    /// The default keystroke, in `keymap.toml`'s canonical form (`cmd-shift-f`).
    pub keystroke: &'static str,
    /// The action name — the allowlist key and what `keymap.toml` writes.
    pub action: &'static str,
    /// The human label the editor shows for this row.
    pub label: &'static str,
    /// The key-context, or `None` for a true global.
    pub context: Option<&'static str>,
}

/// The full bindable-action registry — RED's built-in keybindings and the source
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
    def("ctrl-g", "GoToRow", "Go to row", None),
    def("cmd-q", "Quit", "Quit", None),
    // --- RedRoot: app chrome, fires from any focus within the app ---
    // ⌘C copies the result grid's selection, scoped to `RedRoot` so a focused text
    // field or the SQL editor keeps its own ⌘C (their context sits deeper in the
    // focus path and wins); it only reaches here when neither is focused.
    def("cmd-c", "CopyResult", "Copy selection", Some("RedRoot")),
    // ⌘I toggles the cell detail inspector; Esc closes it. `RedRoot`-scoped so the
    // editor / a field / a modal (deeper contexts) keep their own ⌘I / Esc — this
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
    // Tab management. `RedRoot` is an ancestor of the editor, so these still fire
    // while it's focused — none collide with the editor's keys (it binds plain
    // `tab`, not `ctrl-tab`).
    def("cmd-t", "NewTab", "New tab", Some("RedRoot")),
    def("cmd-w", "CloseTab", "Close tab", Some("RedRoot")),
    def("ctrl-tab", "NextTab", "Next tab", Some("RedRoot")),
    def("ctrl-shift-tab", "PrevTab", "Previous tab", Some("RedRoot")),
    // Schema sidebar + reload + filter. ⌘F reaches `RedRoot` from the editor (the
    // `CodeEditor` context doesn't bind it), so it always opens search.
    def("cmd-b", "ToggleSidebar", "Toggle sidebar", Some("RedRoot")),
    def("cmd-r", "RefreshSchema", "Refresh schema", Some("RedRoot")),
    def("cmd-f", "SearchSchema", "Search schema", Some("RedRoot")),
    // Pane focus: direct jumps (⌥⌘1/2/3) and a cycle (F6 / ⇧F6). ⌥⌘ avoids the
    // bare ⌘+digit keys (macOS / screenshot tools bind several of those).
    def(
        "cmd-alt-1",
        "FocusSchema",
        "Focus schema sidebar",
        Some("RedRoot"),
    ),
    def("cmd-alt-2", "FocusEditor", "Focus editor", Some("RedRoot")),
    def(
        "cmd-alt-3",
        "FocusGrid",
        "Focus result grid",
        Some("RedRoot"),
    ),
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
    // still inserts the character — a global `?` binding would swallow it.
    def(
        "cmd-/",
        "ShowShortcuts",
        "Keyboard shortcuts",
        Some("RedRoot"),
    ),
    // ⌘↵ runs the active tab's query from any pane. The editor's deeper
    // `CodeEditor` context keeps its own ⌘↵ (so a focused editor runs through its
    // Run event); this covers every other focus — grid, schema, root — and tests
    // the connection while the form is open.
    def("cmd-enter", "RunQuery", "Run query", Some("RedRoot")),
    // ⌘N opens a new-connection form on the welcome screen (no-op elsewhere).
    def("cmd-n", "NewConnection", "New connection", Some("RedRoot")),
    // Settings. `⌘,` is the macOS-standard binding; the menu's RED → Settings…
    // item displays this accelerator by looking the action up here. About has no
    // shortcut — it's reachable only from the menu.
    def("cmd-,", "Settings", "Settings", Some("RedRoot")),
    // --- staged grid editing (Track B6) ---
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
    def("cmd-alt-n", "AddRow", "Add row", Some("Table")),
    def("cmd-alt-0", "SetNull", "Set cell to NULL", Some("Table")),
];

/// A `const fn` shorthand so [`DEFAULTS`] reads as a compact table rather than a
/// wall of struct-literal field names — every row is one `def(key, action, label,
/// context)`.
const fn def(
    keystroke: &'static str,
    action: &'static str,
    label: &'static str,
    context: Option<&'static str>,
) -> ActionDef {
    ActionDef {
        keystroke,
        action,
        label,
        context,
    }
}

/// The bindable-action registry — every default binding, for the keymap editor to
/// list and rebind. One row per default keystroke (an action with two default
/// keys appears twice).
pub(crate) fn action_defs() -> &'static [ActionDef] {
    DEFAULTS
}

/// The editor's per-row "effective keystroke" model: a slot for each [`ActionDef`]
/// (same length and order as [`action_defs`]), holding the keystroke that row is
/// currently bound to — `Some(k)` bound, `None` unbound. The pure bridge between
/// the per-keystroke `keymap.toml` and the per-action editor; [`effective_slots`]
/// reads it, [`diff_blocks`] writes it back.
pub(crate) type Slots = Vec<Option<String>>;

/// Build the effective per-row keystrokes by overlaying a user's override blocks
/// on the defaults — the editor's read model. Each row starts at its default
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
/// override blocks — the inverse of [`effective_slots`]. A row still on its
/// default emits nothing; a moved row emits its new `keystroke = action`; and a
/// default keystroke that no row occupies any more is emitted as `"unbind"` so the
/// freed default stops firing its old action. This minimality is what keeps a
/// GUI-written file small and interchangeable with a hand-edited one.
pub(crate) fn diff_blocks(slots: &Slots) -> Vec<KeymapBlock> {
    // Every keystroke a row currently occupies, per context — used to decide
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
            // On its default — nothing to emit.
            Some(k) if k == d.keystroke => {}
            // Moved to a new key — bind it; the freed default is handled below.
            Some(k) => {
                by_ctx
                    .entry(ctx_owned)
                    .or_default()
                    .insert(k.to_string(), d.action.to_string());
            }
            // Unbound — nothing positive to emit; the freed default is handled below.
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

/// The row, if any, that already binds `keystroke` in the same context as `row` —
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
        .map(|d| {
            bind_named(d.keystroke, d.action, d.context).expect("DEFAULTS holds a valid binding")
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
        "ToggleAssistant" => kb!(ToggleAssistant),
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
        "BeginEdit" => kb!(BeginEdit),
        "SubmitChanges" => kb!(SubmitChanges),
        "RevertChanges" => kb!(RevertChanges),
        "DeleteRow" => kb!(DeleteRow),
        "AddRow" => kb!(AddRow),
        "SetNull" => kb!(SetNull),
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
        // No stray unbinds — both defaults are still occupied (by the other action).
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
