//! Connection-list persistence + OS-keychain access, extracted out of the `red`
//! binary so it carries no UI/runtime dependency and both frontends, the GPUI
//! app and the (planned) headless CLI, share **one** `connections.toml` and
//! **one** keychain. Nothing here knows about GPUI, Flint, or the database
//! driver; it speaks only `red-core` types plus file/keychain IO.
//!
//! - [`config`]: the saved-connection list (load/save/serialize, ids, paths).
//! - [`secrets`]: the OS keychain (passwords never touch `connections.toml`).
//! - [`history`]: the per-connection log of statements the user ran.
//! - [`queries`]: the saved-query library (`queries/*.sql`).
//! - [`recent_keys`]: the Redis browser's recently-viewed keys.
//!
//! The last three were UI state until the assistant needed them as *grounding*:
//! what a human actually ran against this database is the highest-signal context
//! a database agent can have, and it has to be reachable from the service thread,
//! not just from a panel.

pub mod config;
pub mod history;
pub mod queries;
pub mod recent_keys;
pub mod secrets;
