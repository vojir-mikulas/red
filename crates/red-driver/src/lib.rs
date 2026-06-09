// SPDX-License-Identifier: GPL-3.0-or-later

//! The database abstraction layer. `DatabaseDriver` is RED's analogue of Nyx's
//! `RemoteClient`: an object-safe trait the service holds as `Arc<dyn …>` and
//! drives across many commands, with one impl per engine.
//!
//! Scaffold scope: `query` materializes the whole result. The streaming/windowed
//! cursor that the performance goals require is the first real piece of work on
//! top of this seam — the trait is shaped to grow a `stream`/`cancel` surface
//! without disturbing callers.

use std::sync::Arc;

use async_trait::async_trait;
use red_core::{
    Column, QueryOptions, RedError, Result, ResultPage, RowWindow, SchemaMeta, TableDetail,
};

mod sqlite;
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
/// Postgres (M7) will wrap its out-of-band cancel request. Cloning is cheap and
/// the token is safe to call from any thread.
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
