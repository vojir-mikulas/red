//! Connection credentials live in the OS keychain, never in `connections.toml`.
//!
//! SECURITY: secrets handled here are *never* logged and *never* written to the
//! config file — they live only in the platform secret store (Keychain on macOS,
//! Credential Manager on Windows, Secret Service on Linux), behind the [`keyring`]
//! crate. The config file persists only a stable connection [`id`](super::config),
//! which is the keychain *account*; the password is fetched by id on demand.
//!
//! These calls are sync and blocking, and on macOS each keychain *item* read can
//! pop a system "allow" dialog. To keep that to **one prompt per connection per
//! app run** (rather than one per connect — painful with reconnect-on-switch), a
//! process-wide in-memory [`CACHE`] answers repeat reads: the keychain is hit on
//! the first read of an id, and every later read of the same id is served from
//! memory. RED accepts UI-thread reads here because the keychain is touched at
//! most once per id and is sub-millisecond once granted.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use zeroize::Zeroizing;

/// The keychain service all RED credentials are filed under.
const SERVICE: &str = "red";

/// In-memory password cache, keyed by connection id. Populated on the first
/// successful keychain read and kept for the rest of the process lifetime so
/// repeat reads (e.g. reconnect-on-switch) don't re-trigger the OS access prompt.
///
/// SECURITY: the cached value is [`Zeroizing`], so its plaintext is wiped from
/// memory when the entry is overwritten, removed, or the map is torn down. This
/// is no broader an exposure than `ConnectionConfig.password`, which already
/// holds the same plaintext in memory for every live session.
static CACHE: LazyLock<Mutex<HashMap<String, Zeroizing<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Build a keychain entry for a connection's password, keyed by its stable id.
fn entry(id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, id).context("open keychain entry")
}

/// Fetch a connection's stored password, or `None` if the keychain has no entry
/// for it (which simply means "ask the user"). Served from [`CACHE`] when present
/// so only the first read per id touches the OS keychain.
pub fn get_password(id: &str) -> Result<Option<String>> {
    if let Some(cached) = CACHE.lock().unwrap().get(id) {
        return Ok(Some(cached.to_string()));
    }
    match entry(id)?.get_password() {
        Ok(password) => {
            CACHE
                .lock()
                .unwrap()
                .insert(id.to_string(), Zeroizing::new(password.clone()));
            Ok(Some(password))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context("read keychain password")),
    }
}

/// Store (or replace) a connection's password in the keychain and refresh the
/// cache, so a later [`get_password`] serves the new value without a prompt.
pub fn set_password(id: &str, password: &str) -> Result<()> {
    entry(id)?
        .set_password(password)
        .context("write keychain password")?;
    CACHE
        .lock()
        .unwrap()
        .insert(id.to_string(), Zeroizing::new(password.to_string()));
    Ok(())
}

/// Remove a connection's password from the keychain and drop any cached copy
/// (zeroizing it). Idempotent: a missing entry is success, so this is safe to
/// call on delete regardless of whether a password was ever stored.
pub fn delete_password(id: &str) -> Result<()> {
    CACHE.lock().unwrap().remove(id);
    match entry(id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context("delete keychain password")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the real OS keychain — ignored by default (CI has none, and on
    /// macOS it pops a system dialog). Run locally with `cargo test -- --ignored`.
    #[test]
    #[ignore = "touches the real OS keychain"]
    fn round_trip() {
        let id = format!("red-test-{}", std::process::id());
        assert!(get_password(&id).unwrap().is_none());

        set_password(&id, "hunter2").unwrap();
        assert_eq!(get_password(&id).unwrap().as_deref(), Some("hunter2"));

        // Overwrite replaces, doesn't append.
        set_password(&id, "swordfish").unwrap();
        assert_eq!(get_password(&id).unwrap().as_deref(), Some("swordfish"));

        delete_password(&id).unwrap();
        assert!(get_password(&id).unwrap().is_none());
        // Deleting an absent entry is a no-op success.
        delete_password(&id).unwrap();
    }
}
