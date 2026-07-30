//! PostgreSQL driver: the second source of `DatabaseDriver`, proving the
//! abstraction on a real network engine. Built on `tokio-postgres`: a live
//! `Client` (its connection driven by a background task), a streaming cursor over
//! `query_raw`, and **out-of-band cancel** via `tokio-postgres`'s `CancelToken`
//! (a separate cancel-request connection, not a dropped future).
//!
//! Caveats for v0.1: connections are `NoTls` (TLS is the next hardening step).
//! Value mapping covers the common scalar types, bool/int/float/text/bytea,
//! plus the richer ones a first-time visitor expects to *see* rather than as empty
//! NULLs: numeric, timestamp(tz), date, time(tz), uuid, and json(b) are rendered
//! from their binary wire form by [`crate::pg_text`] (dependency-free). Anything
//! else decodes through Postgres's string path, and a type that path rejects
//! (enum, inet, interval, array, …) falls back to its raw wire bytes as lossy UTF-8
//! rather than a silent NULL. Read-only sets `default_transaction_read_only`.

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures_util::StreamExt;
use red_core::{
    Column, ColumnMeta, ColumnPredicate, ColumnValue, DbKind, EditOp, ExportFormat, FkEdge, FkJoin,
    ForeignKeyMeta, IndexMeta, KeySpec, ObjectKind, ObjectMeta, QueryOptions, QueryPlan, RedError,
    Result, ResultPage, RowWindow, SchemaMeta, TableDetail, TableRef, Value,
};
use std::fs::File;
use std::io::BufWriter;
use std::sync::Mutex as StdMutex;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, NoTls, Row, RowStream, Statement};

use crate::format::{ExportWriter, ProgressThrottle, strip_trailing};
use crate::pg_text;
use crate::{
    AbortSignal, ArmGuard, CancelToken, CellCap, DatabaseDriver, PageCap, QueryCursor, driver_err,
};

/// Warm fetch connections kept ready for the one-shot read paths. `tokio-postgres`
/// cancellation is *connection-scoped*, so running every page/seek/count on the one
/// shared `Client` would mean a superseded fetch's cancel could land on a sibling
/// fetch pipelined on the same connection. A small pool gives each cancellable
/// fetch its own connection, so its cancel hits exactly its own query. Grows
/// lazily (nothing opened until the first fetch) to respect the cold-start budget.
const FETCH_POOL_CAP: usize = 4;

/// A live PostgreSQL session. Holds the shared `Client` (introspection, `execute`)
/// plus a small lazily-grown pool of warm connections the cancellable one-shot
/// fetches *and* the streaming cursors borrow; see `FETCH_POOL_CAP`.
pub struct PostgresDriver {
    client: Arc<Client>,
    version: String,
    dsn: String,
    read_only: bool,
    /// Idle fetch connections, returned after each one-shot fetch and when a cursor
    /// is dropped. A free list, not a semaphore: `acquire` opens a fresh connection
    /// when it's empty.
    ///
    /// Shared by `Arc` because a [`PgCursor`] outlives the call that opened it and
    /// has to hand its connection back on drop.
    pool: Arc<StdMutex<Vec<Arc<Client>>>>,
}

/// Return `client` to the free list, dropping it if dead or the pool is at cap.
/// Free-standing so both [`PostgresDriver::release`] and `PgCursor`'s `Drop` — which
/// has no driver reference, only the shared pool — hand connections back the same way.
fn release_to(pool: &StdMutex<Vec<Arc<Client>>>, client: Arc<Client>) {
    if client.is_closed() {
        return;
    }
    let mut pool = lock(pool);
    if pool.len() < FETCH_POOL_CAP {
        pool.push(client);
    }
}

/// No bind parameters; `query_raw` needs a typed iterator, so spell out the kind.
fn no_params() -> Vec<&'static (dyn ToSql + Sync)> {
    Vec::new()
}

/// Lock a mutex, tolerating poison (the free-list critical sections can't panic).
fn lock<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// An out-of-band cancel for `client`: a separate cancel request over a fresh
/// connection (not a dropped future). `client` is one connection, so this cancels
/// exactly that connection's in-flight query.
///
/// Only sound where the query is already executing when the token is armed, as a
/// cursor's is: its stream is open before the token exists. A one-shot fetch arms
/// *before* it issues, so it needs [`pg_cancel_token_until`].
fn pg_cancel_token(client: &Client) -> CancelToken {
    let token = client.cancel_token();
    CancelToken::new(move || {
        let token = token.clone();
        tokio::spawn(async move {
            let _ = token.cancel_query(NoTls).await;
        });
    })
}

/// Pause between re-sent cancels, and the cap on how long to keep re-sending
/// before conceding that the backend will not stop.
const CANCEL_RETRY: std::time::Duration = std::time::Duration::from_millis(50);
const CANCEL_ATTEMPTS: usize = 200;

/// An out-of-band cancel for `client`, re-sent until `done` is set.
///
/// Postgres honours a cancel request only against a backend that is *executing*.
/// One that lands while the backend sits idle -- between the `Parse` and the
/// `Execute` of a one-shot fetch -- is discarded by the server, and the fetch then
/// runs to completion uncancelled while the caller believes it was superseded. The
/// window is a single round trip, so a lone request loses the race whenever the
/// engine is slow to start; re-sending until the fetch reports `done` closes it.
fn pg_cancel_token_until(client: &Client, done: Arc<AtomicBool>) -> CancelToken {
    let token = client.cancel_token();
    CancelToken::new(move || {
        let token = token.clone();
        let done = done.clone();
        tokio::spawn(async move {
            for _ in 0..CANCEL_ATTEMPTS {
                if done.load(Ordering::SeqCst) {
                    break;
                }
                // A request that fails means the connection is gone, and with it
                // anything there was to cancel.
                if token.cancel_query(NoTls).await.is_err() {
                    break;
                }
                tokio::time::sleep(CANCEL_RETRY).await;
            }
        });
    })
}

/// Prepare `sql` on `client` and read its column metadata (works for an empty result).
async fn prepare_columns(client: &Client, sql: &str) -> Result<(Statement, Vec<Column>)> {
    // Postgres validates SQL at prepare time, so a user's bad custom query
    // surfaces here; map through `map_pg_err` to keep the server's message
    // instead of the bare `"db error"` that `tokio_postgres::Error` renders.
    let stmt = client.prepare(sql).await.map_err(map_pg_err)?;
    let columns = stmt
        .columns()
        .iter()
        .map(|c| Column {
            name: c.name().to_string(),
            decl_type: Some(c.type_().name().to_string()),
        })
        .collect();
    Ok((stmt, columns))
}

impl PostgresDriver {
    /// Connect over the network, drive the connection in the background, apply the
    /// read-only posture, and read the server version.
    pub async fn connect(dsn: &str, read_only: bool) -> Result<Self> {
        // TLS for Postgres isn't wired yet (the driver dials `NoTls`; adding it
        // needs a rustls connector — tracked in `security-review-2026-07.md`).
        // Rather than silently connect in cleartext when TLS is requested, refuse
        // with an actionable message. Uses the same TLS detection as the DSN
        // parser (`sslmode=`/`ssl=true`/`tls=true`/`require_ssl=true`), not a
        // narrow `sslmode=require` substring match that a raw `?ssl=true` DSN
        // would slip past into a silent cleartext connection.
        if red_core::dsn_requests_tls(dsn) {
            return Err(RedError::Connect(
                "TLS for PostgreSQL isn't supported yet in this build — turn TLS off, \
                 or tunnel the connection over SSH instead."
                    .to_string(),
            ));
        }
        let (client, connection) = tokio_postgres::connect(dsn, NoTls)
            .await
            .map_err(map_connect_err)?;
        tokio::spawn(async move {
            // When the client drops, this resolves and the task ends.
            let _ = connection.await;
        });

        // The literal escaping in `red_core` and in `pg_literal` builds strings for
        // the modern default (`standard_conforming_strings = on`, where `\` is an
        // ordinary character). That is an ordinary GUC a legacy server or a
        // per-database `ALTER DATABASE … SET` can flip, and under `off` a cell value
        // ending in `\` — attacker-controlled on a shared database, and reachable
        // through FK-follow predicates and Cmp filters — escapes the closing quote.
        // Pin it rather than assume it.
        client
            .batch_execute("SET standard_conforming_strings = on")
            .await
            .map_err(|e| RedError::Connect(e.to_string()))?;

        if read_only {
            client
                .batch_execute("SET default_transaction_read_only = on")
                .await
                .map_err(|e| RedError::Connect(e.to_string()))?;
        }

        let version: String = client
            .query_one("SHOW server_version", &[])
            .await
            .map_err(driver_err)?
            .get(0);

        Ok(Self {
            client: Arc::new(client),
            version,
            dsn: dsn.to_string(),
            read_only,
            pool: Arc::new(StdMutex::new(Vec::new())),
        })
    }

    /// Borrow a warm fetch connection: pop a live one off the free list, or open a
    /// fresh one. Dead connections (a dropped backend) are discarded, not reused.
    async fn acquire(&self) -> Result<Arc<Client>> {
        loop {
            let pooled = lock(&self.pool).pop();
            match pooled {
                Some(c) if !c.is_closed() => return Ok(c),
                Some(_) => continue, // closed: drop it and try the next
                None => break,
            }
        }
        self.open_fetch_conn().await
    }

    /// Return a fetch connection to the free list (dropping it if dead or the pool
    /// is at cap). Call only *after* disarming the fetch's cancel, so a late abort
    /// can't fire against a connection that's about to serve someone else.
    fn release(&self, client: Arc<Client>) {
        release_to(&self.pool, client);
    }

    /// Open one fetch connection with the same read-only posture as the main client.
    async fn open_fetch_conn(&self) -> Result<Arc<Client>> {
        let (client, connection) = tokio_postgres::connect(&self.dsn, NoTls)
            .await
            .map_err(|e| RedError::Connect(e.to_string()))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
            .batch_execute("SET standard_conforming_strings = on")
            .await
            .map_err(|e| RedError::Connect(e.to_string()))?;
        if self.read_only {
            client
                .batch_execute("SET default_transaction_read_only = on")
                .await
                .map_err(|e| RedError::Connect(e.to_string()))?;
        }
        Ok(Arc::new(client))
    }

    /// Run `f` on a borrowed fetch connection with `abort` armed to its cancel for
    /// the duration. Disarms *before* the connection returns to the pool, so a late
    /// `abort` never reaches a reused connection. A fetch superseded before it
    /// starts bails with `Interrupted` (a connection-scoped cancel is a no-op with
    /// nothing yet running).
    async fn with_fetch_conn<T, F, Fut>(&self, abort: &AbortSignal, f: F) -> Result<T>
    where
        F: FnOnce(Arc<Client>) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let client = self.acquire().await?;
        // Set the moment the fetch ends, so a cancel stops re-sending before the
        // connection is reusable.
        let done = Arc::new(AtomicBool::new(false));
        let guard = abort.arm(pg_cancel_token_until(&client, done.clone()));
        let result = if abort.is_aborted() {
            Err(RedError::Interrupted)
        } else {
            f(client.clone()).await
        };
        done.store(true, Ordering::SeqCst);
        drop::<ArmGuard>(guard); // disarm before the connection is reusable
        self.release(client);
        result
    }
}

#[async_trait]
impl DatabaseDriver for PostgresDriver {
    async fn ping(&self) -> Result<()> {
        self.client
            .batch_execute("SELECT 1")
            .await
            .map_err(driver_err)
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    /// Unchanged for now. Postgres binds its *database* at connect and cannot switch it
    /// on a live connection; the rebindable namespace is the schema, via `search_path`.
    /// Unlike MySQL's re-acquire-per-operation pool, a `SET search_path` here is
    /// **sticky**: it persists on the long-lived `client` and on free-list clients
    /// (which are never reset on release), so it would leak into later operations and
    /// across tabs. Doing it safely needs per-client tracking or `SET LOCAL` inside
    /// each operation's transaction —.
    fn scoped(self: Arc<Self>, _namespace: Option<&str>) -> Arc<dyn DatabaseDriver> {
        self
    }

    /// A cursor gets a pooled connection of its own, held for its whole lifetime.
    ///
    /// tokio-postgres answers pipelined requests strictly in order over one
    /// connection, so two cursors sharing the shared `client` could not interleave:
    /// the second cursor's `prepare` cannot complete until the first has streamed to
    /// the end through a channel nobody is draining. The same-connection `DiffTables`
    /// does exactly that interleaving, and hung on any table bigger than a test
    /// fixture. Cancellation is connection-scoped for the same reason — a cursor's
    /// own connection means its cancel hits its own query and nothing else.
    async fn open_cursor(&self, sql: &str, opts: QueryOptions) -> Result<Box<dyn QueryCursor>> {
        let client = self.acquire().await?;
        let (stmt, columns) = prepare_columns(&client, sql).await?;
        let stream = client
            .query_raw(&stmt, no_params())
            .await
            .map_err(driver_err)?;

        // Out-of-band cancel: a separate cancel request over a fresh connection.
        let cancel = pg_cancel_token(&client);

        Ok(Box::new(PgCursor {
            columns,
            stream: Mutex::new(Box::pin(stream)),
            cancel,
            full: opts.full_fidelity,
            exhausted: AtomicBool::new(false),
            client: Some(client),
            pool: self.pool.clone(),
        }))
    }

    async fn list_objects(&self) -> Result<Vec<SchemaMeta>> {
        let schema_rows = self
            .client
            .query(
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name NOT IN ('pg_catalog', 'information_schema') \
                 AND schema_name NOT LIKE 'pg\\_%' ORDER BY schema_name",
                &[],
            )
            .await
            .map_err(driver_err)?;

        let mut schemas = Vec::with_capacity(schema_rows.len());
        for schema_row in schema_rows {
            let schema: String = schema_row.get(0);
            // `information_schema.tables` does not list materialized views (they
            // are not in the SQL standard), so the skeleton reads `pg_class`
            // relkind instead: r/p = table, v = view, m = materialized view. One
            // query per schema either way, so the connect cost is unchanged.
            let object_rows = self
                .client
                .query(
                    "SELECT c.relname, c.relkind::text \
                     FROM pg_class c \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm') \
                     ORDER BY c.relname",
                    &[&schema],
                )
                .await
                .map_err(driver_err)?;
            let objects = object_rows
                .iter()
                .map(|row| {
                    let name: String = row.get(0);
                    let relkind: String = row.get(1);
                    ObjectMeta {
                        name,
                        kind: match relkind.as_str() {
                            "v" => ObjectKind::View,
                            "m" => ObjectKind::MaterializedView,
                            // 'r' (ordinary) and 'p' (partitioned parent) are both
                            // selectable tables as far as the explorer cares.
                            _ => ObjectKind::Table,
                        },
                    }
                })
                .collect();
            schemas.push(SchemaMeta {
                name: schema,
                objects,
            });
        }
        Ok(schemas)
    }

    async fn object_group_counts(&self) -> Result<Vec<(String, ObjectKind, usize)>> {
        // One statement, five arms, every namespace at once. Each arm labels its
        // rows with the kind it counted so the caller can route them without
        // knowing the arm order.
        let rows = self
            .client
            .query(
                "SELECT n.nspname::text, 'function', count(*)::bigint \
                   FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                  WHERE p.prokind = 'f' GROUP BY 1 \
                 UNION ALL \
                 SELECT n.nspname::text, 'procedure', count(*)::bigint \
                   FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                  WHERE p.prokind = 'p' GROUP BY 1 \
                 UNION ALL \
                 SELECT n.nspname::text, 'trigger', count(*)::bigint \
                   FROM pg_trigger t \
                   JOIN pg_class c ON c.oid = t.tgrelid \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE NOT t.tgisinternal GROUP BY 1 \
                 UNION ALL \
                 SELECT n.nspname::text, 'sequence', count(*)::bigint \
                   FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE c.relkind = 'S' GROUP BY 1 \
                 UNION ALL \
                 SELECT n.nspname::text, 'type', count(*)::bigint \
                   FROM pg_type t \
                   JOIN pg_namespace n ON n.oid = t.typnamespace \
                   LEFT JOIN pg_class c ON c.oid = t.typrelid \
                  WHERE t.typtype IN ('e', 'c') \
                    AND (t.typrelid = 0 OR c.relkind = 'c') GROUP BY 1",
                &[],
            )
            .await
            .map_err(driver_err)?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let namespace: String = row.get(0);
                let token: &str = row.get(1);
                let count: i64 = row.get(2);
                Some((
                    namespace,
                    ObjectKind::from_token(token)?,
                    count.max(0) as usize,
                ))
            })
            .collect())
    }

    async fn list_object_group(
        &self,
        namespace: &str,
        kind: ObjectKind,
    ) -> Result<Vec<ObjectMeta>> {
        // One statement per kind, each names-only and ordered, run when the user
        // expands that group. Routines carry their argument list in the display
        // name because Postgres overloads on signature: two `fn`s named the same
        // are two different objects, and a bare name would draw one row for both.
        let sql = match kind {
            ObjectKind::Function => {
                "SELECT p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')' \
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = $1 AND p.prokind = 'f' ORDER BY 1"
            }
            // `prokind` is PG 11+. On older servers procedures do not exist at
            // all, and the query simply returns nothing rather than erroring,
            // because `prokind` is still a valid column back to 11 and this
            // driver's floor is well above the versions without it.
            ObjectKind::Procedure => {
                "SELECT p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')' \
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = $1 AND p.prokind = 'p' ORDER BY 1"
            }
            // `tgisinternal` excludes the triggers Postgres creates to enforce
            // foreign keys; those are constraint plumbing, not user objects, and
            // listing them would bury the handful a user actually wrote.
            ObjectKind::Trigger => {
                "SELECT t.tgname || ' on ' || c.relname \
                 FROM pg_trigger t \
                 JOIN pg_class c ON c.oid = t.tgrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND NOT t.tgisinternal ORDER BY 1"
            }
            ObjectKind::Sequence => {
                "SELECT c.relname FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind = 'S' ORDER BY 1"
            }
            // Enums and composites. Postgres also auto-creates a composite type
            // per table, so `typrelid = 0 OR relkind = 'c'` filters those out.
            ObjectKind::Type => {
                "SELECT t.typname FROM pg_type t \
                 JOIN pg_namespace n ON n.oid = t.typnamespace \
                 LEFT JOIN pg_class c ON c.oid = t.typrelid \
                 WHERE n.nspname = $1 AND t.typtype IN ('e', 'c') \
                   AND (t.typrelid = 0 OR c.relkind = 'c') ORDER BY 1"
            }
            // Relations arrive with the skeleton; nothing lazy to fetch.
            _ => return Ok(Vec::new()),
        };
        let rows = self
            .client
            .query(sql, &[&namespace])
            .await
            .map_err(driver_err)?;
        Ok(rows
            .iter()
            .map(|row| ObjectMeta {
                name: row.get(0),
                kind,
            })
            .collect())
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableDetail> {
        // Primary-key columns.
        let pk_rows = self
            .client
            .query(
                "SELECT kcu.column_name FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON kcu.constraint_name = tc.constraint_name \
                  AND kcu.table_schema = tc.table_schema \
                 WHERE tc.constraint_type = 'PRIMARY KEY' \
                   AND tc.table_schema = $1 AND tc.table_name = $2",
                &[&schema, &table],
            )
            .await
            .map_err(driver_err)?;
        let pk: std::collections::HashSet<String> =
            pk_rows.iter().map(|r| r.get::<_, String>(0)).collect();

        // Columns. `udt_name`, not `data_type`: the latter spells types the
        // catalog way (`character varying`, `timestamp without time zone`,
        // `USER-DEFINED`, `ARRAY`), none of which is a *typname* — and this
        // string ends up quoted inside `pg_cast`'s explicit cast, where
        // `::"character varying"` is a 42704 "type does not exist" that fails
        // every copy/import into the column. `udt_name` is the typname
        // (`varchar`, `timestamptz`, the enum's own name, `_text` for arrays),
        // which both `pg_cast` and the migration typemap accept.
        let column_rows = self
            .client
            .query(
                "SELECT column_name, udt_name, is_nullable, column_default \
                 FROM information_schema.columns \
                 WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
                &[&schema, &table],
            )
            .await
            .map_err(driver_err)?;
        let columns = column_rows
            .iter()
            .map(|row| {
                let name: String = row.get(0);
                let type_name: String = row.get(1);
                let nullable: String = row.get(2);
                let default: Option<String> = row.get(3);
                // `serial`/`bigserial` columns default to `nextval('…_seq')`.
                let auto_increment = default.as_deref().is_some_and(|d| d.starts_with("nextval"));
                ColumnMeta {
                    primary_key: pk.contains(&name),
                    not_null: nullable == "NO",
                    type_name: Some(type_name),
                    default,
                    name,
                    auto_increment,
                }
            })
            .collect();

        // Foreign keys.
        let fk_rows = self
            .client
            .query(
                "SELECT kcu.column_name, ccu.table_name, ccu.column_name \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON kcu.constraint_name = tc.constraint_name \
                  AND kcu.table_schema = tc.table_schema \
                 JOIN information_schema.constraint_column_usage ccu \
                   ON ccu.constraint_name = tc.constraint_name \
                  AND ccu.table_schema = tc.table_schema \
                 WHERE tc.constraint_type = 'FOREIGN KEY' \
                   AND tc.table_schema = $1 AND tc.table_name = $2",
                &[&schema, &table],
            )
            .await
            .map_err(driver_err)?;
        let foreign_keys = fk_rows
            .iter()
            .map(|row| ForeignKeyMeta {
                column: row.get(0),
                ref_table: row.get(1),
                ref_column: row.get(2),
            })
            .collect();

        // Indexes (columns parsed out of the index definition).
        let index_rows = self
            .client
            .query(
                "SELECT indexname, indexdef FROM pg_indexes \
                 WHERE schemaname = $1 AND tablename = $2",
                &[&schema, &table],
            )
            .await
            .map_err(driver_err)?;
        let indexes = index_rows
            .iter()
            .map(|row| {
                let name: String = row.get(0);
                let def: String = row.get(1);
                IndexMeta {
                    unique: def.to_uppercase().contains("UNIQUE INDEX"),
                    columns: parse_index_columns(&def),
                    name,
                }
            })
            .collect();

        Ok(TableDetail {
            columns,
            foreign_keys,
            indexes,
        })
    }

    async fn enum_columns(
        &self,
        table: &TableRef,
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        // Each enum-typed column of the table, joined to its `pg_enum` labels in the
        // enum's own sort order. Non-enum columns simply don't join, so they're absent.
        let schema = table.schema.as_deref().unwrap_or("public");
        let rows = self
            .client
            .query(
                "SELECT a.attname, e.enumlabel \
                 FROM pg_attribute a \
                 JOIN pg_type t ON t.oid = a.atttypid \
                 JOIN pg_enum e ON e.enumtypid = t.oid \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relname = $2 AND n.nspname = $1 AND a.attnum > 0 \
                   AND NOT a.attisdropped \
                 ORDER BY a.attnum, e.enumsortorder",
                &[&schema, &table.name],
            )
            .await
            .map_err(driver_err)?;
        let mut out: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for row in &rows {
            let col: String = row.get(0);
            let label: String = row.get(1);
            out.entry(col).or_default().push(label);
        }
        Ok(out)
    }

    async fn foreign_keys(&self) -> Result<Vec<FkEdge>> {
        // One pass over the catalog: every FK column with both endpoints' schema +
        // table, ordered so a composite key's columns arrive together in key order.
        // System schemas are excluded to match `list_objects`'s visible namespaces.
        let rows = self
            .client
            .query(
                "SELECT tc.table_schema, tc.table_name, kcu.column_name, \
                        ccu.table_schema, ccu.table_name, ccu.column_name, tc.constraint_name \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON kcu.constraint_name = tc.constraint_name \
                  AND kcu.table_schema = tc.table_schema \
                 JOIN information_schema.constraint_column_usage ccu \
                   ON ccu.constraint_name = tc.constraint_name \
                  AND ccu.table_schema = tc.table_schema \
                 WHERE tc.constraint_type = 'FOREIGN KEY' \
                   AND tc.table_schema NOT IN ('pg_catalog', 'information_schema') \
                   AND tc.table_schema NOT LIKE 'pg\\_%' \
                 ORDER BY tc.table_schema, tc.table_name, tc.constraint_name, kcu.ordinal_position",
                &[],
            )
            .await
            .map_err(driver_err)?;
        let edges = crate::group_fk_edges(rows.iter().map(|r| crate::FkRow {
            from_schema: r.get(0),
            from_table: r.get(1),
            from_column: r.get(2),
            to_schema: r.get(3),
            to_table: r.get(4),
            to_column: r.get(5),
            constraint: r.get(6),
        }));
        Ok(edges)
    }

    fn contains_predicate(&self, columns: &[ColumnMeta], term: &str) -> Option<String> {
        // Postgres standard strings treat `\` literally, so no extra literal escaping.
        crate::contains_clause(
            columns,
            term,
            pg_quote,
            |c| format!("({c})::text"),
            "ILIKE",
            false,
            true,
        )
    }

    fn eq_predicate(&self, pairs: &[ColumnValue]) -> String {
        crate::eq_clause(pairs, pg_quote, false)
    }

    fn cmp_predicate(&self, preds: &[ColumnPredicate]) -> String {
        // The cast / `ILIKE` / `ESCAPE` knobs match `contains_predicate` above:
        // a column-scoped `Contains` must mean the same thing there and here.
        crate::cmp_clause(
            preds,
            pg_quote,
            |c| format!("({c})::text"),
            "ILIKE",
            false,
            true,
        )
    }

    fn fk_join_wrap(&self, base: &str, base_cols: &[String], joins: &[FkJoin]) -> String {
        crate::join_wrap(base, base_cols, joins, pg_quote)
    }

    async fn count(&self, sql: &str, abort: &AbortSignal) -> Result<i64> {
        let sql = format!("SELECT count(*) FROM ({}) AS _red", strip_trailing(sql));
        self.with_fetch_conn(abort, |client| async move {
            let row = client.query_one(&sql, &[]).await.map_err(map_pg_err)?;
            Ok(row.get(0))
        })
        .await
    }

    async fn column_stats(
        &self,
        sql: &str,
        column: &str,
        flags: red_core::StatsFlags,
        abort: &AbortSignal,
    ) -> Result<red_core::ColumnStats> {
        let sql = crate::stats_sql(sql, column, flags, pg_quote);
        self.with_fetch_conn(abort, |client| async move {
            let row = client.query_one(&sql, &[]).await.map_err(map_pg_err)?;
            // Read the one aggregate row full-fidelity, then map it positionally.
            let cells = pg_row(&row, None);
            Ok(crate::parse_stats(&cells, flags))
        })
        .await
    }

    async fn fetch_page(
        &self,
        sql: &str,
        offset: usize,
        limit: usize,
        cap: PageCap,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let sql = format!(
            "SELECT * FROM ({}) AS _red LIMIT {limit} OFFSET {offset}",
            strip_trailing(sql)
        );
        self.with_fetch_conn(abort, |client| async move {
            let (stmt, columns) = prepare_columns(&client, &sql).await?;
            let rows = client.query(&stmt, &[]).await.map_err(map_pg_err)?;
            let cap = CellCap::resolve(&cap, &columns);
            Ok(ResultPage {
                rows: rows.iter().map(|r| pg_row(r, cap)).collect(),
                columns,
            })
        })
        .await
    }

    async fn fetch_seek(
        &self,
        sql: &str,
        key: &KeySpec,
        bound: Option<&[Value]>,
        scroll: red_core::SortDirection,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let base = strip_trailing(sql);
        let bound_len = bound.map_or(0, <[Value]>::len);
        // Each placeholder carries an explicit cast: the parameter's wire type is
        // fixed by the Rust value (i64 → int8), and without the cast Postgres
        // would infer the column's narrower type (int4) and reject the bind.
        // Cast each cursor bind back to its key column's type: a text-decoded
        // cursor (uuid/timestamp/numeric key) would otherwise bind as `text` and
        // `col > $1::text` has no operator (42883). Int/real values pin their own
        // wire type and ignore the column type.
        let key_types = key.column_types();
        // `seek_clauses` only invokes the closure for `i < bound_len`, and
        // `bound_len > 0` implies `bound` is `Some`, so the empty-slice fallback is
        // never indexed — it just keeps the closure panic-free.
        let bound_vals = bound.unwrap_or(&[]);
        let (where_clause, order_by) =
            crate::seek_clauses(key, bound_len, scroll, false, pg_quote, |i| {
                format!("${}{}", i + 1, pg_cast(&bound_vals[i], key_types[i]))
            });
        let sql = format!(
            "SELECT * FROM ({base}) AS _red {where_clause}ORDER BY {order_by} LIMIT {limit}"
        );
        let boxed = pg_params(bound)?;
        self.with_fetch_conn(abort, |client| async move {
            let (stmt, columns) = prepare_columns(&client, &sql).await?;
            let params: Vec<&(dyn ToSql + Sync)> = boxed
                .iter()
                .map(|b| -> &(dyn ToSql + Sync) { b.as_ref() })
                .collect();
            let rows = client.query(&stmt, &params).await.map_err(map_pg_err)?;
            let cap = CellCap::display(crate::key_positions(key, &columns));
            Ok(ResultPage {
                rows: rows.iter().map(|r| pg_row(r, cap)).collect(),
                columns,
            })
        })
        .await
    }

    async fn fetch_seek_skip(
        &self,
        sql: &str,
        key: &KeySpec,
        from: Option<&[Value]>,
        skip: usize,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let base = strip_trailing(sql);
        let bound_len = from.map_or(0, <[Value]>::len);
        let key_types = key.column_types();
        // See `fetch_seek`: the closure runs only for `i < bound_len`, which
        // implies `from` is `Some`; the fallback keeps it panic-free.
        let from_vals = from.unwrap_or(&[]);
        let (where_clause, order_by) = crate::seek_clauses(
            key,
            bound_len,
            red_core::SortDirection::Asc,
            true,
            pg_quote,
            |i| format!("${}{}", i + 1, pg_cast(&from_vals[i], key_types[i])),
        );
        let sql = format!(
            "SELECT * FROM ({base}) AS _red {where_clause}\
             ORDER BY {order_by} LIMIT {limit} OFFSET {skip}"
        );
        let boxed = pg_params(from)?;
        self.with_fetch_conn(abort, |client| async move {
            let (stmt, columns) = prepare_columns(&client, &sql).await?;
            let params: Vec<&(dyn ToSql + Sync)> = boxed
                .iter()
                .map(|b| -> &(dyn ToSql + Sync) { b.as_ref() })
                .collect();
            let rows = client.query(&stmt, &params).await.map_err(map_pg_err)?;
            let cap = CellCap::display(crate::key_positions(key, &columns));
            Ok(ResultPage {
                rows: rows.iter().map(|r| pg_row(r, cap)).collect(),
                columns,
            })
        })
        .await
    }

    async fn key_bounds(
        &self,
        sql: &str,
        key: &KeySpec,
        abort: &AbortSignal,
    ) -> Result<Option<(i64, i64)>> {
        let col = pg_quote(&key.column);
        let sql = format!(
            "SELECT min({col}), max({col}) FROM ({}) AS _red",
            strip_trailing(sql)
        );
        self.with_fetch_conn(abort, |client| async move {
            let rows = client.query(&sql, &[]).await.map_err(map_pg_err)?;
            Ok(rows.first().map(|r| pg_row(r, None)).and_then(|cells| {
                match (cells.first(), cells.get(1)) {
                    (Some(Value::Integer(min)), Some(Value::Integer(max))) => Some((*min, *max)),
                    _ => None,
                }
            }))
        })
        .await
    }

    async fn execute_abort(&self, sql: &str, abort: &AbortSignal) -> Result<u64> {
        // Run the write on a borrowed pool connection, never the shared `client`
        // that backs the live cursor: a `BEGIN`/`COMMIT` pipelined onto the cursor's
        // connection can entangle an in-flight stream ("another command is already in
        // progress"). The pool connection carries the same read-only posture, so a
        // write on a read-only session is still rejected at the engine.
        // `with_fetch_conn` arms `abort` to this connection's cancel for the
        // duration, so a write wedged on a lock is stoppable (57014 → Interrupted
        // via `map_pg_err`, rolled back below).
        self.with_fetch_conn(abort, |client| async move {
            client.batch_execute("BEGIN").await.map_err(driver_err)?;
            match client.execute(sql, &[]).await {
                Ok(affected) => {
                    client.batch_execute("COMMIT").await.map_err(driver_err)?;
                    Ok(affected)
                }
                Err(e) => {
                    crate::warn_rollback(client.batch_execute("ROLLBACK").await, "execute");
                    Err(map_pg_err(e))
                }
            }
        })
        .await
    }

    async fn execute_batch_abort(
        &self,
        statements: &[String],
        abort: &AbortSignal,
    ) -> Result<Vec<u64>> {
        if statements.is_empty() {
            return Ok(Vec::new());
        }
        self.with_fetch_conn(abort, |client| async move {
            client.batch_execute("BEGIN").await.map_err(driver_err)?;
            let mut affected = Vec::with_capacity(statements.len());
            for sql in statements {
                // Re-checked *between* statements, not just before `BEGIN`. The
                // out-of-band cancel only reaches the statement currently executing,
                // so an abort that fires in the gap — or one that races a
                // statement's completion — was a no-op, and the loop ran every
                // remaining statement through to `COMMIT`. A 200-statement script
                // could blow through its timeout and still report success.
                if abort.is_aborted() {
                    crate::warn_rollback(client.batch_execute("ROLLBACK").await, "execute_batch");
                    return Err(RedError::Interrupted);
                }
                match client.execute(sql.as_str(), &[]).await {
                    Ok(n) => affected.push(n),
                    Err(e) => {
                        crate::warn_rollback(
                            client.batch_execute("ROLLBACK").await,
                            "execute_batch",
                        );
                        return Err(map_pg_err(e));
                    }
                }
            }
            client.batch_execute("COMMIT").await.map_err(driver_err)?;
            Ok(affected)
        })
        .await
    }

    async fn apply_edits(&self, ops: &[EditOp]) -> Result<u64> {
        if ops.is_empty() {
            return Ok(0);
        }
        // Borrow a pool connection so the batch's transaction never shares the
        // cursor's connection; see `execute`.
        let client = self.acquire().await?;
        let result = async {
            client.batch_execute("BEGIN").await.map_err(driver_err)?;
            let mut total = 0u64;
            for op in ops {
                // Typed placeholders (`$n::int8`, …) like the seek path: the value's
                // wire type is fixed by the Rust value, the cast keeps Postgres from
                // re-inferring.
                let (sql, params) = crate::edit_sql(op, pg_quote, |i, cv| {
                    format!("${}{}", i + 1, pg_cast(&cv.value, cv.decl_type.as_deref()))
                });
                let owned: Vec<Value> = params.iter().map(|v| (*v).clone()).collect();
                let boxed = pg_params(Some(&owned))?;
                let refs: Vec<&(dyn ToSql + Sync)> = boxed
                    .iter()
                    .map(|b| -> &(dyn ToSql + Sync) { b.as_ref() })
                    .collect();
                match client.execute(&sql, &refs).await {
                    Ok(affected) => {
                        if affected != 1 {
                            crate::warn_rollback(
                                client.batch_execute("ROLLBACK").await,
                                "apply_edits",
                            );
                            return Err(crate::edit_count_err(op, affected));
                        }
                        total += affected;
                    }
                    Err(e) => {
                        crate::warn_rollback(client.batch_execute("ROLLBACK").await, "apply_edits");
                        return Err(map_pg_err(e));
                    }
                }
            }
            client.batch_execute("COMMIT").await.map_err(driver_err)?;
            Ok(total)
        }
        .await;
        self.release(client);
        result
    }

    async fn insert_rows(
        &self,
        table: &TableRef,
        columns: &[Column],
        rows: &[Vec<Value>],
    ) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        // Borrow a pool connection so the transaction never shares the cursor's
        // connection; see `execute`/`apply_edits`.
        let client = self.acquire().await?;
        let result = async {
            client.batch_execute("BEGIN").await.map_err(driver_err)?;
            let max = crate::insert_chunk_rows(columns.len(), PG_PARAM_CAP);
            let mut total = 0u64;
            for chunk in rows.chunks(max) {
                // Typed placeholders (`$n::int8`, `$n::text::"uuid"`, …) like the
                // edit path, so Postgres can't re-infer the parameter type.
                let (sql, params) =
                    crate::insert_sql(table, columns, chunk, pg_quote, |i, v, dt| {
                        format!("${}{}", i + 1, pg_cast(v, dt))
                    });
                let owned: Vec<Value> = params.iter().map(|v| (*v).clone()).collect();
                let boxed = pg_params(Some(&owned))?;
                let refs: Vec<&(dyn ToSql + Sync)> = boxed
                    .iter()
                    .map(|b| -> &(dyn ToSql + Sync) { b.as_ref() })
                    .collect();
                match client.execute(&sql, &refs).await {
                    Ok(affected) => total += affected,
                    Err(e) => {
                        crate::warn_rollback(client.batch_execute("ROLLBACK").await, "insert_rows");
                        return Err(map_pg_err(e));
                    }
                }
            }
            client.batch_execute("COMMIT").await.map_err(driver_err)?;
            Ok(total)
        }
        .await;
        self.release(client);
        result
    }

    async fn clear_table(&self, table: &TableRef) -> Result<u64> {
        let qualify = match &table.schema {
            Some(s) if !s.is_empty() => format!("{}.{}", pg_quote(s), pg_quote(&table.name)),
            _ => pg_quote(&table.name),
        };
        let client = self.acquire().await?;
        let result = async {
            client.batch_execute("BEGIN").await.map_err(driver_err)?;
            match client.execute(&format!("DELETE FROM {qualify}"), &[]).await {
                Ok(affected) => {
                    client.batch_execute("COMMIT").await.map_err(driver_err)?;
                    Ok(affected)
                }
                Err(e) => {
                    crate::warn_rollback(client.batch_execute("ROLLBACK").await, "clear_table");
                    Err(map_pg_err(e))
                }
            }
        }
        .await;
        self.release(client);
        result
    }

    async fn create_table(&self, table: &TableRef, columns: &[ColumnMeta]) -> Result<u64> {
        let sql = crate::create_table_sql(table, columns, DbKind::Postgres, pg_quote);
        self.execute(&sql).await
    }

    fn quote_table(&self, table: &TableRef) -> String {
        crate::qualify_table(table, pg_quote)
    }

    fn quote_ident(&self, ident: &str) -> String {
        pg_quote(ident)
    }

    fn diff_order_clause(&self, key: &str, key_is_text: bool) -> String {
        // Postgres defaults to locale collation ('apple' < 'Banana' under ICU)
        // and NULLS LAST — both disagree with the merge-walk's byte/NULLs-first
        // order. `COLLATE "C"` is byte order, valid only on collatable types.
        if key_is_text {
            format!("{} COLLATE \"C\" ASC NULLS FIRST", pg_quote(key))
        } else {
            format!("{} ASC NULLS FIRST", pg_quote(key))
        }
    }

    async fn create_index(
        &self,
        table: &TableRef,
        name: &str,
        unique: bool,
        columns: &[String],
    ) -> Result<u64> {
        let sql = crate::create_index_sql(table, name, unique, columns, DbKind::Postgres, pg_quote);
        self.execute(&sql).await
    }

    async fn add_foreign_key(
        &self,
        child: &TableRef,
        columns: &[String],
        parent: &TableRef,
        ref_columns: &[String],
    ) -> Result<u64> {
        let sql = crate::add_fk_sql(child, columns, parent, ref_columns, pg_quote);
        self.execute(&sql).await
    }

    async fn health(&self, namespace: Option<&str>) -> Result<red_core::health::HealthReport> {
        use crate::{human_bytes, now_unix};
        use red_core::health::{
            Finding, FindingKind, HealthReport, Severity, SizeTotals, TableSize, UnavailableCheck,
            floors,
        };

        let mut report = HealthReport::new(
            red_core::DbKind::Postgres,
            namespace.map(str::to_string),
            now_unix(),
        );
        // One scope predicate, applied to every check, so a report scoped to a
        // schema is scoped consistently rather than per query.
        let scope: Option<String> = namespace.map(str::to_string);

        // --- sizes -----------------------------------------------------------
        // `pg_total_relation_size` includes indexes and TOAST, which is what "how
        // big is this table" means to anyone asking. Row counts come from
        // `reltuples` (the planner's estimate): a COUNT(*) per table would turn a
        // report into a scan.
        let rows = self
            .client
            .query(
                "SELECT n.nspname, c.relname, pg_total_relation_size(c.oid), \
                        pg_indexes_size(c.oid), c.reltuples::bigint \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN ('r', 'p') \
                   AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
                   AND ($1::text IS NULL OR n.nspname = $1) \
                 ORDER BY pg_total_relation_size(c.oid) DESC LIMIT 100",
                &[&scope],
            )
            .await
            .map_err(driver_err)?;
        let mut totals = SizeTotals::default();
        for row in &rows {
            let (schema, name): (String, String) = (row.get(0), row.get(1));
            let bytes: i64 = row.get(2);
            let index_bytes: i64 = row.get(3);
            totals.bytes += bytes.max(0) as u64;
            totals.index_bytes += index_bytes.max(0) as u64;
            totals.table_count += 1;
            report.tables.push(TableSize {
                table: TableRef {
                    schema: Some(schema),
                    name,
                },
                bytes: bytes.max(0) as u64,
                index_bytes: index_bytes.max(0) as u64,
                estimated_rows: row.get(4),
            });
        }
        report.totals = totals;

        // --- unused indexes --------------------------------------------------
        // `idx_scan = 0` since the last stats reset. Unique/PK-backing indexes are
        // excluded: they are constraints, not access paths, and dropping one is a
        // different decision entirely.
        match self
            .client
            .query(
                "SELECT s.schemaname, s.relname, s.indexrelname, pg_relation_size(s.indexrelid) \
                 FROM pg_stat_user_indexes s \
                 JOIN pg_index i ON i.indexrelid = s.indexrelid \
                 WHERE s.idx_scan = 0 AND NOT i.indisunique AND NOT i.indisprimary \
                   AND ($1::text IS NULL OR s.schemaname = $1) \
                   AND pg_relation_size(s.indexrelid) > $2 \
                 ORDER BY pg_relation_size(s.indexrelid) DESC LIMIT 50",
                &[&scope, &(floors::BYTES as i64)],
            )
            .await
        {
            Ok(rows) => {
                for row in &rows {
                    let (schema, table, index): (String, String, String) =
                        (row.get(0), row.get(1), row.get(2));
                    let bytes: i64 = row.get(3);
                    report.findings.push(Finding {
                        severity: Severity::Warn,
                        kind: FindingKind::UnusedIndex,
                        object: Some(TableRef {
                            schema: Some(schema.clone()),
                            name: table.clone(),
                        }),
                        title: format!("Index {index} has never been used"),
                        detail: format!(
                            "{} on {schema}.{table}, and no scan has hit it since the last \
                             statistics reset. Confirm the reset time before dropping it.",
                            human_bytes(bytes.max(0) as u64)
                        ),
                        suggested_sql: Some(format!(
                            "DROP INDEX {}.{};",
                            self.quote_ident(&schema),
                            self.quote_ident(&index)
                        )),
                    });
                }
            }
            Err(e) => report.unavailable.push(UnavailableCheck {
                kind: FindingKind::UnusedIndex,
                reason: format!("pg_stat_user_indexes is not readable: {e}"),
            }),
        }

        // --- dead tuples / vacuum lag ---------------------------------------
        match self
            .client
            .query(
                "SELECT schemaname, relname, n_dead_tup, n_live_tup, \
                        COALESCE(last_autovacuum, last_vacuum)::text \
                 FROM pg_stat_user_tables \
                 WHERE n_dead_tup > $2 AND n_dead_tup > n_live_tup * 0.2 \
                   AND ($1::text IS NULL OR schemaname = $1) \
                 ORDER BY n_dead_tup DESC LIMIT 25",
                &[&scope, &floors::ROWS],
            )
            .await
        {
            Ok(rows) => {
                for row in &rows {
                    let (schema, table): (String, String) = (row.get(0), row.get(1));
                    let (dead, live): (i64, i64) = (row.get(2), row.get(3));
                    let vacuumed: Option<String> = row.get(4);
                    report.findings.push(Finding {
                        severity: Severity::Warn,
                        kind: FindingKind::DeadTuples,
                        object: Some(TableRef {
                            schema: Some(schema.clone()),
                            name: table.clone(),
                        }),
                        title: format!("{schema}.{table} is mostly dead tuples"),
                        detail: format!(
                            "{dead} dead against {live} live rows. Last vacuum: {}.",
                            vacuumed.as_deref().unwrap_or("never")
                        ),
                        suggested_sql: Some(format!(
                            "VACUUM (ANALYZE) {}.{};",
                            self.quote_ident(&schema),
                            self.quote_ident(&table)
                        )),
                    });
                }
            }
            Err(e) => report.unavailable.push(UnavailableCheck {
                kind: FindingKind::DeadTuples,
                reason: format!("pg_stat_user_tables is not readable: {e}"),
            }),
        }

        // --- sequential-scan-heavy tables ------------------------------------
        match self
            .client
            .query(
                "SELECT schemaname, relname, seq_scan, idx_scan, n_live_tup \
                 FROM pg_stat_user_tables \
                 WHERE n_live_tup > $2 AND seq_scan > COALESCE(idx_scan, 0) * 4 \
                   AND seq_scan > 100 AND ($1::text IS NULL OR schemaname = $1) \
                 ORDER BY seq_scan DESC LIMIT 25",
                &[&scope, &floors::ROWS],
            )
            .await
        {
            Ok(rows) => {
                for row in &rows {
                    let (schema, table): (String, String) = (row.get(0), row.get(1));
                    let (seq, idx, live): (i64, Option<i64>, i64) =
                        (row.get(2), row.get(3), row.get(4));
                    report.findings.push(Finding {
                        severity: Severity::Warn,
                        kind: FindingKind::SeqScanHeavy,
                        object: Some(TableRef {
                            schema: Some(schema.clone()),
                            name: table.clone(),
                        }),
                        title: format!("{schema}.{table} is read by sequential scan"),
                        detail: format!(
                            "{seq} sequential scans against {} index scans, over ~{live} rows. \
                             Something is querying it on an unindexed column.",
                            idx.unwrap_or(0)
                        ),
                        suggested_sql: None,
                    });
                }
            }
            Err(e) => report.unavailable.push(UnavailableCheck {
                kind: FindingKind::SeqScanHeavy,
                reason: format!("pg_stat_user_tables is not readable: {e}"),
            }),
        }

        // --- tables with no primary key --------------------------------------
        if let Ok(rows) = self
            .client
            .query(
                "SELECT n.nspname, c.relname, c.reltuples::bigint \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind = 'r' \
                   AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
                   AND ($1::text IS NULL OR n.nspname = $1) \
                   AND c.reltuples > $2 \
                   AND NOT EXISTS (SELECT 1 FROM pg_constraint k \
                                   WHERE k.conrelid = c.oid AND k.contype = 'p') \
                 ORDER BY c.reltuples DESC LIMIT 25",
                &[&scope, &(floors::ROWS as f32)],
            )
            .await
        {
            for row in &rows {
                let (schema, table): (String, String) = (row.get(0), row.get(1));
                let rows_est: i64 = row.get(2);
                report.findings.push(Finding {
                    severity: Severity::Warn,
                    kind: FindingKind::NoPrimaryKey,
                    object: Some(TableRef {
                        schema: Some(schema.clone()),
                        name: table.clone(),
                    }),
                    title: format!("{schema}.{table} has no primary key"),
                    // Named for the consequence the user will actually meet: RED
                    // itself cannot offer in-grid editing without a row identity.
                    detail: format!(
                        "~{rows_est} rows with no unique row identity. Replication, \
                         de-duplication, and in-grid editing all need one."
                    ),
                    suggested_sql: None,
                });
            }
        }

        // --- foreign keys with no supporting index ---------------------------
        // Computed from the catalog rather than from `foreign_keys()` so it is one
        // round trip: the child-side index must have the FK columns as a prefix,
        // which is what the array-prefix comparison below checks.
        if let Ok(rows) = self
            .client
            .query(
                "SELECT n.nspname, c.relname, con.conname \
                 FROM pg_constraint con \
                 JOIN pg_class c ON c.oid = con.conrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE con.contype = 'f' \
                   AND ($1::text IS NULL OR n.nspname = $1) \
                   AND c.reltuples > $2 \
                   AND NOT EXISTS ( \
                     SELECT 1 FROM pg_index i \
                     WHERE i.indrelid = con.conrelid \
                       AND (i.indkey::int2[])[0:array_length(con.conkey, 1) - 1] \
                           OPERATOR(pg_catalog.=) con.conkey) \
                 ORDER BY c.reltuples DESC LIMIT 25",
                &[&scope, &(floors::ROWS as f32)],
            )
            .await
        {
            for row in &rows {
                let (schema, table, constraint): (String, String, String) =
                    (row.get(0), row.get(1), row.get(2));
                report.findings.push(Finding {
                    severity: Severity::Bad,
                    kind: FindingKind::MissingFkIndex,
                    object: Some(TableRef {
                        schema: Some(schema.clone()),
                        name: table.clone(),
                    }),
                    title: format!("Foreign key {constraint} has no index"),
                    detail: format!(
                        "Every delete or key update on the parent scans {schema}.{table} \
                         to check this constraint, and takes a lock while it does."
                    ),
                    suggested_sql: None,
                });
            }
        }

        // Bloat is deliberately not estimated here: the statistics-based estimate
        // is famously wrong on tables with unusual column ordering, and RED will
        // not add a pgstattuple dependency or ask anyone to install an extension.
        report.unavailable.push(UnavailableCheck {
            kind: FindingKind::Bloat,
            reason: "not estimated: a statistics-based guess is unreliable. Install \
                     pgstattuple and measure if you need the number."
                .to_string(),
        });
        Ok(report)
    }

    async fn server_sessions(&self) -> Result<(Vec<red_core::ServerSession>, bool)> {
        // `pg_blocking_pids` is the whole reason this is one query and not two: it
        // resolves the lock graph server-side, so RED never walks pg_locks itself.
        // Ordered longest-running first and capped, per the trait's contract.
        let rows = self
            .client
            .query(
                "SELECT pid, usename, application_name, client_addr::text, datname, \
                        state, wait_event_type || ':' || wait_event AS wait, query, \
                        EXTRACT(EPOCH FROM (now() - COALESCE(query_start, backend_start))), \
                        pg_blocking_pids(pid), pid = pg_backend_pid() \
                 FROM pg_stat_activity \
                 WHERE backend_type = 'client backend' \
                 ORDER BY COALESCE(query_start, backend_start) ASC NULLS LAST \
                 LIMIT 500",
                &[],
            )
            .await
            .map_err(driver_err)?;

        let mut restricted = false;
        let sessions = rows
            .iter()
            .map(|row| {
                let pid: i32 = row.get(0);
                let query: Option<String> = row.get(7);
                // Postgres substitutes this string for a role that may not read
                // other backends' SQL. Surface it as "not visible" rather than as
                // a statement that literally says that.
                let hidden = query.as_deref() == Some("<insufficient privilege>");
                restricted |= hidden;
                let blocked: Vec<i32> = row.get(9);
                red_core::ServerSession {
                    key: red_core::SessionKey(pid.to_string()),
                    user: row.get(1),
                    application: row.get::<_, Option<String>>(2).filter(|s| !s.is_empty()),
                    client_addr: row.get(3),
                    database: row.get(4),
                    state: row.get::<_, Option<String>>(5).unwrap_or_default(),
                    wait: row.get(6),
                    blocked_by: blocked
                        .into_iter()
                        .map(|p| red_core::SessionKey(p.to_string()))
                        .collect(),
                    query: if hidden { None } else { query },
                    elapsed_secs: row.get::<_, Option<f64>>(8).unwrap_or(0.0),
                    is_self: row.get(10),
                }
            })
            .collect();
        Ok((sessions, restricted))
    }

    async fn kill_session(
        &self,
        key: &red_core::SessionKey,
        mode: red_core::KillMode,
    ) -> Result<()> {
        if self.read_only {
            return Err(RedError::Query("this connection is read-only".into()));
        }
        // Parsed, not interpolated: the key came from this driver as a pid, and
        // parsing it back is what guarantees no SQL can ride in on it.
        let pid: i32 = key
            .0
            .parse()
            .map_err(|_| RedError::Driver(format!("not a Postgres backend pid: {key}")))?;
        let sql = match mode {
            red_core::KillMode::Cancel => "SELECT pg_cancel_backend($1)",
            red_core::KillMode::Terminate => "SELECT pg_terminate_backend($1)",
        };
        let row = self
            .client
            .query_one(sql, &[&pid])
            .await
            .map_err(driver_err)?;
        // Both functions answer false when the pid is gone or out of reach, which
        // is a real outcome and not an error the caller should swallow.
        if row.get::<_, bool>(0) {
            Ok(())
        } else {
            Err(RedError::Driver(format!(
                "the server refused to stop backend {pid}: it may have already \
                 finished, or your role may not be permitted to signal it"
            )))
        }
    }

    /// A routine gets `None`: `pg_get_functiondef` already returns `CREATE OR
    /// REPLACE FUNCTION`, so a drop is not only unnecessary but harmful — bare
    /// `DROP FUNCTION f` is ambiguous the moment `f` is overloaded.
    ///
    /// A trigger is the one kind that needs its table in the statement, which is
    /// exactly the half the tree's `<name> on <table>` label carries.
    fn drop_object_sql(&self, namespace: &str, name: &str, kind: ObjectKind) -> Option<String> {
        match kind {
            ObjectKind::View => Some(format!(
                "DROP VIEW IF EXISTS {}.{}",
                self.quote_ident(namespace),
                self.quote_ident(name)
            )),
            ObjectKind::Trigger => {
                let (trigger, table) = name.split_once(" on ")?;
                Some(format!(
                    "DROP TRIGGER IF EXISTS {} ON {}.{}",
                    self.quote_ident(trigger.trim()),
                    self.quote_ident(namespace),
                    self.quote_ident(table.trim())
                ))
            }
            _ => None,
        }
    }

    async fn object_ddl(&self, namespace: &str, name: &str, kind: ObjectKind) -> Result<String> {
        // Postgres has no `SHOW CREATE`. Views and routines have a catalog
        // function that returns their source exactly; a *table* does not, so it is
        // assembled below from columns + constraints + indexes + comments.
        match kind {
            ObjectKind::View | ObjectKind::MaterializedView => {
                let materialized = kind == ObjectKind::MaterializedView;
                let row = self
                    .client
                    .query_opt(
                        "SELECT pg_get_viewdef(c.oid, true) \
                         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1 AND c.relname = $2",
                        &[&namespace, &name],
                    )
                    .await
                    .map_err(driver_err)?
                    .ok_or_else(|| RedError::Driver(format!("{name} not found in {namespace}")))?;
                let body: String = row.get(0);
                let what = if materialized {
                    "MATERIALIZED VIEW"
                } else {
                    "VIEW"
                };
                return Ok(format!(
                    "CREATE {what} {}.{} AS\n{}\n",
                    self.quote_ident(namespace),
                    self.quote_ident(name),
                    body.trim_end()
                ));
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                // The tree's routine label carries the identity arguments so
                // overloads are distinguishable; `to_regprocedure` parses exactly
                // that spelling back into the one routine it names.
                let signature = format!("{}.{}", self.quote_ident(namespace), name);
                let row = self
                    .client
                    .query_opt(
                        "SELECT pg_get_functiondef(to_regprocedure($1)::oid)",
                        &[&signature],
                    )
                    .await
                    .map_err(driver_err)?
                    .ok_or_else(|| RedError::Driver(format!("{name} not found in {namespace}")))?;
                let body: Option<String> = row.get(0);
                return body.map(|b| format!("{}\n", b.trim_end())).ok_or_else(|| {
                    RedError::Driver(format!("no definition for {name} in {namespace}"))
                });
            }
            ObjectKind::Trigger => {
                let trigger = name.split(" on ").next().unwrap_or(name);
                let row = self
                    .client
                    .query_opt(
                        "SELECT pg_get_triggerdef(t.oid, true) \
                         FROM pg_trigger t \
                         JOIN pg_class c ON c.oid = t.tgrelid \
                         JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1 AND t.tgname = $2 AND NOT t.tgisinternal",
                        &[&namespace, &trigger],
                    )
                    .await
                    .map_err(driver_err)?
                    .ok_or_else(|| RedError::Driver(format!("{name} not found in {namespace}")))?;
                let body: String = row.get(0);
                return Ok(format!("{};\n", body.trim_end()));
            }
            ObjectKind::Sequence => {
                let row = self
                    .client
                    .query_opt(
                        "SELECT data_type::text, start_value, increment, min_value, max_value, \
                                cycle \
                         FROM pg_sequences WHERE schemaname = $1 AND sequencename = $2",
                        &[&namespace, &name],
                    )
                    .await
                    .map_err(driver_err)?
                    .ok_or_else(|| RedError::Driver(format!("{name} not found in {namespace}")))?;
                let (ty, start, inc): (String, i64, i64) = (row.get(0), row.get(1), row.get(2));
                let (min, max, cycle): (i64, i64, bool) = (row.get(3), row.get(4), row.get(5));
                return Ok(format!(
                    "CREATE SEQUENCE {}.{}\n    AS {ty}\n    START WITH {start}\n    \
                     INCREMENT BY {inc}\n    MINVALUE {min}\n    MAXVALUE {max}\n    {};\n",
                    self.quote_ident(namespace),
                    self.quote_ident(name),
                    if cycle { "CYCLE" } else { "NO CYCLE" },
                ));
            }
            ObjectKind::Type => {
                // Enums render as their label list; a composite as its attributes.
                let labels = self
                    .client
                    .query(
                        "SELECT e.enumlabel FROM pg_enum e \
                         JOIN pg_type t ON t.oid = e.enumtypid \
                         JOIN pg_namespace n ON n.oid = t.typnamespace \
                         WHERE n.nspname = $1 AND t.typname = $2 ORDER BY e.enumsortorder",
                        &[&namespace, &name],
                    )
                    .await
                    .map_err(driver_err)?;
                if !labels.is_empty() {
                    let variants: Vec<String> = labels
                        .iter()
                        .map(|r| format!("    '{}'", r.get::<_, String>(0).replace('\'', "''")))
                        .collect();
                    return Ok(format!(
                        "CREATE TYPE {}.{} AS ENUM (\n{}\n);\n",
                        self.quote_ident(namespace),
                        self.quote_ident(name),
                        variants.join(",\n")
                    ));
                }
                let attrs = self
                    .client
                    .query(
                        "SELECT a.attname, format_type(a.atttypid, a.atttypmod) \
                         FROM pg_attribute a \
                         JOIN pg_class c ON c.oid = a.attrelid \
                         JOIN pg_type t ON t.typrelid = c.oid \
                         JOIN pg_namespace n ON n.oid = t.typnamespace \
                         WHERE n.nspname = $1 AND t.typname = $2 AND a.attnum > 0 \
                           AND NOT a.attisdropped ORDER BY a.attnum",
                        &[&namespace, &name],
                    )
                    .await
                    .map_err(driver_err)?;
                let cols: Vec<String> = attrs
                    .iter()
                    .map(|r| {
                        format!(
                            "    {} {}",
                            self.quote_ident(&r.get::<_, String>(0)),
                            r.get::<_, String>(1)
                        )
                    })
                    .collect();
                return Ok(format!(
                    "CREATE TYPE {}.{} AS (\n{}\n);\n",
                    self.quote_ident(namespace),
                    self.quote_ident(name),
                    cols.join(",\n")
                ));
            }
            ObjectKind::Table => {}
        }

        // --- Table: assembled, and honest about it. ---
        //
        // pg_dump is a large program because faithful Postgres DDL is genuinely
        // hard (partitioning, inheritance, RLS, storage parameters, grants,
        // ownership). RED assembles the parts a reader wants and states the rest
        // as a limitation rather than silently dropping it. Nothing executes this.
        let mut out = String::new();
        out.push_str(
            "-- Assembled by RED from the Postgres catalog. Covers columns, constraints,\n\
             -- indexes, and comments. Does NOT cover: partitioning, inheritance, RLS,\n\
             -- storage parameters, grants, or ownership. Use pg_dump for a migration.\n\n",
        );

        let columns = self
            .client
            .query(
                "SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                        pg_get_expr(d.adbin, d.adrelid), a.attidentity::text \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 \
                   AND NOT a.attisdropped ORDER BY a.attnum",
                &[&namespace, &name],
            )
            .await
            .map_err(driver_err)?;
        if columns.is_empty() {
            return Err(RedError::Driver(format!("{name} not found in {namespace}")));
        }

        let mut parts: Vec<String> = Vec::new();
        for row in &columns {
            let col: String = row.get(0);
            let ty: String = row.get(1);
            let not_null: bool = row.get(2);
            let default: Option<String> = row.get(3);
            let identity: String = row.get(4);
            let mut line = format!("    {} {ty}", self.quote_ident(&col));
            match identity.as_str() {
                "a" => line.push_str(" GENERATED ALWAYS AS IDENTITY"),
                "d" => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                // A `serial` column is a plain integer with a nextval default, so
                // it renders as its default rather than as an identity clause.
                _ => {
                    if let Some(d) = default {
                        line.push_str(&format!(" DEFAULT {d}"));
                    }
                }
            }
            if not_null {
                line.push_str(" NOT NULL");
            }
            parts.push(line);
        }

        // Table constraints, engine-rendered: `pg_get_constraintdef` spells PK,
        // UNIQUE, CHECK, FK, and EXCLUDE correctly so RED does not have to.
        let constraints = self
            .client
            .query(
                "SELECT con.conname, pg_get_constraintdef(con.oid) \
                 FROM pg_constraint con \
                 JOIN pg_class c ON c.oid = con.conrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 ORDER BY con.contype, con.conname",
                &[&namespace, &name],
            )
            .await
            .map_err(driver_err)?;
        for row in &constraints {
            let cname: String = row.get(0);
            let def: String = row.get(1);
            parts.push(format!("    CONSTRAINT {} {def}", self.quote_ident(&cname)));
        }

        out.push_str(&format!(
            "CREATE TABLE {}.{} (\n{}\n);\n",
            self.quote_ident(namespace),
            self.quote_ident(name),
            parts.join(",\n")
        ));

        // Indexes that are not already implied by a constraint above.
        let indexes = self
            .client
            .query(
                "SELECT pg_get_indexdef(i.indexrelid) \
                 FROM pg_index i \
                 JOIN pg_class c ON c.oid = i.indrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 \
                   AND NOT i.indisprimary AND NOT i.indisunique \
                 ORDER BY 1",
                &[&namespace, &name],
            )
            .await
            .map_err(driver_err)?;
        if !indexes.is_empty() {
            out.push('\n');
            for row in &indexes {
                out.push_str(&format!("{};\n", row.get::<_, String>(0)));
            }
        }

        // Comments last, the way pg_dump orders them.
        let comments = self
            .client
            .query(
                "SELECT a.attname, col_description(c.oid, a.attnum) \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 \
                   AND NOT a.attisdropped AND col_description(c.oid, a.attnum) IS NOT NULL \
                 ORDER BY a.attnum",
                &[&namespace, &name],
            )
            .await
            .map_err(driver_err)?;
        let table_comment = self
            .client
            .query_opt(
                "SELECT obj_description(c.oid) FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2",
                &[&namespace, &name],
            )
            .await
            .map_err(driver_err)?
            .and_then(|r| r.get::<_, Option<String>>(0));
        if table_comment.is_some() || !comments.is_empty() {
            out.push('\n');
        }
        if let Some(c) = table_comment {
            out.push_str(&format!(
                "COMMENT ON TABLE {}.{} IS '{}';\n",
                self.quote_ident(namespace),
                self.quote_ident(name),
                c.replace('\'', "''")
            ));
        }
        for row in &comments {
            let col: String = row.get(0);
            let c: String = row.get(1);
            out.push_str(&format!(
                "COMMENT ON COLUMN {}.{}.{} IS '{}';\n",
                self.quote_ident(namespace),
                self.quote_ident(name),
                self.quote_ident(&col),
                c.replace('\'', "''")
            ));
        }
        Ok(out)
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<QueryPlan> {
        // Default `FORMAT TEXT`: the most stable parse target, and avoids the
        // JSON dependency. Plain `EXPLAIN` never executes the statement;
        // `EXPLAIN ANALYZE` does (the caller gates it to read queries, and a
        // read-only connection rejects an underlying write at the engine anyway).
        let verb = if analyze {
            "EXPLAIN ANALYZE "
        } else {
            "EXPLAIN "
        };
        let sql = format!("{verb}{}", strip_trailing(sql));
        let rows = self.client.query(&sql, &[]).await.map_err(map_pg_err)?;
        let text = rows
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(crate::plan::from_text_tree(&text, analyze))
    }

    async fn export(
        &self,
        sql: &str,
        path: &Path,
        format: ExportFormat,
        cancel: Arc<AtomicBool>,
        progress: UnboundedSender<u64>,
    ) -> Result<u64> {
        let sql = format!("SELECT * FROM ({}) AS _red", strip_trailing(sql));
        // Its own pooled connection, for the reason `open_cursor` documents: an
        // export streams for as long as the table is big, and on the shared client
        // every other request would queue behind it.
        let conn = self.acquire().await?;
        let (stmt, columns) = prepare_columns(&conn, &sql).await?;
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();

        let stream = conn
            .query_raw(&stmt, no_params())
            .await
            .map_err(driver_err)?;
        futures_util::pin_mut!(stream);

        let file = File::create(path).map_err(driver_err)?;
        let out = BufWriter::new(file);
        let table = crate::format::sql_table_name(path);
        let mut writer = ExportWriter::begin(out, format, names, table).map_err(driver_err)?;
        let mut throttle = ProgressThrottle::new(progress);

        // Bail on cancel: drop the writer, remove the partial file, and report
        // interruption; never leave a truncated CSV/JSON behind.
        macro_rules! bail_if_cancelled {
            () => {
                if cancel.load(Ordering::Relaxed) {
                    drop(writer);
                    let _ = std::fs::remove_file(path);
                    return Err(RedError::Interrupted);
                }
            };
        }

        while let Some(row) = stream.next().await {
            bail_if_cancelled!();
            let row = row.map_err(map_pg_err)?;
            let cells = pg_row(&row, None);
            writer.write_row(&cells).map_err(driver_err)?;
            throttle.tick(writer.written());
        }
        // The loop only ends when the stream is exhausted, so the connection is
        // clean here. Every other exit closes it by dropping `conn`, which is what a
        // half-answered connection deserves.
        self.release(conn);
        writer.finish().map_err(driver_err)
    }
}

/// The async-side cursor: column metadata + the live row stream behind a `Mutex`
/// (so `next_window(&self)` can pull) + the out-of-band cancel token.
struct PgCursor {
    columns: Vec<Column>,
    stream: Mutex<Pin<Box<RowStream>>>,
    cancel: CancelToken,
    /// Read cells at full fidelity (the table-copy read) rather than the display
    /// fat-cell cap; see [`QueryOptions::full_fidelity`](red_core::QueryOptions).
    full: bool,
    /// Whether the stream ran to its end. A connection is only safe to reuse once
    /// the server has finished answering on it; see [`PgCursor::drop`].
    exhausted: AtomicBool,
    /// The connection this cursor streams over, held for its whole lifetime and
    /// handed back on drop. `Option` only so `drop` can move it out.
    client: Option<Arc<Client>>,
    pool: Arc<StdMutex<Vec<Arc<Client>>>>,
}

/// Hand the connection back when the cursor goes away — including when the service
/// drops a superseded cursor, which is the common case.
///
/// An unexhausted cursor's connection still has rows queued server-side, so
/// returning it to the pool would hand the next borrower someone else's result. Fire
/// the cancel and let the connection close instead: correctness over reuse, and the
/// cancel is what stops the server streaming a result nobody will ever read.
impl Drop for PgCursor {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        if self.exhausted.load(Ordering::Relaxed) {
            release_to(&self.pool, client);
        } else {
            self.cancel.cancel();
        }
    }
}

#[async_trait]
impl QueryCursor for PgCursor {
    fn columns(&self) -> &[Column] {
        &self.columns
    }

    async fn next_window(&self, max: usize) -> Result<RowWindow> {
        // Offset-mode display stream (editor run): cap every cell, no key exempt.
        // A full-fidelity reader (the table copy) reads byte-exact instead.
        let cap = if self.full {
            None
        } else {
            CellCap::display([None, None])
        };
        let mut stream = self.stream.lock().await;
        let mut rows = Vec::with_capacity(crate::window_prealloc(max));
        for _ in 0..max {
            match stream.next().await {
                Some(Ok(row)) => rows.push(pg_row(&row, cap)),
                Some(Err(e)) => return Err(map_pg_err(e)),
                None => {
                    // The server is done answering, so the connection is clean and
                    // `drop` can return it to the pool.
                    self.exhausted.store(true, Ordering::Relaxed);
                    return Ok(RowWindow {
                        rows,
                        exhausted: true,
                    });
                }
            }
        }
        Ok(RowWindow {
            rows,
            exhausted: false,
        })
    }

    fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }
}

/// Double-quote an identifier for interpolation (doubling embedded quotes).
fn pg_quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Postgres' wire-protocol cap on bound parameters per statement (`u16`, 65535),
/// with margin; a multi-row insert sub-chunks below it.
const PG_PARAM_CAP: usize = 60_000;

/// The explicit cast for a bound placeholder, pinning the inferred parameter type
/// to the wire form the value's Rust type encodes (so Postgres can't re-infer it):
/// `i64`→`int8`, `f64`→`float8`, `String`→`text`, `Vec<u8>`→`bytea`.
///
/// A [`Value::Text`] is special on **write**: it binds as `text` (the only form
/// `String` encodes), but the target column may be jsonb/json/timestamp/uuid/numeric
/// /an enum: types with no implicit (assignment) cast *from* text (the post-8.3
/// rule), so `SET jsonb_col = $1::text` is rejected with "column is of type jsonb
/// but expression is of type text". When `decl_type` names such a column we add a
/// second, *explicit* cast, `$1::text::"jsonb"`, which type-checks. Plain
/// text-family columns (and an unknown / absent type, e.g. a key bind) keep `::text`.
fn pg_cast(value: &Value, decl_type: Option<&str>) -> String {
    match value {
        // `pg_value` decodes a `BOOL` cell as `Value::Integer`, and Postgres has no
        // int8→bool cast at all — not even an explicit one — so a plain `::int8`
        // into a `boolean` column fails with 42804 and takes every copy and
        // migration of a bool column with it. `int4` is the width the bool cast is
        // defined on, hence the two hops.
        Value::Integer(_) if decl_type.is_some_and(is_pg_bool_type) => {
            "::int8::int4::bool".to_string()
        }
        Value::Integer(_) => "::int8".to_string(),
        Value::Real(_) => "::float8".to_string(),
        Value::Blob(_) => "::bytea".to_string(),
        Value::Null | Value::Capped(_) => String::new(),
        Value::Text(_) => match decl_type {
            Some(t) if !is_pg_text_type(t) => format!("::text::{}", pg_quote(t)),
            _ => "::text".to_string(),
        },
    }
}

/// Whether a Postgres column type names the boolean type, under either spelling
/// (`information_schema.udt_name` reports `bool`, `format_type` says `boolean`).
fn is_pg_bool_type(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "bool" | "boolean"
    )
}

/// Whether a Postgres column type (the `typname` we store as `decl_type`) is a
/// text-family type a `text` bind assigns to directly, so it needs no second cast
/// on write. Everything else (jsonb, timestamp, uuid, numeric, an enum, …) does.
fn is_pg_text_type(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "text" | "varchar" | "bpchar" | "char" | "name" | "citext" | "unknown"
    )
}

/// Box each seek-bound value as a typed `ToSql` parameter (one per leading key
/// column), for positional binding into the row-value comparison. Key columns are
/// never null/capped, so those variants are a query error rather than a `NULL`.
fn pg_params(bound: Option<&[Value]>) -> Result<Vec<Box<dyn ToSql + Sync + Send>>> {
    bound
        .unwrap_or(&[])
        .iter()
        .map(|v| -> Result<Box<dyn ToSql + Sync + Send>> {
            Ok(match v {
                Value::Integer(n) => Box::new(*n),
                Value::Real(x) => Box::new(*x),
                Value::Text(s) => Box::new(s.to_string()),
                Value::Blob(b) => Box::new(b.clone()),
                Value::Null | Value::Capped(_) => {
                    return Err(RedError::Query("null seek bound".into()));
                }
            })
        })
        .collect()
}

/// Map one row's cells to [`Value`]s by column type (text fallback for the rest).
/// With a display `cap`, over-cap non-key text/blob cells come back [`Value::Capped`]
/// (blob bytes are read as a borrowed slice for their length only, never owned).
fn pg_row(row: &Row, cap: Option<CellCap>) -> Vec<Value> {
    (0..row.len())
        .map(|i| pg_value(row, i, CellCap::caps(cap, i)))
        .collect()
}

fn pg_value(row: &Row, i: usize, max: Option<usize>) -> Value {
    match *row.columns()[i].type_() {
        Type::BOOL => scalar(row, i, max, |b: bool| Value::Integer(b as i64)),
        Type::INT2 => scalar(row, i, max, |n: i16| Value::Integer(i64::from(n))),
        Type::INT4 => scalar(row, i, max, |n: i32| Value::Integer(i64::from(n))),
        Type::INT8 => scalar(row, i, max, |n: i64| Value::Integer(n)),
        Type::FLOAT4 => scalar(row, i, max, |x: f32| Value::Real(x as f64)),
        Type::FLOAT8 => scalar(row, i, max, Value::Real),
        // Types `tokio-postgres` won't decode without an optional crate: render the
        // raw wire bytes to text ourselves (see `pg_text`) so they don't decode-fail
        // into a silent NULL. Each result is short except JSON, which honours `max`.
        Type::NUMERIC => decode_raw(row, i, max, pg_text::numeric_to_string),
        Type::TIMESTAMP => decode_raw(row, i, max, |b| be_i64(b).map(pg_text::timestamp_to_string)),
        Type::TIMESTAMPTZ => decode_raw(row, i, max, |b| {
            be_i64(b).map(pg_text::timestamptz_to_string)
        }),
        Type::DATE => decode_raw(row, i, max, |b| be_i32(b).map(pg_text::date_to_string)),
        Type::TIME => decode_raw(row, i, max, |b| be_i64(b).map(pg_text::time_to_string)),
        Type::TIMETZ => decode_raw(row, i, max, pg_text::timetz_to_string),
        Type::UUID => decode_raw(row, i, max, pg_text::uuid_to_string),
        // JSON is UTF-8 text on the wire; JSONB prefixes a 1-byte version header.
        Type::JSON => decode_raw(row, i, max, |b| {
            Some(String::from_utf8_lossy(b).into_owned())
        }),
        Type::JSONB => decode_raw(row, i, max, |b| {
            let text = b.split_first().map(|(_, rest)| rest).unwrap_or(b);
            Some(String::from_utf8_lossy(text).into_owned())
        }),
        // Capped: read the bytes as a borrowed slice for their length, never owning
        // them. Full fidelity: own the bytes (export / clipboard / key column).
        Type::BYTEA => match max {
            Some(_) => row
                .try_get::<_, Option<&[u8]>>(i)
                .ok()
                .flatten()
                .map(|b| Value::capped_blob(b.len()))
                .unwrap_or(Value::Null),
            None => row
                .try_get::<_, Option<Vec<u8>>>(i)
                .ok()
                .flatten()
                .map(Value::Blob)
                .unwrap_or(Value::Null),
        },
        // text / varchar / name / bpchar / unknown, and a best-effort for the rest.
        // `&str` and `String` accept the same types, so capping doesn't change which
        // columns decode (only how much of an over-cap one is kept).
        //
        // `try_get` returns `Ok(None)` for a SQL NULL and `Err` when the target type
        // *rejects* the column type (its `accepts` said no). The former is a genuine
        // `Null`; the latter is an unmapped type the string decode declined (enum,
        // inet, interval, array, …), and rather than collapse it to a silent NULL we
        // fall back to its raw wire bytes as lossy UTF-8: correct for the text-shaped
        // wire forms (enum labels, citext-likes) and a visible cell for the rest.
        _ => match row.try_get::<_, Option<&str>>(i) {
            Ok(None) => Value::Null,
            Ok(Some(s)) => match max {
                Some(max) => Value::capped_text(s, max),
                None => Value::Text(s.into()),
            },
            Err(_) => raw_text_fallback(row, i, max),
        },
    }
}

/// Decode a scalar cell of type `T` and map it with `f`. A SQL NULL is
/// [`Value::Null`]; a decode *error* (the column isn't the `T` we expected) falls
/// back to the raw wire bytes as text via [`raw_text_fallback`] rather than
/// collapsing to a silent NULL, the same safety the text `_` arm relies on.
fn scalar<'a, T>(row: &'a Row, i: usize, max: Option<usize>, f: impl FnOnce(T) -> Value) -> Value
where
    T: tokio_postgres::types::FromSql<'a>,
{
    match row.try_get::<_, Option<T>>(i) {
        Ok(Some(v)) => f(v),
        Ok(None) => Value::Null,
        Err(_) => raw_text_fallback(row, i, max),
    }
}

/// Captures a column's raw binary wire bytes verbatim, so the driver can render the
/// types `tokio-postgres` declines to decode itself (see [`crate::pg_text`]).
/// `accepts` is unconditional: it's only ever asked for via the explicit type
/// arms in [`pg_value`].
struct RawBytes(Vec<u8>);

impl<'a> tokio_postgres::types::FromSql<'a> for RawBytes {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(RawBytes(raw.to_vec()))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

/// Decode cell `i`'s raw wire bytes with `f`, wrapping the result as a display
/// [`Value`] (honouring `max`). A SQL NULL, a fetch error, or an `f` that can't
/// parse the buffer all collapse to [`Value::Null`].
fn decode_raw(
    row: &Row,
    i: usize,
    max: Option<usize>,
    f: impl FnOnce(&[u8]) -> Option<String>,
) -> Value {
    row.try_get::<_, Option<RawBytes>>(i)
        .ok()
        .flatten()
        .and_then(|b| f(&b.0))
        .map(|s| match max {
            Some(m) => Value::capped_text(&s, m),
            None => Value::Text(s.into()),
        })
        .unwrap_or(Value::Null)
}

/// Last-resort render for a column type the scalar/`pg_text` arms don't name and
/// that the string decode rejected (its `accepts` said no): take the raw wire bytes
/// and render them as lossy UTF-8. Correct for the text-shaped binary forms (enum
/// labels, `citext`-likes, domains over text) and at worst a visible cell for the
/// others: anything but the silent NULL the bare string decode would have produced.
/// A fetch error or genuine SQL NULL still collapses to [`Value::Null`].
fn raw_text_fallback(row: &Row, i: usize, max: Option<usize>) -> Value {
    decode_raw(row, i, max, |b| {
        Some(String::from_utf8_lossy(b).into_owned())
    })
}

fn be_i64(b: &[u8]) -> Option<i64> {
    b.try_into().ok().map(i64::from_be_bytes)
}

fn be_i32(b: &[u8]) -> Option<i32> {
    b.try_into().ok().map(i32::from_be_bytes)
}

/// Map a failed dial to a *fatal* [`RedError::Auth`] (the user must fix the
/// connection before a retry helps) or a transient [`RedError::Connect`]. Bad
/// credentials (28xxx) and a missing database (3D000) are user-correctable; a
/// refused/unreachable host has no server `DbError` and stays a retryable
/// `Connect`. The server's own message is surfaced (its `Display` is a bare
/// `"db error"`; the text lives only in the attached `DbError`).
fn map_connect_err(e: tokio_postgres::Error) -> RedError {
    if let Some(db) = e.as_db_error() {
        // `.get`: an unrecognised SQLSTATE is stored verbatim, so a broken
        // server/pooler can hand back fewer than two bytes.
        let class = db.code().code().get(..2).unwrap_or_default();
        // SQLSTATE class 28 = invalid authorization; 3D000 = invalid catalog
        // (database does not exist). Both need a credential/target fix, not a wait.
        if class == "28" || db.code() == &tokio_postgres::error::SqlState::INVALID_CATALOG_NAME {
            return RedError::Auth(db.message().to_string());
        }
        return RedError::Connect(format!("{}: {}", db.code().code(), db.message()));
    }
    RedError::Connect(e.to_string())
}

/// Map a cancel (SQLSTATE 57014) to the distinct `Interrupted`, else a driver
/// error. A database-side failure (bad SQL, missing relation, type mismatch) is
/// the common case, and `tokio_postgres::Error`'s own `Display` renders it as a
/// bare `"db error"`; the useful text lives only in the attached `DbError`. So
/// surface the server's message (with SQLSTATE and any hint) rather than letting
/// the round-trip bounce back as the cryptic `"db error"`.
fn map_pg_err(e: tokio_postgres::Error) -> RedError {
    if let Some(db) = e.as_db_error() {
        if db.code() == &tokio_postgres::error::SqlState::QUERY_CANCELED {
            return RedError::Interrupted;
        }
        let mut msg = format!("{}: {}", db.code().code(), db.message());
        if let Some(hint) = db.hint() {
            msg.push_str(&format!(" (hint: {hint})"));
        }
        return RedError::Driver(msg);
    }
    driver_err(e)
}

/// The column list inside an index definition's parentheses.
fn parse_index_columns(def: &str) -> Vec<String> {
    let Some(open) = def.find('(') else {
        return Vec::new();
    };
    let Some(close) = def.rfind(')') else {
        return Vec::new();
    };
    if close <= open {
        return Vec::new();
    }
    def[open + 1..close]
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// Tests run against a live PostgreSQL provided via `RED_TEST_POSTGRES_URL`, so CI
// without a server skips cleanly. Spin one up with:
//
//   docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=red \
//     -e POSTGRES_DB=red_test --name red-pg postgres:16
//   export RED_TEST_POSTGRES_URL='host=127.0.0.1 user=postgres password=red dbname=red_test'
#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance as battery;
    use red_core::KeyKind;

    fn test_url() -> Option<String> {
        std::env::var("RED_TEST_POSTGRES_URL").ok()
    }

    /// A unique fixture-name suffix so concurrent tests don't collide on a shared
    /// server. Postgres lowercases unquoted identifiers, so keep it lowercase.
    fn tag(name: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        format!("red_{name}_{}_{n}", std::process::id())
    }

    macro_rules! url_or_skip {
        () => {
            match test_url() {
                Some(u) => u,
                None => {
                    // Visible skip (with `--nocapture`): a missing URL must read as
                    // "not run", never a silent pass. CI sets the URL so it runs.
                    eprintln!("SKIP {}: RED_TEST_POSTGRES_URL not set", module_path!());
                    return;
                }
            }
        };
    }

    /// The write-side cast `pg_cast` emits per `(value, column type)`. A scalar pins
    /// the wire type from its Rust value; a text value bound into a non-text column
    /// (jsonb, timestamp, an enum) gets the second explicit cast that lets the
    /// assignment type-check, while a plain text column (or an unknown / key bind)
    /// stays a bare `::text`. No DB needed: pure string rendering.
    #[test]
    fn pg_cast_casts_text_into_non_text_columns() {
        // Scalars: cast follows the Rust value, column type is irrelevant.
        assert_eq!(pg_cast(&Value::Integer(1), Some("int4")), "::int8");
        assert_eq!(pg_cast(&Value::Real(1.0), Some("numeric")), "::float8");
        assert_eq!(pg_cast(&Value::Blob(vec![1]), None), "::bytea");
        // NULL / capped never bind, so they emit no cast.
        assert_eq!(pg_cast(&Value::Null, Some("jsonb")), "");

        let text = Value::Text("{\"a\":1}".into());
        // A jsonb / json / timestamp / uuid / enum column needs the explicit cast,
        // because Postgres won't assignment-cast text into them.
        assert_eq!(pg_cast(&text, Some("jsonb")), "::text::\"jsonb\"");
        assert_eq!(pg_cast(&text, Some("json")), "::text::\"json\"");
        assert_eq!(
            pg_cast(&text, Some("timestamptz")),
            "::text::\"timestamptz\""
        );
        assert_eq!(pg_cast(&text, Some("uuid")), "::text::\"uuid\"");
        assert_eq!(pg_cast(&text, Some("mood")), "::text::\"mood\"");
        // Plain text-family columns assign directly; no second cast.
        assert_eq!(pg_cast(&text, Some("text")), "::text");
        assert_eq!(pg_cast(&text, Some("VARCHAR")), "::text");
        assert_eq!(pg_cast(&text, Some("bpchar")), "::text");
        // Unknown / absent type (e.g. a key bind) is best-effort `::text`.
        assert_eq!(pg_cast(&text, None), "::text");
    }

    /// The connection's current schema: unqualified fixtures land here, so
    /// introspection filters to it. Read through the public API rather than the
    /// private client.
    async fn current_schema(driver: &PostgresDriver) -> String {
        let page = driver
            .fetch_page(
                "SELECT current_schema()",
                0,
                1,
                PageCap::Full,
                &AbortSignal::new(),
            )
            .await
            .unwrap();
        match &page.rows[0][0] {
            Value::Text(s) => s.to_string(),
            other => panic!("current_schema() returned {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_reports_version() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        assert!(!driver.server_version().is_empty());
        driver.ping().await.unwrap();
    }

    #[tokio::test]
    async fn tls_dsn_is_refused_not_silently_downgraded() {
        // Postgres TLS isn't wired yet; a `sslmode=require` DSN must error rather
        // than connect in cleartext. No server needed — it fails before dialing.
        match PostgresDriver::connect("postgres://h:5432/db?sslmode=require", true).await {
            Ok(_) => panic!("a TLS Postgres DSN should be refused, not connected"),
            Err(e) => assert!(
                e.to_string().to_lowercase().contains("tls"),
                "expected a TLS-not-supported error, got {e}"
            ),
        }
    }

    /// The non-scalar types `pg_value` renders from their binary wire form must
    /// come back as their text, never as a silent NULL (the regression `pg_text`
    /// fixes). Complements the wire-format unit tests in [`crate::pg_text`] with a
    /// live round-trip through the real `tokio-postgres` decode path.
    #[tokio::test]
    async fn rich_types_render_as_text_not_null() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        let sql = "SELECT \
            1234.567::numeric, \
            '2021-03-15 12:30:45'::timestamp, \
            '2021-03-15 12:30:45+00'::timestamptz, \
            '2021-03-15'::date, \
            '12:30:45'::time, \
            '12345678-1234-5678-1234-567812345678'::uuid, \
            '{\"a\":1}'::json, \
            '{\"b\": 2}'::jsonb";
        let page = driver
            .fetch_page(sql, 0, 1, PageCap::Full, &AbortSignal::new())
            .await
            .unwrap();
        let row = &page.rows[0];
        let text = |v: &Value| match v {
            Value::Text(s) => s.to_string(),
            other => panic!("expected text, got {other:?}"),
        };
        assert_eq!(text(&row[0]), "1234.567");
        assert_eq!(text(&row[1]), "2021-03-15 12:30:45");
        // timestamptz is UTC on the wire regardless of session zone.
        assert_eq!(text(&row[2]), "2021-03-15 12:30:45+00");
        assert_eq!(text(&row[3]), "2021-03-15");
        assert_eq!(text(&row[4]), "12:30:45");
        assert_eq!(text(&row[5]), "12345678-1234-5678-1234-567812345678");
        assert_eq!(text(&row[6]), "{\"a\":1}");
        // jsonb normalizes spacing/key order on the server.
        assert_eq!(text(&row[7]), "{\"b\": 2}");
    }

    /// Types neither the scalar arms nor `pg_text` name, and that the string decode
    /// *rejects* (its `accepts` says no): inet, interval, and an array all flow
    /// through the raw-bytes fallback. The contract under test is "visible text, not
    /// a silent NULL"; the exact bytes are server-version dependent, so assert only
    /// that each cell is non-empty text.
    #[tokio::test]
    async fn unmapped_types_fall_back_to_text_not_null() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        let sql = "SELECT \
            '192.168.0.1'::inet, \
            '1 day 02:03:04'::interval, \
            ARRAY[1, 2, 3]";
        let page = driver
            .fetch_page(sql, 0, 1, PageCap::Full, &AbortSignal::new())
            .await
            .unwrap();
        let row = &page.rows[0];
        for (i, cell) in row.iter().enumerate() {
            match cell {
                Value::Text(s) if !s.is_empty() => {}
                other => panic!("col {i} fell back to {other:?}, expected non-empty text"),
            }
        }
    }

    #[tokio::test]
    async fn streams_in_bounded_windows() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        // `generate_series` is a server-side streaming row source: no fixture, and
        // it never materializes server-side, mirroring the windowed read.
        battery::streams_in_bounded_windows(&driver, "SELECT generate_series(1, 100000)", 100_000)
            .await;
    }

    #[tokio::test]
    async fn cancel_aborts_in_flight_fetch() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        // A large cross join keeps the server streaming long enough to cancel
        // out-of-band; Postgres maps the cancel to `QUERY_CANCELED` → Interrupted.
        let sql = "SELECT a FROM generate_series(1, 100000) a \
                   CROSS JOIN generate_series(1, 100000) b";
        battery::cancel_aborts_in_flight_fetch(&driver, sql, std::time::Duration::from_millis(200))
            .await;
    }

    #[tokio::test]
    async fn superseded_one_shot_fetch_is_cancelled() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        // count(*) over a 10^10-row cross join keeps the backend busy to interrupt.
        let heavy = "SELECT a FROM generate_series(1, 100000) a \
                     CROSS JOIN generate_series(1, 100000) b";
        battery::superseded_fetch_is_cancelled(
            &driver,
            heavy,
            std::time::Duration::from_millis(200),
        )
        .await;
        battery::pre_aborted_fetch_returns_immediately(&driver, heavy).await;
        battery::abort_after_completion_is_noop(&driver, "SELECT 1").await;
    }

    /// The reason Postgres fetches use a pool: cancelling one fetch's abort signal
    /// must not disturb another fetch in flight on a *different* pooled connection.
    #[tokio::test]
    async fn superseding_one_fetch_spares_a_concurrent_one() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        let heavy = "SELECT a FROM generate_series(1, 100000) a \
                     CROSS JOIN generate_series(1, 100000) b";

        let doomed = AbortSignal::new();
        let kept = AbortSignal::new();
        // Abort only `doomed` once both are in flight on their own pooled conns.
        let trigger = doomed.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            trigger.abort();
        });

        let (a, b) = tokio::join!(
            driver.count(heavy, &doomed),
            // A finite-but-still-running count that must complete untouched.
            driver.count("SELECT generate_series(1, 5000000)", &kept),
        );
        assert!(
            matches!(a, Err(RedError::Interrupted)),
            "doomed fetch cancelled: {a:?}"
        );
        assert_eq!(
            b.unwrap(),
            5_000_000,
            "the concurrent fetch finished unharmed"
        );
    }

    #[tokio::test]
    async fn introspects_tables_columns_fks_and_indexes() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let authors = tag("authors");
        let books = tag("books");
        let recent = tag("recent");
        let idx = tag("idx");
        let schema = current_schema(&driver).await;

        // Postgres `execute` runs a single statement, so issue the DDL one at a time.
        driver
            .execute(&format!(
                "CREATE TABLE {authors} (id INT PRIMARY KEY, name TEXT NOT NULL)"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "CREATE TABLE {books} (\
                   id INT PRIMARY KEY, \
                   title TEXT NOT NULL DEFAULT 'untitled', \
                   author_id INT REFERENCES {authors}(id))"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!("CREATE INDEX {idx} ON {books}(author_id)"))
            .await
            .unwrap();
        driver
            .execute(&format!("CREATE VIEW {recent} AS SELECT * FROM {books}"))
            .await
            .unwrap();

        battery::introspects_tables_columns_fks_and_indexes(
            &driver, &schema, &authors, &books, &recent,
        )
        .await;
        // The connection-wide FK graph reports the same edge.
        battery::lists_foreign_key_graph(&driver, &schema, &authors, &books).await;

        // Seed a few rows so the column-stats summary has data: author_id is
        // 1,1,2,NULL (NULLs + duplicates), narrowable by `author_id = 1`.
        driver
            .execute(&format!(
                "INSERT INTO {authors}(id, name) VALUES (1, 'Ada'), (2, 'Grace')"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {books}(id, title, author_id) \
                 VALUES (1, 'a', 1), (2, 'b', 1), (3, 'c', 2), (4, 'd', NULL)"
            ))
            .await
            .unwrap();
        battery::column_stats_summary(
            &driver,
            &format!("SELECT * FROM {books}"),
            "author_id",
            "title",
            "author_id = 1",
        )
        .await;
        // The built-not-typed filter wants the same fixture shape.
        battery::filters_cmp(
            &driver,
            &format!("SELECT * FROM {books}"),
            "author_id",
            "title",
        )
        .await;

        for obj in [
            format!("VIEW {recent}"),
            format!("TABLE {books}"),
            format!("TABLE {authors}"),
        ] {
            driver.execute(&format!("DROP {obj}")).await.unwrap();
        }
    }

    #[tokio::test]
    async fn filters_contains() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let f = tag("f");
        let schema = current_schema(&driver).await;
        driver
            .execute(&format!(
                "CREATE TABLE {f} (id INT PRIMARY KEY, name TEXT, note TEXT, data BYTEA)"
            ))
            .await
            .unwrap();
        // Rows 1–2 carry a blob whose bytes spell "apple"; on Postgres `bytea::text`
        // is a hex string anyway, but the predicate must still skip the column.
        driver
            .execute(&format!(
                "INSERT INTO {f} VALUES \
                 (1,'apple','red fruit','\\x6170706c65'::bytea), \
                 (2,'banana','yellow','\\x6170706c65'::bytea), \
                 (3,'apple pie','dessert','\\x00'::bytea), \
                 (4,'100% juice','on sale','\\x00'::bytea), \
                 (5,'O''Brien','name','\\x00'::bytea)"
            ))
            .await
            .unwrap();
        battery::filters_contains(&driver, &schema, &f, &format!("SELECT * FROM {f}")).await;
        driver.execute(&format!("DROP TABLE {f}")).await.unwrap();
    }

    #[tokio::test]
    async fn executes_in_transaction_and_exports() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let t = tag("t");
        driver
            .execute(&format!("CREATE TABLE {t} (id INT, name TEXT)"))
            .await
            .unwrap();

        let affected = driver
            .execute(&format!("INSERT INTO {t} VALUES (1, 'a,b'), (2, NULL)"))
            .await
            .unwrap();
        assert_eq!(affected, 2, "execute reports rows affected");

        battery::exports_csv_and_json(&driver, &format!("SELECT * FROM {t} ORDER BY id"), &t).await;

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn seeks_forward_backward_and_reads_bounds() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let t = tag("seek");
        driver
            .execute(&format!("CREATE TABLE {t} (id INT PRIMARY KEY, name TEXT)"))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {t} SELECT g, 'row ' || g FROM generate_series(1, 1000) g"
            ))
            .await
            .unwrap();

        let key = KeySpec::single("id", KeyKind::Int);
        battery::seeks_forward_backward_and_reads_bounds(
            &driver,
            &format!("SELECT * FROM {t}"),
            &key,
        )
        .await;

        // Composite `(grp, id)` seek over a non-unique sort column.
        let g = tag("seekcomposite");
        driver
            .execute(&format!(
                "CREATE TABLE {g} (id INT PRIMARY KEY, grp INT NOT NULL)"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {g} SELECT s, s % 3 FROM generate_series(1, 30) s"
            ))
            .await
            .unwrap();
        let key_asc = KeySpec {
            column: "grp".into(),
            kind: KeyKind::Int,
            column_type: None,
            tiebreak: Some("id".into()),
            tiebreak_type: None,
            direction: red_core::SortDirection::Asc,
        };
        let key_desc = KeySpec {
            direction: red_core::SortDirection::Desc,
            ..key_asc.clone()
        };
        battery::seeks_composite_sorted(
            &driver,
            &format!("SELECT * FROM {g}"),
            &key_asc,
            &key_desc,
            30,
        )
        .await;
        driver.execute(&format!("DROP TABLE {g}")).await.unwrap();

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn read_only_rejects_writes() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, true).await.unwrap();
        battery::read_only_rejects_write(&driver, "CREATE TABLE red_ro_should_fail (x INT)").await;
    }

    #[tokio::test]
    async fn applies_edits_and_read_only_rejects() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let t = tag("edit");
        driver
            .execute(&format!("CREATE TABLE {t} (id INT PRIMARY KEY, name TEXT)"))
            .await
            .unwrap();
        driver
            .execute(&format!("INSERT INTO {t} VALUES (1, 'one')"))
            .await
            .unwrap();
        let schema = current_schema(&driver).await;
        battery::applies_edits(&driver, &schema, &t).await;

        let ro = PostgresDriver::connect(&url, true).await.unwrap();
        battery::read_only_rejects_edit(&ro, &schema, &t).await;

        // Atomic batch editing (B6) on a fresh seed table.
        let tb = tag("batch");
        driver
            .execute(&format!(
                "CREATE TABLE {tb} (id INT PRIMARY KEY, name TEXT)"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!("INSERT INTO {tb} VALUES (1, 'one')"))
            .await
            .unwrap();
        battery::applies_batch_atomic(&driver, &schema, &tb).await;
        battery::read_only_rejects_batch(&ro, &schema, &tb).await;

        // Bulk insert (data import / table copy) on a fresh empty table.
        let ti = tag("insert");
        driver
            .execute(&format!(
                "CREATE TABLE {ti} (id INT PRIMARY KEY, name TEXT)"
            ))
            .await
            .unwrap();
        battery::inserts_rows(&driver, &schema, &ti).await;
        battery::read_only_rejects_insert_rows(&ro, &schema, &ti).await;

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
        driver.execute(&format!("DROP TABLE {tb}")).await.unwrap();
        driver.execute(&format!("DROP TABLE {ti}")).await.unwrap();
    }

    /// Editing a column whose value decodes to [`Value::Text`] but whose real type
    /// has no assignment cast *from* text (jsonb, timestamptz, uuid) must succeed:
    /// the write-side `::text::"type"` cast (driven by `ColumnValue::decl_type`) lets
    /// the bound text type-check into the column. A bare `::text` would be rejected
    /// ("column is of type jsonb but expression is of type text"); this is the
    /// regression test for that. The PK (int) and a plain text column ride along to
    /// show the typed columns don't disturb the ordinary path.
    #[tokio::test]
    async fn edits_jsonb_timestamp_and_uuid_columns() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let t = tag("typededit");
        driver
            .execute(&format!(
                "CREATE TABLE {t} (id INT PRIMARY KEY, doc JSONB, at TIMESTAMPTZ, ref UUID, name TEXT)"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {t} VALUES (1, '{{\"a\":1}}', '2000-01-01 00:00:00+00', \
                 '00000000-0000-0000-0000-000000000000', 'one')"
            ))
            .await
            .unwrap();
        let schema = current_schema(&driver).await;
        let tref = red_core::TableRef {
            schema: Some(schema),
            name: t.clone(),
        };
        // One UPDATE setting every typed column at once; each `set` carries the
        // column's `decl_type`, the key carries none (an int PK binds bare).
        let set = |column: &str, value: &str, decl: &str| red_core::ColumnValue {
            column: column.into(),
            value: Value::Text(value.into()),
            decl_type: Some(decl.into()),
        };
        let affected = driver
            .apply_edit(&EditOp::Update {
                table: tref,
                keys: vec![red_core::ColumnValue {
                    column: "id".into(),
                    value: Value::Integer(1),
                    decl_type: None,
                }],
                set: vec![
                    set("doc", "{\"b\": [2, 3]}", "jsonb"),
                    set("at", "2021-06-15 12:30:00+00", "timestamptz"),
                    set("ref", "12345678-1234-5678-1234-567812345678", "uuid"),
                    set("name", "two", "text"),
                ],
            })
            .await
            .unwrap();
        assert_eq!(affected, 1, "the typed UPDATE matched exactly its row");

        let page = driver
            .fetch_page(
                &format!("SELECT doc, at, ref, name FROM {t} WHERE id = 1"),
                0,
                1,
                PageCap::Full,
                &AbortSignal::new(),
            )
            .await
            .unwrap();
        let text = |v: &Value| match v {
            Value::Text(s) => s.to_string(),
            other => panic!("expected text, got {other:?}"),
        };
        let row = &page.rows[0];
        // jsonb re-serializes with canonical spacing on the server.
        assert_eq!(text(&row[0]), "{\"b\": [2, 3]}", "jsonb landed");
        assert_eq!(
            text(&row[1]),
            "2021-06-15 12:30:00+00",
            "timestamptz landed"
        );
        assert_eq!(
            text(&row[2]),
            "12345678-1234-5678-1234-567812345678",
            "uuid landed"
        );
        assert_eq!(text(&row[3]), "two", "plain text column unaffected");

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn explains_a_query() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let t = tag("explain");
        driver
            .execute(&format!("CREATE TABLE {t} (id INT PRIMARY KEY, name TEXT)"))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {t} SELECT g, 'row ' || g FROM generate_series(1, 100) g"
            ))
            .await
            .unwrap();

        battery::explains_query(&driver, &format!("SELECT * FROM {t}"), &t).await;

        // EXPLAIN ANALYZE carries actual-time metrics and is flagged analyzed.
        let plan = driver
            .explain(&format!("SELECT count(*) FROM {t}"), true)
            .await
            .unwrap();
        assert!(plan.analyzed, "analyze flag set");
        let has_actual =
            |n: &red_core::PlanNode| n.metrics.iter().any(|(k, _)| k.starts_with("actual"));
        assert!(
            plan.nodes.iter().any(has_actual)
                || plan.nodes.iter().flat_map(|n| &n.children).any(has_actual),
            "ANALYZE plan carries actual metrics: {}",
            plan.raw
        );

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn caps_display_keeps_key_and_export() {
        let url = url_or_skip!();
        let driver = PostgresDriver::connect(&url, false).await.unwrap();
        let t = tag("cap");
        driver
            .execute(&format!(
                "CREATE TABLE {t} (id INT PRIMARY KEY, t TEXT, b BYTEA)"
            ))
            .await
            .unwrap();
        // One row whose text and blob both far exceed the display cap.
        driver
            .execute(&format!(
                "INSERT INTO {t} VALUES (1, repeat('a', 5000), decode(repeat('61', 5000), 'hex'))"
            ))
            .await
            .unwrap();
        let key = KeySpec::single("id", KeyKind::Int);
        battery::caps_display_keeps_key_and_export(
            &driver,
            &format!("SELECT id, t, b FROM {t}"),
            &key,
            b'a',
            5000,
            5000,
            &t,
        )
        .await;
        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }
}
