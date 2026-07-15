//! Plain credential files: `~/.pgpass`, `~/.my.cnf`, `~/.pg_service.conf`.
//!
//! Not GUI clients, but the cheapest importer and fully cross-platform: the files
//! are plaintext, so there is **no crypto and no keychain** to negotiate. Power
//! users keep credentials here, so this complements the GUI importers.
//!
//! Each file yields zero or more [`ImportedConnection`]s with their passwords
//! materialized directly (they are already in the clear on disk); the commit step
//! routes them into the keychain like any other import. A malformed line is
//! skipped with a reason rather than aborting the file.

use std::fs;
use std::path::Path;

use anyhow::Result;
use red_core::{ConnectionConfig, DbKind};

use super::{ImportReport, ImportedConnection};

/// The three files this importer understands, relative to the home directory.
const PGPASS: &str = ".pgpass";
const MY_CNF: &str = ".my.cnf";
const PG_SERVICE: &str = ".pg_service.conf";

/// Whether `dir` holds any of the three credential files (drives detection and the
/// "Browse..." validity check).
pub fn dir_has_any(dir: &Path) -> bool {
    [PGPASS, MY_CNF, PG_SERVICE]
        .iter()
        .any(|f| dir.join(f).is_file())
}

/// Import every credential file present under `dir` (a home directory). Absent
/// files are simply not read; each present file contributes to the merged report.
pub fn import(dir: &Path) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    if let Ok(text) = fs::read_to_string(dir.join(PGPASS)) {
        report.extend(parse_pgpass(&text));
    }
    if let Ok(text) = fs::read_to_string(dir.join(MY_CNF)) {
        report.extend(parse_my_cnf(&text));
    }
    if let Ok(text) = fs::read_to_string(dir.join(PG_SERVICE)) {
        report.extend(parse_pg_service(&text));
    }
    Ok(report)
}

/// Parse `.pgpass` lines: `host:port:database:user:password`, `*` a wildcard,
/// `\` an escape, `#` a comment. Each concrete line becomes a Postgres connection;
/// a wildcard host/port fills in `localhost` / the default port, and a wildcard
/// database maps to "all databases" (empty). A line without a real user is skipped
/// (it names no login to import).
fn parse_pgpass(text: &str) -> ImportReport {
    let mut report = ImportReport::default();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = split_escaped(line, ':');
        if fields.len() < 5 {
            report.skipped.push((
                format!(".pgpass line {}", n + 1),
                "expected 5 fields".into(),
            ));
            continue;
        }
        let host = wildcard(&fields[0]).unwrap_or_else(|| "localhost".to_string());
        let port = wildcard(&fields[1]).and_then(|p| p.parse::<u16>().ok());
        let database = wildcard(&fields[2]).unwrap_or_default();
        let Some(user) = wildcard(&fields[3]) else {
            report.skipped.push((
                format!(".pgpass line {}", n + 1),
                "wildcard user is not a concrete login".into(),
            ));
            continue;
        };
        let password = wildcard(&fields[4]).unwrap_or_default();
        let name = format!("{user}@{host}{}", db_suffix(&database));
        report.imported.push(pg_connection(
            name,
            host,
            port,
            user,
            password,
            database,
            DbKind::Postgres,
        ));
    }
    report
}

/// Parse `~/.my.cnf`: INI-style `[client]` / `[clientN]` / `[mysql]` sections whose
/// `host`/`port`/`user`/`password`/`database` keys form one MySQL connection each.
/// Sections that name no user *and* no host are skipped (nothing to connect to).
fn parse_my_cnf(text: &str) -> ImportReport {
    let mut report = ImportReport::default();
    for section in parse_ini(text) {
        let is_client = section.name == "client"
            || section.name == "mysql"
            || section.name.starts_with("client");
        if !is_client {
            continue;
        }
        let host = section
            .get("host")
            .unwrap_or_else(|| "localhost".to_string());
        let user = section.get("user").unwrap_or_default();
        if user.is_empty() && section.get("host").is_none() {
            continue;
        }
        let port = section.get("port").and_then(|p| p.parse::<u16>().ok());
        let password = section.get("password").unwrap_or_default();
        let database = section.get("database").unwrap_or_default();
        let name = format!("{}@{host}{}", user_or(&user), db_suffix(&database));
        report.imported.push(pg_connection(
            name,
            host,
            port,
            user,
            password,
            database,
            DbKind::Mysql,
        ));
    }
    report
}

/// Parse `~/.pg_service.conf`: named Postgres services (`[svc]` sections with
/// `host`/`port`/`dbname`/`user`/`password`). The service name is the connection
/// name, so a service resolves to a ready-to-dial Postgres connection.
fn parse_pg_service(text: &str) -> ImportReport {
    let mut report = ImportReport::default();
    for section in parse_ini(text) {
        let host = section
            .get("host")
            .unwrap_or_else(|| "localhost".to_string());
        let port = section.get("port").and_then(|p| p.parse::<u16>().ok());
        let user = section.get("user").unwrap_or_default();
        let password = section.get("password").unwrap_or_default();
        let database = section
            .get("dbname")
            .or_else(|| section.get("database"))
            .unwrap_or_default();
        report.imported.push(pg_connection(
            section.name.clone(),
            host,
            port,
            user,
            password,
            database,
            DbKind::Postgres,
        ));
    }
    report
}

/// Build one imported connection from plaintext fields, defaulting a missing port
/// to the engine default. The password is in the clear (it was on disk); the
/// commit step keychain-routes it. No `warning`: these files never degrade.
fn pg_connection(
    name: String,
    host: String,
    port: Option<u16>,
    user: String,
    password: String,
    database: String,
    kind: DbKind,
) -> ImportedConnection {
    ImportedConnection {
        config: ConnectionConfig {
            name: name.clone(),
            kind,
            host,
            port: port.or_else(|| kind.default_port()),
            user,
            password,
            database,
            color: 0,
            read_only: false,
            tls: false,
            ai_enabled: None,
            ai_tier: None,
            ssh: None,
            proxy: None,
            sentinel_master: String::new(),
        },
        source_name: name,
        folder: None,
        warning: None,
    }
}

/// A pgpass field is a wildcard (`*`) meaning "any" -> `None`; otherwise the
/// unescaped literal -> `Some`.
fn wildcard(field: &str) -> Option<String> {
    if field == "*" {
        None
    } else {
        Some(field.to_string())
    }
}

/// Split on `sep`, honouring pgpass's backslash escaping (`\:` is a literal colon,
/// `\\` a literal backslash), so a password containing the separator survives.
fn split_escaped(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => cur.push(chars.next().unwrap_or('\\')),
            c if c == sep => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// `/db` when a database is named, empty otherwise, for a readable default name.
fn db_suffix(database: &str) -> String {
    if database.is_empty() {
        String::new()
    } else {
        format!("/{database}")
    }
}

/// The user for a default name, or a placeholder when the file named none.
fn user_or(user: &str) -> &str {
    if user.is_empty() { "mysql" } else { user }
}

// --- tiny INI reader (std-only; a full crate would be overkill here) ---------

/// One `[name]` section and its `key = value` pairs, in file order.
struct IniSection {
    name: String,
    entries: Vec<(String, String)>,
}

impl IniSection {
    /// The first value for `key` (case-insensitive), unquoted and trimmed. `None`
    /// when the key is absent or its value is empty.
    fn get(&self, key: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    }
}

/// Parse INI text into sections. Tolerant: `#`/`;` comments, blank lines, and keys
/// before any section header (dropped) are all ignored; values are stripped of a
/// single layer of matching quotes so `password = "p:w"` round-trips.
fn parse_ini(text: &str) -> Vec<IniSection> {
    let mut sections: Vec<IniSection> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            sections.push(IniSection {
                name: name.trim().to_string(),
                entries: Vec::new(),
            });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if let Some(section) = sections.last_mut() {
            section
                .entries
                .push((key.trim().to_string(), unquote(value.trim()).to_string()));
        }
    }
    sections
}

/// Strip one layer of matching single or double quotes from an INI value.
fn unquote(v: &str) -> &str {
    let bytes = v.as_bytes();
    if v.len() >= 2 && (bytes[0] == b'"' || bytes[0] == b'\'') && bytes[bytes.len() - 1] == bytes[0]
    {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pgpass_maps_concrete_and_wildcard_lines() {
        let text = "\
# a comment
db.example.com:5432:app:appuser:s3cret
*:*:*:localdev:devpw
localhost:5433:analytics:reader:pa\\:ss";
        let report = parse_pgpass(text);
        assert_eq!(report.imported.len(), 3);

        let first = &report.imported[0].config;
        assert_eq!(first.kind, DbKind::Postgres);
        assert_eq!(first.host, "db.example.com");
        assert_eq!(first.port, Some(5432));
        assert_eq!(first.database, "app");
        assert_eq!(first.user, "appuser");
        assert_eq!(first.password, "s3cret");

        // Wildcard host/port/db fall back; the concrete user/password survive.
        let second = &report.imported[1].config;
        assert_eq!(second.host, "localhost");
        assert_eq!(second.port, Some(5432)); // default, not "*"
        assert_eq!(second.database, "");
        assert_eq!(second.user, "localdev");

        // Escaped colon in the password is preserved, not split into a 6th field.
        let third = &report.imported[2].config;
        assert_eq!(third.password, "pa:ss");
    }

    #[test]
    fn pgpass_skips_short_and_wildcard_user_lines() {
        let report = parse_pgpass("too:few:fields\n*:*:*:*:pw");
        assert!(report.imported.is_empty());
        assert_eq!(report.skipped.len(), 2);
    }

    #[test]
    fn my_cnf_reads_client_section() {
        let text = "\
[client]
host = 10.0.0.5
port = 3307
user = root
password = \"root:pw\"
database = shop
[mysqldump]
user = ignored";
        let report = parse_my_cnf(text);
        assert_eq!(report.imported.len(), 1, "only [client], not [mysqldump]");
        let c = &report.imported[0].config;
        assert_eq!(c.kind, DbKind::Mysql);
        assert_eq!(c.host, "10.0.0.5");
        assert_eq!(c.port, Some(3307));
        assert_eq!(c.user, "root");
        assert_eq!(c.password, "root:pw"); // quotes stripped
        assert_eq!(c.database, "shop");
    }

    #[test]
    fn pg_service_uses_service_name_and_dbname() {
        let text = "\
[prod]
host=pg.internal
port=5432
dbname=orders
user=svc
password=svcpw";
        let report = parse_pg_service(text);
        assert_eq!(report.imported.len(), 1);
        let c = &report.imported[0].config;
        assert_eq!(c.name, "prod");
        assert_eq!(c.database, "orders");
        assert_eq!(c.user, "svc");
        assert_eq!(c.kind, DbKind::Postgres);
    }

    #[test]
    fn import_merges_present_files_only() {
        let dir = std::env::temp_dir().join(format!("red-plain-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PGPASS), "h:5432:d:u:p").unwrap();
        // No .my.cnf / .pg_service.conf: those simply contribute nothing.
        let report = import(&dir).unwrap();
        assert_eq!(report.imported.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
