# Changelog

All notable changes to RED are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.14.0] - 2026-07-09

### Added
- Duplicate a saved connection from the welcome screen.
- Edit the connection you're currently using straight from the connection
  switcher.
- A scrollbar for the SQL editor.

### Changed
- A more compact welcome screen: smaller header, tighter spacing, and the
  import / bug-report links folded into one footer line.
- Importing connections from DBeaver or DBGate is now a wizard: pick the source
  and choose exactly which connections to bring in. Installs kept inside a
  Flatpak sandbox are found too.
- The selected connection on the welcome screen is outlined in its own colour.

### Fixed
- Windows: clicks on the toolbar controls (the Settings gear, the connection
  switcher) no longer get swallowed by window dragging. Dragging the window
  works as before.
- Autocomplete suggestions scroll now: arrow past the ones on screen, or use
  the mouse wheel to preview the rest.
- Wide values in the row-number column are no longer clipped.
- Right-click menus in the result grid close when you click anywhere outside
  them, instead of lingering over other windows.
- Square corners no longer show inside the settings panel's rounded frame.
- Linux: the app shows its icon in the GNOME app switcher and dock.
- Linux: the window no longer draws its own rounded corners.

## [0.13.0] - 2026-07-03

### Added
- Follow foreign keys from the grid: jump to the referenced row, list the rows
  that reference this one, or open a row as an expandable relation tree.
- Expand a foreign-key column in place to see the referenced table's columns
  alongside your result.
- Query history that persists across restarts, in its own panel next to the
  schema.
- Import data into a table from CSV or JSONL files.
- Column stats at a glance: count, distinct, nulls, min/max, sum/avg for the
  selected column.
- Split view: work in two query tabs side by side (⌘\).
- Copy a result or table into another table — in the same database or across
  connections.
- Migrate tables into a new database, with foreign keys, indexes, and
  auto-increment settings carried over.
- Import your saved connections from DBeaver or DBGate.
- A command-line mode for scripting: run queries, copy tables, and manage
  connections without opening the app.
- A What's New panel, and a heads-up toast after RED updates itself.
- Quick actions on the result grid.

## [0.12.0] - 2026-06-24

### Added
- Find in results and in the editor (⌘F).
- A small sample database on first launch, so you can try RED without setting
  anything up.
- Easier Claude sign-in for the AI assistant, with the signed-in account shown
  in Settings.

### Fixed
- Linux: the window now has a proper titlebar — move, resize, minimize, and
  close work on desktops that don't draw one themselves.
- Editing JSON and other typed cells now works reliably, and inline editing in
  the inspector is seamless.
- Cleaner, more readable notifications.
- Security fixes.

## [0.11.0] - 2026-06-23

### Changed
- Smarter SQL autocompletion.

## [0.10.2] - 2026-06-21

### Fixed
- Settings and keyboard shortcuts on Windows.

## [0.10.1] - 2026-06-21

### Fixed
- Windows and Linux downloads.

## [0.10.0] - 2026-06-21

### Added
- ClickHouse support (read-only).

## [0.9.0] - 2026-06-21

### Added
- RED now runs on Linux and Windows, and keeps itself up to date there too.

## [0.8.0] - 2026-06-21

### Added
- AI assistant (⌘L): chat about your schema and data using the Claude API or
  your Claude subscription. You approve every tool it uses, conversations are
  saved, and it can draw chart reports.
- Optionally let the assistant change data on a specific connection — every
  statement still needs your approval.
- Connecting through an unknown SSH host now shows its fingerprint and offers
  "Trust & retry".

### Fixed
- Read-only connections are enforced more strictly.

## [0.7.0] - 2026-06-19

### Added
- SSH tunneling: connect to databases behind a jump host, with password, key,
  or agent authentication.

## [0.6.0] - 2026-06-18

### Added
- Edit data right in the grid: change cells, add and delete rows, review the
  staged changes, then submit them together or revert.

## [0.5.5] - 2026-06-14

### Added
- Custom keyboard shortcuts (`keymap.toml`, applied live).

### Fixed
- Editing a saved connection no longer loses its password; SQL editor
  shortcuts on macOS.

## [0.5.4] - 2026-06-13

Maintenance release.

## [0.5.2] - 2026-06-13

### Fixed
- SQL editor fixes.

## [0.5.1] - 2026-06-13

### Fixed
- Stability fixes.

## [0.5.0] - 2026-06-13

### Added
- The settings panel is fully keyboard-accessible.

### Fixed
- Table and settings scrolling.

## [0.4.0] - 2026-06-13

### Changed
- A refreshed welcome screen and accessibility improvements.

## [0.3.0] - 2026-06-13

### Fixed
- Tab cycling and switching back to recent connections.

## [0.2.0] - 2026-06-13

### Added
- Edit a cell's value, with a confirmation before the change runs.
- RED updates itself on macOS.

## [0.1.3] - 2026-06-13

### Added
- Saved queries: keep snippets and reopen them from the palette.

## [0.1.1] - 2026-06-13

### Added
- Filter a result to a `WHERE` clause without rewriting the query (⌘⇧F).

## [0.1.0] - 2026-06-13

The first release: explore a schema, run SQL, browse large tables, and export.

### Added
- Connect to SQLite, PostgreSQL, and MySQL/MariaDB; passwords live in your
  system keychain, never in a plain file.
- Schema explorer, SQL editor with schema-aware completion, and query tabs.
- Browse huge tables smoothly — rows stream in as you scroll, so even
  million-row results stay fast and light on memory.
- Export results to CSV or JSON.
- Safety rails: read-only connections, query timeouts, cancellable queries,
  and a confirmation before destructive statements.
- Command palette (⌘K) and full keyboard operability.
- Cell/row detail inspector (⌘I).
- Keep several connections open and switch instantly (⌘P).
- Themes and font settings.
- Native macOS menu bar; signed and notarized macOS builds.
