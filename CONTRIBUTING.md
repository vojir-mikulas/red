# Contributing to RED

Thanks for your interest in RED — *Roughly Enough Data*, a fast, native database
explorer in pure Rust on GPUI. This guide gets you from a clone to a green build
to a reviewable pull request.

## Project values

RED prioritises **speed, low memory, and good behaviour on large result sets**
over feature-completeness. Changes are weighed against those values first. The
canonical coding guide is [`docs/conventions.md`](docs/conventions.md) — read it
before your first change.

## Prerequisites

- **Rust** ≥ 1.96 (the workspace `rust-version`). Install via [rustup](https://rustup.rs).
- **macOS:** a full **Xcode** toolchain — GPUI compiles Metal shaders, which the
  Command Line Tools alone can't do. If `xcode-select -p` points at
  `CommandLineTools`, set:

  ```sh
  export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
  ```

- **[`just`](https://github.com/casey/just)** (optional but recommended):
  `brew install just`. It wraps the common tasks — run `just` to list them.

## Build & run

A fresh clone builds with no extra setup — Flint (the shared UI library) is a
pinned **git dependency**, so Cargo fetches it for you:

```sh
cargo run -p red        # or: just run
```

## The pre-push gate

Every change must be green under format, lint, and tests before it's pushed:

```sh
just check              # = fmt + lint + test
```

or the raw commands:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # warnings are errors
cargo test --workspace
```

CI runs the same three checks on every pull request; a PR can't merge until they
pass.

## Working on Flint and RED together

Flint is pinned by git rev in [`Cargo.toml`](Cargo.toml) so outside contributors
build without a sibling checkout. When you need to change a Flint component *and*
RED at the same time, check Flint out next to RED and point Cargo at your local
copy with a patch in the **workspace** `Cargo.toml` (do not commit this):

```toml
[patch."https://github.com/vojir-mikulas/flint"]
flint = { path = "../flint" }
```

New shared components are built **gallery-first in Flint** with a generic,
domain-free API — spike the usage in RED first so the API is right, then push it
down into Flint (`just gallery` runs Flint's component gallery). See the Flint
workflow note in [`CLAUDE.md`](CLAUDE.md).

### The single-`gpui` contract

RED, Flint, and Nyx all pin **gpui to the same Zed revision**. Cargo unifies two
git deps only at a matching URL+rev, so a mismatch silently pulls in a second,
incompatible `gpui`. Bumping the rev — or bumping the Flint pin — is a
coordinated change across all three repos, never done casually in a feature PR.

## Architecture in brief

- **`red`** — the GPUI application binary; renders all UI on the main thread.
- **`red-core`** — shared domain types (`Value`, `QueryResult`, `RedError`, …)
  with no UI or runtime knowledge.
- **`red-driver`** — the `DatabaseDriver` trait and engine implementations
  (SQLite, PostgreSQL, MySQL). The *only* seam to a database engine.
- **`red-service`** — the Tokio backend thread and the `Command`/`Event` bridge.

The UI never blocks on the backend: it sends `Command`s and observes `Event`s
over channels. The UI speaks `red-core` types and Commands/Events — **never**
driver internals.

## Conventions worth calling out

These come up most in review (full list in [`docs/conventions.md`](docs/conventions.md)):

- **Comments explain *why*, not *what*** — and stay free of milestone/ticket
  references. Planning context lives in `docs/plans/`.
- **`thiserror` in libraries, `anyhow` at the app edge.** Use `tracing` for
  diagnostics — no `println!`/`eprintln!` in shipped paths.
- **Build the UI with Flint** and semantic theme tokens (`cx.theme().bg_app`),
  not raw colors.
- **Stream large results** — never materialize a whole result set by default;
  results flow through a windowed cursor behind `DatabaseDriver`.
- **Safe by default** — confirm destructive queries; honour read-only
  connections and query timeouts.

## Pull requests

1. Branch off `main`.
2. Keep the change focused; match the style of the code around it.
3. Run `just check` — it must be green.
4. Write a clear PR description: what changed and why. Conventional-commit-style
   subjects (`feat:`, `fix:`, `refactor:`, `chore:`) are appreciated.

## License

By contributing, you agree your contributions are licensed under
**GPL-3.0-or-later**, the project's license (see [`LICENSE`](LICENSE) and
[`NOTICE`](NOTICE)).
