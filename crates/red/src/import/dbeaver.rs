//! DBeaver connection import.
//!
//! DBeaver stores connections in `<workspace>/<project>/.dbeaver/`:
//! - `data-sources.json`: plaintext connection metadata (host/port/db/driver,
//!   SSH/SSL handlers), keyed by a stable connection id. **No secrets.**
//! - `credentials-config.json`: the usernames + passwords, encrypted, correlated
//!   by the same connection id.
//!
//! The credentials file is **AES-128-CBC / PKCS7** under a key hardcoded in
//! DBeaver's `DefaultSecureStorage.LOCAL_KEY_CACHE` (public; it ships in the
//! binary). The file is raw binary: the first 16 bytes are the IV (fresh per
//! write), the rest is ciphertext. When the user enabled a master password the
//! hardcoded key won't decrypt it; we degrade to importing without passwords
//! rather than failing the whole import.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use aes::Aes128;
use anyhow::{anyhow, bail, Context, Result};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use cbc::Decryptor;
use red_core::{ConnectionConfig, DbKind, SshAuth, SshConfig};
use serde::Deserialize;
use serde_json::Value as JsonValue;

use super::{ImportReport, ImportedConnection};

/// DBeaver's hardcoded AES-128 key (`DefaultSecureStorage.LOCAL_KEY_CACHE`), the
/// signed Java bytes reinterpreted as unsigned. Public knowledge; it is compiled
/// into every DBeaver build that uses this storage path.
const DBEAVER_KEY: [u8; 16] = [
    186, 187, 74, 159, 119, 74, 184, 83, 201, 108, 45, 101, 61, 254, 84, 74,
];

type Aes128CbcDec = Decryptor<Aes128>;

/// Import every connection under a `.dbeaver` directory.
pub fn import(dir: &Path) -> Result<ImportReport> {
    let ds_path = dir.join("data-sources.json");
    let raw =
        fs::read_to_string(&ds_path).with_context(|| format!("read {}", ds_path.display()))?;
    let file: DataSourcesFile =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", ds_path.display()))?;

    // Credentials are optional and may fail to decrypt (master password); either
    // way we still import the connections, just without their passwords.
    let creds = read_credentials(dir).unwrap_or_default();

    let mut report = ImportReport::default();
    for (id, conn) in &file.connections {
        let name = if conn.name.is_empty() {
            id.clone()
        } else {
            conn.name.clone()
        };

        let Some(kind) = map_kind(&conn.provider, &conn.driver) else {
            let engine = if !conn.provider.is_empty() {
                conn.provider.clone()
            } else {
                conn.driver.clone()
            };
            report
                .skipped
                .push((name, format!("unsupported engine: {engine}")));
            continue;
        };

        let node = creds.get(id);
        let db_cred = node.and_then(|m| m.get("#connection"));
        let user = db_cred.map(|c| c.user.clone()).unwrap_or_default();
        let password = db_cred.map(|c| c.password.clone()).unwrap_or_default();

        let cfg = &conn.configuration;
        let (host, port, database) = if kind.is_file() {
            (String::new(), None, sqlite_path(cfg))
        } else {
            (
                cfg.host.clone(),
                parse_port(&cfg.port, kind),
                cfg.database.clone(),
            )
        };

        let ssh = cfg
            .handlers
            .get("ssh_tunnel")
            .and_then(|h| map_ssh(h, node.and_then(|m| m.get("network/ssh_tunnel"))));

        let warning = if !kind.is_file() && conn.save_password && password.is_empty() {
            Some(
                "password unavailable (DBeaver master password or missing credentials store)"
                    .to_string(),
            )
        } else {
            None
        };

        report.imported.push(ImportedConnection {
            config: ConnectionConfig {
                name: name.clone(),
                kind,
                host,
                port,
                user,
                password,
                database,
                color: 0,
                read_only: conn.read_only,
                // TLS isn't extracted from the external store yet; a `rediss://`
                // etc. pasted later still sets it.
                tls: false,
                ai_enabled: None,
                ai_tier: None,
                ssh,
            },
            source_name: name,
            folder: conn.folder.clone(),
            warning,
        });
    }

    Ok(report)
}

/// Read + decrypt `credentials-config.json` into `{ conn-id: { node: {user,pw} } }`.
fn read_credentials(dir: &Path) -> Result<CredFile> {
    let path = dir.join("credentials-config.json");
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let json = decrypt_credentials(&bytes)?;
    serde_json::from_str(&json).context("parse decrypted credentials")
}

/// Decrypt the credentials blob to its UTF-8 JSON. Tolerant of the (rare) legacy
/// case where the file is already plaintext JSON.
fn decrypt_credentials(bytes: &[u8]) -> Result<String> {
    if let Ok(s) = std::str::from_utf8(bytes) {
        if s.trim_start().starts_with('{') {
            return Ok(s.to_string());
        }
    }
    if bytes.len() <= 16 {
        bail!("credentials file too short to hold an IV + ciphertext");
    }
    let (iv, ct) = bytes.split_at(16);
    let plain = Aes128CbcDec::new_from_slices(&DBEAVER_KEY, iv)
        .map_err(|e| anyhow!("bad AES key/IV length: {e}"))?
        .decrypt_padded_vec_mut::<Pkcs7>(ct)
        .map_err(|e| anyhow!("AES decrypt failed (master password?): {e}"))?;
    String::from_utf8(plain).context("decrypted credentials were not UTF-8")
}

/// Map DBeaver's `provider` (falling back to `driver` for old `generic`-parented
/// exports) onto a RED engine. Unknown engines yield `None` → skipped.
fn map_kind(provider: &str, driver: &str) -> Option<DbKind> {
    match provider.to_ascii_lowercase().as_str() {
        // PG-wire relatives (redshift/cockroach/greenplum/timescale/yugabyte) live
        // under the postgresql provider and speak the Postgres protocol.
        "postgresql" => Some(DbKind::Postgres),
        "mysql" => Some(DbKind::Mysql),
        "sqlite" => Some(DbKind::Sqlite),
        "clickhouse" => Some(DbKind::Clickhouse),
        "generic" => {
            let d = driver.to_ascii_lowercase();
            if d.contains("sqlite") || d.contains("libsql") {
                Some(DbKind::Sqlite)
            } else if d.contains("clickhouse") {
                Some(DbKind::Clickhouse)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Parse a DBeaver string port, falling back to the engine's default.
fn parse_port(port: &str, kind: DbKind) -> Option<u16> {
    let p = port.trim();
    if p.is_empty() {
        return kind.default_port();
    }
    p.parse::<u16>().ok().or_else(|| kind.default_port())
}

/// Extract the SQLite file path from the config: the explicit `database`, else the
/// JDBC url with its `jdbc:sqlite:` / `sqlite:` scheme stripped.
fn sqlite_path(cfg: &RawConfig) -> String {
    if !cfg.database.is_empty() {
        return cfg.database.clone();
    }
    let url = cfg.url.trim();
    for prefix in ["jdbc:sqlite:", "sqlite:"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    url.to_string()
}

/// Map an `ssh_tunnel` handler (+ its credential node) onto RED's [`SshConfig`].
fn map_ssh(handler: &RawHandler, cred: Option<&CredNode>) -> Option<SshConfig> {
    if !handler.enabled {
        return None;
    }
    let props = &handler.properties;
    let host = prop_str(props, "host");
    if host.is_empty() {
        return None;
    }
    let port = prop_str(props, "port").parse().unwrap_or(22);
    let key_path = prop_str(props, "keyPath");
    let user = if !handler.user.is_empty() {
        handler.user.clone()
    } else {
        cred.map(|c| c.user.clone()).unwrap_or_default()
    };
    let secret = cred.map(|c| c.password.clone()).unwrap_or_default();

    let (auth, password, passphrase) =
        match prop_str(props, "authType").to_ascii_uppercase().as_str() {
            "PUBLIC_KEY" => (SshAuth::Key { path: key_path }, String::new(), secret),
            "AGENT" => (SshAuth::Agent, String::new(), String::new()),
            // PASSWORD or unset.
            _ => (SshAuth::Password, secret, String::new()),
        };

    Some(SshConfig {
        host,
        port,
        user,
        auth,
        password,
        passphrase,
    })
}

/// A handler property as a string (values are usually JSON strings, but coerce
/// numbers/bools too rather than dropping them).
fn prop_str(props: &BTreeMap<String, JsonValue>, key: &str) -> String {
    match props.get(key) {
        Some(JsonValue::String(s)) => s.clone(),
        Some(JsonValue::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

// --- On-disk shapes (tolerant: unknown fields ignored, all fields optional) ---

#[derive(Deserialize, Default)]
struct DataSourcesFile {
    #[serde(default)]
    connections: BTreeMap<String, RawConnection>,
}

#[derive(Deserialize, Default)]
struct RawConnection {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    driver: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    folder: Option<String>,
    #[serde(rename = "save-password", default)]
    save_password: bool,
    #[serde(rename = "read-only", default)]
    read_only: bool,
    #[serde(default)]
    configuration: RawConfig,
}

#[derive(Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    host: String,
    #[serde(default)]
    port: String,
    #[serde(default)]
    database: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    handlers: BTreeMap<String, RawHandler>,
}

#[derive(Deserialize, Default)]
struct RawHandler {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    user: String,
    #[serde(default)]
    properties: BTreeMap<String, JsonValue>,
}

/// The decrypted credentials file: `conn-id -> node-name -> {user, password}`,
/// where node-name is `#connection` (the DB login) or `network/<handler>`.
type CredFile = BTreeMap<String, BTreeMap<String, CredNode>>;

#[derive(Deserialize, Default)]
struct CredNode {
    #[serde(default)]
    user: String,
    #[serde(default)]
    password: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/import/fixtures/dbeaver")
    }

    fn find<'a>(report: &'a ImportReport, name: &str) -> &'a ImportedConnection {
        report
            .imported
            .iter()
            .find(|c| c.source_name == name)
            .unwrap_or_else(|| panic!("no imported connection named {name}"))
    }

    #[test]
    fn decrypts_and_maps_postgres_with_password_and_ssh() {
        let report = import(&fixture_dir()).expect("import");
        let pg = find(&report, "PostgreSQL - prod");
        assert_eq!(pg.config.kind, DbKind::Postgres);
        assert_eq!(pg.config.host, "db.example.com");
        assert_eq!(pg.config.port, Some(5432));
        assert_eq!(pg.config.database, "app");
        assert_eq!(pg.config.user, "appuser");
        assert_eq!(pg.config.password, "s3cret");
        assert_eq!(pg.folder.as_deref(), Some("PG"));
        assert!(pg.config.read_only);
        assert!(pg.warning.is_none());

        let ssh = pg.config.ssh.as_ref().expect("ssh tunnel");
        assert_eq!(ssh.host, "bastion.example.com");
        assert_eq!(ssh.port, 2222);
        assert_eq!(ssh.user, "ec2-user");
        assert_eq!(
            ssh.auth,
            SshAuth::Key {
                path: "/home/me/.ssh/id_ed25519".into()
            }
        );
        assert_eq!(ssh.passphrase, "keypass");
    }

    #[test]
    fn maps_mysql_and_defaults_port() {
        let report = import(&fixture_dir()).expect("import");
        let my = find(&report, "MariaDB local");
        assert_eq!(my.config.kind, DbKind::Mysql);
        assert_eq!(my.config.port, Some(3306)); // blank port → engine default
        assert_eq!(my.config.user, "root");
        assert_eq!(my.config.password, "rootpw");
    }

    #[test]
    fn parses_sqlite_path_from_url() {
        let report = import(&fixture_dir()).expect("import");
        let lite = find(&report, "Local SQLite");
        assert_eq!(lite.config.kind, DbKind::Sqlite);
        assert_eq!(lite.config.database, "/data/app.db");
        assert!(lite.config.host.is_empty());
        assert!(lite.config.port.is_none());
    }

    #[test]
    fn skips_unsupported_engine_with_reason() {
        let report = import(&fixture_dir()).expect("import");
        assert!(
            report
                .imported
                .iter()
                .all(|c| c.source_name != "SQL Server"),
            "mssql must not import"
        );
        let (name, reason) = report
            .skipped
            .iter()
            .find(|(n, _)| n == "SQL Server")
            .expect("mssql in skip list");
        assert_eq!(name, "SQL Server");
        assert!(reason.contains("unsupported engine"), "reason: {reason}");
    }

    #[test]
    fn wrong_key_bytes_never_decrypt_to_the_password() {
        // Guards against silently accepting garbage: flipping the key must not
        // yield the known plaintext.
        let bytes = fs::read(fixture_dir().join("credentials-config.json")).unwrap();
        let mut bad = DBEAVER_KEY;
        bad[0] ^= 0xff;
        let (iv, ct) = bytes.split_at(16);
        let out = Aes128CbcDec::new_from_slices(&bad, iv)
            .unwrap()
            .decrypt_padded_vec_mut::<Pkcs7>(ct);
        // Either it fails to unpad, or it decrypts to something that isn't the
        // real credentials JSON.
        let ok = match out {
            Ok(p) => !String::from_utf8_lossy(&p).contains("s3cret"),
            Err(_) => true,
        };
        assert!(ok, "flipped key should not recover the password");
    }
}
