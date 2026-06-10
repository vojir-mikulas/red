# RED — Engineering conventions

The canonical guide for working in this repository. `CLAUDE.md` defers here for
specifics. These conventions prioritise the project's core values: speed, low
memory, and clear behaviour on large result sets.

## Comments

Comments carry maintenance cost — they drift from the code and age. A comment
earns its place when it captures something the code itself cannot.

- **Explain the *why*, not the *what*.** Document a non-obvious rationale, a
  platform limit, an ordering constraint, or an invariant the reader must
  preserve. Let the code speak for itself where it already does.
- **Keep comments self-contained.** A comment should make sense on its own,
  without reference to milestones, tickets, or development history. Milestone and
  planning context lives in `docs/plans/`.
- **Module docs describe responsibility.** A `//!` header states what a module
  is for and the invariants it upholds.
- **`TODO`/`FIXME` carry a reference.** Tie an open item to an owner or an issue
  so it has a path to resolution.

Good examples already in the tree set the bar: the `f32` 2^24 layout ceiling in
`result.rs`, the `FLING_ROWS` rationale, and the single-`gpui`-revision contract
in `Cargo.toml`.

## File size & module layout

- **Keep files focused.** A soft ceiling of ~600–800 lines is a useful signal,
  but the real trigger for splitting is *mixed concerns* — a file juggling state,
  rendering, and protocol — not length alone. A long, single-concern file (such
  as a database driver) is fine.
- **Split by concern into folder modules.** When a file grows across several
  responsibilities, break it into `foo/mod.rs` plus focused submodules
  (`foo/buffer.rs`, `foo/render.rs`). Re-export from `mod.rs` to keep the public
  surface stable. Unit tests move with the unit they cover.
- **Respect crate boundaries.** `red-core` holds domain types with no UI or
  runtime knowledge; `red-driver` is the only seam to database engines;
  `red-service` owns the backend thread; `red` is the GPUI application. The UI
  speaks `red-core` types and `Command`/`Event` — never driver internals.

## Errors & logging

- **`thiserror` in libraries** (`red_core::RedError` and peers); **`anyhow` at
  the application edge** (the `red` binary).
- **`tracing` for all diagnostics** — no `println!`/`eprintln!` in shipped paths.

## UI

- **Build the UI with Flint.** Components and theme tokens come from `flint`. Use
  semantic theme tokens (`cx.theme().bg_app`) rather than raw colors. New shared
  components are built gallery-first in Flint with a domain-free API (see the
  Flint workflow note in `CLAUDE.md`).
- **Resolve a single `gpui`,** pinned to the same revision as Flint and Nyx. This
  is a cross-repository contract — coordinate any bump across all three.

## Behaviour defaults

- **Safe by default.** Confirm destructive queries; support read-only connections
  and query timeouts.
- **Stream large results.** Never materialize a whole result set by default —
  results flow through a windowed cursor behind `DatabaseDriver`.

## Before committing

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # warnings are errors
cargo test
```
