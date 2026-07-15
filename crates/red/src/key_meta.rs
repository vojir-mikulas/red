//! Persistence for per-key annotations in the Redis browser: a favorite star, a
//! free-text note, and tags. Closes the "tag / note / favorite keys" gap the
//! zedis comparison flagged (see docs/plans/report/red-redis-vs-zedis.md).
//!
//! Storage mirrors `recent_keys.rs`: one JSON file,
//! `<config>/red/redis-key-meta.json`, rewritten atomically (temp + rename),
//! owner-only (`0o600`) on Unix, keyed by the same `conn_id` the recent-keys,
//! query-history, and analysis stores use. A missing or corrupt file is simply
//! "no annotations"; one bad file never blocks startup (fail-open).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// One key's annotations. All fields default, so an older file (or a key with
/// only some set) round-trips cleanly, and an all-default annotation is pruned
/// from the store rather than persisted.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct KeyAnnotation {
    #[serde(default)]
    pub(crate) favorite: bool,
    #[serde(default)]
    pub(crate) note: String,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
}

impl KeyAnnotation {
    /// True when there's nothing worth persisting (so it can be pruned).
    fn is_empty(&self) -> bool {
        !self.favorite && self.note.is_empty() && self.tags.is_empty()
    }
}

/// The on-disk shape: `conn_id -> (key -> annotation)`, wrapped so the format
/// can grow.
#[derive(Default, Serialize, Deserialize)]
struct KeyMetaFile {
    #[serde(default)]
    keys: HashMap<String, HashMap<String, KeyAnnotation>>,
}

/// `<config>/red/redis-key-meta.json`.
fn key_meta_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("redis-key-meta.json"))
}

/// The saved key-annotation store, persisted immediately on every change
/// (unless `path` is `None`, as in tests).
pub(crate) struct KeyMetaStore {
    keys: HashMap<String, HashMap<String, KeyAnnotation>>,
    path: Option<PathBuf>,
}

impl KeyMetaStore {
    /// Read saved annotations from disk, or start empty. Never fails: a missing
    /// file is an empty store; a corrupt one is warned about and dropped.
    pub(crate) fn load() -> Self {
        let path = key_meta_path();
        let keys = match path.as_ref().map(std::fs::read_to_string) {
            Some(Ok(contents)) => match serde_json::from_str::<KeyMetaFile>(&contents) {
                Ok(file) => file.keys,
                Err(e) => {
                    tracing::warn!("ignoring corrupt redis key-meta store: {e}");
                    HashMap::new()
                }
            },
            _ => HashMap::new(),
        };
        Self { keys, path }
    }

    /// This key's annotation, if any is saved.
    pub(crate) fn get(&self, conn_id: &str, key: &str) -> Option<&KeyAnnotation> {
        self.keys.get(conn_id)?.get(key)
    }

    /// Whether the key is starred.
    pub(crate) fn is_favorite(&self, conn_id: &str, key: &str) -> bool {
        self.get(conn_id, key).is_some_and(|a| a.favorite)
    }

    /// The set of starred keys for a connection (for the browse list's ★).
    pub(crate) fn favorites(&self, conn_id: &str) -> std::collections::HashSet<String> {
        self.keys
            .get(conn_id)
            .map(|m| {
                m.iter()
                    .filter(|(_, a)| a.favorite)
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every distinct tag used across this connection's keys, sorted, for the
    /// browse toolbar's tag-filter dropdown. Empty when nothing is tagged.
    pub(crate) fn all_tags(&self, conn_id: &str) -> Vec<String> {
        let mut set: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        if let Some(m) = self.keys.get(conn_id) {
            for ann in m.values() {
                set.extend(ann.tags.iter().map(String::as_str));
            }
        }
        set.into_iter().map(str::to_string).collect()
    }

    /// The set of keys carrying `tag` for a connection (for the tag filter).
    pub(crate) fn keys_with_tag(
        &self,
        conn_id: &str,
        tag: &str,
    ) -> std::collections::HashSet<String> {
        self.keys
            .get(conn_id)
            .map(|m| {
                m.iter()
                    .filter(|(_, a)| a.tags.iter().any(|t| t == tag))
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Flip the favorite flag, persist, and return the new state.
    pub(crate) fn toggle_favorite(&mut self, conn_id: &str, key: &str) -> bool {
        let mut now = false;
        self.update(conn_id, key, |a| {
            a.favorite = !a.favorite;
            now = a.favorite;
        });
        now
    }

    /// Replace this key's note + tags (favorite is untouched) and persist. Empty
    /// tags are dropped and each tag trimmed.
    pub(crate) fn set_note_tags(
        &mut self,
        conn_id: &str,
        key: &str,
        note: String,
        tags: Vec<String>,
    ) {
        let tags: Vec<String> = tags
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        self.update(conn_id, key, |a| {
            a.note = note.trim().to_string();
            a.tags = tags;
        });
    }

    /// Apply `f` to the key's annotation, then prune it if it went empty, and
    /// persist. Centralizes the "mutate + prune + save" bookkeeping.
    fn update(&mut self, conn_id: &str, key: &str, f: impl FnOnce(&mut KeyAnnotation)) {
        let per_conn = self.keys.entry(conn_id.to_string()).or_default();
        let mut ann = per_conn.remove(key).unwrap_or_default();
        f(&mut ann);
        if ann.is_empty() {
            if per_conn.is_empty() {
                self.keys.remove(conn_id);
            }
        } else {
            per_conn.insert(key.to_string(), ann);
        }
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if let Err(e) = save(&path, &self.keys) {
            tracing::warn!("failed to save redis key-meta store: {e}");
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

/// Serialize `keys` to `path` via a temp file + rename, owner-only on Unix. Like
/// `recent_keys.rs`, not `fsync`ed: this runs on the UI thread on every edit and
/// the store is a throwaway convenience (the loader is fail-open).
fn save(path: &PathBuf, keys: &HashMap<String, HashMap<String, KeyAnnotation>>) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the config directory")?;
    }
    let file = KeyMetaFile { keys: keys.clone() };
    let contents = serde_json::to_string_pretty(&file).context("serializing redis key meta")?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).context("creating the key-meta temp file")?;
    f.write_all(contents.as_bytes())?;
    drop(f);
    std::fs::rename(&tmp, path).context("renaming the key-meta temp file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favorite_toggles_and_prunes() {
        let mut store = KeyMetaStore::in_memory();
        assert!(!store.is_favorite("c", "user:1"));
        assert!(store.toggle_favorite("c", "user:1")); // now on
        assert!(store.is_favorite("c", "user:1"));
        // Toggling back to empty prunes the entry entirely.
        assert!(!store.toggle_favorite("c", "user:1"));
        assert!(store.get("c", "user:1").is_none());
    }

    #[test]
    fn note_and_tags_round_trip_and_trim() {
        let mut store = KeyMetaStore::in_memory();
        store.set_note_tags(
            "c",
            "k",
            "  a note  ".into(),
            vec!["  x ".into(), "".into(), "y".into()],
        );
        let ann = store.get("c", "k").unwrap();
        assert_eq!(ann.note, "a note");
        assert_eq!(ann.tags, vec!["x".to_string(), "y".to_string()]);
        assert!(!ann.favorite);
        // Favorite composes with an existing note (both survive).
        assert!(store.toggle_favorite("c", "k"));
        let ann = store.get("c", "k").unwrap();
        assert!(ann.favorite);
        assert_eq!(ann.note, "a note");
        // Clearing note+tags while favorited keeps the entry (favorite remains).
        store.set_note_tags("c", "k", String::new(), Vec::new());
        assert!(store.is_favorite("c", "k"));
    }

    #[test]
    fn all_tags_and_keys_with_tag() {
        let mut store = KeyMetaStore::in_memory();
        store.set_note_tags(
            "c",
            "user:1",
            String::new(),
            vec!["hot".into(), "prod".into()],
        );
        store.set_note_tags("c", "user:2", String::new(), vec!["prod".into()]);
        store.set_note_tags("c", "job:1", String::new(), vec!["cold".into()]);
        // A different connection's tags don't leak in.
        store.set_note_tags("other", "x", String::new(), vec!["zzz".into()]);

        // Distinct, sorted.
        assert_eq!(store.all_tags("c"), vec!["cold", "hot", "prod"]);
        assert!(store.all_tags("missing").is_empty());

        let prod = store.keys_with_tag("c", "prod");
        assert_eq!(prod.len(), 2);
        assert!(prod.contains("user:1") && prod.contains("user:2"));
        assert_eq!(
            store.keys_with_tag("c", "cold"),
            std::collections::HashSet::from(["job:1".to_string()])
        );
        assert!(store.keys_with_tag("c", "nope").is_empty());
    }
}
