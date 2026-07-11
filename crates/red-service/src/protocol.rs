//! The wire protocol between the UI and the backend thread: `Command`
//! (UI → service), `Event` (service → UI), and the `RunFetch` shape describing
//! one keyset run-window request. These are the only types that cross the
//! channel; the dispatch loop and handle types live in their own modules.
//!
//! Both channels carry their payload in an **envelope** `(Option<SessionId>, _)`
//! so a message routes to one of several keep-alive sessions without threading a
//! `session` field through every variant (see [`SessionId`]). `None` is for the
//! genuinely session-less messages: a `TestConnection` probe, `Shutdown`, and
//! the `TestSucceeded`/`TestFailed` replies.

use std::path::PathBuf;
use std::time::Duration;

use red_core::kv::{
    CollectionKind, KeyMeta, KvCollectionPage, KvEdit, KvScanPage, KvStreamActionReq, KvStreamPage,
    KvValue, PendingEntry, RespValue, ScanBudget, ScanCursor, StreamAction, StreamConsumer,
    StreamGroup,
};
use red_core::{
    ActivityId, ActivityKind, ActivityStatus, AiLimits, AiTier, Column, ColumnMap, ColumnMeta,
    ColumnStats, ConnectionConfig, CopyMode, EditOp, ExportFormat, FkEdge, FkJoin, ImportFormat,
    KeySpec, LookupRow, PlanStep, QueryOptions, QueryPlan, ResultFilter, RowWindow, SchemaMeta,
    TableDetail, TableRef, UpdateState, Value,
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
    /// Append an SSH jump host's key to `~/.ssh/known_hosts`: the "trust this
    /// host" action behind an [`Event::SshHostUnknown`] failure. Session-less; the
    /// UI re-sends `Connect` after this so the retry verifies against the new entry.
    TrustSshHost {
        host: String,
        port: u16,
        /// OpenSSH-encoded public key, as carried by [`Event::SshHostUnknown`].
        key: String,
    },
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
    /// Describe one object's columns / FKs / indexes; sent lazily on tree expand.
    DescribeTable {
        schema: String,
        table: String,
    },
    /// Load the connection-wide foreign-key graph (Track B7) for FK click-through
    /// and inline FK column expansion. Sent once after connect; replied with
    /// `ForeignKeysLoaded`. A failure is swallowed (the feature degrades to absent),
    /// never surfaced as a toast; FK navigation is an optional enhancement.
    LoadForeignKeys,
    /// Open `sql` as a grid result: count its rows and report column metadata +
    /// the total. The result is then browsed page-by-page via `FetchPage`, or,
    /// when a seek key resolves, run-by-run via `FetchRun`.
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
    ///
    /// `filter` narrows the result (Track B2): the backend wraps `sql` in
    /// `SELECT * FROM (sql) WHERE <predicate>` *before* the count / key-bounds
    /// probe, so the total, the seek key, sort, and export all operate on the
    /// filtered set. The wrap preserves `SELECT *`, so the key column survives and
    /// keyset paging is unaffected. `None` is the unfiltered open.
    ///
    /// `joins` (Track B7, inline FK expansion) decorates a table browse with extra,
    /// dotted-aliased columns pulled from referenced tables: the backend wraps the
    /// (already-filtered) base in `SELECT _red_base.*, <ref cols> FROM (base) AS
    /// _red_base LEFT JOIN …` (see [`DatabaseDriver::fk_join_wrap`]). Base columns
    /// stay first, so their positions/key/sort are unaffected; the unique-target gate
    /// keeps the row count identical, so count/keyset/paging are unchanged. Empty for
    /// an unexpanded browse, editor SQL, or an engine without relational FKs.
    OpenResult {
        sql: String,
        epoch: u64,
        table: Option<(String, String)>,
        sort: Option<SortKey>,
        filter: Option<ResultFilter>,
        joins: Vec<FkJoin>,
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
    /// epoch). Unknown epochs are a no-op. Also closes a `KvFetchScan` browse:
    /// `epoch` is a shared id space (see `InFlight::abort_all`), so this
    /// generically stops an in-flight Redis scan too, not just a SQL result.
    CloseResult {
        epoch: u64,
    },
    /// One page of a Redis keyspace scan (see docs/plans/redis.md's R1):
    /// `SCAN` (looping, budgeted, optionally `MATCH`-filtered) plus a
    /// pipelined metadata fetch, via the session's `KvDriver`. Stateless like
    /// `FetchRun`: `cursor` is whatever `next_cursor` the previous
    /// `KvScanPage` reply carried (`0` to start or restart on a new
    /// `pattern`); the service holds no scan position between calls, the UI's
    /// grid buffer does. `epoch` scopes the reply and supersedes any prior
    /// in-flight scan for the same epoch (a fast-retyped filter cancels the
    /// stale request rather than racing it). Replied with `KvScanPage`, or
    /// the global `Event::Error` on failure (not a SQL connection, or the
    /// engine round-trip itself failed).
    KvFetchScan {
        epoch: u64,
        pattern: Option<String>,
        cursor: ScanCursor,
        budget: ScanBudget,
    },
    /// Exact-key jump (see docs/plans/redis.md): resolve one key's metadata
    /// directly, bypassing `SCAN`. Replied with `KvKeyProbed` carrying
    /// `None` when the key doesn't exist — that's a normal outcome, not an
    /// `Event::Error`.
    KvProbeKey {
        epoch: u64,
        key: String,
    },
    /// The keyspace's total key count (`DBSIZE`), for the unfiltered browse's
    /// header stat. Replied with `KvDbSizeReady`; failures are swallowed
    /// (a missing header stat isn't worth a toast), like `LoadForeignKeys`.
    KvDbSize {
        epoch: u64,
    },
    /// One key's value, for the detail inspector opened by selecting a row in
    /// the keyspace browser. `epoch` is the browse's epoch, used only to
    /// scope the in-flight fetch for cancellation (a newer `KvReadValue`,
    /// `KvReadCollectionPage`, or `KvReadListWindow` supersedes it); staleness
    /// of the *reply* is instead checked UI-side by comparing `key`, since the
    /// inspector can outlive a scan restart. Replied with `KvValueReady`.
    KvReadValue {
        epoch: u64,
        key: String,
    },
    /// One page of a big hash/set/zset's elements, for the inspector's
    /// big-collection sub-grid (see docs/plans/redis.md). Stateless like
    /// `KvFetchScan`: `cursor` is the caller-supplied `next_cursor` from the
    /// previous `KvCollectionPageReady`. Replied with `KvCollectionPageReady`.
    KvReadCollectionPage {
        epoch: u64,
        key: String,
        kind: CollectionKind,
        cursor: u64,
        budget: ScanBudget,
    },
    /// A windowed slice of a big list, from the head or the tail (see
    /// `KvDriver::read_list_window`'s docs on why not an arbitrary offset).
    /// Replied with `KvListWindowReady`.
    KvReadListWindow {
        epoch: u64,
        key: String,
        from_head: bool,
        count: usize,
    },
    /// One page of a big stream's entries, newest-first, for the inspector's
    /// stream view (see docs/plans/redis.md's R4). Streams have no `*SCAN`
    /// cursor, so unlike `KvReadCollectionPage` this pages by entry-ID range:
    /// `before` is the previous `KvStreamPageReady`'s `next_before` (the oldest
    /// ID loaded so far), or `None` to start at the newest entry. Replied with
    /// `KvStreamPageReady`.
    KvReadStreamPage {
        epoch: u64,
        key: String,
        before: Option<String>,
        count: usize,
    },
    /// A stream's consumer groups (`XINFO GROUPS`), for the inspector's
    /// consumer-group management view (see docs/plans/redis.md's "stream
    /// consumer-group management" gap). `epoch` scopes cancellation like the
    /// other inspector reads; the reply's staleness is checked UI-side by
    /// `key`. Replied with `KvStreamGroupsReady`.
    KvStreamGroups {
        epoch: u64,
        key: String,
    },
    /// One group's consumers (`XINFO CONSUMERS`). Replied with
    /// `KvStreamConsumersReady`.
    KvStreamConsumers {
        epoch: u64,
        key: String,
        group: String,
    },
    /// Up to `count` of a group's pending entries (`XPENDING ... - + count`).
    /// Replied with `KvStreamPendingReady`.
    KvStreamPending {
        epoch: u64,
        key: String,
        group: String,
        count: usize,
    },
    /// Acknowledge (`XACK`) or reassign (`XCLAIM`) pending entries in a group,
    /// gated by `read_only` (checked service-side, defense in depth alongside
    /// the driver's own refusal). `Claim` carries the target consumer and a
    /// `min_idle` guard; `Ack` drops the entries from the PEL. Replied with
    /// `KvStreamActionDone` (echoing `key`/`group` so the UI refreshes that
    /// group's consumers+pending), or the global `Event::Error` on failure.
    KvStreamAction {
        epoch: u64,
        key: String,
        group: String,
        action: KvStreamActionReq,
    },
    /// Run an arbitrary command through the console (see
    /// docs/plans/redis.md). `epoch` scopes cancellation only; console
    /// history is UI-side. A server-reported command error (WRONGTYPE, a
    /// bad arity, ...) is normal console output via `KvCommandResult`'s
    /// `RespValue::Error`, not `Event::Error` — that's reserved for a
    /// genuine transport/connection failure or the read-only gate.
    KvCommand {
        epoch: u64,
        argv: Vec<String>,
    },
    /// One in-grid edit (see `red_core::kv::KvEdit`), gated by `read_only`
    /// (checked service-side, defense in depth alongside the driver's own
    /// refusal) and, for a destructive shape, the UI's confirm prompt before
    /// this is ever sent. Replied with `KvEditApplied`, echoing `edit` back,
    /// or the global `Event::Error` on failure.
    KvApplyEdit {
        epoch: u64,
        edit: KvEdit,
    },
    /// Start a live Pub/Sub pattern subscription (see docs/plans/redis.md's
    /// R4). `epoch` identifies this subscription; messages stream back as
    /// `KvMessage` until `CloseResult { epoch }` stops it (the same generic
    /// epoch-scoped teardown every other open Kv/SQL thing uses — see that
    /// command's doc comment).
    KvSubscribe {
        epoch: u64,
        pattern: String,
    },
    /// Compute a column's aggregate summary over the open result's *filtered* SQL
    /// (the column-stats bar): a single `count`/`distinct`/`min`/`max`(/`sum`/`avg`)
    /// pushdown, like `count` but wider. `epoch` selects the open result (its stored,
    /// already-wrapped SQL is reused so the summary matches the visible, filtered
    /// rows); a stale epoch is ignored. `numeric` toggles `sum`/`avg` (decided
    /// UI-side from the column's declared type), `distinct` toggles the potentially
    /// expensive `count(distinct)` (guarded UI-side behind a row threshold). Replied
    /// with `ColumnStatsReady` (or the pane-scoped `ColumnStatsFailed`); cancellable
    /// and epoch-superseded like a page fetch.
    ColumnStats {
        epoch: u64,
        column: String,
        numeric: bool,
        distinct: bool,
    },
    /// Fetch a bounded list of a referenced table's existing ids (+ an optional label
    /// column) for the in-cell foreign-key picker (Track B8). `epoch` scopes the reply
    /// to the still-open result; `target`/`id_column`/`label_column` name the referenced
    /// table and columns (resolved UI-side from the FK graph). Replied with `LookupReady`
    /// (or the pane-scoped `LookupFailed`), keyed by `target` so a result with several FK
    /// columns caches each list separately. Only identifiers reach the SQL; the picker
    /// searches this page client-side. Superseded/cancellable like a page fetch.
    FetchLookup {
        epoch: u64,
        target: TableRef,
        id_column: String,
        label_column: Option<String>,
        limit: usize,
    },
    /// Load the enum-typed columns of `table` and their allowed values (Track B8: the
    /// in-cell enum picker), replied with `EnumsLoaded`. Requested lazily the first time
    /// an editable table's cell is edited; the UI caches the result per table. Empty on
    /// engines without enums. Idempotent and cheap (one catalog query).
    LoadEnums {
        table: TableRef,
    },
    /// Run a non-row-returning statement (write/DDL) in a transaction.
    Execute {
        sql: String,
    },
    /// Apply a batch of guarded, PK-keyed data edits (Track B6) **atomically** on the
    /// active session. The driver renders each `op` to dialect SQL, binds every
    /// value, runs them in one transaction, and asserts each touches exactly one
    /// row (all-or-nothing). `epoch` is the active result's epoch so a reply for a
    /// superseded result (tab switched / re-run) is dropped. Replied with
    /// `BatchApplied` (then the UI patches/refetches) or `BatchFailed` (scoped to the
    /// result pane), never a global error toast.
    ApplyBatch {
        epoch: u64,
        ops: Vec<EditOp>,
    },
    /// Run `EXPLAIN` (or `EXPLAIN ANALYZE` when `analyze`) for `sql` and report a
    /// normalized plan (Track B4). `epoch` is the active tab's result epoch so a
    /// stale reply (tab switched / query re-run) is dropped. Plain explain never
    /// executes the statement; `analyze` does (the UI gates it to read queries).
    Explain {
        sql: String,
        analyze: bool,
        epoch: u64,
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
    /// Stream a CSV/JSONL file at `path` into `target`, projecting each source row's
    /// cells to the target columns per `mapping`, inserting in chunks of `chunk_size`
    /// rows. `id` identifies the import so progress / completion events and a
    /// `CancelImport` route to it. Runs off the dispatch loop (file IO on a blocking
    /// thread), holding at most one chunk in memory. Inserts **commit per chunk**
    /// (v1), so a mid-file failure leaves the earlier rows committed, reported in the
    /// `ImportFailed` event's `rows`.
    Import {
        path: PathBuf,
        format: ImportFormat,
        target: TableRef,
        mapping: Vec<ColumnMap>,
        chunk_size: usize,
        id: u64,
    },
    /// Abort an in-flight import by `id` (the toast's Cancel). Rows committed in
    /// earlier chunks remain.
    CancelImport {
        id: u64,
    },
    /// Describe a copy **target** table's columns (name + declared type) so the UI
    /// can auto-map a source result's columns onto it by name before any write,
    /// the copy's equivalent of `ImportColumns`' file-header peek. The envelope's
    /// [`SessionId`] is the **target** connection (which may differ from the source
    /// for a cross-connection copy). `id` correlates the `CopyTargetColumns` reply;
    /// a describe failure comes back as `CopyFailed`.
    CopyTargetColumns {
        id: u64,
        target: TableRef,
    },
    /// Stream a (filtered/sorted) open result straight into another table: the
    /// table-copy headline. The envelope's [`SessionId`] is the **source** session;
    /// `source_epoch` selects its open result, whose already-wrapped SQL is re-read
    /// at **full fidelity** (never the display cap) through a fresh cursor, so the
    /// copy is byte-exact and includes any `⌘⇧F` filter / sort. `target_session` is
    /// where `target` lives (equal to the source for a same-connection copy, another
    /// open connection for a cross-connection one); both ends are pinned against idle
    /// eviction for the copy's lifetime. `mapping` projects each source column onto a
    /// target column by name; `mode` chooses Append vs Truncate+insert. Runs off the
    /// dispatch loop, one chunk resident, committing per chunk like import. `id`
    /// routes progress / completion events and a `CancelCopy`.
    ///
    /// When `create` is `Some`, the target table is **created first** from that column
    /// shape (types mapped into the target dialect via `red_core::typemap`), before the
    /// rows stream in; this is "copy into a *new* table" / database migration. The
    /// `create` columns mirror the source result's columns; `IF NOT EXISTS` makes it a
    /// no-op if the table already exists. `None` requires the target to pre-exist (the
    /// original same-shape copy).
    CopyToTable {
        id: u64,
        source_epoch: u64,
        target: TableRef,
        target_session: SessionId,
        mapping: Vec<ColumnMap>,
        mode: CopyMode,
        create: Option<Vec<ColumnMeta>>,
    },
    /// Abort an in-flight copy by `id` (the toast's Cancel). Rows committed in
    /// earlier chunks remain (per-chunk commit, like import).
    CancelCopy {
        id: u64,
    },
    /// Migrate **many** tables in one job: the whole-database headline. The
    /// envelope's [`SessionId`] is the **source** session; `source_schema` names the
    /// namespace they live in and `tables` the table names to move. Each is created
    /// on `target_session` under `target_schema` (from the source's column shape, types
    /// mapped into the target dialect) and its rows streamed in, FK-ordered, skipping
    /// any table that already exists on the target (migrate populates a *fresh*
    /// database, never appends into an existing table). Both ends are pinned for the
    /// job's lifetime; it reuses the `Copy*` progress/terminal events and a
    /// `CancelCopy { id }`. One window resident at a time, committing per chunk.
    MigrateTables {
        id: u64,
        source_schema: Option<String>,
        tables: Vec<String>,
        target_session: SessionId,
        target_schema: Option<String>,
    },
    /// Peek a CSV/JSONL file's **source column names** (CSV header / first JSONL
    /// object's keys) without importing, so the UI can build a name-based column
    /// mapping against the target table and preview it before any write. `id`
    /// correlates the reply. Replies `ImportColumns` on success, `ImportFailed` on a
    /// read error. Pure file IO; no session needed.
    ImportColumns {
        path: PathBuf,
        format: ImportFormat,
        id: u64,
    },
    /// Abort the active query / drop its cursor.
    Cancel,
    /// Drop the envelope's session and any cursor; the window returns to a
    /// disconnected state. Other warm sessions are untouched.
    Disconnect,
    /// Drop the envelope's session: the user removed/closed a *background*
    /// connection (vs `Disconnect`, the window's active one going away). Same
    /// effect on the backend; kept distinct so the UI's intent stays legible.
    CloseSession,
    /// Tell the backend which session is foregrounded (`None` = the welcome
    /// screen). The foreground session is exempt from idle eviction; a user can
    /// stare at a result without scrolling and it must stay warm. Global (the
    /// payload, not the envelope, carries the id).
    SetActiveSession(Option<SessionId>),
    /// Set the statement timeout applied to every query and its page/run fetches
    /// (`query.statement_timeout`). `None` disables it. Global (sent at launch and
    /// on each settings reload) so it isn't threaded through every fetch command.
    SetStatementTimeout(Option<Duration>),
    /// Set the driver's display fat-cell cap (`grid.max_cell_chars`), in bytes.
    /// Global; applies to every subsequent display fetch. Export stays full-fidelity.
    SetDisplayCellCap(usize),
    /// (Re)configure the macOS self-updater (poll cadence, enable flag, running
    /// version, repo). Global; sent at launch and on each settings reload, like
    /// the tuning knobs above. Disabling stops all polling and network access.
    ConfigureUpdates(UpdateConfig),
    /// Force an immediate update check ("Check for updates" in the About tab).
    /// Global.
    CheckForUpdate,
    /// (Re)configure the AI assistant provider. Global; sent at launch and on
    /// each settings reload, like the other tuning knobs. An empty `api_key`
    /// leaves the assistant unconfigured (a turn then replies with `AiError`).
    /// The key never touches `settings.toml`; the UI reads it from the OS keyring
    /// and hands it across here.
    ConfigureAi(AiConfig),
    /// Run one assistant turn on the envelope's session. The backend drives the
    /// model → tool → model loop (read-only schema/`SELECT` tools, auto-run and
    /// row-capped) and streams `AiDelta` events, ending with `AiTurnFinished` or
    /// `AiError`. `conversation_id` lets the UI route deltas to the right thread
    /// and cancel a specific turn. `agent` is the id of the agent profile *this*
    /// conversation is bound to (M-S6); turns carry it so several chats on
    /// different agents (API-key, subscription, Codex, local) can run concurrently,
    /// rather than every turn following one global provider. An empty or unknown id
    /// resolves to the default agent / a clear `AiError`.
    AiTurn {
        conversation_id: u64,
        agent: String,
        message: String,
        context: AiContext,
    },
    /// Abort an in-flight assistant turn by `conversation_id` (the panel's Stop).
    AiCancel {
        conversation_id: u64,
    },
    /// Forget all per-conversation backend state when the UI closes or deletes a
    /// conversation: the API-key path's running history/cancel/tool tally and the
    /// subscription path's live agent. Without it those maps grow for the whole
    /// session (a reopened conversation comes back under a fresh id, re-seeded), so
    /// this keeps the backend's memory bounded by what's actually open.
    AiForget {
        conversation_id: u64,
    },
    /// Answer a pending agent tool-permission prompt (M-S2, subscription path).
    /// `allow` runs the tool; otherwise it's denied. Routed to the parked request
    /// by `request_id` so a stale answer for a superseded prompt is dropped.
    AiPermission {
        conversation_id: u64,
        request_id: u64,
        allow: bool,
    },
    /// Start an interactive subscription sign-in (or account switch) for an ACP
    /// agent, driven from Settings. The agent's bundled CLI runs a **paste-code**
    /// OAuth flow: it opens the browser to an authorize URL (relayed as
    /// `AiLoginPrompt`), the user authorizes there, then submits the code the
    /// browser shows via [`Command::AiSubmitLoginCode`]. Ends with `AiLoginFinished`.
    /// A no-op for an API agent (those carry a key, not a login). Red never sees the
    /// OAuth tokens; the CLI owns them.
    AiReauthenticateAgent {
        agent_id: String,
    },
    /// Submit the OAuth code the user pasted from the browser, completing the sign-in
    /// started by [`Command::AiReauthenticateAgent`]. Routed to the in-flight login
    /// by `agent_id`; a stale/duplicate submit is dropped.
    AiSubmitLoginCode {
        agent_id: String,
        code: String,
    },
    /// Abandon an in-flight sign-in (the user dismissed the paste prompt). Kills the
    /// CLI; a no-op if no sign-in is running for `agent_id`.
    AiCancelLogin {
        agent_id: String,
    },
    /// Sign out of an ACP agent's subscription (clears its stored credential via the
    /// bundled CLI), then re-checks status so Settings updates. A no-op for an API
    /// agent.
    AiSignOutAgent {
        agent_id: String,
    },
    /// Ask who is signed in on an ACP agent; answered with `AiAgentAuthStatus`.
    /// Sent when Settings → AI opens and after a sign-in/out. A no-op for an API
    /// agent.
    AiCheckAuthStatus {
        agent_id: String,
    },
    /// Change a session config selector (model / reasoning) on the subscription path.
    /// `config_id`/`value` are the opaque agent identifiers from the advertised
    /// `AiConfigOptionsAvailable`. The agent re-advertises the refreshed set, which
    /// comes back as another `AiConfigOptionsAvailable`. A no-op on the API-key path.
    AiSetConfigOption {
        conversation_id: u64,
        config_id: String,
        value: String,
    },
    Shutdown,
}

/// service → UI. Streamed into the UI's async loop, tagged by the channel
/// envelope with the [`SessionId`] it belongs to (`None` for the session-less
/// `TestSucceeded`/`TestFailed` probe replies) so the UI routes it to the right
/// workspace, including a backgrounded one whose query is still populating.
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
    /// A `Connect` attempt failed. `fatal` is `true` for a user-correctable cause
    /// (bad credentials, missing database) the UI should stop retrying and prompt
    /// to edit; `false` for a transient/network failure that warrants a backoff
    /// retry. Distinct from the generic [`Event::Error`] so the connecting splash
    /// can branch without sniffing the message text.
    ConnectFailed {
        message: String,
        fatal: bool,
    },
    /// A `Connect` failed because the SSH jump host's key isn't in
    /// `~/.ssh/known_hosts`. The UI shows the `fingerprint` and offers "trust &
    /// retry", which sends [`Command::TrustSshHost`] with `key` then reconnects.
    /// Distinct from [`Event::ConnectFailed`] so the splash can offer the action
    /// instead of a dead end.
    SshHostUnknown {
        host: String,
        port: u16,
        fingerprint: String,
        key: String,
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
    /// The connection-wide foreign-key graph (Track B7), in response to
    /// `LoadForeignKeys`. Cached on the connected session for click-through.
    ForeignKeysLoaded {
        graph: Vec<FkEdge>,
    },
    /// One page of a Redis keyspace scan, in response to `KvFetchScan`.
    /// `page.next_cursor`/`page.exhausted` are what the UI echoes back as the
    /// next `KvFetchScan`'s `cursor` (see that command's docs for why the
    /// service holds no scan position itself).
    KvScanPage {
        epoch: u64,
        page: KvScanPage,
    },
    /// One key's metadata, in response to `KvProbeKey`. `meta: None` means
    /// the key doesn't exist — a normal outcome the UI shows inline ("no
    /// such key"), not an `Event::Error`.
    KvKeyProbed {
        epoch: u64,
        key: String,
        meta: Option<KeyMeta>,
    },
    /// The keyspace's total key count, in response to `KvDbSize`.
    KvDbSizeReady {
        epoch: u64,
        count: u64,
    },
    /// One key's value, in response to `KvReadValue`. `value: None` means the
    /// key doesn't exist (it may have been deleted between the browse's
    /// `SCAN` and this fetch).
    KvValueReady {
        epoch: u64,
        key: String,
        value: Option<KvValue>,
    },
    /// One page of a big collection's elements, in response to
    /// `KvReadCollectionPage`.
    KvCollectionPageReady {
        epoch: u64,
        key: String,
        page: KvCollectionPage,
    },
    /// A windowed slice of a big list, in response to `KvReadListWindow`.
    KvListWindowReady {
        epoch: u64,
        key: String,
        from_head: bool,
        values: Vec<String>,
    },
    /// One page of a big stream's entries (newest-first), in response to
    /// `KvReadStreamPage`. `page.next_before`/`page.exhausted` are what the UI
    /// feeds back as the next `KvReadStreamPage`'s `before` to page further
    /// back in time.
    KvStreamPageReady {
        epoch: u64,
        key: String,
        page: KvStreamPage,
    },
    /// A stream's consumer groups, in response to `KvStreamGroups`. `key`
    /// lets the UI drop a reply for a key the inspector has since moved off.
    KvStreamGroupsReady {
        epoch: u64,
        key: String,
        groups: Vec<StreamGroup>,
    },
    /// One group's consumers, in response to `KvStreamConsumers`.
    KvStreamConsumersReady {
        epoch: u64,
        key: String,
        group: String,
        consumers: Vec<StreamConsumer>,
    },
    /// One group's pending entries, in response to `KvStreamPending`.
    KvStreamPendingReady {
        epoch: u64,
        key: String,
        group: String,
        pending: Vec<PendingEntry>,
    },
    /// An `XACK`/`XCLAIM` completed, in response to `KvStreamAction`. `count`
    /// is how many entries it affected; `action` says which verb. The UI
    /// re-requests the group's consumers+pending on this to reflect the change.
    KvStreamActionDone {
        epoch: u64,
        key: String,
        group: String,
        action: StreamAction,
        count: u64,
    },
    /// One console command's result, in response to `KvCommand`. Echoes
    /// `argv` back so a console tracking several in-flight lines can match
    /// the reply to its history entry.
    KvCommandResult {
        epoch: u64,
        argv: Vec<String>,
        result: RespValue,
    },
    /// An in-grid edit succeeded, in response to `KvApplyEdit`. Echoes
    /// `edit` back so the UI can pattern-match what to update locally
    /// (patch the inspector's loaded value, rename/remove a browse row, …)
    /// without a round trip back through `KvReadValue`.
    KvEditApplied {
        epoch: u64,
        edit: KvEdit,
    },
    /// One Pub/Sub message, pushed for as long as the `KvSubscribe { epoch,
    /// .. }` that started it stays open.
    KvMessage {
        epoch: u64,
        channel: String,
        payload: String,
    },
    /// A result opened: its columns and total row count (for `OpenResult`).
    /// Echoes the open `epoch` so the grid can ignore a late reply for a result
    /// it has already replaced. `key` is the seek key the backend resolved for
    /// a table browse: present, the grid pages by keyset runs (`FetchRun`)
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
    /// interpolation; its ordinals are approximate until the run touches a
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
    /// Echoed so the grid can free its in-flight slot; without this a single
    /// failed seek would wedge the run buffer and freeze all further fetching.
    ResultRunFailed {
        epoch: u64,
        seq: u64,
    },
    /// A column's aggregate summary, in response to `ColumnStats`. Echoes `epoch`
    /// and the `column` name so the grid routes it to the right result and column
    /// (a re-sort/re-filter bumps the epoch and supersedes an in-flight summary).
    ColumnStatsReady {
        epoch: u64,
        column: String,
        stats: ColumnStats,
    },
    /// A `ColumnStats` request failed; scoped to the stats bar (shown as "stats
    /// unavailable") rather than a global error toast, like `PlanFailed`.
    ColumnStatsFailed {
        epoch: u64,
        column: String,
    },
    /// A foreign-key lookup list, in response to `FetchLookup`. Echoes `epoch` and the
    /// `target` table so the grid caches it per FK target (dropping a reply for a
    /// superseded epoch). `rows` is the bounded, distinct id/label list.
    LookupReady {
        epoch: u64,
        target: TableRef,
        rows: Vec<LookupRow>,
    },
    /// A `FetchLookup` failed; pane-scoped (the picker just shows no suggestions and
    /// the user types the id), not a global toast, like `ColumnStatsFailed`.
    LookupFailed {
        epoch: u64,
        target: TableRef,
    },
    /// The enum columns of a table, in response to `LoadEnums`: `{ column → [variant,
    /// …] }`, empty for a table with no enum columns. Echoes `table` so the UI caches
    /// it per table. A failure is silent (logged), like a missing FK graph.
    EnumsLoaded {
        table: TableRef,
        columns: std::collections::HashMap<String, Vec<String>>,
    },
    /// A write/DDL statement committed; `affected` rows changed.
    Executed {
        affected: usize,
    },
    /// A guarded edit batch (Track B6) committed on its result's session. Echoes
    /// `epoch` so the UI patches/refetches the right result (and drops a reply for a
    /// superseded one). `applied` is the total ops committed.
    BatchApplied {
        epoch: u64,
        applied: u64,
    },
    /// A guarded edit batch failed (engine error, or an op touched ≠1 row) and the
    /// whole transaction rolled back; nothing changed. `failed_index` is the 0-based
    /// position of the offending op when known, so the UI can point at the staged
    /// change. Scoped to the result pane by `epoch` (shown there, not as a global
    /// toast), like `PlanFailed`.
    BatchFailed {
        epoch: u64,
        failed_index: Option<usize>,
        message: String,
    },
    /// An `Explain` produced a plan. Echoes `epoch` so the UI drops a plan for a
    /// result it has already replaced.
    PlanReady {
        epoch: u64,
        plan: QueryPlan,
    },
    /// An `Explain` failed (bad SQL, unsupported statement). Scoped to the plan
    /// pane by `epoch`; shown there rather than as a global error toast.
    PlanFailed {
        epoch: u64,
        message: String,
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
    /// A streamed import made progress: `rows` rows committed so far (throttled).
    /// `id` selects the import's toast.
    ImportProgress {
        id: u64,
        rows: usize,
    },
    /// A streamed import finished: `rows` rows committed into the target. `id`
    /// selects the import's toast.
    ImportFinished {
        id: u64,
        rows: usize,
    },
    /// An import failed (file open, parse, coercion, or engine error). `rows` rows
    /// committed in earlier chunks remain (per-chunk commit). `id` selects the toast.
    ImportFailed {
        id: u64,
        rows: usize,
        message: String,
    },
    /// An in-flight import was cancelled. Rows committed in earlier chunks remain.
    /// `id` selects the import's toast.
    ImportCancelled {
        id: u64,
        rows: usize,
    },
    /// The source column names from an `ImportColumns` peek. `id` correlates it to
    /// the pending UI request, which builds the name-based mapping + confirm dialog.
    ImportColumns {
        id: u64,
        columns: Vec<String>,
    },
    /// A copy target table's columns (name + declared type), in response to
    /// `CopyTargetColumns`. `id` correlates it to the pending copy; the UI maps the
    /// source result's columns onto these by name and raises the copy confirm.
    CopyTargetColumns {
        id: u64,
        columns: Vec<Column>,
    },
    /// A streamed copy made progress: `rows` rows committed so far (throttled). `id`
    /// selects the copy's transfer toast.
    CopyProgress {
        id: u64,
        rows: usize,
    },
    /// A streamed copy finished: `rows` rows committed into the target. `id` selects
    /// the copy's toast.
    CopyFinished {
        id: u64,
        rows: usize,
    },
    /// A copy failed (target describe, source read, coercion, truncate, or engine
    /// error). `rows` rows committed in earlier chunks remain. `id` selects the toast.
    CopyFailed {
        id: u64,
        rows: usize,
        message: String,
    },
    /// An in-flight copy was cancelled. Rows committed in earlier chunks remain. `id`
    /// selects the copy's toast.
    CopyCancelled {
        id: u64,
        rows: usize,
    },
    /// The self-updater's state changed (Phases 3–4). Global (`None` session);
    /// the UI stores it and renders the titlebar pill + About-tab status from it.
    UpdateState(UpdateState),
    /// A streamed increment of an assistant turn. Echoes `conversation_id` so the
    /// panel appends it to the right thread.
    AiDelta {
        conversation_id: u64,
        delta: AiDelta,
    },
    /// An assistant turn completed normally; `usage` is its token accounting.
    AiTurnFinished {
        conversation_id: u64,
        usage: AiUsage,
    },
    /// An assistant turn failed (no provider, auth, network, refusal, cancel).
    /// Scoped to its conversation so the panel shows it inline, not as a global
    /// toast.
    AiError {
        conversation_id: u64,
        message: String,
    },
    /// The subscription agent wants to run a tool Red didn't auto-allow (M-S2):
    /// the panel shows a confirm prompt and answers with `Command::AiPermission`.
    /// `title` is what the agent intends to do; `detail` is a compact rendering of
    /// the tool's input, if any. Scoped to its conversation, shown inline.
    AiPermissionRequest {
        conversation_id: u64,
        request_id: u64,
        title: String,
        detail: Option<String>,
    },
    /// A `generate_report` tool produced a standalone HTML report at `path`; the UI
    /// surfaces it as a card in the originating conversation (with an "Open" button)
    /// rather than auto-opening it. `title` is the model's report title, if any.
    AiReportReady {
        conversation_id: u64,
        path: String,
        title: Option<String>,
    },
    /// The agent asked to open `sql` in a new query tab (so the user has it in the
    /// editor/grid). The UI opens a fresh tab with the SQL loaded and runs it if it's
    /// a read-only SELECT; anything else is left for the user to run (so the write
    /// path's own confirmation still applies).
    AiOpenQuery {
        conversation_id: u64,
        sql: String,
    },
    /// The agent asked to persist `sql` as a reusable saved query under `name` (with
    /// an optional `description`). The UI writes it to the saved-queries directory so
    /// the user can reopen it later (⇧⌘O); nothing executes.
    AiSaveQuery {
        conversation_id: u64,
        name: String,
        description: Option<String>,
        sql: String,
    },
    /// The subscription agent advertised its slash commands (after its session
    /// opened). Scoped to the conversation; the panel stores them so the composer's
    /// `/`-command picker can offer them. May arrive more than once (the agent can
    /// re-advertise); the latest list replaces the previous.
    AiCommandsAvailable {
        conversation_id: u64,
        commands: Vec<AiCommand>,
    },
    /// The subscription agent advertised (or updated) its session config selectors:
    /// model / reasoning dropdowns. Scoped to the conversation; the panel renders
    /// them next to the Send button. The latest list replaces the previous.
    AiConfigOptionsAvailable {
        conversation_id: u64,
        options: Vec<AiConfigOption>,
    },
    /// An interactive subscription sign-in opened the browser to `url` (paste-code
    /// flow). The UI shows it so the user can open it manually if needed, then enter
    /// the code. Scoped to the agent, not a conversation; sign-in lives in Settings.
    AiLoginPrompt {
        agent_id: String,
        url: String,
    },
    /// An interactive sign-in finished. `ok` true means a credential was stored;
    /// otherwise `message` explains the failure (cancelled, wrong code, timeout).
    AiLoginFinished {
        agent_id: String,
        ok: bool,
        message: String,
    },
    /// Who is signed in on an ACP agent, answering `Command::AiCheckAuthStatus` (and
    /// emitted after a sign-in/out). Drives the identity line in Settings → AI.
    AiAgentAuthStatus {
        agent_id: String,
        status: AiAuthStatus,
    },
    Error(String),
}

/// Which backend executes an agent profile's turns. `Api` is the Claude Messages
/// API path (`red-ai`, optionally at a custom base URL); `Acp` drives an external
/// agent over ACP (`red-acp`): Claude Code on a subscription, Codex, a local agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AiAgentKind {
    #[default]
    Api,
    Acp,
}

/// One configured agent the user can run turns on, resolved UI-side from
/// `settings.toml` (`[[ai.agents]]`, or the synthesized legacy built-ins) plus the
/// per-agent API key read from the OS keyring. The service keys its provider
/// registry by [`id`](Self::id); a turn names that id.
#[derive(Clone, PartialEq)]
pub struct AiAgentProfile {
    /// Stable id: the per-turn selector and keyring account (`ai-key:<id>`).
    pub id: String,
    /// Display name (echoed back to the UI for the selector/header; not used by the
    /// service itself).
    pub name: String,
    /// Which backend runs it.
    pub kind: AiAgentKind,
    /// `Acp`: the agent launch command; empty falls back to the default invocation.
    pub command: String,
    /// `Api`: endpoint override; empty uses the default Anthropic base URL.
    pub base_url: String,
    /// `Api`: model id; empty falls back to the Opus default.
    pub model: String,
    /// `Api`: the API key from the keyring. Empty leaves *this* agent unconfigured
    /// (a turn on it replies with `AiError`). Unused for `Acp` (the agent owns its
    /// own auth).
    pub api_key: String,
}

/// Hand-written so the API key is **never** printed, matching the redacting `Debug`
/// on `ConnectionConfig`/`SshConfig`, so a stray `{profile:?}` (or a debug-log of the
/// command stream carrying an `AiConfig`) can't spill the key into the logs.
impl std::fmt::Debug for AiAgentProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiAgentProfile")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("command", &self.command)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<unset>"
                } else {
                    "<redacted>"
                },
            )
            .finish()
    }
}

/// How the AI assistant is configured, carried by `ConfigureAi`. Built UI-side
/// from `settings.toml` (`[ai]`) plus per-agent API keys read from the OS keyring.
#[derive(Debug, Clone, PartialEq)]
pub struct AiConfig {
    /// The configured agents (always at least one; the legacy built-ins are
    /// synthesized when none are defined). Keyed by id in the service registry.
    pub agents: Vec<AiAgentProfile>,
    /// The id a turn falls back to when it names an empty/unknown agent.
    pub default_agent: String,
    /// Surface a summarized "thinking…" affordance (adaptive thinking).
    pub show_thinking: bool,
    /// The global AI master switch (`[ai] enabled`, M-S7). When `false`, the
    /// service refuses turns and never starts an MCP server or agent: a true kill
    /// switch. A connection's `ai_enabled` override can flip it per session.
    pub enabled: bool,
    /// The global access tier (`[ai] tier`, M-S7) deciding which DB tools the model
    /// is offered. A connection's `ai_tier` override can tighten it per session.
    pub tier: AiTier,
    /// The global resource guards (`[ai.limits]`, M-S7): row cap, statement
    /// timeout, result byte cap, and per-conversation tool-call budget.
    pub limits: AiLimits,
}

/// What's on screen when the user sends a turn, assembled by the UI (it knows the
/// screen; the service knows the model). The service folds this into the system
/// prompt / first user message so the model is grounded in *this* database.
#[derive(Debug, Clone, Default)]
pub struct AiContext {
    /// A compact `table(col type, …)` summary of the connected schema. The model
    /// pulls full detail on demand via the `describe_table` tool, so this stays
    /// small even for large databases.
    pub schema_summary: String,
    /// The currently-viewed tab, so the user can refer to it ("this tab", "the
    /// current query/result"): its name and, at `read` tier, a one-line shape of
    /// the result on screen (row/column counts + column names). The SQL itself rides
    /// in `editor_sql`.
    pub current_tab: Option<String>,
    /// The SQL currently in the editor, if any.
    pub editor_sql: Option<String>,
    /// The last query/result error shown, if any ("Explain this error").
    pub last_error: Option<String>,
    /// A textual snapshot of the selected rows, if any.
    pub selection: Option<String>,
    /// A rendered digest of an earlier, persisted conversation (M-S5), set only on
    /// the first turn after a saved chat is reopened. The backend starts a fresh
    /// session (the agent/history isn't restored), so this folds the prior exchange
    /// back into the prompt as context; the conversation continues coherently
    /// across app restarts on both the API-key and subscription paths.
    pub prior_transcript: Option<String>,
    /// `kind` + database name, for the system prompt's grounding line.
    pub connection: String,
    /// Whether this connection forbids writes, folded into the prompt so the
    /// model doesn't propose edits it can't run.
    pub read_only: bool,
    /// The active Red/Flint theme's colors, so an AI-generated report can match
    /// the app's look (Ayu Dark, GitHub Dark, …) instead of a generic light/dark
    /// document. `None` falls back to the report's built-in light/dark (which
    /// follows the OS). The UI fills it from the live `Theme`; only the
    /// `generate_report` path reads it. Boxed so `AiContext` (which rides in the
    /// `Command::AiTurn` variant) stays small.
    pub theme: Option<Box<ReportTheme>>,
    /// Where `generate_report` writes the finished HTML file (Settings → AI agent →
    /// Report folder). `None` (the default) falls back to the system temp dir. The UI
    /// fills it from the user's setting; the directory is created on demand and, if it
    /// can't be used, the report still lands in the temp dir rather than failing.
    pub report_dir: Option<PathBuf>,
}

/// A snapshot of the active theme's colors as CSS color strings, handed to the
/// report generator so the standalone HTML report (page, tables, charts, filter
/// controls) is painted in Red's current palette. UI-agnostic on this side: the
/// UI converts its `Hsla` tokens to CSS; the report shell + chart/table renderer
/// just substitute them.
#[derive(Debug, Clone)]
pub struct ReportTheme {
    /// Dark vs light, so the renderer picks matching shadows / `color-scheme`.
    pub is_dark: bool,
    /// Page background (the app's main surface).
    pub bg: String,
    /// Card / elevated surface (chart cards, table header, filter bar).
    pub surface: String,
    /// Primary text.
    pub fg: String,
    /// Secondary / muted text (axis ticks, counts, labels).
    pub muted: String,
    /// Hairline borders.
    pub border: String,
    /// Faint grid lines.
    pub grid: String,
    /// Hover / zebra background.
    pub hover: String,
    /// Brand accent (primary series, focus rings, links).
    pub accent: String,
    /// Translucent accent for focus-ring glow.
    pub ring: String,
    /// Categorical chart palette pulled from the theme's semantic colors.
    pub palette: Vec<String>,
}

/// One streamed increment of an assistant turn (the `Event::AiDelta` payload).
/// Text and thinking append to the bubble; the activity variants build and update
/// the turn's persisted activity timeline (tool calls, subagents, writes) by id.
#[derive(Debug, Clone)]
pub enum AiDelta {
    /// A chunk of summarized thinking text.
    Thinking(String),
    /// A chunk of visible answer text.
    Text(String),
    /// A new activity node opened (a tool call, subagent, or write). `parent` nests
    /// it under an existing node — a subagent's inner tool calls carry the
    /// subagent's id — or is `None` for a top-level node. The node arrives in
    /// `Running`/`Pending` and is later resolved by `ActivityUpdated`.
    ActivityStarted {
        id: ActivityId,
        parent: Option<ActivityId>,
        kind: ActivityKind,
        status: ActivityStatus,
    },
    /// An open activity node changed state and/or gained a one-line result summary,
    /// matched by `id` anywhere in the tree. `status` is `None` for a detail-only
    /// refresh (e.g. streamed subagent progress) that shouldn't change the lifecycle.
    ActivityUpdated {
        id: ActivityId,
        status: Option<ActivityStatus>,
        detail: Option<String>,
    },
    /// The agent (re)published its plan checklist; replaces the turn's steps.
    PlanUpdated { steps: Vec<PlanStep> },
}

/// One slash command the assistant backend advertises (the `AiCommandsAvailable`
/// payload). Subscription (ACP) only: the agent lists them after its session opens;
/// the composer offers them through a `/`-triggered picker. `name` carries no
/// leading slash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiCommand {
    pub name: String,
    pub description: String,
}

/// Who (if anyone) is signed in on a subscription (ACP) agent, the
/// `AiAgentAuthStatus` payload, surfaced in Settings → AI. Resolved by asking the
/// agent's bundled CLI; Red never sees the OAuth tokens, only this summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AiAuthStatus {
    pub logged_in: bool,
    /// The signed-in account's email, when known.
    pub email: Option<String>,
    /// The Claude subscription tier (e.g. `"max"`, `"pro"`), when on claude.ai auth.
    pub subscription: Option<String>,
    /// How the agent is authenticated (e.g. `"claude.ai"`, `"console"`).
    pub method: Option<String>,
}

/// One session config selector the subscription agent advertises (the
/// `AiConfigOptionsAvailable` payload): a model or reasoning-level dropdown. The
/// `id`/`value` strings are opaque agent identifiers round-tripped via
/// `Command::AiSetConfigOption`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiConfigOption {
    pub id: String,
    pub name: String,
    pub category: AiConfigCategory,
    /// The currently-selected choice's `value`.
    pub current_value: String,
    pub choices: Vec<AiConfigChoice>,
}

/// One choice within an [`AiConfigOption`] dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiConfigChoice {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

/// What an [`AiConfigOption`] controls; it drives where the composer places it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiConfigCategory {
    Model,
    Reasoning,
    Mode,
    Other,
}

/// Token accounting for one assistant turn (the `AiTurnFinished` payload). The
/// subscription (ACP) path reports cumulative session figures and, when the agent
/// provides it, a running `cost_usd`; the API-key path reports per-turn tokens and
/// no cost. The panel renders whichever fields are non-zero/present.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AiUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    /// Running session cost in USD, when the backend reports it (subscription
    /// path). `None` on the API-key path, which prices nothing.
    pub cost_usd: Option<f64>,
}

/// How the self-updater should behave, carried by `ConfigureUpdates`. Built
/// UI-side from `settings.toml` + the running build's `CARGO_PKG_VERSION` (the
/// app knows its own version; the service doesn't).
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateConfig {
    /// `false` (`auto_update = false`) short-circuits the updater entirely: no
    /// timer, no network.
    pub enabled: bool,
    /// The GitHub `owner/repo` to poll, e.g. `vojir-mikulas/red`.
    pub repo: String,
    /// The running build's version, for the semver "am I behind?" compare.
    pub current_version: String,
    /// Poll cadence (startup, then every interval).
    pub interval: Duration,
}

/// A header-click sort on a table browse, carried with `OpenResult` so the
/// backend can resolve a composite keyset key (or wrap for the `OFFSET` fallback).
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    /// 1-based output position of the sort column, used for the `OFFSET`-fallback
    /// `ORDER BY <position>` wrap, which orders by position to dodge identifier
    /// quoting (engine-agnostic).
    pub position: usize,
    /// The sort column's name, for resolving the composite `(sort_col, pk)` key.
    pub column: String,
    pub descending: bool,
}

/// One `FetchRun` shape: how to extend or relocate the grid's resident run of
/// a keyset-keyed result. A boundary is the key *tuple* of one row (lead column,
/// then tiebreaker): one element for a plain browse, two for a sorted browse.
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
    /// key-space interpolated seek (`estimated` reply), which is fast but approximate.
    /// When `exact` is `true` ("go to row N"), interpolation is skipped and the
    /// row is served precisely (one `OFFSET` page; O(ordinal), but the reply's
    /// ordinals are exact), so the gutter shows the true row number.
    Jump { ordinal: usize, exact: bool },
}
