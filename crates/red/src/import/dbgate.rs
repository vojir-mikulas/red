//! DBGate connection import.
//!
//! DBGate stores connections in `~/.dbgate/`:
//! - `connections.jsonl`: one JSON object per line (folders are lines too,
//!   distinguished by carrying no `engine`).
//! - `.key`: the per-install encryption key, itself encrypted with a hardcoded
//!   default key.
//!
//! Secrets use node's `simple-encryptor`: for a key string `K`,
//! `cryptoKey = SHA256(K)`, then **AES-256-CBC / PKCS7** with a random 16-byte IV,
//! and an **HMAC-SHA256** (keyed with the same `cryptoKey`) over `ivHex ‖ ctBase64`.
//! The stored token is `hmac(64 hex) ‖ iv(32 hex) ‖ ciphertext(base64)`, and the
//! plaintext is `JSON.stringify(value)` (so a password comes back JSON-quoted).
//! Connection fields carry a literal `crypt:` marker; a value without it is stored
//! raw (`passwordMode: "saveRaw"`).
//!
//! Two-level key: `.key` is decrypted with the hardcoded default key to yield a
//! random per-install `encryptionKey`, and *that* decrypts the connection fields.

use std::fs;
use std::path::Path;

use aes::Aes256;
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use cbc::Decryptor;
use hmac::{Hmac, Mac};
use red_core::{ConnectionConfig, DbKind, SshAuth, SshConfig};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use super::{ImportReport, ImportedConnection};

/// DBGate's hardcoded default key, used *only* to decrypt the `.key` file, which
/// holds the real per-install key. Public knowledge (compiled into the app).
const DBGATE_DEFAULT_KEY: &str = "mQAUaXhavRGJDxDTXSCg7Ej0xMmGCrx6OKA07DIMBiDcYYkvkaXjTAzPUEHEHEf9";

type Aes256CbcDec = Decryptor<Aes256>;
type HmacSha256 = Hmac<Sha256>;

/// Import every connection under a `.dbgate` directory.
pub fn import(dir: &Path) -> Result<ImportReport> {
    let conn_path = dir.join("connections.jsonl");
    let raw =
        fs::read_to_string(&conn_path).with_context(|| format!("read {}", conn_path.display()))?;

    // The `.key` file exists only once something was encrypted; its absence just
    // means there are no encrypted secrets to recover.
    let enc_key = load_encryption_key(dir).ok();

    let mut report = ImportReport::default();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let conn: RawConn = match serde_json::from_str(line) {
            Ok(c) => c,
            Err(e) => {
                report
                    .skipped
                    .push(("<unparseable line>".into(), format!("invalid JSON: {e}")));
                continue;
            }
        };
        // Folder / group rows carry no engine, not connections.
        if conn.engine.trim().is_empty() {
            continue;
        }

        let name = conn.display();
        let Some(kind) = map_engine(&conn.engine) else {
            report
                .skipped
                .push((name, format!("unsupported engine: {}", conn.engine)));
            continue;
        };

        let mut warnings: Vec<String> = Vec::new();

        let (host, port, database) = if kind.is_file() {
            (String::new(), None, json_str(&conn.database_file))
        } else {
            (
                conn.server.clone(),
                json_port(&conn.port, kind),
                conn.default_database.clone(),
            )
        };

        let (password, pw_warn) = resolve_secret(&conn.password, enc_key.as_deref());
        if let Some(w) = pw_warn {
            warnings.push(w.into());
        }

        let ssh = map_ssh(&conn, enc_key.as_deref(), &mut warnings);

        report.imported.push(ImportedConnection {
            config: ConnectionConfig {
                name: name.clone(),
                kind,
                host,
                port,
                user: conn.user.clone(),
                password,
                database,
                color: 0,
                read_only: false,
                // TLS isn't extracted from the external store yet.
                tls: false,
                ai_enabled: None,
                ai_tier: None,
                ssh,
            },
            source_name: name,
            folder: None,
            warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        });
    }

    Ok(report)
}

/// Decrypt `.key` with the default key → the random per-install `encryptionKey`.
fn load_encryption_key(dir: &Path) -> Result<String> {
    let key_path = dir.join(".key");
    let token =
        fs::read_to_string(&key_path).with_context(|| format!("read {}", key_path.display()))?;
    let json = simple_decrypt(DBGATE_DEFAULT_KEY, token.trim())?;
    let parsed: KeyFile = serde_json::from_str(&json).context("parse .key contents")?;
    Ok(parsed.encryption_key)
}

/// Resolve a possibly-encrypted field. `crypt:`-prefixed values are decrypted;
/// anything else is a plaintext `saveRaw` value. Returns the value plus a warning
/// when an encrypted value couldn't be recovered.
fn resolve_secret(value: &str, enc_key: Option<&str>) -> (String, Option<&'static str>) {
    if value.is_empty() {
        return (String::new(), None);
    }
    let Some(token) = value.strip_prefix("crypt:") else {
        return (value.to_string(), None);
    };
    match enc_key {
        Some(k) => match simple_decrypt(k, token) {
            Ok(json) => (unwrap_json_string(json), None),
            Err(_) => (
                String::new(),
                Some("encrypted secret could not be decrypted"),
            ),
        },
        None => (
            String::new(),
            Some("encrypted secret but no DBGate .key file"),
        ),
    }
}

/// simple-encryptor plaintext is `JSON.stringify(value)`; a string value comes
/// back JSON-quoted, so decode it. Non-string plaintext is returned verbatim.
fn unwrap_json_string(json: String) -> String {
    serde_json::from_str::<String>(&json).unwrap_or(json)
}

/// Decrypt one `simple-encryptor` token with key string `key`. Returns the raw
/// (still JSON-encoded) plaintext.
fn simple_decrypt(key: &str, token: &str) -> Result<String> {
    let crypto_key = Sha256::digest(key.as_bytes());
    let token = token.trim();
    // Default (hmac on): [hmac 64 hex][iv 32 hex][ciphertext base64].
    if token.len() < 64 + 32 {
        bail!("token too short for hmac + iv");
    }
    let (hmac_hex, rest) = token.split_at(64);
    let expected = hex::decode(hmac_hex).context("decode hmac hex")?;
    let mut mac =
        HmacSha256::new_from_slice(crypto_key.as_slice()).expect("hmac accepts any key len");
    mac.update(rest.as_bytes());
    mac.verify_slice(&expected)
        .map_err(|_| anyhow!("HMAC verification failed (wrong key?)"))?;

    let (iv_hex, ct_b64) = rest.split_at(32);
    let iv = hex::decode(iv_hex).context("decode iv hex")?;
    let ct = BASE64.decode(ct_b64).context("decode ciphertext base64")?;
    let plain = Aes256CbcDec::new_from_slices(crypto_key.as_slice(), &iv)
        .map_err(|e| anyhow!("bad AES key/IV length: {e}"))?
        .decrypt_padded_vec_mut::<Pkcs7>(&ct)
        .map_err(|e| anyhow!("AES decrypt failed: {e}"))?;
    String::from_utf8(plain).context("plaintext not UTF-8")
}

/// Map a DBGate `engine` string (`"<engine>@<plugin>"`) onto a RED engine.
fn map_engine(engine: &str) -> Option<DbKind> {
    let base = engine
        .split('@')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        // cockroach/redshift speak the Postgres wire protocol.
        "postgres" | "cockroach" | "redshift" => Some(DbKind::Postgres),
        "mysql" | "mariadb" => Some(DbKind::Mysql),
        "sqlite" => Some(DbKind::Sqlite),
        "clickhouse" => Some(DbKind::Clickhouse),
        _ => None,
    }
}

/// Build an [`SshConfig`] when the connection tunnels; pushes any secret-recovery
/// caveats onto `warnings`.
fn map_ssh(conn: &RawConn, enc_key: Option<&str>, warnings: &mut Vec<String>) -> Option<SshConfig> {
    if !conn.use_ssh_tunnel || conn.ssh_host.trim().is_empty() {
        return None;
    }
    let (ssh_password, w1) = resolve_secret(&conn.ssh_password, enc_key);
    let (key_passphrase, w2) = resolve_secret(&conn.ssh_keyfile_password, enc_key);
    for w in [w1, w2].into_iter().flatten() {
        warnings.push(format!("ssh: {w}"));
    }

    let (auth, password, passphrase) = match conn.ssh_mode.as_str() {
        "keyFile" => (
            SshAuth::Key {
                path: conn.ssh_keyfile.clone(),
            },
            String::new(),
            key_passphrase,
        ),
        "agent" => (SshAuth::Agent, String::new(), String::new()),
        // "userPassword" or unset.
        _ => (SshAuth::Password, ssh_password, String::new()),
    };

    Some(SshConfig {
        host: conn.ssh_host.clone(),
        port: ssh_port(&conn.ssh_port),
        user: conn.ssh_login.clone(),
        auth,
        password,
        passphrase,
    })
}

/// A JSON value (string or number) as a string; anything else → empty.
fn json_str(v: &JsonValue) -> String {
    match v {
        JsonValue::String(s) => s.clone(),
        JsonValue::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// Parse a DBGate port (stored as string or number), defaulting to the engine's.
fn json_port(v: &JsonValue, kind: DbKind) -> Option<u16> {
    match v {
        JsonValue::String(s) if !s.trim().is_empty() => {
            s.trim().parse().ok().or_else(|| kind.default_port())
        }
        JsonValue::Number(n) => n
            .as_u64()
            .and_then(|x| u16::try_from(x).ok())
            .or_else(|| kind.default_port()),
        _ => kind.default_port(),
    }
}

/// SSH port (string or number), defaulting to 22, never an engine DB port.
fn ssh_port(v: &JsonValue) -> u16 {
    match v {
        JsonValue::String(s) => s.trim().parse().unwrap_or(22),
        JsonValue::Number(n) => n.as_u64().and_then(|x| u16::try_from(x).ok()).unwrap_or(22),
        _ => 22,
    }
}

#[derive(Deserialize)]
struct KeyFile {
    #[serde(rename = "encryptionKey")]
    encryption_key: String,
}

// --- On-disk shape (tolerant: unknown fields ignored, all fields optional) ---

#[derive(Deserialize, Default)]
struct RawConn {
    #[serde(default, rename = "_id")]
    id: String,
    #[serde(default)]
    engine: String,
    #[serde(default, rename = "displayName")]
    display_name: String,
    #[serde(default)]
    server: String,
    #[serde(default)]
    port: JsonValue,
    #[serde(default)]
    user: String,
    #[serde(default)]
    password: String,
    #[serde(default, rename = "defaultDatabase")]
    default_database: String,
    #[serde(default, rename = "databaseFile")]
    database_file: JsonValue,
    #[serde(default, rename = "useSshTunnel")]
    use_ssh_tunnel: bool,
    #[serde(default, rename = "sshHost")]
    ssh_host: String,
    #[serde(default, rename = "sshPort")]
    ssh_port: JsonValue,
    #[serde(default, rename = "sshLogin")]
    ssh_login: String,
    #[serde(default, rename = "sshMode")]
    ssh_mode: String,
    #[serde(default, rename = "sshKeyfile")]
    ssh_keyfile: String,
    #[serde(default, rename = "sshPassword")]
    ssh_password: String,
    #[serde(default, rename = "sshKeyfilePassword")]
    ssh_keyfile_password: String,
}

impl RawConn {
    /// Best label: explicit display name, else server, else id.
    fn display(&self) -> String {
        for candidate in [&self.display_name, &self.server, &self.id] {
            if !candidate.trim().is_empty() {
                return candidate.clone();
            }
        }
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/import/fixtures/dbgate")
    }

    fn find<'a>(report: &'a ImportReport, name: &str) -> &'a ImportedConnection {
        report
            .imported
            .iter()
            .find(|c| c.source_name == name)
            .unwrap_or_else(|| panic!("no imported connection named {name}"))
    }

    #[test]
    fn decrypts_postgres_password_via_two_level_key() {
        let report = import(&fixture_dir()).expect("import");
        let pg = find(&report, "Prod Postgres");
        assert_eq!(pg.config.kind, DbKind::Postgres);
        assert_eq!(pg.config.host, "db.example.com");
        assert_eq!(pg.config.port, Some(5432));
        assert_eq!(pg.config.database, "appdb");
        assert_eq!(pg.config.user, "app_user");
        assert_eq!(pg.config.password, "pgsecret");
        assert!(pg.warning.is_none(), "warning: {:?}", pg.warning);
    }

    #[test]
    fn maps_mariadb_engine_string() {
        let report = import(&fixture_dir()).expect("import");
        let my = find(&report, "Maria");
        assert_eq!(my.config.kind, DbKind::Mysql);
        assert_eq!(my.config.port, Some(3306));
    }

    #[test]
    fn honours_saveraw_plaintext_password() {
        let report = import(&fixture_dir()).expect("import");
        let raw = find(&report, "Raw Pw");
        assert_eq!(raw.config.password, "plainpw");
        assert!(raw.warning.is_none());
    }

    #[test]
    fn maps_sqlite_database_file() {
        let report = import(&fixture_dir()).expect("import");
        let lite = find(&report, "Local SQLite");
        assert_eq!(lite.config.kind, DbKind::Sqlite);
        assert_eq!(lite.config.database, "/data/app.sqlite");
        assert!(lite.config.port.is_none());
    }

    #[test]
    fn maps_ssh_keyfile_with_passphrase() {
        let report = import(&fixture_dir()).expect("import");
        let pg = find(&report, "Prod Postgres");
        let ssh = pg.config.ssh.as_ref().expect("ssh tunnel");
        assert_eq!(ssh.host, "bastion.example.com");
        assert_eq!(ssh.port, 2222);
        assert_eq!(ssh.user, "ec2-user");
        assert_eq!(
            ssh.auth,
            SshAuth::Key {
                path: "/home/me/.ssh/id_rsa".into()
            }
        );
        assert_eq!(ssh.passphrase, "keyphrase");
    }

    #[test]
    fn skips_unsupported_engine_with_reason() {
        let report = import(&fixture_dir()).expect("import");
        assert!(report
            .imported
            .iter()
            .all(|c| c.source_name != "SQL Server"));
        let (_, reason) = report
            .skipped
            .iter()
            .find(|(n, _)| n == "SQL Server")
            .expect("mssql in skip list");
        assert!(reason.contains("unsupported engine"), "reason: {reason}");
    }

    #[test]
    fn wrong_key_fails_hmac() {
        // A token decrypted with the wrong key must be rejected by the HMAC, not
        // silently produce garbage plaintext.
        let enc = load_encryption_key(&fixture_dir()).expect("key");
        let raw = fs::read_to_string(fixture_dir().join("connections.jsonl")).unwrap();
        let line = raw.lines().find(|l| l.contains("Prod Postgres")).unwrap();
        let conn: RawConn = serde_json::from_str(line).unwrap();
        let token = conn.password.strip_prefix("crypt:").unwrap();
        // Right key works, a mutated key fails.
        assert!(simple_decrypt(&enc, token).is_ok());
        let mut bad = enc.clone();
        bad.push('x');
        assert!(simple_decrypt(&bad, token).is_err());
    }
}
