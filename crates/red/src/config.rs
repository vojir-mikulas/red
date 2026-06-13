//! Connection-list persistence. File IO + `serde` live here at the app edge
//! (`anyhow`), keeping `red-core` runtime-free. The list round-trips to a TOML
//! file in the platform config dir; a missing or malformed file degrades to an
//! empty list (never a panic), so a corrupt config can't brick launch.
//!
//! SECURITY: passwords are **never** written to this file. Each connection has a
//! stable [`id`](StoredConnection::id) that doubles as the keychain account for
//! its password (see [`crate::secrets`]); the password is fetched by id on
//! demand. [`load`] migrates any legacy plaintext password it finds into the
//! keychain and rewrites the file stripped.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use red_core::{ConnectionConfig, DbKind};
use serde::{Deserialize, Serialize};

/// A saved connection plus a recency stamp for "recent" ordering. The in-memory
/// `config.password` is normally empty — it's materialized from the keychain only
/// transiently (connect / edit / test) and never held here long-term, so it can't
/// leak via a serialized [`StoredConnection`]. Serialization is deliberately *not*
/// derived; saving goes through the password-free [`WriteConnection`].
#[derive(Clone, Debug)]
pub struct StoredConnection {
    /// Stable per-connection identity, also the keychain account holding this
    /// connection's password. Generated on first save; back-filled on load for
    /// connections saved before ids existed.
    pub id: String,
    pub config: ConnectionConfig,
    /// Unix seconds of the last successful connect; `None` until first used.
    pub last_accessed: Option<u64>,
}

/// The on-disk shape used for *loading* — tolerant of both the current structured
/// fields and the legacy single-`dsn` format (and a legacy plaintext `password`),
/// so upgrading never drops a saved connection. Saving always writes the
/// password-free structured shape via [`WriteConnection`].
#[derive(Default, Deserialize)]
struct RawConnection {
    /// Stable id; absent in pre-keychain configs, back-filled on load.
    #[serde(default)]
    id: String,
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
    allow_edit: bool,
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
            allow_edit: self.allow_edit,
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
            id: self.id,
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

/// The on-disk shape used for *saving*. Deliberately omits `password` — that
/// lives in the keychain, keyed by `id` — so a write can never spill a secret to
/// disk. Mirrors the flat `[[connection]]` table the loader expects.
#[derive(Serialize)]
struct WriteConnection {
    id: String,
    name: String,
    kind: DbKind,
    host: String,
    port: Option<u16>,
    user: String,
    database: String,
    color: u8,
    read_only: bool,
    allow_edit: bool,
    last_accessed: Option<u64>,
}

impl From<&StoredConnection> for WriteConnection {
    fn from(s: &StoredConnection) -> Self {
        let c = &s.config;
        WriteConnection {
            id: s.id.clone(),
            name: c.name.clone(),
            kind: c.kind,
            host: c.host.clone(),
            port: c.port,
            user: c.user.clone(),
            database: c.database.clone(),
            color: c.color,
            read_only: c.read_only,
            allow_edit: c.allow_edit,
            last_accessed: s.last_accessed,
        }
    }
}

#[derive(Default, Serialize)]
struct ConfigFile {
    #[serde(rename = "connection")]
    connections: Vec<WriteConnection>,
}

/// Mint a new stable connection id. Monotonic-ish (nanosecond clock + a
/// process-local counter to break same-instant ties) — unique enough to key a
/// keychain entry for a local, single-user app.
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("conn-{nanos:x}-{seq:x}")
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
/// config yields an empty list (with a warning), never an error. On load we also
/// run a one-time [`migrate_secrets`] pass — assign ids to legacy entries and
/// move any plaintext password into the keychain — rewriting the stripped file if
/// anything changed.
pub fn load() -> Vec<StoredConnection> {
    let Some(path) = config_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let raw = match toml::from_str::<RawConfigFile>(&text) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("ignoring malformed config at {}: {e}", path.display());
            return Vec::new();
        }
    };
    let mut connections: Vec<StoredConnection> = raw
        .connections
        .into_iter()
        .map(RawConnection::into_stored)
        .collect();

    if migrate_secrets(&mut connections) {
        if let Err(e) = save(&connections) {
            tracing::warn!("failed to rewrite config after credential migration: {e}");
        }
    }

    connections.sort_by_key(|c| std::cmp::Reverse(c.last_accessed));
    connections
}

/// Bring loaded connections up to the keychain model: back-fill missing ids and
/// move any legacy plaintext passwords into the OS keychain. Returns `true` when
/// the on-disk file should be rewritten (stripped).
///
/// Password migration is **all-or-nothing**: if any keychain write fails (e.g.
/// the user denies access), we leave every password in memory *and* on disk
/// untouched and return `false`, so a transient keychain hiccup never drops a
/// credential. Secrets that did move are cleared from memory; the keychain is the
/// source of truth from then on.
fn migrate_secrets(connections: &mut [StoredConnection]) -> bool {
    let mut id_assigned = false;
    for c in connections.iter_mut() {
        if c.id.is_empty() {
            c.id = new_id();
            id_assigned = true;
        }
    }

    let with_password: Vec<usize> = connections
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.config.password.is_empty())
        .map(|(i, _)| i)
        .collect();
    if with_password.is_empty() {
        return id_assigned;
    }

    for &i in &with_password {
        if let Err(e) =
            crate::secrets::set_password(&connections[i].id, &connections[i].config.password)
        {
            tracing::warn!(
                "keychain unavailable; leaving credentials in the config file for now: {e}"
            );
            return false;
        }
    }
    for &i in &with_password {
        connections[i].config.password.clear();
    }
    true
}

/// Persist the connection list (without passwords — those live in the keychain).
/// Creates the config dir if needed.
pub fn save(connections: &[StoredConnection]) -> Result<()> {
    let path = config_path().context("no platform config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cfg = ConfigFile {
        connections: connections.iter().map(WriteConnection::from).collect(),
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
        let connections = [
            StoredConnection {
                id: "conn-a".into(),
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
                id: "conn-b".into(),
                config: ConnectionConfig {
                    name: "prod".into(),
                    kind: DbKind::Postgres,
                    host: "db".into(),
                    port: Some(5432),
                    user: "analytics".into(),
                    // Passwords belong in the keychain — saving must drop them.
                    password: "secret".into(),
                    database: "analytics".into(),
                    color: 3,
                    read_only: false,
                    allow_edit: false,
                },
                last_accessed: None,
            },
        ];

        let cfg = ConfigFile {
            connections: connections.iter().map(WriteConnection::from).collect(),
        };
        let text = toml::to_string_pretty(&cfg).expect("serialize");
        // The password must not appear anywhere in the serialized file.
        assert!(!text.contains("secret"), "password leaked into config file");

        let back: RawConfigFile = toml::from_str(&text).expect("deserialize");
        let back: Vec<StoredConnection> = back
            .connections
            .into_iter()
            .map(RawConnection::into_stored)
            .collect();

        assert_eq!(back.len(), 2);
        assert_eq!(back[0].id, "conn-a");
        assert_eq!(back[0].config.name, "local");
        assert_eq!(back[0].config.kind, DbKind::Sqlite);
        assert_eq!(back[0].config.database, "/tmp/app.db");
        assert!(back[0].config.read_only);
        assert_eq!(back[0].last_accessed, Some(1_700_000_000));
        assert_eq!(back[1].id, "conn-b");
        assert_eq!(back[1].config.host, "db");
        assert_eq!(back[1].config.user, "analytics");
        // Password is not persisted; it comes back empty.
        assert_eq!(back[1].config.password, "");
        assert_eq!(back[1].last_accessed, None);
    }

    #[test]
    fn new_id_is_unique() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert!(a.starts_with("conn-"));
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
