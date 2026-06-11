# Keyboard reference

RED is operable almost entirely from the keyboard. Every action also lives in the
command palette (`⌘K`) — the floor of discoverability — and the common ones have
the direct bindings below. Press `⌘/` in-app for the same list as an overlay.

Bindings are registered centrally in `crates/red/src/keymap.rs`; the shortcuts
overlay and this doc are both built from `keymap::shortcuts()` so they don't drift.

> Bindings use `⌘` on macOS. The app is macOS-first today; per-platform `ctrl-*`
> splitting is a follow-up.

## Global

| Key | Action |
| --- | --- |
| `⌘K` | Command palette |
| `⌘/` | Keyboard-shortcuts overlay |
| `⌘,` | Settings |
| `⌘Q` | Quit |

## Panes

| Key | Action |
| --- | --- |
| `⌘1` / `⌘2` / `⌘3` | Focus schema / editor / grid |
| `F6` / `⇧F6` | Cycle focus forward / back |
| `⌘B` | Toggle the schema sidebar |

## Query tabs

| Key | Action |
| --- | --- |
| `⌘T` | New tab |
| `⌘W` | Close tab (confirms if it holds unsaved work) |
| `⌃Tab` / `⌃⇧Tab` | Next / previous tab |
| `⌘↵` | Run the query (or the selection) |

## Result grid

Focus the grid (`⌘3`, or click a cell), then drive the cell cursor:

| Key | Action |
| --- | --- |
| `↑ ↓ ← →` | Move the cell cursor |
| `⇧` + arrows | Extend the selection |
| `⌘←` / `⌘→` | Row start / end |
| `⌘↑` / `⌘↓` | First / last row |
| `PgUp` / `PgDn` | Page up / down |
| `⌃G` | Go to row… |
| `⌘C` | Copy the selection (TSV) |

The cursor lives in absolute row ordinals while the grid is windowed over a
multi-million-row result; moving off the visible window re-centers it through the
same paging machinery the scrollbar uses, so it follows without stutter.

## Schema tree

Focus the sidebar (`⌘1`), then:

| Key | Action |
| --- | --- |
| `↑` / `↓` | Move the selection |
| `←` / `→` | Collapse / expand (or jump to parent / first child) |
| `↵` | Open a table/view preview, or toggle a namespace |
| `⌘R` | Refresh the schema |

## Dialogs

| Key | Action |
| --- | --- |
| `↵` | Confirm (run the destructive statement, close the tab, or connect) |
| `Esc` | Cancel / close the dialog or overlay |

In the connection form, `↵` in any field connects and `Esc` closes the form.

## Deferred

These are noted in `docs/plans/keyboard-operability.md` and not yet wired:

- Editor `Esc` → jump to the grid (needs Flint `CodeEditor` to surface a
  no-completion escape).
- `⌘↵` = Test in the connection form (needs Flint `TextInput` to report the
  modifier on submit, or a Flint `Modal` `on_confirm`).
- Disconnected-screen card navigation (`↑/↓`, `↵`, `⌘N`).
- History-popover arrow navigation; the `ToggleHistory` direct binding.
- Pane focus rings, chrome tooltips, and pushing the generic grid/tree/modal
  keyboard nav down into Flint (it's spiked in RED today).
