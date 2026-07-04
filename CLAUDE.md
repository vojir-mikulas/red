# CLAUDE.md — RED

> RED — Roughly Enough Data. A fast, simple, native database explorer in pure
> Rust, built on GPUI. Priorities: speed, low memory, and excellent behaviour
> on large result sets over feature-completeness.

## Architecture in 4 lines

- **GPUI main thread** renders all UI; a **Tokio backend thread** (`red-service`)
  owns database sessions + the query lifecycle. They talk over channels
  (`Command` UI→service, `Event` service→UI). The UI never blocks on the backend.
- Crate map: `red` (app binary) · `red-core` (shared domain types, no UI/runtime)
  · `red-driver` (`DatabaseDriver` trait + engine impls) · `red-service` (backend
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

## Conventions

**`docs/conventions.md` is the canonical coding guide — read it.** It covers
comments, file size & module layout, errors/logging, the Flint and single-gpui
rules, and the safe-by-default behaviour. The load-bearing project invariants:

- **UI independent from drivers.** The UI speaks `red-core` types and Commands/
  Events, never driver internals. `DatabaseDriver` is the only seam to engines.
- **Single gpui**, pinned to the same Zed rev as Flint and Nyx — a cross-repo
  contract, never bumped casually.
- **Never materialize a whole result by default** — stream through a windowed
  cursor behind `DatabaseDriver`.

## Working with Flint

When RED needs a new component, build it gallery-first in Flint with a generic
(domain-free) API — but spike the usage in RED first so the API is right, then
push it down into Flint (see "Working on Flint and RED together" in
`CONTRIBUTING.md`). Open RED as the project root and add `../flint` as an
extra directory for that work.

## Plans

Feature plans and the roadmap live in `docs/plans/` (local-only, gitignored).
