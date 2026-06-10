//! Connection credentials live in the OS keychain, never in `connections.toml`.
//!
//! SECURITY: secrets handled here are *never* logged and *never* written to the
//! config file — they live only in the platform secret store (Keychain on macOS,
//! Credential Manager on Windows, Secret Service on Linux), behind the [`keyring`]
//! crate. The config file persists only a stable connection [`id`](super::config),
//! which is the keychain *account*; the password is fetched by id on demand.
//!
//! These calls are sync and blocking, and on macOS the *first* access pops a
//! system "allow" dialog. RED accepts UI-thread reads here because they happen
//! only on an explicit connect / edit and are sub-millisecond once granted.

use anyhow::{Context, Result};

/// The keychain service all RED credentials are filed under.
const SERVICE: &str = "red";

/// Build a keychain entry for a connection's password, keyed by its stable id.
fn entry(id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, id).context("open keychain entry")
}

/// Fetch a connection's stored password, or `None` if the keychain has no entry
/// for it (which simply means "ask the user").
pub fn get_password(id: &str) -> Result<Option<String>> {
    match entry(id)?.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context("read keychain password")),
    }
}

/// Store (or replace) a connection's password in the keychain.
pub fn set_password(id: &str, password: &str) -> Result<()> {
    entry(id)?
        .set_password(password)
        .context("write keychain password")
}

/// Remove a connection's password from the keychain. Idempotent: a missing entry
/// is success, so this is safe to call on delete regardless of whether a password
/// was ever stored.
pub fn delete_password(id: &str) -> Result<()> {
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
