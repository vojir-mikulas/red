//! RedisInsight connection import.
//!
//! RedisInsight (v2) keeps its saved databases in a SQLite file,
//! `~/.redisinsight-v2/redisinsight.db`, table `database_instance`. The metadata
//! (name/host/port/db/username) is plain; the password is encrypted **only when**
//! the `encryption` column says so (`KEYTAR`, keyed by the OS keychain). We import
//! the metadata unconditionally and take the password only when it is stored in the
//! clear; a keychain-encrypted secret we can't reproduce here **degrades** to an
//! import-without-password plus a `warning`, never a failed import.

use std::path::Path;

use anyhow::{Context, Result};
use red_core::{ConnectionConfig, DbKind};
use rusqlite::{Connection, OpenFlags};

use super::{ImportReport, ImportedConnection};

/// The store file, relative to a `.redisinsight-v2` (or `.redisinsight-app`) dir.
const DB_FILE: &str = "redisinsight.db";

/// Whether `dir` holds a RedisInsight store (drives detection + "Browse..." check).
pub fn dir_has_store(dir: &Path) -> bool {
    dir.join(DB_FILE).is_file()
}

/// Import every row of `database_instance` from the RedisInsight SQLite store under
/// `dir`. Opens the file **read-only** so a running RedisInsight isn't disturbed.
pub fn import(dir: &Path) -> Result<ImportReport> {
    let path = dir.join(DB_FILE);
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", path.display()))?;
    read_report(&conn)
}

/// Read the report from an open connection. Split from [`import`] so a test can
/// hand it an in-memory database. Column presence varies across RedisInsight
/// versions, so rows are read by column *name* (missing columns just read as
/// empty) rather than a fixed positional schema.
fn read_report(conn: &Connection) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    let mut stmt = conn
        .prepare("SELECT * FROM database_instance")
        .context("query database_instance (unrecognised RedisInsight schema)")?;
    let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
    let index = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));

    let mut rows = stmt.query([]).context("read RedisInsight rows")?;
    while let Some(row) = rows.next().context("read RedisInsight row")? {
        let cell = |name: &str| -> String {
            index(name)
                .and_then(|i| row.get::<_, Option<String>>(i).ok().flatten())
                .unwrap_or_default()
        };
        let cell_u16 = |name: &str| -> Option<u16> {
            index(name)
                .and_then(|i| row.get::<_, Option<i64>>(i).ok().flatten())
                .and_then(|n| u16::try_from(n).ok())
        };
        // `tls` may be stored as an integer (0/1) or text ("true"); read the raw
        // value so a type-strict `get::<String>` doesn't silently drop an integer.
        let cell_bool = |name: &str| -> bool {
            index(name)
                .and_then(|i| row.get_ref(i).ok())
                .map(|v| match v {
                    rusqlite::types::ValueRef::Integer(n) => n != 0,
                    rusqlite::types::ValueRef::Text(t) => truthy(&String::from_utf8_lossy(t)),
                    _ => false,
                })
                .unwrap_or(false)
        };

        let name = {
            let n = cell("name");
            if n.is_empty() { cell("host") } else { n }
        };
        let host = cell("host");
        if host.is_empty() {
            report
                .skipped
                .push((name, "no host in the RedisInsight record".into()));
            continue;
        }

        // A keychain-encrypted password can't be recovered here; only a plaintext
        // (or unencrypted) one is carried over, otherwise we degrade with a note.
        let encryption = cell("encryption");
        let raw_password = cell("password");
        let plaintext = encryption.is_empty() || encryption.eq_ignore_ascii_case("PLAIN");
        let (password, warning) = if raw_password.is_empty() {
            (String::new(), None)
        } else if plaintext {
            (raw_password, None)
        } else {
            (
                String::new(),
                Some(
                    "password is encrypted in RedisInsight's keychain and can't be \
                     imported; re-enter it after import"
                        .to_string(),
                ),
            )
        };

        // `db` is Redis's logical database index (0..15); keep it as the database
        // segment RED threads through the DSN, like any other engine's database.
        let database = cell_u16("db").map(|n| n.to_string()).unwrap_or_default();
        let tls = cell_bool("tls");

        report.imported.push(ImportedConnection {
            config: ConnectionConfig {
                name: name.clone(),
                kind: DbKind::Redis,
                host,
                port: cell_u16("port").or_else(|| DbKind::Redis.default_port()),
                user: cell("username"),
                password,
                database,
                color: 0,
                read_only: false,
                tls,
                ai_enabled: None,
                ai_tier: None,
                ssh: None,
                proxy: None,
                sentinel_master: String::new(),
            },
            source_name: name,
            folder: None,
            warning,
        });
    }
    Ok(report)
}

/// Interpret a SQLite boolean-ish text/int cell (`1`, `true`) as `true`.
fn truthy(v: &str) -> bool {
    matches!(v.trim(), "1" | "true" | "TRUE" | "True")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `database_instance` table with the columns RedisInsight writes,
    /// seeded with a plaintext, a keychain-encrypted, and a TLS row.
    fn seed() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE database_instance (
                id TEXT, name TEXT, host TEXT, port INTEGER, db INTEGER,
                username TEXT, password TEXT, encryption TEXT, tls INTEGER
             );
             INSERT INTO database_instance VALUES
                ('1','cache','10.0.0.1',6380,3,'default','plainpw','PLAIN',0),
                ('2','prod','redis.example.com',6379,0,'','ENCRYPTEDBLOB','KEYTAR',1),
                ('3','',  'bare.example.com', 6379,0,'','','',0);",
        )
        .unwrap();
        conn
    }

    fn find<'a>(r: &'a ImportReport, name: &str) -> &'a ImportedConnection {
        r.imported
            .iter()
            .find(|c| c.source_name == name)
            .unwrap_or_else(|| panic!("no import named {name}"))
    }

    #[test]
    fn imports_plaintext_password_and_db_index() {
        let r = read_report(&seed()).unwrap();
        let cache = find(&r, "cache");
        assert_eq!(cache.config.kind, DbKind::Redis);
        assert_eq!(cache.config.host, "10.0.0.1");
        assert_eq!(cache.config.port, Some(6380));
        assert_eq!(cache.config.database, "3"); // logical db index
        assert_eq!(cache.config.user, "default");
        assert_eq!(cache.config.password, "plainpw");
        assert!(cache.warning.is_none());
    }

    #[test]
    fn degrades_keychain_encrypted_password() {
        let r = read_report(&seed()).unwrap();
        let prod = find(&r, "prod");
        assert!(prod.config.password.is_empty(), "encrypted pw not imported");
        assert!(prod.warning.is_some(), "degrade note attached");
        assert!(prod.config.tls, "tls flag carried over");
    }

    #[test]
    fn names_a_nameless_record_by_host() {
        let r = read_report(&seed()).unwrap();
        // Row 3 has an empty name, so it's named by host.
        find(&r, "bare.example.com");
    }

    #[test]
    fn unrecognised_schema_errors_cleanly() {
        let conn = Connection::open_in_memory().unwrap();
        // No `database_instance` table at all.
        assert!(read_report(&conn).is_err());
    }
}
