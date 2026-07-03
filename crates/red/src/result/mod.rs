//! The result grid: a virtualized, horizontally-scrolling table backed by a
//! random-access window buffer. The grid never holds the whole result — its
//! load-on-scroll callback fetches the pages around the viewport and evicts the
//! rest, so memory stays flat over a multi-million-row result. Cell ranges select
//! and copy as TSV; clicking a column header sorts (re-running with `ORDER BY`).
//!
//! Split across three files: [`buffer`] (the windowed paging core), [`render`]
//! (cell rendering + the results-pane view), and this module ([`ResultGrid`]
//! state plus the `AppState` command handlers that drive it).

mod buffer;
mod copy;
mod edit;
mod render;

pub(crate) use edit::GridEdit;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use flint::prelude::*;
use gpui::{
    point, px, ClipboardItem, Context, PathPromptOptions, Pixels, ScrollHandle,
    UniformListScrollHandle,
};
use red_core::{
    Column as ResultColumn, ColumnMap, ColumnStats, ColumnValue, ExportFormat, FkEdge, FkJoin,
    ImportFormat, KeySpec, ResultFilter, TableRef, Value, BASE_ALIAS,
};
use red_service::{Command, CommandSender, RunFetch, SessionId, SortKey};

use crate::app::{
    AppState, EditContext, ExportProgress, ForeignEdit, Notification, PendingImportPeek,
    PendingWrite, Phase, TransferKind,
};

use buffer::{next_epoch, window_decision, BufferMode, GridBuffer, KeyedRun, WindowView, WINDOW};

/// The resolved identity of an editable cell — `(row, data_col, pk_value, decl_type,
/// foreign)` — returned by [`ResultGrid::edit_identity`]. `foreign` is `Some` for an
/// inline-expanded FK column that writes back to its referenced table (Track B7).
type EditIdentity = (usize, usize, Value, Option<String>, Option<ForeignEdit>);
pub(crate) use render::group_digits;

/// Mint a fresh, process-unique epoch for a non-grid consumer (the plan view,
/// Track B4) so its echoed replies are dropped once superseded — shares the
/// grid's monotonic source so the two never collide.
pub(crate) fn new_epoch() -> u64 {
    next_epoch()
}

/// Fixed wide-mode column widths, shared between the renderer (which lays the
/// table out at these widths) and the keyboard cursor's horizontal
/// scroll-into-view (which derives a cell's x-extent arithmetically). Keep in
/// sync with the `Column` widths built in [`render`].
pub(in crate::result) const DATA_COL_WIDTH: f32 = 180.0;
pub(in crate::result) const GUTTER_WIDTH: f32 = 56.0;

/// One inline-expanded reference column (Track B7): a dotted path from the base
/// table down through single-column FKs to a leaf column — e.g.
/// `["tier_id", "cascade_id", "name"]`, shown and aliased as
/// `tier_id.cascade_id.name`. The leading segments are FK columns (each resolved
/// against the connection's FK graph); the last is the selected leaf column.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(in crate::result) struct ExpandedCol {
    /// FK columns then the leaf column; `len >= 2`.
    pub(in crate::result) path: Vec<String>,
}

impl ExpandedCol {
    /// The dotted unique name / output column alias (`tier_id.name`).
    pub(in crate::result) fn dotted(&self) -> String {
        self.path.join(".")
    }
}

/// The cell-menu data for expanding the focused FK cell's reference table inline
/// (Track B7): the referenced table's name (the section header) and each of its
/// columns with whether it's already shown and the dotted path that toggles it.
/// Built only when the focused column is a single-column FK whose target table is
/// already described (the eager prefetch usually has it).
pub(in crate::result) struct ReferenceMenu {
    pub(in crate::result) ref_table: String,
    pub(in crate::result) columns: Vec<ReferenceMenuItem>,
}

/// One row of a [`ReferenceMenu`]: a referenced-table column the user can toggle.
pub(in crate::result) struct ReferenceMenuItem {
    pub(in crate::result) label: String,
    pub(in crate::result) path: Vec<String>,
    pub(in crate::result) shown: bool,
}

/// The bundle a grid hands the app for a re-open driven by an expansion change: the
/// base SQL, the fresh epoch, the table ref, the preserved sort + filter, the
/// resolved joins, and the superseded epoch to close.
type ReopenSpec = (
    String,
    u64,
    Option<(String, String)>,
    Option<SortKey>,
    Option<ResultFilter>,
    Vec<FkJoin>,
    u64,
);

/// Resolve a browse's [`ExpandedCol`] list into the ordered [`FkJoin`] spec the
/// backend folds into the query. Each distinct FK *prefix* of a path becomes one
/// `LEFT JOIN` (deduped, so several leaves under one reference share a join), keyed
/// off the FK graph: the edge for `(current table, fk column)` gives the target
/// table and the `ON` column pairs, and the target becomes the next hop's table —
/// so a chain like `tier → cascade → placement` resolves hop by hop. A path whose
/// FK can't be resolved (graph not loaded, not a single-column FK) is skipped. Join
/// aliases are simple, unique identifiers (`_red_j0`, …); the meaningful dotted name
/// rides as the select output alias.
pub(in crate::result) fn build_joins(
    graph: &[FkEdge],
    base: (&str, &str),
    expansion: &[ExpandedCol],
) -> Vec<FkJoin> {
    // Each resolved FK prefix → (alias, the table that prefix points at).
    let mut resolved: HashMap<Vec<String>, (String, Option<String>, String)> = HashMap::new();
    let mut idx_by_alias: HashMap<String, usize> = HashMap::new();
    let mut joins: Vec<FkJoin> = Vec::new();
    let mut next_alias = 0usize;

    for col in expansion {
        if col.path.len() < 2 {
            continue;
        }
        let fk_path = &col.path[..col.path.len() - 1];
        let leaf = col.path[col.path.len() - 1].clone();
        let mut parent_alias = BASE_ALIAS.to_string();
        let mut cur_schema = Some(base.0.to_string());
        let mut cur_table = base.1.to_string();
        let mut ok = true;
        // Ensure every prefix [..=i] of the FK path is joined, walking from the base.
        for i in 0..fk_path.len() {
            let prefix = fk_path[..=i].to_vec();
            let (alias, ch_schema, ch_table) = if let Some(e) = resolved.get(&prefix) {
                e.clone()
            } else {
                let fkcol = &fk_path[i];
                let Some(edge) = graph.iter().find(|e| {
                    e.columns.len() == 1
                        && e.from_table == cur_table
                        && e.from_schema.as_deref() == cur_schema.as_deref()
                        && e.columns[0].0 == *fkcol
                }) else {
                    ok = false;
                    break;
                };
                let alias = format!("_red_j{next_alias}");
                next_alias += 1;
                // SQLite edges may omit the target schema; stay within the current
                // namespace rather than dropping qualification.
                let to_schema = edge.to_schema.clone().or_else(|| cur_schema.clone());
                idx_by_alias.insert(alias.clone(), joins.len());
                joins.push(FkJoin {
                    alias: alias.clone(),
                    parent_alias: parent_alias.clone(),
                    on: edge.columns.clone(),
                    to_schema: to_schema.clone(),
                    to_table: edge.to_table.clone(),
                    select: Vec::new(),
                });
                let entry = (alias, to_schema, edge.to_table.clone());
                resolved.insert(prefix, entry.clone());
                entry
            };
            parent_alias = alias;
            cur_schema = ch_schema;
            cur_table = ch_table;
        }
        if !ok {
            continue;
        }
        if let Some(&ji) = idx_by_alias.get(&parent_alias) {
            joins[ji].select.push((leaf, col.dotted()));
        }
    }
    joins
}

/// All the state for one open result. When the row-number gutter is shown
/// (`grid.row_numbers`) it occupies table column `0`, so data column `n` sits at
/// table column `n + 1`; with the gutter hidden the data columns start at `0`. The
/// offset is [`AppState::gutter`] and selection/copy/sort map through it.
pub(crate) struct ResultGrid {
    pub label: String,
    base_sql: String,
    pub(in crate::result) columns: Vec<ResultColumn>,
    pub(in crate::result) total: usize,
    pub(in crate::result) ready: bool,
    pub(in crate::result) error: Option<String>,
    /// `(data column, ascending)` — `None` is unsorted.
    pub(in crate::result) sort: Option<(usize, bool)>,
    /// The active result filter (Track B2), pushed into the query on (re)open.
    /// `None` is unfiltered. Survives a re-sort (both ride the same `OpenResult`).
    pub(in crate::result) filter: Option<ResultFilter>,
    pub(in crate::result) selection: Option<CellRange>,
    /// The `(schema, table)` this result browses, when it's a plain table
    /// preview — sent with `OpenResult` so the backend can resolve a seek key.
    /// `None` for editor SQL and for sorted re-opens (which wrap the SQL).
    table: Option<(String, String)>,
    /// Data-column indices that are single-column forward foreign keys of `table`
    /// (Track B7), recomputed from the connection's FK graph on open / graph-load.
    /// Drives the in-grid FK accent; always empty for non-table results.
    fk_cols: HashSet<usize>,
    /// Inline FK expansion (Track B7): the reference columns the user pulled into
    /// this browse, each a dotted path from the base table (`["tier_id","name"]` →
    /// shown as `tier_id.name`). The source of truth the cell menu / Columns panel
    /// toggle; [`joins`](Self::joins) is derived from it against the FK graph. Empty
    /// for an unexpanded browse, editor SQL, or a no-FK engine.
    pub(in crate::result) expansion: Vec<ExpandedCol>,
    /// The resolved `LEFT JOIN` spec sent with `OpenResult`, rebuilt from
    /// [`expansion`](Self::expansion) + the FK graph whenever either changes
    /// ([`rebuild_joins`](Self::rebuild_joins)). Cached here so the re-open paths
    /// (sort / filter / edit refresh) carry it without re-resolving.
    joins: Vec<FkJoin>,
    /// Result-column indices that are inline-expanded (joined) reference columns —
    /// recomputed from [`expansion`](Self::expansion) when the column set lands.
    /// Drives the joined-column tint; editing a single-hop one writes back to the
    /// referenced table (see [`foreign_edit_for`](Self::foreign_edit_for)).
    joined_cols: HashSet<usize>,
    /// Which FK nodes are expanded open in the Columns panel's tree (Track B7),
    /// keyed by their dotted FK path (`["tier_id","cascade_id"]`). Purely panel UI
    /// state — distinct from [`expansion`](Self::expansion) (the *checked* columns);
    /// expanding a node only reveals its children, it doesn't add a column.
    pub(in crate::result) tree_expanded: HashSet<Vec<String>>,
    /// The seek key the backend resolved (`ResultReady`). Track B5 reads its PK to
    /// key a guarded edit; `None` (editor SQL / no usable PK) means not editable.
    key: Option<KeySpec>,
    pub(in crate::result) buffer: Rc<RefCell<GridBuffer>>,
    /// Staged, not-yet-submitted edits for this result (Track B6) — keyed by PK so
    /// they survive the windowed buffer's eviction. Cleared on every (re)open.
    pub(in crate::result) pending: edit::PendingChanges,
    pub(in crate::result) sender: CommandSender,
    pub(in crate::result) scroll: UniformListScrollHandle,
    pub(in crate::result) h_scroll: ScrollHandle,
    /// The overlay scrollbar's in-flight drag.
    pub(in crate::result) scrollbar: ScrollbarState,
    /// Virtual-scroll window: the absolute ordinal that list-local index 0 maps
    /// to. `Rc` so the scrollbar's scrub closure can move it; `Cell` because
    /// `Table`/`uniform_list` are stateless across frames, so the base lives
    /// here. See `WINDOW` and `prepare_window`.
    pub(in crate::result) window_base: Rc<Cell<usize>>,
    /// Rows per fetched page (the `grid.page_size` in effect when this result was
    /// opened) — used to (re)build the buffer in either paging mode. A live result
    /// keeps the page it was opened with; a settings change applies to the next open.
    page_size: usize,
    /// Identifies the current open SQL; bumped on every (re)open so stale page
    /// fetches and late `ResultReady`/`ResultPageLoaded` replies are ignored.
    pub(crate) epoch: u64,
    /// When the current query was issued — drives the live "running…" timer.
    query_started: Instant,
    /// Frozen wall-clock time the query took, set once it lands (ready or error).
    /// `None` while still running, so the elapsed time keeps counting up.
    query_elapsed: Option<Duration>,
    /// The column-stats bar's current view (selected column + load state), or
    /// `None` when no column is selected. Only populated while stats mode is on.
    pub(in crate::result) stats: Option<ColumnStatsView>,
    /// Per-data-column stats cache, cleared on every (re)open so re-selecting a
    /// column is instant (the summary is correct only for this epoch's SQL).
    stats_cache: HashMap<usize, ColumnStats>,
}

/// The column-stats bar's load state for the currently-selected column.
#[derive(Clone)]
pub(in crate::result) enum StatsState {
    Loading,
    Ready(ColumnStats),
    Failed,
}

/// Which column the stats bar is showing, plus its load state. `numeric` is the
/// column's number-ness (decided from its declared type), kept so the "compute
/// distinct" re-request preserves the sum/avg aggregates.
pub(in crate::result) struct ColumnStatsView {
    pub(in crate::result) data_col: usize,
    pub(in crate::result) column: String,
    pub(in crate::result) numeric: bool,
    pub(in crate::result) state: StatsState,
}

impl ResultGrid {
    pub fn new(
        label: String,
        base_sql: String,
        table: Option<(String, String)>,
        sender: CommandSender,
        page_size: usize,
    ) -> Self {
        Self {
            label,
            base_sql,
            columns: Vec::new(),
            total: 0,
            ready: false,
            error: None,
            sort: None,
            filter: None,
            selection: None,
            table,
            fk_cols: HashSet::new(),
            expansion: Vec::new(),
            joins: Vec::new(),
            joined_cols: HashSet::new(),
            tree_expanded: HashSet::new(),
            key: None,
            buffer: Rc::new(RefCell::new(GridBuffer::new(page_size))),
            pending: edit::PendingChanges::default(),
            sender,
            page_size,
            scroll: UniformListScrollHandle::new(),
            h_scroll: ScrollHandle::new(),
            scrollbar: ScrollbarState::new(),
            window_base: Rc::new(Cell::new(0)),
            epoch: next_epoch(),
            query_started: Instant::now(),
            query_elapsed: None,
            stats: None,
            stats_cache: HashMap::new(),
        }
    }

    /// Wall-clock time the query has taken: frozen once it lands, otherwise the
    /// live elapsed time since it was issued (the running counter).
    pub(in crate::result) fn query_time(&self) -> Duration {
        self.query_elapsed
            .unwrap_or_else(|| self.query_started.elapsed())
    }

    /// Restart the timer for a re-run (re-sort) of the same grid.
    fn restart_timer(&mut self) {
        self.query_started = Instant::now();
        self.query_elapsed = None;
    }

    /// Freeze the elapsed time — the query has landed (ready or error).
    fn stop_timer(&mut self) {
        if self.query_elapsed.is_none() {
            self.query_elapsed = Some(self.query_started.elapsed());
        }
    }

    /// Whether the open query has landed (ready or errored) vs. still running —
    /// drives the shell's live query-timer ticker.
    pub(crate) fn is_ready(&self) -> bool {
        self.ready
    }

    /// Drop any cell selection — used when the gutter offset changes under it (the
    /// selection is stored in table-column coordinates, see `AppState::set_row_numbers`).
    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// The error this result failed with, if any — surfaced so the assistant's
    /// "Explain error" action can ground a turn in the last query failure.
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The `(absolute row, data column)` of the cell under the keyboard cursor —
    /// the selection's focus, mapped through the gutter and clamped to the data
    /// columns. `None` when nothing is selected or the result has no columns. The
    /// detail inspector resolves the focused cell through this.
    pub(crate) fn cursor_cell(&self, gutter: usize) -> Option<(usize, usize)> {
        let focus = self.selection?.focus;
        let ncols = self.columns.len();
        (ncols > 0).then(|| (focus.0, focus.1.saturating_sub(gutter).min(ncols - 1)))
    }

    /// The `(schema, table)` this result browses — `Some` only for a single-table
    /// preview (the FK click-through / relation-tree entry conditions, Track B7).
    pub(crate) fn base_table(&self) -> Option<&(String, String)> {
        self.table.as_ref()
    }

    /// The result's column metadata — read by the relation tree to build records.
    pub(crate) fn columns(&self) -> &[ResultColumn] {
        &self.columns
    }

    /// Recompute which data columns are single-column forward foreign keys of this
    /// grid's base table, from the connection's FK graph (Track B7). An empty set
    /// for non-table results or before the graph loads. Drives the in-grid accent.
    pub(crate) fn set_fk_cols(&mut self, graph: &[FkEdge]) {
        self.fk_cols.clear();
        let Some((schema, table)) = &self.table else {
            return;
        };
        for (i, col) in self.columns.iter().enumerate() {
            let is_fk = graph.iter().any(|e| {
                e.columns.len() == 1
                    && e.from_table == *table
                    && e.from_schema.as_deref() == Some(schema.as_str())
                    && e.columns[0].0 == col.name
            });
            if is_fk {
                self.fk_cols.insert(i);
            }
        }
    }

    /// Recompute which result columns are inline-expanded reference columns (Track
    /// B7), matching each column's name against the [`expansion`](Self::expansion)'s
    /// dotted output aliases. Called when a fresh column set lands. Drives the
    /// joined-column tint and the edit exclusion (these aren't base-table columns).
    pub(in crate::result) fn set_joined_cols(&mut self) {
        self.joined_cols.clear();
        if self.expansion.is_empty() {
            return;
        }
        let names: HashSet<String> = self.expansion.iter().map(|e| e.dotted()).collect();
        for (i, col) in self.columns.iter().enumerate() {
            if names.contains(&col.name) {
                self.joined_cols.insert(i);
            }
        }
    }

    /// If result column `i` is an inline-expanded reference column, its expansion
    /// path — for a "hide this column" toggle from the cell menu.
    pub(in crate::result) fn expansion_path_at(&self, i: usize) -> Option<Vec<String>> {
        if !self.joined_cols.contains(&i) {
            return None;
        }
        let name = &self.columns.get(i)?.name;
        self.expansion
            .iter()
            .find(|e| &e.dotted() == name)
            .map(|e| e.path.clone())
    }

    /// The dotted output aliases currently expanded under base FK column `fk_col`
    /// (single-hop), so the cell menu can check the columns already shown.
    pub(in crate::result) fn shown_under(&self, fk_col: &str) -> HashSet<String> {
        self.expansion
            .iter()
            .filter(|e| e.path.first().map(String::as_str) == Some(fk_col))
            .map(|e| e.dotted())
            .collect()
    }

    /// Rebuild the resolved [`joins`](Self::joins) from the current
    /// [`expansion`](Self::expansion) against `graph`. Called after the expansion is
    /// toggled and when the FK graph (re)loads.
    pub(crate) fn rebuild_joins(&mut self, graph: &[FkEdge]) {
        self.joins = match &self.table {
            Some((s, t)) => build_joins(graph, (s.as_str(), t.as_str()), &self.expansion),
            None => Vec::new(),
        };
    }

    /// Toggle one reference column in the expansion (add if absent, remove if
    /// present), returning whether the expansion changed. The caller rebuilds the
    /// joins and re-opens.
    pub(in crate::result) fn toggle_expansion(&mut self, path: Vec<String>) -> bool {
        match self.expansion.iter().position(|e| e.path == path) {
            Some(pos) => {
                self.expansion.remove(pos);
            }
            None => self.expansion.push(ExpandedCol { path }),
        }
        true
    }

    /// The resolved joins to send with an `OpenResult` (re)open.
    pub(in crate::result) fn open_joins(&self) -> Vec<FkJoin> {
        self.joins.clone()
    }

    /// Whether any reference columns are currently expanded into this browse.
    pub(crate) fn has_expansion(&self) -> bool {
        !self.expansion.is_empty()
    }

    /// Whether the dotted path is currently a shown (checked) reference column.
    pub(crate) fn is_shown(&self, path: &[String]) -> bool {
        self.expansion.iter().any(|e| e.path == path)
    }

    /// Whether the Columns-panel tree node at `path` is expanded open.
    pub(crate) fn is_tree_expanded(&self, path: &[String]) -> bool {
        self.tree_expanded.contains(path)
    }

    /// Toggle a Columns-panel tree node open/closed, returning the new state (`true`
    /// = now open). Pure panel UI state; doesn't touch the joins.
    pub(crate) fn toggle_tree_node(&mut self, path: Vec<String>) -> bool {
        if self.tree_expanded.remove(&path) {
            false
        } else {
            self.tree_expanded.insert(path);
            true
        }
    }

    /// Reset the grid's live view for an expansion-driven re-open and return the
    /// [`ReopenSpec`] the app sends — mirroring the re-sort / filter re-open
    /// (selection cleared, buffer + timer reset, a fresh epoch so in-flight pages for
    /// the old SQL are dropped), preserving the current header-click sort + filter.
    pub(in crate::result) fn reopen_spec(&mut self) -> ReopenSpec {
        let old_epoch = self.epoch;
        self.selection = None;
        self.ready = false;
        self.restart_timer();
        self.reset_buffer();
        self.epoch = next_epoch();
        let sort = self.sort.and_then(|(dcol, asc)| {
            self.columns.get(dcol).map(|col| SortKey {
                position: dcol + 1,
                column: col.name.clone(),
                descending: !asc,
            })
        });
        (
            self.base_sql.clone(),
            self.epoch,
            self.table.clone(),
            sort,
            self.filter.clone(),
            self.open_joins(),
            old_epoch,
        )
    }

    /// A data column's `(name, declared type)` — for the inspector header.
    pub(crate) fn column_meta(&self, col: usize) -> Option<(String, Option<String>)> {
        self.columns
            .get(col)
            .map(|c| (c.name.clone(), c.decl_type.clone()))
    }

    /// The resident value at `(row, col)`, cloned. `None` when the row is off the
    /// resident window (evicted) or the column is out of range. A whole resident
    /// cell is bounded by the driver's display cap, so this clone is cheap; a
    /// `Value::Capped` comes back as itself so the caller can tell it's partial.
    pub(crate) fn cell_value(&self, row: usize, col: usize) -> Option<Value> {
        self.buffer
            .borrow()
            .row(row)
            .and_then(|r| r.values.get(col).cloned())
    }

    /// Whether this result is an editable single-table keyed browse (a base table
    /// plus a resolved PK) — the precondition for any staged edit / insert / delete.
    pub(crate) fn editable_browse(&self) -> bool {
        self.table.is_some() && self.key.is_some()
    }

    /// The data-column index of the identity (PK) column — the sorted browse's
    /// tiebreaker, else the lead key — when this is an editable browse and the PK
    /// column is present in the result.
    pub(in crate::result) fn pk_column_index(&self) -> Option<usize> {
        let key = self.key.as_ref()?;
        let pk_column = key.tiebreak.clone().unwrap_or_else(|| key.column.clone());
        self.columns.iter().position(|c| c.name == pk_column)
    }

    /// Assemble the guarded-edit target (Track B5) for the cell under the cursor —
    /// the base table, its PK column + value (the row's identity), and the focused
    /// column's name / declared type / current value. `None` when the result isn't
    /// an editable single-table keyed browse, the cursor is on the PK column itself
    /// (changing identity is out of scope), the PK value is missing or clipped, or
    /// the target cell is binary / display-clipped (no safe inline round-trip).
    /// `gutter` is the data-column table offset (see [`AppState::gutter`]).
    pub(crate) fn edit_target(&self, gutter: usize) -> Option<EditContext> {
        let (row, col, pk_value, decl_type, foreign) = self.edit_identity(gutter)?;
        let original = self.cell_value(row, col)?;
        // A resident inline edit needs a safe round-trip: no binary, and no
        // display-clipped cell (we'd only have its head). The inspector's
        // [`edit_target_full`] lifts the clipped restriction once the full value
        // has been loaded out-of-band.
        if matches!(original, Value::Blob(_) | Value::Capped(_)) {
            return None;
        }
        Some(EditContext {
            epoch: self.epoch,
            row,
            data_col: col,
            pk_value,
            decl_type,
            original,
            foreign,
        })
    }

    /// Like [`edit_target`], but for a value fetched in full out-of-band (the
    /// inspector's "Load full value"): the *resident* cell may be display-capped, so
    /// the caller supplies `original` rather than reading the clipped buffer. This
    /// is what makes a large `TEXT`/JSON cell editable. Binary is still refused —
    /// there is no text round-trip for a blob.
    pub(crate) fn edit_target_full(&self, gutter: usize, original: Value) -> Option<EditContext> {
        if matches!(original, Value::Blob(_)) {
            return None;
        }
        let (row, col, pk_value, decl_type, foreign) = self.edit_identity(gutter)?;
        Some(EditContext {
            epoch: self.epoch,
            row,
            data_col: col,
            pk_value,
            decl_type,
            original,
            foreign,
        })
    }

    /// The row identity + target column for an inline edit, independent of the
    /// cell's current value — shared by [`edit_target`] (resident value) and
    /// [`edit_target_full`] (a supplied loaded value). `foreign` is `Some` when the
    /// cell is a single-hop inline-expanded FK column (the edit rewrites the
    /// referenced table). `None` unless this is an editable single-table keyed browse
    /// with a usable (present, uncapped) PK and the cursor sitting off the PK column.
    pub(crate) fn edit_identity(&self, gutter: usize) -> Option<EditIdentity> {
        self.table.as_ref()?; // must be a single-table browse to be editable
        let key = self.key.as_ref()?;
        // The identity column: the tiebreaker (the PK) for a sorted browse, else the
        // lead key (which is the PK for a plain browse).
        let pk_column = key.tiebreak.clone().unwrap_or_else(|| key.column.clone());
        let pk_idx = self.columns.iter().position(|c| c.name == pk_column)?;
        let (row, col) = self.cursor_cell(gutter)?;
        let target = self.columns.get(col)?;
        if target.name == pk_column {
            return None;
        }
        // An inline-expanded reference column (Track B7) writes back to the *joined*
        // table, not this browse's base table — resolve its foreign target. A
        // multi-hop / composite / orphaned-FK expansion can't be resolved and stays
        // read-only (`foreign_edit_for` returns `None`).
        let foreign = if self.joined_cols.contains(&col) {
            Some(self.foreign_edit_for(col, row)?)
        } else {
            None
        };
        let pk_value = self.cell_value(row, pk_idx)?;
        if matches!(pk_value, Value::Null | Value::Capped(_)) {
            return None;
        }
        Some((row, col, pk_value, target.decl_type.clone(), foreign))
    }

    /// Resolve the referenced-table write target for the inline-expanded FK column at
    /// result column `col` in absolute `row` (Track B7 editable joined columns). The
    /// referenced row is identified by the FK value resident in the base row, so only
    /// a **single-hop** join (parent = the base subquery) with a **single-column** key
    /// is editable; a deeper chain would need an intermediate join's value that may
    /// not be resident, and a composite key isn't expressible as one
    /// [`EditOp::Update`](red_core::EditOp) predicate. `None` (stay read-only) when
    /// the column isn't a resolvable join output, the hop is nested/composite, or the
    /// row's FK is NULL/clipped (an orphaned reference has no row to update).
    fn foreign_edit_for(&self, col: usize, row: usize) -> Option<ForeignEdit> {
        let name = self.columns.get(col)?.name.clone(); // dotted alias, e.g. "tier_id.name"
        let join = self
            .joins
            .iter()
            .find(|j| j.select.iter().any(|(_, out)| out == &name))?;
        // Single-hop only: the referenced row's key value must be a base column.
        if join.parent_alias != red_core::BASE_ALIAS {
            return None;
        }
        // Single-column key only (a composite FK isn't one WHERE predicate).
        let [(parent_col, target_col)] = join.on.as_slice() else {
            return None;
        };
        let parent_idx = self.columns.iter().position(|c| &c.name == parent_col)?;
        let key_value = self.cell_value(row, parent_idx)?;
        if matches!(key_value, Value::Null | Value::Capped(_)) {
            return None;
        }
        let set_column = join
            .select
            .iter()
            .find(|(_, out)| out == &name)
            .map(|(leaf, _)| leaf.clone())?;
        Some(ForeignEdit {
            table: red_core::TableRef {
                schema: join.to_schema.clone(),
                name: join.to_table.clone(),
            },
            key_column: target_col.clone(),
            key_value,
            key_type: self
                .columns
                .get(parent_idx)
                .and_then(|c| c.decl_type.clone()),
            set_column,
        })
    }

    /// Patch the resident cell at `(row, data_col)` to `value` in place, after a
    /// committed edit (Track B5) — avoids a refetch round-trip for the common case.
    pub(crate) fn patch_cell(&mut self, row: usize, data_col: usize, value: Value) {
        self.buffer.borrow_mut().patch_cell(row, data_col, value);
    }

    /// Total rows in the open result (0 until `ResultReady`) — for the
    /// go-to-row prompt's range hint and bound.
    pub(crate) fn total_rows(&self) -> usize {
        self.total
    }

    /// The import target this browse represents — `(table, its columns)` — or `None`
    /// when the result isn't a single-table browse (editor SQL / a join). Drives the
    /// toolbar "Import…" affordance and the name-based column mapping.
    pub(in crate::result) fn import_target(&self) -> Option<(TableRef, Vec<ResultColumn>)> {
        let (schema, name) = self.table.clone()?;
        Some((
            TableRef {
                schema: Some(schema),
                name,
            },
            self.columns.clone(),
        ))
    }

    /// `(rows, columns)` once the result is ready — for the shell's status bar.
    pub fn status_counts(&self) -> Option<(usize, usize)> {
        self.ready
            .then_some((self.total, self.columns.len()))
            .filter(|_| self.error.is_none())
    }

    /// Install the open result's metadata and reset the buffer into the right
    /// mode: keyed when the backend resolved a seek key that names a result
    /// column, offset otherwise.
    fn on_ready(&mut self, columns: Vec<ResultColumn>, total: usize, key: Option<KeySpec>) {
        // Keyed only when every key column (lead, then tiebreaker) is present in
        // the result — a `SELECT *` table browse always satisfies this.
        let key_cols = key.as_ref().and_then(|k| {
            let cols: Vec<usize> = k
                .column_names()
                .iter()
                .filter_map(|name| columns.iter().position(|c| &c.name == name))
                .collect();
            (cols.len() == k.column_names().len()).then_some(cols)
        });
        self.key = key;
        self.columns = columns;
        self.total = total;
        self.ready = true;
        self.error = None;
        // A fresh result set starts with a clean change-set + empty stats cache (the
        // summary is keyed to the prior epoch's SQL).
        self.pending = edit::PendingChanges::default();
        self.stats = None;
        self.stats_cache.clear();
        self.stop_timer();
        self.window_base.set(0);
        let page = self.page_size;
        let mut buffer = self.buffer.borrow_mut();
        *buffer = GridBuffer::new(page);
        if let Some(key_cols) = key_cols {
            buffer.mode = BufferMode::Keyed(KeyedRun::new(key_cols, page));
        }
    }

    /// The current virtual-scroll window. Recenters it on the viewport when the
    /// scroll nears a window edge (compensating the list's pixel offset so the
    /// visible rows hold still), and returns the base + length to feed the list
    /// plus the scrollbar's fraction/thumb. Call once per render, *before*
    /// building the `Table`, so `row_count`, `window_base`, and the list's pixel
    /// offset all agree within the frame.
    pub(in crate::result) fn prepare_window(&self, row_height: Pixels) -> WindowView {
        let total = self.total;
        let rh = f32::from(row_height).max(1.0);

        let (offset_x, offset_y, viewport_h) = {
            let st = self.scroll.0.borrow();
            let off = st.base_handle.offset();
            let vh = st
                .last_item_size
                .map(|s| f32::from(s.item.height))
                .unwrap_or(0.0);
            (off.x, f32::from(off.y), vh)
        };
        let viewport_rows = (viewport_h / rh).ceil() as usize;

        let len = total.min(WINDOW);
        // The viewport's top row, in list-local then absolute coordinates.
        let local_first = (-offset_y / rh).round().max(0.0) as usize;
        let abs_first = self.window_base.get().min(total.saturating_sub(len)) + local_first;

        let (base, reanchor) =
            window_decision(total, self.window_base.get(), local_first, viewport_rows);
        if let Some(new_local_first) = reanchor {
            // The window slid; shift the list's pixel offset by the same amount
            // so the rows on screen don't move — the user only ever sees one
            // continuous scroll.
            let st = self.scroll.0.borrow();
            st.base_handle
                .set_offset(point(offset_x, px(-(new_local_first as f32 * rh))));
        }
        self.window_base.set(base);

        // Scrollbar position is absolute (fraction of the whole result), not of
        // the window — so the thumb reflects where we are in all 50M rows.
        let denom = total.saturating_sub(viewport_rows).max(1) as f32;
        let fraction = (abs_first as f32 / denom).clamp(0.0, 1.0);
        let thumb = if total > 0 {
            (viewport_rows as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            1.0
        };

        WindowView {
            base,
            len,
            fraction,
            thumb,
        }
    }

    fn reset_buffer(&mut self) {
        *self.buffer.borrow_mut() = GridBuffer::new(self.page_size);
        self.window_base.set(0);
        self.pending = edit::PendingChanges::default();
        // A re-open computes a new epoch's SQL — the prior summary no longer applies.
        self.stats = None;
        self.stats_cache.clear();
    }

    /// Resolve the stats request for the currently-selected column (the bar's
    /// auto-on-select trigger). Points the bar at that column: a cache hit fills it
    /// instantly and returns `None`; a miss marks it `Loading` and returns the
    /// `(epoch, column, numeric, distinct)` to request. `None` when no column is
    /// selected, or the same column is already showing (loading or ready).
    /// `distinct_max` guards the `count(distinct)`: it's auto-included only when the
    /// result is at or below the threshold.
    fn prepare_stats(
        &mut self,
        gutter: usize,
        distinct_max: usize,
    ) -> Option<(u64, String, bool, bool)> {
        let dcol = self.cursor_cell(gutter).map(|(_, c)| c)?;
        let col = self.columns.get(dcol)?;
        let column = col.name.clone();
        let numeric = red_core::is_numeric_type(col.decl_type.as_deref());
        // Already showing this column and not in a (retryable) failed state.
        if self
            .stats
            .as_ref()
            .is_some_and(|v| v.data_col == dcol && !matches!(v.state, StatsState::Failed))
        {
            return None;
        }
        // Cache hit → instant, no query.
        if let Some(stats) = self.stats_cache.get(&dcol).cloned() {
            self.stats = Some(ColumnStatsView {
                data_col: dcol,
                column,
                numeric,
                state: StatsState::Ready(stats),
            });
            return None;
        }
        // Guard the (potentially full-scan) count-distinct on a large result.
        let distinct = self.total <= distinct_max;
        self.stats = Some(ColumnStatsView {
            data_col: dcol,
            column: column.clone(),
            numeric,
            state: StatsState::Loading,
        });
        Some((self.epoch, column, numeric, distinct))
    }

    /// Re-request the visible column's summary with `count(distinct)` forced on —
    /// the bar's "[compute]" affordance when the guard withheld it. Returns the
    /// `(epoch, column, numeric)` to send, or `None` when no column is shown.
    fn force_distinct_request(&mut self) -> Option<(u64, String, bool)> {
        let view = self.stats.as_mut()?;
        let (column, numeric) = (view.column.clone(), view.numeric);
        view.state = StatsState::Loading;
        Some((self.epoch, column, numeric))
    }

    /// Apply a `ColumnStatsReady` reply: cache it (by the column's data index) and,
    /// if the bar is still waiting on this column, show it.
    fn apply_stats(&mut self, column: &str, stats: ColumnStats) {
        // Prefer the visible view's known data index (handles duplicate names);
        // otherwise resolve by name. Resolved first to avoid a borrow conflict.
        let dcol = self
            .stats
            .as_ref()
            .filter(|v| v.column == column)
            .map(|v| v.data_col)
            .or_else(|| self.columns.iter().position(|c| c.name == column));
        if let Some(dcol) = dcol {
            self.stats_cache.insert(dcol, stats.clone());
        }
        if let Some(view) = self.stats.as_mut() {
            if view.column == column {
                view.state = StatsState::Ready(stats);
            }
        }
    }

    /// Apply a `ColumnStatsFailed` reply: mark the bar "unavailable" if it's still
    /// waiting on this column (a stale failure for a since-changed selection is
    /// ignored).
    fn fail_stats(&mut self, column: &str) {
        if let Some(view) = self.stats.as_mut() {
            if view.column == column && matches!(view.state, StatsState::Loading) {
                view.state = StatsState::Failed;
            }
        }
    }

    /// Jump the grid to `ordinal` (0-based) — the explicit "go to row N". Places
    /// the virtual-scroll window so the row sits at the viewport top, then, for a
    /// keyed result, forces an **exact** relocation (keyset auto-jumps would only
    /// interpolate). An offset result needs no special fetch: positioning alone
    /// makes the next paint request the exact `OFFSET` page at `ordinal`.
    pub(in crate::result) fn go_to_row(&self, ordinal: usize, row_height: f32) {
        let target = ordinal.min(self.total.saturating_sub(1));
        place_window(
            &self.window_base,
            &self.scroll,
            self.total,
            target,
            row_height,
        );
        if let BufferMode::Keyed(run) = &mut self.buffer.borrow_mut().mode {
            run.jump_exact(target, self.epoch, &self.sender);
        }
    }

    /// Find-in-result (Track B2, Tier 1): resident cells whose display text
    /// contains `term_lower` (already lower-cased), as `(ordinal, data column)`.
    /// Scans only loaded rows — see [`buffer::GridBuffer::find_matches`].
    pub(crate) fn find_matches(&self, term_lower: &str) -> Vec<(usize, usize)> {
        self.buffer.borrow().find_matches(term_lower)
    }

    /// Select `(ordinal, table_col)` and scroll it into view — the find bar's
    /// "reveal this match", so the grid's selection highlight marks the current
    /// match. `table_col` is in table-column space (gutter included).
    pub(crate) fn reveal_cell(&mut self, ordinal: usize, table_col: usize, row_height: f32) {
        self.selection = Some(CellRange::single(ordinal, table_col));
        self.go_to_row(ordinal, row_height);
    }

    /// How many whole rows the on-screen viewport shows — the PageUp/PageDown
    /// step. Reads the list's last measured viewport height (0 before first paint).
    pub(in crate::result) fn viewport_rows(&self, row_height: f32) -> usize {
        let rh = row_height.max(1.0);
        let st = self.scroll.0.borrow();
        let vh = st
            .last_item_size
            .map(|s| f32::from(s.item.height))
            .unwrap_or(0.0);
        (vh / rh).floor() as usize
    }

    /// The absolute ordinal of the first row currently visible at the viewport
    /// top — where a fresh keyboard cursor starts when nothing is selected yet.
    pub(in crate::result) fn first_visible_row(&self, row_height: f32) -> usize {
        let rh = row_height.max(1.0);
        let st = self.scroll.0.borrow();
        let off_y = f32::from(st.base_handle.offset().y);
        let local_first = (-off_y / rh).round().max(0.0) as usize;
        (self.window_base.get() + local_first).min(self.total.saturating_sub(1))
    }

    /// Keep the keyboard cursor on screen after it moves to absolute ordinal
    /// `abs_row`. Two regimes: if the row left the resident buffer window, reuse
    /// the proven `go_to_row` jump (recenter + keyed exact relocation); if it's
    /// still in the window but scrolled out of the viewport, nudge the list's
    /// pixel offset by the minimum so the row sits at the near edge — no full
    /// recenter, so a one-row step never restyles the whole window.
    pub(in crate::result) fn scroll_cursor_into_view(&self, abs_row: usize, row_height: f32) {
        if self.total == 0 {
            return;
        }
        let rh = row_height.max(1.0);
        let len = self.total.min(WINDOW);
        let base = self.window_base.get();
        if abs_row < base || abs_row >= base + len {
            // Off the resident window — recenter (and, when keyed, fetch exactly).
            self.go_to_row(abs_row, rh);
            return;
        }
        let st = self.scroll.0.borrow();
        let off = st.base_handle.offset();
        let vh = st
            .last_item_size
            .map(|s| f32::from(s.item.height))
            .unwrap_or(0.0);
        let viewport_rows = (vh / rh).floor().max(1.0) as usize;
        let local = abs_row - base;
        let local_first = (-f32::from(off.y) / rh).round().max(0.0) as usize;
        let new_first = if local < local_first {
            local
        } else if local >= local_first + viewport_rows {
            local + 1 - viewport_rows
        } else {
            return; // already visible — leave the scroll untouched
        };
        st.base_handle
            .set_offset(point(off.x, px(-(new_first as f32 * rh))));
    }

    /// Keep the keyboard cursor's *column* on screen after a horizontal move —
    /// the wide-mode counterpart to [`scroll_cursor_into_view`]. Columns are
    /// fixed-width (a [`GUTTER_WIDTH`] row-number column when shown, then
    /// [`DATA_COL_WIDTH`] per data column), so the focused cell's x-extent is
    /// pure arithmetic; nudge the horizontal handle by the minimum to bring the
    /// cell fully into the viewport, leaving it untouched when already visible.
    /// `table_col` is in table space (gutter included); `gutter` is its width in
    /// columns (0 or 1).
    pub(in crate::result) fn scroll_col_into_view(&self, table_col: usize, gutter: usize) {
        let viewport_w = f32::from(self.h_scroll.bounds().size.width);
        if viewport_w <= 0.0 {
            return; // not laid out yet
        }
        let gutter_w = gutter as f32 * GUTTER_WIDTH;
        let data_col = table_col.saturating_sub(gutter);
        let col_left = gutter_w + data_col as f32 * DATA_COL_WIDTH;
        let col_right = col_left + DATA_COL_WIDTH;
        // The handle's x offset is 0 at the left edge and grows negative as the
        // content scrolls left, so the visible window is `[-off.x, -off.x + w]`.
        let off = self.h_scroll.offset();
        let scroll_left = -f32::from(off.x);
        let new_left = if col_left < scroll_left {
            col_left
        } else if col_right > scroll_left + viewport_w {
            col_right - viewport_w
        } else {
            return; // already fully visible — leave the scroll untouched
        };
        let content_w = gutter_w + self.columns.len() as f32 * DATA_COL_WIDTH;
        let max_left = (content_w - viewport_w).max(0.0);
        self.h_scroll
            .set_offset(point(px(-new_left.clamp(0.0, max_left)), off.y));
    }

    /// A glance at this grid's footprint for the dev perf HUD: resident rows,
    /// paging mode, in-flight fetches, and the last query's wall-clock time.
    #[cfg(feature = "dev-stats")]
    pub(crate) fn dev_snapshot(&self) -> crate::dev_stats::GridSnapshot {
        let buffer = self.buffer.borrow();
        crate::dev_stats::GridSnapshot {
            resident_rows: buffer.resident_rows(),
            mode: buffer.mode_label(),
            in_flight: buffer.in_flight(),
            last_query_ms: self.query_time().as_secs_f32() * 1000.0,
        }
    }

    /// How to satisfy a copy of the current selection (NULL → empty, gutter
    /// column skipped). When every selected cell is resident and untruncated the
    /// clipboard text is built straight from the buffer ([`CopyPlan::Ready`]);
    /// when the selection touches a cell the grid clipped for display, or reaches
    /// rows scrolled out of the window (a whole-column select), those rows must be
    /// re-fetched in full first ([`CopyPlan::Refetch`]). `None` when the selection
    /// is empty or covers only the gutter. `gutter` is the data-column table offset
    /// (`1` with the row-number gutter shown, else `0`).
    fn copy_plan(&self, gutter: usize) -> Option<CopyPlan> {
        let (r0, c0, r1, c1) = self.selection?.bounds();
        let ncol = self.columns.len();
        let dc0 = c0.max(gutter);
        if dc0 > c1 {
            return None;
        }
        let dcol_lo = dc0 - gutter;
        let dcol_hi = (c1 - gutter).min(ncol.saturating_sub(1));
        let buffer = self.buffer.borrow();
        // Any selected row that's off-window (not resident) or holds a clipped
        // display stand-in forces a full re-fetch; otherwise the buffer already
        // has the real values. `any` short-circuits at the first such row, so a
        // whole-column select doesn't scan the entire result here.
        let needs_full = (r0..=r1).any(|r| match buffer.row(r) {
            None => true,
            Some(row) => (dcol_lo..=dcol_hi).any(|c| row.is_truncated(c)),
        });
        if needs_full {
            return Some(CopyPlan::Refetch {
                epoch: self.epoch,
                offset: r0,
                limit: r1 - r0 + 1,
                dcol_lo,
                dcol_hi,
            });
        }
        let mut out = String::new();
        for r in r0..=r1 {
            for (i, dcol) in (dcol_lo..=dcol_hi).enumerate() {
                if i > 0 {
                    out.push('\t');
                }
                if let Some(value) = buffer.row(r).and_then(|row| row.values.get(dcol)) {
                    out.push_str(&cell_string(value));
                }
            }
            out.push('\n');
        }
        Some(CopyPlan::Ready(out))
    }
}

/// A copy awaiting its full-row re-fetch (see [`CopyPlan::Refetch`]). Holds the
/// selected data-column span so the [`Event::CopyRowsLoaded`] reply can be turned
/// into the clipboard text.
///
/// [`Event::CopyRowsLoaded`]: red_service::Event::CopyRowsLoaded
pub(crate) struct PendingCopy {
    pub(crate) id: u64,
    pub(crate) dcol_lo: usize,
    pub(crate) dcol_hi: usize,
}

/// An in-flight FK click-through (Track B7) awaiting its single-row `CopyRows`
/// re-fetch: once the row's typed value(s) arrive, the target browse is opened
/// filtered to them. See [`AppState::on_fk_rows`].
pub(crate) struct PendingFkFollow {
    id: u64,
    plan: FkPlan,
}

/// One reverse FK edge surfaced in the cell menu: a table that references the
/// current grid's table. "Show rows in `table` (`from_column`)" opens `table`
/// filtered to `from_column = <this row's to_column value>`.
pub(crate) struct FkReverse {
    pub(crate) schema: String,
    pub(crate) table: String,
    pub(crate) from_column: String,
    pub(crate) to_column: String,
}

/// The resolved target of an FK follow: which table to open, and for each of its
/// filter columns, the source-row column index to read the key value from. One
/// pair for a single-column FK; several for a composite.
struct FkPlan {
    /// The source result's epoch (the `CopyRows` is issued against it).
    epoch: u64,
    /// The source row whose key value(s) we fetch in full.
    row: usize,
    /// `(schema, table)` of the browse to open.
    target: (String, String),
    /// `(target_filter_column, source_column_index)` pairs.
    filters: Vec<(String, usize)>,
}

/// How [`ResultGrid::copy_plan`] resolves a selection copy.
pub(crate) enum CopyPlan {
    /// Ready to copy now — the assembled TSV.
    Ready(String),
    /// The selection holds display-clipped cells; re-fetch the rows in full
    /// (`CopyRows`) and assemble the clipboard text from the reply. `dcol_lo..=dcol_hi`
    /// are the selected data columns (the re-fetched rows carry every column).
    Refetch {
        epoch: u64,
        offset: usize,
        limit: usize,
        dcol_lo: usize,
        dcol_hi: usize,
    },
}

/// Assemble TSV from freshly re-fetched rows over data columns `dcol_lo..=dcol_hi`
/// (NULL → empty) — the [`CopyPlan::Refetch`] counterpart to the buffer path.
pub(crate) fn rows_tsv(rows: &[Vec<Value>], dcol_lo: usize, dcol_hi: usize) -> String {
    let mut out = String::new();
    for row in rows {
        for (i, dcol) in (dcol_lo..=dcol_hi).enumerate() {
            if i > 0 {
                out.push('\t');
            }
            if let Some(value) = row.get(dcol) {
                out.push_str(&cell_string(value));
            }
        }
        out.push('\n');
    }
    out
}

/// Position the virtual-scroll window so absolute ordinal `target` sits at the
/// viewport top: re-center the window on it and set the list's pixel offset
/// directly (no `scroll_to_item`, which degenerates on a multi-million-row f32
/// canvas). Shared by the scrollbar scrub and the explicit "go to row" jump.
pub(in crate::result) fn place_window(
    window_base: &Cell<usize>,
    scroll: &UniformListScrollHandle,
    total: usize,
    target: usize,
    row_height: f32,
) {
    let base = if total > WINDOW {
        target.saturating_sub(WINDOW / 2).min(total - WINDOW)
    } else {
        0
    };
    window_base.set(base);
    let local = target - base;
    let st = scroll.0.borrow();
    let x = st.base_handle.offset().x;
    st.base_handle
        .set_offset(point(x, px(-(local as f32 * row_height))));
}

/// A value as a plain TSV/clipboard string (NULL → empty).
fn cell_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
        // A capped blob copies as its summary; a capped text would re-fetch full
        // before reaching here (`copy_plan`), so its head is only a defensive form.
        Value::Capped(c) if c.blob => format!("<{} bytes>", c.len),
        Value::Capped(c) => format!("{}…", c.head),
    }
}

impl AppState {
    /// Open `base_sql` as the grid's result (preview or editor run). Resets sort +
    /// selection and asks the backend for the row count + columns. `table` names
    /// the browsed `(schema, table)` for a plain preview, so the backend can
    /// resolve a keyset seek key; editor runs pass `None`.
    pub(crate) fn open_result(
        &mut self,
        label: impl Into<String>,
        base_sql: String,
        table: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        self.open_result_filtered(label, base_sql, table, None, cx);
    }

    /// Like [`open_result`](Self::open_result) but seeds an initial result filter —
    /// the FK click-through (Track B7) opens the target browse pre-filtered to the
    /// followed key. The filter rides the grid (so a re-sort preserves it) and the
    /// first `OpenResult`, exactly like an applied Track B2 filter.
    pub(crate) fn open_result_filtered(
        &mut self,
        label: impl Into<String>,
        base_sql: String,
        table: Option<(String, String)>,
        filter: Option<ResultFilter>,
        cx: &mut Context<Self>,
    ) {
        let opened = match &mut self.phase {
            Phase::Connected(active) if active.active().is_some() => {
                // Bind the grid's load-on-scroll sender to this workspace's session.
                let sender = self.service.command_sender(active.session);
                let mut grid = ResultGrid::new(
                    label.into(),
                    base_sql,
                    table,
                    sender,
                    self.settings.grid.page_size,
                );
                grid.filter = filter;
                let opened = (
                    grid.base_sql.clone(),
                    grid.epoch,
                    grid.table.clone(),
                    grid.filter.clone(),
                );
                // Safe: the guard above ensured a focused tab exists. A fresh run
                // replaces any open plan (Track B4) with its grid.
                let tab = active.active_mut().unwrap();
                tab.result = Some(grid);
                tab.plan = None;
                opened
            }
            _ => return,
        };
        let (sql, epoch, table, filter) = opened;
        // Ensure the base table's column detail is loaded so the reference-column tree
        // (Track B7) can render — a schema-tree browse prefetches this, but a hand-typed
        // `SELECT * FROM t` resolved to a base table may not have it yet. Idempotent: it
        // only fires when the detail is missing (the accent path needs only the FK graph,
        // not this — but the columns panel reads the base table's columns from here).
        if let Some((schema, tbl)) = &table {
            let missing = matches!(&self.phase, Phase::Connected(active)
                if !active.schema.details.contains_key(&(schema.clone(), tbl.clone())));
            if missing {
                self.send_active(Command::DescribeTable {
                    schema: schema.clone(),
                    table: tbl.clone(),
                });
            }
        }
        // A fresh open is never sorted — the backend keys it from `table`; the filter
        // (FK follow) is pushed into the query like a Track B2 filter.
        self.send_active(Command::OpenResult {
            sql,
            epoch,
            table,
            sort: None,
            filter,
            joins: Vec::new(),
        });
        self.start_query_ticker(cx);
        cx.notify();
    }

    /// Backend reported the open result's columns + total (+ resolved seek key).
    pub(crate) fn on_result_ready(
        &mut self,
        session: Option<SessionId>,
        columns: Vec<ResultColumn>,
        total: usize,
        epoch: u64,
        key: Option<KeySpec>,
        cx: &mut Context<Self>,
    ) {
        // Route to the event's session (it may be a backgrounded workspace), then
        // by epoch within it. A late reply for a closed result finds no match.
        if let Some(active) = self.conn_mut(session) {
            // Clone the small FK graph so the grid's mutable borrow doesn't collide
            // with the shared one; mark FK columns now that the column set is known.
            let graph = active.fk_graph.clone();
            if let Some(grid) = active.result_by_epoch(epoch) {
                grid.on_ready(columns, total, key);
                grid.set_fk_cols(&graph);
                grid.set_joined_cols();
            }
        }
        cx.notify();
    }

    /// A keyset run fetch failed — free the grid's in-flight slot so scrolling
    /// can fetch again (the error itself arrives separately as a toast).
    pub(crate) fn on_result_run_failed(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        seq: u64,
    ) {
        if let Some(active) = self.conn_mut(session) {
            if let Some(grid) = active.result_by_epoch(epoch) {
                grid.buffer.borrow_mut().run_failed(seq);
            }
        }
    }

    /// A keyset run window arrived — extend/relocate the grid's run and repaint.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn on_result_run(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        fetch: RunFetch,
        rows: Vec<Vec<Value>>,
        estimated: bool,
        seq: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.conn_mut(session) {
            if let Some(grid) = active.result_by_epoch(epoch) {
                let total = grid.total;
                grid.buffer
                    .borrow_mut()
                    .apply_run(fetch, rows, estimated, seq, total);
            }
        }
        cx.notify();
    }

    /// A page arrived — drop it into the buffer and repaint.
    pub(crate) fn on_result_page(
        &mut self,
        session: Option<SessionId>,
        offset: usize,
        rows: Vec<Vec<Value>>,
        epoch: u64,
        cx: &mut Context<Self>,
    ) {
        // Route by session then epoch so a background tab's page lands in its own
        // grid; a page for a superseded result finds no match and is dropped.
        if let Some(active) = self.conn_mut(session) {
            if let Some(grid) = active.result_by_epoch(epoch) {
                grid.buffer.borrow_mut().insert_page(offset, rows);
            }
        }
        cx.notify();
    }

    /// Record a result error against the session's focused tab grid (also surfaced
    /// as a toast). Errors aren't epoch-tagged, so they attach to the focused tab.
    pub(crate) fn on_result_error(&mut self, session: Option<SessionId>, message: &str) {
        if let Some(active) = self.conn_mut(session) {
            if let Some(grid) = active.active_result_mut() {
                grid.error = Some(message.to_string());
                grid.ready = true;
                grid.stop_timer();
            }
        }
    }

    /// Table-column index of the first *data* column: `1` when the row-number
    /// gutter occupies column 0, else `0`. A data column `d` sits at table column
    /// `d + gutter`; selection/copy/sort all map through this offset.
    pub(crate) fn gutter(&self) -> usize {
        self.settings.grid.row_numbers as usize
    }

    /// Header click on a data column: toggle / set sort and re-open the result.
    pub(crate) fn result_sort(&mut self, table_col: usize, cx: &mut Context<Self>) {
        let gutter = self.gutter();
        if table_col < gutter {
            return; // the row-number gutter isn't sortable
        }
        let dcol = table_col - gutter;
        let reopen = match &mut self.phase {
            Phase::Connected(active) => match active.active_result_mut() {
                Some(grid) => {
                    // A header click can arrive a frame after a re-open delivered a
                    // narrower column set; ignore a click whose data column no longer
                    // exists rather than indexing past `columns` (and before mutating
                    // any sort state, so a stale click is a clean no-op).
                    let Some(col_name) = grid.columns.get(dcol).map(|c| c.name.clone()) else {
                        return;
                    };
                    let old_epoch = grid.epoch;
                    let asc = match grid.sort {
                        Some((c, asc)) if c == dcol => !asc,
                        _ => true,
                    };
                    grid.sort = Some((dcol, asc));
                    grid.selection = None;
                    grid.ready = false;
                    grid.restart_timer();
                    grid.reset_buffer();
                    // New SQL → new epoch, so pages still in flight for the old
                    // ordering are dropped rather than landing in the wrong rows.
                    grid.epoch = next_epoch();
                    // Carry the table ref + sort down so the backend resolves the
                    // composite `(sort_col, pk)` keyset key (or wraps for OFFSET).
                    let sort = SortKey {
                        position: dcol + 1,
                        column: col_name,
                        descending: !asc,
                    };
                    Some((
                        grid.base_sql.clone(),
                        grid.table.clone(),
                        sort,
                        grid.filter.clone(),
                        grid.open_joins(),
                        grid.epoch,
                        old_epoch,
                    ))
                }
                None => None,
            },
            _ => None,
        };
        if let Some((sql, table, sort, filter, joins, epoch, old_epoch)) = reopen {
            // Evict the superseded SQL so the backend's result map can't grow.
            self.send_active(Command::CloseResult { epoch: old_epoch });
            self.send_active(Command::OpenResult {
                sql,
                epoch,
                table,
                sort: Some(sort),
                filter,
                joins,
            });
            self.start_query_ticker(cx);
        }
        cx.notify();
    }

    /// Apply (or clear) the result filter (Track B2): re-open the active grid with
    /// `filter` pushed into the query, preserving the current header-click sort. A
    /// new epoch drops pages still in flight for the prior (un)filtered ordering.
    /// `None` clears the filter. A no-op when the filter is unchanged.
    pub(crate) fn apply_result_filter(
        &mut self,
        filter: Option<ResultFilter>,
        cx: &mut Context<Self>,
    ) {
        let reopen = match &mut self.phase {
            Phase::Connected(active) => match active.active_result_mut() {
                Some(grid) if grid.filter != filter => {
                    let old_epoch = grid.epoch;
                    grid.filter = filter;
                    grid.selection = None;
                    grid.ready = false;
                    grid.restart_timer();
                    grid.reset_buffer();
                    grid.epoch = next_epoch();
                    // Preserve the header-click sort across the re-open. If the
                    // stored sort column no longer exists (the set shrank), drop the
                    // sort rather than indexing past `columns`.
                    let sort = grid.sort.and_then(|(dcol, asc)| {
                        grid.columns.get(dcol).map(|col| SortKey {
                            position: dcol + 1,
                            column: col.name.clone(),
                            descending: !asc,
                        })
                    });
                    Some((
                        grid.base_sql.clone(),
                        grid.table.clone(),
                        sort,
                        grid.filter.clone(),
                        grid.open_joins(),
                        grid.epoch,
                        old_epoch,
                    ))
                }
                _ => None,
            },
            _ => None,
        };
        if let Some((sql, table, sort, filter, joins, epoch, old_epoch)) = reopen {
            self.send_active(Command::CloseResult { epoch: old_epoch });
            self.send_active(Command::OpenResult {
                sql,
                epoch,
                table,
                sort,
                filter,
                joins,
            });
            self.start_query_ticker(cx);
        }
        cx.notify();
    }

    /// Toggle one inline-expanded reference column (Track B7) into / out of the
    /// active browse, then re-open it (new epoch) so the backend re-runs with the
    /// updated `LEFT JOIN` set, preserving the current sort + filter. `path` is the
    /// dotted FK path (`["tier_id","name"]`). No-op unless the active result is a
    /// single-table browse.
    pub(crate) fn toggle_reference_column(&mut self, path: Vec<String>, cx: &mut Context<Self>) {
        let reopen = match &mut self.phase {
            Phase::Connected(active) => {
                let graph = active.fk_graph.clone();
                match active.active_result_mut() {
                    Some(grid) if grid.table.is_some() => {
                        grid.toggle_expansion(path);
                        grid.rebuild_joins(&graph);
                        Some(grid.reopen_spec())
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        self.apply_reopen(reopen, cx);
    }

    /// Drop every inline-expanded reference column from the active browse and re-open
    /// it unexpanded. No-op when nothing is expanded.
    pub(crate) fn clear_reference_columns(&mut self, cx: &mut Context<Self>) {
        let reopen = match &mut self.phase {
            Phase::Connected(active) => match active.active_result_mut() {
                Some(grid) if grid.has_expansion() => {
                    grid.expansion.clear();
                    grid.joins.clear();
                    Some(grid.reopen_spec())
                }
                _ => None,
            },
            _ => None,
        };
        self.apply_reopen(reopen, cx);
    }

    /// Build the inline-FK-expansion menu for the focused cell (Track B7): when the
    /// focused column is a single-column forward FK and its target table has been
    /// described, list the target's columns with their current shown state. `None`
    /// for a non-FK cell, an undescribed target, editor SQL, or before the graph
    /// loads — the cell menu then omits the section.
    pub(in crate::result) fn reference_menu(&self) -> Option<ReferenceMenu> {
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let grid = active.active_result()?;
        let (schema, table) = grid.table.as_ref()?;
        let (_, col) = grid.cursor_cell(self.gutter())?;
        let cname = grid.columns.get(col)?.name.clone();
        let edge = active.fk_graph.iter().find(|e| {
            e.columns.len() == 1
                && e.from_table == *table
                && e.from_schema.as_deref() == Some(schema.as_str())
                && e.columns[0].0 == cname
        })?;
        let ref_schema = edge.to_schema.clone().unwrap_or_else(|| schema.clone());
        let ref_table = edge.to_table.clone();
        let detail = active
            .schema
            .details
            .get(&(ref_schema, ref_table.clone()))?;
        let shown = grid.shown_under(&cname);
        let columns = detail
            .columns
            .iter()
            .map(|c| {
                let path = vec![cname.clone(), c.name.clone()];
                ReferenceMenuItem {
                    shown: shown.contains(&path.join(".")),
                    label: c.name.clone(),
                    path,
                }
            })
            .collect();
        Some(ReferenceMenu { ref_table, columns })
    }

    /// The expansion path of the focused cell when it sits in an inline-expanded
    /// reference column — for the cell menu's "hide this column" action.
    pub(in crate::result) fn focused_joined_path(&self) -> Option<Vec<String>> {
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let grid = active.active_result()?;
        let (_, col) = grid.cursor_cell(self.gutter())?;
        grid.expansion_path_at(col)
    }

    /// Whether the active result currently has any inline-expanded reference columns
    /// (drives the cell menu's "Hide all reference columns" item).
    pub(in crate::result) fn active_has_expansion(&self) -> bool {
        matches!(&self.phase, Phase::Connected(a) if a.active_result().is_some_and(|g| g.has_expansion()))
    }

    /// Close the superseded epoch and re-open the active grid from a
    /// [`ResultGrid::reopen_spec`] bundle (shared by the expansion toggles).
    fn apply_reopen(&mut self, reopen: Option<ReopenSpec>, cx: &mut Context<Self>) {
        if let Some((sql, epoch, table, sort, filter, joins, old_epoch)) = reopen {
            self.send_active(Command::CloseResult { epoch: old_epoch });
            self.send_active(Command::OpenResult {
                sql,
                epoch,
                table,
                sort,
                filter,
                joins,
            });
            self.start_query_ticker(cx);
        }
        cx.notify();
    }

    /// The active result's current filter, for the toolbar chip / filter-bar seed.
    pub(crate) fn active_result_filter(&self) -> Option<ResultFilter> {
        match &self.phase {
            Phase::Connected(active) => active.active_result().and_then(|g| g.filter.clone()),
            _ => None,
        }
    }

    /// The foreground connection's open result carrying `epoch`, for epoch-scoped
    /// replies on the visible workspace (e.g. a committed in-place cell edit,
    /// Track B5). Delegates to [`ActiveConn::result_by_epoch`].
    pub(crate) fn result_by_epoch(&mut self, epoch: u64) -> Option<&mut ResultGrid> {
        match &mut self.phase {
            Phase::Connected(active) => active.result_by_epoch(epoch),
            _ => None,
        }
    }

    /// Cell click: set the selection anchor, or extend it on shift-click. A click
    /// in the row-number gutter (table column `0`) selects the whole row (every
    /// data column); shift-click there extends the block across rows.
    pub(crate) fn result_select(
        &mut self,
        row: usize,
        table_col: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let gutter = self.gutter();
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                let ncols = grid.columns.len();
                grid.selection = if gutter == 1 && table_col == 0 {
                    // Gutter click: span every data column (table cols
                    // `gutter..=ncols`); an empty result has no columns to select.
                    (ncols > 0).then(|| match (extend, grid.selection) {
                        (true, Some(mut range)) => {
                            range.focus = (row, ncols);
                            range
                        }
                        _ => CellRange {
                            anchor: (row, 1),
                            focus: (row, ncols),
                        },
                    })
                } else {
                    Some(match (extend, grid.selection) {
                        (true, Some(mut range)) => {
                            range.focus = (row, table_col);
                            range
                        }
                        _ => CellRange::single(row, table_col),
                    })
                };
            }
        }
        // A new cell selection — refresh the stats bar to its column.
        self.refresh_column_stats(cx);
        cx.notify();
    }

    /// Move the keyboard cell cursor over the active grid (arrows, Home/End,
    /// PageUp/Down, ⌘arrows). `extend` (Shift held) grows the selection from its
    /// anchor; otherwise the cursor becomes a fresh single-cell selection. The
    /// cursor lives in absolute ordinals while the list is windowed, so it then
    /// re-centers the window to follow (see [`ResultGrid::scroll_cursor_into_view`]).
    /// No-op until the result is ready and has columns.
    pub(crate) fn result_cursor_move(
        &mut self,
        mv: TableNav,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let row_height = f32::from(self.settings.grid.density.row_height());
        let gutter = self.gutter();
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                if !grid.ready || grid.error.is_some() || grid.columns.is_empty() {
                    return;
                }
                let ncols = grid.columns.len();
                let last_row = grid.total.saturating_sub(1);
                let page = grid.viewport_rows(row_height).max(1);
                // Data columns occupy table indices `gutter..=ncols-1+gutter`.
                let (first_col, last_col) = (gutter, ncols + gutter - 1);
                // The cursor is the selection's focus; with nothing selected yet
                // it starts at the first visible row's first data column.
                let (row, col) = match grid.selection {
                    Some(r) => r.focus,
                    None => (grid.first_visible_row(row_height), first_col),
                };
                let col = col.clamp(first_col, last_col);
                let (new_row, new_col) = match mv {
                    TableNav::Up => (row.saturating_sub(1), col),
                    TableNav::Down => ((row + 1).min(last_row), col),
                    TableNav::Left => (row, (col - 1).max(first_col)),
                    TableNav::Right => (row, (col + 1).min(last_col)),
                    TableNav::RowStart => (row, first_col),
                    TableNav::RowEnd => (row, last_col),
                    TableNav::PageUp => (row.saturating_sub(page), col),
                    TableNav::PageDown => ((row + page).min(last_row), col),
                    TableNav::First => (0, col),
                    TableNav::Last => (last_row, col),
                };
                grid.selection = Some(match (extend, grid.selection) {
                    (true, Some(mut range)) => {
                        range.focus = (new_row, new_col);
                        range
                    }
                    _ => CellRange::single(new_row, new_col),
                });
                grid.scroll_cursor_into_view(new_row, row_height);
                grid.scroll_col_into_view(new_col, gutter);
            }
        }
        // The keyboard cursor moved — update the stats bar to the focused column.
        self.refresh_column_stats(cx);
        cx.notify();
    }

    /// ⌘/Ctrl-click on a header: select that whole data column (every row). With
    /// `extend` (⌘/Ctrl+Shift-click), grow the existing selection to span every
    /// column between its anchor and this one — full-height, so it reads as a
    /// multi-column block. The selection spans the full result, so copying it
    /// re-fetches the off-window rows in full (see [`ResultGrid::copy_plan`]). The
    /// gutter isn't selectable.
    pub(crate) fn result_select_column(
        &mut self,
        table_col: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let gutter = self.gutter();
        if table_col < gutter {
            return;
        }
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                let last = grid.total.saturating_sub(1);
                grid.selection = match (extend, grid.selection) {
                    // Keep the anchor column, pull the focus to this one, and force
                    // full height so the block stays a clean column span.
                    (true, Some(mut range)) => {
                        range.anchor = (0, range.anchor.1.max(gutter));
                        range.focus = (last, table_col);
                        Some(range)
                    }
                    _ => Some(CellRange {
                        anchor: (0, table_col),
                        focus: (last, table_col),
                    }),
                };
            }
        }
        // Header ⌘/Ctrl-click selected this whole column — its natural stats target.
        self.refresh_column_stats(cx);
        cx.notify();
    }

    /// ⌘A: select the whole result — every row and every data column. Like
    /// [`Self::result_select_column`] the selection spans the full result, so
    /// copying it re-fetches the off-window rows in full (see
    /// [`ResultGrid::copy_plan`]). Anchored top-left, focused bottom-right, both in
    /// table-column space (the gutter sits at column 0). No-op until the result is
    /// ready and has columns.
    pub(crate) fn result_select_all(&mut self, cx: &mut Context<Self>) {
        let gutter = self.gutter();
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                if !grid.ready || grid.error.is_some() || grid.columns.is_empty() {
                    return;
                }
                let last = grid.total.saturating_sub(1);
                let last_col = grid.columns.len() + gutter - 1;
                grid.selection = Some(CellRange {
                    anchor: (0, gutter),
                    focus: (last, last_col),
                });
            }
        }
        cx.notify();
    }

    /// Toggle the column-stats bar (the toolbar "Stats" button). Turning it on
    /// requests the summary for the currently-selected column; turning it off hides
    /// the bar (the cached summaries stay, so re-toggling is instant).
    pub(crate) fn toggle_stats_bar(&mut self, cx: &mut Context<Self>) {
        self.stats_bar = !self.stats_bar;
        if self.stats_bar {
            self.refresh_column_stats(cx);
        } else if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                grid.stats = None;
            }
        }
        cx.notify();
    }

    /// Point the stats bar at the currently-selected column and request its summary
    /// (a cache hit is instant). Called when the bar is on and the selection moves;
    /// a no-op when the bar is off or nothing has changed.
    pub(crate) fn refresh_column_stats(&mut self, cx: &mut Context<Self>) {
        if !self.stats_bar {
            return;
        }
        let gutter = self.gutter();
        let distinct_max = self.settings.grid.stats_distinct_max_rows;
        let req = match &mut self.phase {
            Phase::Connected(active) => match active.active_result_mut() {
                Some(grid) if grid.ready && grid.error.is_none() => {
                    grid.prepare_stats(gutter, distinct_max)
                }
                _ => None,
            },
            _ => None,
        };
        if let Some((epoch, column, numeric, distinct)) = req {
            self.send_active(Command::ColumnStats {
                epoch,
                column,
                numeric,
                distinct,
            });
        }
        cx.notify();
    }

    /// The stats bar's "[compute]" button: re-request the shown column's summary
    /// with `count(distinct)` forced on (the guard had withheld it on a large
    /// result).
    pub(crate) fn compute_column_distinct(&mut self, cx: &mut Context<Self>) {
        let req = match &mut self.phase {
            Phase::Connected(active) => active
                .active_result_mut()
                .and_then(|grid| grid.force_distinct_request()),
            _ => None,
        };
        if let Some((epoch, column, numeric)) = req {
            self.send_active(Command::ColumnStats {
                epoch,
                column,
                numeric,
                distinct: true,
            });
        }
        cx.notify();
    }

    /// A `ColumnStatsReady` reply landed — cache it and update the bar.
    pub(crate) fn on_column_stats(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        column: String,
        stats: ColumnStats,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.conn_mut(session) {
            if let Some(grid) = active.result_by_epoch(epoch) {
                grid.apply_stats(&column, stats);
            }
        }
        cx.notify();
    }

    /// A `ColumnStatsFailed` reply landed — mark the bar "unavailable" (scoped, no
    /// global toast).
    pub(crate) fn on_column_stats_failed(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        column: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.conn_mut(session) {
            if let Some(grid) = active.result_by_epoch(epoch) {
                grid.fail_stats(&column);
            }
        }
        cx.notify();
    }

    /// Prompt for a save path, then stream the active tab's result there in `format`.
    pub(crate) fn export_result(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
        let epoch = match &self.phase {
            Phase::Connected(a) => a.active_result().map(|g| g.epoch),
            _ => None,
        };
        let Some(epoch) = epoch else {
            return;
        };
        let name = match format {
            ExportFormat::Csv => "red-export.csv",
            ExportFormat::Json => "red-export.json",
            ExportFormat::Html => "red-report.html",
        };
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some(name));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update(cx, |this, cx| this.start_export(format, path, epoch, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Begin an export once the save path is chosen: allocate its id, fire the
    /// command off to the backend, and stand up the persistent progress toast
    /// (its `✕` is a Cancel — see [`AppState::close_notification`]). The total is
    /// the open result's row count, already known from `ResultReady`.
    fn start_export(
        &mut self,
        format: ExportFormat,
        path: PathBuf,
        epoch: u64,
        cx: &mut Context<Self>,
    ) {
        let total = match &self.phase {
            Phase::Connected(a) => a.active_result().map(|g| g.total_rows()).unwrap_or(0),
            _ => 0,
        };
        let id = self.next_export_id;
        self.next_export_id += 1;
        self.send_active(Command::Export {
            format,
            path,
            epoch,
            id,
        });
        self.push_notification(
            Notification {
                id: 0,
                variant: ToastVariant::Info,
                message: "Exporting…".into(),
                detail: None,
                detail_label: None,
                auto_dismiss: None,
                export: Some(ExportProgress {
                    id,
                    rows: 0,
                    total,
                    kind: TransferKind::Export,
                }),
                expanded: false,
                hovered: false,
                dismiss_gen: 0,
                action: None,
            },
            cx,
        );
    }

    // --- export progress events ---

    /// The notification id of the export toast carrying `export_id`, if it's still
    /// on screen.
    fn export_notification_id(&self, export_id: u64) -> Option<u64> {
        self.notifications
            .iter()
            .find(|n| n.export.as_ref().is_some_and(|e| e.id == export_id))
            .map(|n| n.id)
    }

    /// `ExportProgress`: advance the export toast's row count + percentage.
    pub(crate) fn on_export_progress(&mut self, id: u64, rows: usize, cx: &mut Context<Self>) {
        if let Some(n) = self
            .notifications
            .iter_mut()
            .find(|n| n.export.as_ref().is_some_and(|e| e.id == id))
        {
            if let Some(export) = &mut n.export {
                export.rows = rows;
                let pct = rows
                    .saturating_mul(100)
                    .checked_div(export.total)
                    .unwrap_or(0)
                    .min(100);
                n.message = format!("Exporting… {pct}%").into();
            }
        }
        cx.notify();
    }

    /// `ExportFinished`: drop the progress toast, leave an auto-dismissing success.
    pub(crate) fn on_export_finished(
        &mut self,
        id: u64,
        path: String,
        rows: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(nid) = self.export_notification_id(id) {
            self.dismiss(nid, cx);
        }
        self.notify(
            ToastVariant::Success,
            format!("Exported {rows} row(s) to {path}"),
            cx,
        );
    }

    /// `ExportCancelled`: drop the progress toast, leave an auto-dismissing notice.
    pub(crate) fn on_export_cancelled(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(nid) = self.export_notification_id(id) {
            self.dismiss(nid, cx);
        }
        self.notify(ToastVariant::Info, "Export cancelled", cx);
    }

    // --- import progress events (data import) ---

    /// `ImportProgress`: advance the import toast's running committed-row count (an
    /// import streams a file of unknown length, so it shows a count, not a %).
    pub(crate) fn on_import_progress(&mut self, id: u64, rows: usize, cx: &mut Context<Self>) {
        if let Some(n) = self
            .notifications
            .iter_mut()
            .find(|n| n.export.as_ref().is_some_and(|e| e.id == id))
        {
            if let Some(t) = &mut n.export {
                t.rows = rows;
            }
            n.message = format!("Importing… {rows} row(s)").into();
        }
        cx.notify();
    }

    /// `ImportFinished`: drop the progress toast, leave an auto-dismissing success.
    pub(crate) fn on_import_finished(&mut self, id: u64, rows: usize, cx: &mut Context<Self>) {
        if let Some(nid) = self.export_notification_id(id) {
            self.dismiss(nid, cx);
        }
        self.notify(ToastVariant::Success, format!("Imported {rows} row(s)"), cx);
    }

    /// `ImportFailed`: drop the progress toast, surface the error. Inserts commit per
    /// chunk, so the message says how far it got.
    pub(crate) fn on_import_failed(
        &mut self,
        id: u64,
        rows: usize,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(nid) = self.export_notification_id(id) {
            self.dismiss(nid, cx);
        }
        let msg = if rows > 0 {
            format!("Import failed after {rows} row(s): {message}")
        } else {
            format!("Import failed: {message}")
        };
        self.notify(ToastVariant::Error, msg, cx);
    }

    /// `ImportCancelled`: drop the progress toast; earlier chunks stay committed.
    pub(crate) fn on_import_cancelled(&mut self, id: u64, rows: usize, cx: &mut Context<Self>) {
        if let Some(nid) = self.export_notification_id(id) {
            self.dismiss(nid, cx);
        }
        let msg = if rows > 0 {
            format!("Import cancelled ({rows} row(s) kept)")
        } else {
            "Import cancelled".to_string()
        };
        self.notify(ToastVariant::Info, msg, cx);
    }

    // --- import trigger (file pick → peek → confirm → import) ---

    /// "Import…" on a single-table browse: pick a CSV/JSONL file, then peek its
    /// header (backend) so a name-based mapping can be built and **confirmed before
    /// any write**. No-op (with a hint) on editor SQL / joins — no single target.
    pub(crate) fn import_into_result(&mut self, cx: &mut Context<Self>) {
        let target = match &self.phase {
            Phase::Connected(a) => a.active_result().and_then(|g| g.import_target()),
            _ => None,
        };
        let Some((target, target_cols)) = target else {
            self.notify(
                ToastVariant::Info,
                "Open a single table to import into it",
                cx,
            );
            return;
        };
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import data file".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                if let Some(path) = paths.into_iter().next() {
                    this.update(cx, |this, _| {
                        this.begin_import_peek(path, target, target_cols)
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    /// File chosen: infer the format from its extension, stash the pending peek, and
    /// ask the backend for the file's source column names.
    fn begin_import_peek(
        &mut self,
        path: PathBuf,
        target: TableRef,
        target_cols: Vec<ResultColumn>,
    ) {
        let format = match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("jsonl") | Some("ndjson") => ImportFormat::Jsonl,
            _ => ImportFormat::Csv,
        };
        let id = self.next_export_id;
        self.next_export_id += 1;
        self.pending_import = Some(PendingImportPeek {
            id,
            path: path.clone(),
            format,
            target,
            target_cols,
        });
        self.send_active(Command::ImportColumns { path, format, id });
    }

    /// `ImportColumns`: the file's source columns arrived — build a name-based
    /// mapping against the pending target, summarize it, and raise the import confirm
    /// so the user sees the file→table mapping before any write.
    pub(crate) fn on_import_columns(
        &mut self,
        id: u64,
        source_cols: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(peek) = self.pending_import.take().filter(|p| p.id == id) else {
            return;
        };
        let lower: Vec<String> = source_cols.iter().map(|s| s.to_ascii_lowercase()).collect();
        let mut mapping = Vec::new();
        let mut unmatched_target = Vec::new();
        for col in &peek.target_cols {
            match lower
                .iter()
                .position(|s| *s == col.name.to_ascii_lowercase())
            {
                Some(idx) => mapping.push(ColumnMap {
                    source: idx,
                    column: col.name.clone(),
                    decl_type: col.decl_type.clone(),
                }),
                None => unmatched_target.push(col.name.clone()),
            }
        }
        if mapping.is_empty() {
            self.notify(
                ToastVariant::Error,
                "No columns in the file match this table's columns",
                cx,
            );
            return;
        }
        let ignored_source: Vec<String> = source_cols
            .iter()
            .enumerate()
            .filter(|(i, _)| !mapping.iter().any(|m| m.source == *i))
            .map(|(_, s)| s.clone())
            .collect();
        let file = peek
            .path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("file")
            .to_string();
        let table_disp = match &peek.target.schema {
            Some(s) => format!("{s}.{}", peek.target.name),
            None => peek.target.name.clone(),
        };
        let prose = format!(
            "Append rows from {file} into {table_disp}. {} column(s) matched by name; \
             rows insert in chunks and commit per chunk.",
            mapping.len()
        );
        let mut preview = mapping
            .iter()
            .map(|m| {
                format!(
                    "{}  ←  {}",
                    m.column,
                    source_cols.get(m.source).map(String::as_str).unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !unmatched_target.is_empty() {
            preview.push_str(&format!(
                "\n\nTarget columns left to default/NULL: {}",
                unmatched_target.join(", ")
            ));
        }
        if !ignored_source.is_empty() {
            preview.push_str(&format!(
                "\nFile columns ignored: {}",
                ignored_source.join(", ")
            ));
        }
        self.confirm_exec = Some(PendingWrite::Import {
            path: peek.path,
            format: peek.format,
            target: peek.target,
            mapping,
            id: peek.id,
            prose,
            preview,
        });
        cx.notify();
    }

    /// Confirmed: fire the import and stand up the progress toast (its `✕` cancels).
    pub(crate) fn start_import(
        &mut self,
        path: PathBuf,
        format: ImportFormat,
        target: TableRef,
        mapping: Vec<ColumnMap>,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        /// Default rows per insert chunk (the driver re-clamps to its parameter cap).
        /// A `[import]` setting is a later refinement.
        const DEFAULT_IMPORT_CHUNK: usize = 500;
        self.send_active(Command::Import {
            path,
            format,
            target,
            mapping,
            chunk_size: DEFAULT_IMPORT_CHUNK,
            id,
        });
        self.push_notification(
            Notification {
                id: 0,
                variant: ToastVariant::Info,
                message: "Importing…".into(),
                detail: None,
                detail_label: None,
                auto_dismiss: None,
                export: Some(ExportProgress {
                    id,
                    rows: 0,
                    total: 0,
                    kind: TransferKind::Import,
                }),
                expanded: false,
                hovered: false,
                dismiss_gen: 0,
                action: None,
            },
            cx,
        );
    }

    /// "Go to row N" from the palette prompt — `one_based` is the row number the
    /// user typed (1-based). Scrolls the active result's grid to that exact row,
    /// clamped to the result's bounds. No-op when no result is open.
    pub(crate) fn go_to_row(&mut self, one_based: usize, cx: &mut Context<Self>) {
        let row_height = f32::from(self.settings.grid.density.row_height());
        if let Phase::Connected(active) = &self.phase {
            if let Some(grid) = active.active_result() {
                grid.go_to_row(one_based.saturating_sub(1), row_height);
            }
        }
        cx.notify();
    }

    pub(crate) fn copy_result_selection(&mut self, cx: &mut Context<Self>) {
        let gutter = self.gutter();
        let plan = match &self.phase {
            Phase::Connected(active) => active.active_result().and_then(|g| g.copy_plan(gutter)),
            _ => None,
        };
        match plan {
            // Everything selected is resident in full — copy straight away.
            Some(CopyPlan::Ready(tsv)) => {
                cx.write_to_clipboard(ClipboardItem::new_string(tsv));
            }
            // The selection touches display-clipped text; re-fetch the rows in
            // full, then `on_copy_rows` assembles the clipboard from the reply.
            Some(CopyPlan::Refetch {
                epoch,
                offset,
                limit,
                dcol_lo,
                dcol_hi,
            }) => {
                let id = self.next_copy_id;
                self.next_copy_id += 1;
                self.pending_copy = Some(PendingCopy {
                    id,
                    dcol_lo,
                    dcol_hi,
                });
                self.send_active(Command::CopyRows {
                    offset,
                    limit,
                    epoch,
                    id,
                });
            }
            None => {}
        }
    }

    /// A `CopyRows` reply landed: if it's the copy still pending, assemble the
    /// untruncated selection and put it on the clipboard. A superseded reply (the
    /// user copied again before this returned) finds a stale id and is dropped.
    pub(crate) fn on_copy_rows(&mut self, id: u64, rows: Vec<Vec<Value>>, cx: &mut Context<Self>) {
        // The detail inspector draws full values from the same `CopyRows` path; if
        // this reply is its in-flight fetch, it claims it (and never reaches the
        // clipboard). Ids come from one counter, so the three never collide.
        if self.on_inspect_rows(id, &rows) {
            cx.notify();
            return;
        }
        // An FK click-through (Track B7) also re-fetches one row in full to read the
        // typed key; if this reply is its, it opens the target browse.
        if self.on_fk_rows(id, &rows, cx) {
            cx.notify();
            return;
        }
        let Some(pending) = self.pending_copy.take_if(|p| p.id == id) else {
            return;
        };
        let tsv = rows_tsv(&rows, pending.dcol_lo, pending.dcol_hi);
        cx.write_to_clipboard(ClipboardItem::new_string(tsv));
    }

    /// FK click-through affordances for the focused cell's grid (Track B7): the
    /// forward target table name when the focused column is a single-column FK, and
    /// the reverse edges (tables referencing this grid's table) as
    /// `(child_schema, child_table, from_column, to_column)`. Empty without a base
    /// table or a loaded graph. Drives the result cell context menu.
    pub(crate) fn fk_menu(&self) -> (Option<String>, Vec<FkReverse>) {
        let empty = (None, Vec::new());
        let Phase::Connected(active) = &self.phase else {
            return empty;
        };
        let Some(grid) = active.active_result() else {
            return empty;
        };
        let Some((schema, table)) = grid.table.as_ref() else {
            return empty;
        };
        let forward = grid.cursor_cell(self.gutter()).and_then(|(_, col)| {
            let cname = grid.columns.get(col)?.name.clone();
            active
                .fk_graph
                .iter()
                .find(|e| {
                    e.columns.len() == 1
                        && e.from_table == *table
                        && e.from_schema.as_deref() == Some(schema.as_str())
                        && e.columns[0].0 == cname
                })
                .map(|e| e.to_table.clone())
        });
        let reverse = active
            .fk_graph
            .iter()
            .filter(|e| {
                e.columns.len() == 1
                    && e.to_table == *table
                    && e.to_schema.as_deref() == Some(schema.as_str())
            })
            .map(|e| FkReverse {
                schema: e.from_schema.clone().unwrap_or_default(),
                table: e.from_table.clone(),
                from_column: e.columns[0].0.clone(),
                to_column: e.columns[0].1.clone(),
            })
            .collect();
        (forward, reverse)
    }

    /// "Go to referenced row": resolve the forward FK of the focused cell, then fetch
    /// that cell's full value (the target browse opens in [`on_fk_rows`]). No-op if
    /// the focused column isn't a single-column FK or the graph hasn't loaded.
    pub(crate) fn follow_fk_forward(&mut self, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let Some(grid) = active.active_result() else {
            return;
        };
        let Some((schema, table)) = grid.table.as_ref() else {
            return;
        };
        let Some((row, col)) = grid.cursor_cell(self.gutter()) else {
            return;
        };
        let Some(cname) = grid.columns.get(col).map(|c| c.name.clone()) else {
            return;
        };
        let Some(edge) = active.fk_graph.iter().find(|e| {
            e.columns.len() == 1
                && e.from_table == *table
                && e.from_schema.as_deref() == Some(schema.as_str())
                && e.columns[0].0 == cname
        }) else {
            return;
        };
        let plan = FkPlan {
            epoch: grid.epoch,
            row,
            target: (
                edge.to_schema.clone().unwrap_or_else(|| schema.clone()),
                edge.to_table.clone(),
            ),
            filters: vec![(edge.columns[0].1.clone(), col)],
        };
        self.begin_fk_follow(plan, cx);
    }

    /// "Show referencing rows": open `child` filtered to `child.from_column =
    /// <this row's to_column value>`. The source value is read from the focused
    /// row's `to_column` (the referenced column — usually a PK), fetched in full.
    pub(crate) fn follow_fk_reverse(
        &mut self,
        child_schema: String,
        child_table: String,
        from_column: String,
        to_column: String,
        cx: &mut Context<Self>,
    ) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let Some(grid) = active.active_result() else {
            return;
        };
        let Some((row, _)) = grid.cursor_cell(self.gutter()) else {
            return;
        };
        let Some(src) = grid.columns.iter().position(|c| c.name == to_column) else {
            return;
        };
        let plan = FkPlan {
            epoch: grid.epoch,
            row,
            target: (child_schema, child_table),
            filters: vec![(from_column, src)],
        };
        self.begin_fk_follow(plan, cx);
    }

    /// Issue the single-row `CopyRows` re-fetch that backs an FK follow, recording
    /// the plan so [`on_fk_rows`](Self::on_fk_rows) can complete it on the reply.
    fn begin_fk_follow(&mut self, plan: FkPlan, cx: &mut Context<Self>) {
        let id = self.next_copy_id;
        self.next_copy_id += 1;
        let (epoch, row) = (plan.epoch, plan.row);
        self.pending_fk = Some(PendingFkFollow { id, plan });
        self.send_active(Command::CopyRows {
            offset: row,
            limit: 1,
            epoch,
            id,
        });
        cx.notify();
    }

    /// A `CopyRows` reply claimed by a pending FK follow: read the typed key
    /// value(s) and open the target browse filtered to them. A NULL key isn't
    /// followable (nothing to point at), so it's reported and dropped. Returns
    /// whether it claimed the reply.
    fn on_fk_rows(&mut self, id: u64, rows: &[Vec<Value>], cx: &mut Context<Self>) -> bool {
        let Some(p) = self.pending_fk.take_if(|p| p.id == id) else {
            return false;
        };
        let Some(row) = rows.first() else {
            return true;
        };
        let mut pairs = Vec::with_capacity(p.plan.filters.len());
        for (target_col, src) in &p.plan.filters {
            match row.get(*src) {
                Some(Value::Null) | None => {
                    self.notify(
                        ToastVariant::Info,
                        "Referenced value is NULL — nothing to follow",
                        cx,
                    );
                    return true;
                }
                Some(value) => pairs.push(ColumnValue {
                    column: target_col.clone(),
                    value: value.clone(),
                    decl_type: None,
                }),
            }
        }
        let (schema, table) = p.plan.target;
        self.open_table_browse(schema, table, Some(ResultFilter::Eq(pairs)), cx);
        true
    }
}

#[cfg(test)]
mod join_tests {
    use super::{build_joins, ExpandedCol};
    use red_core::FkEdge;

    fn edge(from: &str, fk: &str, to: &str, refc: &str) -> FkEdge {
        FkEdge {
            from_schema: Some("main".into()),
            from_table: from.into(),
            to_schema: Some("main".into()),
            to_table: to.into(),
            columns: vec![(fk.into(), refc.into())],
        }
    }

    fn exp(path: &[&str]) -> ExpandedCol {
        ExpandedCol {
            path: path.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A two-hop chain (`channel → tier → cascade`): the shared first hop is
    /// resolved once (deduped), the deeper hop chains off it (`parent_alias` is the
    /// first hop's alias), and each leaf is selected from its hop with the dotted
    /// output alias.
    #[test]
    fn chains_and_dedupes_shared_prefixes() {
        let graph = vec![
            edge("channel", "tier_id", "tier", "id"),
            edge("tier", "cascade_id", "cascade", "id"),
        ];
        let expansion = vec![
            exp(&["tier_id", "name"]),
            exp(&["tier_id", "cascade_id", "name"]),
        ];
        let joins = build_joins(&graph, ("main", "channel"), &expansion);

        assert_eq!(joins.len(), 2, "shared `tier_id` hop is joined once");
        // First hop: off the base, selecting tier.name.
        assert_eq!(joins[0].alias, "_red_j0");
        assert_eq!(joins[0].parent_alias, "_red_base");
        assert_eq!(joins[0].to_table, "tier");
        assert_eq!(joins[0].on, vec![("tier_id".into(), "id".into())]);
        assert_eq!(
            joins[0].select,
            vec![("name".into(), "tier_id.name".into())]
        );
        // Second hop: chains off the first, selecting cascade.name.
        assert_eq!(joins[1].alias, "_red_j1");
        assert_eq!(joins[1].parent_alias, "_red_j0");
        assert_eq!(joins[1].to_table, "cascade");
        assert_eq!(
            joins[1].select,
            vec![("name".into(), "tier_id.cascade_id.name".into())]
        );
    }

    /// A path whose FK can't be resolved against the graph is skipped, leaving the
    /// resolvable ones intact (a missing edge degrades to no join, never a panic).
    #[test]
    fn skips_unresolvable_paths() {
        let graph = vec![edge("channel", "tier_id", "tier", "id")];
        let expansion = vec![exp(&["bogus", "x"]), exp(&["tier_id", "name"])];
        let joins = build_joins(&graph, ("main", "channel"), &expansion);
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].to_table, "tier");
        assert_eq!(
            joins[0].select,
            vec![("name".into(), "tier_id.name".into())]
        );
    }
}

/// Editing an inline-expanded FK column (Track B7): a joined cell resolves to an
/// `UPDATE` against its *referenced* table, keyed by the FK value from the base row.
#[cfg(test)]
mod foreign_edit_tests {
    use super::*;
    use crate::result::edit::{PkKey, StagedCell, UpdatedRow};
    use red_core::{Column, EditOp, FkJoin, KeyKind, KeySpec, TableRef, Value, BASE_ALIAS};
    use red_service::{spawn, SessionId};
    use std::collections::HashMap;

    fn col(name: &str, ty: &str) -> Column {
        Column {
            name: name.into(),
            decl_type: Some(ty.into()),
        }
    }

    /// A single-hop expanded browse: `channel(id, tier_id)` with `tier_id.name`
    /// pulled inline from `tier` via a `LEFT JOIN`, with one row resident
    /// (`id=10, tier_id=3, tier_id.name='Gold'`).
    fn expanded_grid() -> ResultGrid {
        let handle = spawn();
        let sender = handle.command_sender(SessionId(1));
        let mut grid = ResultGrid::new(
            "channel".into(),
            "SELECT * FROM channel".into(),
            Some(("main".into(), "channel".into())),
            sender,
            100,
        );
        grid.columns = vec![
            col("id", "integer"),
            col("tier_id", "integer"),
            col("tier_id.name", "text"),
        ];
        grid.key = Some(KeySpec::single("id", KeyKind::Int));
        grid.joins = vec![FkJoin {
            alias: "_red_j0".into(),
            parent_alias: BASE_ALIAS.into(),
            on: vec![("tier_id".into(), "id".into())],
            to_schema: Some("main".into()),
            to_table: "tier".into(),
            select: vec![("name".into(), "tier_id.name".into())],
        }];
        grid.expansion = vec![ExpandedCol {
            path: vec!["tier_id".into(), "name".into()],
        }];
        grid.set_joined_cols();
        grid.buffer.borrow_mut().insert_page(
            0,
            vec![vec![
                Value::Integer(10),
                Value::Integer(3),
                Value::Text("Gold".into()),
            ]],
        );
        grid
    }

    #[test]
    fn resolves_single_hop_target() {
        let grid = expanded_grid();
        // Column 2 is `tier_id.name`; row 0's `tier_id` is 3.
        let f = grid
            .foreign_edit_for(2, 0)
            .expect("single-hop FK column is editable");
        assert_eq!(
            f.table,
            TableRef {
                schema: Some("main".into()),
                name: "tier".into()
            }
        );
        assert_eq!(f.key_column, "id");
        assert_eq!(f.key_value, Value::Integer(3)); // the FK value, not the base PK
        assert_eq!(f.set_column, "name"); // the leaf, not the dotted alias
        assert_eq!(f.key_type.as_deref(), Some("integer")); // for the WHERE cast
    }

    #[test]
    fn refuses_nested_base_and_null_fk() {
        // A nested hop (parent is another join, not the base) stays read-only — the
        // referenced row's key value wouldn't be resident in the base row.
        let mut nested = expanded_grid();
        nested.joins[0].parent_alias = "_red_j9".into();
        assert!(nested.foreign_edit_for(2, 0).is_none());

        // A base column isn't a join output — no foreign target.
        let base = expanded_grid();
        assert!(base.foreign_edit_for(0, 0).is_none());

        // An orphaned / NULL FK has no referenced row to update.
        let null_fk = expanded_grid();
        null_fk.buffer.borrow_mut().patch_cell(0, 1, Value::Null);
        assert!(null_fk.foreign_edit_for(2, 0).is_none());
    }

    #[test]
    fn build_ops_splits_base_and_foreign_updates() {
        let mut grid = expanded_grid();
        let f = grid.foreign_edit_for(2, 0).unwrap();
        // Stage a base edit (`tier_id`) and a foreign edit (`tier_id.name`) on one row.
        let mut cells = HashMap::new();
        cells.insert(
            1,
            StagedCell {
                value: Value::Integer(4),
                foreign: None,
            },
        );
        cells.insert(
            2,
            StagedCell {
                value: Value::Text("Platinum".into()),
                foreign: Some(f),
            },
        );
        grid.pending.updates.insert(
            PkKey::Int(10),
            UpdatedRow {
                pk_value: Value::Integer(10),
                row: 0,
                cells,
            },
        );
        let ops = grid.build_edit_ops();
        assert_eq!(
            ops.len(),
            2,
            "one base UPDATE + one referenced-table UPDATE"
        );

        let (base_key, base_set) = ops
            .iter()
            .find_map(|op| match op {
                EditOp::Update { table, key, set } if table.name == "channel" => Some((key, set)),
                _ => None,
            })
            .expect("base UPDATE present");
        assert_eq!(base_key.column, "id");
        assert_eq!(base_key.value, Value::Integer(10));
        assert_eq!(base_set.len(), 1);
        assert_eq!(base_set[0].column, "tier_id");

        let (fk_key, fk_set) = ops
            .iter()
            .find_map(|op| match op {
                EditOp::Update { table, key, set } if table.name == "tier" => Some((key, set)),
                _ => None,
            })
            .expect("referenced-table UPDATE present");
        assert_eq!(fk_key.column, "id");
        assert_eq!(fk_key.value, Value::Integer(3)); // the FK value identifies the ref row
        assert_eq!(fk_set[0].column, "name"); // leaf, not the dotted alias
        assert_eq!(fk_set[0].value, Value::Text("Platinum".into()));
        assert_eq!(fk_set[0].decl_type.as_deref(), Some("text"));
    }
}
