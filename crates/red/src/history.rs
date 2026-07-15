//! Query history: a persistent, connection-scoped log of executed statements.
//!
//! Every statement the user runs from the editor is recorded here so it survives
//! a restart, unlike the old in-memory `Vec<String>` that died with the session.
//! The log is centralized on [`AppState`] (one store across all connections) but
//! each entry carries its `conn_id`, so the History panel shows only the active
//! connection's history while the file keeps everything, the groundwork for a
//! future cross-connection history sidebar (see `docs/plans/query-history.md`).
//!
//! Storage is one JSON file, `<config>/red/history.json`, rewritten atomically
//! (temp + rename) on every change: the same crash-safe discipline as
//! `conversations.rs`/`queries.rs`. The log is capped per connection (and a
//! global backstop), so the file stays small enough that a full rewrite per run
//! is cheap. Like those modules, a missing or corrupt file is simply an empty
//! log; one bad file never blocks startup. Written owner-only (`0o600`) on Unix:
//! a statement can embed literal credentials or PII.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

// `Context as _` brings anyhow's `.context()` into scope without taking the
// `Context` name, which `gpui::Context` (used by `render_history`) needs.
use anyhow::{Context as _, Result};
use gpui::{Context, KeyDownEvent, prelude::*};
use serde::{Deserialize, Serialize};

use crate::app::{ActiveConn, AppState};

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
pub(crate) struct HistoryEntry {
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
pub(crate) struct QueryHistory {
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
    pub(crate) fn load() -> Self {
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

    /// Record a freshly-run statement for `conn_id`. De-dupes against that
    /// connection's most-recent entry (so holding ⌘↵ doesn't spam the log),
    /// prunes to the caps, and persists.
    pub(crate) fn record(&mut self, conn_id: &str, sql: &str) {
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
                sql: sql.to_string(),
                conn_id: conn_id.to_string(),
                ran_unix: crate::conversations::now_unix(),
            },
        );
        self.prune();
        self.persist();
    }

    /// Remove one entry by id (the panel's per-row ✕). A no-op if it's gone.
    pub(crate) fn delete(&mut self, id: u64) {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        if self.entries.len() != before {
            self.persist();
        }
    }

    /// Drop all of one connection's history (the "clear history" command).
    pub(crate) fn clear_conn(&mut self, conn_id: &str) {
        let before = self.entries.len();
        self.entries.retain(|e| e.conn_id != conn_id);
        if self.entries.len() != before {
            self.persist();
        }
    }

    /// One connection's entries, newest-first; what the panel renders.
    pub(crate) fn for_conn(&self, conn_id: &str) -> Vec<HistoryEntry> {
        self.entries
            .iter()
            .filter(|e| e.conn_id == conn_id)
            .cloned()
            .collect()
    }

    /// How many entries one connection has, without cloning them.
    pub(crate) fn count_for_conn(&self, conn_id: &str) -> usize {
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

    /// Write the whole log to disk atomically. A failure is logged, not fatal:
    /// history is best-effort, never worth interrupting a query over.
    fn persist(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if let Err(e) = save(&path, &self.entries) {
            tracing::warn!("failed to save query history: {e}");
        }
    }
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
/// row's subline. Empty for a missing/future stamp (clock skew); no fake time.
pub(crate) fn relative_time(unix: u64) -> String {
    let now = crate::conversations::now_unix();
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

/// The rolling time bucket a history entry falls into: index 0 = Today
/// (< 24h ago, plus any clock-skewed future/zero stamp), 1 = Yesterday
/// (24–48h), 2 = Earlier. Uses the same `now - ran` clock as [`relative_time`].
fn bucket_index(now: u64, ran: u64) -> usize {
    if ran == 0 || now < ran {
        return 0;
    }
    match now - ran {
        0..86_400 => 0,
        86_400..172_800 => 1,
        _ => 2,
    }
}

impl AppState {
    /// The History panel for the left dock: this connection's past queries,
    /// newest first, grouped into collapsible Today/Yesterday/Earlier buckets
    /// with a search box on top. Clicking a row loads it into the active editor;
    /// hovering a row reveals a ✕ to delete it. Pure adapter over the shared
    /// [`crate::history_panel`] renderer.
    pub(crate) fn render_history(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        use crate::history_panel::{HistoryNav, HistoryPanelSpec, HistoryRow, HistorySection};

        let entries = self.query_history.for_conn(&active.conn_id);
        let total = entries.len();
        let query = active
            .history_search
            .read(cx)
            .content()
            .trim()
            .to_lowercase();
        let searching = !query.is_empty();
        let now = crate::conversations::now_unix();

        // Bucket the (already newest-first) entries. `nav_index` stays the row's
        // flat position so the existing ↑/↓ nav — which indexes the full list —
        // keeps landing on the right entry across the bucket headers.
        let mut buckets: [Vec<HistoryRow>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for (i, entry) in entries.into_iter().enumerate() {
            if searching && !entry.sql.to_lowercase().contains(&query) {
                continue;
            }
            let sql = entry.sql.clone();
            let id = entry.id;
            let idx = bucket_index(now, entry.ran_unix);
            buckets[idx].push(HistoryRow {
                primary: crate::editor::history_label(&entry.sql).into(),
                secondary: relative_time(entry.ran_unix).into(),
                badge: None,
                nav_index: Some(i),
                activate: Rc::new(move |this: &mut AppState, replace, cx| {
                    this.open_history(sql.clone(), replace, cx)
                }),
                delete: Some(Rc::new(move |this: &mut AppState, cx| {
                    this.delete_history(id, cx)
                })),
            });
        }

        const LABELS: [(&str, &str); 3] = [
            ("today", "Today"),
            ("yesterday", "Yesterday"),
            ("earlier", "Earlier"),
        ];
        let sections: Vec<HistorySection> = buckets
            .into_iter()
            .zip(LABELS)
            .filter(|(rows, _)| !rows.is_empty())
            .map(|(rows, (key, title))| HistorySection {
                key,
                title: Some(title.into()),
                // A live search force-expands every bucket so matches always show.
                collapsed: !searching && active.history_bucket_collapsed.contains(key),
                toggle: Some(Rc::new(move |this: &mut AppState, cx| {
                    this.history_toggle_bucket(key, cx)
                })),
                rows,
            })
            .collect();

        // Keyboard nav: same key map the dock has always had (arrows + optional
        // vim hjkl/g/G), now routed through the shared renderer.
        let on_key = Rc::new(
            |this: &mut AppState, event: &KeyDownEvent, cx: &mut Context<AppState>| -> bool {
                let vim = this.vim_mode();
                match event.keystroke.key.as_str() {
                    "up" => this.history_move(-1, cx),
                    "down" => this.history_move(1, cx),
                    "k" if vim => this.history_move(-1, cx),
                    "j" if vim => this.history_move(1, cx),
                    // Half-range deltas jump to the ends without overflowing the
                    // `history_sel + delta` sum (`history_move` clamps the rest).
                    "g" if vim => this.history_move(isize::MIN / 2, cx),
                    "G" if vim => this.history_move(isize::MAX / 2, cx),
                    "enter" => this.history_accept(cx),
                    "l" if vim => this.history_accept(cx),
                    "escape" => {
                        this.pending_focus = Some(crate::app::Pane::Editor);
                        cx.notify();
                    }
                    "h" if vim => {
                        this.pending_focus = Some(crate::app::Pane::Editor);
                        cx.notify();
                    }
                    _ => return false,
                }
                true
            },
        );

        let spec = HistoryPanelSpec {
            sections,
            empty_text: if searching {
                "No matches".into()
            } else {
                "No queries yet".into()
            },
            show_clear: total > 0,
            on_clear: Rc::new(|this: &mut AppState, cx| this.clear_history(cx)),
            search: Some(active.history_search.clone()),
            nav: Some(HistoryNav {
                focus: active.history_focus.clone(),
                on_key,
            }),
            selected: Some(active.history_sel),
        };
        self.render_history_panel(spec, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory store (no disk) for exercising the pure record/prune/delete
    /// logic.
    fn in_memory() -> QueryHistory {
        QueryHistory {
            entries: Vec::new(),
            next_id: 1,
            path: None,
        }
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
