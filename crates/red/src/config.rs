// SPDX-License-Identifier: GPL-3.0-or-later

//! Connection-list persistence. File IO + `serde` live here at the app edge
//! (`anyhow`), keeping `red-core` runtime-free. The list round-trips to a TOML
//! file in the platform config dir; a missing or malformed file degrades to an
//! empty list (never a panic), so a corrupt config can't brick launch.
//!
//! NOTE: passwords are persisted in **plaintext** in this file for now (v0.1).
//! Route credentials through the OS keyring before shipping (v0.2).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use red_core::{ConnectionConfig, DbKind};
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

/// The on-disk shape used for *loading* — tolerant of both the current structured
/// fields and the legacy single-`dsn` format, so upgrading never drops a saved
/// connection. Saving always writes the structured shape via [`StoredConnection`].
#[derive(Default, Deserialize)]
struct RawConnection {
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: DbKind,
    /// Legacy v0.1 field: one opaque DSN. Migrated into the structured fields when
    /// the structured ones are absent.
    #[serde(default)]
    dsn: Option<String>,
    #[serde(default)]
    host: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    user: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    database: String,
    #[serde(default)]
    color: u8,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    last_accessed: Option<u64>,
}

impl RawConnection {
    fn into_stored(self) -> StoredConnection {
        let mut config = ConnectionConfig {
            name: self.name,
            kind: self.kind,
            host: self.host,
            port: self.port.or_else(|| self.kind.default_port()),
            user: self.user,
            password: self.password,
            database: self.database,
            color: self.color,
            read_only: self.read_only,
        };
        // Legacy migration: an old `dsn` with no structured fields populated. For a
        // file engine the DSN *is* the path; otherwise parse it back into fields.
        if config.host.is_empty() && config.database.is_empty() {
            if let Some(dsn) = self.dsn {
                if self.kind.is_file() {
                    config.database = dsn;
                } else if let Some(p) = ConnectionConfig::parse_conn_str(&dsn) {
                    config.kind = p.kind;
                    config.host = p.host;
                    config.port = p.port;
                    config.user = p.user;
                    config.password = p.password;
                    config.database = p.database;
                } else {
                    config.database = dsn;
                }
            }
        }
        StoredConnection {
            config,
            last_accessed: self.last_accessed,
        }
    }
}

#[derive(Default, Deserialize)]
struct RawConfigFile {
    #[serde(default, rename = "connection")]
    connections: Vec<RawConnection>,
}

#[derive(Default, Serialize)]
struct ConfigFile {
    #[serde(rename = "connection")]
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
    match toml::from_str::<RawConfigFile>(&text) {
        Ok(cfg) => {
            let mut connections: Vec<StoredConnection> = cfg
                .connections
                .into_iter()
                .map(RawConnection::into_stored)
                .collect();
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
                    database: "/tmp/app.db".into(),
                    read_only: true,
                    ..Default::default()
                },
                last_accessed: Some(1_700_000_000),
            },
            StoredConnection {
                config: ConnectionConfig {
                    name: "prod".into(),
                    kind: DbKind::Postgres,
                    host: "db".into(),
                    port: Some(5432),
                    user: "analytics".into(),
                    password: "secret".into(),
                    database: "analytics".into(),
                    color: 3,
                    read_only: false,
                },
                last_accessed: None,
            },
        ];

        let cfg = ConfigFile {
            connections: connections.clone(),
        };
        let text = toml::to_string_pretty(&cfg).expect("serialize");
        let back: RawConfigFile = toml::from_str(&text).expect("deserialize");
        let back: Vec<StoredConnection> = back
            .connections
            .into_iter()
            .map(RawConnection::into_stored)
            .collect();

        assert_eq!(back.len(), 2);
        assert_eq!(back[0].config.name, "local");
        assert_eq!(back[0].config.kind, DbKind::Sqlite);
        assert_eq!(back[0].config.database, "/tmp/app.db");
        assert!(back[0].config.read_only);
        assert_eq!(back[0].last_accessed, Some(1_700_000_000));
        assert_eq!(back[1].config.host, "db");
        assert_eq!(back[1].config.user, "analytics");
        assert_eq!(back[1].config.password, "secret");
        assert_eq!(back[1].last_accessed, None);
    }

    #[test]
    fn migrates_legacy_dsn_format() {
        // The v0.1 single-`dsn` shape must still load and decompose into fields.
        let text = r#"
[[connection]]
name = "legacy-pg"
kind = "Postgres"
dsn = "postgres://bob:pw@example.com:5433/shop"
read_only = true

[[connection]]
name = "legacy-sqlite"
kind = "Sqlite"
dsn = "/var/data/app.db"
"#;
        let raw: RawConfigFile = toml::from_str(text).expect("deserialize legacy");
        let conns: Vec<StoredConnection> = raw
            .connections
            .into_iter()
            .map(RawConnection::into_stored)
            .collect();
        assert_eq!(conns[0].config.host, "example.com");
        assert_eq!(conns[0].config.port, Some(5433));
        assert_eq!(conns[0].config.user, "bob");
        assert_eq!(conns[0].config.password, "pw");
        assert_eq!(conns[0].config.database, "shop");
        assert!(conns[0].config.read_only);
        assert_eq!(conns[1].config.kind, DbKind::Sqlite);
        assert_eq!(conns[1].config.database, "/var/data/app.db");
    }
}
