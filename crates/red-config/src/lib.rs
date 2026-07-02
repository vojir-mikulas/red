//! Connection-list persistence + OS-keychain access, extracted out of the `red`
//! binary so it carries no UI/runtime dependency and both frontends — the GPUI
//! app and the (planned) headless CLI — share **one** `connections.toml` and
//! **one** keychain. Nothing here knows about GPUI, Flint, or the database
//! driver; it speaks only `red-core` types plus file/keychain IO.
//!
//! - [`config`] — the saved-connection list (load/save/serialize, ids, paths).
//! - [`secrets`] — the OS keychain (passwords never touch `connections.toml`).

pub mod config;
pub mod secrets;
