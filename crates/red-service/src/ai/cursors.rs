//! Live agent cursors: the registry that lets the assistant page a large result
//! instead of truncating it.
//!
//! RED's founding invariant is *never materialize a whole result -- stream it
//! through a windowed cursor*. The agent's reads were the one place that broke
//! it: `run_select` fetched, formatted, capped at a byte budget, and whatever
//! fell off the end was simply gone. The model's two options were to reason over
//! the first page as if it were the data, or to hand-roll `OFFSET` paging -- which
//! re-executes the query per page, is quadratic over the result, and silently
//! skips or double-counts rows whenever the ordering isn't total. That is the
//! exact pattern RED's own grid abandoned for keyset paging.
//!
//! So the grid's cursor is handed to the agent too. The tool is thin; **the
//! lifecycle is the work**, because an open cursor is a held connection and, on
//! some engines, a held snapshot. Every bound here exists to stop a model that
//! opens cursors and forgets them from pinning the pool:
//!
//! - a per-conversation cap and a process cap, both evicting the oldest;
//! - an idle reap, shorter than the ACP agent reap because a cursor is heavier
//!   than an idle subprocess;
//! - closure on `forget` (the conversation went away) and on cancel (the user
//!   stopped the turn);
//! - a total-rows guard per cursor, which reports itself rather than stopping
//!   quietly.
//!
//! Every close fires the cursor's own [`CancelToken`], so the engine-side fetch
//! stops rather than being dropped mid-flight.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use red_core::Column;
use red_driver::QueryCursor;

use crate::protocol::ConversationId;

/// Open cursors one conversation may hold. Four is more than any real reading
/// pattern needs and small enough that a forgetful model cannot pin the pool.
const MAX_PER_CONVERSATION: usize = 4;
/// Open cursors across the whole process, however many conversations are live.
const MAX_TOTAL: usize = 16;
/// How long a cursor may sit untouched before it is closed. Deliberately shorter
/// than the ACP agent's 15-minute reap: an idle subprocess costs memory, an idle
/// cursor costs a connection and possibly a snapshot.
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Multiplier on `AiLimits::max_rows` for the total a single cursor may yield.
/// A runaway guard rather than a budget -- at the default that is 100k rows, far
/// past any reading a turn should be doing, and it says so when it trips.
pub(in crate::ai) const ROWS_PER_CURSOR_FACTOR: u64 = 100;

/// The short opaque handle the model uses to continue a read.
///
/// Short on purpose: it round-trips through the model on every call, and a UUID
/// would be tokens spent on nothing a human ever reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::ai) struct CursorId(String);

impl CursorId {
    pub(in crate::ai) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CursorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One live cursor and everything needed to describe it without re-reading it.
pub(in crate::ai) struct AgentCursor {
    pub(in crate::ai) conversation_id: ConversationId,
    pub(in crate::ai) cursor: Box<dyn QueryCursor>,
    pub(in crate::ai) columns: Vec<Column>,
    pub(in crate::ai) rows_read: u64,
    pub(in crate::ai) last_used: Instant,
    /// The statement being read, so an expired handle can say *what* went stale
    /// rather than only that something did. A model told "cursor c3 is gone" can
    /// do nothing; one told "cursor c3 (SELECT … FROM orders) expired" can reopen it.
    pub(in crate::ai) sql: String,
}

impl AgentCursor {
    /// A one-line description for the "still open" line folded into the next
    /// turn's context.
    fn summary(&self, id: &CursorId) -> String {
        format!(
            "{id} ({}, {} rows read so far)",
            super::util::truncate_summary(self.sql.trim(), 60),
            self.rows_read
        )
    }
}

/// The process-wide cursor registry. Lives on `AiState` so it inherits the
/// existing `forget` cleanup path rather than growing a second lifecycle nobody
/// remembers to call.
#[derive(Default)]
pub(crate) struct CursorRegistry {
    open: HashMap<CursorId, AgentCursor>,
    /// Monotonic handle counter. Process-global rather than per-conversation, so
    /// two chats can never be handed the same `c3`.
    next: u64,
}

impl CursorRegistry {
    /// Register `cursor` and return the handle to give the model.
    ///
    /// Evicts to stay inside both caps first, oldest-used first, closing whatever
    /// it drops. Eviction is silent to the evicted conversation by design: the
    /// stale handle's next `fetch_more` reports itself, which is the moment the
    /// model can actually act on it.
    pub(in crate::ai) fn open(
        &mut self,
        conversation_id: ConversationId,
        cursor: Box<dyn QueryCursor>,
        columns: Vec<Column>,
        sql: String,
    ) -> CursorId {
        self.evict_for(conversation_id);
        self.next += 1;
        let id = CursorId(format!("c{}", self.next));
        self.open.insert(
            id.clone(),
            AgentCursor {
                conversation_id,
                cursor,
                columns,
                rows_read: 0,
                last_used: Instant::now(),
                sql,
            },
        );
        id
    }

    /// Take a cursor out for a read.
    ///
    /// Out, not borrowed: `next_window` is awaited, and the registry lives behind
    /// a sync mutex that must not be held across an await. Removal also serializes
    /// use of one cursor, which a forward-only cursor wants anyway -- two
    /// concurrent reads of the same handle would interleave rows.
    pub(in crate::ai) fn take(&mut self, id: &str) -> Option<AgentCursor> {
        self.open.remove(&CursorId(id.to_string()))
    }

    /// Put a cursor back after a read, refreshing its idle clock.
    pub(in crate::ai) fn put_back(&mut self, id: &str, mut entry: AgentCursor) {
        entry.last_used = Instant::now();
        self.open.insert(CursorId(id.to_string()), entry);
    }

    /// Close every cursor a conversation holds. Called from `AiState::forget`
    /// (the chat was closed or deleted) and on cancel (the user stopped the turn).
    pub(in crate::ai) fn close_conversation(&mut self, conversation_id: ConversationId) {
        let doomed: Vec<CursorId> = self
            .open
            .iter()
            .filter(|(_, c)| c.conversation_id == conversation_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in doomed {
            self.close(&id);
        }
    }

    /// Close every cursor left untouched for [`IDLE_TIMEOUT`], returning how many
    /// went. A cursor nobody has read from in five minutes is one the model has
    /// moved on from.
    pub(crate) fn reap_idle(&mut self) -> usize {
        let now = Instant::now();
        let stale: Vec<CursorId> = self
            .open
            .iter()
            .filter(|(_, c)| now.duration_since(c.last_used) >= IDLE_TIMEOUT)
            .map(|(id, _)| id.clone())
            .collect();
        let n = stale.len();
        for id in stale {
            self.close(&id);
        }
        n
    }

    /// The "these are still open" line for the next turn's context, or `None`.
    ///
    /// A cursor may legitimately outlive the turn that opened it -- "keep going"
    /// is a real follow-up -- but it must never do so *silently*, or the model
    /// guesses at handles. This is volatile per-turn context, which is why it
    /// rides in the user message rather than the cached system prompt.
    pub(in crate::ai) fn open_line(&self, conversation_id: ConversationId) -> Option<String> {
        let mut live: Vec<(&CursorId, &AgentCursor)> = self
            .open
            .iter()
            .filter(|(_, c)| c.conversation_id == conversation_id)
            .collect();
        if live.is_empty() {
            return None;
        }
        // Stable order, so the same set reads the same way twice.
        live.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        let list: Vec<String> = live.iter().map(|(id, c)| c.summary(id)).collect();
        Some(format!(
            "Cursors still open from earlier in this conversation: {}. Continue one with \
             fetch_more, or ignore them and they expire.",
            list.join("; ")
        ))
    }

    /// How many cursors are open.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.open.len()
    }

    /// Close one cursor, firing its engine-side cancel so an in-flight fetch stops
    /// rather than being dropped mid-flight.
    fn close(&mut self, id: &CursorId) {
        if let Some(entry) = self.open.remove(id) {
            entry.cursor.cancel_token().cancel();
        }
    }

    /// Make room for one more cursor: drop this conversation's oldest past its
    /// own cap, then the process's oldest past the global one.
    fn evict_for(&mut self, conversation_id: ConversationId) {
        while self
            .open
            .values()
            .filter(|c| c.conversation_id == conversation_id)
            .count()
            >= MAX_PER_CONVERSATION
        {
            let Some(oldest) = self.oldest(Some(conversation_id)) else {
                break;
            };
            self.close(&oldest);
        }
        while self.open.len() >= MAX_TOTAL {
            let Some(oldest) = self.oldest(None) else {
                break;
            };
            self.close(&oldest);
        }
    }

    /// The least recently used handle, optionally within one conversation.
    fn oldest(&self, conversation_id: Option<ConversationId>) -> Option<CursorId> {
        self.open
            .iter()
            .filter(|(_, c)| conversation_id.is_none_or(|w| c.conversation_id == w))
            .min_by_key(|(_, c)| c.last_used)
            .map(|(id, _)| id.clone())
    }
}

/// Ceiling on one round trip while filling a window, and the size of the first
/// (measuring) pull.
///
/// The window's real bound is bytes, and a cursor cannot be rewound: rows pulled
/// past the budget are consumed and must still be shown, or they would be lost --
/// exactly the truncation this feature replaces. So the fill pulls a small probe
/// chunk, measures what a row actually costs, and sizes every later pull to the
/// budget that is left. Overshoot is bounded by one chunk's *estimate* rather than
/// by one `max_rows` of arbitrarily wide rows.
const FILL_CHUNK_MAX: usize = 100;
const FILL_CHUNK_PROBE: usize = 10;

/// What one window read produced.
pub(in crate::ai) struct Window {
    /// The rendered rows, header included.
    pub(in crate::ai) text: String,
    pub(in crate::ai) rows: usize,
    /// No more rows behind this window.
    pub(in crate::ai) exhausted: bool,
    /// The per-cursor total-rows guard tripped; the read stopped short.
    pub(in crate::ai) hit_row_cap: bool,
}

/// Fill one window from `entry`, bounded by rows *and* bytes.
///
/// A window is `min(max_rows, whatever fits the byte budget)`. A table with a wide
/// text column may yield forty rows where a narrow one yields a thousand, and that
/// is the correct behaviour rather than a bug: the budget is bytes, and rows are
/// whatever fits in them.
///
/// Unlike the old single-shot read, nothing is lost at the boundary -- the cursor
/// keeps its place, so the rows that did not fit are the next window rather than
/// gone.
pub(in crate::ai) async fn fill_window(
    entry: &mut AgentCursor,
    max_rows: usize,
    max_bytes: usize,
    row_cap: u64,
) -> red_core::Result<Window> {
    // Stop filling at nine tenths of the budget so the overshoot from the final
    // chunk still lands inside it; the caller must never have to truncate this
    // text, since truncation here would drop rows the cursor has already yielded.
    let soft_cap = if max_bytes == 0 {
        usize::MAX
    } else {
        max_bytes - max_bytes / 10
    };
    let header: Vec<&str> = entry.columns.iter().map(|c| c.name.as_str()).collect();
    let mut text = format!("{}\n", header.join(" | "));
    let (mut rows, mut exhausted, mut hit_row_cap) = (0usize, false, false);

    let mut chunk = FILL_CHUNK_PROBE;
    while rows < max_rows.max(1) {
        if entry.rows_read >= row_cap {
            hit_row_cap = true;
            break;
        }
        let want = chunk
            .max(1)
            .min(max_rows.max(1) - rows)
            .min((row_cap - entry.rows_read) as usize);
        let window = entry.cursor.next_window(want).await?;
        for row in &window.rows {
            let cells: Vec<String> = row.iter().map(super::sql::format::render_cell).collect();
            text.push_str(&cells.join(" | "));
            text.push('\n');
        }
        rows += window.rows.len();
        entry.rows_read += window.rows.len() as u64;
        if window.exhausted {
            exhausted = true;
            break;
        }
        if text.len() >= soft_cap {
            break;
        }
        // Size the next pull from what a row has actually cost so far, so a wide
        // column narrows the window instead of blowing past the budget in one go.
        if let Some(per_row) = text.len().checked_div(rows) {
            let per_row = per_row.max(1);
            chunk = ((soft_cap - text.len()) / per_row).clamp(1, FILL_CHUNK_MAX);
        }
    }
    Ok(Window {
        text,
        rows,
        exhausted,
        hit_row_cap,
    })
}

/// The sentence after a window that says whether there is more, and how to get it.
///
/// Registers the cursor when there is, and closes it when there isn't -- an
/// exhausted cursor is a connection held for nothing.
pub(in crate::ai) fn continuation(
    window: &Window,
    entry: AgentCursor,
    state: &Arc<Mutex<super::state::AiState>>,
) -> String {
    if !window.exhausted && !window.hit_row_cap {
        // More to read: keep the cursor alive and hand back its handle. The
        // discipline goes in the text, because the failure mode of a paging tool
        // is a model that pages the whole result into context and then hits the
        // ceiling.
        let conversation_id = entry.conversation_id;
        let rows_read = entry.rows_read;
        let id = super::state::lock(state).cursors.open(
            conversation_id,
            entry.cursor,
            entry.columns,
            entry.sql,
        );
        // The registry starts a fresh count; carry over what this window already read.
        if let Some(mut e) = super::state::lock(state).cursors.take(id.as_str()) {
            e.rows_read = rows_read;
            super::state::lock(state).cursors.put_back(id.as_str(), e);
        }
        return format!(
            "\n({} rows in this window; more remain. Call fetch_more with cursor \"{id}\" to read \
             the next one. Reading a window drops the previous one from your context, so \
             summarize as you go rather than trying to hold it all.)",
            window.rows
        );
    }
    if window.hit_row_cap {
        return format!(
            "\n({} rows in this window; {} read from this query in total, which is the per-read \
             ceiling. The result has more rows than the agent will page through in one go - \
             narrow the query, or aggregate it in SQL instead of reading it all.)",
            window.rows, entry.rows_read
        );
    }
    if window.exhausted {
        return format!(
            "\n({} rows in this window; {} rows in total. That is the whole result.)",
            window.rows, entry.rows_read
        );
    }
    String::new()
}

/// Run one `fetch_more` call.
pub(in crate::ai) async fn fetch_more(
    input: &serde_json::Value,
    limits: &red_core::AiLimits,
    state: &Arc<Mutex<super::state::AiState>>,
) -> (String, bool) {
    let handle = input
        .get("cursor")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if handle.is_empty() {
        return ("error: fetch_more needs a `cursor` handle".into(), false);
    }
    // Out of the registry for the read: the lock cannot be held across the await,
    // and removing it also stops two concurrent reads interleaving one cursor's
    // rows.
    let Some(mut entry) = super::state::lock(state).cursors.take(&handle) else {
        return (
            format!(
                "error: cursor {handle} is no longer open - it was finished, superseded by newer \
                 cursors, or timed out. Re-run the query with run_select to start again."
            ),
            false,
        );
    };
    let max_rows = limits.max_rows.max(1);
    let limit = input
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .map_or(max_rows, |n| (n as usize).clamp(1, max_rows));
    let row_cap = (max_rows as u64).saturating_mul(ROWS_PER_CURSOR_FACTOR);

    match fill_window(&mut entry, limit, limits.max_result_bytes, row_cap).await {
        Ok(window) => {
            let mut out = String::new();
            out.push_str(&window.text);
            // Put the *same* handle back rather than minting a new one: the model
            // is mid-loop and a changing handle each round is a needless way to
            // lose its place.
            if !window.exhausted && !window.hit_row_cap {
                let rows = window.rows;
                let read = entry.rows_read;
                super::state::lock(state).cursors.put_back(&handle, entry);
                out.push_str(&format!(
                    "\n({rows} rows in this window; {read} read so far. More remain: call \
                     fetch_more with cursor \"{handle}\" again. Summarize what you have before \
                     you do.)"
                ));
            } else {
                out.push_str(&continuation(&window, entry, state));
            }
            (out, true)
        }
        Err(e) => {
            // A failed read leaves the cursor in an unknown position, so it is not
            // put back: continuing from an unknown place would silently skip rows.
            (
                format!("error: reading the next window failed: {e}. The cursor is closed."),
                false,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_core::RowWindow;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// An unknown or expired handle is a recoverable tool error naming what to do
    /// next, never a panic: the model can re-run the query, but only if it is
    /// told that is the way out.
    #[tokio::test]
    async fn an_unknown_handle_is_a_recoverable_error() {
        let state = Arc::new(Mutex::new(super::super::state::AiState::default()));
        let (msg, ok) = fetch_more(
            &serde_json::json!({ "cursor": "c99" }),
            &red_core::AiLimits::default(),
            &state,
        )
        .await;
        assert!(!ok);
        assert!(msg.contains("c99"), "{msg}");
        assert!(msg.contains("run_select"), "the way out is named: {msg}");

        // A missing handle is refused just as clearly.
        let (msg, ok) = fetch_more(
            &serde_json::json!({}),
            &red_core::AiLimits::default(),
            &state,
        )
        .await;
        assert!(!ok);
        assert!(msg.contains("cursor"), "{msg}");
    }

    /// Paging a seeded result must tile it **exactly**: every row once, in order,
    /// no gaps and no repeats. This is the property the whole feature rests on and
    /// the one `OFFSET` paging cannot hold once the ordering is not total.
    #[tokio::test]
    async fn windows_tile_the_result_exactly() {
        let driver: std::sync::Arc<dyn red_driver::DatabaseDriver> =
            std::sync::Arc::new(red_driver::SqliteDriver::new(":memory:", true));
        const TOTAL: i64 = 5_000;
        let sql = format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < {TOTAL}) \
             SELECT x FROM c"
        );
        let cursor = driver
            .open_cursor(&sql, red_core::QueryOptions::default())
            .await
            .unwrap();
        let columns = cursor.columns().to_vec();
        let mut entry = AgentCursor {
            conversation_id: ConversationId::new(1),
            cursor,
            columns,
            rows_read: 0,
            last_used: Instant::now(),
            sql: sql.clone(),
        };

        // Read it in small windows, collecting the values back out of the rendered
        // text: what the model sees is what gets asserted.
        let mut seen: Vec<i64> = Vec::new();
        let mut windows = 0;
        loop {
            let w = fill_window(&mut entry, 350, 64 * 1024, 1_000_000)
                .await
                .unwrap();
            windows += 1;
            for line in w.text.lines().skip(1) {
                if let Ok(n) = line.trim().parse::<i64>() {
                    seen.push(n);
                }
            }
            if w.exhausted {
                break;
            }
            assert!(windows < 100, "should not take 100 windows");
        }
        assert!(windows > 1, "the point is that it took several windows");
        assert_eq!(seen.len() as i64, TOTAL, "every row came back exactly once");
        assert_eq!(entry.rows_read, TOTAL as u64);
        // In order, no gaps, no duplicates.
        assert!(
            seen.iter().enumerate().all(|(i, n)| *n == i as i64 + 1),
            "windows must tile the result in order"
        );
    }

    /// The window's real bound is bytes, so a wide column yields fewer rows than
    /// `max_rows` -- and the windows still tile.
    #[tokio::test]
    async fn a_wide_column_yields_fewer_rows_per_window() {
        let driver: std::sync::Arc<dyn red_driver::DatabaseDriver> =
            std::sync::Arc::new(red_driver::SqliteDriver::new(":memory:", true));
        // 400 rows of ~500 bytes each: far more bytes than the budget below, far
        // fewer rows than the row cap.
        let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 400) \
                   SELECT x, printf('%.500d', x) AS wide FROM c";
        let cursor = driver
            .open_cursor(sql, red_core::QueryOptions::default())
            .await
            .unwrap();
        let columns = cursor.columns().to_vec();
        let mut entry = AgentCursor {
            conversation_id: ConversationId::new(1),
            cursor,
            columns,
            rows_read: 0,
            last_used: Instant::now(),
            sql: sql.into(),
        };
        // Row budget 400, byte budget ~20KB: bytes bind first.
        let w = fill_window(&mut entry, 400, 20 * 1024, 1_000_000)
            .await
            .unwrap();
        assert!(!w.exhausted, "the byte budget stopped it short of the end");
        assert!(w.rows < 400, "bytes bound before rows: got {} rows", w.rows);
        assert!(w.rows > 0);
        assert!(
            w.text.len() <= 20 * 1024,
            "the window stays inside the hard budget: {} bytes",
            w.text.len()
        );

        // And the rest is still readable rather than lost, which is the whole
        // difference from the truncation this replaced.
        let mut total = w.rows;
        loop {
            let next = fill_window(&mut entry, 400, 20 * 1024, 1_000_000)
                .await
                .unwrap();
            total += next.rows;
            if next.exhausted {
                break;
            }
        }
        assert_eq!(total, 400, "the windows tile the whole result");
    }

    /// The per-cursor runaway guard stops the read and says so, rather than
    /// stopping quietly and letting the model treat a partial as complete.
    #[tokio::test]
    async fn the_row_cap_reports_itself() {
        let driver: std::sync::Arc<dyn red_driver::DatabaseDriver> =
            std::sync::Arc::new(red_driver::SqliteDriver::new(":memory:", true));
        let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 5000) \
                   SELECT x FROM c";
        let cursor = driver
            .open_cursor(sql, red_core::QueryOptions::default())
            .await
            .unwrap();
        let columns = cursor.columns().to_vec();
        let mut entry = AgentCursor {
            conversation_id: ConversationId::new(1),
            cursor,
            columns,
            rows_read: 0,
            last_used: Instant::now(),
            sql: sql.into(),
        };
        let w = fill_window(&mut entry, 5000, 0, 250).await.unwrap();
        assert!(w.hit_row_cap);
        assert_eq!(entry.rows_read, 250);
        let note = continuation(
            &w,
            entry,
            &Arc::new(Mutex::new(super::super::state::AiState::default())),
        );
        assert!(note.contains("per-read ceiling"), "{note}");
    }

    /// A cursor that yields nothing and records whether it was cancelled, so the
    /// registry's closing behaviour can be asserted directly rather than inferred
    /// from the map being empty.
    struct StubCursor {
        cancelled: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl QueryCursor for StubCursor {
        fn columns(&self) -> &[Column] {
            &[]
        }
        async fn next_window(&self, _max: usize) -> red_core::Result<RowWindow> {
            Ok(RowWindow {
                rows: Vec::new(),
                exhausted: true,
            })
        }
        fn cancel_token(&self) -> red_driver::CancelToken {
            let flag = self.cancelled.clone();
            red_driver::CancelToken::new(move || flag.store(true, Ordering::SeqCst))
        }
    }

    fn stub() -> (Box<dyn QueryCursor>, Arc<AtomicBool>) {
        let cancelled = Arc::new(AtomicBool::new(false));
        (
            Box::new(StubCursor {
                cancelled: cancelled.clone(),
            }),
            cancelled,
        )
    }

    fn open(reg: &mut CursorRegistry, conv: u64) -> (CursorId, Arc<AtomicBool>) {
        let (cursor, cancelled) = stub();
        let id = reg.open(
            ConversationId::new(conv),
            cursor,
            Vec::new(),
            "SELECT * FROM orders".into(),
        );
        (id, cancelled)
    }

    /// The per-conversation cap evicts the oldest rather than refusing, and the
    /// evicted cursor is *closed* -- a dropped handle that left its connection
    /// pinned would be the whole hazard this registry exists for.
    #[test]
    fn a_fifth_cursor_closes_the_first() {
        let mut reg = CursorRegistry::default();
        let (first, first_cancelled) = open(&mut reg, 1);
        for _ in 0..(MAX_PER_CONVERSATION - 1) {
            open(&mut reg, 1);
        }
        assert_eq!(reg.len(), MAX_PER_CONVERSATION);

        open(&mut reg, 1);
        assert_eq!(reg.len(), MAX_PER_CONVERSATION, "the cap holds");
        assert!(
            reg.take(first.as_str()).is_none(),
            "the oldest handle is gone"
        );
        assert!(
            first_cancelled.load(Ordering::SeqCst),
            "an evicted cursor must be cancelled, not merely forgotten"
        );
    }

    /// The process cap bounds the total however many conversations are live.
    #[test]
    fn the_process_cap_bounds_every_conversation_together() {
        let mut reg = CursorRegistry::default();
        for conv in 0..20u64 {
            open(&mut reg, conv);
        }
        assert!(reg.len() <= MAX_TOTAL, "{} open", reg.len());
    }

    /// Closing a conversation closes its cursors and leaves everyone else's alone.
    #[test]
    fn closing_a_conversation_spares_the_others() {
        let mut reg = CursorRegistry::default();
        let (mine, mine_cancelled) = open(&mut reg, 1);
        let (theirs, theirs_cancelled) = open(&mut reg, 2);

        reg.close_conversation(ConversationId::new(1));
        assert!(reg.take(mine.as_str()).is_none());
        assert!(mine_cancelled.load(Ordering::SeqCst));
        assert!(!theirs_cancelled.load(Ordering::SeqCst));
        assert!(reg.take(theirs.as_str()).is_some(), "{theirs} survives");
    }

    /// An idle cursor is reaped and its handle invalidated.
    #[test]
    fn an_idle_cursor_is_reaped() {
        let mut reg = CursorRegistry::default();
        let (id, cancelled) = open(&mut reg, 1);
        assert_eq!(reg.reap_idle(), 0, "a fresh cursor is not idle");

        // Backdate it past the timeout rather than sleeping five minutes.
        if let Some(entry) = reg.open.get_mut(&id) {
            entry.last_used = Instant::now() - IDLE_TIMEOUT - Duration::from_secs(1);
        }
        assert_eq!(reg.reap_idle(), 1);
        assert!(reg.take(id.as_str()).is_none());
        assert!(cancelled.load(Ordering::SeqCst));
    }

    /// The context line names every live handle with what it is reading, so the
    /// model continues one rather than guessing at a name.
    #[test]
    fn the_open_line_names_the_handles_and_their_statements() {
        let mut reg = CursorRegistry::default();
        assert_eq!(reg.open_line(ConversationId::new(1)), None);

        let (id, _) = open(&mut reg, 1);
        let line = reg
            .open_line(ConversationId::new(1))
            .expect("a live cursor is announced");
        assert!(line.contains(id.as_str()), "{line}");
        assert!(line.contains("SELECT * FROM orders"), "{line}");
        assert!(line.contains("fetch_more"), "{line}");
        // Another conversation sees nothing of it.
        assert_eq!(reg.open_line(ConversationId::new(2)), None);
    }

    /// Taking a cursor out and putting it back refreshes its idle clock, so a
    /// cursor being actively paged is never reaped mid-read.
    #[test]
    fn using_a_cursor_refreshes_its_idle_clock() {
        let mut reg = CursorRegistry::default();
        let (id, _) = open(&mut reg, 1);
        if let Some(entry) = reg.open.get_mut(&id) {
            entry.last_used = Instant::now() - IDLE_TIMEOUT - Duration::from_secs(1);
        }
        let entry = reg.take(id.as_str()).expect("still open");
        reg.put_back(id.as_str(), entry);
        assert_eq!(reg.reap_idle(), 0, "a just-read cursor is not idle");
    }
}
