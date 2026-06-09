// SPDX-License-Identifier: GPL-3.0-or-later

//! Connection-list persistence. File IO + `serde` live here at the app edge
//! (`anyhow`), keeping `red-core` runtime-free. The list round-trips to a TOML
//! file in the platform config dir; a missing or malformed file degrades to an
//! empty list (never a panic), so a corrupt config can't brick launch.
//!
//! NOTE: SQLite DSNs are file paths — no secrets. When Postgres lands (M7), do
//! **not** widen this to persist passwords; route credentials through a keyring
//! (v0.2) instead.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use red_core::ConnectionConfig;
use serde::{Deserialize, Serialize};

/// A saved connection plus a recency stamp for "recent" ordering. Flattens the
/// domain `ConnectionConfig` so each `[[connection]]` entry stays a flat table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredConnection {
    #[serde(flatten)]
    pub config: ConnectionConfig,
    /// Unix seconds of the last successful connect; `None` until first used.
    #[serde(default)]
    pub last_accessed: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(default, rename = "connection")]
    connections: Vec<StoredConnection>,
}

/// Current unix time in seconds, or `0` if the clock is before the epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("red").join("connections.toml"))
}

/// Load the saved connections, newest-used first. Missing/unreadable/malformed
/// config yields an empty list (with a warning), never an error.
pub fn load() -> Vec<StoredConnection> {
    let Some(path) = config_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match toml::from_str::<ConfigFile>(&text) {
        Ok(cfg) => {
            let mut connections = cfg.connections;
            connections.sort_by_key(|c| std::cmp::Reverse(c.last_accessed));
            connections
        }
        Err(e) => {
            tracing::warn!("ignoring malformed config at {}: {e}", path.display());
            Vec::new()
        }
    }
}

/// Persist the connection list. Creates the config dir if needed.
pub fn save(connections: &[StoredConnection]) -> Result<()> {
    let path = config_path().context("no platform config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cfg = ConfigFile {
        connections: connections.to_vec(),
    };
    let text = toml::to_string_pretty(&cfg)?;
    std::fs::write(&path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_core::DbKind;

    #[test]
    fn config_file_round_trips() {
        let connections = vec![
            StoredConnection {
                config: ConnectionConfig {
                    name: "local".into(),
                    kind: DbKind::Sqlite,
                    dsn: "/tmp/app.db".into(),
                    read_only: true,
                },
                last_accessed: Some(1_700_000_000),
            },
            StoredConnection {
                config: ConnectionConfig {
                    name: "prod".into(),
                    kind: DbKind::Postgres,
                    dsn: "postgres://db/analytics".into(),
                    read_only: false,
                },
                last_accessed: None,
            },
        ];

        let cfg = ConfigFile {
            connections: connections.clone(),
        };
        let text = toml::to_string_pretty(&cfg).expect("serialize");
        let back: ConfigFile = toml::from_str(&text).expect("deserialize");

        assert_eq!(back.connections.len(), 2);
        assert_eq!(back.connections[0].config.name, "local");
        assert_eq!(back.connections[0].config.kind, DbKind::Sqlite);
        assert!(back.connections[0].config.read_only);
        assert_eq!(back.connections[0].last_accessed, Some(1_700_000_000));
        assert_eq!(back.connections[1].config.dsn, "postgres://db/analytics");
        assert_eq!(back.connections[1].last_accessed, None);
    }
}
