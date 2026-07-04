//! Dialing a connection off the dispatch loop: the timeout wrapper, error
//! classification (fatal vs. transient, unknown-SSH-host prompt), and the
//! engine-specific connect that stands up an SSH tunnel first when configured.

use std::sync::Arc;
use std::time::Duration;

use red_core::{ConnectionConfig, DbKind, RedError};
use red_driver::{ClickhouseDriver, DatabaseDriver, MysqlDriver, PostgresDriver, SqliteDriver};

use crate::tunnel::Tunnel;

use super::session::{ConnectFail, HostKeyPrompt};

/// Cap on how long one connect attempt may run before the backend gives up and
/// reports a timeout. Bounds a hung connect (a black-hole host) so the dispatch
/// loop frees up for the next command; the UI drives retry/backoff and cancel
/// on top of this, but those only work if the loop isn't wedged awaiting a dial.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// [`connect`] bounded by [`CONNECT_TIMEOUT`]. A timeout surfaces as a connect
/// error like any other failure, so the UI's retry/backoff path handles it.
pub(crate) async fn attempt_connect(
    config: &ConnectionConfig,
) -> Result<(Arc<dyn DatabaseDriver>, Option<Tunnel>), ConnectFail> {
    match tokio::time::timeout(CONNECT_TIMEOUT, connect(config)).await {
        Ok(result) => result.map_err(classify_connect_err),
        Err(_) => Err(ConnectFail {
            message: format!("connection timed out after {}s", CONNECT_TIMEOUT.as_secs()),
            fatal: false,
            host_key: None,
        }),
    }
}

/// Classify a connect error for the UI. An unknown SSH host key becomes a
/// trustable `host_key` prompt; a driver `RedError::Auth` (bad credentials,
/// missing database) is fatal so the UI stops retrying; everything else is a
/// transient failure that warrants a backoff retry.
fn classify_connect_err(e: RedError) -> ConnectFail {
    match e {
        RedError::SshHostUnknown {
            host,
            port,
            fingerprint,
            key,
        } => ConnectFail {
            message: format!("unknown SSH host key for {host}"),
            fatal: true,
            host_key: Some(HostKeyPrompt {
                host,
                port,
                fingerprint,
                key,
            }),
        },
        other => ConnectFail {
            fatal: matches!(other, RedError::Auth(_)),
            message: other.to_string(),
            host_key: None,
        },
    }
}

async fn connect(
    config: &ConnectionConfig,
) -> red_core::Result<(Arc<dyn DatabaseDriver>, Option<Tunnel>)> {
    // SQLite is a local file (no network), so SSH never applies.
    if let DbKind::Sqlite = config.kind {
        let driver = SqliteDriver::new(config.dsn(), config.read_only);
        driver.ping().await?;
        return Ok((Arc::new(driver), None));
    }

    // For a network engine, stand up the SSH tunnel first (when configured) and
    // dial the local forwarded port instead of the real host. `dsn` is what the
    // driver connects to; `tunnel` must outlive it, so it rides into the session.
    let (dsn, tunnel) = match &config.ssh {
        Some(ssh) => {
            let port = config
                .port
                .or_else(|| config.kind.default_port())
                .unwrap_or(0);
            let tunnel = Tunnel::open(ssh, &config.host, port).await?;
            (
                config.local_dsn("127.0.0.1", tunnel.local_addr().port()),
                Some(tunnel),
            )
        }
        None => (config.dsn(), None),
    };

    let driver: Arc<dyn DatabaseDriver> = match config.kind {
        DbKind::Postgres => Arc::new(PostgresDriver::connect(&dsn, config.read_only).await?),
        DbKind::Mysql => {
            // A MySQL connection can see every database on the server; scope the
            // schema tree to the chosen one when the connection names a database.
            Arc::new(
                MysqlDriver::connect(&dsn, config.read_only)
                    .await?
                    .with_scope(Some(config.database.clone())),
            )
        }
        DbKind::Clickhouse => {
            // Like MySQL, a ClickHouse connection can see every database; scope the
            // tree to the chosen one. Read-only first (the driver refuses in-grid
            // edits regardless, since ClickHouse is OLAP).
            Arc::new(
                ClickhouseDriver::connect(&dsn, config.read_only)
                    .await?
                    .with_scope(Some(config.database.clone())),
            )
        }
        DbKind::Sqlite => unreachable!("handled above"),
    };
    Ok((driver, tunnel))
}
