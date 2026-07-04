# Red — Roughly Enough Data

A fast, native database explorer built in Rust.

Red helps you inspect schemas, browse large tables, run SQL queries, and export data without the complexity of heavyweight IDE-style database tools.

> **Roughly Enough Data.** Show enough to make decisions quickly.

Built with GPUI and rendered natively on the GPU. No Electron, no browser runtime, no web stack.

> **Status: MVP.** Core workflows are functional, but APIs and UI are still evolving. Expect rough edges and breaking changes before the first stable release.

## Databases

* SQLite
* PostgreSQL
* MySQL / MariaDB
* ClickHouse *(read-only)*

## Features

* Schema explorer
* SQL editor with autocompletion
* Large-table browsing through a windowed, keyset-paginated result grid
* Find in results and in the editor
* Filter results to a `WHERE` clause without rewriting the query
* Cell / row detail inspector
* Inline data editing with staged, reviewable batch changes
* Saved queries
* Data export
* Multiple connections with a quick connection switcher
* AI assistant sidebar with grounded chat over your schema, via the Claude API or a Claude subscription
* SSH tunneling through a jump host
* Themes (Ayu Dark / Light, High Contrast) and a fully customizable keymap
* Production-safe inspection workflows

## Install

Prebuilt, signed binaries are on the [latest release](https://github.com/vojir-mikulas/red/releases/latest):

* **macOS**: download the `.dmg` (signed and notarized).
* **Linux**: download the `.AppImage`, `chmod +x` it, and run.
* **Windows**: download the `.exe`, or the `.zip` if you prefer to unpack it.

Or build from source; see [Development](#development).

On first launch Red seeds a small, read-only **Sample database** so you can explore the schema browser, run queries, and try the result grid immediately, with no database setup required.

## Privacy

Red has no telemetry and makes no network calls of its own. Connection credentials are stored in your operating system's keychain, never in plaintext. The AI assistant is opt-in and only talks to the provider you configure.

## Architecture

A GPUI main thread renders the interface while a Tokio backend service owns database sessions and query execution. The UI and backend communicate through a Command/Event channel bridge, so the interface never blocks on the database.

Workspace crates:

* `red`: desktop application
* `red-core`: shared domain types
* `red-driver`: database driver abstractions and implementations
* `red-service`: backend runtime and query lifecycle
* `red-ai` / `red-acp`: AI assistant providers (direct API and agent-client-protocol)

UI components and theming come from Flint, a shared component library built on GPUI.

## Development

```sh
cargo run -p red
cargo test
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

> On macOS, building requires a full Xcode installation for Metal shader compilation. If `xcode-select` points to the Command Line Tools, set:
>
> `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer`

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for development setup and contribution guidelines.

## License

GPL-3.0-or-later.

Red links against GPUI, whose dependency tree includes GPL-licensed crates. See [`NOTICE`](NOTICE) for details.
