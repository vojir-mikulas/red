//! The database abstraction layer. `DatabaseDriver` is RED's analogue of Nyx's
//! `RemoteClient`: an object-safe trait the service holds as `Arc<dyn …>` and
//! drives across many commands, with one impl per engine.
//!
//! Nothing here materializes a whole result: queries run behind a windowed
//! [`QueryCursor`], paging is random-access (`fetch_page`) or indexed-seek
//! (`fetch_seek`), and `export` streams row-by-row. This keeps memory flat over
//! results of any size, the layer's central performance contract.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use red_core::{
    Column, ExportFormat, KeySpec, QueryOptions, RedError, Result, ResultPage, RowWindow,
    SchemaMeta, TableDetail, Value,
};
use tokio::sync::mpsc::UnboundedSender;

#[cfg(test)]
mod conformance;
mod format;
mod mysql;
mod pg_text;
mod postgres;
mod sqlite;
pub use mysql::MysqlDriver;
pub use postgres::PostgresDriver;
pub use sqlite::SqliteDriver;

/// Default bytes of a non-key cell's content a *display* fetch keeps; past it,
/// text is truncated to a [`Value::Capped`] prefix and a blob to its length only.
/// The resident-cell budget that keeps the grid's RAM flat over fat `TEXT`/`BLOB`
/// columns — the driver never materializes the over-cap bytes, so a page of huge
/// cells can't spike the channel. The live value is [`display_cell_cap`], driven
/// by `grid.max_cell_chars` via [`set_display_cell_cap`].
pub const DEFAULT_DISPLAY_CELL_CAP: usize = 4096;

/// The live display cap (see [`DEFAULT_DISPLAY_CELL_CAP`]). A process-global so a
/// settings change applies to every subsequent display fetch across all sessions
/// without threading the value through each `fetch_page`/`fetch_seek` call; the
/// service sets it from `grid.max_cell_chars` on launch and on every reload.
static DISPLAY_CELL_CAP: AtomicUsize = AtomicUsize::new(DEFAULT_DISPLAY_CELL_CAP);

/// Set the live display cap (bytes of a non-key cell a display fetch keeps). The
/// caller clamps to a sane range; export fetches ([`PageCap::Full`]) ignore it.
pub fn set_display_cell_cap(bytes: usize) {
    DISPLAY_CELL_CAP.store(bytes.max(1), Ordering::Relaxed);
}

/// The live display cap currently in effect.
pub(crate) fn display_cell_cap() -> usize {
    DISPLAY_CELL_CAP.load(Ordering::Relaxed)
}

/// Whether [`DatabaseDriver::fetch_page`] caps oversized cells. The display path
/// caps non-key cells to [`display_cell_cap`]; the clipboard re-fetch wants the
/// real values, so it asks for `Full`. (The seek paths are always display-capped
/// and learn their exempt key from their own `KeySpec` argument.)
#[derive(Clone)]
pub enum PageCap {
    /// Cap non-key cells; `key` (when the result is keyed) rides through verbatim
    /// so its bytes round-trip as a seek bound.
    Display { key: Option<KeySpec> },
    /// No cap — full-fidelity rows, for the clipboard re-fetch.
    Full,
}

/// The positional, per-extraction form of a display cap: the byte budget plus the
/// result-column indices of the key columns (exempt — each rides back verbatim as
/// a seek bound, so its value must round-trip exactly). A composite sort key has
/// two exempt columns (lead + tiebreaker); a plain browse has one.
/// `None` everywhere a fetch is full-fidelity (export / clipboard re-fetch).
#[derive(Clone, Copy)]
pub(crate) struct CellCap {
    pub(crate) max_bytes: usize,
    pub(crate) key_cols: [Option<usize>; 2],
}

impl CellCap {
    /// Resolve a [`PageCap`] against a result's columns into a positional cap.
    /// `Full` → `None` (nothing capped); `Display` → the key's column indices (the
    /// present ones) are the exempt columns.
    pub(crate) fn resolve(cap: &PageCap, columns: &[Column]) -> Option<CellCap> {
        match cap {
            PageCap::Full => None,
            PageCap::Display { key } => Some(CellCap {
                max_bytes: display_cell_cap(),
                key_cols: key
                    .as_ref()
                    .map(|k| key_positions(k, columns))
                    .unwrap_or([None, None]),
            }),
        }
    }

    /// The display cap a seek/cursor fetch applies: always on, exempting the key
    /// columns (see [`key_positions`]).
    pub(crate) fn display(key_cols: [Option<usize>; 2]) -> Option<CellCap> {
        Some(CellCap {
            max_bytes: display_cell_cap(),
            key_cols,
        })
    }

    /// Whether column `i` is capped under `cap` (`None` cap → nothing capped; a key
    /// column → never capped).
    pub(crate) fn caps(cap: Option<CellCap>, i: usize) -> Option<usize> {
        match cap {
            Some(c) if !c.key_cols.contains(&Some(i)) => Some(c.max_bytes),
            _ => None,
        }
    }
}

/// The result-column indices of `key`'s columns (lead, then tiebreaker), used to
/// exempt them from the display cap. A missing column resolves to `None`.
pub(crate) fn key_positions(key: &KeySpec, columns: &[Column]) -> [Option<usize>; 2] {
    let find = |name: &str| columns.iter().position(|c| c.name == name);
    [find(&key.column), key.tiebreak.as_deref().and_then(find)]
}

/// Build a seek's `WHERE (cols) cmp (ph…)` and `ORDER BY cols dir` clauses, shared
/// across the three drivers (only quoting and placeholder syntax differ). The seek
/// is a single row-value comparison over the leading `bound_len` key columns, so
/// every column shares one direction: the key's [`descending`](KeySpec::descending)
/// sort, XOR'd with `scroll_descending` (the up/down scroll direction).
/// `inclusive` picks `>=`/`<=` (the `fetch_seek_skip` lower bound) over `>`/`<`.
///
/// `quote` quotes one identifier; `placeholder(i)` renders the `i`-th (0-based)
/// bind slot (e.g. `?` or `$1::int8`). `bound_len == 0` yields an empty `WHERE`
/// (a first/last page). The returned `WHERE` clause carries a trailing space so it
/// drops cleanly before `ORDER BY` when present.
pub(crate) fn seek_clauses(
    key: &KeySpec,
    bound_len: usize,
    scroll_descending: bool,
    inclusive: bool,
    quote: impl Fn(&str) -> String,
    placeholder: impl Fn(usize) -> String,
) -> (String, String) {
    let cols: Vec<String> = key.column_names().iter().map(|c| quote(c)).collect();
    let descending = key.descending ^ scroll_descending;
    let (strict, dir) = if descending {
        ("<", "DESC")
    } else {
        (">", "ASC")
    };
    let cmp = if inclusive {
        format!("{strict}=")
    } else {
        strict.to_string()
    };
    let order_by = cols
        .iter()
        .map(|c| format!("{c} {dir}"))
        .collect::<Vec<_>>()
        .join(", ");
    let where_clause = if bound_len == 0 {
        String::new()
    } else {
        let lhs = cols[..bound_len].join(", ");
        let rhs = (0..bound_len)
            .map(&placeholder)
            .collect::<Vec<_>>()
            .join(", ");
        format!("WHERE ({lhs}) {cmp} ({rhs}) ")
    };
    (where_clause, order_by)
}

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
    /// the grid show a real scrollbar without holding every row. `abort` cancels
    /// the (potentially full-table) scan out-of-band when the result is superseded.
    async fn count(&self, sql: &str, abort: &AbortSignal) -> Result<i64>;

    /// A random-access `(offset, limit)` page of `sql`'s result. Backs the grid's
    /// load-on-scroll so memory stays flat: only the pages around the viewport are
    /// ever resident. `cap` chooses display capping (the common scroll path) or
    /// full fidelity (the clipboard re-fetch) — see [`PageCap`]. `abort` cancels a
    /// superseded fetch (a flung scrollbar, a closed tab) at the engine.
    async fn fetch_page(
        &self,
        sql: &str,
        offset: usize,
        limit: usize,
        cap: PageCap,
        abort: &AbortSignal,
    ) -> Result<ResultPage>;

    /// One keyset (seek) page of `sql`'s result, ordered by `key`'s column tuple:
    /// the rows strictly after (`descending = false`) or strictly before
    /// (`descending = true`, returned in reverse order — the caller flips)
    /// `bound`; `None` starts from the result's first/last row. An indexed seek,
    /// so it costs the same at row 200 or 46,000,000 — unlike `fetch_page`'s
    /// O(offset).
    ///
    /// `descending` is the *scroll* direction; it composes (XOR) with the key's
    /// own [`descending`](KeySpec::descending) sort direction. `bound` carries one
    /// value per leading key column — the full tuple for a contiguous seek, or
    /// just the lead value for a key-space interpolation jump (a prefix
    /// comparison). Each value is bound as a real parameter, never interpolated.
    async fn fetch_seek(
        &self,
        sql: &str,
        key: &KeySpec,
        bound: Option<&[Value]>,
        descending: bool,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage>;

    /// One keyset page from an inclusive lower bound with a bounded `skip`:
    /// rows at or after `from` in the key's sort order, skipping `skip` then
    /// taking `limit`. `from = None` starts at the result's first row. The lower
    /// bound is an *indexed* seek, so the `OFFSET skip` only walks within the
    /// post-seek window — O(skip), not O(offset-from-row-0). Backs exact "go to
    /// row N" jumps and the checkpoint-index build, which both seek to a known
    /// key and then step a bounded number of rows. `from` carries one value per
    /// leading key column, each bound as a real parameter.
    async fn fetch_seek_skip(
        &self,
        sql: &str,
        key: &KeySpec,
        from: Option<&[Value]>,
        skip: usize,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage>;

    /// `MIN`/`MAX` of `key` over `sql`'s result — one indexed probe, backing
    /// key-space interpolation for fraction jumps. `None` when the result is
    /// empty or the key isn't an integer (not interpolable). `abort` cancels the
    /// probe out-of-band when the open it belongs to is superseded.
    async fn key_bounds(
        &self,
        sql: &str,
        key: &KeySpec,
        abort: &AbortSignal,
    ) -> Result<Option<(i64, i64)>>;

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

/// A caller-created abort handle for one in-flight one-shot fetch (`count`,
/// `fetch_page`, `fetch_seek`, `fetch_seek_skip`, `key_bounds`). The service makes
/// one per cancellable fetch, keeps a clone, and calls [`abort`](Self::abort) when
/// that fetch is superseded — a flung scrollbar, a re-sort, a closed tab.
///
/// Where [`CancelToken`] is produced *by* the driver (the streaming cursor hands
/// one back), a one-shot `async fn` can't return a handle before it's awaited — so
/// this inverts it: the caller owns the handle and the driver [`arm`](Self::arm)s
/// it with an engine [`CancelToken`] for the fetch's lifetime. The arm is dropped
/// when the fetch returns ([`ArmGuard`]), so a late `abort` after completion — the
/// connection already back in a pool and reused — is a harmless no-op.
///
/// A single signal can be armed by several concurrent fetches (the open probe runs
/// `count` + `fetch_page` + `key_bounds` together under one signal); `abort` fires
/// every armed token.
#[derive(Clone, Default)]
pub struct AbortSignal(Arc<AbortState>);

#[derive(Default)]
struct AbortState {
    aborted: AtomicBool,
    next_id: AtomicU64,
    /// The engine cancels currently armed (one per in-flight fetch sharing this
    /// signal), each tagged with a unique id so its [`ArmGuard`] removes only its own.
    armed: Mutex<Vec<(u64, CancelToken)>>,
}

impl AbortSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Supersede every fetch armed on this signal: fire each armed engine cancel
    /// and latch the aborted state so a fetch that arms *after* this dies at once.
    /// Idempotent and non-blocking.
    pub fn abort(&self) {
        let armed = lock(&self.0.armed);
        self.0.aborted.store(true, Ordering::SeqCst);
        for (_, token) in armed.iter() {
            token.cancel();
        }
    }

    /// Whether [`abort`](Self::abort) has fired. Drivers check this right before the
    /// engine call so a fetch superseded *before* it starts bails immediately —
    /// some engines no-op an out-of-band cancel with nothing yet running.
    pub fn is_aborted(&self) -> bool {
        self.0.aborted.load(Ordering::SeqCst)
    }

    /// Driver side: install `token` as this fetch's engine cancel for the duration
    /// of the returned guard. If the signal already aborted, fire `token` now (the
    /// arm-after-abort race). Cancel/arm are serialized on the same lock so a
    /// concurrent `abort` can't slip between the check and the install.
    pub(crate) fn arm(&self, token: CancelToken) -> ArmGuard {
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let mut armed = lock(&self.0.armed);
        if self.0.aborted.load(Ordering::SeqCst) {
            token.cancel();
        }
        armed.push((id, token));
        ArmGuard {
            state: self.0.clone(),
            id,
        }
    }
}

/// Disarms its fetch's engine cancel on drop (fetch completion), so a later
/// `abort` can't reach a connection that's since been returned to a pool/reused.
pub(crate) struct ArmGuard {
    state: Arc<AbortState>,
    id: u64,
}

impl Drop for ArmGuard {
    fn drop(&mut self) {
        lock(&self.state.armed).retain(|(id, _)| *id != self.id);
    }
}

/// Lock a mutex, tolerating poison — the armed-list critical sections can't panic,
/// but recovering the guard keeps a stray panic elsewhere from wedging cancels.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Map any error carrying a message into a driver error. Small helper so impls
/// stay terse.
pub(crate) fn driver_err(e: impl std::fmt::Display) -> RedError {
    RedError::Driver(e.to_string())
}
