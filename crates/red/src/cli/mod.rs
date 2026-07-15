//! Headless CLI mode: a second frontend onto the same `red-service` backend the
//! GPUI app drives, for scripting and seeding dev/staging environments.
//!
//! [`run`] is called first thing in `main`: when argv names a CLI verb it runs
//! the command headless and returns an exit code (so `main` never touches GPUI);
//! otherwise it returns `None` and the desktop app opens as usual. Each verb is a
//! thin request/response wrapper over the `Command`/`Event` protocol (connect,
//! send, drain events until the terminal one), so it reuses the service's connect,
//! streaming, and (Phase 2) copy/migrate paths verbatim, with no risk of drift
//! from the GUI.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use red_core::{ConnectionConfig, QueryOptions, SshAuth};
use red_service::{Command, Event, ServiceHandle, SessionId};

mod copy;
mod format;
mod mcp;
mod sql_split;

use format::{OutFormat, Writer};

/// The source/only session the CLI opens. Single-connection verbs (query, exec,
/// test) use just this one.
const PRIMARY: SessionId = SessionId::new(0);
/// The target session for the two-connection verbs (copy, migrate).
const TARGET: SessionId = SessionId::new(1);

// Exit codes; a script can branch on the failure class:
//   0  success
//   2  usage / resolution error (bad args, unknown connection, no SQL)
//   3  connect failure (auth, unreachable host, untrusted SSH, backend gone)
//   4  runtime failure (query/exec error, copy/migrate failure)
const EXIT_OK: u8 = 0;
const EXIT_USAGE: u8 = 2;
const EXIT_CONNECT: u8 = 3;
const EXIT_QUERY: u8 = 4;

/// `--quiet`: suppress progress and success summaries on stderr. Errors and the
/// actual result (stdout) always print. Process-global; the CLI runs one command.
static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether `--quiet` was passed. Read by the [`note!`]/[`progress!`] macros.
fn quiet() -> bool {
    QUIET.load(std::sync::atomic::Ordering::Relaxed)
}

/// Print an informational/summary line to stderr unless `--quiet`. Errors never
/// route through this; they use `eprintln!` directly so they always show.
macro_rules! note {
    ($($arg:tt)*) => {
        if !$crate::cli::quiet() {
            eprintln!($($arg)*);
        }
    };
}
pub(crate) use note;

/// Print an in-place progress fragment (no newline) to stderr unless `--quiet`,
/// flushing so it appears immediately.
macro_rules! progress {
    ($($arg:tt)*) => {
        if !$crate::cli::quiet() {
            use std::io::Write as _;
            eprint!($($arg)*);
            let _ = std::io::stderr().flush();
        }
    };
}
pub(crate) use progress;

/// The service's event stream: `(session, event)` items.
type EventRx = UnboundedReceiver<(Option<SessionId>, Event)>;

#[derive(Parser)]
#[command(
    name = "red",
    bin_name = "red",
    version,
    about = "RED: headless database CLI (a bare `red` opens the desktop app)"
)]
struct Cli {
    #[command(subcommand)]
    command: Verb,
    /// Suppress progress and success summaries on stderr (errors still print).
    #[arg(short = 'q', long, global = true)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Verb {
    /// List saved connections.
    Connections(ConnectionsArgs),
    /// Check that a connection can be reached.
    Test(ConnArgs),
    /// Run a query and stream the rows to stdout.
    Query(QueryArgs),
    /// Execute a statement or script (writes/DDL); the seed verb.
    Exec(ExecArgs),
    /// Stream one table into another table (same or cross connection).
    Copy(copy::CopyArgs),
    /// Migrate whole tables into another connection (create-fresh, FK-ordered).
    Migrate(copy::MigrateArgs),
    /// Remove all RED data: config + data directories and every keychain secret
    /// (connection passwords, SSH secrets, AI keys). Does not touch the binary.
    Reset(ResetArgs),
    /// Serve Red's read-only database tools to an MCP client over stdio (for
    /// Claude Code and other chats; no GUI, no ports).
    Mcp(mcp::McpArgs),
}

#[derive(Args)]
struct ResetArgs {
    /// Skip the confirmation prompt (for scripts / CI teardown).
    #[arg(long)]
    yes: bool,
}

#[derive(Args)]
struct ConnectionsArgs {
    /// Optional `list` (the default action).
    #[command(subcommand)]
    action: Option<ConnectionsAction>,
    /// Emit machine-readable JSON instead of a table.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum ConnectionsAction {
    /// List saved connections.
    List,
}

#[derive(Args)]
struct ConnArgs {
    /// A saved connection name, or an inline DSN (e.g. `postgres://u:p@h/db`).
    conn: String,
}

#[derive(Args)]
struct QueryArgs {
    /// A saved connection name, or an inline DSN.
    conn: String,
    /// SQL to run (omit to read from `--file` or stdin).
    sql: Option<String>,
    /// Read SQL from a file (`-` for stdin).
    #[arg(short = 'f', long = "file")]
    file: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutFormat::Table)]
    format: OutFormat,
}

#[derive(Args)]
struct ExecArgs {
    /// A saved connection name, or an inline DSN.
    conn: String,
    /// SQL to run (omit to read from `--file` or stdin). Multi-statement scripts
    /// are split and run one statement at a time, each in its own transaction.
    sql: Option<String>,
    /// Read SQL from a file (`-` for stdin).
    #[arg(short = 'f', long = "file")]
    file: Option<PathBuf>,
}

/// Entry point from `main`. Returns `Some(code)` when a CLI verb ran (the caller
/// exits with it), or `None` to fall through and open the desktop app.
pub fn run() -> Option<u8> {
    // Intercept only when the first *positional* arg is a recognised verb (so a
    // leading global flag like `--quiet` doesn't hide it), or a help/version flag
    // is present. A bare launch (or a stray token like macOS's `-psn_…`) falls
    // through to the GUI untouched.
    const VERBS: &[&str] = &[
        "connections",
        "test",
        "query",
        "exec",
        "copy",
        "migrate",
        "reset",
        "mcp",
    ];
    let args: Vec<String> = std::env::args().skip(1).collect();
    let first_positional = args.iter().find(|a| !a.starts_with('-'));
    let is_verb = first_positional.is_some_and(|a| VERBS.contains(&a.as_str()) || a == "help");
    let is_help = args
        .iter()
        .any(|a| matches!(a.as_str(), "-h" | "--help" | "-V" | "--version"));
    if !is_verb && !is_help {
        return None;
    }

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return Some(match e.kind() {
                clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayVersion
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => EXIT_OK,
                _ => EXIT_USAGE,
            });
        }
    };

    QUIET.store(cli.quiet, std::sync::atomic::Ordering::Relaxed);
    init_tracing();
    Some(match cli.command {
        Verb::Connections(a) => cmd_connections(a),
        Verb::Test(a) => cmd_test(a),
        Verb::Query(a) => cmd_query(a),
        Verb::Exec(a) => cmd_exec(a),
        Verb::Copy(a) => copy::cmd_copy(a),
        Verb::Migrate(a) => copy::cmd_migrate(a),
        Verb::Reset(a) => cmd_reset(a),
        Verb::Mcp(a) => mcp::cmd_mcp(a),
    })
}

/// CLI logging: quiet by default (warnings/errors to stderr) so stdout stays
/// pipe-clean, but honour `RUST_LOG` for debugging.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .try_init();
}

// ---- verbs -----------------------------------------------------------------

fn cmd_connections(args: ConnectionsArgs) -> u8 {
    // `list` is the only (and default) action today.
    let ConnectionsAction::List = args.action.unwrap_or(ConnectionsAction::List);
    let saved = red_config::config::load();
    if args.json {
        let items: Vec<serde_json::Value> = saved
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.config.name,
                    "kind": format!("{:?}", s.config.kind).to_lowercase(),
                    "target": s.config.display_target(),
                    "read_only": s.config.read_only,
                    "pinned": s.pinned,
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(items));
        return EXIT_OK;
    }
    if saved.is_empty() {
        eprintln!("no saved connections (add one in the app, or pass an inline DSN)");
        return EXIT_OK;
    }
    let mut out = std::io::stdout().lock();
    for s in &saved {
        let kind = format!("{:?}", s.config.kind).to_lowercase();
        let ro = if s.config.read_only {
            " [read-only]"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{}\t{}\t{}{}",
            s.config.name,
            kind,
            s.config.display_target(),
            ro
        );
    }
    EXIT_OK
}

fn cmd_test(args: ConnArgs) -> u8 {
    let config = match resolve(&args.conn) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };
    let (svc, mut events) = start();
    // `TestConnection` is session-less: it opens a throwaway session and replies
    // TestSucceeded/TestFailed.
    svc.send_global(Command::TestConnection(config));
    let code = loop {
        match recv(&mut events) {
            Some(Event::TestSucceeded { version }) => {
                println!("ok: {version}");
                break EXIT_OK;
            }
            Some(Event::TestFailed { message }) => {
                eprintln!("test failed: {message}");
                break EXIT_CONNECT;
            }
            Some(Event::Error(e)) => {
                eprintln!("error: {e}");
                break EXIT_CONNECT;
            }
            Some(_) => continue,
            None => break backend_gone(),
        }
    };
    shutdown(&svc);
    code
}

fn cmd_query(args: QueryArgs) -> u8 {
    let sql = match read_sql(args.sql, args.file.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };
    let config = match resolve(&args.conn) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };
    let (svc, mut events) = start();
    if let Err(code) = connect(&svc, &mut events, config) {
        shutdown(&svc);
        return code;
    }

    // Full fidelity: the CLI wants whole values, not the grid's fat-cell cap
    // (which would render long text/blobs as truncated `Value::Capped`).
    let opts = QueryOptions {
        window: 1000,
        timeout: None,
        full_fidelity: true,
    };
    svc.send_to(PRIMARY, Command::Query { sql, opts });

    let mut writer = Writer::new(args.format, std::io::stdout().lock());
    let code = loop {
        match recv(&mut events) {
            Some(Event::QueryStarted { columns }) => writer.start(&columns),
            Some(Event::QueryRows(window)) => writer.rows(&window.rows),
            Some(Event::QueryFinished {
                rows_streamed,
                elapsed,
            }) => {
                writer.finish();
                note!("{rows_streamed} rows in {elapsed:?}");
                break EXIT_OK;
            }
            Some(Event::QueryCancelled) => {
                eprintln!("query cancelled");
                break EXIT_QUERY;
            }
            Some(Event::Error(e)) => {
                eprintln!("error: {e}");
                break EXIT_QUERY;
            }
            Some(_) => continue,
            None => break backend_gone(),
        }
    };
    shutdown(&svc);
    code
}

fn cmd_exec(args: ExecArgs) -> u8 {
    let sql = match read_sql(args.sql, args.file.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };
    let statements = sql_split::split_statements(&sql);
    if statements.is_empty() {
        eprintln!("no statements to execute");
        return EXIT_OK;
    }
    let config = match resolve(&args.conn) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };
    let (svc, mut events) = start();
    if let Err(code) = connect(&svc, &mut events, config) {
        shutdown(&svc);
        return code;
    }

    let mut total: u64 = 0;
    for (idx, stmt) in statements.iter().enumerate() {
        svc.send_to(PRIMARY, Command::Execute { sql: stmt.clone() });
        match wait_execute(&mut events) {
            Ok(affected) => total += affected,
            Err(msg) => {
                eprintln!("statement {} failed: {msg}", idx + 1);
                shutdown(&svc);
                return EXIT_QUERY;
            }
        }
    }
    note!(
        "ok: {} statement(s), {total} row(s) affected",
        statements.len()
    );
    shutdown(&svc);
    EXIT_OK
}

/// `red reset [--yes]`: wipe every RED directory and keychain secret. Prompts for
/// confirmation on an interactive terminal unless `--yes`; prints the report and
/// exits non-zero if any step failed. Runs the shared `crate::reset` teardown
/// directly (no backend needed — it's pure filesystem + keychain work).
fn cmd_reset(args: ResetArgs) -> u8 {
    if !args.yes {
        if !std::io::stdin().is_terminal() {
            eprintln!("refusing to reset without confirmation; pass --yes to proceed");
            return EXIT_USAGE;
        }
        eprint!(
            "This removes ALL RED data — config, cached data, and every keychain \
             secret (connection passwords, SSH secrets, AI keys). It does not touch \
             the RED binary. This is irreversible.\nContinue? [y/N] "
        );
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err()
            || !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
        {
            eprintln!("aborted");
            return EXIT_OK;
        }
    }

    let report = crate::reset::remove_all_data();
    note!(
        "removed {} connection secret-set(s), {} AI key(s); config dir {}, data dir {}",
        report.connections_cleared,
        report.ai_keys_cleared,
        if report.config_dir_removed {
            "removed"
        } else {
            "not removed"
        },
        if report.data_dir_removed {
            "removed"
        } else {
            "not removed"
        },
    );
    if report.errors.is_empty() {
        EXIT_OK
    } else {
        for e in &report.errors {
            eprintln!("reset: {e}");
        }
        EXIT_QUERY
    }
}

// ---- service pump ----------------------------------------------------------

/// Spawn the backend and take its event stream.
fn start() -> (ServiceHandle, EventRx) {
    let mut svc = red_service::spawn();
    #[allow(
        clippy::expect_used,
        reason = "events taken exactly once, right after spawn"
    )]
    let events = svc.take_events().expect("service event stream");
    (svc, events)
}

/// Block for the next event, discarding the session tag (the CLI drives one
/// connection at a time). `None` once the backend thread is gone.
fn recv(events: &mut EventRx) -> Option<Event> {
    futures::executor::block_on(events.next()).map(|(_, ev)| ev)
}

/// Open the primary session and wait for it to come up.
fn connect(svc: &ServiceHandle, events: &mut EventRx, config: ConnectionConfig) -> Result<(), u8> {
    connect_session(svc, events, PRIMARY, config)
}

/// Open `session` and wait for it to come up. On failure prints the cause and
/// returns the exit code to use. Used directly by copy/migrate, which open a
/// source and a target session.
fn connect_session(
    svc: &ServiceHandle,
    events: &mut EventRx,
    session: SessionId,
    config: ConnectionConfig,
) -> Result<(), u8> {
    svc.send_to(session, Command::Connect(config));
    loop {
        match recv(events) {
            Some(Event::Connected { .. }) => return Ok(()),
            Some(Event::ConnectFailed { message, .. }) => {
                eprintln!("connect failed: {message}");
                return Err(EXIT_CONNECT);
            }
            Some(Event::SshHostUnknown {
                host,
                port,
                fingerprint,
                ..
            }) => {
                eprintln!(
                    "ssh host {host}:{port} is not trusted (fingerprint {fingerprint}); \
                     connect once in the app to trust it"
                );
                return Err(EXIT_CONNECT);
            }
            Some(Event::Error(e)) => {
                eprintln!("error: {e}");
                return Err(EXIT_CONNECT);
            }
            Some(_) => continue,
            None => return Err(backend_gone()),
        }
    }
}

/// Drain events until the current `Execute` resolves.
fn wait_execute(events: &mut EventRx) -> Result<u64, String> {
    loop {
        match recv(events) {
            Some(Event::Executed { affected }) => return Ok(affected as u64),
            Some(Event::Error(e)) => return Err(e),
            Some(_) => continue,
            None => return Err("backend closed unexpectedly".into()),
        }
    }
}

/// Ask the backend to shut down cleanly. Best-effort, as the process is exiting.
fn shutdown(svc: &ServiceHandle) {
    svc.send_global(Command::Shutdown);
}

/// Print the "backend vanished" message and yield its exit code.
fn backend_gone() -> u8 {
    eprintln!("backend closed unexpectedly");
    EXIT_CONNECT
}

// ---- connection resolution -------------------------------------------------

/// Resolve `conn` to a ready-to-dial config: a saved connection by name (then by
/// id), with its secrets hydrated from the keychain; failing that, an inline DSN
/// (for CI, where nothing is saved). Errors list the available names.
fn resolve(conn: &str) -> Result<ConnectionConfig, String> {
    let saved = red_config::config::load();
    if let Some(stored) = saved
        .iter()
        .find(|s| s.config.name == conn)
        .or_else(|| saved.iter().find(|s| s.id == conn))
    {
        let mut config = stored.config.clone();
        hydrate_secrets(&stored.id, &mut config);
        return Ok(config);
    }
    if let Some(parsed) = ConnectionConfig::parse_conn_str(conn) {
        return Ok(ConnectionConfig {
            name: conn.to_string(),
            kind: parsed.kind,
            host: parsed.host,
            port: parsed.port,
            user: parsed.user,
            password: parsed.password,
            database: parsed.database,
            ..Default::default()
        });
    }
    let names: Vec<&str> = saved.iter().map(|s| s.config.name.as_str()).collect();
    let known = if names.is_empty() {
        "none saved".to_string()
    } else {
        names.join(", ")
    };
    Err(format!(
        "no saved connection named {conn:?}, and it isn't a recognised DSN.\nknown connections: {known}"
    ))
}

/// Materialize the DB password and any SSH secret from the keychain, exactly as
/// the GUI does before connecting (the saved config carries these empty).
fn hydrate_secrets(id: &str, config: &mut ConnectionConfig) {
    if config.password.is_empty() && !config.kind.is_file() {
        match red_config::secrets::get_password(id) {
            Ok(Some(pw)) => config.password = pw,
            Ok(None) => {}
            Err(e) => tracing::warn!("failed to read password from keychain: {e}"),
        }
    }
    if let Some(ssh) = config.ssh.as_mut() {
        match &ssh.auth {
            SshAuth::Password if ssh.password.is_empty() => {
                if let Ok(Some(pw)) = red_config::secrets::get_ssh_password(id) {
                    ssh.password = pw;
                }
            }
            SshAuth::Key { .. } if ssh.passphrase.is_empty() => {
                if let Ok(Some(pp)) = red_config::secrets::get_ssh_passphrase(id) {
                    ssh.passphrase = pp;
                }
            }
            _ => {}
        }
    }
    if let Some(proxy) = config.proxy.as_mut()
        && proxy.password.is_empty()
        && let Ok(Some(pw)) = red_config::secrets::get_proxy_password(id)
    {
        proxy.password = pw;
    }
}

// ---- SQL input -------------------------------------------------------------

/// Resolve the SQL text from an inline argument, a file (`-` = stdin), or, when
/// neither is given, stdin. Erroring only when stdin is an interactive terminal
/// (so `red exec conn` with nothing to run doesn't hang waiting for typing).
fn read_sql(inline: Option<String>, file: Option<&Path>) -> Result<String, String> {
    if let Some(sql) = inline {
        return Ok(sql);
    }
    if let Some(path) = file {
        if path == Path::new("-") {
            return read_stdin();
        }
        return std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()));
    }
    if std::io::stdin().is_terminal() {
        return Err("no SQL provided (pass it inline, with --file, or on stdin)".into());
    }
    read_stdin()
}

fn read_stdin() -> Result<String, String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("cannot read stdin: {e}"))?;
    Ok(buf)
}
