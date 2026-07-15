//! "Remove all RED data": one idempotent teardown that wipes everything RED wrote
//! to the machine — its config/data directories **and** its OS-keychain secrets.
//!
//! The keychain is the whole reason this exists. Dragging the app to the trash
//! leaves the per-connection passwords, SSH secrets, and AI keys behind, invisible
//! and unenumerable (the `keyring` crate has no "list a service's accounts" API on
//! any backend). So a reset can't "delete every RED item" directly; it
//! reconstructs the account set from what RED knows — every saved connection id
//! and every configured AI provider — deletes those, then removes the two dirs.
//!
//! Order is load-bearing: **secrets first, directories last**. A failure partway
//! through then never leaves `connections.toml` gone while its keychain entries
//! survive orphaned. Every step is best-effort and records into
//! [`ResetReport::errors`] rather than aborting, so one locked keychain item or a
//! busy file can't strand the rest of the teardown.

use std::path::PathBuf;

/// What a reset removed (and what it couldn't), for the summary shown to the user
/// and the CLI's exit code. Counts are of *attempts that succeeded*; a per-step
/// failure lands in [`errors`](Self::errors) instead of a count.
#[derive(Debug, Default, Clone)]
pub(crate) struct ResetReport {
    /// Connection secret-sets removed (DB password + both SSH secrets per id).
    pub connections_cleared: usize,
    /// AI provider API keys removed.
    pub ai_keys_cleared: usize,
    /// Whether `<config_dir>/red` was removed (false if it didn't exist or failed).
    pub config_dir_removed: bool,
    /// Whether `<data_dir>/red` was removed.
    pub data_dir_removed: bool,
    /// Non-fatal, per-step failures, each a human-readable line for the summary.
    pub errors: Vec<String>,
}

/// The keychain operations a reset needs, abstracted so the directory-removal and
/// reporting logic can be unit-tested without touching the real OS keychain (which
/// CI lacks and macOS gates behind a system prompt). The real implementation
/// ([`RealSecrets`]) forwards to [`red_config::secrets`].
pub(crate) trait SecretStore {
    /// Remove every secret filed under a connection id (DB + both SSH secrets).
    fn delete_connection(&self, id: &str) -> anyhow::Result<()>;
    /// Remove one AI provider's API key.
    fn delete_ai_key(&self, provider: &str) -> anyhow::Result<()>;
    /// Drop any in-memory plaintext cache after the keychain entries are gone.
    fn clear_cache(&self);
}

/// The production [`SecretStore`]: the real OS keychain via [`red_config::secrets`].
pub(crate) struct RealSecrets;

impl SecretStore for RealSecrets {
    fn delete_connection(&self, id: &str) -> anyhow::Result<()> {
        red_config::secrets::delete_all(id)
    }
    fn delete_ai_key(&self, provider: &str) -> anyhow::Result<()> {
        red_config::secrets::delete_ai_key(provider)
    }
    fn clear_cache(&self) {
        red_config::secrets::clear_cache();
    }
}

/// Remove everything RED wrote to this machine: keychain secrets first, then the
/// config and data directories. Best-effort throughout; see [`ResetReport`].
///
/// The connection ids come from [`red_config::config::load`] and the AI provider
/// list from the configured agents (see [`ai_providers`]), so the enumeration
/// tracks whatever the user actually has — a new provider can't be silently
/// missed. Runs blocking keychain calls, so callers on the UI thread spawn it off.
pub(crate) fn remove_all_data() -> ResetReport {
    let ids: Vec<String> = red_config::config::load()
        .into_iter()
        .map(|s| s.id)
        .collect();
    remove_all_data_with(
        &ids,
        &ai_providers(),
        dirs::config_dir().map(|d| d.join("red")),
        dirs::data_dir().map(|d| d.join("red")),
        &RealSecrets,
    )
}

/// The testable core: given the exact account set, the two directories, and a
/// [`SecretStore`], perform the teardown and return a [`ResetReport`]. Split from
/// [`remove_all_data`] so a test can inject temp dirs and a stub store.
pub(crate) fn remove_all_data_with(
    connection_ids: &[String],
    ai_providers: &[String],
    config_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    secrets: &dyn SecretStore,
) -> ResetReport {
    let mut report = ResetReport::default();

    // 1. Per-connection secrets (DB password + SSH password + SSH passphrase).
    for id in connection_ids {
        match secrets.delete_connection(id) {
            Ok(()) => report.connections_cleared += 1,
            Err(e) => report
                .errors
                .push(format!("keychain: connection {id}: {e}")),
        }
    }

    // 2. AI provider API keys.
    for provider in ai_providers {
        match secrets.delete_ai_key(provider) {
            Ok(()) => report.ai_keys_cleared += 1,
            Err(e) => report
                .errors
                .push(format!("keychain: AI key {provider}: {e}")),
        }
    }

    // 3. Forget any plaintext still cached in memory for the rest of the process.
    secrets.clear_cache();

    // 4. The directories, last — so a mid-run failure above never orphans a
    //    keychain entry whose connection record is already gone.
    report.config_dir_removed = remove_dir(config_dir, "config", &mut report.errors);
    report.data_dir_removed = remove_dir(data_dir, "data", &mut report.errors);

    report
}

/// Remove one directory tree, returning whether it's now gone. A missing directory
/// counts as removed (the goal state is "not there"); a real removal error is
/// recorded and returns `false`. `None` (platform couldn't resolve the base dir)
/// is a recorded error too, so a summary never silently claims a clean wipe.
fn remove_dir(dir: Option<PathBuf>, label: &str, errors: &mut Vec<String>) -> bool {
    let Some(dir) = dir else {
        errors.push(format!("could not resolve the {label} directory"));
        return false;
    };
    if !dir.exists() {
        return true;
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => true,
        Err(e) => {
            errors.push(format!("{label} directory {}: {e}", dir.display()));
            false
        }
    }
}

/// The AI providers to clear keys for: every configured agent's id (the keychain
/// account is `ai-key:<id>`), plus the built-in `anthropic`/`subscription` ids so
/// a key left by a default agent is caught even if the settings file was reset to
/// no explicit agents. Deleting an absent key is a no-op, so over-listing is safe.
fn ai_providers() -> Vec<String> {
    let settings = crate::settings::FileSettingsStore::open_default()
        .map(|s| s.load_report().settings)
        .unwrap_or_default();
    let mut providers: Vec<String> = settings
        .ai
        .resolved_agents()
        .into_iter()
        .map(|a| a.id)
        .collect();
    for builtin in ["anthropic", "subscription"] {
        if !providers.iter().any(|p| p == builtin) {
            providers.push(builtin.to_string());
        }
    }
    providers
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records which accounts were asked to be deleted, so a test can assert the
    /// enumeration without a real keychain. `fail_on` forces an error for one id to
    /// exercise the best-effort path.
    #[derive(Default)]
    struct StubSecrets {
        deleted_conns: RefCell<Vec<String>>,
        deleted_ai: RefCell<Vec<String>>,
        cache_cleared: RefCell<bool>,
        fail_on: Option<String>,
    }

    impl SecretStore for StubSecrets {
        fn delete_connection(&self, id: &str) -> anyhow::Result<()> {
            if self.fail_on.as_deref() == Some(id) {
                anyhow::bail!("locked");
            }
            self.deleted_conns.borrow_mut().push(id.to_string());
            Ok(())
        }
        fn delete_ai_key(&self, provider: &str) -> anyhow::Result<()> {
            self.deleted_ai.borrow_mut().push(provider.to_string());
            Ok(())
        }
        fn clear_cache(&self) {
            *self.cache_cleared.borrow_mut() = true;
        }
    }

    #[test]
    fn removes_dirs_and_enumerates_secrets() {
        let base = std::env::temp_dir().join(format!("red-reset-test-{}", std::process::id()));
        let config = base.join("config");
        let data = base.join("data");
        std::fs::create_dir_all(config.join("queries")).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(config.join("settings.toml"), b"x").unwrap();

        let stub = StubSecrets::default();
        let report = remove_all_data_with(
            &["conn-a".into(), "conn-b".into()],
            &["anthropic".into()],
            Some(config.clone()),
            Some(data.clone()),
            &stub,
        );

        assert_eq!(report.connections_cleared, 2);
        assert_eq!(report.ai_keys_cleared, 1);
        assert!(report.config_dir_removed && report.data_dir_removed);
        assert!(report.errors.is_empty());
        assert!(!config.exists() && !data.exists(), "dirs are gone");
        assert!(*stub.cache_cleared.borrow(), "in-memory cache was cleared");
        assert_eq!(*stub.deleted_ai.borrow(), vec!["anthropic".to_string()]);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_locked_secret_is_recorded_not_fatal() {
        // A failing secret delete records an error but the rest of the teardown —
        // including removing the directories — still runs.
        let base = std::env::temp_dir().join(format!("red-reset-lock-{}", std::process::id()));
        let config = base.join("config");
        std::fs::create_dir_all(&config).unwrap();

        let stub = StubSecrets {
            fail_on: Some("conn-a".into()),
            ..Default::default()
        };
        let report = remove_all_data_with(
            &["conn-a".into(), "conn-b".into()],
            &[],
            Some(config.clone()),
            None,
            &stub,
        );

        assert_eq!(report.connections_cleared, 1, "only conn-b succeeded");
        assert_eq!(report.errors.len(), 2, "the lock + the missing data dir");
        assert!(
            report.config_dir_removed,
            "dir removal ran despite the lock"
        );
        assert!(!config.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_directory_counts_as_removed() {
        let report = remove_all_data_with(
            &[],
            &[],
            Some(std::env::temp_dir().join("red-reset-does-not-exist-xyz")),
            Some(std::env::temp_dir().join("red-reset-nope-xyz")),
            &StubSecrets::default(),
        );
        assert!(report.config_dir_removed && report.data_dir_removed);
        assert!(report.errors.is_empty());
    }
}
