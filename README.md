# RED — Roughly Enough Data

A fast, simple, native **database explorer** in pure Rust. Built on GPUI (via the
shared [Flint](https://github.com/vojir-mikulas/flint) component library), the
same way [Nyx](https://github.com/vojir-mikulas/nyx) is built — no web stack, GPU
-rendered UI, a Tokio backend talking to the UI over channels.

> **Roughly Enough Data.** Show enough to make decisions quickly, without the
> complexity and overhead of full IDE-style database tools.

Explore schemas · run SQL · browse large tables · export · inspect production
databases safely. v0.1 targets **SQLite** and **PostgreSQL**.

> ⚠️ Early scaffold. Today this opens a window and proves the architecture; the
> connection manager, schema explorer, SQL editor, and result grid are in
> progress.

## Architecture

Mirrors Nyx: a **GPUI main thread** renders the UI; a **Tokio backend thread**
(`red-service`) owns database sessions and the query lifecycle. They communicate
over channels — `Command` (UI → service) and `Event` (service → UI).

- `red` — the GPUI application binary
- `red-core` — shared domain types (no UI, no runtime)
- `red-driver` — the `DatabaseDriver` abstraction + the SQLite implementation
- `red-service` — the backend thread and the Command/Event bridge

UI components and theme come from **Flint** (`flint = { path = "../flint" }`),
which pins and re-exports GPUI so RED resolves a single shared `gpui`.

## Develop

```sh
cargo run -p red                                       # open the window
cargo test                                             # run the backend tests
cargo clippy --workspace --all-targets -- -D warnings  # lint (warnings = errors)
cargo fmt --all
```

> Builds need a full Xcode toolchain on macOS (Metal shader compile). If
> `xcode-select` points at the Command Line Tools, set
> `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer`.

## License

GPL-3.0-or-later — RED links GPUI, whose dependency tree includes
GPL-3.0-or-later crates. See [`NOTICE`](NOTICE).
