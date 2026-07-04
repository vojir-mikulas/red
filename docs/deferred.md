# Deferred work

Things deliberately left undone, with enough context to pick them up later.
Add an entry here instead of a silent `TODO` when a feature ships partially.

## Native macOS menu bar

The menu bar (`crates/red/src/menu.rs`, mounted in `main.rs` via `cx.set_menus`)
shipped with the bold app menu (About, Settings…, Services, Quit) plus
**Edit / View / Query / Tabs / Help**, every item wired to an action that already
existed. The pieces below were intentionally skipped to keep that change small
and low-risk.

### Multi-window / "New Window"

No **File** menu and no second window yet. This is the largest architectural
piece. Picking it up means:

- Extract the `cx.open_window(…)` block in `main.rs` into an `open_main_window(…)`
  helper, add a `NewWindow` (⌘N) action, and generalise the existing
  `on_window_closed` "last window quits" logic.
- Decide between a shared backend service and one `red_service` per window.
  Shared is the likely answer (windows are views onto the same sessions), and
  the same decision affects future multi-connection work, so settle both
  together.
- A **File** menu would then carry: New Window ⌘N · Open Connection… ⌘O ·
  Open Recent ▶ · Close Window ⇧⌘W. None of it exists yet.

Note: `NewConnection` (⌘N, welcome-screen "add a connection") already exists and
is unrelated; it opens the connection *form*, not a window. The deferred ⌘N is
the window one, so the two will need to be reconciled when multi-window lands.

### Open Recent + dynamic menus

The menu tree is a **static snapshot** built once at startup. There is no
`refresh_menus(cx)` helper and no `cx.set_menus` re-call on state changes, so:

- **Open Recent** (an MRU submenu of recent connections) is not present. It needs
  a persisted MRU list (slot into the `settings.toml` system, updated whenever a
  connection opens) plus a rebuilt `MenuItem::submenu` and a `set_menus` re-call
  when the list changes.
- Any `.checked()` menu state (e.g. a ✓ on "Toggle Sidebar" when the sidebar is
  shown) would likewise need the refresh helper.

`build_menus()` is already side-effect-free and cheap precisely so it can be
re-called once a `refresh_menus` helper exists.

**Settings…** is wired (⌘,) but opens the in-app settings *panel*
(`open_settings`) rather than `settings.toml` in an external editor; the panel
is the richer existing surface. `About RED` opens the same panel on its About
tab.

### `.app` bundle so the title reads "RED"

macOS forces the bold app-menu title to the process/bundle name. When RED runs
as a bare binary, the app menu reads as the lowercase binary name, not **RED**.
Fixing it means shipping a `.app` bundle with an `Info.plist`
(`CFBundleName = RED`, bundle id, icon), e.g. `cargo-bundle` config under
`[package.metadata.bundle]`. The menu works without it; only the title
cosmetics are affected.

### Edit menu: Undo / Redo

Flint's `TextInput` and `CodeEditor` have **no undo stack**, so Undo/Redo menu
items would be dead. Rather than shipping items that do nothing, they're left
out. They can be added once Flint grows undo support (paired with
`OsAction::Undo/Redo`).

### Edit menu: SQL editor clipboard (partial)

The Edit menu's Cut/Copy/Paste/Select All pair Flint's **`TextInput`** clipboard
actions with `OsAction`s, so they drive the connection-form text fields. Flint's
**`CodeEditor`** (the SQL editor) declares a *separate* set of clipboard actions,
so a single menu item can't reach both. The SQL editor's own ⌘C/⌘V keystrokes
still work, but the menu items don't target it. Unifying the two clipboard
action sets in Flint, so one menu item serves both surfaces, is the clean fix.
