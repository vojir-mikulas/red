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
    AiLimits, AiTier, Column, ConnectionConfig, EditOp, ExportFormat, KeySpec, QueryOptions,
    QueryPlan, ResultFilter, RowWindow, SchemaMeta, TableDetail, UpdateState, Value,
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
    /// Append an SSH jump host's key to `~/.ssh/known_hosts` — the "trust this
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
    ///
    /// `filter` narrows the result (Track B2): the backend wraps `sql` in
    /// `SELECT * FROM (sql) WHERE <predicate>` *before* the count / key-bounds
    /// probe, so the total, the seek key, sort, and export all operate on the
    /// filtered set. The wrap preserves `SELECT *`, so the key column survives and
    /// keyset paging is unaffected. `None` is the unfiltered open.
    OpenResult {
        sql: String,
        epoch: u64,
        table: Option<(String, String)>,
        sort: Option<SortKey>,
        filter: Option<ResultFilter>,
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
    /// Apply a batch of guarded, PK-keyed data edits (Track B6) **atomically** on the
    /// active session. The driver renders each `op` to dialect SQL, binds every
    /// value, runs them in one transaction, and asserts each touches exactly one row
    /// — all-or-nothing. `epoch` is the active result's epoch so a reply for a
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
    /// executes the statement; `analyze` does — the UI gates it to read queries.
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
    /// Set the statement timeout applied to every query and its page/run fetches
    /// (`query.statement_timeout`). `None` disables it. Global — sent at launch and
    /// on each settings reload — so it isn't threaded through every fetch command.
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
    /// (Re)configure the AI assistant provider. Global — sent at launch and on
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
    /// conversation is bound to (M-S6) — turns carry it so several chats on
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
    /// Re-authenticate / switch account for an ACP agent, driven from Settings
    /// (M-S4). The agent owns `/login`, so Red can't drive it directly — instead it
    /// spawns the agent and runs a fresh ACP handshake, which pops the agent's own
    /// browser login when it isn't signed in, then drops the probe and forces idle
    /// conversations to re-handshake so they pick up the new account. A no-op for an
    /// API agent. Red never touches the subscription tokens.
    AiReauthenticateAgent {
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
    /// A guarded edit batch (Track B6) committed on its result's session. Echoes
    /// `epoch` so the UI patches/refetches the right result (and drops a reply for a
    /// superseded one). `applied` is the total ops committed.
    BatchApplied {
        epoch: u64,
        applied: u64,
    },
    /// A guarded edit batch failed (engine error, or an op touched ≠1 row) and the
    /// whole transaction rolled back — nothing changed. `failed_index` is the 0-based
    /// position of the offending op when known, so the UI can point at the staged
    /// change. Scoped to the result pane by `epoch` — shown there, not as a global
    /// toast — like `PlanFailed`.
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
    /// pane by `epoch` — shown there rather than as a global error toast.
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
    /// The self-updater's state changed (Phases 3–4). Global (`None` session) —
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
    /// opens it in the system browser. Scoped to its conversation so the originating
    /// chat (sidebar or agent tab) could also note it.
    AiReportReady {
        conversation_id: u64,
        path: String,
    },
    /// The agent asked to open `sql` in a new query tab (so the user has it in the
    /// editor/grid). The UI opens a fresh tab with the SQL loaded and runs it if it's
    /// a read-only SELECT; anything else is left for the user to run (so the write
    /// path's own confirmation still applies).
    AiOpenQuery {
        conversation_id: u64,
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
    /// The subscription agent advertised (or updated) its session config selectors —
    /// model / reasoning dropdowns. Scoped to the conversation; the panel renders
    /// them next to the Send button. The latest list replaces the previous.
    AiConfigOptionsAvailable {
        conversation_id: u64,
        options: Vec<AiConfigOption>,
    },
    Error(String),
}

/// Which backend executes an agent profile's turns. `Api` is the Claude Messages
/// API path (`red-ai`, optionally at a custom base URL); `Acp` drives an external
/// agent over ACP (`red-acp`) — Claude Code on a subscription, Codex, a local agent.
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
#[derive(Debug, Clone, PartialEq)]
pub struct AiAgentProfile {
    /// Stable id — the per-turn selector and keyring account (`ai-key:<id>`).
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

/// How the AI assistant is configured, carried by `ConfigureAi`. Built UI-side
/// from `settings.toml` (`[ai]`) plus per-agent API keys read from the OS keyring.
#[derive(Debug, Clone, PartialEq)]
pub struct AiConfig {
    /// The configured agents (always at least one — the legacy built-ins are
    /// synthesized when none are defined). Keyed by id in the service registry.
    pub agents: Vec<AiAgentProfile>,
    /// The id a turn falls back to when it names an empty/unknown agent.
    pub default_agent: String,
    /// Surface a summarized "thinking…" affordance (adaptive thinking).
    pub show_thinking: bool,
    /// The global AI master switch (`[ai] enabled`, M-S7). When `false`, the
    /// service refuses turns and never starts an MCP server or agent — a true kill
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
    /// current query/result"): its name and — at `read` tier — a one-line shape of
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
    /// back into the prompt as context — the conversation continues coherently
    /// across app restarts on both the API-key and subscription paths.
    pub prior_transcript: Option<String>,
    /// `kind` + database name, for the system prompt's grounding line.
    pub connection: String,
    /// Whether this connection forbids writes — folded into the prompt so the
    /// model doesn't propose edits it can't run.
    pub read_only: bool,
}

/// One streamed increment of an assistant turn (the `Event::AiDelta` payload).
#[derive(Debug, Clone)]
pub enum AiDelta {
    /// A chunk of summarized thinking text.
    Thinking(String),
    /// A chunk of visible answer text.
    Text(String),
    /// The model began running a read-only tool (shown as a transient status).
    ToolStarted { name: String },
    /// A tool finished; `ok` is false when it errored.
    ToolFinished { name: String, ok: bool },
}

/// One slash command the assistant backend advertises (the `AiCommandsAvailable`
/// payload). Subscription (ACP) only — the agent lists them after its session opens;
/// the composer offers them through a `/`-triggered picker. `name` carries no
/// leading slash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiCommand {
    pub name: String,
    pub description: String,
}

/// One session config selector the subscription agent advertises (the
/// `AiConfigOptionsAvailable` payload) — a model or reasoning-level dropdown. The
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

/// What an [`AiConfigOption`] controls — drives where the composer places it.
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
    /// `false` (`auto_update = false`) short-circuits the updater entirely — no
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
