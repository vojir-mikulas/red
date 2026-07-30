//! The assistant's backend half: the agentic loop and the tool catalogs it
//! stands on. A turn runs as a spawned task off the dispatch loop, streams
//! `AiDelta` events as tokens arrive, and drives the model -> tool -> model loop
//! itself (the plain Messages API tool-use loop, on the service thread).
//!
//! The tree mirrors that shape. Engine-agnostic machinery sits at this level:
//! [`turn`] runs the loop, [`state`] holds per-conversation state and the
//! write-approval registry, [`gate`] is the single source of truth for what
//! counts as a write, and [`report`]/[`export`] are the two ways a tool hands
//! the user a file. Below it, one subtree per driver seam -- [`sql`], [`kv`],
//! [`doc`] -- each laid out the same way: `catalog` declares the tools and the
//! system prompt that introduces them, the subtree's own `mod` holds the
//! executor that dispatches them, `tools` holds the composed reads, and `format`
//! turns driver types into the text the model reads. Where a seam has more to
//! say it adds a file rather than growing one: `sql/diff`, `kv/write`, `kv/set`,
//! `doc/write`.
//!
//! The invariant that spans all of it: every tool is backed by a driver seam
//! that already exists and inherits its guard, so the model gets the same
//! windowed, never-materialized reads a human does, and every mutation rides the
//! per-call approval in [`gate`].

use std::sync::Arc;

use red_ai::{CancelToken, ToolDef};
use red_core::{AiPolicy, sql::Dialect};
use red_driver::{DatabaseDriver, DocDriver, KvDriver};
use serde_json::Value as Json;

use crate::protocol::AiContext;

mod doc;
mod export;
mod gate;
mod kv;
mod report;
mod sql;
mod state;
mod turn;
mod util;

#[cfg(test)]
mod testutil;

pub(crate) use gate::{is_headless_tool, is_write_tool};
pub(crate) use sql::user_turn;
pub(crate) use state::{AiState, ReportSink};
pub(crate) use turn::run_turn;

use doc::catalog::{doc_system_prompt, doc_tool_catalog};
use doc::doc_run_tool;
use kv::catalog::{kv_system_prompt, kv_tool_catalog};
use kv::kv_run_tool;
use sql::catalog::{system_prompt, tool_catalog};
use sql::run_tool;

/// Which engine the agent turn is grounded in. The model→tool loop, streaming,
/// budget, write gate, and history are identical across all three; only the tool
/// catalog, the tool execution, and the system prompt differ. This enum is the
/// one place that dispatch happens, which is what lets [`turn`] stay entirely
/// engine-agnostic.
#[derive(Clone)]
pub(crate) enum AiBackend {
    Sql {
        driver: Arc<dyn DatabaseDriver>,
        /// The engine's lexical dialect, threaded into every gate that scans SQL
        /// (`is_read_only_select`, `write_shape`): scanning a statement with the
        /// wrong string/comment rules is a gate bypass, not a nicety — e.g.
        /// Postgres ends `'a\'` at the second quote, so what follows is live SQL.
        dialect: Dialect,
    },
    Kv(Arc<dyn KvDriver>),
    Doc(Arc<dyn DocDriver>),
}
impl AiBackend {
    /// The tier-filtered tool catalog this backend offers under `policy`. Routes to
    /// the SQL schema/query tools, the Redis `kv_*` tools, or the MongoDB doc tools.
    pub(crate) fn catalog(&self, policy: &AiPolicy) -> Vec<ToolDef> {
        match self {
            AiBackend::Sql { .. } => tool_catalog(policy),
            AiBackend::Kv(_) => kv_tool_catalog(policy),
            AiBackend::Doc(_) => doc_tool_catalog(policy),
        }
    }

    /// The SQL lexical dialect the gates must scan with; [`Dialect::Generic`]
    /// for the non-SQL backends (their gates never lex SQL).
    pub(crate) fn dialect(&self) -> Dialect {
        match self {
            AiBackend::Sql { dialect, .. } => *dialect,
            AiBackend::Kv(_) | AiBackend::Doc(_) => Dialect::Generic,
        }
    }

    /// The full grounding system prompt for this backend under `ctx`/`policy`.
    pub(crate) fn system_prompt(&self, ctx: &AiContext, policy: &AiPolicy) -> String {
        match self {
            AiBackend::Sql { .. } => system_prompt(ctx, policy),
            AiBackend::Kv(_) => kv_system_prompt(ctx, policy),
            AiBackend::Doc(_) => doc_system_prompt(ctx, policy),
        }
    }

    /// Whether `name` is a mutating tool for this backend. Used to withhold writes
    /// over the subscription/MCP path (each backend has its own writer set: the SQL
    /// `propose_*` tools vs. the Redis `kv_*` writers).
    pub(crate) fn is_write_tool(&self, name: &str) -> bool {
        // Both backends fail *closed*: a tool is a write unless it's explicitly
        // named in the read-only allowlist (`READ_ONLY_TOOLS`, which lists the
        // `kv_*` reads too). Classifying KV via the `KV_WRITE_TOOLS` denylist
        // here would fail *open* — a future KV writer not added to that list
        // would be advertised over MCP and auto-allowed over ACP with no
        // approval. (`is_kv_write_tool` still routes the known writers to their
        // KV-specific validator inside `assess_write`.)
        is_write_tool(name)
    }

    /// Run one tool call against this backend's driver, returning `(content, ok)`.
    pub(crate) async fn run_tool(
        &self,
        name: &str,
        input: &Json,
        policy: &AiPolicy,
        cancel: &CancelToken,
        report: &ReportSink,
    ) -> (String, bool) {
        match self {
            AiBackend::Sql { driver, dialect } => {
                run_tool(driver, *dialect, name, input, policy, cancel, report).await
            }
            AiBackend::Kv(d) => kv_run_tool(d, name, input, policy, cancel, report).await,
            AiBackend::Doc(d) => doc_run_tool(d, name, input, policy, cancel, report).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::state::ReportSink;
    use super::*;
    use crate::ai::testutil::{doc_stub, doc_tool};
    use red_ai::CancelToken;
    use red_core::sql::Dialect;
    use red_core::{AiPolicy, AiTier};
    use red_driver::DatabaseDriver;
    use serde_json::json;

    /// Every tool a catalog advertises must also be reachable in its executor.
    /// A `ToolDef` with no `run_tool` arm is a tool that exists right up until
    /// the model calls it, which is worse than one that was never offered — so
    /// this asserts the *structural* wiring by checking nothing falls through to
    /// the unknown-tool arm. (`spawn_subagent` is intercepted in `run_turn`
    /// before `run_tool` and is excluded by design.)
    #[tokio::test]
    async fn every_advertised_tool_has_an_executor_arm() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        let db = std::env::temp_dir().join(format!("red-arm-{}.db", uuid::Uuid::new_v4().simple()));
        {
            rusqlite::Connection::open(&db)
                .unwrap()
                .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);")
                .unwrap();
        }
        let sql: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        for tool in tool_catalog(&write) {
            if tool.name == "spawn_subagent" {
                continue;
            }
            let (content, _) = run_tool(
                &sql,
                Dialect::Sqlite,
                &tool.name,
                &json!({}),
                &write,
                &CancelToken::new(),
                &ReportSink::disabled(),
            )
            .await;
            assert!(
                !content.contains("unknown tool"),
                "SQL tool `{}` is advertised but has no executor arm",
                tool.name
            );
        }
        let doc = doc_stub();
        for tool in doc_tool_catalog(&write) {
            if tool.name == "spawn_subagent" {
                continue;
            }
            let (content, _) = doc_tool(&doc, &tool.name, json!({})).await;
            assert!(
                !content.contains("unknown tool"),
                "doc tool `{}` is advertised but has no executor arm",
                tool.name
            );
        }
    }
}
