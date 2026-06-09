# CLAUDE.md — RED

> RED — Roughly Enough Data. A fast, simple, native database explorer in pure
> Rust, built on GPUI the way Nyx is built. Priorities: speed, low memory, and
> excellent behaviour on large result sets over feature-completeness.

## Architecture in 4 lines

- **GPUI main thread** renders all UI; a **Tokio backend thread** (`red-service`)
  owns database sessions + the query lifecycle. They talk over channels
  (`Command` UI→service, `Event` service→UI). The UI never blocks on the backend.
- Crate map: `red` (app binary) · `red-core` (shared domain types, no UI/runtime)
  · `red-driver` (`DatabaseDriver` trait + SQLite impl) · `red-service` (backend
  thread + bridge).
- UI components + theme come from **Flint** (`../flint`), the shared in-house
  GPUI library (also used by Nyx).
- `red-core` holds types with no UI/runtime knowledge; the driver/service run on
  the Tokio thread; the UI observes events via `cx.spawn`.

## Commands

```sh
cargo run -p red                                       # open the window
cargo test                                             # backend tests
cargo clippy --workspace --all-targets -- -D warnings  # lint (warnings = errors)
cargo fmt --all
```

## Hard rules

- **Use Flint for UI.** Components + theme tokens come from `flint`. Don't add raw
  hex colors in app code — use a semantic token (`cx.theme().bg_app`). New shared
  components belong in Flint, not here (and must stay domain-free) — see the Flint
  workflow note below.
- **Single GPUI.** RED resolves one `gpui`, pinned to the same Zed rev as Flint
  and Nyx. Flint re-exports it (`flint::gpui`). Never bump gpui casually — it's a
  cross-repo contract (flint → nyx → red).
- **UI independent from drivers.** The UI speaks `red-core` types and Commands/
  Events, never driver internals. Keep `DatabaseDriver` the only seam to engines.
- **Read-only friendly / safe by default.** Confirm destructive queries, support
  read-only connections and query timeouts (see the plan).

## Conventions

- **Errors:** `thiserror` in libraries (`red_core::RedError`), `anyhow` at the app
  edge.
- **Logging:** `tracing`.
- **Large result sets:** never materialize a whole result by default — the goal is
  a streamed, windowed cursor behind `DatabaseDriver`. The scaffold's eager
  `QueryResult` is a placeholder to replace, not a pattern to spread.
- **Comments minimal** — explain a non-obvious *why* or an invariant, not the
  next line.

## Working with Flint

When RED needs a new component, build it **gallery-first in Flint** with a generic
(domain-free) API — but spike the usage in RED first so the API is right, then
push it down into Flint. Open RED as the project root and add `../flint` as an
extra directory for that work.

## Plans

The canonical RED roadmap and the Flint extraction history live in the Nyx repo:
`docs/plans/red-db-explorer.md` and `docs/plans/plan-02-nyx-ui-flint.md`.
