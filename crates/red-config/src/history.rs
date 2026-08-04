//! Query history: a persistent, connection-scoped log of executed statements.
//!
//! Every statement the user runs from the editor (and every command the Redis
//! console runs) is recorded here so it survives a restart. One store spans all
//! connections but each entry carries its `conn_id`, so a panel shows only the
//! active connection's history while the file keeps everything.
//!
//! Storage is one JSON file, `<config>/red/history.json`, rewritten atomically
//! (temp + rename) on every change: the same crash-safe discipline as
//! [`crate::queries`]. The log is capped per connection (and a global backstop),
//! so the file stays small enough that a full rewrite per run is cheap. A missing
//! or corrupt file is simply an empty log; one bad file never blocks startup.
//! Written owner-only (`0o600`) on Unix: a statement can embed literal
//! credentials or PII.
//!
//! This lives below the UI because it is grounding, not just a panel: the
//! assistant's `search_query_history` retrieves from it on the service thread,
//! and a log of statements a human actually wrote against *this* database
//! encodes the real join paths and filter idioms that no schema can.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// The most SQL bytes one history entry stores. A 10MB pasted script recorded
/// verbatim would be re-cloned, re-serialized, and fsynced on every subsequent
/// run; past this the entry keeps a head + a marker so the log stays light while
/// still showing what ran.
const MAX_ENTRY_SQL_BYTES: usize = 16 * 1024;

/// Newest entries retained per connection. Past this, the oldest for that
/// connection are dropped on the next record/delete.
const MAX_PER_CONN: usize = 100;
/// Global backstop across all connections, so a hundred connections can't grow
/// the file without bound.
const MAX_TOTAL: usize = 1000;

/// One logged statement: the SQL, which connection ran it, and when. `id` is
/// process-monotonic (seeded past the max on load) so it stays unique across
/// restarts and gives the panel a stable handle to delete a row by.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: u64,
    pub sql: String,
    pub conn_id: String,
    /// Unix seconds when it ran (0 if the clock was before the epoch).
    #[serde(default)]
    pub ran_unix: u64,
}

/// The on-disk shape: a wrapper object (not a bare array) so the format can grow
/// later fields without breaking older files.
#[derive(Default, Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default)]
    entries: Vec<HistoryEntry>,
}

/// The query-history store. Entries are kept newest-first; mutations persist
/// immediately (unless `path` is `None`, as in tests).
pub struct QueryHistory {
    entries: Vec<HistoryEntry>,
    next_id: u64,
    path: Option<PathBuf>,
}

/// `<config>/red/history.json`.
fn history_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("history.json"))
}

impl QueryHistory {
    /// Read the log from disk, or start empty. Never fails: a missing file is an
    /// empty log; a corrupt one is warned about and dropped (fail-open, like the
    /// other persisted-data loaders). Entries are sorted newest-first by `id`.
    pub fn load() -> Self {
        let path = history_path();
        let mut entries = match path.as_ref().map(std::fs::read_to_string) {
            Some(Ok(contents)) => match serde_json::from_str::<HistoryFile>(&contents) {
                Ok(file) => file.entries,
                Err(e) => {
                    tracing::warn!("ignoring corrupt query history: {e}");
                    Vec::new()
                }
            },
            // Missing file or unreadable dir means an empty log, not an error.
            _ => Vec::new(),
        };
        // `id` is monotonic, so descending `id` is reverse-chronological.
        entries.sort_by_key(|e| std::cmp::Reverse(e.id));
        let next_id = entries.iter().map(|e| e.id).max().map_or(1, |m| m + 1);
        Self {
            entries,
            next_id,
            path,
        }
    }

    /// A store with no file behind it: nothing it records is persisted. For
    /// tests (in this crate and in the UI crate's dock tests), which must never
    /// touch the user's real `history.json`.
    pub fn in_memory() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
            path: None,
        }
    }

    /// Record a freshly-run statement for `conn_id`. De-dupes against that
    /// connection's most-recent entry (so holding ⌘↵ doesn't spam the log),
    /// prunes to the caps, and persists.
    ///
    /// Called from the **user's** run path only. The assistant's `run_select`
    /// deliberately does not record: history is grounding precisely because it is
    /// human-authored, and feeding the agent's own output back in is a confidence
    /// loop with no ground truth.
    pub fn record(&mut self, conn_id: &str, sql: &str) {
        // The first entry matching this connection is its most recent one.
        let dup = self
            .entries
            .iter()
            .find(|e| e.conn_id == conn_id)
            .is_some_and(|e| e.sql == sql);
        if dup {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entries.insert(
            0,
            HistoryEntry {
                id,
                sql: cap_entry_sql(sql),
                conn_id: conn_id.to_string(),
                ran_unix: crate::config::now(),
            },
        );
        self.prune();
        self.persist();
    }

    /// Remove one entry by id (the panel's per-row ✕). A no-op if it's gone.
    pub fn delete(&mut self, id: u64) {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        if self.entries.len() != before {
            self.persist();
        }
    }

    /// Drop all of one connection's history (the "clear history" command).
    pub fn clear_conn(&mut self, conn_id: &str) {
        let before = self.entries.len();
        self.entries.retain(|e| e.conn_id != conn_id);
        if self.entries.len() != before {
            self.persist();
        }
    }

    /// One connection's entries, newest-first; what the panel renders.
    pub fn for_conn(&self, conn_id: &str) -> Vec<HistoryEntry> {
        self.entries
            .iter()
            .filter(|e| e.conn_id == conn_id)
            .cloned()
            .collect()
    }

    /// How many entries one connection has, without cloning them.
    pub fn count_for_conn(&self, conn_id: &str) -> usize {
        self.entries.iter().filter(|e| e.conn_id == conn_id).count()
    }

    /// Enforce the per-connection and global caps. Entries are newest-first, so a
    /// running per-connection tally keeps the newest and drops the overflow.
    fn prune(&mut self) {
        let mut counts: HashMap<String, usize> = HashMap::new();
        self.entries.retain(|e| {
            let n = counts.entry(e.conn_id.clone()).or_insert(0);
            *n += 1;
            *n <= MAX_PER_CONN
        });
        self.entries.truncate(MAX_TOTAL);
    }

    /// Write the whole log to disk atomically, **off the calling thread**.
    ///
    /// This runs on every ⌘↵. Done inline it deep-cloned up to `MAX_TOTAL`
    /// entries, pretty-printed the lot, and called `sync_all()` — `F_FULLFSYNC` on
    /// macOS, which waits on the physical device — all on the GPUI thread, so every
    /// statement run cost a visible stall. The clone is the price of moving it; the
    /// fsync is what it buys back. `filters.rs` and `key_meta.rs` document the same
    /// trade from the other side (they skip the fsync instead).
    ///
    /// Ordering is not guaranteed between two persists in flight, and does not need
    /// to be: each writes the *whole* log, and the temp-file + rename makes the file
    /// on disk always one complete snapshot. A racing pair leaves whichever landed
    /// last, which is one run's worth of history at worst.
    ///
    /// A failure is logged, not fatal: history is best-effort, never worth
    /// interrupting a query over.
    fn persist(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let entries = self.entries.clone();
        std::thread::spawn(move || {
            if let Err(e) = save(&path, &entries) {
                tracing::warn!("failed to save query history: {e}");
            }
        });
    }
}

/// Cap one entry's SQL to [`MAX_ENTRY_SQL_BYTES`], keeping a head slice plus a
/// truncation marker on a char boundary. A short statement (the overwhelming
/// case) is stored verbatim.
fn cap_entry_sql(sql: &str) -> String {
    if sql.len() <= MAX_ENTRY_SQL_BYTES {
        return sql.to_string();
    }
    let mut cut = MAX_ENTRY_SQL_BYTES;
    while cut > 0 && !sql.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n-- … (truncated in history; {} total)",
        &sql[..cut],
        sql.len()
    )
}

/// Serialize `entries` to `path` via a temp file + rename, owner-only on Unix.
fn save(path: &PathBuf, entries: &[HistoryEntry]) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the config directory")?;
    }
    let file = HistoryFile {
        entries: entries.to_vec(),
    };
    let contents = serde_json::to_string_pretty(&file).context("serializing query history")?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).context("creating the history temp file")?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).context("renaming the history temp file")?;
    Ok(())
}

/// A short, human relative time ("just now", "5m ago", "3h ago", "2d ago") for a
/// row's subline or a retrieved entry's header. Empty for a missing/future stamp
/// (clock skew); no fake time.
pub fn relative_time(unix: u64) -> String {
    let now = crate::config::now();
    if unix == 0 || now < unix {
        return String::new();
    }
    let secs = now - unix;
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory store (no disk) for exercising the pure record/prune/delete
    /// logic.
    fn in_memory() -> QueryHistory {
        QueryHistory::in_memory()
    }

    #[test]
    fn records_newest_first_and_scopes_by_connection() {
        let mut h = in_memory();
        h.record("a", "select 1");
        h.record("b", "select 2");
        h.record("a", "select 3");

        let a = h.for_conn("a");
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].sql, "select 3"); // newest first
        assert_eq!(a[1].sql, "select 1");
        assert_eq!(h.for_conn("b").len(), 1);
        assert_eq!(h.count_for_conn("a"), 2);
    }

    #[test]
    fn de_dupes_consecutive_identical_runs_per_connection() {
        let mut h = in_memory();
        h.record("a", "select 1");
        h.record("a", "select 1"); // immediate repeat, ignored
        assert_eq!(h.for_conn("a").len(), 1);
        // A different connection's identical SQL is its own entry.
        h.record("b", "select 1");
        assert_eq!(h.for_conn("b").len(), 1);
        // Re-running after something else is recorded again.
        h.record("a", "select 2");
        h.record("a", "select 1");
        assert_eq!(h.for_conn("a").len(), 3);
    }

    #[test]
    fn ids_are_unique_and_delete_targets_one_entry() {
        let mut h = in_memory();
        h.record("a", "select 1");
        h.record("a", "select 2");
        let ids: Vec<u64> = h.for_conn("a").iter().map(|e| e.id).collect();
        assert_ne!(ids[0], ids[1]);
        h.delete(ids[0]);
        let left = h.for_conn("a");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].sql, "select 1");
    }

    #[test]
    fn clear_conn_only_clears_that_connection() {
        let mut h = in_memory();
        h.record("a", "select 1");
        h.record("b", "select 2");
        h.clear_conn("a");
        assert_eq!(h.for_conn("a").len(), 0);
        assert_eq!(h.for_conn("b").len(), 1);
    }

    #[test]
    fn prune_caps_entries_per_connection_keeping_newest() {
        let mut h = in_memory();
        for i in 0..(MAX_PER_CONN + 25) {
            h.record("a", &format!("select {i}"));
        }
        let a = h.for_conn("a");
        assert_eq!(a.len(), MAX_PER_CONN);
        // The newest survives; the oldest were dropped.
        assert_eq!(a[0].sql, format!("select {}", MAX_PER_CONN + 24));
    }

    #[test]
    fn round_trips_through_json() {
        let entries = vec![HistoryEntry {
            id: 7,
            sql: "select 1".into(),
            conn_id: "a".into(),
            ran_unix: 123,
        }];
        let json = serde_json::to_string_pretty(&HistoryFile {
            entries: entries.clone(),
        })
        .unwrap();
        let back: HistoryFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].id, 7);
        assert_eq!(back.entries[0].sql, "select 1");
        assert_eq!(back.entries[0].ran_unix, 123);
    }
}
