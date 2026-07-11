//! Persistence for the Redis inspector's "recently viewed keys" list (see
//! docs/plans/redis-workflow-parity.md Part 2). The History dock's Keys section
//! reads it, and — unlike the in-memory-only first cut — it survives a restart
//! so the browsing history a user builds up isn't lost when they reconnect.
//!
//! Storage mirrors `redis_analysis.rs`: one JSON file,
//! `<config>/red/redis-recent-keys.json`, rewritten atomically (temp + rename),
//! owner-only (`0o600`) on Unix, keyed by the same `conn_id` the query-history
//! and analysis stores use. A missing or corrupt file is simply "no history";
//! one bad file never blocks startup (fail-open, like the other loaders).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// One persisted recently-viewed key. A plain, serde-friendly mirror of
/// `kvbrowse::RecentKey` (which holds a `KvType`/`Duration` the UI wants but
/// that don't need to round-trip as their own types): the type is stored as its
/// label string and the TTL as whole seconds.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RecentKeyRec {
    pub(crate) key: String,
    pub(crate) kv_type: String,
    pub(crate) ttl_secs: Option<u64>,
    pub(crate) viewed_unix: u64,
}

/// The on-disk shape: a wrapper object (not a bare map) so the format can grow.
#[derive(Default, Serialize, Deserialize)]
struct RecentKeysFile {
    #[serde(default)]
    keys: HashMap<String, Vec<RecentKeyRec>>,
}

/// `<config>/red/redis-recent-keys.json`.
fn recent_keys_path() -> Option<PathBuf> {
    Some(
        dirs::config_dir()?
            .join("red")
            .join("redis-recent-keys.json"),
    )
}

/// The saved recent-keys store: the recently-viewed list per connection,
/// persisted immediately on `set` (unless `path` is `None`, as in tests).
pub(crate) struct RecentKeysStore {
    keys: HashMap<String, Vec<RecentKeyRec>>,
    path: Option<PathBuf>,
}

impl RecentKeysStore {
    /// Read saved recent keys from disk, or start empty. Never fails: a missing
    /// file is an empty store; a corrupt one is warned about and dropped.
    pub(crate) fn load() -> Self {
        let path = recent_keys_path();
        let keys = match path.as_ref().map(std::fs::read_to_string) {
            Some(Ok(contents)) => match serde_json::from_str::<RecentKeysFile>(&contents) {
                Ok(file) => file.keys,
                Err(e) => {
                    tracing::warn!("ignoring corrupt redis recent-keys store: {e}");
                    HashMap::new()
                }
            },
            _ => HashMap::new(),
        };
        Self { keys, path }
    }

    /// The saved recent keys for `conn_id`, newest-first, if any.
    pub(crate) fn get(&self, conn_id: &str) -> Option<&Vec<RecentKeyRec>> {
        self.keys.get(conn_id)
    }

    /// Save (overwrite) the recent-keys list for `conn_id` and persist. A
    /// persistence failure is logged, not fatal: the list still shows in-session.
    pub(crate) fn set(&mut self, conn_id: &str, keys: Vec<RecentKeyRec>) {
        self.keys.insert(conn_id.to_string(), keys);
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if let Err(e) = save(&path, &self.keys) {
            tracing::warn!("failed to save redis recent-keys store: {e}");
        }
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        Self {
            keys: HashMap::new(),
            path: None,
        }
    }
}

/// Serialize `keys` to `path` via a temp file + rename, owner-only on Unix (the
/// same crash-safe discipline as `redis_analysis.rs`).
fn save(path: &PathBuf, keys: &HashMap<String, Vec<RecentKeyRec>>) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the config directory")?;
    }
    let file = RecentKeysFile { keys: keys.clone() };
    let contents = serde_json::to_string_pretty(&file).context("serializing redis recent keys")?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp)
        .context("creating the recent-keys temp file")?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).context("renaming the recent-keys temp file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: &str) -> RecentKeyRec {
        RecentKeyRec {
            key: key.to_string(),
            kv_type: "string".to_string(),
            ttl_secs: None,
            viewed_unix: 1_700_000_000,
        }
    }

    #[test]
    fn set_then_get_round_trips_in_memory() {
        let mut store = RecentKeysStore::in_memory();
        assert!(store.get("conn-a").is_none());
        store.set("conn-a", vec![rec("user:1"), rec("user:2")]);
        assert_eq!(store.get("conn-a").unwrap().len(), 2);
        assert_eq!(store.get("conn-a").unwrap()[0].key, "user:1");
        // A second connection is independent.
        assert!(store.get("conn-b").is_none());
    }
}
