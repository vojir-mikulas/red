// SPDX-License-Identifier: GPL-3.0-or-later

//! The database abstraction layer. `DatabaseDriver` is RED's analogue of Nyx's
//! `RemoteClient`: an object-safe trait the service holds as `Arc<dyn …>` and
//! drives across many commands, with one impl per engine.
//!
//! Scaffold scope: `query` materializes the whole result. The streaming/windowed
//! cursor that the performance goals require is the first real piece of work on
//! top of this seam — the trait is shaped to grow a `stream`/`cancel` surface
//! without disturbing callers.

use async_trait::async_trait;
use red_core::{QueryResult, RedError, Result};

mod sqlite;
pub use sqlite::SqliteDriver;

/// One open database session. Object-safe so the service can hold
/// `Arc<dyn DatabaseDriver>` and swap engines behind it.
#[async_trait]
pub trait DatabaseDriver: Send + Sync {
    /// Cheap liveness probe — opens/touches the underlying connection.
    async fn ping(&self) -> Result<()>;

    /// Execute SQL and return the full result. Large result sets will move to a
    /// streamed cursor; this eager form is the scaffold baseline.
    async fn query(&self, sql: &str) -> Result<QueryResult>;
}

/// Map any error carrying a message into a driver error. Small helper so impls
/// stay terse.
pub(crate) fn driver_err(e: impl std::fmt::Display) -> RedError {
    RedError::Driver(e.to_string())
}
