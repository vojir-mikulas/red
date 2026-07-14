//! ClickHouse driver: the fourth source of `DatabaseDriver`, the first OLAP
//! engine, proving the abstraction against a column store. Built on the
//! **HTTP interface** (port 8123) rather than a heavy native-protocol crate: the
//! `JSONCompactEachRowWithNamesAndTypes` format returns column names *and* types in
//! its first two lines then streams one JSON array per row, so a windowed read maps
//! directly onto reading newline-delimited lines off the byte stream. This keeps
//! the dependency-light ethos (reqwest/rustls + serde_json are already in the tree
//! via `red-ai`), and **out-of-band cancel** is a `KILL QUERY WHERE query_id = …`
//! over a second request, the same shape MySQL's `KILL QUERY` cancel proves.
//!
//! Writes are gated on the connection's `read_only` flag (like every engine): when
//! set it appends the `readonly=1` server setting, so any write is refused at the
//! engine. A *writable* ClickHouse connection can be an INSERT / copy / migration
//! **target**: [`insert_rows`](ClickhouseDriver::insert_rows) streams an
//! `INSERT … FORMAT JSONCompactEachRow`, [`create_table`](ClickhouseDriver::create_table)
//! emits `MergeTree` DDL, and [`clear_table`](ClickhouseDriver::clear_table)
//! `TRUNCATE`s. In-grid **editing** stays **unsupported**: ClickHouse `UPDATE`/`DELETE`
//! are asynchronous `ALTER TABLE … UPDATE` mutations with no transaction or rollback
//! over a non-unique sort key, so the trait's "batch in one transaction, assert exactly
//! one row, roll back on failure" contract cannot be honored; [`apply_edits`] returns a
//! typed error (a best-effort mutation mode is a later phase). Secondary indexes and
//! foreign keys have no OLAP equivalent, so those migration passes are logged skips.
//!
//! Value mapping leans on the engine: the `JSON…` formats render every type to JSON
//! text for us, so a cell is a "JSON scalar/array → [`Value`]" map; no hand-written
//! binary decoder. Integers come back as JSON numbers (or quoted strings for the
//! 64-bit widths, which JSON can't hold losslessly); composites (`Array`, `Tuple`,
//! `Map`) and the date/decimal/uuid/enum shapes render as text.

use std::fs::{remove_file, File};
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use red_core::{
    Column, ColumnMeta, ColumnValue, ConnectionConfig, DbKind, EditOp, ExportFormat, FkEdge,
    KeySpec, ObjectKind, ObjectMeta, QueryOptions, QueryPlan, RedError, Result, ResultPage,
    RowWindow, SchemaMeta, TableDetail, TableRef, Value,
};
use serde_json::Value as Json;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::format::{strip_trailing, ExportWriter, ProgressThrottle};
use crate::{
    driver_err, window_prealloc, AbortSignal, CancelToken, CellCap, DatabaseDriver, PageCap,
    QueryCursor,
};

/// A collected (bounded) read: the result columns, their raw ClickHouse type
/// strings (for value mapping), and the raw JSON cell rows.
type RowBlock = (Vec<Column>, Vec<String>, Vec<Vec<Json>>);

/// An opened streaming read: columns + types, the live response, and the stream
/// bytes already buffered past the two header lines.
type OpenedStream = (Vec<Column>, Vec<String>, reqwest::Response, Vec<u8>);

/// The streaming row format: header line 1 = column names, line 2 = column types,
/// then one JSON array per row. Names + types up front is what lets `open_cursor`
/// report columns without stepping rows, and the per-row newline framing is the
/// natural windowed read.
const ROW_FORMAT: &str = "JSONCompactEachRowWithNamesAndTypes";

/// The format an `INSERT … FORMAT …` body carries: one JSON array per row, with
/// **no** names/types header (unlike [`ROW_FORMAT`], which the *read* path uses to
/// learn its columns up front). The column list rides in the `INSERT INTO … (cols)`
/// clause instead, so the two header lines a WithNamesAndTypes insert would demand
/// aren't sent, and can't be mistaken for the first two data rows.
const INSERT_FORMAT: &str = "JSONCompactEachRow";

/// A live ClickHouse session over the HTTP interface. Holds the reused
/// `reqwest::Client`, the resolved endpoint, and the credentials (sent per request
/// as `X-ClickHouse-*` headers, never in the logged URL).
pub struct ClickhouseDriver {
    client: reqwest::Client,
    /// `http://host:port/`: every request POSTs its SQL here with the query/format
    /// options as URL params.
    base_url: String,
    user: String,
    password: String,
    database: String,
    read_only: bool,
    version: String,
    /// When set, the schema tree is restricted to this one database (the
    /// connection's chosen `database`); `None` lists every non-system database.
    scope: Option<String>,
}

impl ClickhouseDriver {
    /// Resolve the endpoint from the DSN, verify connectivity, and read the server
    /// version. The DSN is `clickhouse://user:pass@host:port/database`; we reuse
    /// `red-core`'s tested parser (it percent-decodes userinfo/database) rather than
    /// re-implement it. Defaults follow ClickHouse: user `default`, database
    /// `default`, port `8123`.
    pub async fn connect(dsn: &str, read_only: bool) -> Result<Self> {
        let parsed = ConnectionConfig::parse_conn_str(dsn)
            .ok_or_else(|| RedError::Connect(format!("invalid ClickHouse DSN: {dsn}")))?;
        let host = if parsed.host.is_empty() {
            "localhost".to_string()
        } else {
            parsed.host
        };
        // TLS (a `clickhouses://` DSN, see `ConnectionConfig::parse_conn_str`)
        // uses HTTPS on the secure interface's default port (8443); reqwest's
        // rustls stack (already in the tree) handles the handshake.
        let (scheme, default_port) = if parsed.tls {
            ("https", 8443)
        } else {
            ("http", 8123)
        };
        let port = parsed.port.unwrap_or(default_port);
        let base_url = format!("{scheme}://{}/", host_authority(&host, port));
        let user = if parsed.user.is_empty() {
            "default".to_string()
        } else {
            parsed.user
        };
        let database = if parsed.database.is_empty() {
            "default".to_string()
        } else {
            parsed.database
        };

        let mut driver = Self {
            client: reqwest::Client::new(),
            base_url,
            user,
            password: parsed.password,
            database,
            read_only,
            version: String::new(),
            scope: None,
        };
        driver.version = driver.fetch_version().await?;
        Ok(driver)
    }

    /// Restrict the schema tree to a single database. An empty name clears the
    /// scope (browse all databases). Like MySQL, a ClickHouse connection can see
    /// every database on the server. See the `scope` field.
    pub fn with_scope(mut self, database: Option<String>) -> Self {
        self.scope = database.filter(|d| !d.is_empty());
        self
    }

    /// Read `version()` at connect, mapping the result so a bad credential is a
    /// *fatal* [`RedError::Auth`] (the UI stops retrying and prompts for an edit)
    /// while an unreachable host stays a retryable [`RedError::Connect`]. ClickHouse
    /// answers an auth failure with HTTP 403/401 and the rest as plain-text bodies.
    async fn fetch_version(&self) -> Result<String> {
        let qid = new_query_id();
        let resp = self
            .build_query(
                "SELECT version() FORMAT JSONCompactEachRow".to_string(),
                &qid,
                &[],
            )
            .send()
            .await
            .map_err(|e| RedError::Connect(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|e| RedError::Connect(e.to_string()))?;
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(RedError::Auth(clean_error(&body)));
        }
        if !status.is_success() {
            return Err(RedError::Connect(clean_error(&body)));
        }
        // `JSONCompactEachRow` of a single scalar is one line: `["23.8.1.2"]`.
        let line = body.split(|&b| b == b'\n').find(|l| !l.is_empty());
        let vals: Vec<String> = line
            .and_then(|l| serde_json::from_slice(l).ok())
            .unwrap_or_default();
        Ok(vals.into_iter().next().unwrap_or_default())
    }

    /// Build a POST carrying `sql` in the body, with the query id, the read-only
    /// posture, and any `extra` URL params (parameter binds / settings). Credentials
    /// ride in headers, not the URL. A read-only connection appends `readonly=1`, so
    /// a write (including a write attempted through `execute`) is refused at the
    /// engine.
    fn build_query(
        &self,
        sql: String,
        query_id: &str,
        extra: &[(String, String)],
    ) -> reqwest::RequestBuilder {
        let mut q: Vec<(String, String)> = Vec::with_capacity(extra.len() + 2);
        q.push(("query_id".to_string(), query_id.to_string()));
        if self.read_only {
            q.push(("readonly".to_string(), "1".to_string()));
        }
        q.extend(extra.iter().cloned());
        self.client
            .post(&self.base_url)
            .header("X-ClickHouse-User", self.user.as_str())
            .header("X-ClickHouse-Key", self.password.as_str())
            .header("X-ClickHouse-Database", self.database.as_str())
            .query(&q)
            .body(sql)
    }

    /// An out-of-band cancel for `target_query_id`: `KILL QUERY WHERE query_id = …`
    /// over a fresh request (ClickHouse has no in-band cancel-request protocol).
    /// `ASYNC` so the kill returns without waiting for the doomed query to wind down.
    /// The kill never carries `readonly=1` (a read-only session still cancels its own
    /// query). Query ids are unique UUIDs, so a kill that races a just-finished fetch
    /// targets an id that no longer exists, a harmless no-op, so no liveness flag is
    /// needed (unlike MySQL's recycled thread ids).
    fn kill_token(&self, target_query_id: &str) -> CancelToken {
        let client = self.client.clone();
        let url = self.base_url.clone();
        let user = self.user.clone();
        let pass = self.password.clone();
        let target = target_query_id.to_string();
        CancelToken::new(move || {
            let client = client.clone();
            let url = url.clone();
            let user = user.clone();
            let pass = pass.clone();
            let target = target.clone();
            tokio::spawn(async move {
                let kill = format!("KILL QUERY WHERE query_id = '{target}' ASYNC");
                let _ = client
                    .post(&url)
                    .header("X-ClickHouse-User", user)
                    .header("X-ClickHouse-Key", pass)
                    .body(kill)
                    .send()
                    .await;
            });
        })
    }

    /// Run `base_sql` (FORMAT appended here) to completion and collect every row.
    /// Only the bounded one-shot paths use this: `count`, `fetch_page` (`LIMIT`),
    /// the seeks, `key_bounds`, introspection. Thus the whole (small) response fits in
    /// memory; the unbounded cursor/export paths stream instead. `abort` arms a
    /// `KILL QUERY` for the request's lifetime, so a superseded fetch is cancelled at
    /// the engine, not merely abandoned.
    async fn run_collect(
        &self,
        base_sql: String,
        params: &[(String, String)],
        abort: &AbortSignal,
    ) -> Result<RowBlock> {
        let qid = new_query_id();
        let guard = abort.arm(self.kill_token(&qid));
        // A fetch superseded before it starts bails without touching the engine.
        let result = if abort.is_aborted() {
            Err(RedError::Interrupted)
        } else {
            self.run_collect_inner(base_sql, params, &qid, abort).await
        };
        drop(guard); // disarm before returning, so a late abort can't re-fire
        result
    }

    async fn run_collect_inner(
        &self,
        base_sql: String,
        params: &[(String, String)],
        qid: &str,
        abort: &AbortSignal,
    ) -> Result<RowBlock> {
        let sql = format!("{base_sql} FORMAT {ROW_FORMAT}");
        let to_err = |e: reqwest::Error| {
            if abort.is_aborted() {
                RedError::Interrupted
            } else {
                driver_err(e)
            }
        };
        let resp = self
            .build_query(sql, qid, params)
            .send()
            .await
            .map_err(to_err)?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(to_err)?;
        if abort.is_aborted() {
            return Err(RedError::Interrupted);
        }
        if !status.is_success() {
            return Err(ch_error(&body));
        }
        parse_block(&body)
    }

    /// Introspection convenience: a collected fetch with no cancellation handle
    /// (`list_objects`/`describe_table` carry no `AbortSignal` in the trait).
    async fn run_simple(&self, base_sql: String, params: &[(String, String)]) -> Result<RowBlock> {
        self.run_collect(base_sql, params, &AbortSignal::new())
            .await
    }

    /// Open a streaming SELECT and read its two header lines (names, then types),
    /// returning the live response and whatever bytes were buffered past the header.
    /// Shared by the cursor and `export`. A query that fails *before* streaming
    /// (syntax/permission) surfaces here as a non-success status with the error in
    /// the body: the validation the trait expects at open time.
    async fn open_stream(&self, base_sql: &str, query_id: &str) -> Result<OpenedStream> {
        let sql = format!("{base_sql} FORMAT {ROW_FORMAT}");
        let resp = self
            .build_query(sql, query_id, &[])
            .send()
            .await
            .map_err(driver_err)?;
        if !resp.status().is_success() {
            let body = resp.bytes().await.map_err(driver_err)?;
            return Err(ch_error(&body));
        }
        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            if let Some((names, types, consumed)) = try_header(&buf) {
                buf.drain(..consumed);
                let columns = names
                    .iter()
                    .zip(types.iter())
                    .map(|(n, t)| Column {
                        name: n.clone(),
                        decl_type: Some(t.clone()),
                    })
                    .collect();
                return Ok((columns, types, resp, buf));
            }
            match resp.chunk().await.map_err(driver_err)? {
                Some(c) => buf.extend_from_slice(&c),
                None => {
                    return Err(RedError::Driver(
                        "ClickHouse returned no result header".to_string(),
                    ))
                }
            }
        }
    }
}

#[async_trait]
impl DatabaseDriver for ClickhouseDriver {
    async fn ping(&self) -> Result<()> {
        self.run_simple("SELECT 1".to_string(), &[])
            .await
            .map(|_| ())
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    async fn open_cursor(&self, sql: &str, opts: QueryOptions) -> Result<Box<dyn QueryCursor>> {
        let query_id = new_query_id();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = self.cursor_cancel_token(&query_id, cancelled.clone());
        let (columns, types, resp, buf) = self.open_stream(strip_trailing(sql), &query_id).await?;
        Ok(Box::new(ChCursor {
            columns,
            types,
            cancelled,
            cancel,
            full: opts.full_fidelity,
            inner: Mutex::new(ChStream {
                resp,
                buf,
                exhausted: false,
            }),
        }))
    }

    async fn list_objects(&self) -> Result<Vec<SchemaMeta>> {
        // One pass over `system.tables`: names + engines only (the cheap skeleton),
        // grouped into namespaces. `engine` ending in `View` (View / MaterializedView
        // / LiveView) marks a view; everything else is a table.
        let mut base = "SELECT database, name, engine FROM system.tables \
             WHERE database NOT IN ('system', 'information_schema', 'INFORMATION_SCHEMA')"
            .to_string();
        let params = if let Some(scope) = &self.scope {
            base.push_str(" AND database = {db:String}");
            vec![("param_db".to_string(), scope.clone())]
        } else {
            Vec::new()
        };
        base.push_str(" ORDER BY database, name");
        let (_, _, rows) = self.run_simple(base, &params).await?;

        let mut schemas: Vec<SchemaMeta> = Vec::new();
        for row in &rows {
            let db = row
                .first()
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string();
            let name = row
                .get(1)
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string();
            let engine = row.get(2).and_then(Json::as_str).unwrap_or_default();
            let kind = if engine.ends_with("View") {
                ObjectKind::View
            } else {
                ObjectKind::Table
            };
            // Rows are ordered by database, so consecutive same-db rows group.
            match schemas.last_mut() {
                Some(s) if s.name == db => s.objects.push(ObjectMeta { name, kind }),
                _ => schemas.push(SchemaMeta {
                    name: db,
                    objects: vec![ObjectMeta { name, kind }],
                }),
            }
        }
        Ok(schemas)
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableDetail> {
        // Columns from `system.columns`. A column is NOT NULL unless its type is
        // `Nullable(…)`; primary-key membership is `is_in_primary_key` (the MergeTree
        // ORDER BY / PRIMARY KEY). ClickHouse is OLAP: there are no foreign keys and
        // no secondary indexes in the relational sense, so both vecs stay empty.
        let base = "SELECT name, type, is_in_primary_key FROM system.columns \
             WHERE database = {db:String} AND table = {tbl:String} ORDER BY position"
            .to_string();
        let params = vec![
            ("param_db".to_string(), schema.to_string()),
            ("param_tbl".to_string(), table.to_string()),
        ];
        let (_, _, rows) = self.run_simple(base, &params).await?;
        let columns = rows
            .iter()
            .map(|row| {
                let name = row
                    .first()
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string();
                let type_name = row
                    .get(1)
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string();
                let in_pk = row.get(2).and_then(json_to_i64).unwrap_or(0) == 1;
                ColumnMeta {
                    not_null: !type_name.starts_with("Nullable("),
                    primary_key: in_pk,
                    type_name: Some(type_name),
                    default: None,
                    name,
                    auto_increment: false,
                }
            })
            .collect();
        Ok(TableDetail {
            columns,
            foreign_keys: Vec::new(),
            indexes: Vec::new(),
        })
    }

    async fn foreign_keys(&self) -> Result<Vec<FkEdge>> {
        // OLAP: ClickHouse has no relational foreign keys, so the graph is empty and
        // the FK-navigation feature degrades to absent.
        Ok(Vec::new())
    }

    fn contains_predicate(&self, columns: &[ColumnMeta], term: &str) -> Option<String> {
        // ClickHouse `ILIKE` is case-insensitive; its escape char is always `\` and
        // there is no `ESCAPE` clause, so suppress it (last arg `false`). String
        // literals treat `\` as an escape, so the pattern's backslashes get the
        // second doubling (`backslash_escapes = true`).
        crate::contains_clause(
            columns,
            term,
            ch_quote,
            |c| format!("CAST({c} AS String)"),
            "ILIKE",
            true,
            false,
        )
    }

    fn eq_predicate(&self, pairs: &[ColumnValue]) -> String {
        crate::eq_clause(pairs, ch_quote, true)
    }

    async fn count(&self, sql: &str, abort: &AbortSignal) -> Result<i64> {
        let base = format!("SELECT count() FROM ({}) AS _red", strip_trailing(sql));
        let (_, _, rows) = self.run_collect(base, &[], abort).await?;
        Ok(rows
            .first()
            .and_then(|r| r.first())
            .and_then(json_to_i64)
            .unwrap_or(0))
    }

    async fn column_stats(
        &self,
        sql: &str,
        column: &str,
        numeric: bool,
        distinct: bool,
        abort: &AbortSignal,
    ) -> Result<red_core::ColumnStats> {
        // OLAP loves aggregates; this is a plain read, like every other ClickHouse path.
        let base = crate::stats_sql(sql, column, numeric, distinct, ch_quote);
        let (_, types, rows) = self.run_collect(base, &[], abort).await?;
        // One aggregate row, decoded by the response's column types then read
        // positionally.
        let cells = rows
            .first()
            .map(|r| ch_row(r, &types, None))
            .unwrap_or_default();
        Ok(crate::parse_stats(&cells, numeric, distinct))
    }

    async fn fetch_page(
        &self,
        sql: &str,
        offset: usize,
        limit: usize,
        cap: PageCap,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let base = format!(
            "SELECT * FROM ({}) AS _red LIMIT {limit} OFFSET {offset}",
            strip_trailing(sql)
        );
        let (columns, types, rows) = self.run_collect(base, &[], abort).await?;
        let cap = CellCap::resolve(&cap, &columns);
        Ok(ResultPage {
            rows: rows.iter().map(|r| ch_row(r, &types, cap)).collect(),
            columns,
        })
    }

    async fn fetch_seek(
        &self,
        sql: &str,
        key: &KeySpec,
        bound: Option<&[Value]>,
        descending: bool,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let bound = bound.unwrap_or(&[]);
        let types = bound
            .iter()
            .map(ch_param_type)
            .collect::<Result<Vec<_>>>()?;
        // Typed placeholders `{p0:Int64}` keep the bound a real parameter (bound via
        // `param_p0` URL params), never string-interpolated into the SQL.
        let (where_clause, order_by) =
            crate::seek_clauses(key, bound.len(), descending, false, ch_quote, |i| {
                format!("{{p{i}:{}}}", types[i])
            });
        let base = format!(
            "SELECT * FROM ({}) AS _red {where_clause}ORDER BY {order_by} LIMIT {limit}",
            strip_trailing(sql)
        );
        let (columns, ctypes, rows) = self.run_collect(base, &ch_params(bound), abort).await?;
        let cap = CellCap::display(crate::key_positions(key, &columns));
        Ok(ResultPage {
            rows: rows.iter().map(|r| ch_row(r, &ctypes, cap)).collect(),
            columns,
        })
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
        let from = from.unwrap_or(&[]);
        let types = from.iter().map(ch_param_type).collect::<Result<Vec<_>>>()?;
        // Inclusive lower bound (`>=`), then `OFFSET skip` within the post-seek window.
        let (where_clause, order_by) =
            crate::seek_clauses(key, from.len(), false, true, ch_quote, |i| {
                format!("{{p{i}:{}}}", types[i])
            });
        let base = format!(
            "SELECT * FROM ({}) AS _red {where_clause}ORDER BY {order_by} LIMIT {limit} OFFSET {skip}",
            strip_trailing(sql)
        );
        let (columns, ctypes, rows) = self.run_collect(base, &ch_params(from), abort).await?;
        let cap = CellCap::display(crate::key_positions(key, &columns));
        Ok(ResultPage {
            rows: rows.iter().map(|r| ch_row(r, &ctypes, cap)).collect(),
            columns,
        })
    }

    async fn key_bounds(
        &self,
        sql: &str,
        key: &KeySpec,
        abort: &AbortSignal,
    ) -> Result<Option<(i64, i64)>> {
        let col = ch_quote(&key.column);
        let base = format!(
            "SELECT min({col}) AS lo, max({col}) AS hi FROM ({}) AS _red",
            strip_trailing(sql)
        );
        let (_, _, rows) = self.run_collect(base, &[], abort).await?;
        Ok(rows.first().and_then(|r| {
            match (
                r.first().and_then(json_to_i64),
                r.get(1).and_then(json_to_i64),
            ) {
                (Some(lo), Some(hi)) => Some((lo, hi)),
                _ => None,
            }
        }))
    }

    async fn execute(&self, sql: &str) -> Result<u64> {
        // DDL / INSERT from the SQL editor. A read-only connection carries
        // `readonly=1`, so the engine refuses the write (defense in depth). On a
        // writable connection, `wait_end_of_query=1` makes ClickHouse finish before
        // responding so the `X-ClickHouse-Summary` (carrying `written_rows`) is known
        // at the response head rather than only as a streamed trailer.
        let qid = new_query_id();
        let settings: Vec<(String, String)> = if self.read_only {
            Vec::new()
        } else {
            vec![("wait_end_of_query".to_string(), "1".to_string())]
        };
        let resp = self
            .build_query(strip_trailing(sql).to_string(), &qid, &settings)
            .send()
            .await
            .map_err(driver_err)?;
        let status = resp.status();
        let summary = resp
            .headers()
            .get("x-clickhouse-summary")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = resp.bytes().await.map_err(driver_err)?;
        if !status.is_success() {
            return Err(ch_error(&body));
        }
        Ok(summary.as_deref().and_then(parse_written_rows).unwrap_or(0))
    }

    async fn execute_batch(&self, statements: &[String]) -> Result<Vec<u64>> {
        // ClickHouse has no multi-statement transactions, so this is NOT atomic: the
        // statements run in order and a failure leaves earlier ones applied (the error
        // stops the rest). Acceptable because ClickHouse is a rarely-written OLAP
        // target here; the SQL engines above wrap the same call in a real transaction.
        let mut affected = Vec::with_capacity(statements.len());
        for sql in statements {
            affected.push(self.execute(sql).await?);
        }
        Ok(affected)
    }

    async fn apply_edits(&self, ops: &[EditOp]) -> Result<u64> {
        // An empty batch is a no-op (matching the trait contract) so a stray submit
        // doesn't error. Otherwise: ClickHouse is OLAP, so `UPDATE`/`DELETE` are
        // asynchronous `ALTER TABLE … UPDATE` mutations with no transaction or
        // rollback, so the atomic, exactly-one-row contract this method promises
        // cannot be honored. Refuse clearly rather than half-apply something the grid
        // can't safely offer. A best-effort, non-atomic mutation mode is a later phase.
        if ops.is_empty() {
            return Ok(0);
        }
        Err(RedError::Driver(
            "in-grid editing is not supported on ClickHouse (OLAP): UPDATE/DELETE are \
             asynchronous ALTER … mutations with no transactional rollback. Use the SQL \
             editor for ALTER TABLE … UPDATE/DELETE if you need them."
                .to_string(),
        ))
    }

    async fn insert_rows(
        &self,
        table: &TableRef,
        columns: &[Column],
        rows: &[Vec<Value>],
    ) -> Result<u64> {
        // An empty chunk is a no-op (matching the trait contract) without a round-trip.
        if rows.is_empty() {
            return Ok(0);
        }
        // ClickHouse's HTTP interface has no bound-parameter protocol for bulk rows,
        // so insert the native way: an `INSERT … FORMAT JSONCompactEachRow` statement
        // followed by one JSON array per row in the same POST body. `serde_json` does
        // the escaping, so no value is string-interpolated into SQL.
        let cols = columns
            .iter()
            .map(|c| ch_quote(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let mut body = format!(
            "INSERT INTO {} ({cols}) FORMAT {INSERT_FORMAT}\n",
            crate::qualify_table(table, ch_quote)
        );
        for row in rows {
            let cells: Vec<Json> = row.iter().map(ch_json_cell).collect();
            body.push_str(&serde_json::to_string(&cells).map_err(driver_err)?);
            body.push('\n');
        }
        // `wait_end_of_query=1` on a writable connection so the summary's
        // `written_rows` is known at the response head (mirrors `execute`); a
        // read-only connection carries `readonly=1` and the engine refuses the write.
        let qid = new_query_id();
        let settings: Vec<(String, String)> = if self.read_only {
            Vec::new()
        } else {
            vec![("wait_end_of_query".to_string(), "1".to_string())]
        };
        let resp = self
            .build_query(body, &qid, &settings)
            .send()
            .await
            .map_err(driver_err)?;
        let status = resp.status();
        let summary = resp
            .headers()
            .get("x-clickhouse-summary")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let resp_body = resp.bytes().await.map_err(driver_err)?;
        if !status.is_success() {
            return Err(ch_error(&resp_body));
        }
        // The summary carries the real count; fall back to the row count we sent.
        Ok(summary
            .as_deref()
            .and_then(parse_written_rows)
            .unwrap_or(rows.len() as u64))
    }

    async fn clear_table(&self, table: &TableRef) -> Result<u64> {
        // `TRUNCATE` is ClickHouse's clean, synchronous table-empty, the natural
        // copy-replace op. (The trait's DELETE-for-uniformity note is about MySQL's
        // auto-committing, auto-increment-resetting TRUNCATE; ClickHouse's has no such
        // surprise.) It reports no row count, so the affected count comes back 0. A
        // read-only connection is refused at the engine via `execute`.
        self.execute(&format!(
            "TRUNCATE TABLE {}",
            crate::qualify_table(table, ch_quote)
        ))
        .await
    }

    async fn create_table(&self, table: &TableRef, columns: &[ColumnMeta]) -> Result<u64> {
        // ClickHouse DDL diverges enough from the shared `create_table_sql` (an engine
        // + sort key are mandatory, nullability is `Nullable(T)` not a `NOT NULL`
        // suffix) to warrant its own builder. Runs through `execute`, so a read-only
        // connection is refused at the engine.
        self.execute(&ch_create_table_sql(table, columns)).await
    }

    fn quote_table(&self, table: &TableRef) -> String {
        crate::qualify_table(table, ch_quote)
    }

    fn quote_ident(&self, ident: &str) -> String {
        ch_quote(ident)
    }

    async fn create_index(
        &self,
        _table: &TableRef,
        _name: &str,
        _unique: bool,
        _columns: &[String],
    ) -> Result<u64> {
        // ClickHouse's data-skipping indexes aren't relational secondary indexes, so a
        // migrated index has no faithful equivalent; the migrate job logs the skip.
        Err(RedError::Driver(
            "secondary indexes have no relational equivalent on ClickHouse (OLAP)".to_string(),
        ))
    }

    async fn add_foreign_key(
        &self,
        _child: &TableRef,
        _columns: &[String],
        _parent: &TableRef,
        _ref_columns: &[String],
    ) -> Result<u64> {
        // ClickHouse (OLAP) has no foreign keys, so the migrate job logs the skip.
        Err(RedError::Driver(
            "foreign keys are not supported on ClickHouse (OLAP)".to_string(),
        ))
    }

    async fn explain(&self, sql: &str, _analyze: bool) -> Result<QueryPlan> {
        // ClickHouse `EXPLAIN` is plan-only and read-only-safe: it never executes the
        // statement, so there is no `EXPLAIN ANALYZE` actual-time/row counterpart; the
        // `analyze` flag is accepted but ignored. The output is an indentation-nested
        // text plan with no node markers, parsed by `plan::from_indent_tree`.
        let base = format!("EXPLAIN {}", strip_trailing(sql));
        let (_, _, rows) = self.run_simple(base, &[]).await?;
        let text = rows
            .iter()
            .filter_map(|r| r.first())
            .filter_map(Json::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(crate::plan::from_indent_tree(&text))
    }

    async fn export(
        &self,
        sql: &str,
        path: &Path,
        format: ExportFormat,
        cancel: Arc<AtomicBool>,
        progress: UnboundedSender<u64>,
    ) -> Result<u64> {
        let qid = new_query_id();
        let base = format!("SELECT * FROM ({}) AS _red", strip_trailing(sql));
        let (columns, types, mut resp, mut buf) = self.open_stream(&base, &qid).await?;
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();

        let file = File::create(path).map_err(driver_err)?;
        let table = crate::format::sql_table_name(path);
        let mut writer =
            ExportWriter::begin(BufWriter::new(file), format, names, table).map_err(driver_err)?;
        let mut throttle = ProgressThrottle::new(progress);

        // Bail on cancel: drop the writer, remove the partial file, report interruption.
        macro_rules! bail_if_cancelled {
            () => {
                if cancel.load(Ordering::Relaxed) {
                    drop(writer);
                    let _ = remove_file(path);
                    return Err(RedError::Interrupted);
                }
            };
        }

        let mut exhausted = false;
        loop {
            // Drain every complete line currently buffered, full-fidelity (no cap).
            while let Some(line) = take_line(&mut buf) {
                if line.is_empty() {
                    continue;
                }
                bail_if_cancelled!();
                let raw: Vec<Json> = serde_json::from_slice(&line).map_err(driver_err)?;
                writer
                    .write_row(&ch_row(&raw, &types, None))
                    .map_err(driver_err)?;
                throttle.tick(writer.written());
            }
            if exhausted {
                break;
            }
            bail_if_cancelled!();
            match resp.chunk().await.map_err(driver_err)? {
                Some(c) => buf.extend_from_slice(&c),
                None => {
                    exhausted = true;
                    // A trailing line without a newline (ClickHouse normally terminates
                    // every row, but be safe).
                    if !buf.iter().all(u8::is_ascii_whitespace) {
                        bail_if_cancelled!();
                        let raw: Vec<Json> = serde_json::from_slice(&buf).map_err(driver_err)?;
                        writer
                            .write_row(&ch_row(&raw, &types, None))
                            .map_err(driver_err)?;
                        buf.clear();
                    }
                }
            }
        }
        writer.finish().map_err(driver_err)
    }
}

impl ClickhouseDriver {
    /// The cursor's cancel token: flip the cursor's `cancelled` flag *and* fire the
    /// `KILL QUERY`. The flag is what `next_window` checks after the killed stream
    /// ends/errors, so it surfaces a clean [`RedError::Interrupted`] rather than a
    /// truncated result or a connection-reset error.
    fn cursor_cancel_token(&self, query_id: &str, cancelled: Arc<AtomicBool>) -> CancelToken {
        let killer = self.kill_token(query_id);
        CancelToken::new(move || {
            cancelled.store(true, Ordering::SeqCst);
            killer.cancel();
        })
    }
}

/// The streaming cursor: column metadata + types known up front, the live response
/// behind a `Mutex` (so `next_window(&self)` can pull), a `cancelled` flag the kill
/// path flips, and the out-of-band cancel token.
struct ChCursor {
    columns: Vec<Column>,
    types: Vec<String>,
    cancelled: Arc<AtomicBool>,
    cancel: CancelToken,
    /// Read cells at full fidelity (the table-copy read, e.g. ClickHouse → SQLite)
    /// rather than the display fat-cell cap; see
    /// [`QueryOptions::full_fidelity`](red_core::QueryOptions).
    full: bool,
    inner: Mutex<ChStream>,
}

/// The mutable streaming state behind the cursor's `Mutex`: the live HTTP response,
/// a byte buffer of not-yet-parsed stream bytes, and whether the stream is drained.
struct ChStream {
    resp: reqwest::Response,
    buf: Vec<u8>,
    exhausted: bool,
}

#[async_trait]
impl QueryCursor for ChCursor {
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
        let mut inner = self.inner.lock().await;
        let mut rows = Vec::with_capacity(window_prealloc(max));
        loop {
            // Parse complete buffered lines up to the window size.
            while rows.len() < max {
                match take_line(&mut inner.buf) {
                    Some(line) if line.is_empty() => continue,
                    Some(line) => rows.push(parse_row_line(&line, &self.types, cap)?),
                    None => break,
                }
            }
            if rows.len() >= max {
                return Ok(RowWindow {
                    rows,
                    exhausted: false,
                });
            }
            // A cancel that fired between iterations surfaces promptly.
            if self.cancelled.load(Ordering::SeqCst) {
                return Err(RedError::Interrupted);
            }
            if inner.exhausted {
                // Flush any trailing newline-less line, then we're done.
                if !inner.buf.iter().all(u8::is_ascii_whitespace) {
                    let line = std::mem::take(&mut inner.buf);
                    rows.push(parse_row_line(&line, &self.types, cap)?);
                }
                return Ok(RowWindow {
                    rows,
                    exhausted: true,
                });
            }
            match inner.resp.chunk().await {
                Ok(Some(chunk)) => inner.buf.extend_from_slice(&chunk),
                Ok(None) => inner.exhausted = true,
                Err(e) => {
                    // A killed stream ends with an error (or an abrupt close); the
                    // `cancelled` flag is the authoritative signal that this was a
                    // cancel, not a genuine failure.
                    if self.cancelled.load(Ordering::SeqCst) {
                        return Err(RedError::Interrupted);
                    }
                    return Err(driver_err(e));
                }
            }
        }
    }

    fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }
}

/// A fresh per-query id for `query_id` (and so the `KILL QUERY` target). A UUID is
/// `[0-9a-f-]` only, so it embeds safely in the kill statement's literal.
fn new_query_id() -> String {
    Uuid::new_v4().to_string()
}

/// `host:port`, bracketing an IPv6 literal so the `:port` separator stays
/// unambiguous. The host comes unbracketed from the DSN parser.
fn host_authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Build a ClickHouse `CREATE TABLE IF NOT EXISTS … ENGINE = MergeTree ORDER BY …`.
/// The shared [`create_table_sql`](crate::create_table_sql) isn't usable here:
/// ClickHouse expresses nullability as `Nullable(T)` (columns are NOT NULL by
/// default, with no `NOT NULL` suffix), a `MergeTree` table *requires* an `ENGINE`
/// and an `ORDER BY`, and the relational trailing `PRIMARY KEY (…)` clause maps onto
/// the sort key instead. Column types are spelled into ClickHouse's dialect via
/// [`typemap`](red_core::typemap); the primary-key columns become the `ORDER BY`
/// (or `tuple()`, the no-sort-key sentinel, when the source had none). A nullable
/// sort-key column (a migration source can have one) needs `allow_nullable_key`,
/// which MergeTree otherwise rejects. Identifiers are quoted, never interpolated raw.
fn ch_create_table_sql(table: &TableRef, columns: &[ColumnMeta]) -> String {
    use red_core::typemap::{normalize, spell};
    let defs: Vec<String> = columns
        .iter()
        .map(|c| {
            let nt = normalize(c.type_name.as_deref().unwrap_or(""));
            let ty = spell(DbKind::Clickhouse, &nt);
            // NOT NULL is the ClickHouse default; a nullable source column wraps.
            let ty = if c.not_null {
                ty
            } else {
                format!("Nullable({ty})")
            };
            format!("{} {ty}", ch_quote(&c.name))
        })
        .collect();
    let pk: Vec<&ColumnMeta> = columns.iter().filter(|c| c.primary_key).collect();
    let order_by = if pk.is_empty() {
        "tuple()".to_string()
    } else {
        format!(
            "({})",
            pk.iter()
                .map(|c| ch_quote(&c.name))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let settings = if pk.iter().any(|c| !c.not_null) {
        " SETTINGS allow_nullable_key = 1"
    } else {
        ""
    };
    format!(
        "CREATE TABLE IF NOT EXISTS {} ({}) ENGINE = MergeTree ORDER BY {order_by}{settings}",
        crate::qualify_table(table, ch_quote),
        defs.join(", ")
    )
}

/// Map a [`Value`] to the JSON cell an `INSERT … FORMAT JSONCompactEachRow` body
/// carries. A [`Value::Capped`] never reaches a write path by contract (capped cells
/// are display-only), but is mapped to its head defensively rather than dropped. A
/// blob becomes a JSON string via lossy UTF-8; ClickHouse's only binary-ish type is
/// `String`, and a genuinely non-UTF-8 blob copied in from another engine is a rare
/// edge that would need `RowBinary` to preserve exactly.
fn ch_json_cell(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Integer(n) => Json::from(*n),
        Value::Real(x) => Json::from(*x),
        Value::Text(s) => Json::from(&**s),
        Value::Blob(b) => Json::from(String::from_utf8_lossy(b).into_owned()),
        Value::Capped(c) => Json::from(c.head.as_str()),
    }
}

/// Quote a ClickHouse identifier with backticks. ClickHouse processes backslash
/// escapes *inside* backtick-quoted identifiers (unlike MySQL backticks), so the
/// backslash must be doubled as well as the backtick; otherwise a name ending in
/// `\` (or a smuggled `` \` ``) escapes the closing backtick and breaks out of the
/// identifier. Double `\` first so the backticks added next aren't re-escaped.
fn ch_quote(ident: &str) -> String {
    format!("`{}`", ident.replace('\\', "\\\\").replace('`', "``"))
}

/// Extract the next newline-delimited line from `buf` (consuming it, including the
/// `\n`), with the trailing `\n`/`\r` stripped. `None` when no complete line is
/// buffered yet.
fn take_line(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let pos = buf.iter().position(|&b| b == b'\n')?;
    let mut line: Vec<u8> = buf.drain(..=pos).collect();
    line.pop(); // the '\n'
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Some(line)
}

/// Read the first two header lines (column names, then types) from a streamed
/// response's buffered prefix, returning them plus the number of bytes consumed.
/// `None` until both lines are fully buffered.
fn try_header(buf: &[u8]) -> Option<(Vec<String>, Vec<String>, usize)> {
    let first = buf.iter().position(|&b| b == b'\n')?;
    let second_rel = buf[first + 1..].iter().position(|&b| b == b'\n')?;
    let second = first + 1 + second_rel;
    let names: Vec<String> = serde_json::from_slice(&buf[..first]).ok()?;
    let types: Vec<String> = serde_json::from_slice(&buf[first + 1..second]).ok()?;
    Some((names, types, second + 1))
}

/// Parse a whole `JSONCompactEachRowWithNamesAndTypes` body into columns, the raw
/// type strings, and the raw JSON cell rows: the collected (bounded) read path.
fn parse_block(body: &[u8]) -> Result<RowBlock> {
    let mut lines = body
        .split(|&b| b == b'\n')
        .filter(|l| !l.iter().all(|c| c.is_ascii_whitespace()));
    let names: Vec<String> = serde_json::from_slice(
        lines
            .next()
            .ok_or_else(|| RedError::Driver("empty ClickHouse response".to_string()))?,
    )
    .map_err(driver_err)?;
    let types: Vec<String> =
        serde_json::from_slice(lines.next().ok_or_else(|| {
            RedError::Driver("ClickHouse response missing type header".to_string())
        })?)
        .map_err(driver_err)?;
    let columns = names
        .iter()
        .zip(types.iter())
        .map(|(n, t)| Column {
            name: n.clone(),
            decl_type: Some(t.clone()),
        })
        .collect();
    let mut rows = Vec::new();
    for l in lines {
        rows.push(serde_json::from_slice::<Vec<Json>>(l).map_err(driver_err)?);
    }
    Ok((columns, types, rows))
}

/// Parse one streamed JSON-array line into a display row.
fn parse_row_line(line: &[u8], types: &[String], cap: Option<CellCap>) -> Result<Vec<Value>> {
    let raw: Vec<Json> = serde_json::from_slice(line).map_err(driver_err)?;
    Ok(ch_row(&raw, types, cap))
}

/// Map one raw JSON row to display [`Value`]s, per the column types and any cell cap.
fn ch_row(raw: &[Json], types: &[String], cap: Option<CellCap>) -> Vec<Value> {
    raw.iter()
        .enumerate()
        .map(|(i, v)| {
            let ty = types.get(i).map(String::as_str).unwrap_or("");
            ch_value(v, ty, CellCap::caps(cap, i))
        })
        .collect()
}

/// Map one JSON cell to a [`Value`], guided by the ClickHouse declared type. The
/// `JSON…` format already rendered every type to JSON text, so this is a small
/// classification: integers (numbers, or quoted strings for the 64-bit widths) →
/// [`Value::Integer`]; floats → [`Value::Real`]; everything else (decimal, date,
/// uuid, enum, and the composite `Array`/`Tuple`/`Map`) → text, capped if oversized.
fn ch_value(v: &Json, ch_type: &str, max: Option<usize>) -> Value {
    match v {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Integer(*b as i64),
        Json::Number(n) => {
            if is_ch_int(ch_type) {
                if let Some(i) = n.as_i64() {
                    return Value::Integer(i);
                }
            }
            if is_ch_float(ch_type) {
                if let Some(f) = n.as_f64() {
                    return Value::Real(f);
                }
            }
            // Decimal and out-of-i64-range integers keep their exact JSON text.
            text_value(&n.to_string(), max)
        }
        Json::String(s) => {
            if is_ch_int(ch_type) {
                if let Ok(i) = s.parse::<i64>() {
                    return Value::Integer(i);
                }
            }
            if is_ch_float(ch_type) {
                if let Ok(f) = s.parse::<f64>() {
                    return Value::Real(f);
                }
            }
            text_value(s, max)
        }
        // Composite (Array / Tuple / Map / Nested): render the compact JSON text.
        other => text_value(&other.to_string(), max),
    }
}

/// A text [`Value`], capped to a display prefix when `max` is set.
fn text_value(s: &str, max: Option<usize>) -> Value {
    match max {
        Some(m) => Value::capped_text(s, m),
        None => Value::Text(s.into()),
    }
}

/// The ClickHouse base type, with `Nullable(…)` / `LowCardinality(…)` wrappers
/// peeled off so type classification sees `Int32` rather than `Nullable(Int32)`.
fn ch_base_type(ty: &str) -> &str {
    let mut t = ty.trim();
    loop {
        if let Some(inner) = t
            .strip_prefix("Nullable(")
            .and_then(|s| s.strip_suffix(')'))
        {
            t = inner.trim();
        } else if let Some(inner) = t
            .strip_prefix("LowCardinality(")
            .and_then(|s| s.strip_suffix(')'))
        {
            t = inner.trim();
        } else {
            return t;
        }
    }
}

/// Whether a ClickHouse type is an integer family (`Int8`..`Int256`,
/// `UInt8`..`UInt256`), but not `Interval*`, which also begins `Int`-adjacent.
fn is_ch_int(ty: &str) -> bool {
    let base = ch_base_type(ty);
    base.starts_with("UInt") || (base.starts_with("Int") && !base.starts_with("Interval"))
}

/// Whether a ClickHouse type is a floating type (`Float32`/`Float64`). `Decimal`
/// is deliberately *not* here; it's rendered as exact text to avoid f64 rounding.
fn is_ch_float(ty: &str) -> bool {
    ch_base_type(ty).starts_with("Float")
}

/// The ClickHouse placeholder type for a seek-bound value. Key columns are never
/// null/capped/blob (the contract), so those are a query error rather than a bind.
fn ch_param_type(v: &Value) -> Result<&'static str> {
    Ok(match v {
        Value::Integer(_) => "Int64",
        Value::Real(_) => "Float64",
        Value::Text(_) => "String",
        Value::Blob(_) | Value::Null | Value::Capped(_) => {
            return Err(RedError::Query(
                "unsupported ClickHouse seek bound".to_string(),
            ))
        }
    })
}

/// The `param_pN` URL params binding each seek-bound value. ClickHouse substitutes
/// the value per the placeholder's declared type, so the text form is enough (no
/// quoting); a non-bindable variant yields an empty string (already rejected by
/// [`ch_param_type`] before this is reached).
fn ch_params(bound: &[Value]) -> Vec<(String, String)> {
    bound
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let text = match v {
                Value::Integer(n) => n.to_string(),
                Value::Real(x) => x.to_string(),
                Value::Text(s) => s.to_string(),
                _ => String::new(),
            };
            (format!("param_p{i}"), text)
        })
        .collect()
}

/// Coerce a JSON cell to `i64` for `count` / `key_bounds` / `is_in_primary_key`:
/// a JSON number directly, or a quoted 64-bit integer string parsed.
fn json_to_i64(v: &Json) -> Option<i64> {
    match v {
        Json::Number(n) => n.as_i64(),
        Json::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Pull `written_rows` out of an `X-ClickHouse-Summary` header value (a JSON object
/// whose counters are quoted strings), for `execute`'s affected-row count.
fn parse_written_rows(summary: &str) -> Option<u64> {
    let json: Json = serde_json::from_str(summary).ok()?;
    json.get("written_rows")?.as_str()?.parse().ok()
}

/// Map a ClickHouse error body to a [`RedError`]: a query that was killed becomes
/// the distinct [`RedError::Interrupted`]; anything else is a [`RedError::Query`]
/// carrying the server's (cleaned) message.
fn ch_error(body: &[u8]) -> RedError {
    let text = String::from_utf8_lossy(body);
    if is_cancel_error(&text) {
        return RedError::Interrupted;
    }
    RedError::Query(clean_error(body))
}

/// Whether an error body is ClickHouse's query-cancellation (`KILL QUERY`) signal.
fn is_cancel_error(text: &str) -> bool {
    text.contains("QUERY_WAS_CANCELLED")
        || text.contains("Query was cancelled")
        || text.contains("Code: 394")
}

/// Trim a ClickHouse error/text body to a tidy single message (bounded length so a
/// giant stack-y exception can't flood a toast).
fn clean_error(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if trimmed.len() > 500 {
        let mut end = 500;
        while !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    } else {
        trimmed.to_string()
    }
}

// Live tests run against a ClickHouse server provided via `RED_TEST_CLICKHOUSE_URL`,
// so CI without one skips cleanly. Spin one up with:
//
//   docker run --rm -d -p 8123:8123 --name red-ch clickhouse/clickhouse-server:24
//   export RED_TEST_CLICKHOUSE_URL='clickhouse://default@127.0.0.1:8123/default'
//
// ClickHouse is OLAP: the conformance battery's 3 edit scenarios are excluded by
// design (no transactional in-grid editing), and two scenarios are replaced by
// ClickHouse-specific variants because their relational assumptions don't hold:
//   * introspection: ClickHouse has no foreign keys or secondary indexes, so the
//     shared helper (which asserts both) is replaced by a tables/views/columns/PK
//     check with empty FK/index vecs;
//   * the contains filter and the display-cap check assert a distinct BLOB type,
//     which ClickHouse lacks (binary is `String`), so those get tailored variants.
// Everything else (streaming, cancel, seek, count, export, explain, read-only) runs
// the shared battery unchanged.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance as battery;
    use red_core::{KeyKind, ObjectKind};
    use std::time::Duration;

    fn test_url() -> Option<String> {
        std::env::var("RED_TEST_CLICKHOUSE_URL").ok()
    }

    macro_rules! url_or_skip {
        () => {
            match test_url() {
                Some(u) => u,
                None => {
                    eprintln!("SKIP {}: RED_TEST_CLICKHOUSE_URL not set", module_path!());
                    return;
                }
            }
        };
    }

    /// A unique fixture-name suffix so concurrent tests don't collide on a shared
    /// server.
    fn tag(name: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        format!("red_{name}_{}_{n}", std::process::id())
    }

    /// The connection's database: unqualified fixtures land here, so introspection
    /// filters to it. Pulled from the DSN we connected with.
    fn database(url: &str) -> String {
        red_core::ConnectionConfig::parse_conn_str(url)
            .map(|p| {
                if p.database.is_empty() {
                    "default".to_string()
                } else {
                    p.database
                }
            })
            .unwrap_or_else(|| "default".to_string())
    }

    #[tokio::test]
    async fn connect_reports_version() {
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, true).await.unwrap();
        assert!(!driver.server_version().is_empty());
        driver.ping().await.unwrap();
    }

    #[tokio::test]
    async fn streams_in_bounded_windows() {
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, true).await.unwrap();
        // `system.numbers` is a server-side streaming source: no fixture, never
        // materialized server-side, mirroring the windowed read.
        battery::streams_in_bounded_windows(
            &driver,
            "SELECT number FROM system.numbers LIMIT 100000",
            100_000,
        )
        .await;
    }

    #[tokio::test]
    async fn cancel_aborts_in_flight_fetch() {
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, true).await.unwrap();
        // An unbounded scan keeps the server streaming long enough to KILL it.
        battery::cancel_aborts_in_flight_fetch(
            &driver,
            "SELECT number FROM system.numbers",
            Duration::from_millis(200),
        )
        .await;
    }

    #[tokio::test]
    async fn superseded_one_shot_fetch_is_cancelled() {
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, true).await.unwrap();
        // A 10^11-row count keeps the server busy enough to interrupt out-of-band.
        let heavy = "SELECT number FROM numbers(100000000000)";
        battery::superseded_fetch_is_cancelled(&driver, heavy, Duration::from_millis(200)).await;
        battery::pre_aborted_fetch_returns_immediately(&driver, heavy).await;
        battery::abort_after_completion_is_noop(&driver, "SELECT 1").await;
    }

    #[tokio::test]
    async fn introspects_tables_views_columns_and_pk() {
        // ClickHouse-specific introspection: tables/views/columns/PK, with empty FK
        // and index vecs (OLAP has neither). Replaces the shared helper, which asserts
        // a foreign key and a secondary index.
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, false).await.unwrap();
        let db = database(&url);
        let books = tag("books");
        let recent = tag("recent");

        driver
            .execute(&format!(
                "CREATE TABLE {books} (\
                   id Int32, \
                   title String, \
                   author_id Int32\
                 ) ENGINE = MergeTree ORDER BY id"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!("CREATE VIEW {recent} AS SELECT * FROM {books}"))
            .await
            .unwrap();

        let schemas = driver.list_objects().await.unwrap();
        let ns = schemas
            .iter()
            .find(|s| s.name == db)
            .unwrap_or_else(|| panic!("database {db} present in the tree"));
        let objects: Vec<(&str, ObjectKind)> = ns
            .objects
            .iter()
            .map(|o| (o.name.as_str(), o.kind))
            .collect();
        assert!(objects.contains(&(books.as_str(), ObjectKind::Table)));
        assert!(objects.contains(&(recent.as_str(), ObjectKind::View)));

        let detail = driver.describe_table(&db, &books).await.unwrap();
        let col = |n: &str| {
            detail
                .columns
                .iter()
                .find(|c| c.name == n)
                .unwrap_or_else(|| panic!("column {n} present on {books}"))
        };
        assert!(
            col("id").primary_key,
            "id is in the (MergeTree) primary key"
        );
        assert!(col("title").not_null, "a non-Nullable column is NOT NULL");
        assert!(detail.foreign_keys.is_empty(), "OLAP: no foreign keys");
        assert!(detail.indexes.is_empty(), "OLAP: no secondary indexes");

        driver
            .execute(&format!("DROP TABLE {recent}"))
            .await
            .unwrap();
        driver
            .execute(&format!("DROP TABLE {books}"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn filters_contains_literally_and_case_insensitively() {
        // ClickHouse-specific contains: ClickHouse has no distinct BLOB type (binary
        // is `String`), so this drops the shared helper's blob-exclusion assertion and
        // keeps the literal-match / case-insensitive / quote-escaping checks.
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, false).await.unwrap();
        let t = tag("filter");
        driver
            .execute(&format!(
                "CREATE TABLE {t} (id Int32, name String, note String) ENGINE = MergeTree ORDER BY id"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {t} VALUES \
                 (1,'apple','red fruit'),(2,'banana','yellow'),\
                 (3,'apple pie','dessert'),(4,'100% juice','on sale'),(5,'O''Brien','name')"
            ))
            .await
            .unwrap();

        let detail = driver.describe_table(&database(&url), &t).await.unwrap();
        let abort = AbortSignal::new();
        // Borrow `driver` (don't move it into a closure) so it survives for the DROP.
        let filtered = |term: &str| {
            let pred = driver
                .contains_predicate(&detail.columns, term)
                .expect("a text column is searchable");
            format!("SELECT * FROM (SELECT * FROM {t}) AS _f WHERE ({pred})")
        };
        // Capture references (Copy) so the closure stays `Fn` and `driver` survives
        // for the DROP below.
        let d = &driver;
        let abort = &abort;
        let count = |sql: String| async move { d.count(&sql, abort).await.unwrap() };
        assert_eq!(
            count(filtered("apple")).await,
            2,
            "matches across text columns"
        );
        assert_eq!(count(filtered("APPLE")).await, 2, "case-insensitive");
        assert_eq!(
            count(filtered("%")).await,
            1,
            "LIKE metacharacters match literally"
        );
        assert_eq!(
            count(filtered("O'Brien")).await,
            1,
            "embedded quote is escaped"
        );
        assert_eq!(
            count(filtered("zzznope")).await,
            0,
            "no match → empty result"
        );

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn seeks_forward_backward_and_reads_bounds() {
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, false).await.unwrap();
        let t = tag("seek");
        driver
            .execute(&format!(
                "CREATE TABLE {t} (id Int32, name String) ENGINE = MergeTree ORDER BY id"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {t} SELECT number + 1, concat('row ', toString(number + 1)) \
                 FROM numbers(1000)"
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
                "CREATE TABLE {g} (id Int32, grp Int32) ENGINE = MergeTree ORDER BY id"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {g} SELECT number + 1, (number + 1) % 3 FROM numbers(30)"
            ))
            .await
            .unwrap();
        let key_asc = KeySpec {
            column: "grp".into(),
            kind: KeyKind::Int,
            column_type: None,
            tiebreak: Some("id".into()),
            tiebreak_type: None,
            descending: false,
        };
        let key_desc = KeySpec {
            descending: true,
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
    async fn executes_and_exports() {
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, false).await.unwrap();
        let t = tag("exec");
        driver
            .execute(&format!(
                "CREATE TABLE {t} (id Int32, name Nullable(String)) ENGINE = MergeTree ORDER BY id"
            ))
            .await
            .unwrap();
        let affected = driver
            .execute(&format!("INSERT INTO {t} VALUES (1, 'a,b'), (2, NULL)"))
            .await
            .unwrap();
        assert_eq!(affected, 2, "execute reports rows written");

        battery::exports_csv_and_json(&driver, &format!("SELECT * FROM {t} ORDER BY id"), &t).await;
        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn column_stats_summary() {
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, false).await.unwrap();
        let t = tag("stats");
        driver
            .execute(&format!(
                "CREATE TABLE {t} (id Int32, title String, author_id Nullable(Int32)) \
                 ENGINE = MergeTree ORDER BY id"
            ))
            .await
            .unwrap();
        // author_id is 1,1,2,NULL: NULLs + duplicates, narrowable by `author_id = 1`.
        driver
            .execute(&format!(
                "INSERT INTO {t} VALUES (1, 'a', 1), (2, 'b', 1), (3, 'c', 2), (4, 'd', NULL)"
            ))
            .await
            .unwrap();
        battery::column_stats_summary(
            &driver,
            &format!("SELECT * FROM {t}"),
            "author_id",
            "title",
            "author_id = 1",
        )
        .await;
        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn read_only_rejects_write() {
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, true).await.unwrap();
        battery::read_only_rejects_write(
            &driver,
            "CREATE TABLE red_ro_should_fail (x Int32) ENGINE = MergeTree ORDER BY x",
        )
        .await;
    }

    #[tokio::test]
    async fn explains_a_query() {
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, false).await.unwrap();
        let t = tag("explain");
        driver
            .execute(&format!(
                "CREATE TABLE {t} (id Int32, name String) ENGINE = MergeTree ORDER BY id"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!(
                "INSERT INTO {t} SELECT number, toString(number) FROM numbers(100)"
            ))
            .await
            .unwrap();
        battery::explains_query(&driver, &format!("SELECT * FROM {t}"), &t).await;
        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn caps_display_keeps_key_and_export() {
        // ClickHouse-specific cap check: a fat `String` cell is capped as text (CH has
        // no distinct blob type), the integer key rides through whole, and export stays
        // byte-exact. Mirrors the shared helper minus its blob-column assertion.
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, false).await.unwrap();
        let t = tag("cap");
        driver
            .execute(&format!(
                "CREATE TABLE {t} (id Int32, t String) ENGINE = MergeTree ORDER BY id"
            ))
            .await
            .unwrap();
        driver
            .execute(&format!("INSERT INTO {t} VALUES (1, repeat('a', 5000))"))
            .await
            .unwrap();

        let key = KeySpec::single("id", KeyKind::Int);
        let abort = AbortSignal::new();
        let page = driver
            .fetch_seek(
                &format!("SELECT id, t FROM {t}"),
                &key,
                None,
                false,
                5,
                &abort,
            )
            .await
            .unwrap();
        assert_eq!(page.rows.len(), 1, "fixture has exactly one row");
        assert!(
            matches!(page.rows[0][0], Value::Integer(1)),
            "the key rides through whole"
        );
        match &page.rows[0][1] {
            Value::Capped(c) => {
                assert!(!c.blob, "text capped as text");
                assert_eq!(c.len, 5000, "the true text length is preserved");
                assert!(
                    c.head.len() <= crate::DEFAULT_DISPLAY_CELL_CAP,
                    "head within the cap"
                );
            }
            other => panic!("expected capped text, got {other:?}"),
        }

        // A Full page keeps the whole cell (the clipboard re-fetch).
        let full = driver
            .fetch_page(
                &format!("SELECT id, t FROM {t}"),
                0,
                5,
                PageCap::Full,
                &abort,
            )
            .await
            .unwrap();
        match &full.rows[0][1] {
            Value::Text(s) => assert_eq!(s.len(), 5000, "Full keeps the whole text"),
            other => panic!("expected whole text under Full, got {other:?}"),
        }

        // Export stays byte-exact.
        let dir = std::env::temp_dir();
        let csv_path = dir.join(format!("red_conf_chcap_{t}.csv"));
        let no_cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let drain = tokio::sync::mpsc::unbounded_channel().0;
        driver
            .export(
                &format!("SELECT id, t FROM {t}"),
                &csv_path,
                ExportFormat::Csv,
                no_cancel,
                drain,
            )
            .await
            .unwrap();
        let csv = std::fs::read_to_string(&csv_path).unwrap();
        assert!(
            csv.contains(&"a".repeat(5000)),
            "export carries the full 5000-byte text uncapped"
        );
        std::fs::remove_file(&csv_path).ok();
        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    // Server-free unit test; always runs (no ClickHouse needed).
    #[test]
    fn create_table_sql_builds_mergetree_ddl() {
        let tref = TableRef {
            schema: Some("db".into()),
            name: "t".into(),
        };
        let col = |name: &str, ty: &str, not_null: bool, pk: bool| ColumnMeta {
            name: name.into(),
            type_name: Some(ty.into()),
            not_null,
            primary_key: pk,
            default: None,
            auto_increment: false,
        };

        // A NOT NULL int PK + a nullable text: types spelled via typemap, the nullable
        // column wrapped, the PK as the MergeTree ORDER BY.
        let sql = ch_create_table_sql(
            &tref,
            &[
                col("id", "integer", true, true),
                col("name", "text", false, false),
            ],
        );
        assert_eq!(
            sql,
            "CREATE TABLE IF NOT EXISTS `db`.`t` \
             (`id` Int32, `name` Nullable(String)) ENGINE = MergeTree ORDER BY (`id`)"
        );

        // No primary key → the no-sort-key sentinel `tuple()`.
        let sql = ch_create_table_sql(&tref, &[col("v", "integer", false, false)]);
        assert!(
            sql.ends_with("ENGINE = MergeTree ORDER BY tuple()"),
            "no PK → ORDER BY tuple(): {sql}"
        );

        // A nullable sort-key column needs `allow_nullable_key`.
        let sql = ch_create_table_sql(&tref, &[col("id", "integer", false, true)]);
        assert!(
            sql.contains("ORDER BY (`id`) SETTINGS allow_nullable_key = 1"),
            "nullable PK opts into allow_nullable_key: {sql}"
        );
    }

    #[tokio::test]
    async fn writes_create_insert_read_clear() {
        // ClickHouse as a copy/migration *target*: `create_table` emits MergeTree DDL
        // from cross-engine `ColumnMeta` (types spelled via typemap, nullable columns
        // wrapped `Nullable`, PK → ORDER BY), `insert_rows` streams a native
        // JSONCompactEachRow body, and `clear_table` TRUNCATEs. In-grid UPDATE/DELETE
        // stays unsupported (see `editing_is_unsupported`).
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, false).await.unwrap();
        let db = database(&url);
        let t = tag("writes");
        let tref = TableRef {
            schema: Some(db.clone()),
            name: t.clone(),
        };
        // Source-shaped column metadata (foreign type spellings on purpose, so the
        // typemap path is exercised): a NOT NULL int PK, a nullable text, a nullable
        // float.
        let col = |name: &str, ty: &str, not_null: bool, pk: bool| ColumnMeta {
            name: name.into(),
            type_name: Some(ty.into()),
            not_null,
            primary_key: pk,
            default: None,
            auto_increment: false,
        };
        let columns = vec![
            col("id", "integer", true, true),
            col("name", "text", false, false),
            col("score", "double precision", false, false),
        ];
        driver.create_table(&tref, &columns).await.unwrap();
        // Idempotent: a second create over the same table is a no-op, not an error.
        driver.create_table(&tref, &columns).await.unwrap();

        // The created table carries the PK as its (MergeTree) sort key, and the
        // nullable columns are Nullable.
        let detail = driver.describe_table(&db, &t).await.unwrap();
        let dcol = |n: &str| detail.columns.iter().find(|c| c.name == n).unwrap();
        assert!(dcol("id").primary_key, "id is the MergeTree sort key");
        assert!(dcol("id").not_null, "the PK column is NOT NULL");
        assert!(
            !dcol("name").not_null,
            "a nullable source column stays Nullable"
        );

        // Bulk insert: a plain row, a SQL-metacharacter value (escaped by serde_json,
        // never interpolated), and a NULL name.
        let insert_cols = vec![
            Column {
                name: "id".into(),
                decl_type: None,
            },
            Column {
                name: "name".into(),
                decl_type: None,
            },
            Column {
                name: "score".into(),
                decl_type: None,
            },
        ];
        let evil = "'); DROP TABLE x;--";
        let rows = vec![
            vec![
                Value::Integer(1),
                Value::Text("one".into()),
                Value::Real(1.5),
            ],
            vec![Value::Integer(2), Value::Text(evil.into()), Value::Null],
            vec![Value::Integer(3), Value::Null, Value::Real(3.25)],
        ];
        let n = driver
            .insert_rows(&tref, &insert_cols, &rows)
            .await
            .unwrap();
        assert_eq!(n, 3, "insert_rows reports the rows inserted");

        // An empty chunk is a no-op returning 0, without a round-trip.
        assert_eq!(
            driver.insert_rows(&tref, &insert_cols, &[]).await.unwrap(),
            0
        );

        let abort = AbortSignal::new();
        let all = format!("SELECT id, name, score FROM {t} ORDER BY id");
        assert_eq!(
            driver.count(&all, &abort).await.unwrap(),
            3,
            "all rows landed"
        );
        let page = driver
            .fetch_page(&all, 0, 10, PageCap::Full, &abort)
            .await
            .unwrap();
        assert_eq!(
            page.rows[1][1],
            Value::Text(evil.into()),
            "value stored verbatim: escaped by serde_json, not interpolated"
        );
        assert_eq!(page.rows[2][1], Value::Null, "NULL inserted as NULL");

        // `clear_table` empties the table (TRUNCATE); the rows are gone.
        driver.clear_table(&tref).await.unwrap();
        assert_eq!(
            driver.count(&all, &abort).await.unwrap(),
            0,
            "clear_table (TRUNCATE) emptied the table"
        );

        driver.execute(&format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn read_only_rejects_writes() {
        // Defense in depth: a read-only ClickHouse connection refuses every write
        // seam at the engine (`readonly=1`), even though the UI already gates them.
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, true).await.unwrap();
        let tref = TableRef {
            schema: Some(database(&url)),
            name: "red_ro_writes".into(),
        };
        let columns = vec![ColumnMeta {
            name: "id".into(),
            type_name: Some("integer".into()),
            not_null: true,
            primary_key: true,
            default: None,
            auto_increment: false,
        }];
        assert!(
            driver.create_table(&tref, &columns).await.is_err(),
            "read-only rejects create_table"
        );
        let cols = vec![Column {
            name: "id".into(),
            decl_type: None,
        }];
        assert!(
            driver
                .insert_rows(&tref, &cols, &[vec![Value::Integer(1)]])
                .await
                .is_err(),
            "read-only rejects insert_rows"
        );
        assert!(
            driver.clear_table(&tref).await.is_err(),
            "read-only rejects clear_table"
        );
        // An empty insert chunk is still a short-circuit no-op (no engine round-trip).
        assert_eq!(driver.insert_rows(&tref, &cols, &[]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn editing_is_unsupported() {
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, false).await.unwrap();
        // A non-empty edit batch is refused (OLAP has no transactional in-grid edit);
        // an empty batch is a no-op returning 0.
        let op = EditOp::Delete {
            table: red_core::TableRef {
                schema: Some(database(&url)),
                name: "whatever".into(),
            },
            key: red_core::ColumnValue {
                column: "id".into(),
                value: Value::Integer(1),
                decl_type: None,
            },
        };
        assert!(driver.apply_edit(&op).await.is_err(), "edits are refused");
        assert_eq!(
            driver.apply_edits(&[]).await.unwrap(),
            0,
            "empty batch is a no-op"
        );
    }
}
