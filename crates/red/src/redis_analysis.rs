//! Persistence for the Redis keyspace-analysis report (see docs/plans/redis.md's
//! "persistent database analysis report" gap).
//!
//! Unlike the ephemeral biggest-keys sampler, an analysis report is *saved* so
//! it can be revisited after a restart — the whole point of it being a
//! point-in-time report rather than a live sample. One report is kept per
//! connection (the latest run overwrites the previous), keyed by the same
//! `conn_id` the query-history store uses.
//!
//! Storage mirrors `history.rs`: one JSON file, `<config>/red/redis-analysis.json`,
//! rewritten atomically (temp + rename), owner-only (`0o600`) on Unix. A missing
//! or corrupt file is simply "no saved reports"; one bad file never blocks
//! startup (fail-open, like the other persisted-data loaders).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use red_core::kv::RedisAnalysis;
use serde::{Deserialize, Serialize};

/// The on-disk shape: a wrapper object (not a bare map) so the format can grow
/// fields later without breaking older files.
#[derive(Default, Serialize, Deserialize)]
struct AnalysisFile {
    #[serde(default)]
    reports: HashMap<String, RedisAnalysis>,
}

/// `<config>/red/redis-analysis.json`.
fn analysis_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("redis-analysis.json"))
}

/// The saved-analysis store: the latest report per connection, persisted
/// immediately on `set` (unless `path` is `None`, as in tests).
pub(crate) struct AnalysisStore {
    reports: HashMap<String, RedisAnalysis>,
    path: Option<PathBuf>,
}

impl AnalysisStore {
    /// Read saved reports from disk, or start empty. Never fails: a missing
    /// file is an empty store; a corrupt one is warned about and dropped.
    pub(crate) fn load() -> Self {
        let path = analysis_path();
        let reports = match path.as_ref().map(std::fs::read_to_string) {
            Some(Ok(contents)) => match serde_json::from_str::<AnalysisFile>(&contents) {
                Ok(file) => file.reports,
                Err(e) => {
                    tracing::warn!("ignoring corrupt redis analysis store: {e}");
                    HashMap::new()
                }
            },
            _ => HashMap::new(),
        };
        Self { reports, path }
    }

    /// The saved report for `conn_id`, if any.
    pub(crate) fn get(&self, conn_id: &str) -> Option<&RedisAnalysis> {
        self.reports.get(conn_id)
    }

    /// Save (overwrite) the report for `conn_id` and persist. A persistence
    /// failure is logged, not fatal: the report still shows in-session.
    pub(crate) fn set(&mut self, conn_id: &str, report: RedisAnalysis) {
        self.reports.insert(conn_id.to_string(), report);
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if let Err(e) = save(&path, &self.reports) {
            tracing::warn!("failed to save redis analysis store: {e}");
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
fn save(path: &PathBuf, reports: &HashMap<String, RedisAnalysis>) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the config directory")?;
    }
    let file = AnalysisFile {
        reports: reports.clone(),
    };
    let contents = serde_json::to_string_pretty(&file).context("serializing redis analysis")?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).context("creating the analysis temp file")?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).context("renaming the analysis temp file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_core::kv::TtlSummary;

    fn sample_report() -> RedisAnalysis {
        RedisAnalysis {
            generated_at: 1_700_000_000,
            sampled: 3,
            total_keys: 3,
            total_bytes: 42,
            truncated: false,
            types: Vec::new(),
            namespaces: Vec::new(),
            ttl: TtlSummary::default(),
        }
    }

    #[test]
    fn set_then_get_round_trips_in_memory() {
        let mut store = AnalysisStore::in_memory();
        assert!(store.get("conn-a").is_none());
        store.set("conn-a", sample_report());
        assert_eq!(store.get("conn-a").unwrap().total_bytes, 42);
        // A second connection is independent.
        assert!(store.get("conn-b").is_none());
    }
}
