# Changelog

All notable changes to RED are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are cut by tagging a version — `git tag v0.1.0 && git push origin v0.1.0`
— after which the entry below moves from _Unreleased_ to its version heading.

## [Unreleased]

The first MVP toward `0.1.0`: explore a schema, run SQL, browse large tables, and
export — fast, native, low-memory.

### Added
- Connection manager with SQLite, PostgreSQL, and MySQL/MariaDB drivers behind a
  single `DatabaseDriver` seam.
- Schema explorer, SQL editor, and a windowed result grid that streams large
  results through a keyset cursor without materializing them.
- CSV and JSON export, streamed row-by-row.
- Read-only connections and query timeouts; cancellable in-flight queries.
- Backend-thread panic isolation: a driver panic is logged and surfaced to the UI
  as an error rather than silently killing the service.
- Shared, engine-agnostic driver conformance battery exercised by every driver.

### Project
- Continuous integration (format, clippy, tests, and a `cargo-deny` supply-chain
  gate).
- Contributor docs: `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`.
- Flint pinned as a git dependency so a fresh clone builds without a sibling
  checkout.
