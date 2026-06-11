//! The database abstraction layer. `DatabaseDriver` is RED's analogue of Nyx's
//! `RemoteClient`: an object-safe trait the service holds as `Arc<dyn …>` and
//! drives across many commands, with one impl per engine.
//!
//! Nothing here materializes a whole result: queries run behind a windowed
//! [`QueryCursor`], paging is random-access (`fetch_page`) or indexed-seek
//! (`fetch_seek`), and `export` streams row-by-row. This keeps memory flat over
//! results of any size, the layer's central performance contract.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use red_core::{
    Column, ExportFormat, KeySpec, QueryOptions, RedError, Result, ResultPage, RowWindow,
    SchemaMeta, TableDetail, Value,
};

#[cfg(test)]
mod conformance;
mod format;
mod mysql;
mod postgres;
mod sqlite;
pub use mysql::MysqlDriver;
pub use postgres::PostgresDriver;
pub use sqlite::SqliteDriver;

/// One open database session. Object-safe so the service can hold
/// `Arc<dyn DatabaseDriver>` and swap engines behind it.
#[async_trait]
pub trait DatabaseDriver: Send + Sync {
    /// Cheap liveness probe — opens/touches the underlying connection.
    async fn ping(&self) -> Result<()>;

    /// Engine version string (e.g. `"3.46.0"`), for the status bar. Cheap and
    /// synchronous — drivers report a compiled-in or already-known value.
    fn server_version(&self) -> String;

    /// Prepare `sql`, read column metadata, and return a live cursor. Cheap by
    /// design: this does NOT step rows — the first (potentially expensive) step
    /// happens on the first `next_window`, which is the cancellable path.
    async fn open_cursor(&self, sql: &str, opts: QueryOptions) -> Result<Box<dyn QueryCursor>>;

    /// The schema-tree skeleton: every namespace with its table/view names. Cheap
    /// by contract — names + kinds only, no per-table `COUNT(*)` and no column
    /// walk (that's `describe_table`, pulled lazily on expand).
    async fn list_objects(&self) -> Result<Vec<SchemaMeta>>;

    /// One object's columns, foreign keys, and indexes. Loaded on demand when the
    /// user expands a table, so the initial tree load stays light.
    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableDetail>;

    /// Total row count of `sql`'s result — one pass, no row materialization. Lets
    /// the grid show a real scrollbar without holding every row.
    async fn count(&self, sql: &str) -> Result<i64>;

    /// A random-access `(offset, limit)` page of `sql`'s result. Backs the grid's
    /// load-on-scroll so memory stays flat: only the pages around the viewport are
    /// ever resident.
    async fn fetch_page(&self, sql: &str, offset: usize, limit: usize) -> Result<ResultPage>;

    /// One keyset (seek) page of `sql`'s result, ordered by `key`: the rows
    /// strictly after (`descending = false`) or strictly before (`descending =
    /// true`, returned in reverse order — the caller flips) `bound`; `None`
    /// starts from the result's first/last row. An indexed seek, so it costs the
    /// same at row 200 or 46,000,000 — unlike `fetch_page`'s O(offset). `bound`
    /// is bound as a real parameter, never string-interpolated.
    async fn fetch_seek(
        &self,
        sql: &str,
        key: &KeySpec,
        bound: Option<&Value>,
        descending: bool,
        limit: usize,
    ) -> Result<ResultPage>;

    /// One keyset page from an inclusive lower bound with a bounded `skip`:
    /// rows with `key >= from`, ordered by `key`, skipping `skip` then taking
    /// `limit`. `from = None` starts at the result's first row. The lower bound
    /// is an *indexed* seek, so the `OFFSET skip` only walks within the
    /// post-seek window — O(skip), not O(offset-from-row-0). Backs exact "go to
    /// row N" jumps and the checkpoint-index build, which both seek to a known
    /// key and then step a bounded number of rows. `from` is bound as a real
    /// parameter, never string-interpolated.
    async fn fetch_seek_skip(
        &self,
        sql: &str,
        key: &KeySpec,
        from: Option<&Value>,
        skip: usize,
        limit: usize,
    ) -> Result<ResultPage>;

    /// `MIN`/`MAX` of `key` over `sql`'s result — one indexed probe, backing
    /// key-space interpolation for fraction jumps. `None` when the result is
    /// empty or the key isn't an integer (not interpolable).
    async fn key_bounds(&self, sql: &str, key: &KeySpec) -> Result<Option<(i64, i64)>>;

    /// Run a non-row-returning statement wrapped in a transaction, returning the
    /// number of rows affected. A read-only driver rejects the write at the engine.
    async fn execute(&self, sql: &str) -> Result<u64>;

    /// Stream `sql`'s result straight to `path` in `format`, row-by-row — never
    /// materializing the whole result. Returns the number of data rows written.
    ///
    /// `cancel` is checked per row: when it flips true the export bails early,
    /// removes the partial file, and returns [`RedError::Interrupted`]. `progress`
    /// receives the running row count, throttled (every N rows / ~50ms) so the
    /// channel isn't flooded — the caller maps it to a progress event.
    async fn export(
        &self,
        sql: &str,
        path: &Path,
        format: ExportFormat,
        cancel: Arc<AtomicBool>,
        progress: UnboundedSender<u64>,
    ) -> Result<u64>;
}

/// A live, windowed result cursor. Object-safe; the service holds it as
/// `Box<dyn QueryCursor>`. `next_window` takes `&self` — all mutable cursor state
/// lives on the driver's blocking thread — so the returned future is
/// `Send + 'static` and can be raced against incoming commands for cancellation.
#[async_trait]
pub trait QueryCursor: Send {
    /// Column metadata, known up front (read at `open_cursor` without stepping).
    fn columns(&self) -> &[Column];

    /// Fetch up to `max` more rows. `RowWindow::exhausted` marks the end of the
    /// result; once `true`, no further `next_window` calls should be made.
    async fn next_window(&self, max: usize) -> Result<RowWindow>;

    /// A clone-able, thread-safe handle that aborts an in-flight fetch
    /// out-of-band (user cancel / timeout).
    fn cancel_token(&self) -> CancelToken;
}

/// Engine-agnostic cancel handle. SQLite wraps `rusqlite`'s `InterruptHandle`;
/// Postgres wraps its out-of-band cancel request. Cloning is cheap and the token
/// is safe to call from any thread.
#[derive(Clone)]
pub struct CancelToken(Arc<dyn Fn() + Send + Sync>);

impl CancelToken {
    pub(crate) fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// Signal the in-flight fetch to abort. Idempotent and non-blocking.
    pub fn cancel(&self) {
        (self.0)()
    }
}

/// Map any error carrying a message into a driver error. Small helper so impls
/// stay terse.
pub(crate) fn driver_err(e: impl std::fmt::Display) -> RedError {
    RedError::Driver(e.to_string())
}
