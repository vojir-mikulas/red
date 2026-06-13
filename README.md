# RED — Roughly Enough Data

A fast, native database explorer built in Rust.

RED helps you inspect schemas, browse large tables, run SQL queries, and export data without the complexity of heavyweight IDE-style database tools.

> **Roughly Enough Data.** Show enough to make decisions quickly.

Supports:

* SQLite
* PostgreSQL
* MySQL / MariaDB

Built with GPUI and rendered natively on the GPU — no Electron, no browser runtime, no web stack.

> **Status: MVP.** Core workflows are functional, but APIs and UI are still evolving. Expect rough edges and breaking changes before the first stable release.

## Features

* Schema explorer
* SQL editor
* Large-table browsing
* Windowed result grid
* Data export
* Production-safe inspection workflows

## Architecture

RED follows the same architecture as Nyx.

A GPUI main thread renders the interface while a Tokio backend service owns database sessions and query execution. The UI and backend communicate through a Command/Event channel bridge.

Workspace crates:

* `red` — desktop application
* `red-core` — shared domain types
* `red-driver` — database driver abstractions and implementations
* `red-service` — backend runtime and query lifecycle

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

RED links against GPUI, whose dependency tree includes GPL-licensed crates. See [`NOTICE`](NOTICE) for details.
