# Keyboard reference

RED is operable almost entirely from the keyboard. **Every discrete action lives
in the command palette (`⌘K`)** — the complete, searchable list, phase-aware so it
only offers what's actionable — and the common ones also have the direct bindings
below. Press `⌘/` in-app for the shortcut reference as an overlay.

(Continuous navigation — the grid cell cursor, the tree selection, modal `Esc`/
`Enter` — isn't in the palette: those are cursor movement and dialog responses,
not commands.)

Bindings are registered centrally in `crates/red/src/keymap.rs`. The `⌘/` overlay
is generated from `keymap::shortcuts()`; keep this doc in sync with it.

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
| `⌥⌘1` / `⌥⌘2` / `⌥⌘3` | Focus schema / editor / grid |
| `F6` / `⇧F6` | Cycle focus forward / back |
| `⌘B` | Toggle the schema sidebar |

## Query tabs

| Key | Action |
| --- | --- |
| `⌘T` | New tab |
| `⌘W` | Close tab (confirms if it holds unsaved work) |
| `⌃Tab` / `⌃⇧Tab` | Next / previous tab |
| `⌘↵` | Run the active tab's query (or selection) — works from any pane, not just the editor |
| `Esc` | Leave the editor for the result grid (when no completion is open) |

The history popover (toggle from the palette or the History button) is keyboard
driven once open: `↑`/`↓` move, `↵` loads the entry, `Esc` closes.

## Result grid

Focus the grid (`⌥⌘3`, or click a cell), then drive the cell cursor:

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

Focus the sidebar (`⌥⌘1`), then:

| Key | Action |
| --- | --- |
| `↑` / `↓` | Move the selection |
| `←` / `→` | Collapse / expand (or jump to parent / first child) |
| `↵` | Open a table/view preview, or toggle a namespace |
| `⌘F` | Search the schema — reveals the sidebar and focuses its filter field |
| `⌘R` | Refresh the schema |

`⌘F` works from anywhere (it reveals a collapsed sidebar first); type to filter the
tree live, then `↓` into the results.

## Dialogs

| Key | Action |
| --- | --- |
| `↵` | Confirm (run the destructive statement, close the tab, or connect) |
| `Esc` | Cancel / close the dialog or overlay |
| `Tab` / `⇧Tab` | Cycle the dialog's controls (focus is trapped inside) |

Confirmation dialogs and the shortcuts overlay handle these keys through Flint's
`Modal`; `Tab` stays within the dialog and never escapes to the backdrop. In the
connection form, `↵` in any field connects, `⌘↵` runs **Test connection**, `Tab`
moves between fields, and `Esc` closes the form (which auto-focuses the name field
on open).

## Welcome screen

| Key | Action |
| --- | --- |
| `↑` / `↓` | Move between saved-connection cards |
| `↵` | Connect to the highlighted card |
| `⌘N` | New connection |

## Implementation notes

The generic, domain-free keyboard navigation now lives in Flint, per the
gallery-first rule: `Table` and `Tree` own a `FocusHandle` and emit
`TableNav`/`TreeNav` move-selection intents (RED keeps the selection state and
windowing); `Modal` owns `Esc`/`on_confirm` plus `Tab`/`⇧Tab` cycling with a
caller-supplied focus handle; `CodeEditor` emits an `Escape` event;
`Button`/`IconButton` take a `tooltip`.

Modals **trap focus**: `Tab` cycles within the dialog and a focus-out listener
pulls focus back if it would reach the backdrop. `Esc`/`Enter`/scrim-dismiss all
work too.
