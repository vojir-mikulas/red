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

/// Build a keychain entry for a secret, keyed by its `account` string. A
/// connection's DB password uses the bare connection id; its SSH secrets use
/// suffixed accounts (see [`ssh_password_account`]/[`ssh_passphrase_account`]),
/// so one connection can hold several distinct credentials.
fn entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, account).context("open keychain entry")
}

/// Keychain account for a connection's SSH password.
fn ssh_password_account(id: &str) -> String {
    format!("{id}#ssh-pw")
}

/// Keychain account for a connection's SSH private-key passphrase.
fn ssh_passphrase_account(id: &str) -> String {
    format!("{id}#ssh-key")
}

/// Read a secret by keychain account, or `None` when there's no entry (which
/// simply means "ask the user"). Served from [`CACHE`] when present so only the
/// first read per account touches the OS keychain.
fn read(account: &str) -> Result<Option<String>> {
    if let Some(cached) = CACHE.lock().unwrap().get(account) {
        return Ok(Some(cached.to_string()));
    }
    match entry(account)?.get_password() {
        Ok(secret) => {
            CACHE
                .lock()
                .unwrap()
                .insert(account.to_string(), Zeroizing::new(secret.clone()));
            Ok(Some(secret))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context("read keychain secret")),
    }
}

/// Store (or replace) a secret by keychain account and refresh the cache, so a
/// later [`read`] serves the new value without an OS prompt.
fn write(account: &str, secret: &str) -> Result<()> {
    entry(account)?
        .set_password(secret)
        .context("write keychain secret")?;
    CACHE
        .lock()
        .unwrap()
        .insert(account.to_string(), Zeroizing::new(secret.to_string()));
    Ok(())
}

/// Remove a secret by keychain account and drop any cached copy (zeroizing it).
/// Idempotent: a missing entry is success.
fn remove(account: &str) -> Result<()> {
    CACHE.lock().unwrap().remove(account);
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context("delete keychain secret")),
    }
}

/// Fetch a connection's stored DB password, or `None` if the keychain has no
/// entry for it.
pub fn get_password(id: &str) -> Result<Option<String>> {
    read(id)
}

/// Store (or replace) a connection's DB password.
pub fn set_password(id: &str, password: &str) -> Result<()> {
    write(id, password)
}

/// Remove a connection's DB password. Idempotent; safe to call on delete whether
/// or not a password was ever stored. Prefer [`delete_all`] on connection delete.
pub fn delete_password(id: &str) -> Result<()> {
    remove(id)
}

/// Fetch a connection's stored SSH password (password-auth mode), or `None`.
pub fn get_ssh_password(id: &str) -> Result<Option<String>> {
    read(&ssh_password_account(id))
}

/// Store (or replace) a connection's SSH password.
pub fn set_ssh_password(id: &str, secret: &str) -> Result<()> {
    write(&ssh_password_account(id), secret)
}

/// Remove a connection's SSH password. Idempotent.
pub fn delete_ssh_password(id: &str) -> Result<()> {
    remove(&ssh_password_account(id))
}

/// Fetch a connection's stored SSH key passphrase, or `None`.
pub fn get_ssh_passphrase(id: &str) -> Result<Option<String>> {
    read(&ssh_passphrase_account(id))
}

/// Store (or replace) a connection's SSH key passphrase.
pub fn set_ssh_passphrase(id: &str, secret: &str) -> Result<()> {
    write(&ssh_passphrase_account(id), secret)
}

/// Remove a connection's SSH key passphrase. Idempotent.
pub fn delete_ssh_passphrase(id: &str) -> Result<()> {
    remove(&ssh_passphrase_account(id))
}

/// Keychain account for the AI assistant provider's API key. App-global (not
/// per-connection), namespaced by provider so multiple providers can coexist.
fn ai_key_account(provider: &str) -> String {
    format!("ai-key:{provider}")
}

/// Fetch the AI provider's API key, or `None` if unset. The assistant stays off
/// until this is present.
pub fn get_ai_key(provider: &str) -> Result<Option<String>> {
    read(&ai_key_account(provider))
}

/// Store (or replace) the AI provider's API key. Never written to `settings.toml`
/// — it lives only in the OS keychain, like connection passwords.
pub fn set_ai_key(provider: &str, key: &str) -> Result<()> {
    write(&ai_key_account(provider), key)
}

/// Remove the AI provider's API key. Idempotent. The symmetric remove of the
/// set/get/delete trio, kept for the settings "remove key" path.
#[allow(dead_code)]
pub fn delete_ai_key(provider: &str) -> Result<()> {
    remove(&ai_key_account(provider))
}

/// Remove every secret filed under a connection id — DB password plus both SSH
/// secrets — so deleting a connection never orphans a credential. Idempotent.
pub fn delete_all(id: &str) -> Result<()> {
    remove(id)?;
    remove(&ssh_password_account(id))?;
    remove(&ssh_passphrase_account(id))?;
    Ok(())
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
