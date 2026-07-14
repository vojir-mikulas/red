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
use red_core::{AiTier, ConnectionConfig, DbKind, SshAuth, SshConfig};
use serde::{Deserialize, Serialize};

/// A saved connection plus a recency stamp for "recent" ordering. The in-memory
/// `config.password` is normally empty: it's materialized from the keychain only
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
    /// Pinned to the top of the welcome list and the connection switcher,
    /// independent of recency, so a user's favourite "projects" stay reachable on
    /// the low ⌘-digit slots no matter what they touched last. Defaults to false;
    /// omitted from the file when unset so pre-pin configs round-trip unchanged.
    pub pinned: bool,
}

/// The on-disk shape used for *loading*, tolerant of both the current structured
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
    /// Encrypt with TLS (`rediss`/HTTPS/`require_ssl`). Absent in pre-TLS
    /// configs, where it defaults to off.
    #[serde(default)]
    tls: bool,
    /// Optional per-connection AI master-switch override (M-S7). Absent inherits
    /// the global `[ai] enabled`.
    #[serde(default)]
    ai_enabled: Option<bool>,
    /// Optional per-connection AI access-tier override (M-S7): `off`/`schema`/
    /// `read`. Absent inherits the global `[ai] tier`.
    #[serde(default)]
    ai_tier: Option<AiTier>,
    #[serde(default)]
    last_accessed: Option<u64>,
    /// Pin this connection to the top of the lists (favourite). Absent in pre-pin
    /// configs, where it defaults to unpinned.
    #[serde(default)]
    pinned: bool,
    /// Optional SSH jump host. Absent in pre-SSH configs; secrets are never here.
    #[serde(default)]
    ssh: Option<RawSsh>,
}

/// On-disk SSH config: flat scalars (no enum) so it round-trips through TOML
/// cleanly as a `[connection.ssh]` sub-table. Secrets are never stored here;
/// they live in the keychain keyed by the connection id.
#[derive(Default, Deserialize)]
struct RawSsh {
    #[serde(default)]
    host: String,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    user: String,
    /// `agent` (default) | `password` | `key`.
    #[serde(default)]
    auth: String,
    #[serde(default)]
    key_path: String,
}

impl RawSsh {
    /// Decode into an [`SshConfig`], or `None` when no jump host is named (an
    /// empty `host` means "no tunnel", however the rest of the table looks).
    fn into_config(self) -> Option<SshConfig> {
        if self.host.trim().is_empty() {
            return None;
        }
        let auth = match self.auth.as_str() {
            "password" => SshAuth::Password,
            "key" => SshAuth::Key {
                path: self.key_path,
            },
            _ => SshAuth::Agent,
        };
        Some(SshConfig {
            host: self.host,
            port: if self.port == 0 { 22 } else { self.port },
            user: self.user,
            auth,
            password: String::new(),
            passphrase: String::new(),
        })
    }
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
            tls: self.tls,
            ai_enabled: self.ai_enabled,
            ai_tier: self.ai_tier,
            ssh: self.ssh.and_then(RawSsh::into_config),
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
                    config.tls = p.tls;
                } else {
                    config.database = dsn;
                }
            }
        }
        StoredConnection {
            id: self.id,
            config,
            last_accessed: self.last_accessed,
            pinned: self.pinned,
        }
    }
}

#[derive(Default, Deserialize)]
struct RawConfigFile {
    #[serde(default, rename = "connection")]
    connections: Vec<RawConnection>,
}

/// The on-disk shape used for *saving*. Deliberately omits `password` (that
/// lives in the keychain, keyed by `id`) so a write can never spill a secret to
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
    /// TLS toggle; omitted when off so pre-TLS files round-trip byte-for-byte.
    #[serde(skip_serializing_if = "is_false")]
    tls: bool,
    /// Per-connection AI overrides (M-S7); omitted when unset so they inherit the
    /// global `[ai]` policy and pre-M-S7 files round-trip unchanged. Scalar keys,
    /// so they stay ahead of the `ssh` sub-table.
    #[serde(skip_serializing_if = "Option::is_none")]
    ai_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ai_tier: Option<AiTier>,
    last_accessed: Option<u64>,
    /// Omitted when unpinned so the common case adds no noise and pre-pin files
    /// round-trip byte-for-byte.
    #[serde(skip_serializing_if = "is_false")]
    pinned: bool,
    /// Optional `[connection.ssh]` sub-table. Must stay the **last** field: TOML
    /// requires a table-valued key to follow all of a struct's scalar keys.
    #[serde(skip_serializing_if = "Option::is_none")]
    ssh: Option<WriteSsh>,
}

/// On-disk SSH config (save side): mirror of [`RawSsh`], password-free. `auth`
/// is a plain string so the table needs no TOML enum gymnastics.
#[derive(Serialize)]
struct WriteSsh {
    host: String,
    port: u16,
    user: String,
    auth: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    key_path: String,
}

impl WriteSsh {
    fn from_config(s: &SshConfig) -> Self {
        let (auth, key_path) = match &s.auth {
            SshAuth::Agent => ("agent", String::new()),
            SshAuth::Password => ("password", String::new()),
            SshAuth::Key { path } => ("key", path.clone()),
        };
        WriteSsh {
            host: s.host.clone(),
            port: s.port,
            user: s.user.clone(),
            auth,
            key_path,
        }
    }
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
            tls: c.tls,
            ai_enabled: c.ai_enabled,
            ai_tier: c.ai_tier,
            last_accessed: s.last_accessed,
            pinned: s.pinned,
            ssh: c.ssh.as_ref().map(WriteSsh::from_config),
        }
    }
}

/// `skip_serializing_if` predicate for the boolean `pinned` flag; `serde` needs a
/// `fn(&bool) -> bool`, which `std::ops::Not::not` (taking `bool` by value) isn't.
fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Default, Serialize)]
struct ConfigFile {
    #[serde(rename = "connection")]
    connections: Vec<WriteConnection>,
}

/// Mint a new stable connection id. Monotonic-ish (nanosecond clock + a
/// process-local counter to break same-instant ties), unique enough to key a
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

/// The path to the saved-connections file (`connections.toml`), for the loader,
/// the atomic save, and the file-first "edit connections" workflow. `None` when
/// the platform has no config directory.
pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("red").join("connections.toml"))
}

/// Load the saved connections, newest-used first. Missing/unreadable/malformed
/// config yields an empty list (with a warning), never an error. On load we also
/// run a one-time [`migrate_secrets`] pass (assign ids to legacy entries and
/// move any plaintext password into the keychain), rewriting the stripped file if
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

/// Persist the connection list. Normally password-free (secrets live in the
/// keychain), but a legacy file whose keychain migration failed can still hold
/// plaintext passwords ([`migrate_secrets`]), so the file is written owner-only on
/// Unix and atomically (temp + rename), never leaving a world-readable credential
/// or a half-written file. Creates the config dir if needed.
pub fn save(connections: &[StoredConnection]) -> Result<()> {
    use std::io::Write;

    let path = config_path().context("no platform config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = serialize(connections)?;
    let tmp = path.with_extension(format!("toml.tmp.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp)
        .context("creating the connections temp file")?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &path).context("renaming the connections temp file")?;
    Ok(())
}

/// Render the connection list to the exact TOML bytes [`save`] writes, so a
/// caller can hash them ahead of a save to suppress the watcher's self-reload.
pub fn serialize(connections: &[StoredConnection]) -> Result<String> {
    let cfg = ConfigFile {
        connections: connections.iter().map(WriteConnection::from).collect(),
    };
    Ok(toml::to_string_pretty(&cfg)?)
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
                // A pinned connection must round-trip (conn-b stays unpinned, so
                // its flag is omitted from the file and defaults back to false).
                pinned: true,
            },
            StoredConnection {
                id: "conn-b".into(),
                config: ConnectionConfig {
                    name: "prod".into(),
                    kind: DbKind::Postgres,
                    host: "db".into(),
                    port: Some(5432),
                    user: "analytics".into(),
                    // Passwords belong in the keychain; saving must drop them.
                    password: "secret".into(),
                    database: "analytics".into(),
                    color: 3,
                    read_only: false,
                    tls: false,
                    // Per-connection AI overrides (M-S7) must round-trip; conn-a
                    // leaves them unset to confirm they're omitted and inherited.
                    ai_enabled: Some(false),
                    ai_tier: Some(AiTier::Schema),
                    ssh: None,
                },
                last_accessed: None,
                pinned: false,
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
        // Per-connection AI overrides round-trip; an unset one stays inherited.
        assert_eq!(back[0].config.ai_enabled, None);
        assert_eq!(back[0].config.ai_tier, None);
        assert_eq!(back[1].config.ai_enabled, Some(false));
        assert_eq!(back[1].config.ai_tier, Some(AiTier::Schema));
        // The pinned flag round-trips; an unpinned connection omits it and reads
        // back false.
        assert!(back[0].pinned);
        assert!(!back[1].pinned);
    }

    #[test]
    fn ssh_config_round_trips_without_secrets() {
        let connections = [StoredConnection {
            id: "conn-ssh".into(),
            config: ConnectionConfig {
                name: "tunneled".into(),
                kind: DbKind::Postgres,
                host: "10.0.0.5".into(),
                port: Some(5432),
                user: "analytics".into(),
                database: "shop".into(),
                ssh: Some(SshConfig {
                    host: "bastion.example.com".into(),
                    port: 2222,
                    user: "jump".into(),
                    auth: SshAuth::Key {
                        path: "/home/me/.ssh/id_ed25519".into(),
                    },
                    // Secrets must never reach the file.
                    password: "should-not-persist".into(),
                    passphrase: "neither-should-this".into(),
                }),
                ..Default::default()
            },
            last_accessed: None,
            pinned: false,
        }];

        let cfg = ConfigFile {
            connections: connections.iter().map(WriteConnection::from).collect(),
        };
        let text = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !text.contains("should-not-persist") && !text.contains("neither-should-this"),
            "ssh secret leaked into config file"
        );

        let back: RawConfigFile = toml::from_str(&text).expect("deserialize");
        let back: Vec<StoredConnection> = back
            .connections
            .into_iter()
            .map(RawConnection::into_stored)
            .collect();
        let ssh = back[0]
            .config
            .ssh
            .as_ref()
            .expect("ssh survives round-trip");
        assert_eq!(ssh.host, "bastion.example.com");
        assert_eq!(ssh.port, 2222);
        assert_eq!(ssh.user, "jump");
        assert_eq!(
            ssh.auth,
            SshAuth::Key {
                path: "/home/me/.ssh/id_ed25519".into()
            }
        );
        // Secrets come back empty: they belong in the keychain.
        assert_eq!(ssh.password, "");
        assert_eq!(ssh.passphrase, "");
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
