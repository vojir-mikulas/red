//! Persistence for the SQL connection health report. Structurally a copy of
//! `redis_analysis.rs`, and deliberately so: the two are the same object (one saved,
//! point-in-time report per connection, revisitable after a restart) for two different
//! seams, and the Redis one had already made every decision worth making. One report
//! per connection, the latest run overwriting the previous, keyed by the same `conn_id`
//! the query-history store uses. Storage mirrors `history.rs`: one JSON file,
//! `<config>/red/health.json`, rewritten atomically (temp + rename), owner-only
//! (`0o600`) on Unix. A missing or corrupt file is simply "no saved reports"; one bad
//! file never blocks startup (fail-open, like the other persisted-data loaders). No
//! history and no trend: a report is a thing you ask for, and keeping a series would
//! make it a monitoring feature, which this deliberately is not.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use red_core::health::HealthReport;
use serde::{Deserialize, Serialize};

/// The on-disk shape: a wrapper object (not a bare map) so the format can grow
/// fields later without breaking older files.
#[derive(Default, Serialize, Deserialize)]
struct HealthFile {
    #[serde(default)]
    reports: HashMap<String, HealthReport>,
}

/// `<config>/red/redis-analysis.json`.
fn health_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("health.json"))
}

/// The saved-health store: the latest report per connection, persisted
/// immediately on `set` (unless `path` is `None`, as in tests).
pub(crate) struct HealthStore {
    reports: HashMap<String, HealthReport>,
    path: Option<PathBuf>,
}

impl HealthStore {
    /// Read saved reports from disk, or start empty. Never fails: a missing
    /// file is an empty store; a corrupt one is warned about and dropped.
    pub(crate) fn load() -> Self {
        let path = health_path();
        let reports = match path.as_ref().map(std::fs::read_to_string) {
            Some(Ok(contents)) => match serde_json::from_str::<HealthFile>(&contents) {
                Ok(file) => file.reports,
                Err(e) => {
                    tracing::warn!("ignoring corrupt health store: {e}");
                    HashMap::new()
                }
            },
            _ => HashMap::new(),
        };
        Self { reports, path }
    }

    /// The saved report for `conn_id`, if any.
    pub(crate) fn get(&self, conn_id: &str) -> Option<&HealthReport> {
        self.reports.get(conn_id)
    }

    /// Save (overwrite) the report for `conn_id` and persist. A persistence
    /// failure is logged, not fatal: the report still shows in-session.
    pub(crate) fn set(&mut self, conn_id: &str, report: HealthReport) {
        self.reports.insert(conn_id.to_string(), report);
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if let Err(e) = save(&path, &self.reports) {
            tracing::warn!("failed to save the health store: {e}");
        }
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        Self {
            reports: HashMap::new(),
            path: None,
        }
    }
}

/// Serialize `reports` to `path` via a temp file + rename, owner-only on Unix
/// (the same crash-safe discipline as `history.rs`).
fn save(path: &PathBuf, reports: &HashMap<String, HealthReport>) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the config directory")?;
    }
    let file = HealthFile {
        reports: reports.clone(),
    };
    let contents = serde_json::to_string_pretty(&file).context("serializing the health report")?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).context("creating the health temp file")?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).context("renaming the health temp file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_core::DbKind;

    fn sample_report() -> HealthReport {
        let mut r = HealthReport::new(DbKind::Postgres, Some("public".into()), 1_700_000_000);
        r.totals.bytes = 42;
        r
    }

    #[test]
    fn set_then_get_round_trips_in_memory() {
        let mut store = HealthStore::in_memory();
        assert!(store.get("conn-a").is_none());
        store.set("conn-a", sample_report());
        assert_eq!(store.get("conn-a").unwrap().totals.bytes, 42);
        // A second connection is independent.
        assert!(store.get("conn-b").is_none());
    }
}
