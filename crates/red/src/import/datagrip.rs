//! JetBrains DataGrip / IntelliJ connection import.
//!
//! DataGrip stores connection **metadata** in plaintext XML:
//! - global: `~/.config/JetBrains/DataGrip<ver>/options/dataSources.xml`
//! - per-project: `<project>/.idea/dataSources.xml`
//!
//! Both files hold `<data-source>` entries with a driver id and a JDBC URL
//! (host/port/database), but **not** the password: DataGrip keeps that in the OS
//! keychain or an encrypted KeePass `c.kdbx` behind a master password. Rather than
//! reproduce that store, we **degrade** exactly like DBeaver's master-password
//! branch: import the metadata and attach a `warning` so the user re-enters the
//! password after import, never failing the whole import for a missing secret.

use std::path::Path;

use anyhow::{Context, Result};
use red_core::{ConnectionConfig, DbKind};

use super::{ImportReport, ImportedConnection};

/// Import every `<data-source>` in a directory's `dataSources.xml` (an `options/`
/// dir for the global store, or a project `.idea/`). Unsupported engines are
/// skipped with a reason; supported ones import without their password + a warning.
pub fn import(dir: &Path) -> Result<ImportReport> {
    let path = dir.join("dataSources.xml");
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(parse(&raw))
}

/// Parse the XML text into a report. Kept separate from [`import`] so tests exercise
/// it without a file. A malformed document yields an empty report with one skip
/// note rather than an error, so one bad file can't abort a multi-source wizard.
fn parse(xml: &str) -> ImportReport {
    let mut report = ImportReport::default();
    let doc = match roxmltree::Document::parse(xml) {
        Ok(doc) => doc,
        Err(e) => {
            report
                .skipped
                .push(("dataSources.xml".into(), format!("not valid XML: {e}")));
            return report;
        }
    };

    for ds in doc.descendants().filter(|n| n.has_tag_name("data-source")) {
        let name = ds
            .attribute("name")
            .filter(|n| !n.is_empty())
            .unwrap_or("DataGrip connection")
            .to_string();
        let driver = child_text(ds, "driver-ref").unwrap_or_default();
        let jdbc = child_text(ds, "jdbc-url").unwrap_or_default();

        let Some(kind) = map_kind(&driver, &jdbc) else {
            let engine = if driver.is_empty() {
                jdbc.clone()
            } else {
                driver.clone()
            };
            report
                .skipped
                .push((name, format!("unsupported engine: {engine}")));
            continue;
        };

        let parsed = parse_jdbc(&jdbc, kind);
        // The `<user-name>` element wins over any user embedded in the URL.
        let user = child_text(ds, "user-name")
            .filter(|u| !u.is_empty())
            .unwrap_or(parsed.user);

        let warning = (!kind.is_file()).then(|| {
            "password not stored in this file (DataGrip keeps it in the OS keychain \
             or an encrypted store); re-enter it after import"
                .to_string()
        });

        report.imported.push(ImportedConnection {
            config: ConnectionConfig {
                name: name.clone(),
                kind,
                host: parsed.host,
                port: parsed.port.or_else(|| kind.default_port()),
                user,
                password: String::new(),
                database: parsed.database,
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
            warning,
        });
    }
    report
}

/// The text of the first direct child named `tag`, trimmed. `None` when absent.
fn child_text<'a>(node: roxmltree::Node<'a, 'a>, tag: &str) -> Option<String> {
    node.children()
        .find(|c| c.has_tag_name(tag))
        .and_then(|c| c.text())
        .map(|t| t.trim().to_string())
}

/// Map a DataGrip `driver-ref` (e.g. `postgresql`, `mysql.8`, `sqlite.xerial`,
/// `clickhouse`) onto a RED engine, falling back to the JDBC URL scheme when the
/// driver id is unfamiliar. Unknown engines yield `None` -> skipped.
fn map_kind(driver: &str, jdbc: &str) -> Option<DbKind> {
    let d = driver.to_ascii_lowercase();
    // Postgres-wire relatives (redshift/cockroach/greenplum) speak the protocol.
    let by_driver = if d.starts_with("postgres")
        || d.starts_with("redshift")
        || d.starts_with("cockroach")
        || d.starts_with("greenplum")
    {
        Some(DbKind::Postgres)
    } else if d.starts_with("mysql") || d.starts_with("mariadb") {
        Some(DbKind::Mysql)
    } else if d.contains("sqlite") {
        Some(DbKind::Sqlite)
    } else if d.starts_with("clickhouse") || d == "ch" {
        Some(DbKind::Clickhouse)
    } else {
        None
    };
    by_driver.or_else(|| jdbc_scheme(jdbc).and_then(|s| scheme_to_kind(&s)))
}

/// Map a JDBC scheme token onto an engine (the fallback when `driver-ref` is blank
/// or unfamiliar).
fn scheme_to_kind(scheme: &str) -> Option<DbKind> {
    match scheme {
        "postgresql" | "postgres" | "redshift" => Some(DbKind::Postgres),
        "mysql" | "mariadb" => Some(DbKind::Mysql),
        "sqlite" => Some(DbKind::Sqlite),
        "clickhouse" | "ch" => Some(DbKind::Clickhouse),
        _ => None,
    }
}

/// The scheme of a `jdbc:<scheme>:...` URL, lowercased.
fn jdbc_scheme(jdbc: &str) -> Option<String> {
    let rest = jdbc.strip_prefix("jdbc:")?;
    let scheme = rest.split([':', '/']).next()?;
    (!scheme.is_empty()).then(|| scheme.to_ascii_lowercase())
}

/// The pieces of a JDBC URL relevant to a RED connection.
#[derive(Default)]
struct Jdbc {
    host: String,
    port: Option<u16>,
    database: String,
    user: String,
}

/// Decompose a JDBC URL. File engines (`jdbc:sqlite:/path`) yield the bare path in
/// `database`; network URLs (`jdbc:postgresql://[user@]host:port/db?params`) yield
/// host/port/db and any embedded user. Best-effort: unrecognisable input yields an
/// empty [`Jdbc`], so the caller still imports a (host-less) stub the user can fix.
fn parse_jdbc(jdbc: &str, kind: DbKind) -> Jdbc {
    let Some(rest) = jdbc.strip_prefix("jdbc:") else {
        return Jdbc::default();
    };
    if kind.is_file() {
        // `sqlite:/path`, `sqlite:C:\path`, or `sqlite::memory:` -> everything after
        // the scheme is the file path.
        let path = rest.split_once(':').map(|x| x.1).unwrap_or("").trim();
        return Jdbc {
            database: path.to_string(),
            ..Default::default()
        };
    }
    // `scheme://authority/db?query` -> drop scheme + query, split authority/path.
    let Some((_, after_scheme)) = rest.split_once("://") else {
        return Jdbc::default();
    };
    let after_scheme = after_scheme
        .split(['?', ';'])
        .next()
        .unwrap_or(after_scheme);
    let (authority, database) = match after_scheme.split_once('/') {
        Some((a, d)) => (a, d.to_string()),
        None => (after_scheme, String::new()),
    };
    // authority = [user[:pass]@]host[:port]
    let (user, hostport) = match authority.rsplit_once('@') {
        Some((creds, hp)) => (creds.split(':').next().unwrap_or("").to_string(), hp),
        None => (String::new(), authority),
    };
    let (host, port) = split_host_port(hostport);
    Jdbc {
        host,
        port,
        database,
        user,
    }
}

/// Split `host[:port]`, honouring an IPv6 literal in brackets (`[::1]:5432`).
fn split_host_port(hostport: &str) -> (String, Option<u16>) {
    if let Some(rest) = hostport.strip_prefix('[') {
        // `[ipv6]` or `[ipv6]:port`
        if let Some((addr, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return (addr.to_string(), port);
        }
    }
    match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse().ok())
        }
        _ => (hostport.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<component name="DataSourceManagerImpl">
  <data-source source="LOCAL" name="PG prod" uuid="a">
    <driver-ref>postgresql</driver-ref>
    <jdbc-url>jdbc:postgresql://db.example.com:5432/app?ssl=true</jdbc-url>
    <user-name>appuser</user-name>
  </data-source>
  <data-source source="LOCAL" name="Maria" uuid="b">
    <driver-ref>mariadb</driver-ref>
    <jdbc-url>jdbc:mariadb://user@10.0.0.9/shop</jdbc-url>
  </data-source>
  <data-source source="LOCAL" name="Local file" uuid="c">
    <driver-ref>sqlite.xerial</driver-ref>
    <jdbc-url>jdbc:sqlite:/data/app.db</jdbc-url>
  </data-source>
  <data-source source="LOCAL" name="SQL Server" uuid="d">
    <driver-ref>sqlserver</driver-ref>
    <jdbc-url>jdbc:sqlserver://mssql;databaseName=x</jdbc-url>
  </data-source>
</component>"#;

    fn find<'a>(r: &'a ImportReport, name: &str) -> &'a ImportedConnection {
        r.imported
            .iter()
            .find(|c| c.source_name == name)
            .unwrap_or_else(|| panic!("no import named {name}"))
    }

    #[test]
    fn maps_postgres_metadata_and_warns_about_password() {
        let r = parse(SAMPLE);
        let pg = find(&r, "PG prod");
        assert_eq!(pg.config.kind, DbKind::Postgres);
        assert_eq!(pg.config.host, "db.example.com");
        assert_eq!(pg.config.port, Some(5432));
        assert_eq!(pg.config.database, "app");
        assert_eq!(pg.config.user, "appuser"); // <user-name> element
        assert!(pg.config.password.is_empty());
        assert!(
            pg.warning.is_some(),
            "degrade note for the missing password"
        );
    }

    #[test]
    fn reads_user_from_url_and_defaults_port() {
        let r = parse(SAMPLE);
        let maria = find(&r, "Maria");
        assert_eq!(maria.config.kind, DbKind::Mysql);
        assert_eq!(maria.config.host, "10.0.0.9");
        assert_eq!(maria.config.port, Some(3306)); // engine default (URL had none)
        assert_eq!(maria.config.user, "user"); // embedded in the URL
        assert_eq!(maria.config.database, "shop");
    }

    #[test]
    fn sqlite_path_and_no_password_warning() {
        let r = parse(SAMPLE);
        let lite = find(&r, "Local file");
        assert_eq!(lite.config.kind, DbKind::Sqlite);
        assert_eq!(lite.config.database, "/data/app.db");
        assert!(lite.config.host.is_empty());
        assert!(lite.warning.is_none(), "a file engine needs no password");
    }

    #[test]
    fn skips_unsupported_engine_with_reason() {
        let r = parse(SAMPLE);
        assert!(r.imported.iter().all(|c| c.source_name != "SQL Server"));
        let (name, reason) = r
            .skipped
            .iter()
            .find(|(n, _)| n == "SQL Server")
            .expect("mssql skipped");
        assert_eq!(name, "SQL Server");
        assert!(reason.contains("unsupported"));
    }

    #[test]
    fn malformed_xml_is_a_skip_not_an_error() {
        let r = parse("<not xml");
        assert!(r.imported.is_empty());
        assert_eq!(r.skipped.len(), 1);
    }
}
