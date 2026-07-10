//! `copy` and `migrate`: the two-connection verbs that seed a dev/staging
//! database. Both open a **source** and a **target** session and send the exact
//! `Command` the GUI sends (`CopyToTable` / `MigrateTables`), so the streamed,
//! FK-ordered, per-chunk-committed jobs in `red-service` run verbatim.
//!
//! - `copy` moves **one** table into another (append / replace, or `--create` the
//!   target from the source's columns first). It opens the source as a result to
//!   get an epoch + columns, name-maps onto the target, then fires `CopyToTable`.
//! - `migrate` moves **many** tables create-fresh (skipping any that already
//!   exist on the target), FK-ordered (the whole-schema headline). It just needs
//!   the table names + both sessions.

use std::collections::HashSet;

use clap::{Args, ValueEnum};
use red_core::{Column, ColumnMap, ColumnMeta, CopyMode, DbKind, ObjectKind, TableRef};
use red_service::{Command, Event};

use super::{
    backend_gone, connect_session, note, progress, recv, resolve, shutdown, start, EventRx,
    EXIT_OK, EXIT_QUERY, EXIT_USAGE, PRIMARY, TARGET,
};
use crate::schema::quote_ident;

/// Correlation id for the single copy/migrate job a CLI invocation runs.
const JOB_ID: u64 = 1;
/// Epoch for the source result the copy opens.
const SOURCE_EPOCH: u64 = 1;

#[derive(Args)]
pub struct CopyArgs {
    /// Source connection (a saved name or an inline DSN).
    conn: String,
    /// Source table (optionally schema-qualified: `schema.table`).
    table: String,
    /// Target connection (a saved name or an inline DSN).
    #[arg(long = "to")]
    to: String,
    /// Create the target table from the source's columns if it doesn't exist.
    #[arg(long)]
    create: bool,
    /// Target table name (defaults to the source table's name).
    #[arg(long = "as")]
    as_table: Option<String>,
    /// Target schema/database (defaults to the connection's default).
    #[arg(long = "target-schema")]
    target_schema: Option<String>,
    /// Write mode: add rows, or clear the target first.
    #[arg(long, value_enum, default_value_t = ModeArg::Append)]
    mode: ModeArg,
    /// Print what would be copied (target, mode, column mapping) and exit without
    /// writing.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
pub struct MigrateArgs {
    /// Source connection (a saved name or an inline DSN).
    conn: String,
    /// Target connection (a saved name or an inline DSN).
    #[arg(long = "to")]
    to: String,
    /// Tables to migrate (comma-separated). Default: every table in the schema.
    #[arg(long, value_delimiter = ',')]
    tables: Vec<String>,
    /// Source schema/database (required when the source has more than one).
    #[arg(long)]
    schema: Option<String>,
    /// Target schema/database (defaults to the connection's default).
    #[arg(long = "target-schema")]
    target_schema: Option<String>,
    /// Print the migration plan (which tables would be created vs skipped) and
    /// exit without writing.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Copy, Clone, ValueEnum)]
enum ModeArg {
    /// Insert the source rows, keeping the target's existing rows.
    Append,
    /// Clear the target (`DELETE FROM`) before inserting.
    Replace,
}

impl From<ModeArg> for CopyMode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Append => CopyMode::Append,
            ModeArg::Replace => CopyMode::TruncateInsert,
        }
    }
}

pub fn cmd_copy(args: CopyArgs) -> u8 {
    let source = match resolve(&args.conn) {
        Ok(c) => c,
        Err(e) => return usage(e),
    };
    let target_config = match resolve(&args.to) {
        Ok(c) => c,
        Err(e) => return usage(e),
    };
    if let Some(code) = reject_kv(&source, &target_config) {
        return code;
    }
    let source_kind = source.kind;
    // The target's schema/database is needed to address the target table (SQLite's
    // describe/insert reject an empty schema). Default it per engine when
    // `--target-schema` is absent; the same namespace the GUI's picker supplies.
    let target_schema = args
        .target_schema
        .clone()
        .or_else(|| default_schema(target_config.kind, &target_config.database));

    let (svc, mut events) = start();
    if let Err(code) = connect_session(&svc, &mut events, PRIMARY, source) {
        shutdown(&svc);
        return code;
    }
    if let Err(code) = connect_session(&svc, &mut events, TARGET, target_config) {
        shutdown(&svc);
        return code;
    }

    // Open the source table as a result to get its epoch + column shape. The copy
    // job re-reads this result's SQL at full fidelity, so nothing materializes here.
    let (schema, name) = split_table(&args.table);
    let sql = source_select(schema.as_deref(), &name, source_kind);
    let source_cols = match open_source(&svc, &mut events, sql) {
        Ok(cols) => cols,
        Err(code) => {
            shutdown(&svc);
            return code;
        }
    };

    let target = TableRef {
        schema: target_schema,
        name: args.as_table.clone().unwrap_or(name),
    };

    // Build the column mapping (and, with --create, the target's column spec).
    let (mapping, create) = if args.create {
        let (create, mapping) = create_spec(&source_cols);
        (mapping, Some(create))
    } else {
        let target_cols = match describe_target(&svc, &mut events, target.clone()) {
            Ok(cols) => cols,
            Err(code) => {
                shutdown(&svc);
                return code;
            }
        };
        // A real table has columns; empty means it doesn't exist (some engines
        // report a missing table as an empty describe rather than an error).
        if target_cols.is_empty() {
            eprintln!(
                "target table {} not found; pass --create to make it",
                target.name
            );
            shutdown(&svc);
            return EXIT_QUERY;
        }
        let mapping = auto_map(&source_cols, &target_cols);
        if mapping.is_empty() {
            eprintln!("no source columns match {}'s columns", target.name);
            shutdown(&svc);
            return EXIT_QUERY;
        }
        (mapping, None)
    };

    if args.dry_run {
        print_copy_plan(
            &args.table,
            &target,
            args.mode,
            create.is_some(),
            &mapping,
            &source_cols,
        );
        shutdown(&svc);
        return EXIT_OK;
    }

    svc.send_to(
        PRIMARY,
        Command::CopyToTable {
            id: JOB_ID,
            source_epoch: SOURCE_EPOCH,
            target,
            target_session: TARGET,
            mapping,
            mode: args.mode.into(),
            create,
        },
    );
    let code = drain_transfer(&mut events, "copy");
    shutdown(&svc);
    code
}

/// Print the `--dry-run` copy plan to stdout: the target, the write mode, whether
/// the target is created, and the resolved source→target column mapping.
fn print_copy_plan(
    source_table: &str,
    target: &TableRef,
    mode: ModeArg,
    creating: bool,
    mapping: &[ColumnMap],
    source_cols: &[Column],
) {
    let verb = match (creating, CopyMode::from(mode)) {
        (true, _) => "create + copy into",
        (false, CopyMode::Append) => "append into",
        (false, CopyMode::TruncateInsert) => "replace",
    };
    println!("would {verb} {} ← {source_table}", target_label(target));
    println!("column mapping ({} column(s)):", mapping.len());
    for m in mapping {
        let src = source_cols
            .get(m.source)
            .map(|c| c.name.as_str())
            .unwrap_or("?");
        println!("  {} ← {src}", m.column);
    }
}

/// `[schema.]table` for display in a plan.
fn target_label(target: &TableRef) -> String {
    match &target.schema {
        Some(s) => format!("{s}.{}", target.name),
        None => target.name.clone(),
    }
}

pub fn cmd_migrate(args: MigrateArgs) -> u8 {
    let source = match resolve(&args.conn) {
        Ok(c) => c,
        Err(e) => return usage(e),
    };
    let target_config = match resolve(&args.to) {
        Ok(c) => c,
        Err(e) => return usage(e),
    };
    if let Some(code) = reject_kv(&source, &target_config) {
        return code;
    }
    let source_kind = source.kind;
    let source_db = source.database.clone();

    let (svc, mut events) = start();
    if let Err(code) = connect_session(&svc, &mut events, PRIMARY, source) {
        shutdown(&svc);
        return code;
    }
    if let Err(code) = connect_session(&svc, &mut events, TARGET, target_config) {
        shutdown(&svc);
        return code;
    }

    // Explicit --tables, or enumerate the (chosen) schema's tables from the source.
    // With explicit tables, default the source schema per engine when --schema is
    // absent (SQLite's per-table describe needs a non-empty schema).
    let (schema, tables) = if !args.tables.is_empty() {
        let schema = args
            .schema
            .clone()
            .or_else(|| default_schema(source_kind, &source_db));
        (schema, args.tables.clone())
    } else {
        match list_tables(&svc, &mut events, args.schema.as_deref()) {
            Ok(v) => v,
            Err(code) => {
                shutdown(&svc);
                return code;
            }
        }
    };
    if tables.is_empty() {
        eprintln!("no tables to migrate");
        shutdown(&svc);
        return EXIT_OK;
    }

    if args.dry_run {
        let existing = match target_table_names(&svc, &mut events) {
            Ok(set) => set,
            Err(code) => {
                shutdown(&svc);
                return code;
            }
        };
        print_migrate_plan(&tables, &existing, args.target_schema.as_deref());
        shutdown(&svc);
        return EXIT_OK;
    }

    note!("migrating {} table(s)…", tables.len());
    svc.send_to(
        PRIMARY,
        Command::MigrateTables {
            id: JOB_ID,
            source_schema: schema,
            tables,
            target_session: TARGET,
            target_schema: args.target_schema.clone(),
        },
    );
    let code = drain_transfer(&mut events, "migrate");
    shutdown(&svc);
    code
}

// ---- source result ---------------------------------------------------------

/// Split `schema.table` into `(Some(schema), table)`, or `(None, table)` for a
/// bare name. Only the first dot separates; a dotted table name is unusual and
/// out of scope.
fn split_table(table: &str) -> (Option<String>, String) {
    match table.split_once('.') {
        Some((s, t)) => (Some(s.to_string()), t.to_string()),
        None => (None, table.to_string()),
    }
}

/// `copy`/`migrate` are SQL-shaped (`SELECT`/`INSERT` over a `DatabaseDriver`);
/// a Redis connection has no such driver at all (see docs/plans/redis.md), so
/// reject it up front with a clean usage error rather than connecting and
/// failing deep inside `default_schema`'s exhaustive match.
fn reject_kv(
    source: &red_core::ConnectionConfig,
    target: &red_core::ConnectionConfig,
) -> Option<u8> {
    if source.kind == DbKind::Redis || target.kind == DbKind::Redis {
        return Some(usage(
            "copy/migrate isn't supported for Redis connections yet".into(),
        ));
    }
    None
}

/// The default namespace to address a table in when the user passes no explicit
/// schema: SQLite's sole attached DB is `main`, Postgres's default schema is
/// `public`, and a MySQL/ClickHouse "schema" *is* the connected database. Used for
/// both the copy target and the migrate source (SQLite's describe/insert reject an
/// empty schema); matches the namespace the GUI's picker/tree supplies.
fn default_schema(kind: DbKind, database: &str) -> Option<String> {
    match kind {
        DbKind::Sqlite => Some("main".into()),
        DbKind::Postgres => Some("public".into()),
        DbKind::Mysql | DbKind::Clickhouse => (!database.is_empty()).then(|| database.to_string()),
        // `red copy`/`red migrate` are SQL-shaped (SELECT/INSERT); the CLI
        // never routes a Redis connection into them (see docs/plans/redis.md).
        DbKind::Redis => unreachable!("Redis isn't a copy/migrate SQL source or target"),
    }
}

/// `SELECT * FROM [schema.]table`, identifiers quoted for the source dialect.
fn source_select(schema: Option<&str>, table: &str, kind: DbKind) -> String {
    match schema {
        Some(s) => format!(
            "SELECT * FROM {}.{}",
            quote_ident(s, kind),
            quote_ident(table, kind)
        ),
        None => format!("SELECT * FROM {}", quote_ident(table, kind)),
    }
}

/// Open the source `SELECT` as a result and return its columns.
fn open_source(
    svc: &red_service::ServiceHandle,
    events: &mut EventRx,
    sql: String,
) -> Result<Vec<Column>, u8> {
    svc.send_to(
        PRIMARY,
        Command::OpenResult {
            sql,
            epoch: SOURCE_EPOCH,
            table: None,
            sort: None,
            filter: None,
            joins: Vec::new(),
        },
    );
    loop {
        match recv(events) {
            Some(Event::ResultReady { columns, .. }) => return Ok(columns),
            Some(Event::Error(e)) => {
                eprintln!("cannot read source table: {e}");
                return Err(EXIT_QUERY);
            }
            Some(_) => continue,
            None => return Err(backend_gone()),
        }
    }
}

// ---- column mapping --------------------------------------------------------

/// The `--create` spec: a plain, nullable target column per source column (their
/// types are mapped into the target dialect by `create_table`), plus the identity
/// mapping. A query result carries no PK / not-null / default, so the new table's
/// columns are plain, matching the GUI's "new table" copy.
fn create_spec(source_cols: &[Column]) -> (Vec<ColumnMeta>, Vec<ColumnMap>) {
    let create = source_cols
        .iter()
        .map(|c| ColumnMeta {
            name: c.name.clone(),
            type_name: c.decl_type.clone(),
            not_null: false,
            primary_key: false,
            default: None,
            auto_increment: false,
        })
        .collect();
    let mapping = source_cols
        .iter()
        .enumerate()
        .map(|(i, c)| ColumnMap {
            source: i,
            column: c.name.clone(),
            decl_type: c.decl_type.clone(),
        })
        .collect();
    (create, mapping)
}

/// Auto-map source→target columns by name (case-insensitive), like the GUI's copy
/// confirm. Target columns with no source match are left to default/NULL; source
/// columns with no target match are ignored.
fn auto_map(source_cols: &[Column], target_cols: &[Column]) -> Vec<ColumnMap> {
    let mut mapping = Vec::new();
    for tcol in target_cols {
        if let Some(idx) = source_cols
            .iter()
            .position(|s| s.name.eq_ignore_ascii_case(&tcol.name))
        {
            mapping.push(ColumnMap {
                source: idx,
                column: tcol.name.clone(),
                decl_type: tcol.decl_type.clone(),
            });
        }
    }
    mapping
}

/// Describe the target table's columns (on the target session) for name-mapping.
/// A describe failure comes back as `CopyFailed`, meaning the table doesn't exist
/// (use `--create`) or isn't reachable.
fn describe_target(
    svc: &red_service::ServiceHandle,
    events: &mut EventRx,
    target: TableRef,
) -> Result<Vec<Column>, u8> {
    svc.send_to(TARGET, Command::CopyTargetColumns { id: JOB_ID, target });
    loop {
        match recv(events) {
            Some(Event::CopyTargetColumns { columns, .. }) => return Ok(columns),
            Some(Event::CopyFailed { message, .. }) => {
                eprintln!("target table not found ({message}); pass --create to make it");
                return Err(EXIT_QUERY);
            }
            Some(Event::Error(e)) => {
                eprintln!("error: {e}");
                return Err(EXIT_QUERY);
            }
            Some(_) => continue,
            None => return Err(backend_gone()),
        }
    }
}

// ---- migrate table discovery -----------------------------------------------

/// Enumerate the source's tables in the chosen schema. Picks the schema by
/// `--schema`, or the sole schema when there's exactly one; otherwise asks for
/// `--schema`. Returns the resolved schema name plus its table names.
fn list_tables(
    svc: &red_service::ServiceHandle,
    events: &mut EventRx,
    want_schema: Option<&str>,
) -> Result<(Option<String>, Vec<String>), u8> {
    svc.send_to(PRIMARY, Command::LoadObjects);
    let schemas = loop {
        match recv(events) {
            Some(Event::ObjectsLoaded { schemas }) => break schemas,
            Some(Event::Error(e)) => {
                eprintln!("cannot list source tables: {e}");
                return Err(EXIT_QUERY);
            }
            Some(_) => continue,
            None => return Err(backend_gone()),
        }
    };

    let chosen = match want_schema {
        Some(want) => match schemas.iter().find(|m| m.name == want) {
            Some(m) => m,
            None => {
                eprintln!("no schema named {want:?} on the source");
                return Err(EXIT_USAGE);
            }
        },
        None => match schemas.as_slice() {
            [only] => only,
            [] => {
                eprintln!("source has no schemas");
                return Err(EXIT_QUERY);
            }
            many => {
                let names: Vec<&str> = many.iter().map(|m| m.name.as_str()).collect();
                eprintln!(
                    "source has multiple schemas; pass --schema (one of: {})",
                    names.join(", ")
                );
                return Err(EXIT_USAGE);
            }
        },
    };
    let tables = chosen
        .objects
        .iter()
        .filter(|o| matches!(o.kind, ObjectKind::Table))
        .map(|o| o.name.clone())
        .collect();
    Ok((Some(chosen.name.clone()), tables))
}

/// The lowercased table names present anywhere on the **target**, for the migrate
/// `--dry-run` skip check (migrate skips a table already on the target).
fn target_table_names(
    svc: &red_service::ServiceHandle,
    events: &mut EventRx,
) -> Result<HashSet<String>, u8> {
    svc.send_to(TARGET, Command::LoadObjects);
    loop {
        match recv(events) {
            Some(Event::ObjectsLoaded { schemas }) => {
                return Ok(schemas
                    .iter()
                    .flat_map(|s| s.objects.iter())
                    .filter(|o| matches!(o.kind, ObjectKind::Table))
                    .map(|o| o.name.to_ascii_lowercase())
                    .collect())
            }
            Some(Event::Error(e)) => {
                eprintln!("cannot inspect target: {e}");
                return Err(EXIT_QUERY);
            }
            Some(_) => continue,
            None => return Err(backend_gone()),
        }
    }
}

/// Print the `--dry-run` migrate plan to stdout: per table, whether it would be
/// created or skipped (already on the target). Migrate never appends into an
/// existing table, so an existing name is a skip.
fn print_migrate_plan(tables: &[String], existing: &HashSet<String>, target_schema: Option<&str>) {
    let dest = target_schema.unwrap_or("(default schema)");
    println!("would migrate {} table(s) into {dest}:", tables.len());
    for t in tables {
        if existing.contains(&t.to_ascii_lowercase()) {
            println!("  skip   {t} (already on target)");
        } else {
            println!("  create {t}");
        }
    }
}

// ---- shared transfer drain -------------------------------------------------

/// Drain the shared `Copy*` progress/terminal events (used by both copy and
/// migrate). Progress overwrites one stderr line; the terminal event prints a
/// summary and yields the exit code. `noun` is "copy" or "migrate".
fn drain_transfer(events: &mut EventRx, noun: &str) -> u8 {
    loop {
        match recv(events) {
            Some(Event::CopyProgress { rows, .. }) => {
                progress!("\r{rows} row(s)…");
            }
            Some(Event::CopyFinished { rows, .. }) => {
                note!("\rok: {noun} moved {rows} row(s)   ");
                return EXIT_OK;
            }
            Some(Event::CopyFailed { rows, message, .. }) => {
                let so_far = if rows > 0 {
                    format!(" after {rows} row(s)")
                } else {
                    String::new()
                };
                eprintln!("\r{noun} failed{so_far}: {message}");
                return EXIT_QUERY;
            }
            Some(Event::CopyCancelled { rows, .. }) => {
                eprintln!("\r{noun} cancelled ({rows} row(s) kept)");
                return EXIT_QUERY;
            }
            Some(_) => continue,
            None => return backend_gone(),
        }
    }
}

/// Print a resolution error to stderr and yield the usage exit code.
fn usage(message: String) -> u8 {
    eprintln!("{message}");
    EXIT_USAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_table_separates_schema() {
        assert_eq!(split_table("users"), (None, "users".into()));
        assert_eq!(
            split_table("public.users"),
            (Some("public".into()), "users".into())
        );
        // Only the first dot splits.
        assert_eq!(split_table("a.b.c"), (Some("a".into()), "b.c".into()));
    }

    #[test]
    fn source_select_quotes_per_dialect() {
        assert_eq!(
            source_select(None, "users", DbKind::Postgres),
            r#"SELECT * FROM "users""#
        );
        assert_eq!(
            source_select(Some("public"), "users", DbKind::Postgres),
            r#"SELECT * FROM "public"."users""#
        );
        assert_eq!(
            source_select(None, "users", DbKind::Mysql),
            "SELECT * FROM `users`"
        );
    }

    #[test]
    fn default_schema_is_engine_appropriate() {
        assert_eq!(default_schema(DbKind::Sqlite, ""), Some("main".into()));
        assert_eq!(default_schema(DbKind::Postgres, ""), Some("public".into()));
        // MySQL/ClickHouse "schema" is the connected database.
        assert_eq!(default_schema(DbKind::Mysql, "shop"), Some("shop".into()));
        assert_eq!(default_schema(DbKind::Mysql, ""), None);
    }
}
