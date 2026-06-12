//! The wire protocol between the UI and the backend thread: `Command`
//! (UI → service), `Event` (service → UI), and the `RunFetch` shape describing
//! one keyset run-window request. These are the only types that cross the
//! channel; the dispatch loop and handle types live in their own modules.
//!
//! Both channels carry their payload in an **envelope** `(Option<SessionId>, _)`
//! so a message routes to one of several keep-alive sessions without threading a
//! `session` field through every variant (see [`SessionId`]). `None` is for the
//! genuinely session-less messages — a `TestConnection` probe, `Shutdown`, and
//! the `TestSucceeded`/`TestFailed` replies.

use std::path::PathBuf;
use std::time::Duration;

use red_core::{
    Column, ConnectionConfig, ExportFormat, KeySpec, QueryOptions, RowWindow, SchemaMeta,
    TableDetail, Value,
};

/// Identifies one keep-alive backend session. Minted UI-side at connect start so
/// the UI can address a session before it's live (the connecting splash, a
/// cancel, a retry), and stable across an errored session's retries so the
/// workspace identity doesn't churn. The service keys its `HashMap<SessionId,
/// SessionState>` by this; the UI keys its parked-workspace map by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u64);

/// UI → service. Routed to a session by the channel envelope's [`SessionId`]
/// (see the module docs). `Connect` *creates* the session its envelope names;
/// `Disconnect`/`CloseSession` drop it; the rest address an existing one.
#[derive(Debug)]
pub enum Command {
    Connect(ConnectionConfig),
    /// Open a throwaway session to validate a config, then drop it. Reports back
    /// via `TestSucceeded`/`TestFailed` without disturbing the active session.
    TestConnection(ConnectionConfig),
    /// Open a cursor for `sql` and stream the first window.
    Query {
        sql: String,
        opts: QueryOptions,
    },
    /// Pull the next window from the active cursor.
    FetchMore {
        max: usize,
    },
    /// Load the schema-tree skeleton (namespaces + object names) for the sidebar.
    LoadObjects,
    /// Describe one object's columns / FKs / indexes — sent lazily on tree expand.
    DescribeTable {
        schema: String,
        table: String,
    },
    /// Open `sql` as a grid result: count its rows and report column metadata +
    /// the total. The result is then browsed page-by-page via `FetchPage`, or —
    /// when a seek key resolves — run-by-run via `FetchRun`.
    ///
    /// `epoch` identifies this open result. Several results can be open at once
    /// (one per query tab), each keyed by its epoch; a page or export names the
    /// epoch it wants. `CloseResult` drops one when its tab closes.
    ///
    /// `table` names the `(schema, table)` when `sql` is a plain table browse:
    /// the backend introspects it for a keyset seek key (single-column PK or
    /// unique not-null index) and echoes the resolved [`KeySpec`] in
    /// `ResultReady`. `None` (editor SQL) pages by `OFFSET`.
    ///
    /// `sort` describes a header-click sort over a table browse: `sql` is the
    /// *unwrapped* base query and the backend either resolves a composite
    /// `(sort_col, pk)` keyset key (fast seek) or, failing that, wraps the base in
    /// `ORDER BY <position>` and pages by `OFFSET`. `None` is the unsorted open.
    OpenResult {
        sql: String,
        epoch: u64,
        table: Option<(String, String)>,
        sort: Option<SortKey>,
    },
    /// Fetch one random-access page of an open result (grid load-on-scroll).
    /// `epoch` selects which open result; an unknown epoch is ignored (the tab
    /// closed or re-sorted).
    FetchPage {
        offset: usize,
        limit: usize,
        epoch: u64,
    },
    /// Fetch one run window of a keyset-keyed open result: extend the
    /// grid's resident run from a boundary key, or jump to an ordinal. Replied
    /// with `ResultRunLoaded`, echoing `fetch`/`seq` so the grid can drop a
    /// reply its buffer has moved past.
    FetchRun {
        epoch: u64,
        fetch: RunFetch,
        limit: usize,
        seq: u64,
    },
    /// Re-fetch a contiguous row range of an open result *in full*, for a copy
    /// whose selection holds cells the grid clipped for display. The grid buffer
    /// caps fat text per resident row, so its copy would otherwise hand over the
    /// clipped head; this pulls the rows fresh (like a page fetch, but routed to
    /// the clipboard via `CopyRowsLoaded` rather than into the buffer). `id`
    /// matches the reply to the pending copy. Like `FetchPage`, a stale epoch is
    /// ignored.
    CopyRows {
        offset: usize,
        limit: usize,
        epoch: u64,
        id: u64,
    },
    /// Drop an open result (its query tab closed, or it was re-sorted into a new
    /// epoch). Unknown epochs are a no-op.
    CloseResult {
        epoch: u64,
    },
    /// Run a non-row-returning statement (write/DDL) in a transaction.
    Execute {
        sql: String,
    },
    /// Stream an open result to `path` in `format`, row-by-row. `epoch` selects
    /// which open result (the active tab's grid); `id` identifies the export so
    /// progress / completion events and a `CancelExport` route to it. The export
    /// runs off the dispatch loop, so the loop stays responsive while it streams.
    Export {
        format: ExportFormat,
        path: PathBuf,
        epoch: u64,
        id: u64,
    },
    /// Abort an in-flight export by `id` (the toast's Cancel). The partial file is
    /// removed so no truncated CSV/JSON is left behind.
    CancelExport {
        id: u64,
    },
    /// Abort the active query / drop its cursor.
    Cancel,
    /// Drop the envelope's session and any cursor; the window returns to a
    /// disconnected state. Other warm sessions are untouched.
    Disconnect,
    /// Drop the envelope's session — the user removed/closed a *background*
    /// connection (vs `Disconnect`, the window's active one going away). Same
    /// effect on the backend; kept distinct so the UI's intent stays legible.
    CloseSession,
    /// Tell the backend which session is foregrounded (`None` = the welcome
    /// screen). The foreground session is exempt from idle eviction — a user can
    /// stare at a result without scrolling and it must stay warm. Global (the
    /// payload, not the envelope, carries the id).
    SetActiveSession(Option<SessionId>),
    Shutdown,
}

/// service → UI. Streamed into the UI's async loop, tagged by the channel
/// envelope with the [`SessionId`] it belongs to (`None` for the session-less
/// `TestSucceeded`/`TestFailed` probe replies) so the UI routes it to the right
/// workspace — including a backgrounded one whose query is still populating.
#[derive(Debug)]
pub enum Event {
    /// A session opened. `version` is the engine version for the status bar.
    Connected {
        version: String,
    },
    /// The session was dropped (in response to `Disconnect`).
    Disconnected,
    /// A `TestConnection` probe opened a session successfully; `version` is the
    /// engine version it reported.
    TestSucceeded {
        version: String,
    },
    /// A `TestConnection` probe failed; `message` is the driver error.
    TestFailed {
        message: String,
    },
    /// A query opened; column metadata is known before any rows arrive.
    QueryStarted {
        columns: Vec<Column>,
    },
    /// One bounded window of rows. `RowWindow::exhausted` marks the last one.
    QueryRows(RowWindow),
    /// The cursor reached the end of the result.
    QueryFinished {
        rows_streamed: usize,
        elapsed: Duration,
    },
    /// The active query was cancelled (user `Cancel`).
    QueryCancelled,
    /// The schema-tree skeleton, in response to `LoadObjects`.
    ObjectsLoaded {
        schemas: Vec<SchemaMeta>,
    },
    /// One object's detail, in response to `DescribeTable`. Echoes `schema`/`table`
    /// so the async UI routes the detail to the right node regardless of order.
    TableDescribed {
        schema: String,
        table: String,
        detail: TableDetail,
    },
    /// A result opened: its columns and total row count (for `OpenResult`).
    /// Echoes the open `epoch` so the grid can ignore a late reply for a result
    /// it has already replaced. `key` is the seek key the backend resolved for
    /// a table browse — present, the grid pages by keyset runs (`FetchRun`)
    /// instead of `OFFSET`.
    ResultReady {
        columns: Vec<Column>,
        total: usize,
        epoch: u64,
        key: Option<KeySpec>,
    },
    /// One page of the open result. Echoes `offset` so the grid drops it into the
    /// right slot of its window buffer regardless of arrival order, and `epoch`
    /// so a page for a superseded result is discarded.
    ResultPageLoaded {
        offset: usize,
        rows: Vec<Vec<red_core::Value>>,
        epoch: u64,
    },
    /// One run window of a keyset result, in response to `FetchRun`. Echoes the
    /// request (`fetch`, `seq`) so the grid can match it against its in-flight
    /// state. `estimated` is `true` when a `Jump` landed by key-space
    /// interpolation — its ordinals are approximate until the run touches a
    /// true end of the result.
    ResultRunLoaded {
        epoch: u64,
        fetch: RunFetch,
        rows: Vec<Vec<red_core::Value>>,
        estimated: bool,
        seq: u64,
    },
    /// The full rows for a `CopyRows` request. Echoes `id` so the UI matches it
    /// to the pending copy and writes the untruncated selection to the clipboard.
    CopyRowsLoaded {
        id: u64,
        rows: Vec<Vec<red_core::Value>>,
    },
    /// A `FetchRun` failed (the error itself is also surfaced via `Error`).
    /// Echoed so the grid can free its in-flight slot — without this a single
    /// failed seek would wedge the run buffer and freeze all further fetching.
    ResultRunFailed {
        epoch: u64,
        seq: u64,
    },
    /// A write/DDL statement committed; `affected` rows changed.
    Executed {
        affected: usize,
    },
    /// A streamed export made progress: `rows` rows written so far (throttled,
    /// not per-row). `id` selects the export's toast.
    ExportProgress {
        id: u64,
        rows: usize,
    },
    /// A streamed export finished: `rows` rows written to `path`. `id` selects the
    /// export's toast.
    ExportFinished {
        id: u64,
        path: String,
        rows: usize,
    },
    /// An in-flight export was cancelled (its partial file removed). `id` selects
    /// the export's toast.
    ExportCancelled {
        id: u64,
    },
    Error(String),
}

/// A header-click sort on a table browse, carried with `OpenResult` so the
/// backend can resolve a composite keyset key (or wrap for the `OFFSET` fallback).
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    /// 1-based output position of the sort column — used for the `OFFSET`-fallback
    /// `ORDER BY <position>` wrap, which orders by position to dodge identifier
    /// quoting (engine-agnostic).
    pub position: usize,
    /// The sort column's name, for resolving the composite `(sort_col, pk)` key.
    pub column: String,
    pub descending: bool,
}

/// One `FetchRun` shape: how to extend or relocate the grid's resident run of
/// a keyset-keyed result. A boundary is the key *tuple* of one row (lead column,
/// then tiebreaker) — one element for a plain browse, two for a sorted browse.
#[derive(Debug, Clone, PartialEq)]
pub enum RunFetch {
    /// Rows strictly after `after`, in sort order. `None` starts from the result's
    /// first row.
    Forward { after: Option<Vec<Value>> },
    /// Rows strictly before `before`, delivered reversed (the grid prepends them
    /// in arrival order, which restores sort order).
    Backward { before: Vec<Value> },
    /// Replace the run near row `ordinal`. When `exact` is `false` (scroll /
    /// scrollbar relocations), an integer key with known bounds is served by a
    /// key-space interpolated seek (`estimated` reply) — fast but approximate.
    /// When `exact` is `true` ("go to row N"), interpolation is skipped and the
    /// row is served precisely (one `OFFSET` page — O(ordinal), but the reply's
    /// ordinals are exact), so the gutter shows the true row number.
    Jump { ordinal: usize, exact: bool },
}
