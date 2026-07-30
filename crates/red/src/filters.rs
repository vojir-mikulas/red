//! Recent result filters: the predicates a user has actually applied, kept per
//! connection **and** per browsed table so the same table offers the same filters
//! next time it is opened.
//!
//! Query history (`history.rs`) logs statements; this logs the narrowings applied
//! on top of one. They are separate stores because they are recalled from
//! different places and scoped differently: a filter only means anything against
//! the columns it was written for, so it is keyed by the table it was applied to,
//! not just the connection.
//!
//! Storage mirrors `history.rs`: one JSON file, `<config>/red/filters.json`,
//! rewritten atomically (temp + rename), owner-only (`0o600`) on Unix — a
//! predicate can embed PII as a literal — capped per scope with a global
//! backstop, and fail-open (a missing or corrupt file is simply "no history").
//!
//! The mode rides as a string tag rather than the [`crate::filter::FilterMode`]
//! enum so a file written by a newer build (with a mode this one doesn't know)
//! loads fine and merely skips those entries, instead of failing the whole read.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::filter::FilterMode;

/// Newest filters retained per `(connection, scope)`. Past this, the oldest for
/// that scope are dropped on the next record.
const MAX_PER_SCOPE: usize = 20;
/// Global backstop across every connection and table, so a long-lived install
/// can't grow the file without bound.
const MAX_TOTAL: usize = 500;

/// One applied filter, remembered for recall. Flat (not nested per scope) so the
/// caps and the newest-first ordering work exactly as `history.rs`'s do.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RecentFilter {
    pub conn_id: String,
    /// The browsed table the filter was applied to. Empty for an editor result,
    /// whose columns change with every statement, so those share one bucket per
    /// connection rather than pretending to be table-scoped.
    #[serde(default)]
    pub scope: String,
    /// [`FilterMode::tag`] — how `text` is read.
    pub mode: String,
    pub text: String,
    /// Unix seconds when it was last applied (0 if the clock was before the epoch).
    #[serde(default)]
    pub used_unix: u64,
}

impl RecentFilter {
    /// The mode this entry's text is read in, or `None` for a tag written by a
    /// newer build (such an entry is simply not offered).
    pub(crate) fn mode(&self) -> Option<FilterMode> {
        FilterMode::from_tag(&self.mode)
    }
}

/// The on-disk shape: a wrapper object (not a bare array) so the format can grow.
#[derive(Default, Serialize, Deserialize)]
struct FiltersFile {
    #[serde(default)]
    entries: Vec<RecentFilter>,
}

/// The recent-filters store, newest-first. Mutations persist immediately (unless
/// `path` is `None`, as in tests).
pub(crate) struct FilterHistory {
    entries: Vec<RecentFilter>,
    path: Option<PathBuf>,
}

/// `<config>/red/filters.json`.
fn filters_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("filters.json"))
}

impl FilterHistory {
    /// Read the store from disk, or start empty. Never fails: a missing file is
    /// an empty store; a corrupt one is warned about and dropped (fail-open, like
    /// the other persisted-data loaders).
    pub(crate) fn load() -> Self {
        let path = filters_path();
        let entries = match path.as_ref().map(std::fs::read_to_string) {
            Some(Ok(contents)) => match serde_json::from_str::<FiltersFile>(&contents) {
                Ok(file) => file.entries,
                Err(e) => {
                    tracing::warn!("ignoring corrupt filter history: {e}");
                    Vec::new()
                }
            },
            // Missing file or unreadable dir means an empty store, not an error.
            _ => Vec::new(),
        };
        Self { entries, path }
    }

    /// Remember a filter that was just applied. Blank text is not a filter, so it
    /// isn't recorded. Re-applying something already remembered moves it back to
    /// the front (MRU) instead of duplicating it, so refining a predicate doesn't
    /// bury the ones before it.
    pub(crate) fn record(
        &mut self,
        conn_id: &str,
        scope: Option<&str>,
        mode: FilterMode,
        text: &str,
    ) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let scope = scope.unwrap_or_default();
        let tag = mode.tag();
        self.entries.retain(|e| {
            !(e.conn_id == conn_id && e.scope == scope && e.mode == tag && e.text == text)
        });
        self.entries.insert(
            0,
            RecentFilter {
                conn_id: conn_id.to_string(),
                scope: scope.to_string(),
                mode: tag.to_string(),
                text: text.to_string(),
                used_unix: crate::conversations::now_unix(),
            },
        );
        self.prune();
        self.persist();
    }

    /// One scope's filters, newest-first: what the recall dropdown lists and what
    /// ↑/↓ walks. Entries whose mode this build doesn't know are skipped.
    pub(crate) fn for_scope(&self, conn_id: &str, scope: Option<&str>) -> Vec<RecentFilter> {
        let scope = scope.unwrap_or_default();
        self.entries
            .iter()
            .filter(|e| e.conn_id == conn_id && e.scope == scope && e.mode().is_some())
            .cloned()
            .collect()
    }

    /// Forget one entry (the dropdown's per-row ✕). Matched on its identity
    /// (scope + mode + text), since entries carry no id of their own.
    pub(crate) fn forget(&mut self, conn_id: &str, scope: Option<&str>, mode: &str, text: &str) {
        let scope = scope.unwrap_or_default();
        let before = self.entries.len();
        self.entries.retain(|e| {
            !(e.conn_id == conn_id && e.scope == scope && e.mode == mode && e.text == text)
        });
        if self.entries.len() != before {
            self.persist();
        }
    }

    /// Enforce the per-scope and global caps. Entries are newest-first, so a
    /// running per-scope tally keeps the newest and drops the overflow.
    fn prune(&mut self) {
        let mut counts: HashMap<(String, String), usize> = HashMap::new();
        self.entries.retain(|e| {
            let n = counts
                .entry((e.conn_id.clone(), e.scope.clone()))
                .or_insert(0);
            *n += 1;
            *n <= MAX_PER_SCOPE
        });
        self.entries.truncate(MAX_TOTAL);
    }

    /// Write the whole store to disk atomically. A failure is logged, not fatal:
    /// recall is a convenience, never worth interrupting a query over.
    fn persist(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if let Err(e) = save(&path, &self.entries) {
            tracing::warn!("failed to save filter history: {e}");
        }
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        Self {
            entries: Vec::new(),
            path: None,
        }
    }
}

/// Serialize `entries` to `path` via a temp file + rename, owner-only on Unix.
///
/// Deliberately *not* `fsync`ed (like `recent_keys.rs`, unlike `history.rs`):
/// this runs on the UI thread on every Apply, and a durable disk flush per
/// keystroke-and-Enter is a visible stall. The atomic rename still guarantees a
/// reader never sees a torn file; only the durability wait is dropped, and the
/// loader is fail-open, so losing the last write to a crash is harmless.
fn save(path: &PathBuf, entries: &[RecentFilter]) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the config directory")?;
    }
    let file = FiltersFile {
        entries: entries.to_vec(),
    };
    let contents = serde_json::to_string_pretty(&file).context("serializing filter history")?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).context("creating the filters temp file")?;
    f.write_all(contents.as_bytes())?;
    drop(f);
    std::fs::rename(&tmp, path).context("renaming the filters temp file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_scopes_by_connection_and_table() {
        let mut store = FilterHistory::in_memory();
        store.record("a", Some("users"), FilterMode::Contains, "acme");
        store.record("a", Some("orders"), FilterMode::Where, "amount > 1");
        store.record("b", Some("users"), FilterMode::Contains, "other");

        let users = store.for_scope("a", Some("users"));
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].text, "acme");
        assert_eq!(users[0].mode(), Some(FilterMode::Contains));
        assert_eq!(store.for_scope("a", Some("orders")).len(), 1);
        assert_eq!(store.for_scope("b", Some("users"))[0].text, "other");
        // An editor result (no browsed table) is its own per-connection bucket.
        assert!(store.for_scope("a", None).is_empty());
    }

    #[test]
    fn re_applying_moves_to_the_front_without_duplicating() {
        let mut store = FilterHistory::in_memory();
        store.record("a", Some("t"), FilterMode::Contains, "one");
        store.record("a", Some("t"), FilterMode::Contains, "two");
        store.record("a", Some("t"), FilterMode::Contains, "one");
        let recent = store.for_scope("a", Some("t"));
        assert_eq!(
            recent.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[test]
    fn same_text_in_two_modes_is_two_entries() {
        let mut store = FilterHistory::in_memory();
        store.record("a", Some("t"), FilterMode::Contains, "x");
        store.record("a", Some("t"), FilterMode::Where, "x");
        assert_eq!(store.for_scope("a", Some("t")).len(), 2);
    }

    #[test]
    fn blank_text_is_not_a_filter() {
        let mut store = FilterHistory::in_memory();
        store.record("a", Some("t"), FilterMode::Contains, "   ");
        assert!(store.for_scope("a", Some("t")).is_empty());
    }

    #[test]
    fn per_scope_cap_keeps_the_newest() {
        let mut store = FilterHistory::in_memory();
        for i in 0..MAX_PER_SCOPE + 5 {
            store.record("a", Some("t"), FilterMode::Contains, &format!("f{i}"));
        }
        let recent = store.for_scope("a", Some("t"));
        assert_eq!(recent.len(), MAX_PER_SCOPE);
        assert_eq!(recent[0].text, format!("f{}", MAX_PER_SCOPE + 4));
        // A second scope is unaffected by the first one's overflow.
        store.record("a", Some("u"), FilterMode::Contains, "kept");
        assert_eq!(store.for_scope("a", Some("u")).len(), 1);
    }

    #[test]
    fn forget_removes_one_entry() {
        let mut store = FilterHistory::in_memory();
        store.record("a", Some("t"), FilterMode::Contains, "one");
        store.record("a", Some("t"), FilterMode::Where, "two");
        store.forget("a", Some("t"), FilterMode::Contains.tag(), "one");
        let recent = store.for_scope("a", Some("t"));
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].text, "two");
    }

    #[test]
    fn an_unknown_mode_tag_is_skipped_not_fatal() {
        let mut store = FilterHistory::in_memory();
        store.record("a", Some("t"), FilterMode::Contains, "known");
        store.entries.insert(
            0,
            RecentFilter {
                conn_id: "a".into(),
                scope: "t".into(),
                mode: "from-a-newer-build".into(),
                text: "?".into(),
                used_unix: 0,
            },
        );
        let recent = store.for_scope("a", Some("t"));
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].text, "known");
    }
}
