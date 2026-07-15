//! Shared domain types for RED. No UI, no async runtime, no driver knowledge:
//! the protocol/service/UI layers all speak in these types. Mirrors the role of
//! `nyx-core` in the Nyx codebase.

use std::fmt;
use std::sync::Arc;

pub mod diff;
pub mod doc;
pub mod kv;
pub mod typemap;

/// Which database engine a connection targets. Drives driver selection and,
/// via [`DbKind::all`]/the metadata accessors, how the connection form renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DbKind {
    Postgres,
    #[default]
    Sqlite,
    Mysql,
    /// ClickHouse: the OLAP engine, reached over its HTTP interface. A writable
    /// connection can be an INSERT / copy / migration *target*, but has no in-grid
    /// editing: `UPDATE`/`DELETE` are async `ALTER … UPDATE` mutations with no
    /// transaction/rollback over a non-unique sort key, so its driver refuses
    /// `apply_edits` (see [`DbKind::write_caps`]).
    Clickhouse,
    /// Redis/Valkey: a key-value store, not SQL-shaped at all. Reached through
    /// the parallel `KvDriver` seam (`red-driver`'s `redis_kv` module), not
    /// `DatabaseDriver` — see `docs/plans/redis.md`. Read-only in R0/R1;
    /// `write_caps` reflects that until R3 lands in-grid editing.
    Redis,
    /// MongoDB: a document store, neither SQL- nor Redis-shaped. Reached through
    /// the third `DocDriver` seam (`red-driver`'s `doc` module), a
    /// `server → databases → collections → documents` hierarchy of nested BSON
    /// trees — see `docs/plans/todo/doc-driver.md`. Writes ride the seam, not the
    /// SQL `WriteCaps` set, so `write_caps` stays all-`false` like Redis.
    Mongo,
}

impl DbKind {
    /// Every engine, in the order the connection form lists them. Adding a driver
    /// means adding a variant here and an arm to each accessor below; the form is
    /// data-driven off these, not a hand-maintained list.
    pub const fn all() -> &'static [DbKind] {
        &[
            DbKind::Postgres,
            DbKind::Sqlite,
            DbKind::Mysql,
            DbKind::Clickhouse,
            DbKind::Redis,
            DbKind::Mongo,
        ]
    }

    /// File-based engines (SQLite) take a single path, not host/port/user/pass.
    pub const fn is_file(self) -> bool {
        matches!(self, DbKind::Sqlite)
    }

    /// The conventional default port, or `None` for file engines. ClickHouse's
    /// default is its HTTP interface port (`8123`), which the driver speaks.
    pub const fn default_port(self) -> Option<u16> {
        match self {
            DbKind::Postgres => Some(5432),
            DbKind::Mysql => Some(3306),
            DbKind::Clickhouse => Some(8123),
            DbKind::Redis => Some(6379),
            DbKind::Mongo => Some(27017),
            DbKind::Sqlite => None,
        }
    }

    /// The URL scheme used when composing a connection string for the driver.
    pub const fn url_scheme(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgres",
            DbKind::Mysql => "mysql",
            DbKind::Sqlite => "sqlite",
            DbKind::Clickhouse => "clickhouse",
            DbKind::Redis => "redis",
            DbKind::Mongo => "mongodb",
        }
    }

    /// Map a connection-string scheme (`postgresql`, `mariadb`, `file`, …) onto an
    /// engine, for parsing a pasted DSN.
    pub fn from_scheme(scheme: &str) -> Option<DbKind> {
        match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Some(DbKind::Postgres),
            "mysql" | "mariadb" => Some(DbKind::Mysql),
            "sqlite" | "sqlite3" | "file" => Some(DbKind::Sqlite),
            "clickhouse" | "clickhouse-http" | "ch" | "clickhouses" => Some(DbKind::Clickhouse),
            // The TLS schemes (`rediss`/`clickhouses`) parse to the same engine;
            // `parse_conn_str` reads the `tls` bit from the scheme separately.
            "redis" | "rediss" => Some(DbKind::Redis),
            // Both the plain and the SRV/DNS-seedlist spelling map to one engine;
            // `mongodb+srv` implies TLS, which the driver reads off the scheme.
            "mongodb" | "mongodb+srv" => Some(DbKind::Mongo),
            _ => None,
        }
    }

    /// Whether a URL scheme denotes a TLS connection (`rediss`/`clickhouses`),
    /// so a pasted secure DSN pre-checks the form's TLS toggle.
    fn scheme_is_tls(scheme: &str) -> bool {
        matches!(
            scheme.to_ascii_lowercase().as_str(),
            "rediss" | "clickhouses"
        )
    }

    /// What write operations this engine can honor, *independent* of the
    /// connection's `read_only` flag (a read-only connection refuses every write
    /// regardless). Both the UI (which only holds a [`DbKind`] + `read_only`, never
    /// the driver) and the driver consult this so the affordances they offer and the
    /// operations they'll actually run agree, replacing the scattered
    /// `== DbKind::Clickhouse` checks that used to disagree with the read-only gate.
    pub const fn write_caps(self) -> WriteCaps {
        match self {
            // The relational engines: transactional, PK-guarded in-grid editing and
            // bulk insert / copy-and-migrate target.
            DbKind::Postgres | DbKind::Sqlite | DbKind::Mysql => WriteCaps {
                insert: true,
                guarded_edit: true,
                best_effort_edit: false,
            },
            // ClickHouse (OLAP): a writable connection can be an INSERT / copy /
            // migration *target*, but has no transactional, exactly-one-row in-grid
            // editing; its `UPDATE`/`DELETE` are asynchronous, non-atomic mutations
            // over a non-unique sort key (a best-effort edit mode is a later phase).
            DbKind::Clickhouse => WriteCaps {
                insert: true,
                guarded_edit: false,
                best_effort_edit: false,
            },
            // Redis: no write path exists yet (R0/R1 are read-only browsing).
            // R3 (see docs/plans/redis.md) adds SET/HSET/EXPIRE/DEL through the
            // KvDriver seam, not through this SQL-shaped capability set at all —
            // this stays all-`false` so any UI affordance still gated on
            // `write_caps` (rather than the connection kind) stays hidden.
            DbKind::Redis => WriteCaps {
                insert: false,
                guarded_edit: false,
                best_effort_edit: false,
            },
            // MongoDB: like Redis, every write rides its own seam (`DocDriver`),
            // not this SQL capability set, so any affordance gated on `write_caps`
            // (rather than the connection kind) stays hidden. See
            // `docs/plans/todo/doc-driver.md`.
            DbKind::Mongo => WriteCaps {
                insert: false,
                guarded_edit: false,
                best_effort_edit: false,
            },
        }
    }
}

/// Whether a DSN string requests TLS — by scheme (`rediss`/`clickhouses`) or a
/// recognized query flag (`sslmode=<anything but disable>`, `require_ssl=true`,
/// `ssl=true`, `tls=true`). The single source of truth for "is this a TLS DSN",
/// shared by [`ConnectionConfig::parse_conn_str`] and the engine drivers'
/// cleartext-refusal guard, so the two can't disagree on which spellings count
/// (a driver matching only `sslmode=require` would silently connect in
/// cleartext for `?ssl=true` or `?sslmode=prefer`).
pub fn dsn_requests_tls(dsn: &str) -> bool {
    let Some((scheme, rest)) = dsn.trim().split_once("://") else {
        return false;
    };
    if DbKind::scheme_is_tls(scheme) {
        return true;
    }
    let q = rest
        .split(['?', '#'])
        .nth(1)
        .unwrap_or("")
        .to_ascii_lowercase();
    (q.contains("sslmode=") && !q.contains("sslmode=disable"))
        || q.contains("require_ssl=true")
        || q.contains("ssl=true")
        || q.contains("tls=true")
}

/// The write operations an engine can honor, independent of a connection's
/// `read_only` flag (see [`DbKind::write_caps`]). A cheap `Copy` descriptor the UI
/// reads to gate the edit affordances and the copy/migration target pickers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteCaps {
    /// Bulk `INSERT` / data import / copy-and-migrate *target* is possible.
    pub insert: bool,
    /// Transactional, guarded, exactly-one-row in-grid `UPDATE`/`DELETE` (the
    /// relational edit contract) is possible.
    pub guarded_edit: bool,
    /// Best-effort, non-atomic in-grid `UPDATE`/`DELETE` (async mutations, no
    /// rollback, no one-row guarantee) is possible; reserved for a later phase.
    pub best_effort_edit: bool,
}

impl fmt::Display for DbKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbKind::Sqlite => write!(f, "SQLite"),
            DbKind::Postgres => write!(f, "PostgreSQL"),
            DbKind::Mysql => write!(f, "MySQL/MariaDB"),
            DbKind::Clickhouse => write!(f, "ClickHouse"),
            DbKind::Redis => write!(f, "Redis"),
            DbKind::Mongo => write!(f, "MongoDB"),
        }
    }
}

/// How RED authenticates to an SSH jump host. Mirrors DataGrip's three modes: a
/// running ssh-agent, a password, or an OpenSSH private-key file (optionally
/// passphrase-protected). The secrets these need (the password, the key
/// passphrase) are **never** serialized; like a connection's DB password they
/// live in the OS keychain and are materialized only transiently.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "mode", rename_all = "snake_case"))]
pub enum SshAuth {
    /// Reuse a running agent (ssh-agent / Pageant). Carries no secret of its own.
    #[default]
    Agent,
    /// Password authentication; the password lives in the keychain.
    Password,
    /// An OpenSSH private key at `path`, decrypted with a keychain-stored
    /// passphrase when the key is encrypted.
    Key { path: String },
}

/// Reach the database *through* an SSH jump host (the `ssh -L` model). When a
/// [`ConnectionConfig`] carries one of these, the service opens a local port
/// forward and points the driver at it; the connection's `host`/`port` are the
/// target as seen *from the jump host*. Secrets (`password`, `passphrase`) are
/// keychain-backed and never persisted, hence the redacting `Debug` and the
/// `serde(skip)` on those fields.
#[derive(Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    /// Password for [`SshAuth::Password`]; empty otherwise. Keychain-backed,
    /// never serialized.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub password: String,
    /// Passphrase for an encrypted [`SshAuth::Key`]; empty otherwise.
    /// Keychain-backed, never serialized.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub passphrase: String,
}

impl fmt::Debug for SshConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("auth", &self.auth)
            .field("password", &redact(&self.password))
            .field("passphrase", &redact(&self.passphrase))
            .finish()
    }
}

/// The kind of proxy a [`ProxyConfig`] speaks: a SOCKS5 proxy (RFC 1928, with
/// optional RFC 1929 user/pass auth) or an HTTP `CONNECT` proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ProxyKind {
    #[default]
    Socks5,
    HttpConnect,
}

impl ProxyKind {
    /// Every kind, in the order a picker lists them.
    pub const fn all() -> &'static [ProxyKind] {
        &[ProxyKind::Socks5, ProxyKind::HttpConnect]
    }

    /// The picker label for this kind.
    pub const fn label(self) -> &'static str {
        match self {
            ProxyKind::Socks5 => "SOCKS5",
            ProxyKind::HttpConnect => "HTTP CONNECT",
        }
    }
}

/// Reach the database *through* a forward proxy (SOCKS5 or HTTP `CONNECT`). Like
/// [`SshConfig`], when a [`ConnectionConfig`] carries one of these the service
/// resolves a local forward and points the driver at `127.0.0.1:<port>`; the
/// connection's `host`/`port` are the target reached *through the proxy*. A simpler
/// forward than SSH: no host-key dance, just the proxy handshake. The auth
/// `password` is keychain-backed and never persisted, hence the redacting `Debug`
/// and the `serde(skip)`.
#[derive(Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProxyConfig {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    /// Optional proxy-auth username; empty means unauthenticated.
    pub user: String,
    /// Proxy-auth password; empty when unauthenticated. Keychain-backed, never
    /// serialized.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub password: String,
}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &redact(&self.password))
            .finish()
    }
}

/// How much of the connected database the AI assistant's tools may reach. The
/// tier is enforced where the MCP tool catalog is *constructed*: a tool above
/// the tier simply isn't offered, so the model (on either the API-key or the
/// subscription/ACP backend) can't call something the tier withholds. It is a
/// least-privilege ladder: each rung adds to the one below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum AiTier {
    /// No DB tools at all; the assistant chats without database grounding.
    Off,
    /// Structure only: `list_schema` + `describe_table`. The model sees tables,
    /// columns, types, and keys but never reads a row of data.
    Schema,
    /// The full read catalog: adds `run_select` and `explain`, subject to the
    /// resource guards in [`AiLimits`]. Honors a connection's `read_only` posture.
    /// The shipped default.
    #[default]
    Read,
    /// Read **plus** the gated write tool (`propose_write`): a single
    /// INSERT/UPDATE/DELETE that requires explicit, per-statement user approval and
    /// is blocked on a read-only connection and for destructive shapes (DDL,
    /// unqualified UPDATE/DELETE). Opt-in only: set globally (`[ai] tier = "write"`)
    /// or per-connection (`ai_tier = "write"`); never a default.
    Write,
}

impl AiTier {
    /// Whether the named tool is exposed at this tier. The single source of truth
    /// for catalog membership, so both backends gate identically.
    pub fn allows_tool(self, tool: &str) -> bool {
        match self {
            AiTier::Off => false,
            // Structure-only tools, both backends. For Redis (KV) these read
            // metadata (types/TTL/sizes/server info) but never a key's value.
            AiTier::Schema => matches!(
                tool,
                "list_schema"
                    | "describe_table"
                    | "kv_server_info"
                    | "kv_scan_keys"
                    | "kv_key_info"
                    // MongoDB (doc) metadata tools: catalog + inferred schema, no
                    // document reads.
                    | "doc_server_info"
                    | "list_collections"
                    | "describe_collection"
            ),
            AiTier::Read => matches!(
                tool,
                "list_schema"
                    | "describe_table"
                    | "run_select"
                    | "explain"
                    | "profile_table"
                    | "generate_report"
                    | "open_query"
                    | "save_query"
                    // Delegation grants no new capability — a subagent runs a
                    // read-only subset of this same tier — so it rides with Read.
                    | "spawn_subagent"
                    // Redis (KV) read tools: the metadata tools from Schema plus
                    // value/sample/diagnostic reads. All read-only.
                    | "kv_server_info"
                    | "kv_scan_keys"
                    | "kv_key_info"
                    | "kv_get_value"
                    | "kv_biggest_keys"
                    | "kv_analyze"
                    | "kv_slowlog"
                    | "kv_config_get"
                    // MongoDB (doc) read tools: the metadata tools from Schema plus
                    // document/sample/analytical reads. All read-only.
                    | "doc_server_info"
                    | "list_collections"
                    | "describe_collection"
                    | "profile_collection"
                    | "sample_documents"
                    | "find"
                    | "aggregate"
                    | "count"
                    | "distinct"
                    | "explain_query"
                    | "index_advice"
                    | "audit_collection"
            ),
            // Write inherits the full read catalog and adds the gated write tools:
            // SQL (a single statement or a transactional changeset) and Redis
            // (gated key mutations). Every one rides the per-call approval gate.
            AiTier::Write => {
                matches!(
                    tool,
                    "propose_write"
                        | "propose_changeset"
                        | "kv_expire"
                        | "kv_delete"
                        | "kv_rename"
                        | "kv_config_set"
                        // MongoDB (doc) gated writes.
                        | "propose_doc_write"
                        | "propose_index"
                        | "propose_collection_op"
                ) || AiTier::Read.allows_tool(tool)
            }
        }
    }

    /// A short label for status lines and logs.
    pub fn label(self) -> &'static str {
        match self {
            AiTier::Off => "off",
            AiTier::Schema => "schema",
            AiTier::Read => "read",
            AiTier::Write => "write",
        }
    }

    /// Parse a settings string. Recognized tiers map directly; an **unrecognized**
    /// value fails **closed** to [`AiTier::Off`] rather than to a permissive tier;
    /// a typo like `"readonly"` or `"scema"` when locking the assistant down must
    /// not silently grant row-data (`read`) access. `write` is accepted both
    /// globally (`[ai] tier`) and per-connection (`ai_tier`); it only ever grants
    /// the write tool on a writable connection, gated by approval.
    pub fn parse(s: &str) -> AiTier {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => AiTier::Off,
            "schema" => AiTier::Schema,
            "read" => AiTier::Read,
            "write" => AiTier::Write,
            _ => AiTier::Off,
        }
    }
}

/// Resource guards on the `read` tier: defense in depth so neither backend can
/// make the assistant read 1M rows by accident or hang the session thread. They
/// are enforced server-side in the tool layer, mirroring the windowed-cursor and
/// fat-cell caps the human-facing paths already carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AiLimits {
    /// Hard ceiling on rows one `run_select` may return; a larger requested
    /// `LIMIT` is clamped and the truncation reported back to the model.
    pub max_rows: usize,
    /// Per-tool-call statement timeout. `0` disables it.
    pub statement_timeout_ms: u64,
    /// Cap on the size of one tool result handed back to the model; a larger
    /// result is truncated (and the truncation noted) so context can't balloon.
    pub max_result_bytes: usize,
    /// Cap on tool calls per conversation, bounding a runaway agent loop. `0`
    /// disables the cap.
    pub max_tool_calls: usize,
}

impl Default for AiLimits {
    fn default() -> Self {
        Self {
            max_rows: 1000,
            statement_timeout_ms: 15_000,
            max_result_bytes: 256 * 1024,
            max_tool_calls: 100,
        }
    }
}

/// The resolved AI access policy for one turn: the master switch, the access
/// tier, and the resource guards. Built by layering a connection's optional
/// overrides over the global `[ai]` settings (see [`Self::with_overrides`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AiPolicy {
    /// The master switch. When `false` the assistant is a true no-op: no panel
    /// entry, no tools, and, critically, no MCP server or agent process.
    pub enabled: bool,
    pub tier: AiTier,
    pub limits: AiLimits,
    /// The connection's read-only posture, carried into the tool layer so the write
    /// tool is withheld (and rejected, defense in depth) on a read-only connection;
    /// the same guard the human write path is held to. Authoritative (set from the
    /// session), not the UI-supplied `AiContext.read_only`.
    pub read_only: bool,
}

impl Default for AiPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            tier: AiTier::Read,
            limits: AiLimits::default(),
            read_only: false,
        }
    }
}

impl AiPolicy {
    /// Layer a connection's optional overrides over this (global) policy. A set
    /// override tightens (or loosens) the field for that connection; an unset one
    /// inherits the global value, so an unconfigured connection gets the global
    /// default, and a sensitive one can be pinned `off`/`schema` without touching
    /// the global setting.
    pub fn with_overrides(self, enabled: Option<bool>, tier: Option<AiTier>) -> AiPolicy {
        AiPolicy {
            enabled: enabled.unwrap_or(self.enabled),
            tier: tier.unwrap_or(self.tier),
            limits: self.limits,
            read_only: self.read_only,
        }
    }
}

/// A stable id for a node in an assistant turn's activity timeline. On the
/// direct-provider path it is the provider's `tool_use` id; on the ACP path it is
/// the agent's `tool_call_id`. Opaque; only used to correlate a node's start with
/// its later status updates and to attach children to their parent. A newtype (not
/// a bare `String`) so it can't be transposed with the other string ids that flow
/// through the delta stream; `serde(transparent)` keeps its persisted form a plain
/// string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct ActivityId(String);

impl ActivityId {
    /// Borrow the underlying id string (for UI element keys, formatting, matching).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ActivityId {
    fn from(s: String) -> Self {
        ActivityId(s)
    }
}

impl From<&str> for ActivityId {
    fn from(s: &str) -> Self {
        ActivityId(s.to_string())
    }
}

impl std::fmt::Display for ActivityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What one [`ActivityNode`] represents. The agent's work is a tree of these:
/// tool calls, delegated subagents, and proposed writes. New kinds extend the
/// agent's legible surface without touching the delta plumbing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "kind", rename_all = "snake_case"))]
pub enum ActivityKind {
    /// A tool call: the tool `name` plus a one-line summary of its arguments (the
    /// SQL's first line, the table name), shown so the trace reads without expanding.
    Tool {
        name: String,
        #[cfg_attr(feature = "serde", serde(default))]
        args_summary: Option<String>,
    },
    /// A delegated subagent working on `task`; its own tool calls are this node's
    /// `children` (Phase 1). Kept flat until the ACP path nests them.
    Subagent { task: String },
    /// A proposed or executed write statement, surfaced verbatim for review.
    Write { sql: String },
    /// A standalone HTML report the `generate_report` tool wrote to `path`. Rendered
    /// as a card with an "Open" button rather than auto-opened, so the report stays
    /// in the transcript and the user chooses when to open it in their browser.
    Report {
        path: String,
        #[cfg_attr(feature = "serde", serde(default))]
        title: Option<String>,
    },
}

/// The lifecycle of an [`ActivityNode`]. Drives the status glyph and whether a
/// spinner shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ActivityStatus {
    /// Created but not yet running (e.g. a write awaiting approval).
    Pending,
    /// Executing now.
    Running,
    /// Finished successfully.
    Ok,
    /// Finished with an error (`detail` carries the message).
    Failed,
    /// The user denied it; it never ran.
    Denied,
}

/// One node in an assistant turn's activity timeline — a tool call, a subagent, or
/// a proposed write. Nodes nest via `children`: a subagent's inner tool calls are
/// its children, which is what lets the panel draw a delegation as one collapsible
/// card rather than a flat run of unrelated tool lines.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ActivityNode {
    pub id: ActivityId,
    pub kind: ActivityKind,
    pub status: ActivityStatus,
    /// A one-line result summary once known: a row count, a byte size, or the
    /// error message on failure. `None` until the node completes.
    #[cfg_attr(feature = "serde", serde(default))]
    pub detail: Option<String>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub children: Vec<ActivityNode>,
}

impl ActivityNode {
    /// A fresh `Running` node with no result and no children.
    pub fn running(id: impl Into<ActivityId>, kind: ActivityKind) -> Self {
        ActivityNode {
            id: id.into(),
            kind,
            status: ActivityStatus::Running,
            detail: None,
            children: Vec::new(),
        }
    }
}

/// Where one plan step stands, mirrored from the agent's own checklist (ACP
/// `PlanEntryStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

/// One entry in the agent's plan checklist, shown at the top of the turn so the
/// user sees the intended steps and watches them tick off.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PlanStep {
    pub title: String,
    pub status: PlanStepStatus,
}

/// A saved connection target. Stored as structured fields rather than one opaque
/// DSN so the form can offer both entry modes and so engines stay swappable; the
/// driver-facing connection string is composed on demand by [`Self::dsn`]. For a
/// file engine the path lives in `database`; `host`/`port`/`user`/`password` are
/// unused. `color` is a label-palette index (UI-defined). `read_only` reflects
/// RED's read-mostly safety posture (enforced by the driver). An optional
/// [`ssh`](Self::ssh) tunnels the whole connection through a jump host.
#[derive(Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ConnectionConfig {
    pub name: String,
    pub kind: DbKind,
    #[cfg_attr(feature = "serde", serde(default))]
    pub host: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub port: Option<u16>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub user: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub password: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub database: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub color: u8,
    /// When unset (read/write), the connection allows writes: deliberate `UPDATE`s
    /// from the SQL editor and guarded, PK-keyed, previewed in-grid cell edits
    /// (Track B5). Read-only is the safe default: the driver opens read-only and
    /// every write path is refused up front.
    #[cfg_attr(feature = "serde", serde(default))]
    pub read_only: bool,
    /// Encrypt the connection with TLS. For Redis this dials `rediss://`; for
    /// MySQL it requires SSL (`require_ssl`); for ClickHouse it uses HTTPS. (A
    /// pasted `rediss://`/`clickhouses://` or an `sslmode=require`/`require_ssl`
    /// DSN also sets this.) The connection form surfaces it as a checkbox — see
    /// docs/plans/redis.md's "first-class TLS toggle" item and
    /// `security-review-2026-07.md`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub tls: bool,
    /// Per-connection AI master-switch override. `None` inherits the global
    /// `[ai] enabled`; `Some(false)` is a true kill switch for *this* connection
    /// (no panel, no tools, no agent), e.g. a production database.
    #[cfg_attr(feature = "serde", serde(default))]
    pub ai_enabled: Option<bool>,
    /// Per-connection AI access-tier override. `None` inherits the global
    /// `[ai] tier`; set it to pin a sensitive connection to `off`/`schema`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub ai_tier: Option<AiTier>,
    /// Optional SSH jump host. When set, the service tunnels the connection
    /// through it (see [`SshConfig`]); `None` connects directly.
    #[cfg_attr(feature = "serde", serde(default))]
    pub ssh: Option<SshConfig>,
    /// Optional forward proxy (SOCKS5 / HTTP CONNECT). When set, the service
    /// resolves a local forward through it (see [`ProxyConfig`]); `None` connects
    /// directly. Mutually exclusive with [`ssh`](Self::ssh) in v1 (rejected up
    /// front by the connect path).
    #[cfg_attr(feature = "serde", serde(default))]
    pub proxy: Option<ProxyConfig>,
    /// Redis Sentinel master group name. When set (Redis only), `host`/`port`
    /// name a Sentinel and the driver resolves the current master via
    /// `SENTINEL get-master-addr-by-name` (see `redis_kv.rs`'s `resolve_sentinel`);
    /// [`Self::dsn`] carries it as the `?master=` query the driver reads. Empty =
    /// a direct (non-Sentinel) connection.
    #[cfg_attr(feature = "serde", serde(default))]
    pub sentinel_master: String,
}

/// Render a secret for `Debug`: `<unset>` when empty, `<redacted>` otherwise, so
/// a stray `{config:?}` can never spill a credential into the logs.
fn redact(secret: &str) -> &'static str {
    if secret.is_empty() {
        "<unset>"
    } else {
        "<redacted>"
    }
}

/// Hand-written so the password is **never** printed: a redacting `Debug` makes
/// the "secrets stay out of logs" rule a compile-time guarantee rather than a
/// convention a stray `{config:?}` could break. (`Serialize` for persistence goes
/// through the password-free `WriteConnection`, so the disk path is already safe.)
impl fmt::Debug for ConnectionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionConfig")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &redact(&self.password))
            .field("database", &self.database)
            .field("color", &self.color)
            .field("read_only", &self.read_only)
            .field("tls", &self.tls)
            .field("ai_enabled", &self.ai_enabled)
            .field("ai_tier", &self.ai_tier)
            .field("ssh", &self.ssh)
            .field("proxy", &self.proxy)
            .field("sentinel_master", &self.sentinel_master)
            .finish()
    }
}

impl ConnectionConfig {
    /// The host to dial for a network engine, defaulting to `localhost` when the
    /// field is left blank. A blank host is the common "just connect to my local
    /// server" case, so we fill it in rather than reject it, matching the
    /// convention of clients like `psql` and `redis-cli`.
    pub fn effective_host(&self) -> &str {
        if self.host.is_empty() {
            "localhost"
        } else {
            &self.host
        }
    }

    /// The connection string handed to the driver. File engines yield the bare
    /// path; network engines compose `scheme://user:pass@host:port/database`, with
    /// the userinfo and database percent-encoded so credentials with reserved
    /// characters survive the round-trip.
    pub fn dsn(&self) -> String {
        if self.kind.is_file() {
            return self.database.clone();
        }
        // TLS is spelled per engine: a dedicated scheme where the client keys
        // off it (`rediss`/`clickhouses`), or a query flag the client honors
        // (`sslmode=require` for Postgres, `require_ssl=true` for MySQL —
        // `mysql_async` reads that natively).
        let scheme = match (self.kind, self.tls) {
            (DbKind::Redis, true) => "rediss",
            (DbKind::Clickhouse, true) => "clickhouses",
            _ => self.kind.url_scheme(),
        };
        let mut url = format!("{scheme}://");
        // A username is optional even with a password set: Redis's classic
        // `AUTH <password>` (pre-ACL) form is a password-only credential, which
        // its URI convention spells as `redis://:password@host`. Emitting
        // nothing here for that case would silently drop the password.
        if !self.user.is_empty() || !self.password.is_empty() {
            url.push_str(&encode(&self.user));
            if !self.password.is_empty() {
                url.push(':');
                url.push_str(&encode(&self.password));
            }
            url.push('@');
        }
        url.push_str(&bracket_host(self.effective_host()));
        if let Some(port) = self.port {
            url.push(':');
            url.push_str(&port.to_string());
        }
        url.push('/');
        url.push_str(&encode(&self.database));
        // Postgres/MySQL carry TLS as a query flag rather than a scheme.
        if self.tls {
            match self.kind {
                DbKind::Postgres => url.push_str("?sslmode=require"),
                DbKind::Mysql => url.push_str("?require_ssl=true"),
                // The `mongodb` crate reads `tls=true` from the URI query.
                DbKind::Mongo => url.push_str("?tls=true"),
                _ => {}
            }
        }
        // Redis Sentinel: `?master=<group>` tells the driver to treat host/port as
        // a Sentinel and resolve the master. Redis spells TLS via the scheme, so
        // this is always the sole query param (no `?`/`&` conflict).
        if self.kind == DbKind::Redis && !self.sentinel_master.is_empty() {
            url.push_str("?master=");
            url.push_str(&encode(&self.sentinel_master));
        }
        url
    }

    /// The DSN for reaching this database through a local forwarded port: the
    /// normal [`dsn`](Self::dsn) but with `host`/`port` swapped for the tunnel's
    /// local endpoint. Reuses `dsn`'s userinfo/database encoding and IPv6
    /// bracketing, so the only difference is where the driver dials. Meaningful
    /// only for network engines; an SSH tunnel never fronts a file engine.
    pub fn local_dsn(&self, host: &str, port: u16) -> String {
        ConnectionConfig {
            host: host.to_string(),
            port: Some(port),
            ssh: None,
            proxy: None,
            ..self.clone()
        }
        .dsn()
    }

    /// A short human label for the connection's target (the file path, or
    /// `user@host:port/database`), shown on cards and in the status bar.
    pub fn display_target(&self) -> String {
        if self.kind.is_file() {
            return self.database.clone();
        }
        let mut s = String::new();
        if !self.user.is_empty() {
            s.push_str(&self.user);
            s.push('@');
        }
        s.push_str(&bracket_host(self.effective_host()));
        if let Some(port) = self.port {
            s.push(':');
            s.push_str(&port.to_string());
        }
        if !self.database.is_empty() {
            s.push('/');
            s.push_str(&self.database);
        }
        s
    }

    /// Decompose a pasted connection string into engine + fields, best-effort.
    /// Returns `None` if there's no recognizable `scheme://…`. Query strings and
    /// fragments are ignored; RED keeps "roughly enough" of the URL to refill the
    /// form, not every libpq option.
    pub fn parse_conn_str(input: &str) -> Option<ParsedDsn> {
        let input = input.trim();
        let (scheme, rest) = input.split_once("://")?;
        let kind = DbKind::from_scheme(scheme).unwrap_or(DbKind::Postgres);
        // TLS is signalled either by the scheme (`rediss`/`clickhouses`) or by a
        // recognized query flag — read it before the query tail is dropped below.
        let tls = dsn_requests_tls(input);
        // Drop any ?query / #fragment tail.
        let rest = rest
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .trim_start_matches('/');

        if kind.is_file() {
            // `sqlite:///abs/path` → authority empty, path is the file. Hand the
            // whole remainder back as the database/path.
            return Some(ParsedDsn {
                kind,
                database: format!("/{}", rest.trim_start_matches('/')),
                ..ParsedDsn::for_kind(kind)
            });
        }

        // authority[/database]
        let (authority, database) = match rest.split_once('/') {
            Some((a, db)) => (a, db.to_string()),
            None => (rest, String::new()),
        };
        // [user[:pass]@]host[:port]
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        let (user, password) = match userinfo {
            Some(u) => match u.split_once(':') {
                Some((user, pass)) => (decode(user), decode(pass)),
                None => (decode(u), String::new()),
            },
            None => (String::new(), String::new()),
        };
        // Bracketed IPv6 literal (`[::1]` / `[::1]:5432`): split the port off after
        // the closing bracket so a colon *inside* the address isn't mistaken for the
        // port separator, and store the host without its brackets.
        let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
            match rest.split_once(']') {
                Some((addr, tail)) => (
                    addr.to_string(),
                    tail.strip_prefix(':').and_then(|p| p.parse::<u16>().ok()),
                ),
                None => (rest.to_string(), None),
            }
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()),
                None => (hostport.to_string(), None),
            }
        };
        Some(ParsedDsn {
            kind,
            host,
            port: port.or_else(|| kind.default_port()),
            user,
            password,
            database: decode(&database),
            tls,
        })
    }
}

/// The fields recovered from a pasted connection string by
/// [`ConnectionConfig::parse_conn_str`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDsn {
    pub kind: DbKind,
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    pub password: String,
    pub database: String,
    /// Whether the DSN signalled TLS (a `rediss`/`clickhouses` scheme or an
    /// `sslmode`/`require_ssl`/`ssl=true` query flag), used to pre-check the
    /// form's TLS toggle.
    pub tls: bool,
}

impl ParsedDsn {
    fn for_kind(kind: DbKind) -> Self {
        ParsedDsn {
            kind,
            host: String::new(),
            port: kind.default_port(),
            user: String::new(),
            password: String::new(),
            database: String::new(),
            tls: false,
        }
    }
}

/// Wrap an IPv6 literal host in `[...]` so the following `:port` separator stays
/// unambiguous; any other host (hostname / IPv4 / already-bracketed) is returned
/// unchanged. Mirrors [`ConnectionConfig::parse_conn_str`]'s bracket handling.
fn bracket_host(host: &str) -> std::borrow::Cow<'_, str> {
    if host.contains(':') && !host.starts_with('[') {
        std::borrow::Cow::Owned(format!("[{host}]"))
    } else {
        std::borrow::Cow::Borrowed(host)
    }
}

/// Percent-encode the characters that would otherwise be parsed as URL syntax
/// inside a userinfo/database component. Deliberately small: not a general
/// RFC 3986 encoder, just enough to keep credentials intact.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'@' | b':' | b'/' | b'?' | b'#' | b'[' | b']' | b'%' | b' ' => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
            _ => out.push(b as char),
        }
    }
    out
}

/// Reverse of [`encode`]: decode `%XX` escapes, leaving anything malformed as-is.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One cell value in a result set. A deliberately small, render-friendly tagged
/// union: drivers map their native types onto this; the UI formats per variant.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    /// Text is an `Arc<str>`, not a `String`, so the display path can share the
    /// same buffer (a refcount bump) instead of copying the bytes a second time
    /// into the grid's `SharedString` cell — a resident text cell is stored once,
    /// not twice. `Arc<str>` is also narrower (16 vs 24 bytes). No UI/runtime dep:
    /// `Arc<str>` is std, and gpui's `SharedString` wraps it zero-copy.
    Text(Arc<str>),
    Blob(Vec<u8>),
    /// A cell the *display* fetch path capped: only a bounded prefix was read
    /// (over-cap text) or only the length (blob), so the full value was never
    /// materialized. Produced solely by capped fetch paths, never by `export` or
    /// a write path, where every cell is whole.
    ///
    /// Boxed: it's the rare, cold variant, and inlining its `String`+`usize`+`bool`
    /// (40 bytes) would widen *every* `Value` — including the common `Integer`/
    /// `Text` cell — to that size across a whole resident window. The box keeps
    /// `Value` at the `String`/`Vec` width (one indirection only on a capped cell).
    Capped(Box<CappedCell>),
}

/// The payload of a [`Value::Capped`] cell: what the grid shows plus the true byte
/// length of the source value, so a detail view can re-fetch the whole thing.
#[derive(Debug, Clone, PartialEq)]
pub struct CappedCell {
    /// The shown text: a char-boundary-safe prefix of over-cap text, or empty for
    /// a blob (rendered as its `<len bytes>` summary). No ellipsis; that's added
    /// at render time, so a copy path can tell the real head from the marker.
    pub head: String,
    /// True byte length of the source value (full text length, or blob size).
    pub len: usize,
    /// Blob vs text: drives the `<N bytes>` summary and the grid's faint styling.
    pub blob: bool,
}

impl Value {
    /// Build a display value from full text `s`, capping to `max_bytes`. Under the
    /// cap it's a whole [`Value::Text`]; over it, a [`Value::Capped`] holding only a
    /// char-boundary-safe prefix plus the true length; the bytes past the cap are
    /// never copied into the value, which is the point on the display fetch path.
    pub fn capped_text(s: &str, max_bytes: usize) -> Value {
        if s.len() <= max_bytes {
            return Value::Text(Arc::from(s));
        }
        let mut end = max_bytes;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        Value::Capped(Box::new(CappedCell {
            head: s[..end].to_owned(),
            len: s.len(),
            blob: false,
        }))
    }

    /// A blob reduced to its length for display; the bytes are never read. The
    /// grid only ever paints a blob as its `<N bytes>` summary; a copy/inspector
    /// re-fetches the real bytes on demand.
    pub fn capped_blob(len: usize) -> Value {
        Value::Capped(Box::new(CappedCell {
            head: String::new(),
            len,
            blob: true,
        }))
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Integer(n) => write!(f, "{n}"),
            Value::Real(x) => write!(f, "{x}"),
            Value::Text(s) => write!(f, "{s}"),
            Value::Blob(b) => write!(f, "<{} bytes>", b.len()),
            Value::Capped(c) if c.blob => write!(f, "<{} bytes>", c.len),
            Value::Capped(c) => write!(f, "{}…", c.head),
        }
    }
}

/// Column metadata for a result set. `name` is always present; `decl_type` is the
/// engine's declared type (best-effort; `None` for computed expressions) and
/// feeds type-aware cell rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub decl_type: Option<String>,
}

/// A one-column aggregate summary, all engine-computed by a single pushdown query
/// (`SELECT count(*), count(col), … FROM (<the result's filtered SQL>) sub`),
/// never by scanning the materialized window. `min`/`max`/`sum`/`avg` ride through
/// as typed [`Value`]s so the UI formats them like grid cells; the counts are
/// plain integers. See `DatabaseDriver::column_stats`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnStats {
    /// `count(*)` over the (filtered) result.
    pub total: i64,
    /// `count(col)`: the non-null rows. `nulls = total - non_null` (derived).
    pub non_null: i64,
    /// `count(distinct col)`; `None` when not computed (the count-distinct guard
    /// withheld it on a large result, recomputable on demand).
    pub distinct: Option<i64>,
    /// `min(col)`, valid for text (lexicographic) and numbers.
    pub min: Value,
    /// `max(col)`.
    pub max: Value,
    /// `sum(col)`: numeric columns only (`None` otherwise, or when every row is
    /// null).
    pub sum: Option<Value>,
    /// `avg(col)`, numeric columns only.
    pub avg: Option<Value>,
}

/// Which aggregates a [`ColumnStats`] request should compute. Bundles the two
/// independent toggles so the boolean pair can't be transposed at a call site
/// (`column_stats(sql, col, numeric, distinct)` → `column_stats(sql, col, flags)`).
/// `numeric` adds the `sum`/`avg` pair (decided UI-side from the column's declared
/// type); `distinct` adds the potentially expensive `count(distinct col)` (guarded
/// UI-side behind a row threshold).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsFlags {
    /// Compute `sum(col)`/`avg(col)` (numeric columns only).
    pub numeric: bool,
    /// Compute `count(distinct col)`.
    pub distinct: bool,
}

/// One row of a foreign-key lookup list (the in-cell id picker): a referenced-table
/// row's identity value plus an optional human-readable label column, so the UI can
/// show `"<id> — <label>"` and let the user search/pick an existing id instead of
/// typing it. See `DatabaseDriver::fetch_lookup`.
#[derive(Debug, Clone, PartialEq)]
pub struct LookupRow {
    /// The referenced (id) column's value — what an FK edit actually writes.
    pub id: Value,
    /// A human label column's value (a name/title/…), when one was resolved;
    /// `None` means only the id is shown.
    pub label: Option<Value>,
}

/// Whether a declared column type is numeric (integer or floating/decimal). Drives
/// the column-stats `sum`/`avg` aggregates, which only make sense on a number, and
/// reuses the same base-token whitelists the key/edit-coercion paths use.
pub fn is_numeric_type(decl_type: Option<&str>) -> bool {
    decl_type.is_some_and(|t| is_int_type(t) || is_real_type(t))
}

/// A result-narrowing filter pushed into the query (Track B2). The service wraps
/// the open's base SQL in `SELECT * FROM (base) WHERE <predicate>` *before* the
/// count / key-bounds probe, so the whole result (count, keyset seek, sort,
/// export) operates on the filtered set without ever materializing it. Because
/// the wrap preserves `SELECT *`, the key column survives and keyset paging is
/// unaffected.
///
/// The UI stays driver-independent by sending this *semantic* filter, not SQL: the
/// backend renders [`Contains`](Self::Contains) to a portable, escaped predicate
/// per engine (see `DatabaseDriver::contains_predicate`) and wraps
/// [`Where`](Self::Where) (power-user SQL, same trust level as the editor)
/// verbatim.
///
/// (`Eq` carries [`ColumnValue`], whose [`Value`] is `PartialEq` but not `Eq`, so
/// this enum is `PartialEq` only.)
#[derive(Debug, Clone, PartialEq)]
pub enum ResultFilter {
    /// "Any text-representable column contains this term": the portable quick
    /// filter. Rendered per engine to a case-insensitive `LIKE`/`ILIKE` OR-chain
    /// over the non-blob columns, with the term escaped to match literally.
    Contains(String),
    /// A raw boolean SQL expression, wrapped verbatim into the `WHERE`. For users
    /// who want a precise predicate; trusted like editor SQL.
    Where(String),
    /// A conjunction of `column = value` equalities (Track B7 foreign-key follow):
    /// one [`ColumnValue`] for a single-column FK, several for a composite. Rendered
    /// per engine to an escaped *literal* predicate (`DatabaseDriver::eq_predicate`)
    /// AND-joined; comparison context coerces each literal to the column's type, so
    /// no cast is needed and the column stays index-usable. Built by the UI from an
    /// [`FkEdge`] + the followed row's values, never from raw SQL; NULL values are
    /// excluded by the caller (a null FK isn't followable).
    Eq(Vec<ColumnValue>),
}

/// A single guarded data edit (Track B5), keyed on a result's primary key. Built by
/// the UI from the result's [`KeySpec`] + base table; a *semantic* edit carrying no
/// SQL, so the UI stays engine-independent. The driver renders it to dialect SQL,
/// **binds** every value (never interpolates), and asserts it touches exactly one
/// row (rolling back otherwise). NULL values are emitted as the literal `NULL`
/// keyword by the renderer, so the per-engine value binders only ever see non-null
/// values.
#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    /// Set one or more columns of the PK-identified row.
    Update {
        table: TableRef,
        key: ColumnValue,
        set: Vec<ColumnValue>,
    },
    /// Delete the PK-identified row.
    Delete { table: TableRef, key: ColumnValue },
    /// Insert a row with the given column values; omitted columns take their DB
    /// default. After a successful insert the caller refetches to surface
    /// server-assigned values (autoincrement PK, defaults).
    Insert {
        table: TableRef,
        values: Vec<ColumnValue>,
    },
}

/// A (schema, name) table reference for an [`EditOp`]. `schema` is the namespace a
/// browse came from (`OpenResult.table`); the renderer qualifies and quotes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

/// One `column = value` pair of an [`EditOp`]. `decl_type` is the column's declared
/// engine type (best-effort, `None` for a key/PK or where it isn't known): the
/// driver needs it to bind correctly into a non-text column. A value the driver
/// decoded to [`Value::Text`] (jsonb, timestamp, uuid, numeric, an enum) must be
/// *cast* back to that column type on write, because Postgres has no implicit
/// (assignment) cast from `text` to those, so a bare text bind is rejected. Keys
/// (always int/text, see `PkKey`) bind fine without it, so they carry `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnValue {
    pub column: String,
    pub value: Value,
    pub decl_type: Option<String>,
}

impl EditOp {
    /// The human verb for the confirm modal ("Update" / "Delete" / "Insert").
    pub fn verb(&self) -> &'static str {
        match self {
            EditOp::Update { .. } => "Update",
            EditOp::Delete { .. } => "Delete",
            EditOp::Insert { .. } => "Insert",
        }
    }

    /// A readable, **display-only** rendering of the statement, values inlined as
    /// literals; what the confirm modal shows. This is *not* what executes: the
    /// driver renders dialect SQL and binds the values as parameters. Quoting is
    /// generic (double-quoted identifiers); the live statement uses the engine's.
    pub fn preview_sql(&self) -> String {
        let q = |id: &str| format!("\"{}\"", id.replace('"', "\"\""));
        let qualify = |t: &TableRef| match &t.schema {
            Some(s) if !s.is_empty() => format!("{}.{}", q(s), q(&t.name)),
            _ => q(&t.name),
        };
        match self {
            EditOp::Update { table, key, set } => {
                let assigns = set
                    .iter()
                    .map(|cv| format!("{} = {}", q(&cv.column), literal(&cv.value)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "UPDATE {} SET {} WHERE {} = {}",
                    qualify(table),
                    assigns,
                    q(&key.column),
                    literal(&key.value)
                )
            }
            EditOp::Delete { table, key } => format!(
                "DELETE FROM {} WHERE {} = {}",
                qualify(table),
                q(&key.column),
                literal(&key.value)
            ),
            EditOp::Insert { table, values } => {
                let cols = values
                    .iter()
                    .map(|cv| q(&cv.column))
                    .collect::<Vec<_>>()
                    .join(", ");
                let vals = values
                    .iter()
                    .map(|cv| literal(&cv.value))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "INSERT INTO {} ({}) VALUES ({})",
                    qualify(table),
                    cols,
                    vals
                )
            }
        }
    }
}

/// Render a [`Value`] as a SQL literal for an [`EditOp`] **preview only**, quoting
/// text with embedded quotes doubled. Never used to build executed SQL (the driver
/// binds values), so it's a readability helper, not an injection surface.
fn literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
        Value::Capped(c) if c.blob => format!("<{} bytes>", c.len),
        Value::Capped(c) => format!("'{}…'", c.head.replace('\'', "''")),
    }
}

/// Coerce user-entered `text` to a [`Value`] for an edit, guided by the column's
/// declared type. An empty string maps to [`Value::Null`] (clearing a cell). Numeric
/// columns parse to [`Value::Integer`]/[`Value::Real`]; a parse failure is an
/// `Err(reason)` the editor shows inline, so the preview never opens with an
/// un-bindable value. Everything else is [`Value::Text`].
pub fn coerce_edit_value(
    text: &str,
    decl_type: Option<&str>,
) -> std::result::Result<Value, String> {
    if text.is_empty() {
        return Ok(Value::Null);
    }
    if decl_type.is_some_and(is_int_type) {
        return text
            .trim()
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|_| format!("‘{text}’ is not a valid integer"));
    }
    if decl_type.is_some_and(is_real_type) {
        return text
            .trim()
            .parse::<f64>()
            .map(Value::Real)
            .map_err(|_| format!("‘{text}’ is not a valid number"));
    }
    Ok(Value::Text(Arc::from(text)))
}

/// A query execution plan (Track B4: EXPLAIN). A small, fully-materialized tree
/// the driver builds from the engine's *native* EXPLAIN output (SQLite's
/// `EXPLAIN QUERY PLAN` rows, Postgres's indented text plan, MySQL's `FORMAT=TREE`)
/// so the UI renders it readably without any engine knowledge. A plan is
/// inherently tiny and bounded, so unlike a result set it is held whole; the
/// "never materialize" rule targets row data, not a fixed-size plan.
///
/// Metrics stay *engine-named* on purpose: SQLite reports none, Postgres reports
/// `cost`/`rows`/`width` (plus `actual time`/`rows`/`loops` under ANALYZE), MySQL
/// its own. We render whatever the engine names rather than inventing a false
/// cross-engine cost unit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryPlan {
    /// The plan's root operations, usually one. A SQLite plan is a shallow tree
    /// of steps; a Postgres/MySQL plan nests.
    pub nodes: Vec<PlanNode>,
    /// The engine's verbatim EXPLAIN text: the "Copy plan" payload and the
    /// guaranteed fallback the UI shows when `nodes` is empty (an exotic plan the
    /// structural parse couldn't tree).
    pub raw: String,
    /// `true` iff this came from `EXPLAIN ANALYZE`; actual-row/time metrics are
    /// present and the UI may tint the costliest node. SQLite has no ANALYZE.
    pub analyzed: bool,
}

/// One operation in a [`QueryPlan`] tree.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    /// The operation line, e.g. `"Seq Scan on users"` / `"SEARCH t USING INDEX ix"`.
    pub label: String,
    /// Engine extras attached to this node (a filter, index condition, join key),
    /// joined for display. `None` when the node carries none.
    pub detail: Option<String>,
    /// Engine-named metrics rendered as-is: `("cost", "0.00..18.50")`,
    /// `("rows", "850")`, `("actual time", "0.011..0.012")`. Empty for SQLite.
    pub metrics: Vec<(String, String)>,
    pub children: Vec<PlanNode>,
}

impl PlanNode {
    /// A leaf node with just a label: the building block the parsers grow.
    pub fn leaf(label: impl Into<String>) -> PlanNode {
        PlanNode {
            label: label.into(),
            detail: None,
            metrics: Vec::new(),
            children: Vec::new(),
        }
    }
}

/// What a schema object is. SQLite has tables and views; Postgres maps onto
/// the same two for the explorer's purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Table,
    View,
}

/// A namespace of objects, the top level of the schema tree. For SQLite this is
/// a database from `PRAGMA database_list` (`main` / `temp` / an attached DB); for
/// Postgres it's a real schema. One level so both engines fit the same tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaMeta {
    pub name: String,
    /// Names + kinds only: the cheap tree skeleton, loaded on connect. Column
    /// detail is pulled per table via [`TableDetail`] on expand.
    pub objects: Vec<ObjectMeta>,
}

/// One table or view in a [`SchemaMeta`], just enough to draw the tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub name: String,
    pub kind: ObjectKind,
}

/// On-demand detail for one table/view: its columns, foreign keys, and indexes.
/// Loaded lazily when the user expands the object, never as part of the skeleton.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableDetail {
    pub columns: Vec<ColumnMeta>,
    pub foreign_keys: Vec<ForeignKeyMeta>,
    pub indexes: Vec<IndexMeta>,
}

/// One column of a table/view. Richer than result-set [`Column`]: it carries the
/// schema facts the tree shows (nullability, primary-key membership, default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    /// Declared type, best-effort (`None` for an untyped/computed column).
    pub type_name: Option<String>,
    pub not_null: bool,
    pub primary_key: bool,
    pub default: Option<String>,
    /// The column auto-numbers (SQLite `INTEGER PRIMARY KEY` rowid alias, Postgres
    /// `serial`/identity, MySQL `AUTO_INCREMENT`). Detected by `describe_table` and
    /// re-emitted per-dialect by the migration's `create_table` so the target table
    /// keeps auto-numbering future inserts. `false` for result-derived columns (a
    /// query result carries no such flag) and read-only engines.
    pub auto_increment: bool,
}

/// A foreign-key edge from a local column to a referenced table/column. The tree
/// derives a column's FK badge by matching `column` against the table's columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyMeta {
    pub column: String,
    pub ref_table: String,
    pub ref_column: String,
}

/// One foreign-key edge of the connection-wide relation graph (Track B7). Unlike
/// the per-table [`ForeignKeyMeta`], this carries both endpoints' namespaces and is
/// usable in either direction: *forward* (`from_table` points out) backs "go to the
/// referenced row", *reverse* (`to_table` is pointed at) backs "show referencing
/// rows". `columns` pairs each local column with its referenced column in key order;
/// `len > 1` is a composite key. Loaded once per connection via
/// [`DatabaseDriver::foreign_keys`](../red_driver/trait.DatabaseDriver.html) and
/// indexed both ways by the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkEdge {
    pub from_schema: Option<String>,
    pub from_table: String,
    pub to_schema: Option<String>,
    pub to_table: String,
    /// `(from_column, to_column)` pairs, in key order.
    pub columns: Vec<(String, String)>,
}

/// The base subquery's alias in an inline-FK-expansion wrap (see [`FkJoin`] and the
/// driver's `join_wrap`): the result exposes `_red_base.*` first, so every base
/// column keeps its position and name. Shared so the UI's join builder and the
/// driver's SQL wrapper agree on the same identifier for a first hop's parent.
pub const BASE_ALIAS: &str = "_red_base";

/// One inline foreign-key *column expansion* (Track B7): a `LEFT JOIN` that pulls
/// selected columns of a referenced table into a browse as extra, dotted-aliased
/// columns. The service folds an ordered `Vec<FkJoin>` into the result's base SQL
/// (see `join_wrap`), so each hop decorates the page without changing its row count:
/// the join target is always a *unique* key, so a `LEFT JOIN` matches ≤1 row.
///
/// Joins are ordered outer→inner: a deeper hop's [`parent_alias`](Self::parent_alias)
/// names an *earlier* join's [`alias`](Self::alias) (or the base, `_red_base`), so a
/// chain like `tier → cascade → placement` references only already-declared aliases.
/// Aliases are caller-assigned, simple identifiers (`_red_j0`, `_red_j1`, …) so they
/// never need quoting or length-shortening; the meaningful dotted names live in the
/// output [`select`](Self::select) aliases (e.g. `tier_id.name`), which is how the
/// grid keys a joined cell back to its tree column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkJoin {
    /// This join's table alias: a simple, unique identifier (`_red_j0`, …).
    pub alias: String,
    /// The alias this join's `ON` reads its local columns from: an earlier join's
    /// [`alias`](Self::alias), or the base subquery alias (`_red_base`) for a first hop.
    pub parent_alias: String,
    /// `(parent_column, target_column)` equalities forming the `ON` clause, in key
    /// order; usually one pair (a single-column FK), `len > 1` for a composite key.
    pub on: Vec<(String, String)>,
    /// The referenced table's namespace, when the engine qualifies by schema.
    pub to_schema: Option<String>,
    /// The referenced table.
    pub to_table: String,
    /// `(target_column, output_alias)` columns to select from the joined table. The
    /// output alias is the dotted tree path (`tier_id.name`), unique across the result.
    pub select: Vec<(String, String)>,
}

/// An index over one or more columns of a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMeta {
    pub name: String,
    pub unique: bool,
    pub columns: Vec<String>,
}

/// How a result's rows are keyed for seek (keyset) pagination: an ordered,
/// effectively-unique tuple of columns. A plain table browse is a single key
/// column (the PK / unique index, see [`KeySpec::from_detail`]); a header-click
/// sorted browse is `(sort_col, pk)` with the PK as tiebreaker (see
/// [`KeySpec::sorted`]). Arbitrary editor SQL has no key and pages by `OFFSET`.
///
/// The seek is always a single row-value comparison `(c1, …) </> (…)`, so every
/// column in the tuple shares one [`direction`](Self::direction). RED
/// only offers a single-column header sort, so the tiebreaker inherits the lead's
/// direction and a mixed-direction tuple never arises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySpec {
    /// The lead key column's name, as it appears in the result set: the sort
    /// column for a sorted browse, or the PK for a plain browse.
    pub column: String,
    /// The lead column's kind. Drives key-space interpolation (only the lead
    /// matters): an `Int` lead supports fraction jumps, `Other` degrades to
    /// `OFFSET` for far jumps.
    pub kind: KeyKind,
    /// The lead column's declared type (best-effort), so a driver that pins bind
    /// types can cast the keyset cursor back to the column's type. Without it a
    /// text-decoded cursor (a uuid/timestamp/numeric key) binds as `text` and the
    /// seek comparison `col > $1::text` has no operator (Postgres 42883).
    pub column_type: Option<String>,
    /// The PK tiebreaker, appended after [`column`](Self::column) when sorting by
    /// a non-PK column so rows sharing a `column` value order deterministically.
    /// `None` for a plain browse (the lead column is itself the unique key).
    pub tiebreak: Option<String>,
    /// The tiebreaker column's declared type, paired with [`tiebreak`](Self::tiebreak)
    /// for the same cursor-cast reason as [`column_type`](Self::column_type).
    pub tiebreak_type: Option<String>,
    /// The sort direction of the lead column (header click); `Asc` for a plain
    /// browse. The tiebreaker shares this direction.
    pub direction: SortDirection,
}

/// Whether the key is numerically interpolable. `Int` keys support key-space
/// seek (jump to a fraction via `min + f·(max − min)`); `Other` keys still get
/// keyset scroll but fall back to `OFFSET` for far jumps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Int,
    Other,
}

/// A sort direction. Replaces the `descending: bool` that flowed through
/// [`KeySpec`], the sort spec, and the four drivers' seek builders — where a bare
/// bool invited transposition and read as noise at the `if descending { "DESC" }`
/// call sites. Note the two distinct roles it plays: a key's *own* ordering
/// (`KeySpec::direction`) and the *scroll* direction of a page fetch; the keyset
/// seek composes them with [`reversed_when`](Self::reversed_when).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDirection {
    #[default]
    Asc,
    Desc,
}

impl SortDirection {
    /// Build from the legacy `descending` flag (a header click / scroll flag).
    pub fn from_descending(descending: bool) -> Self {
        if descending { Self::Desc } else { Self::Asc }
    }

    /// Whether this sorts descending.
    pub fn is_descending(self) -> bool {
        matches!(self, Self::Desc)
    }

    /// The SQL keyword for this direction.
    pub fn sql(self) -> &'static str {
        if self.is_descending() { "DESC" } else { "ASC" }
    }

    /// This direction, reversed when `flip` is set. The keyset seek composes a
    /// key's own direction with the up/down scroll direction — the old
    /// `key.descending ^ scroll_descending`.
    pub fn reversed_when(self, flip: bool) -> Self {
        Self::from_descending(self.is_descending() ^ flip)
    }
}

impl KeySpec {
    /// A single-column, ascending key (a plain table browse; also the building
    /// block the conformance battery seeds with).
    pub fn single(column: impl Into<String>, kind: KeyKind) -> KeySpec {
        KeySpec {
            column: column.into(),
            kind,
            column_type: None,
            tiebreak: None,
            tiebreak_type: None,
            direction: SortDirection::Asc,
        }
    }

    /// The lead column's declared type for the seek cursor cast.
    pub fn with_column_type(mut self, type_name: Option<String>) -> KeySpec {
        self.column_type = type_name;
        self
    }

    /// The seek columns in order: the lead column then the tiebreaker, if any.
    /// Drivers quote these for the `ORDER BY` and the row-value comparison.
    pub fn column_names(&self) -> Vec<&str> {
        let mut cols = vec![self.column.as_str()];
        if let Some(t) = &self.tiebreak {
            cols.push(t.as_str());
        }
        cols
    }

    /// The seek columns' declared types, index-aligned with [`column_names`](Self::column_names),
    /// for a driver that casts the bound cursor back to each column's type.
    pub fn column_types(&self) -> Vec<Option<&str>> {
        let mut types = vec![self.column_type.as_deref()];
        if self.tiebreak.is_some() {
            types.push(self.tiebreak_type.as_deref());
        }
        types
    }

    /// Resolve a table's seek key from its introspected detail: a single-column
    /// primary key, or failing that a single-column unique not-null index.
    /// `None` (composite or nullable key, or no key at all) means the result
    /// isn't keyset-eligible and pages by `OFFSET`.
    pub fn from_detail(detail: &TableDetail) -> Option<KeySpec> {
        let column = resolve_key_column(detail)?;
        // SQLite reports `INTEGER PRIMARY KEY` (the rowid alias, never null) as
        // nullable, so an integer key passes without `not_null`; any other
        // nullable key disqualifies keyset (NULLs don't order reliably).
        let kind = key_kind(column);
        (column.not_null || kind == KeyKind::Int).then(|| {
            KeySpec::single(column.name.clone(), kind).with_column_type(column.type_name.clone())
        })
    }

    /// Resolve the composite seek key for a header-click sort by `sort_col`: the
    /// sort column led, with the table's PK appended as a tiebreaker so equal
    /// `sort_col` rows page deterministically. `None` (→ `OFFSET` fallback) when
    /// the table has no usable PK, when `sort_col` isn't a real column of the
    /// table (an expression/alias), or when it's nullable (NULLs don't order
    /// reliably across engines). Sorting by the PK itself collapses to the plain
    /// single-column key, just carrying the direction.
    pub fn sorted(
        detail: &TableDetail,
        sort_col: &str,
        direction: SortDirection,
    ) -> Option<KeySpec> {
        let pk = resolve_key_column(detail)?;
        let lead = detail.columns.iter().find(|c| c.name == sort_col)?;
        if lead.name == pk.name {
            return Some(KeySpec {
                column: pk.name.clone(),
                kind: key_kind(pk),
                column_type: pk.type_name.clone(),
                tiebreak: None,
                tiebreak_type: None,
                direction,
            });
        }
        // A nullable non-PK lead disqualifies keyset, the same posture `from_detail`
        // takes for nullable keys.
        let kind = key_kind(lead);
        (lead.not_null || kind == KeyKind::Int).then(|| KeySpec {
            column: lead.name.clone(),
            kind,
            column_type: lead.type_name.clone(),
            tiebreak: Some(pk.name.clone()),
            tiebreak_type: pk.type_name.clone(),
            direction,
        })
    }
}

/// The PK / unique-not-null-index column a table's keyset key is built from, if
/// any. Shared by [`KeySpec::from_detail`] (the key itself) and
/// [`KeySpec::sorted`] (the tiebreaker).
fn resolve_key_column(detail: &TableDetail) -> Option<&ColumnMeta> {
    let pks: Vec<&ColumnMeta> = detail.columns.iter().filter(|c| c.primary_key).collect();
    match pks.as_slice() {
        [single] => Some(*single),
        [] => detail
            .indexes
            .iter()
            .filter(|i| i.unique && i.columns.len() == 1)
            .find_map(|i| {
                detail
                    .columns
                    .iter()
                    .find(|c| c.name == i.columns[0] && c.not_null)
            }),
        _ => None,
    }
}

/// Whether a column is numerically interpolable (drives key-space fraction jumps).
fn key_kind(column: &ColumnMeta) -> KeyKind {
    if column.type_name.as_deref().is_some_and(is_int_type) {
        KeyKind::Int
    } else {
        KeyKind::Other
    }
}

/// Whether a declared type names an integer column across the three engines
/// (`INTEGER`, `bigint`, `int(11)`, `serial`, …). Deliberately a whitelist of
/// the base token: `interval`/`point` must not match.
fn is_int_type(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    matches!(
        base,
        "int"
            | "integer"
            | "int2"
            | "int4"
            | "int8"
            | "tinyint"
            | "smallint"
            | "mediumint"
            | "bigint"
            | "serial"
            | "smallserial"
            | "bigserial"
    )
}

/// Whether a declared type is a floating/decimal numeric column, for edit-value
/// coercion (parse to [`Value::Real`]). Best-effort across the three engines.
fn is_real_type(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    matches!(
        base,
        "real" | "double" | "float" | "float4" | "float8" | "numeric" | "decimal" | "dec"
    )
}

/// One bounded window of rows pulled from a streaming cursor. The streaming path
/// never materializes a whole result; it yields these fixed-size windows.
#[derive(Debug, Clone, Default)]
pub struct RowWindow {
    pub rows: Vec<Vec<Value>>,
    /// `true` once this window reaches the end of the result (no more fetches).
    pub exhausted: bool,
}

/// One random-access page of a result, fetched by `(offset, limit)`. Backs the
/// result grid's load-on-scroll: a bounded window buffer requests the pages
/// around the viewport and evicts the rest, so memory stays flat over a
/// multi-million-row result. Columns ride along (stable, but cheap to repeat).
#[derive(Debug, Clone, Default)]
pub struct ResultPage {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

/// A streamed-export target format. The driver writes rows straight to disk
/// row-by-row, never materializing the whole result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    /// A themed, self-contained HTML report: a standalone document (inline CSS,
    /// light/dark via `prefers-color-scheme`) opened in the system browser, not a
    /// data interchange file. Streamed row-by-row like the others.
    Html,
    /// A stream of `INSERT INTO "table" (...) VALUES (...);` statements, one per
    /// row. The table name is derived from the destination file stem; identifiers
    /// and string literals use portable ANSI quoting (double / single quotes).
    Sql,
}

/// A streamed-import source format, the read-side mirror of [`ExportFormat`]. The
/// reader yields one row of raw text cells at a time, never materializing the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    Csv,
    /// Newline-delimited JSON: one JSON object per line.
    Jsonl,
    /// A single top-level JSON array of objects (`[ {…}, {…} ]`), read one element
    /// at a time so the whole file is never materialized.
    JsonArray,
}

/// One column mapping for a data import / copy: which **source** cell index feeds
/// which **target** column (carrying the column's best-effort declared type, used to
/// coerce the text cell and to cast the bound parameter). Built UI-side from the
/// file header + the target table's columns; the dispatch import loop reads it to
/// project each source row into target-column order.
#[derive(Debug, Clone)]
pub struct ColumnMap {
    pub source: usize,
    pub column: String,
    pub decl_type: Option<String>,
}

/// Per-query knobs carried UI → service → driver.
#[derive(Debug, Clone)]
pub struct QueryOptions {
    /// Max rows per fetched window. Read by the streaming cursor (`open_cursor`)
    /// to size its row channel.
    pub window: usize,
    /// Abort a single streamed fetch that stalls longer than this; guards a
    /// runaway query that computes a huge intermediate before yielding row 1.
    /// Enforced by the *service*, not the driver: the dispatch loop races the
    /// cursor's `next_window` against this deadline and fires the engine cancel on
    /// expiry (see `drive_fetch`). The windowed `OpenResult` path uses the global
    /// `statement_timeout` instead. `None` = no cap.
    pub timeout: Option<std::time::Duration>,
    /// Read every cell at **full fidelity**, never the display fat-cell cap. The
    /// interactive cursor caps long text/blobs for paint performance
    /// (`Value::Capped`), which is right for the grid but **data loss** for a copy:
    /// the table-copy read sets this so a long `TEXT`/blob round-trips byte-exact
    /// into the target (the same invariant export holds via `PageCap::Full`). The
    /// default is `false`; the grid's streaming path stays capped.
    pub full_fidelity: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            window: 1000,
            timeout: None,
            full_fidelity: false,
        }
    }
}

/// How a table copy writes into its target ([`crate`]'s `CopyToTable`). Append is
/// the default "add these rows"; `TruncateInsert` clears the target first (behind
/// the destructive confirm): "refresh this table from the source". Upsert/merge is
/// deliberately **not** here: it needs a per-engine conflict-key seam and a key
/// picker, which is the on-ramp to the sync machinery table-copy exists to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// Insert the source rows into the target as-is (the target keeps its rows).
    Append,
    /// Clear the target (`DELETE FROM`) before inserting: a full refresh.
    TruncateInsert,
}

/// The error type that crosses every RED layer boundary.
#[derive(Debug, thiserror::Error)]
pub enum RedError {
    #[error("connection failed: {0}")]
    Connect(String),
    /// A connect attempt the user must fix before retrying makes sense: bad
    /// credentials, a missing database, an unknown role. Distinct from
    /// [`RedError::Connect`] (a transient/network failure) so the UI can stop the
    /// backoff loop and prompt for an edit instead of retrying forever.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// An SSH jump host whose key isn't in `~/.ssh/known_hosts`. Distinct from
    /// [`RedError::Auth`] so the UI can offer "trust this host & retry" instead of
    /// a dead end: it carries the fingerprint to show and the OpenSSH-encoded key
    /// to append to `known_hosts` on accept.
    #[error("unknown SSH host key for {host}")]
    SshHostUnknown {
        host: String,
        port: u16,
        fingerprint: String,
        key: String,
    },
    #[error("query failed: {0}")]
    Query(String),
    #[error("driver error: {0}")]
    Driver(String),
    /// A fetch was aborted out-of-band (user cancel). Distinct from a failure so
    /// the service can emit a clean "cancelled" rather than an error.
    #[error("query cancelled")]
    Interrupted,
    /// A fetch exceeded its configured timeout.
    #[error("query timed out")]
    Timeout,
}

pub type Result<T> = std::result::Result<T, RedError>;

/// The self-updater's lifecycle, surfaced from the backend to the UI titlebar
/// pill + About tab (Phases 3–4 of docs/plans/self-update.md). macOS-only today;
/// on other platforms the updater never advances past `Unknown`. A run is
/// "stage on disk, apply on restart": the new bundle is fully swapped over the
/// installed app *before* `ReadyToRestart`, so a restart is just a relaunch.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum UpdateState {
    /// No check has completed this session (the initial state, and while updates
    /// are disabled).
    #[default]
    Unknown,
    /// A check is in flight.
    Checking,
    /// The running build is the latest published release.
    UpToDate { current: String },
    /// A newer release is downloading/staging over the installed app. `pct` is a
    /// coarse 0–100 progress hint.
    Downloading { version: String, pct: u8 },
    /// A newer build is fully staged; clicking the pill relaunches into it.
    ReadyToRestart { version: String },
    /// The check or staging failed; `reason` is user-facing.
    Failed { reason: String },
    /// A newer release exists but RED can't self-swap (not running from a
    /// writable `/Applications/Red.app`: a dev build, Homebrew, a read-only
    /// volume). `url` links the GitHub release for a manual download.
    Unsupported { version: String, url: String },
}

#[cfg(test)]
mod value_size_tests {
    use super::*;

    /// A resident result window holds one `Value` per cell (rows × columns), so the
    /// enum's width is paid on every cell — including the common `Integer`/`Text`.
    /// Keep it at the `String`/`Vec` width by boxing the cold `Capped` variant; if
    /// someone inlines a fat field again this guard fails rather than silently
    /// widening every cell. `CappedCell` itself is deliberately larger (40 bytes).
    #[test]
    fn value_stays_pointer_width() {
        assert!(
            std::mem::size_of::<Value>() <= 4 * std::mem::size_of::<usize>(),
            "Value grew to {} bytes; a fat variant is inlined — box it",
            std::mem::size_of::<Value>()
        );
        assert!(std::mem::size_of::<CappedCell>() > std::mem::size_of::<Value>());
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    fn col(name: &str, type_name: &str, not_null: bool, primary_key: bool) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: Some(type_name.into()),
            not_null,
            primary_key,
            default: None,
            auto_increment: false,
        }
    }

    #[test]
    fn single_int_pk_is_the_key() {
        // SQLite-style: INTEGER PRIMARY KEY reports notnull = 0 but still counts.
        let detail = TableDetail {
            columns: vec![
                col("id", "INTEGER", false, true),
                col("name", "TEXT", true, false),
            ],
            ..Default::default()
        };
        let key = KeySpec::from_detail(&detail).unwrap();
        assert_eq!(key.column, "id");
        assert_eq!(key.kind, KeyKind::Int);
    }

    #[test]
    fn text_pk_needs_not_null() {
        let nullable = TableDetail {
            columns: vec![col("id", "TEXT", false, true)],
            ..Default::default()
        };
        assert!(KeySpec::from_detail(&nullable).is_none());

        let not_null = TableDetail {
            columns: vec![col("id", "TEXT", true, true)],
            ..Default::default()
        };
        let key = KeySpec::from_detail(&not_null).unwrap();
        assert_eq!(key.kind, KeyKind::Other);
    }

    #[test]
    fn composite_pk_is_ineligible() {
        let detail = TableDetail {
            columns: vec![
                col("a", "INTEGER", true, true),
                col("b", "INTEGER", true, true),
            ],
            ..Default::default()
        };
        assert!(KeySpec::from_detail(&detail).is_none());
    }

    #[test]
    fn unique_not_null_index_substitutes_for_a_pk() {
        let detail = TableDetail {
            columns: vec![
                col("email", "varchar", true, false),
                col("name", "text", false, false),
            ],
            indexes: vec![IndexMeta {
                name: "uq_email".into(),
                unique: true,
                columns: vec!["email".into()],
            }],
            ..Default::default()
        };
        let key = KeySpec::from_detail(&detail).unwrap();
        assert_eq!(key.column, "email");
        assert_eq!(key.kind, KeyKind::Other);
    }

    #[test]
    fn no_key_at_all_is_ineligible() {
        let detail = TableDetail {
            columns: vec![col("x", "INTEGER", true, false)],
            ..Default::default()
        };
        assert!(KeySpec::from_detail(&detail).is_none());
    }

    #[test]
    fn key_carries_column_types_for_the_cursor_cast() {
        // A uuid PK: the key must carry "uuid" so the driver casts the keyset
        // cursor back to it (`$1::text::"uuid"`) instead of binding bare text;
        // otherwise `id > $1::text` is a 42883 "no operator" error on Postgres.
        let detail = TableDetail {
            columns: vec![
                col("id", "uuid", true, true),
                col("created_at", "timestamptz", true, false),
            ],
            ..Default::default()
        };
        let key = KeySpec::from_detail(&detail).unwrap();
        assert_eq!(key.column_type.as_deref(), Some("uuid"));
        assert_eq!(key.column_types(), vec![Some("uuid")]);

        // A header-click sort by a non-PK column leads with that column's type and
        // appends the PK as a typed tiebreaker; both need their cast.
        let sorted = KeySpec::sorted(&detail, "created_at", SortDirection::Desc).unwrap();
        assert_eq!(sorted.column_type.as_deref(), Some("timestamptz"));
        assert_eq!(sorted.tiebreak.as_deref(), Some("id"));
        assert_eq!(sorted.tiebreak_type.as_deref(), Some("uuid"));
        assert_eq!(
            sorted.column_types(),
            vec![Some("timestamptz"), Some("uuid")]
        );
    }

    #[test]
    fn int_detection_is_a_whitelist() {
        assert!(is_int_type("INTEGER"));
        assert!(is_int_type("bigint"));
        assert!(is_int_type("int(11) unsigned"));
        assert!(is_int_type("serial"));
        assert!(!is_int_type("interval"));
        assert!(!is_int_type("point"));
        assert!(!is_int_type("text"));
    }
}

#[cfg(test)]
mod conn_tests {
    use super::*;

    #[test]
    fn dsn_composes_network_url() {
        let cfg = ConnectionConfig {
            kind: DbKind::Postgres,
            host: "localhost".into(),
            port: Some(5432),
            user: "postgres".into(),
            password: "p@ss:word".into(),
            database: "analytics".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.dsn(),
            "postgres://postgres:p%40ss%3Aword@localhost:5432/analytics"
        );
    }

    #[test]
    fn dsn_composes_password_only_auth() {
        // Redis's classic `AUTH <password>` (pre-ACL, no username) must still
        // reach the driver: dropping it here would silently connect
        // unauthenticated instead of failing loudly.
        let cfg = ConnectionConfig {
            kind: DbKind::Redis,
            host: "localhost".into(),
            port: Some(6379),
            password: "hunter2".into(),
            ..Default::default()
        };
        assert_eq!(cfg.dsn(), "redis://:hunter2@localhost:6379/");
    }

    #[test]
    fn dsn_emits_sentinel_master_query_for_redis() {
        let cfg = ConnectionConfig {
            kind: DbKind::Redis,
            host: "sentinel.local".into(),
            port: Some(26379),
            sentinel_master: "mymaster".into(),
            ..Default::default()
        };
        assert_eq!(cfg.dsn(), "redis://sentinel.local:26379/?master=mymaster");
        // TLS composes: the scheme flips to rediss, master stays the sole query.
        let tls = ConnectionConfig {
            tls: true,
            ..cfg.clone()
        };
        assert_eq!(tls.dsn(), "rediss://sentinel.local:26379/?master=mymaster");
        // A non-Redis engine never emits it, even if the field is set.
        let pg = ConnectionConfig {
            kind: DbKind::Postgres,
            ..cfg
        };
        assert!(!pg.dsn().contains("master="));
    }

    #[test]
    fn blank_host_falls_back_to_localhost() {
        // Leaving the host empty is the common "connect to my local server" case;
        // both the DSN and the display label fill in `localhost` rather than
        // emitting an empty authority.
        let cfg = ConnectionConfig {
            kind: DbKind::Redis,
            host: String::new(),
            port: Some(6379),
            ..Default::default()
        };
        assert_eq!(cfg.dsn(), "redis://localhost:6379/");
        assert_eq!(cfg.display_target(), "localhost:6379");
        assert_eq!(cfg.effective_host(), "localhost");
    }

    #[test]
    fn dsn_encodes_tls_per_engine() {
        let tls = |kind: DbKind| {
            ConnectionConfig {
                kind,
                host: "h".into(),
                port: Some(1),
                tls: true,
                ..Default::default()
            }
            .dsn()
        };
        assert_eq!(tls(DbKind::Redis), "rediss://h:1/");
        assert_eq!(tls(DbKind::Clickhouse), "clickhouses://h:1/");
        assert_eq!(tls(DbKind::Postgres), "postgres://h:1/?sslmode=require");
        assert_eq!(tls(DbKind::Mysql), "mysql://h:1/?require_ssl=true");
    }

    #[test]
    fn parse_detects_tls_from_scheme_and_query() {
        assert!(
            ConnectionConfig::parse_conn_str("rediss://h:6379/0")
                .unwrap()
                .tls
        );
        assert!(
            ConnectionConfig::parse_conn_str("clickhouses://h:8443/db")
                .unwrap()
                .tls
        );
        assert!(
            ConnectionConfig::parse_conn_str("postgres://h/db?sslmode=require")
                .unwrap()
                .tls
        );
        assert!(
            ConnectionConfig::parse_conn_str("mysql://h/db?require_ssl=true")
                .unwrap()
                .tls
        );
        // Plain schemes and an explicitly-disabled sslmode are not TLS.
        assert!(
            !ConnectionConfig::parse_conn_str("redis://h:6379/0")
                .unwrap()
                .tls
        );
        assert!(
            !ConnectionConfig::parse_conn_str("postgres://h/db?sslmode=disable")
                .unwrap()
                .tls
        );
    }

    #[test]
    fn tls_dsn_round_trips_through_parse() {
        // A TLS config's DSN, re-parsed, keeps the tls bit for each engine.
        for kind in [
            DbKind::Redis,
            DbKind::Clickhouse,
            DbKind::Postgres,
            DbKind::Mysql,
        ] {
            let cfg = ConnectionConfig {
                kind,
                host: "host".into(),
                port: Some(5),
                database: "db".into(),
                tls: true,
                ..Default::default()
            };
            let parsed = ConnectionConfig::parse_conn_str(&cfg.dsn()).unwrap();
            assert!(parsed.tls, "{kind:?} lost tls through parse");
            assert_eq!(parsed.kind, kind);
        }
    }

    #[test]
    fn dsn_for_file_is_the_path() {
        let cfg = ConnectionConfig {
            kind: DbKind::Sqlite,
            database: "/tmp/app.sqlite".into(),
            ..Default::default()
        };
        assert_eq!(cfg.dsn(), "/tmp/app.sqlite");
    }

    #[test]
    fn parse_network_dsn() {
        let p = ConnectionConfig::parse_conn_str("mysql://root:secret@db.host:3307/shop").unwrap();
        assert_eq!(p.kind, DbKind::Mysql);
        assert_eq!(p.host, "db.host");
        assert_eq!(p.port, Some(3307));
        assert_eq!(p.user, "root");
        assert_eq!(p.password, "secret");
        assert_eq!(p.database, "shop");
    }

    #[test]
    fn parse_dsn_defaults_port() {
        let p = ConnectionConfig::parse_conn_str("postgresql://localhost/mydb").unwrap();
        assert_eq!(p.kind, DbKind::Postgres);
        assert_eq!(p.host, "localhost");
        assert_eq!(p.port, Some(5432));
        assert!(p.user.is_empty());
        assert_eq!(p.database, "mydb");
    }

    #[test]
    fn parse_sqlite_dsn_keeps_path() {
        let p = ConnectionConfig::parse_conn_str("sqlite:///Users/me/data/app.db").unwrap();
        assert_eq!(p.kind, DbKind::Sqlite);
        assert_eq!(p.database, "/Users/me/data/app.db");
    }

    #[test]
    fn parse_rejects_non_url() {
        assert!(ConnectionConfig::parse_conn_str("not a url").is_none());
    }

    #[test]
    fn dsn_round_trips_through_parse() {
        let cfg = ConnectionConfig {
            kind: DbKind::Postgres,
            host: "h".into(),
            port: Some(5432),
            user: "u".into(),
            password: "pw".into(),
            database: "d".into(),
            ..Default::default()
        };
        let p = ConnectionConfig::parse_conn_str(&cfg.dsn()).unwrap();
        assert_eq!(p.host, "h");
        assert_eq!(p.user, "u");
        assert_eq!(p.password, "pw");
        assert_eq!(p.database, "d");
    }

    #[test]
    fn local_dsn_swaps_only_host_and_port() {
        // The tunnel rewrite keeps userinfo/database (and their encoding) intact,
        // changing only where the driver dials.
        let cfg = ConnectionConfig {
            kind: DbKind::Postgres,
            host: "remote.internal".into(),
            port: Some(5432),
            user: "u".into(),
            password: "p@ss".into(),
            database: "shop".into(),
            ssh: Some(SshConfig {
                host: "bastion".into(),
                port: 22,
                user: "jump".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            cfg.local_dsn("127.0.0.1", 49152),
            "postgres://u:p%40ss@127.0.0.1:49152/shop"
        );
    }

    #[test]
    fn ssh_debug_redacts_secrets() {
        let ssh = SshConfig {
            host: "bastion".into(),
            port: 22,
            user: "jump".into(),
            auth: SshAuth::Password,
            password: "hunter2".into(),
            passphrase: "swordfish".into(),
        };
        let shown = format!("{ssh:?}");
        assert!(!shown.contains("hunter2"), "ssh password leaked into Debug");
        assert!(
            !shown.contains("swordfish"),
            "ssh passphrase leaked into Debug"
        );
        assert!(shown.contains("<redacted>"));
    }
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    fn cv(column: &str, value: Value) -> ColumnValue {
        ColumnValue {
            column: column.into(),
            value,
            decl_type: None,
        }
    }

    #[test]
    fn preview_renders_readable_statements() {
        let table = TableRef {
            schema: Some("main".into()),
            name: "users".into(),
        };
        let update = EditOp::Update {
            table: table.clone(),
            key: cv("id", Value::Integer(7)),
            set: vec![cv("name", Value::Text("O'Brien".into()))],
        };
        // Identifiers double-quoted, the value single-quoted with the quote doubled.
        assert_eq!(
            update.preview_sql(),
            "UPDATE \"main\".\"users\" SET \"name\" = 'O''Brien' WHERE \"id\" = 7"
        );
        assert_eq!(update.verb(), "Update");

        let del = EditOp::Delete {
            table: table.clone(),
            key: cv("id", Value::Integer(7)),
        };
        assert_eq!(
            del.preview_sql(),
            "DELETE FROM \"main\".\"users\" WHERE \"id\" = 7"
        );

        let ins = EditOp::Insert {
            table,
            values: vec![cv("id", Value::Integer(2)), cv("name", Value::Null)],
        };
        assert_eq!(
            ins.preview_sql(),
            "INSERT INTO \"main\".\"users\" (\"id\", \"name\") VALUES (2, NULL)"
        );
    }

    #[test]
    fn coercion_follows_declared_type() {
        // Empty input clears the cell regardless of type.
        assert_eq!(coerce_edit_value("", Some("text")), Ok(Value::Null));
        // Integer / real columns parse; everything else is text.
        assert_eq!(
            coerce_edit_value("42", Some("integer")),
            Ok(Value::Integer(42))
        );
        assert_eq!(
            coerce_edit_value("3.5", Some("numeric")),
            Ok(Value::Real(3.5))
        );
        assert_eq!(
            coerce_edit_value("hi", Some("varchar(20)")),
            Ok(Value::Text("hi".into()))
        );
        // An untyped column keeps the text verbatim.
        assert_eq!(coerce_edit_value("x", None), Ok(Value::Text("x".into())));
        // A non-numeric value on an integer column is a coercion error (no preview).
        assert!(coerce_edit_value("abc", Some("int")).is_err());
    }
}

#[cfg(test)]
mod ai_policy_tests {
    use super::*;

    #[test]
    fn tier_ladder_gates_tools() {
        // off → nothing; schema → structure only; read → the full catalog.
        assert!(!AiTier::Off.allows_tool("list_schema"));
        assert!(AiTier::Schema.allows_tool("list_schema"));
        assert!(AiTier::Schema.allows_tool("describe_table"));
        assert!(!AiTier::Schema.allows_tool("run_select"));
        assert!(!AiTier::Schema.allows_tool("explain"));
        assert!(AiTier::Read.allows_tool("run_select"));
        assert!(AiTier::Read.allows_tool("explain"));
        assert!(AiTier::Read.allows_tool("generate_report"));
        assert!(AiTier::Read.allows_tool("open_query"));
        // The write tool is gated to the Write tier, and Write inherits the read
        // catalog on top of it.
        assert!(!AiTier::Read.allows_tool("propose_write"));
        assert!(AiTier::Write.allows_tool("propose_write"));
        assert!(AiTier::Write.allows_tool("run_select"));
        // Unknown tools are never allowed at any tier.
        assert!(!AiTier::Read.allows_tool("frobnicate"));
        assert!(!AiTier::Write.allows_tool("frobnicate"));
    }

    #[test]
    fn tier_parse_recognizes_known_tiers_and_fails_closed() {
        assert_eq!(AiTier::parse("off"), AiTier::Off);
        assert_eq!(AiTier::parse(" Schema "), AiTier::Schema);
        assert_eq!(AiTier::parse("READ"), AiTier::Read);
        assert_eq!(AiTier::parse("write"), AiTier::Write);
        // A typo or empty string fails CLOSED (Off), never to a permissive tier:
        // locking the assistant down must not silently grant read access.
        assert_eq!(AiTier::parse("nonsense"), AiTier::Off);
        assert_eq!(AiTier::parse("readonly"), AiTier::Off);
        assert_eq!(AiTier::parse(""), AiTier::Off);
    }

    #[test]
    fn overrides_layer_over_global() {
        let global = AiPolicy {
            enabled: true,
            tier: AiTier::Read,
            limits: AiLimits::default(),
            read_only: false,
        };
        // Unset overrides inherit the global policy verbatim.
        assert_eq!(global.with_overrides(None, None), global);
        // A connection can tighten the tier and flip the master switch without
        // touching the global policy; limits stay global.
        let tightened = global.with_overrides(Some(false), Some(AiTier::Schema));
        assert!(!tightened.enabled);
        assert_eq!(tightened.tier, AiTier::Schema);
        assert_eq!(tightened.limits, global.limits);
    }
}
