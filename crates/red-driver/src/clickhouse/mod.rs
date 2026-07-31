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
//! `TRUNCATE`s. The grid's **draft-row insert** rides the same path:
//! [`apply_edits`](ClickhouseDriver::apply_edits) folds an all-`Insert` batch into
//! bulk inserts. In-grid **update / delete** stay unsupported *on that seam*:
//! ClickHouse `UPDATE`/`DELETE` are asynchronous `ALTER TABLE … UPDATE` mutations with
//! no transaction or rollback over a non-unique sort key, so the trait's "batch in one
//! transaction, assert exactly one row, roll back on failure" contract cannot be
//! honored; `apply_edits` returns a typed error (a best-effort mutation mode is a later
//! phase). Secondary indexes and foreign keys have no OLAP equivalent, so those
//! migration passes are logged skips.
//!
//! Value mapping leans on the engine: the `JSON…` formats render every type to JSON
//! text for us, so a cell is a "JSON scalar/array → [`Value`]" map; no hand-written
//! binary decoder. Integers come back as JSON numbers (or quoted strings for the
//! 64-bit widths, which JSON can't hold losslessly); composites (`Array`, `Tuple`,
//! `Map`) and the date/decimal/uuid/enum shapes render as text.

use std::fs::{File, remove_file};
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use red_core::{
    Column, ColumnMeta, ColumnPredicate, ColumnValue, ConnectionConfig, DbKind, EditOp,
    ExportFormat, ExportOutcome, FkEdge, KeySpec, ObjectKind, ObjectMeta, QueryOptions, QueryPlan,
    RedError, Result, ResultPage, RowEditCaps, RowWindow, SchemaMeta, TableDetail, TableRef, Value,
};
use std::borrow::Cow;

use serde_json::Value as Json;
use serde_json::value::RawValue;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Mutex, OnceCell};
use uuid::Uuid;

use crate::format::{ExportWriter, ProgressThrottle, strip_trailing};
use crate::{
    AbortSignal, CancelToken, CellCap, DatabaseDriver, PageCap, QueryCursor, driver_err, now_unix,
    secs_to_duration, window_prealloc,
};

mod write;

/// A collected (bounded) read: the result columns, their raw ClickHouse type
/// strings (for value mapping), and the raw JSON cell rows.
type RowBlock = (Vec<Column>, Vec<String>, Vec<Vec<Cell>>);

/// One JSON cell exactly as ClickHouse serialised it, kept as raw source text
/// rather than a parsed [`Json`].
///
/// Parsing loses data this driver has to keep. `serde_json` without
/// `arbitrary_precision` widens any number that does not fit `i64`/`u64` into
/// `f64`, and ClickHouse leaves `Decimal` unquoted
/// (`output_format_json_quote_decimals` defaults to 0), so a
/// `Decimal128(30, 10)` arrived as a bare JSON number, became a double, and was
/// re-rendered *from the double* — wrong digits in the grid and in every export,
/// with nothing to indicate it. Asking the server to quote decimals instead would
/// mean changing a setting per request, which a ClickHouse user whose profile is
/// `readonly = 1` is not allowed to do.
///
/// The accessors below mirror the [`Json`] ones the metadata queries used, with
/// the "…or a quoted number" fallback folded in: which of the two forms a counter
/// arrives in depends on server settings, and every caller wanted both.
#[derive(Debug, Clone)]
pub(super) struct Cell(Box<RawValue>);

impl Cell {
    /// The cell's raw JSON source (`"abc"` with its quotes, `1.5` verbatim).
    fn raw(&self) -> &str {
        self.0.get()
    }

    /// The content of a JSON string cell, unescaped. `None` for every other shape,
    /// matching `serde_json::Value::as_str`.
    fn as_str(&self) -> Option<Cow<'_, str>> {
        let raw = self.raw();
        let body = raw.strip_prefix('"')?.strip_suffix('"')?;
        if !body.contains('\\') {
            return Some(Cow::Borrowed(body));
        }
        serde_json::from_str::<String>(raw).ok().map(Cow::Owned)
    }

    /// The cell as `i64`: a bare JSON integer, or one that arrived quoted.
    fn as_i64(&self) -> Option<i64> {
        match self.as_str() {
            Some(s) => s.parse().ok(),
            None => self.raw().parse().ok(),
        }
    }

    /// The cell as `f64`: a bare JSON number, or one that arrived quoted.
    fn as_f64(&self) -> Option<f64> {
        match self.as_str() {
            Some(s) => s.parse().ok(),
            None => self.raw().parse().ok(),
        }
    }
}

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
    /// Overrides [`Self::database`] for this handle's requests when set (see
    /// [`scoped`](DatabaseDriver::scoped)). ClickHouse names the database per
    /// request rather than per session, so rebinding costs nothing at all here —
    /// no extra round-trip, just a different header value.
    ///
    /// Distinct from `scope`, which only filters the schema *tree*.
    namespace: Option<String>,
    read_only: bool,
    version: String,
    /// When set, the schema tree is restricted to this one database (the
    /// connection's chosen `database`); `None` lists every non-system database.
    scope: Option<String>,
    /// Which mutation spellings this server has, probed lazily on the first write
    /// (see [`write::features_from`]). Shared across [`scoped`](DatabaseDriver::scoped)
    /// handles so rebinding the database doesn't re-probe, and never touched at all
    /// by a read-only session.
    features: Arc<OnceCell<write::ChFeatures>>,
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
            namespace: None,
            read_only,
            version: String::new(),
            scope: None,
            features: Arc::new(OnceCell::new()),
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
            .header(
                "X-ClickHouse-Database",
                self.namespace.as_deref().unwrap_or(&self.database),
            )
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

    /// The `system.columns` + `system.tables` facts that decide what a table will let
    /// the grid write (see [`write::ChTableFacts`]). One round trip; shared by
    /// [`edit_caps`](DatabaseDriver::edit_caps) and the mutation path, which needs the
    /// same answers plus the engine name.
    async fn table_facts(&self, schema: &str, table: &str) -> Result<write::ChTableFacts> {
        let base = "SELECT c.name, c.type, c.default_kind, c.is_in_sorting_key, \
             c.is_in_primary_key, c.is_in_partition_key, c.is_in_sampling_key, t.engine \
             FROM system.columns AS c INNER JOIN system.tables AS t \
             ON t.database = c.database AND t.name = c.table \
             WHERE c.database = {db:String} AND c.table = {tbl:String} ORDER BY c.position"
            .to_string();
        let (_, _, rows) = self.run_simple(base, &table_params(schema, table)).await?;
        let flag = |row: &Vec<Cell>, i: usize| cell_num(row, i) == 1;
        let mut engine = String::new();
        let columns: Vec<write::ChColumn> = rows
            .iter()
            .map(|row| {
                engine = cell_text(row, 7);
                write::ChColumn {
                    name: cell_text(row, 0),
                    type_name: cell_text(row, 1),
                    default_kind: cell_text(row, 2),
                    in_sorting_key: flag(row, 3),
                    in_primary_key: flag(row, 4),
                    in_partition_key: flag(row, 5),
                    in_sampling_key: flag(row, 6),
                }
            })
            .collect();
        if columns.is_empty() {
            return Err(RedError::Query(format!(
                "{schema}.{table} has no readable columns"
            )));
        }
        Ok(write::ChTableFacts { engine, columns })
    }

    /// What mutation spellings this server has, probed once and cached (see the
    /// `features` field). Probing rather than assuming is the point: the lightweight
    /// DML forms and their sync settings arrived across several releases, and sending
    /// a setting the server doesn't know is an error rather than a no-op.
    ///
    /// A failed probe answers "nothing is available", which is the conservative
    /// reading: the always-present `ALTER …` form with no sync setting attached, and
    /// therefore an outcome reported as *submitted* rather than applied.
    async fn write_features(&self) -> write::ChFeatures {
        *self
            .features
            .get_or_init(|| async {
                let names: Vec<String> = (0..write::PROBE_SETTINGS.len())
                    .map(|i| format!("{{s{i}:String}}"))
                    .collect();
                let sql = format!(
                    "SELECT name, value FROM system.settings WHERE name IN ({})",
                    names.join(", ")
                );
                let params: Vec<(String, String)> = write::PROBE_SETTINGS
                    .iter()
                    .enumerate()
                    .map(|(i, name)| (format!("param_s{i}"), (*name).to_string()))
                    .collect();
                let settings = match self.run_simple(sql, &params).await {
                    Ok((_, _, rows)) => rows
                        .iter()
                        .map(|row| {
                            let cell = |i: usize| {
                                row.get(i)
                                    .and_then(Cell::as_str)
                                    .unwrap_or_default()
                                    .to_string()
                            };
                            (cell(0), cell(1))
                        })
                        .collect::<Vec<_>>(),
                    Err(e) => {
                        tracing::warn!("clickhouse write-feature probe failed: {e}");
                        Vec::new()
                    }
                };
                write::features_from(&self.version, &settings)
            })
            .await
    }

    /// POST one mutation and classify the reply. A wait that times out is **not** a
    /// failure: `mutations_sync` only bounds how long we watch, and the mutation the
    /// server accepted keeps running, so it is reported as still-running rather than
    /// as an error the user would be tempted to retry into a second part rewrite.
    async fn run_mutation(&self, sql: String, params: Vec<(String, String)>) -> MutationReply {
        let qid = new_query_id();
        let resp = match self.build_query(sql, &qid, &params).send().await {
            Ok(resp) => resp,
            Err(e) => return MutationReply::Failed(e.to_string()),
        };
        let status = resp.status();
        let body = match resp.bytes().await {
            Ok(body) => body,
            Err(e) => return MutationReply::Failed(e.to_string()),
        };
        if status.is_success() {
            return MutationReply::Done;
        }
        let text = String::from_utf8_lossy(&body).to_string();
        if write::is_timeout_error(&text) {
            MutationReply::StillRunning
        } else {
            MutationReply::Failed(clean_error(&body))
        }
    }

    /// Everything decided about one op before anything is written: the statement it
    /// renders to, how many rows its identity matches, or why it can't run.
    /// Read the write-relevant catalog facts for every table `ops` touches, once
    /// each. A submit is usually many rows of one table; without this, a 50-row batch
    /// would re-read the same `system.columns` join 50 times. Scoped to the one call,
    /// so it can never serve a stale answer after a concurrent `ALTER`.
    async fn facts_for(&self, ops: &[EditOp]) -> FactsCache {
        let mut cache: FactsCache = std::collections::HashMap::new();
        for op in ops {
            let key = (self.schema_of(op.table()), op.table().name.clone());
            if cache.contains_key(&key) {
                continue;
            }
            let facts = self
                .table_facts(&key.0, &key.1)
                .await
                .map_err(|e| e.to_string());
            cache.insert(key, facts);
        }
        cache
    }

    /// The database an op's table lives in: its own when qualified, else whatever
    /// this handle is bound to.
    fn schema_of(&self, table: &TableRef) -> String {
        match table.schema.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None => self
                .namespace
                .clone()
                .unwrap_or_else(|| self.database.clone()),
        }
    }

    async fn preflight_one(
        &self,
        op: &EditOp,
        allow_multi_match: bool,
        facts: &FactsCache,
    ) -> Preflight {
        let blocked = |reason: String| Preflight {
            display: op.preview_sql(),
            blocked: Some(reason),
            ..Preflight::default()
        };
        // An insert addresses no existing row, so none of the identity machinery
        // applies: it rides the bulk insert path, unpreflighted.
        if matches!(op, EditOp::Insert { .. }) {
            return Preflight {
                display: op.preview_sql(),
                ..Preflight::default()
            };
        }
        let table = op.table();
        let schema = self.schema_of(table);
        let facts = match facts.get(&(schema.clone(), table.name.clone())) {
            Some(Ok(facts)) => facts,
            Some(Err(e)) => return blocked(e.clone()),
            None => return blocked(format!("no catalog entry for {}", table.name)),
        };
        let caps = facts.edit_caps();
        if !caps.editable() {
            return blocked(
                caps.note
                    .unwrap_or_else(|| format!("{} is not editable", table.name)),
            );
        }
        // A key or engine-computed column is refused *here*, so the user is told
        // before confirming rather than by the engine after.
        if let EditOp::Update { set, .. } = op
            && let Some(cv) = set
                .iter()
                .find(|cv| caps.no_update.iter().any(|n| n == &cv.column))
        {
            return blocked(format!(
                "{} can't be updated: it's part of the table's key or computed by the engine",
                cv.column
            ));
        }
        let replicated = facts.engine.starts_with("Replicated");
        let form = self.write_features().await.form(op.verb());
        let qualified = crate::qualify_table(
            &TableRef {
                schema: Some(schema),
                name: table.name.clone(),
            },
            ch_quote,
        );
        let rendered = match write::render_op(&qualified, op, form) {
            Ok(rendered) => rendered,
            Err(e) => return blocked(e.to_string()),
        };
        // The identity count is the whole guarantee on an engine with no unique row
        // address, so it runs before every write, not just the first.
        let identity_params: Vec<(String, String)> = rendered
            .params
            .iter()
            .filter(|(name, _)| name.starts_with("param_i"))
            .cloned()
            .collect();
        let matched = match self
            .run_collect(
                rendered.count_sql.clone(),
                &identity_params,
                &AbortSignal::new(),
            )
            .await
        {
            Ok((_, _, rows)) => rows
                .first()
                .and_then(|r| r.first())
                .and_then(Cell::as_i64)
                .unwrap_or(0)
                .max(0) as u64,
            Err(e) => return blocked(format!("couldn't check which rows this matches: {e}")),
        };
        let display = rendered.display.clone();
        let refusal = match matched {
            0 => Some(
                "no longer matches any row: it changed or was removed since this result \
                 loaded. Refresh and try again."
                    .to_string(),
            ),
            1 => None,
            n if allow_multi_match => {
                tracing::info!(
                    rows = n,
                    "clickhouse edit applies to several rows by consent"
                );
                None
            }
            n => Some(format!(
                "matches {n} rows, not one: ClickHouse has no unique row identity, so \
                 this would change all of them. Confirm applying it to all {n}, or add \
                 columns that tell them apart."
            )),
        };
        Preflight {
            rendered: Some(rendered),
            display,
            matches: Some(matched),
            blocked: refusal,
            replicated,
            form,
        }
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
                    ));
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

    /// Rebind the database this handle's requests name. ClickHouse carries the
    /// database as a per-request header, so this is free: no round-trip, no
    /// session state, nothing to reset. Shares the `reqwest::Client` (internally
    /// refcounted), so the handle is a field copy.
    fn scoped(self: Arc<Self>, namespace: Option<&str>) -> Arc<dyn DatabaseDriver> {
        let requested = namespace.filter(|n| !n.is_empty()).map(str::to_owned);
        if requested == self.namespace {
            return self;
        }
        Arc::new(Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            user: self.user.clone(),
            password: self.password.clone(),
            database: self.database.clone(),
            namespace: requested,
            read_only: self.read_only,
            version: self.version.clone(),
            scope: self.scope.clone(),
            features: self.features.clone(),
        })
    }

    async fn open_cursor(&self, sql: &str, opts: QueryOptions) -> Result<Box<dyn QueryCursor>> {
        let query_id = new_query_id();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = self.cursor_cancel_token(&query_id, cancelled.clone());
        let (columns, types, resp, buf) = self.open_stream(&strip_trailing(sql), &query_id).await?;
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
                .and_then(Cell::as_str)
                .unwrap_or_default()
                .to_string();
            let name = row
                .get(1)
                .and_then(Cell::as_str)
                .unwrap_or_default()
                .to_string();
            let engine = row.get(2).and_then(Cell::as_str).unwrap_or_default();
            // A MaterializedView is a stored, incrementally-populated relation and
            // a plain View is a query rewrite; they behave differently enough (one
            // has parts and a size, the other does not) to draw apart.
            let kind = match engine.as_ref() {
                "MaterializedView" => ObjectKind::MaterializedView,
                e if e.ends_with("View") => ObjectKind::View,
                _ => ObjectKind::Table,
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

    async fn object_group_counts(&self) -> Result<Vec<(String, ObjectKind, usize)>> {
        // ClickHouse UDFs are server-wide, not per database, so the same count is
        // reported for every database. That mirrors `list_object_group`, which
        // returns the same list under whichever database is asked. The wart is
        // that the tree has no server-level node to hang them off; reporting them
        // per database at least keeps the count and the contents agreeing.
        let (_, _, rows) = self
            .run_simple(
                "SELECT d.name, (SELECT count() FROM system.functions \
                                  WHERE origin = 'SQLUserDefined') \
                   FROM system.databases d \
                  WHERE d.name NOT IN ('system', 'information_schema', 'INFORMATION_SCHEMA')"
                    .to_string(),
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let name = row.first().and_then(Cell::as_str)?;
                let count = row.get(1).and_then(Cell::as_i64).unwrap_or(0);
                Some((
                    name.to_string(),
                    ObjectKind::Function,
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
        // Only user-defined functions are lazy here: ClickHouse has no triggers,
        // sequences, or user types, and its materialized views ride the skeleton
        // as relations. `origin = 'SQLUserDefined'` excludes the ~1500 built-ins,
        // which are engine surface, not this database's objects.
        //
        // `system.functions` is server-wide, not per database, so the namespace is
        // accepted and ignored rather than filtered on a column that is not there.
        if kind != ObjectKind::Function {
            return Ok(Vec::new());
        }
        let _ = namespace;
        let (_, _, rows) = self
            .run_simple(
                "SELECT name FROM system.functions WHERE origin = 'SQLUserDefined' ORDER BY name"
                    .to_string(),
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| row.first().and_then(Cell::as_str))
            .map(|name| ObjectMeta {
                name: name.to_string(),
                kind: ObjectKind::Function,
            })
            .collect())
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableDetail> {
        // Columns from `system.columns`. A column is NOT NULL unless its type is
        // `Nullable(…)`; primary-key membership is `is_in_primary_key` (the MergeTree
        // ORDER BY / PRIMARY KEY). ClickHouse is OLAP: there are no foreign keys and
        // no secondary indexes in the relational sense, so both vecs stay empty.
        let base = "SELECT name, type, is_in_primary_key, default_expression FROM system.columns \
             WHERE database = {db:String} AND table = {tbl:String} ORDER BY position"
            .to_string();
        let (_, _, rows) = self.run_simple(base, &table_params(schema, table)).await?;
        let columns = rows
            .iter()
            .map(|row| {
                let name = row
                    .first()
                    .and_then(Cell::as_str)
                    .unwrap_or_default()
                    .to_string();
                let type_name = row
                    .get(1)
                    .and_then(Cell::as_str)
                    .unwrap_or_default()
                    .to_string();
                let in_pk = row.get(2).and_then(Cell::as_i64).unwrap_or(0) == 1;
                // `default_expression` is empty for a column without one; keep the
                // schema tree's "no default" as `None` rather than an empty string.
                let default = row
                    .get(3)
                    .and_then(Cell::as_str)
                    .filter(|s| !s.is_empty())
                    .map(Cow::into_owned);
                ColumnMeta {
                    not_null: !type_name.starts_with("Nullable("),
                    primary_key: in_pk,
                    type_name: Some(type_name),
                    default,
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

    /// One catalog round trip joining `system.columns` to `system.tables`, because
    /// nothing that decides whether a ClickHouse row is editable survives into a
    /// [`TableDetail`]: the table engine, sorting / partition / sampling key
    /// membership, and whether a column is `MATERIALIZED`/`ALIAS`. `detail` is
    /// therefore unused here.
    ///
    /// A probe failure is **not** an error: it degrades to "not editable, and here is
    /// why", so a catalog the user can't read (a restricted `system` grant) hides the
    /// affordance instead of breaking the browse.
    async fn edit_caps(
        &self,
        schema: &str,
        table: &str,
        _detail: &TableDetail,
    ) -> Result<RowEditCaps> {
        match self.table_facts(schema, table).await {
            Ok(facts) => Ok(facts.edit_caps()),
            Err(e) => {
                tracing::warn!(%schema, %table, "clickhouse edit_caps probe failed: {e}");
                Ok(RowEditCaps {
                    note: Some(format!("couldn't read the catalog for {table}: {e}")),
                    ..RowEditCaps::default()
                })
            }
        }
    }

    async fn preflight_edits(&self, ops: &[EditOp]) -> Result<Vec<red_core::OpPlan>> {
        let facts = self.facts_for(ops).await;
        let mut out = Vec::with_capacity(ops.len());
        for (index, op) in ops.iter().enumerate() {
            // The plan is what the *unacknowledged* submit would do, so a
            // several-row match shows up as a refusal the dialog can offer to
            // override -- that offer is the acknowledgement.
            let pre = self.preflight_one(op, false, &facts).await;
            out.push(red_core::OpPlan {
                index,
                verb: op.verb(),
                sql: pre.display,
                matches: pre.matches,
                blocked: pre.blocked,
            });
        }
        Ok(out)
    }

    /// ClickHouse's real edit path. Every op preflights its identity count, then
    /// runs on its own: there is no transaction to roll the batch back with, so
    /// stopping at the first failure would leave the user with neither the change nor
    /// a report of what did land. One outcome per op, always.
    async fn apply_edits_best_effort(
        &self,
        ops: &[EditOp],
        mode: red_core::BatchMode,
    ) -> Result<Vec<red_core::OpOutcome>> {
        use red_core::{OpOutcome, OpStatus};
        crate::refuse_if_read_only(self.read_only)?;
        let allow_multi_match = matches!(
            mode,
            red_core::BatchMode::BestEffort {
                allow_multi_match: true
            }
        );
        let features = self.write_features().await;
        let facts = self.facts_for(ops).await;
        let mut out = Vec::with_capacity(ops.len());
        for (index, op) in ops.iter().enumerate() {
            let status = match op {
                // Inserts need no identity and no mutation: straight to the bulk path.
                EditOp::Insert { table, values } => {
                    let columns: Vec<Column> = values
                        .iter()
                        .map(|cv| Column {
                            name: cv.column.clone(),
                            decl_type: cv.decl_type.clone(),
                        })
                        .collect();
                    let row: Vec<Value> = values.iter().map(|cv| cv.value.clone()).collect();
                    match self.insert_rows(table, &columns, &[row]).await {
                        Ok(affected) => OpStatus::Applied { affected },
                        Err(e) => OpStatus::Failed(e.to_string()),
                    }
                }
                _ => {
                    let pre = self.preflight_one(op, allow_multi_match, &facts).await;
                    match (pre.blocked, pre.rendered) {
                        (Some(reason), _) => OpStatus::Blocked(reason),
                        (None, None) => OpStatus::Blocked(
                            "this edit couldn't be rendered as a statement".to_string(),
                        ),
                        (None, Some(rendered)) => {
                            let sync = features.sync_settings(pre.form, pre.replicated);
                            // Without a sync setting the server returns before the
                            // mutation is visible, so "applied" would be a claim we
                            // can't back. Say submitted instead.
                            let waited = !sync.is_empty();
                            let mut params = rendered.params;
                            params.extend(sync);
                            match self.run_mutation(rendered.sql, params).await {
                                MutationReply::Done if waited => OpStatus::Applied {
                                    affected: pre.matches.unwrap_or(0),
                                },
                                MutationReply::Done | MutationReply::StillRunning => {
                                    OpStatus::Submitted
                                }
                                MutationReply::Failed(message) => OpStatus::Failed(message),
                            }
                        }
                    }
                }
            };
            out.push(OpOutcome {
                index,
                verb: op.verb(),
                status,
            });
        }
        Ok(out)
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

    fn cmp_predicate(&self, preds: &[ColumnPredicate]) -> String {
        // Knobs match `contains_predicate` above; ClickHouse has no `ESCAPE`
        // clause, so it omits one here too.
        crate::cmp_clause(
            preds,
            ch_quote,
            |c| format!("CAST({c} AS String)"),
            "ILIKE",
            true,
            false,
        )
    }

    async fn count(&self, sql: &str, abort: &AbortSignal) -> Result<i64> {
        let base = format!("SELECT count() FROM ({}) AS _red", strip_trailing(sql));
        let (_, _, rows) = self.run_collect(base, &[], abort).await?;
        Ok(rows
            .first()
            .and_then(|r| r.first())
            .and_then(Cell::as_i64)
            .unwrap_or(0))
    }

    async fn column_stats(
        &self,
        sql: &str,
        column: &str,
        flags: red_core::StatsFlags,
        abort: &AbortSignal,
    ) -> Result<red_core::ColumnStats> {
        // OLAP loves aggregates; this is a plain read, like every other ClickHouse path.
        let base = crate::stats_sql(sql, column, flags, ch_quote);
        let (_, types, rows) = self.run_collect(base, &[], abort).await?;
        // One aggregate row, decoded by the response's column types then read
        // positionally.
        let cells = rows
            .first()
            .map(|r| ch_row(r, &types, None))
            .unwrap_or_default();
        Ok(crate::parse_stats(&cells, flags))
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
            // Drain the raw JSON rows as display rows are built, so the parsed
            // `Vec<Vec<Json>>` frees incrementally instead of coexisting whole with
            // the output `Vec<Vec<Value>>` (a page's worth of double residency).
            rows: rows.into_iter().map(|r| ch_row(&r, &types, cap)).collect(),
            columns,
        })
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
        let bound = bound.unwrap_or(&[]);
        let types = bound
            .iter()
            .map(ch_param_type)
            .collect::<Result<Vec<_>>>()?;
        // Typed placeholders `{p0:Int64}` keep the bound a real parameter (bound via
        // `param_p0` URL params), never string-interpolated into the SQL.
        let (where_clause, order_by) =
            crate::seek_clauses(key, bound.len(), scroll, false, ch_quote, |i| {
                format!("{{p{i}:{}}}", types[i])
            });
        let base = format!(
            "SELECT * FROM ({}) AS _red {where_clause}ORDER BY {order_by} LIMIT {limit}",
            strip_trailing(sql)
        );
        let (columns, ctypes, rows) = self.run_collect(base, &ch_params(bound), abort).await?;
        let cap = CellCap::display(crate::key_positions(key, &columns));
        Ok(ResultPage {
            rows: rows.into_iter().map(|r| ch_row(&r, &ctypes, cap)).collect(),
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
        let (where_clause, order_by) = crate::seek_clauses(
            key,
            from.len(),
            red_core::SortDirection::Asc,
            true,
            ch_quote,
            |i| format!("{{p{i}:{}}}", types[i]),
        );
        let base = format!(
            "SELECT * FROM ({}) AS _red {where_clause}ORDER BY {order_by} LIMIT {limit} OFFSET {skip}",
            strip_trailing(sql)
        );
        let (columns, ctypes, rows) = self.run_collect(base, &ch_params(from), abort).await?;
        let cap = CellCap::display(crate::key_positions(key, &columns));
        Ok(ResultPage {
            rows: rows.into_iter().map(|r| ch_row(&r, &ctypes, cap)).collect(),
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
                r.first().and_then(Cell::as_i64),
                r.get(1).and_then(Cell::as_i64),
            ) {
                (Some(lo), Some(hi)) => Some((lo, hi)),
                _ => None,
            }
        }))
    }

    async fn execute_abort(&self, sql: &str, abort: &AbortSignal) -> Result<u64> {
        // DDL / INSERT from the SQL editor. A read-only connection carries
        // `readonly=1`, so the engine refuses the write (defense in depth). On a
        // writable connection, `wait_end_of_query=1` makes ClickHouse finish before
        // responding so the `X-ClickHouse-Summary` (carrying `written_rows`) is known
        // at the response head rather than only as a streamed trailer.
        let qid = new_query_id();
        // `KILL QUERY WHERE query_id` armed for the write's duration, same as
        // the fetch paths: a long mutation is stoppable at the engine.
        let _guard = abort.arm(self.kill_token(&qid));
        if abort.is_aborted() {
            return Err(RedError::Interrupted);
        }
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

    async fn execute_batch_abort(
        &self,
        statements: &[String],
        abort: &AbortSignal,
    ) -> Result<Vec<u64>> {
        // ClickHouse has no multi-statement transactions, so this is NOT atomic: the
        // statements run in order and a failure leaves earlier ones applied (the error
        // stops the rest). Acceptable because ClickHouse is a rarely-written OLAP
        // target here; the SQL engines above wrap the same call in a real transaction.
        let mut affected = Vec::with_capacity(statements.len());
        for sql in statements {
            affected.push(self.execute_abort(sql, abort).await?);
        }
        Ok(affected)
    }

    /// An **insert-only** batch is honored here by folding into the native bulk
    /// insert; `UPDATE`/`DELETE` are refused (see the module docs). Unlike the
    /// relational engines this is *not* atomic across the batch: ClickHouse has no
    /// multi-statement transaction, so each column-signature group is its own
    /// `INSERT` and an error leaves earlier groups applied. For a pure insert batch
    /// that is the normal ClickHouse posture -- an insert needs none of the
    /// guarantees an OLAP engine cannot give -- and it is what makes the grid's
    /// draft-row zone usable here.
    async fn apply_edits(&self, ops: &[EditOp]) -> Result<u64> {
        // An empty batch is a no-op (matching the trait contract) so a stray submit
        // doesn't error.
        if ops.is_empty() {
            return Ok(0);
        }
        let mut total = 0u64;
        for group in insert_groups(ops)? {
            total += self
                .insert_rows(&group.table, &group.columns, &group.rows)
                .await?;
        }
        Ok(total)
    }

    async fn mutations(&self) -> Result<Vec<red_core::MutationInfo>> {
        // Unfinished first, then newest: what is still running is what the panel
        // exists for, and a finished mutation is only useful as recent history.
        // Bounded, because `system.mutations` keeps completed entries around and a
        // long-lived server accumulates them.
        let base = "SELECT database, table, mutation_id, command, toString(create_time), \
             parts_to_do, is_done, latest_fail_reason FROM system.mutations \
             ORDER BY is_done ASC, create_time DESC LIMIT 200"
            .to_string();
        let (_, _, rows) = self.run_simple(base, &[]).await?;
        Ok(rows
            .iter()
            .map(|row| red_core::MutationInfo {
                database: cell_text(row, 0),
                table: cell_text(row, 1),
                id: cell_text(row, 2),
                command: cell_text(row, 3),
                created: cell_text(row, 4),
                parts_to_do: row.get(5).and_then(Cell::as_i64).unwrap_or(0),
                done: row.get(6).and_then(Cell::as_i64).unwrap_or(0) == 1,
                fail_reason: Some(cell_text(row, 7)).filter(|s| !s.is_empty()),
            })
            .collect())
    }

    async fn kill_mutation(&self, table: &TableRef, id: &str) -> Result<()> {
        crate::refuse_if_read_only(self.read_only)?;
        let schema = match table.schema.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None => self
                .namespace
                .clone()
                .unwrap_or_else(|| self.database.clone()),
        };
        // Bound, not interpolated: a mutation id and a table name both come from the
        // engine's own catalog, but they still reach the statement as parameters.
        let sql = "KILL MUTATION WHERE database = {db:String} AND table = {tbl:String} \
             AND mutation_id = {mid:String}"
            .to_string();
        let mut params = table_params(&schema, &table.name);
        params.push(("param_mid".to_string(), id.to_string()));
        let qid = new_query_id();
        let resp = self
            .build_query(sql, &qid, &params)
            .send()
            .await
            .map_err(driver_err)?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(driver_err)?;
        if status.is_success() {
            Ok(())
        } else {
            Err(ch_error(&body))
        }
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

    fn diff_order_clause(&self, key: &str, _key_is_text: bool) -> String {
        // ClickHouse String ordering is already byte order, but a Nullable key
        // sorts NULLS LAST by default while the merge-walk ranks NULLs first.
        format!("{} ASC NULLS FIRST", ch_quote(key))
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

    async fn health(&self, namespace: Option<&str>) -> Result<red_core::health::HealthReport> {
        use crate::now_unix;
        use red_core::health::{
            Finding, FindingKind, HealthReport, Severity, SizeTotals, TableSize, UnavailableCheck,
        };

        let scope = namespace.map(str::to_string).or_else(|| self.scope.clone());
        let mut report = HealthReport::new(red_core::DbKind::Clickhouse, scope.clone(), now_unix());

        // Sizes come from `system.parts`, which is the only place they exist:
        // ClickHouse stores a table as parts on disk and the compressed size is
        // what "how big is this" means here.
        let (_, _, rows) = self
            .run_simple(
                "SELECT `database`, `table`, sum(bytes_on_disk), sum(rows), count() \
                 FROM system.parts WHERE active \
                 GROUP BY `database`, `table` ORDER BY sum(bytes_on_disk) DESC LIMIT 100"
                    .to_string(),
                &[],
            )
            .await?;
        let mut totals = SizeTotals::default();
        for row in &rows {
            let db = row.first().and_then(Cell::as_str).unwrap_or_default();
            let name = row.get(1).and_then(Cell::as_str).unwrap_or_default();
            if scope.as_deref().is_some_and(|s| s != db) {
                continue;
            }
            let bytes = cell_num(row, 2).max(0) as u64;
            totals.bytes += bytes;
            totals.table_count += 1;
            report.tables.push(TableSize {
                table: TableRef {
                    schema: Some(db.to_string()),
                    name: name.to_string(),
                },
                bytes,
                // Parts carry no separate index size: the sparse primary index is
                // a rounding error next to the data and is not reported apart.
                index_bytes: 0,
                estimated_rows: cell_num(row, 3),
            });
        }
        report.totals = totals;

        // Too many parts in one partition is *the* ClickHouse foot-gun: it is what
        // small, frequent inserts produce, and it degrades reads until merges catch
        // up (or the server starts refusing inserts outright).
        let (_, _, parts) = self
            .run_simple(
                "SELECT `database`, `table`, partition, count() AS n FROM system.parts \
                 WHERE active GROUP BY `database`, `table`, partition \
                 HAVING n > 300 ORDER BY n DESC LIMIT 25"
                    .to_string(),
                &[],
            )
            .await
            .unwrap_or_default();
        for row in &parts {
            let db = row.first().and_then(Cell::as_str).unwrap_or_default();
            let name = row.get(1).and_then(Cell::as_str).unwrap_or_default();
            if scope.as_deref().is_some_and(|s| s != db) {
                continue;
            }
            let partition = row.get(2).and_then(Cell::as_str).unwrap_or_default();
            let n = cell_num(row, 3);
            report.findings.push(Finding {
                severity: if n > 1000 {
                    Severity::Bad
                } else {
                    Severity::Warn
                },
                kind: FindingKind::TooManyParts,
                object: Some(TableRef {
                    schema: Some(db.to_string()),
                    name: name.to_string(),
                }),
                title: format!("{db}.{name} has {n} active parts in one partition"),
                detail: format!(
                    "Partition {partition}. This is what frequent small inserts produce; \
                     reads slow down until merges catch up, and past the server's limit \
                     inserts start being refused."
                ),
                suggested_sql: Some(format!(
                    "OPTIMIZE TABLE {}.{} PARTITION {partition};",
                    self.quote_ident(&db),
                    self.quote_ident(&name)
                )),
            });
        }

        // The index-shaped checks have no ClickHouse meaning at all, which is worth
        // saying: an empty findings list should not read as "nothing to look at".
        report.unavailable.push(UnavailableCheck {
            kind: FindingKind::UnusedIndex,
            reason: "ClickHouse has no secondary-index usage statistics to read".to_string(),
        });
        Ok(report)
    }

    async fn server_metrics(&self) -> Result<red_core::server::ServerSnapshot> {
        use red_core::server::{MetricGroup as G, MetricValue as V, ServerMetric as M};

        let mut snap =
            red_core::server::ServerSnapshot::new(red_core::DbKind::Clickhouse, now_unix());

        // One query per system table rather than one joined query: they are
        // three different shapes, and a role denied one of them must lose only
        // that group. `system.metrics` and `system.events` are gauges and
        // counters respectively, which is exactly the `Count`/`Total` split.
        let gauges = self
            .run_simple(
                "SELECT metric, value FROM system.metrics \
                 WHERE metric IN ('Query', 'Merge', 'PartMutation', 'TCPConnection', \
                                  'HTTPConnection', 'ReadonlyReplica', 'DelayedInserts')"
                    .to_string(),
                &[],
            )
            .await;
        match gauges {
            Ok((_, _, rows)) => {
                let find = |name: &str| {
                    rows.iter()
                        .find(|r| r.first().and_then(Cell::as_str).as_deref() == Some(name))
                        .and_then(|r| r.get(1))
                        .and_then(Cell::as_i64)
                        .map(|v| v.max(0) as u64)
                };
                for (metric, key, label, group) in [
                    ("Query", "queries", "Running queries", G::Throughput),
                    ("Merge", "merges", "Running merges", G::Throughput),
                    (
                        "PartMutation",
                        "part_mutations",
                        "Running mutations",
                        G::Throughput,
                    ),
                    (
                        "DelayedInserts",
                        "delayed_inserts",
                        "Delayed inserts",
                        G::Throughput,
                    ),
                    (
                        "TCPConnection",
                        "tcp_connections",
                        "Native connections",
                        G::Connections,
                    ),
                    (
                        "HTTPConnection",
                        "http_connections",
                        "HTTP connections",
                        G::Connections,
                    ),
                    (
                        "ReadonlyReplica",
                        "readonly_replicas",
                        "Read-only replicas",
                        G::Replication,
                    ),
                ] {
                    if let Some(v) = find(metric) {
                        snap.push(M::new(group, key, label, V::Count(v)));
                    }
                }
            }
            Err(e) => snap.note_unavailable(format!("query, merge and connection counts: {e}")),
        }

        // Memory and uptime. `asynchronous_metrics` is sampled by the server on
        // its own schedule, so these lag by up to a minute by design; that is
        // still the only place ClickHouse publishes them.
        match self
            .run_simple(
                "SELECT metric, value FROM system.asynchronous_metrics \
                 WHERE metric IN ('MemoryResident', 'Uptime', 'OSMemoryTotal', \
                                  'ReplicasMaxAbsoluteDelay')"
                    .to_string(),
                &[],
            )
            .await
        {
            Ok((_, _, rows)) => {
                let find = |name: &str| {
                    rows.iter()
                        .find(|r| r.first().and_then(Cell::as_str).as_deref() == Some(name))
                        .and_then(|r| r.get(1))
                        .and_then(Cell::as_f64)
                };
                if let Some(rss) = find("MemoryResident") {
                    let used = rss.max(0.0) as u64;
                    snap.push(M::new(
                        G::Memory,
                        "memory_resident",
                        "Resident memory",
                        match find("OSMemoryTotal").map(|t| t.max(0.0) as u64) {
                            Some(total) if total > 0 => V::Ratio { used, total },
                            _ => V::Bytes(used),
                        },
                    ));
                }
                if let Some(uptime) = find("Uptime") {
                    snap.push(M::new(
                        G::Server,
                        "uptime",
                        "Uptime",
                        V::Duration(secs_to_duration(uptime)),
                    ));
                }
                if let Some(delay) = find("ReplicasMaxAbsoluteDelay") {
                    snap.push(
                        M::new(
                            G::Replication,
                            "replica_delay",
                            "Replica delay",
                            V::Duration(secs_to_duration(delay)),
                        )
                        .with_detail("largest absolute delay across replicated tables"),
                    );
                }
            }
            Err(e) => snap.note_unavailable(format!("memory and uptime: {e}")),
        }

        match self
            .run_simple(
                "SELECT event, value FROM system.events \
                 WHERE event IN ('Query', 'SelectedBytes', 'InsertedRows', 'FailedQuery')"
                    .to_string(),
                &[],
            )
            .await
        {
            Ok((_, _, rows)) => {
                for (event, key, label) in [
                    ("Query", "total_queries", "Queries run"),
                    ("SelectedBytes", "selected_bytes", "Bytes read"),
                    ("InsertedRows", "inserted_rows", "Rows inserted"),
                    ("FailedQuery", "failed_queries", "Queries failed"),
                ] {
                    let Some(v) = rows
                        .iter()
                        .find(|r| r.first().and_then(Cell::as_str).as_deref() == Some(event))
                        .and_then(|r| r.get(1))
                        .and_then(Cell::as_i64)
                    else {
                        continue;
                    };
                    let n = v.max(0) as u64;
                    snap.push(M::new(
                        G::Throughput,
                        key,
                        label,
                        // Bytes read is a byte total; the rest are plain counts.
                        // Both are cumulative, so both derive a rate.
                        V::Total(n),
                    ));
                }
            }
            Err(e) => snap.note_unavailable(format!("cumulative query counters: {e}")),
        }

        match self
            .run_simple(
                "SELECT sum(bytes_on_disk), sum(rows) FROM system.parts WHERE active".to_string(),
                &[],
            )
            .await
        {
            Ok((_, _, rows)) => {
                if let Some(row) = rows.first() {
                    if let Some(bytes) = row.first().and_then(Cell::as_i64) {
                        snap.push(M::new(
                            G::Storage,
                            "bytes_on_disk",
                            "Data on disk",
                            V::Bytes(bytes.max(0) as u64),
                        ));
                    }
                    if let Some(n) = row.get(1).and_then(Cell::as_i64) {
                        snap.push(M::new(
                            G::Storage,
                            "total_rows",
                            "Rows stored",
                            V::Count(n.max(0) as u64),
                        ));
                    }
                }
            }
            Err(e) => snap.note_unavailable(format!("stored size: {e}")),
        }

        snap.push(M::new(
            G::Server,
            "version",
            "Version",
            V::Text(self.server_version()),
        ));
        Ok(snap)
    }

    async fn server_sessions(&self) -> Result<(Vec<red_core::ServerSession>, bool)> {
        // `system.processes` is the whole picture on ClickHouse: one row per
        // in-flight query. There is no idle session to show (HTTP: a request is a
        // session) and no lock manager, so no wait graph.
        let (_, _, rows) = self
            .run_simple(
                "SELECT query_id, user, client_name, address, `database`, elapsed, query \
                 FROM system.processes ORDER BY elapsed DESC LIMIT 500"
                    .to_string(),
                &[],
            )
            .await?;
        let opt_text =
            |row: &Vec<Cell>, i: usize| Some(cell_text(row, i)).filter(|s| !s.is_empty());
        let sessions = rows
            .iter()
            .map(|row| red_core::ServerSession {
                key: red_core::SessionKey(opt_text(row, 0).unwrap_or_default()),
                user: opt_text(row, 1),
                application: opt_text(row, 2),
                client_addr: opt_text(row, 3),
                database: opt_text(row, 4),
                // ClickHouse reports no per-query state word; every row here is by
                // definition executing, and saying so beats an empty column.
                state: "executing".to_string(),
                wait: None,
                blocked_by: Vec::new(),
                query: opt_text(row, 6),
                // `elapsed` is a Float64 that the JSON-compact reply may render as
                // either a number or a quoted string depending on settings.
                elapsed_secs: row.get(5).and_then(Cell::as_f64).unwrap_or(0.0),
                // RED's own reads run under their own query ids and finish before
                // this list is rendered, so nothing here is ever RED itself.
                is_self: false,
            })
            .collect();
        Ok((sessions, false))
    }

    async fn kill_session(
        &self,
        key: &red_core::SessionKey,
        mode: red_core::KillMode,
    ) -> Result<()> {
        if self.read_only {
            return Err(RedError::Query("this connection is read-only".into()));
        }
        if mode == red_core::KillMode::Terminate {
            // `session_caps().can_terminate` is false, so the UI never offers this;
            // the guard is here because a capability descriptor the driver does not
            // also enforce is a comment, not a rule.
            return Err(RedError::Driver(
                "ClickHouse has no session to terminate apart from its query".to_string(),
            ));
        }
        // Bound as a parameter, not interpolated: a query id is server-supplied
        // text and must not reach the statement body.
        self.run_simple(
            "KILL QUERY WHERE query_id = {qid:String}".to_string(),
            &[("param_qid".to_string(), key.0.clone())],
        )
        .await
        .map(|_| ())
    }

    /// ClickHouse has views and UDFs; no triggers and no procedures. A UDF is
    /// server-wide, not per-database, so it drops unqualified — the same asymmetry
    /// `object_ddl` below already has to observe.
    fn drop_object_sql(&self, namespace: &str, name: &str, kind: ObjectKind) -> Option<String> {
        match kind {
            ObjectKind::Function => Some(format!(
                "DROP FUNCTION IF EXISTS {}",
                self.quote_ident(name)
            )),
            ObjectKind::View => Some(format!(
                "DROP VIEW IF EXISTS {}.{}",
                self.quote_ident(namespace),
                self.quote_ident(name)
            )),
            _ => None,
        }
    }

    async fn object_ddl(&self, namespace: &str, name: &str, kind: ObjectKind) -> Result<String> {
        // `SHOW CREATE` covers tables, views, and materialized views alike (they
        // are all entries in `system.tables`); a UDF is `SHOW CREATE FUNCTION`.
        let statement = match kind {
            ObjectKind::Function => format!("SHOW CREATE FUNCTION {}", self.quote_ident(name)),
            k if k.is_relation() => format!(
                "SHOW CREATE TABLE {}.{}",
                self.quote_ident(namespace),
                self.quote_ident(name)
            ),
            other => {
                return Err(RedError::Driver(format!(
                    "ClickHouse has no definition for a {}",
                    other.as_str()
                )));
            }
        };
        let (_, _, rows) = self.run_simple(statement, &[]).await?;
        let ddl = rows
            .first()
            .and_then(|row| row.first())
            .and_then(Cell::as_str)
            .ok_or_else(|| RedError::Driver(format!("{name} not found in {namespace}")))?;
        // ClickHouse renders the statement with literal `\n` escapes in the
        // JSON-compact reply; the JSON decode already resolved those, so this is
        // the multi-line text as the server formats it.
        Ok(format!("{ddl};\n"))
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
            .filter_map(Cell::as_str)
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
    ) -> Result<ExportOutcome> {
        let qid = new_query_id();
        let base = format!("SELECT * FROM ({}) AS _red", strip_trailing(sql));
        let (columns, types, mut resp, mut buf) = self.open_stream(&base, &qid).await?;
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();

        let file = File::create(path).map_err(driver_err)?;
        let mut writer =
            ExportWriter::begin(BufWriter::new(file), format, names, path).map_err(driver_err)?;
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
                writer
                    .write_row(&parse_row_line(&line, &types, None)?)
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
                        writer
                            .write_row(&parse_row_line(&buf, &types, None)?)
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

/// The write-relevant catalog facts for the tables one batch touches, keyed by
/// `(database, table)`. A failed read is kept as its message rather than dropped, so
/// the ops on that table are blocked with a reason instead of silently skipped.
type FactsCache =
    std::collections::HashMap<(String, String), std::result::Result<write::ChTableFacts, String>>;

/// How a POSTed mutation ended. Separate from `Result` because "the wait expired" is
/// a third answer, neither success nor failure: the server accepted the mutation and
/// is still applying it.
enum MutationReply {
    Done,
    StillRunning,
    Failed(String),
}

/// One op, decided but not yet written: see `ClickhouseDriver::preflight_one`.
#[derive(Default)]
struct Preflight {
    /// The rendered statement + binds. `None` for an insert (which needs none) and
    /// for a blocked op (which will never run).
    rendered: Option<write::OpSql>,
    /// The statement as the confirm dialog shows it, values inline.
    display: String,
    /// Rows the identity currently matches; `None` for an insert.
    matches: Option<u64>,
    /// Why this op can't run. `Some` means nothing will be attempted.
    blocked: Option<String>,
    /// The table is on a `Replicated*` engine, so the sync wait must cover every
    /// replica rather than this server alone.
    replicated: bool,
    form: write::Form,
}

/// The `param_db` / `param_tbl` binds every `system.*` catalog probe takes, so the
/// database and table names stay real parameters rather than interpolated text.
fn table_params(schema: &str, table: &str) -> Vec<(String, String)> {
    vec![
        ("param_db".to_string(), schema.to_string()),
        ("param_tbl".to_string(), table.to_string()),
    ]
}

/// One bulk `INSERT`'s worth of an edit batch: the target table, the columns every
/// row in the group fills, and the rows themselves. Built by [`insert_groups`].
struct InsertGroup {
    table: TableRef,
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
}

/// Regroup an all-[`EditOp::Insert`] batch into one bulk insert per
/// `(table, column list)`, preserving batch order. Two draft rows that filled
/// *different* columns must not share a statement (their values would land in the
/// wrong columns), so the column signature is part of the grouping key, not just the
/// table; drafts that filled the same columns fold into a single `INSERT`.
///
/// An `Update`/`Delete` op is an error rather than a skip: ClickHouse cannot honor
/// the atomic, exactly-one-row contract [`apply_edits`](DatabaseDriver::apply_edits)
/// promises, and silently dropping half a submit would be worse than refusing it.
fn insert_groups(ops: &[EditOp]) -> Result<Vec<InsertGroup>> {
    let mut groups: Vec<InsertGroup> = Vec::new();
    for op in ops {
        let EditOp::Insert { table, values } = op else {
            return Err(RedError::Driver(
                "in-grid UPDATE/DELETE is not supported on ClickHouse (OLAP): they are \
                 asynchronous ALTER … mutations with no transactional rollback. Use the SQL \
                 editor for ALTER TABLE … UPDATE/DELETE if you need them."
                    .to_string(),
            ));
        };
        let columns: Vec<Column> = values
            .iter()
            .map(|cv| Column {
                name: cv.column.clone(),
                decl_type: cv.decl_type.clone(),
            })
            .collect();
        let row: Vec<Value> = values.iter().map(|cv| cv.value.clone()).collect();
        // Linear scan: a submit carries a handful of draft rows, never enough to
        // warrant a map keyed on the signature.
        match groups
            .iter_mut()
            .find(|g| &g.table == table && same_columns(&g.columns, &columns))
        {
            Some(g) => g.rows.push(row),
            None => groups.push(InsertGroup {
                table: table.clone(),
                columns,
                rows: vec![row],
            }),
        }
    }
    Ok(groups)
}

/// Whether two column lists name the same columns in the same order (the grouping
/// signature for [`insert_groups`]). Declared types are ignored: they come from the
/// same result's column metadata, so equal names imply equal types.
fn same_columns(a: &[Column], b: &[Column]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.name == y.name)
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
pub(super) fn ch_quote(ident: &str) -> String {
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
        // `Box<RawValue>` rather than a derived `Deserialize` on `Cell`: red-driver
        // takes no direct `serde` dependency, and the map is one move per cell.
        let line: Vec<Box<RawValue>> = serde_json::from_slice(l).map_err(driver_err)?;
        rows.push(line.into_iter().map(Cell).collect());
    }
    Ok((columns, types, rows))
}

/// Parse one streamed JSON-array line into a display row.
fn parse_row_line(line: &[u8], types: &[String], cap: Option<CellCap>) -> Result<Vec<Value>> {
    // Borrowed `&RawValue` rather than [`Cell`]'s `Box`: the line outlives the map,
    // so the hot streaming path needs no per-cell allocation at all.
    let raw: Vec<&RawValue> = serde_json::from_slice(line).map_err(driver_err)?;
    Ok(raw
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let ty = types.get(i).map(String::as_str).unwrap_or("");
            ch_value(v.get(), ty, CellCap::caps(cap, i))
        })
        .collect())
}

/// Map one raw JSON row to display [`Value`]s, per the column types and any cell cap.
fn ch_row(raw: &[Cell], types: &[String], cap: Option<CellCap>) -> Vec<Value> {
    raw.iter()
        .enumerate()
        .map(|(i, v)| {
            let ty = types.get(i).map(String::as_str).unwrap_or("");
            ch_value(v.raw(), ty, CellCap::caps(cap, i))
        })
        .collect()
}

/// Map one JSON cell to a [`Value`], guided by the ClickHouse declared type. The
/// `JSON…` format already rendered every type to JSON text, so this is a small
/// classification: integers (numbers, or quoted strings for the 64-bit widths) →
/// [`Value::Integer`]; floats → [`Value::Real`]; everything else (decimal, date,
/// uuid, enum, and the composite `Array`/`Tuple`/`Map`) → text, capped if oversized.
///
/// `raw` is the cell's JSON *source text* (see [`Cell`]), so a value that fits no
/// Rust scalar falls through to text with the digits the server actually sent.
fn ch_value(raw: &str, ch_type: &str, max: Option<usize>) -> Value {
    match raw.as_bytes().first() {
        None => Value::Null,
        Some(b'n') => Value::Null,
        Some(b't') => Value::Integer(1),
        Some(b'f') => Value::Integer(0),
        // A JSON string: the 64-bit widths arrive quoted, and so does everything
        // ClickHouse renders as text (date, uuid, enum, and — once the server is
        // asked to — decimal).
        Some(b'"') => {
            let body = raw
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(raw);
            if body.contains('\\') {
                match serde_json::from_str::<String>(raw) {
                    Ok(s) => ch_scalar(&s, ch_type, max),
                    Err(_) => text_value(body, max),
                }
            } else {
                ch_scalar(body, ch_type, max)
            }
        }
        // Composite (Array / Tuple / Map / Nested): the source text is already the
        // compact JSON rendering, so it needs no re-serialisation.
        Some(b'[' | b'{') => text_value(raw, max),
        // A bare JSON number. Classified from its text, never through `f64` unless
        // the column really is a float.
        _ => ch_scalar(raw, ch_type, max),
    }
}

/// Classify one scalar cell's text against the declared ClickHouse type. Anything
/// that is not an in-range `Int*`/`Float*` keeps its text verbatim — which is what
/// makes `Decimal`, `Int128` and `Int256` exact.
fn ch_scalar(s: &str, ch_type: &str, max: Option<usize>) -> Value {
    if is_ch_int(ch_type)
        && let Ok(i) = s.parse::<i64>()
    {
        return Value::Integer(i);
    }
    if is_ch_float(ch_type)
        && let Ok(f) = s.parse::<f64>()
    {
        return Value::Real(f);
    }
    text_value(s, max)
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
            ));
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

/// The `i`th cell of a metadata row as an owned string; empty when the cell is
/// absent or is not a JSON string.
fn cell_text(row: &[Cell], i: usize) -> String {
    row.get(i)
        .and_then(Cell::as_str)
        .unwrap_or_default()
        .into_owned()
}

/// The `i`th cell of a metadata row as `i64`; `0` when absent or unparseable.
fn cell_num(row: &[Cell], i: usize) -> i64 {
    row.get(i).and_then(Cell::as_i64).unwrap_or(0)
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

    /// The cell mapping runs off raw JSON source text, so a value too wide for any
    /// Rust scalar keeps the digits the server sent. Parsing through `serde_json`'s
    /// `Number` rounded these through `f64` and re-rendered the double — the grid
    /// and every export showed a number that was never in the database.
    #[test]
    fn wide_numerics_keep_every_digit() {
        // ClickHouse leaves Decimal unquoted by default, so this arrives bare.
        let exact = "12345678901234567.8901234567";
        assert_eq!(
            ch_value(exact, "Decimal128(30, 10)", None),
            Value::Text(exact.into())
        );
        assert_eq!(
            ch_value(exact, "Nullable(Decimal(38, 10))", None),
            Value::Text(exact.into())
        );
        // Int128/Int256 exceed i64 either way round.
        let big = "170141183460469231731687303715884105727";
        assert_eq!(ch_value(big, "Int128", None), Value::Text(big.into()));
        assert_eq!(
            ch_value(&format!("\"{big}\""), "Int256", None),
            Value::Text(big.into())
        );
        // The ordinary widths still classify as scalars.
        assert_eq!(ch_value("42", "Int32", None), Value::Integer(42));
        assert_eq!(ch_value("\"42\"", "Int64", None), Value::Integer(42));
        assert_eq!(ch_value("1.5", "Float64", None), Value::Real(1.5));
        assert_eq!(ch_value("null", "Int32", None), Value::Null);
        assert_eq!(ch_value("true", "Bool", None), Value::Integer(1));
        // Strings keep their escapes decoded; composites keep their JSON text.
        assert_eq!(
            ch_value(r#""a\nb""#, "String", None),
            Value::Text("a\nb".into())
        );
        assert_eq!(
            ch_value("[1,2,3]", "Array(Int32)", None),
            Value::Text("[1,2,3]".into())
        );
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

    /// ClickHouse has no multi-statement transactions, so the sandbox is not
    /// merely unimplemented here - it is *unavailable*, and the driver has to say
    /// so rather than hand back a handle that would apply every write the moment
    /// it ran. Both halves are asserted: the capability answer and the refusal.
    #[tokio::test]
    async fn has_no_sandbox() {
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, true).await.unwrap();
        battery::no_sandbox_when_unsupported(&driver).await;
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
        // The built-not-typed filter wants the same fixture shape.
        battery::filters_cmp(&driver, &format!("SELECT * FROM {t}"), "author_id", "title").await;
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
                red_core::SortDirection::Asc,
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

    // Server-free unit test; always runs (no ClickHouse needed).
    #[test]
    fn insert_groups_fold_by_table_and_columns() {
        let tref = |name: &str| TableRef {
            schema: Some("db".into()),
            name: name.into(),
        };
        let cv = |column: &str, value: Value| ColumnValue {
            column: column.into(),
            value,
            decl_type: None,
        };
        let insert = |table: &str, values: Vec<ColumnValue>| EditOp::Insert {
            table: tref(table),
            values,
        };

        // Same table, same columns → one statement holding both rows, in batch order.
        let groups = insert_groups(&[
            insert("t", vec![cv("id", Value::Integer(1))]),
            insert("t", vec![cv("id", Value::Integer(2))]),
        ])
        .unwrap();
        assert_eq!(groups.len(), 1, "same signature folds into one INSERT");
        assert_eq!(
            groups[0].rows,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
        );

        // A different column set must not share a statement: its values would land
        // in the wrong columns.
        let groups = insert_groups(&[
            insert("t", vec![cv("id", Value::Integer(1))]),
            insert(
                "t",
                vec![cv("id", Value::Integer(2)), cv("n", Value::Integer(9))],
            ),
            insert("u", vec![cv("id", Value::Integer(3))]),
        ])
        .unwrap();
        assert_eq!(groups.len(), 3, "column set and table both split groups");
        assert_eq!(groups[2].table.name, "u");

        // An update in the batch is refused, not silently dropped.
        let update = EditOp::Update {
            table: tref("t"),
            keys: vec![cv("id", Value::Integer(1))],
            set: vec![cv("n", Value::Integer(2))],
        };
        assert!(
            insert_groups(&[insert("t", vec![cv("id", Value::Integer(1))]), update]).is_err(),
            "a non-insert op is refused rather than skipped"
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
    async fn draft_row_inserts_apply_but_update_delete_refuse() {
        // The grid's draft-row zone submits `EditOp::Insert`s through `apply_edits`;
        // ClickHouse honors those (an insert needs none of the guarantees OLAP can't
        // give) and refuses update/delete on the same seam.
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, false).await.unwrap();
        let db = database(&url);
        let t = tag("edits");
        let tref = TableRef {
            schema: Some(db.clone()),
            name: t.clone(),
        };
        driver
            .execute(&format!(
                "CREATE TABLE IF NOT EXISTS `{db}`.`{t}` \
                 (id Int32, name Nullable(String)) ENGINE = MergeTree ORDER BY (id)"
            ))
            .await
            .unwrap();
        let cv = |column: &str, value: Value| ColumnValue {
            column: column.into(),
            value,
            decl_type: None,
        };

        // An empty batch is a no-op returning 0, with no engine round-trip.
        assert_eq!(
            driver.apply_edits(&[]).await.unwrap(),
            0,
            "empty batch is a no-op"
        );

        // Two drafts sharing a column signature fold into one INSERT; a third filling
        // only `id` is its own statement. All three land.
        let applied = driver
            .apply_edits(&[
                EditOp::Insert {
                    table: tref.clone(),
                    values: vec![
                        cv("id", Value::Integer(1)),
                        cv("name", Value::Text("one".into())),
                    ],
                },
                EditOp::Insert {
                    table: tref.clone(),
                    values: vec![
                        cv("id", Value::Integer(2)),
                        cv("name", Value::Text("two".into())),
                    ],
                },
                EditOp::Insert {
                    table: tref.clone(),
                    values: vec![cv("id", Value::Integer(3))],
                },
            ])
            .await
            .unwrap();
        assert_eq!(applied, 3, "every draft row is reported inserted");
        let abort = AbortSignal::new();
        assert_eq!(
            driver
                .count(&format!("SELECT id FROM `{db}`.`{t}`"), &abort)
                .await
                .unwrap(),
            3,
            "the draft rows are in the table"
        );

        // Update / delete stay refused on this seam: OLAP has no transactional,
        // exactly-one-row in-grid edit.
        assert!(
            driver
                .apply_edit(&EditOp::Delete {
                    table: tref.clone(),
                    keys: vec![cv("id", Value::Integer(1))],
                })
                .await
                .is_err(),
            "delete is refused"
        );
        assert!(
            driver
                .apply_edit(&EditOp::Update {
                    table: tref.clone(),
                    keys: vec![cv("id", Value::Integer(1))],
                    set: vec![cv("name", Value::Text("changed".into()))],
                })
                .await
                .is_err(),
            "update is refused"
        );
        driver
            .execute(&format!("DROP TABLE `{db}`.`{t}`"))
            .await
            .unwrap();
    }

    /// The best-effort edit contract end to end: insert, update and delete each land
    /// and read back; a duplicated row, a stale row and a key-column edit are each
    /// refused **by the preflight**, by name and count, rather than by the engine
    /// after the user confirmed; and a batch mixing good and bad ops reports one
    /// outcome per op instead of stopping at the first failure.
    #[tokio::test]
    async fn applies_edits_best_effort() {
        use red_core::{BatchMode, OpStatus};
        let url = url_or_skip!();
        let driver = ClickhouseDriver::connect(&url, false).await.unwrap();
        let db = database(&url);
        let t = tag("besteffort");
        let tref = TableRef {
            schema: Some(db.clone()),
            name: t.clone(),
        };
        // `id` is the sorting key (so it leads the identity and can never be
        // updated); `name` and `note` are ordinary columns.
        driver
            .execute(&format!(
                "CREATE TABLE IF NOT EXISTS `{db}`.`{t}` \
                 (id Int32, name String, note Nullable(String)) ENGINE = MergeTree ORDER BY (id)"
            ))
            .await
            .unwrap();
        let cv = |column: &str, value: Value, decl: &str| ColumnValue {
            column: column.into(),
            value,
            decl_type: Some(decl.into()),
        };
        let id = |n: i32| cv("id", Value::Integer(n.into()), "Int32");
        let name = |v: &str| cv("name", Value::Text(v.into()), "String");
        let solo = BatchMode::BestEffort {
            allow_multi_match: false,
        };
        let abort = AbortSignal::new();
        let count_where = |predicate: &str| {
            let sql = format!("SELECT id FROM `{db}`.`{t}` WHERE {predicate}");
            let abort = &abort;
            let driver = &driver;
            async move { driver.count(&sql, abort).await.unwrap() }
        };
        let one = |outcomes: Vec<red_core::OpOutcome>| {
            assert_eq!(outcomes.len(), 1);
            outcomes.into_iter().next().expect("one outcome")
        };

        // Insert two rows through the best-effort seam.
        let outcomes = driver
            .apply_edits_best_effort(
                &[
                    EditOp::Insert {
                        table: tref.clone(),
                        values: vec![id(1), name("one")],
                    },
                    EditOp::Insert {
                        table: tref.clone(),
                        values: vec![id(2), name("two")],
                    },
                ],
                solo,
            )
            .await
            .unwrap();
        assert!(
            outcomes.iter().all(|o| !o.status.unfinished()),
            "both inserts land: {outcomes:?}"
        );

        // A single-cell update against a sorting-key identity lands and is visible.
        let updated = one(driver
            .apply_edits_best_effort(
                &[EditOp::Update {
                    table: tref.clone(),
                    keys: vec![id(1), name("one")],
                    set: vec![cv("note", Value::Text("edited".into()), "Nullable(String)")],
                }],
                solo,
            )
            .await
            .unwrap());
        assert!(
            matches!(
                updated.status,
                OpStatus::Applied { .. } | OpStatus::Submitted
            ),
            "the update ran: {:?}",
            updated.status
        );
        if matches!(updated.status, OpStatus::Applied { .. }) {
            assert_eq!(
                count_where("note = 'edited'").await,
                1,
                "an applied mutation is visible on the next read"
            );
        }

        // A key column is refused in preflight, not by the engine.
        let key_edit = one(driver
            .apply_edits_best_effort(
                &[EditOp::Update {
                    table: tref.clone(),
                    keys: vec![id(2), name("two")],
                    set: vec![id(99)],
                }],
                solo,
            )
            .await
            .unwrap());
        match key_edit.status {
            OpStatus::Blocked(reason) => assert!(
                reason.contains("id"),
                "the refusal names the column: {reason}"
            ),
            other => panic!("expected a blocked key-column edit, got {other:?}"),
        }

        // A row that changed underneath is refused as stale, by count.
        let stale = one(driver
            .apply_edits_best_effort(
                &[EditOp::Delete {
                    table: tref.clone(),
                    keys: vec![id(1), name("gone")],
                }],
                solo,
            )
            .await
            .unwrap());
        match stale.status {
            OpStatus::Blocked(reason) => assert!(
                reason.contains("no longer matches"),
                "a stale row says so: {reason}"
            ),
            other => panic!("expected a blocked stale delete, got {other:?}"),
        }

        // Duplicates are normal on ClickHouse, and an ambiguous identity is refused
        // by count unless the user acknowledged applying to all of them.
        driver
            .apply_edits_best_effort(
                &[EditOp::Insert {
                    table: tref.clone(),
                    values: vec![id(2), name("two")],
                }],
                solo,
            )
            .await
            .unwrap();
        let ambiguous = one(driver
            .apply_edits_best_effort(
                &[EditOp::Delete {
                    table: tref.clone(),
                    keys: vec![id(2), name("two")],
                }],
                solo,
            )
            .await
            .unwrap());
        match ambiguous.status {
            OpStatus::Blocked(reason) => assert!(
                reason.contains("matches 2 rows"),
                "the refusal reports the count: {reason}"
            ),
            other => panic!("expected a blocked ambiguous delete, got {other:?}"),
        }
        // With the acknowledgement it runs, taking both rows.
        let acknowledged = one(driver
            .apply_edits_best_effort(
                &[EditOp::Delete {
                    table: tref.clone(),
                    keys: vec![id(2), name("two")],
                }],
                BatchMode::BestEffort {
                    allow_multi_match: true,
                },
            )
            .await
            .unwrap());
        assert!(
            !acknowledged.status.unfinished(),
            "the acknowledged delete ran: {:?}",
            acknowledged.status
        );

        // A partial batch reports per-op outcomes and never stops at the first
        // failure: the good op after a blocked one still runs.
        let outcomes = driver
            .apply_edits_best_effort(
                &[
                    EditOp::Delete {
                        table: tref.clone(),
                        keys: vec![id(404), name("missing")],
                    },
                    EditOp::Insert {
                        table: tref.clone(),
                        values: vec![id(3), name("three")],
                    },
                ],
                solo,
            )
            .await
            .unwrap();
        assert_eq!(outcomes.len(), 2, "one outcome per op");
        assert!(
            outcomes[0].status.unfinished(),
            "the stale delete is blocked"
        );
        assert!(
            !outcomes[1].status.unfinished(),
            "the op after a blocked one still runs: {:?}",
            outcomes[1].status
        );
        assert_eq!(count_where("id = 3").await, 1);

        // The preflight reports the same refusals without writing anything.
        let plan = driver
            .preflight_edits(&[EditOp::Delete {
                table: tref.clone(),
                keys: vec![id(404), name("missing")],
            }])
            .await
            .unwrap();
        assert_eq!(plan[0].matches, Some(0));
        assert!(plan[0].blocked.is_some());
        assert!(
            plan[0].sql.contains("DELETE") && plan[0].sql.contains("`id` = 404"),
            "the plan shows the real statement: {}",
            plan[0].sql
        );

        // A non-MergeTree table reports why its rows can't be edited.
        let memory = tag("memory");
        driver
            .execute(&format!(
                "CREATE TABLE IF NOT EXISTS `{db}`.`{memory}` (id Int32) ENGINE = Memory"
            ))
            .await
            .unwrap();
        let caps = driver
            .edit_caps(&db, &memory, &TableDetail::default())
            .await
            .unwrap();
        assert_eq!(caps.mode, red_core::EditMode::None);
        assert!(
            caps.note.unwrap_or_default().contains("Memory"),
            "the reason names the engine"
        );

        driver
            .execute(&format!("DROP TABLE `{db}`.`{memory}`"))
            .await
            .unwrap();
        driver
            .execute(&format!("DROP TABLE `{db}`.`{t}`"))
            .await
            .unwrap();
    }
}
