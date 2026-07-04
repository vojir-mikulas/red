//! Track B6: DataGrip-style staged grid editing.
//!
//! Editing is no longer a per-cell modal round-trip (the old B5 palette prompt).
//! The user edits cells *in place*, the changes accumulate in a per-result
//! [`PendingChanges`] set (marked dirty in the grid), and one **Submit** flushes
//! the whole set to the backend as a single transactional batch (`ApplyBatch`);
//! **Revert** drops it.
//!
//! The load-bearing decision: staged edits key by **primary key**, not row index,
//! so they survive the windowed buffer's eviction; a dirty cell is recognised by
//! its row's PK at paint time. The set is bounded by how many edits the user made,
//! never by result size, so it stays inside the perf budget.

use std::collections::{HashMap, HashSet};

use flint::{TextInput, TextInputEvent, ToastVariant};
use gpui::{prelude::*, Context, Entity, Focusable, Subscription};
use red_core::{coerce_edit_value, ColumnValue, EditOp, TableRef, Value};

use super::buffer::DisplayCell;
use super::ResultGrid;
use crate::app::{AppState, ForeignEdit, Pane, PendingWrite, Phase};

/// A hashable identity for a row's primary-key value, so staged edits survive the
/// windowed buffer's eviction (they key by PK, not by row index). Only the PK
/// types a keyed browse actually exposes are representable; a real/blob/NULL PK
/// yields `None` and the cell simply isn't stageable (the edit gate already
/// rejects those).
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum PkKey {
    Int(i64),
    Text(String),
}

impl PkKey {
    pub(crate) fn from_value(v: &Value) -> Option<PkKey> {
        match v {
            Value::Integer(n) => Some(PkKey::Int(*n)),
            Value::Text(s) => Some(PkKey::Text(s.clone())),
            _ => None,
        }
    }
}

/// One staged cell change: the new value, plus, for an inline-expanded FK column
/// (Track B7), the referenced-table target the edit writes to. A base-table cell
/// carries `foreign = None` and is written via its row's PK; a joined cell carries
/// the [`ForeignEdit`] resolved when the edit began, so submit needn't re-resolve it
/// against a possibly-evicted buffer row.
pub(crate) struct StagedCell {
    pub(crate) value: Value,
    pub(crate) foreign: Option<ForeignEdit>,
}

/// One staged row update: the columns the user changed (data-column index → staged
/// cell), the PK value (to build the base `UPDATE`), and the absolute row the PK sat
/// at when staged. The row stays valid for an updates-only batch (no rows move),
/// so submit can patch the resident buffer in place without a refetch.
pub(crate) struct UpdatedRow {
    pub(crate) pk_value: Value,
    pub(crate) row: usize,
    pub(crate) cells: HashMap<usize, StagedCell>,
}

/// One row marked for deletion: the PK value (to build the `DELETE`) and the
/// absolute row it sat at when marked (to paint it struck-through; stays valid
/// until a structural submit reloads the result).
pub(crate) struct DeletedRow {
    pub(crate) pk_value: Value,
    pub(crate) row: usize,
}

/// A draft row the user is composing for `INSERT`: per-column staged values.
/// Columns left unset take the engine default; an all-unset draft is skipped at
/// submit (an empty `INSERT` is invalid).
#[derive(Default)]
pub(crate) struct DraftRow {
    pub(crate) cells: HashMap<usize, Value>,
}

/// All staged, not-yet-submitted edits for one result (Track B6). Lives on the
/// [`ResultGrid`], so it's naturally scoped per result and cleared whenever the
/// result is (re)opened, sorted, or filtered.
#[derive(Default)]
pub(crate) struct PendingChanges {
    /// PK → the row's staged column changes.
    pub(crate) updates: HashMap<PkKey, UpdatedRow>,
    /// PK → the row marked for deletion.
    pub(crate) deletes: HashMap<PkKey, DeletedRow>,
    /// Locally-authored draft rows, rendered in the grid's bottom zone.
    pub(crate) inserts: Vec<DraftRow>,
}

impl PendingChanges {
    pub(crate) fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.deletes.is_empty() && self.inserts.is_empty()
    }

    /// The staged value for a resident row's `(pk, data_col)`, for the render
    /// overlay. `None` when that cell isn't dirty.
    pub(crate) fn cell_override(&self, pk: &PkKey, col: usize) -> Option<&Value> {
        self.updates
            .get(pk)
            .and_then(|u| u.cells.get(&col))
            .map(|c| &c.value)
    }

    /// A render overlay snapshot for the visible grid: each staged cell formatted to
    /// its [`DisplayCell`] (keyed by `(abs_row, data_col)`), and the absolute rows
    /// marked for deletion. Bounded by edits made, so it's cheap to clone per frame.
    pub(crate) fn overlay(&self) -> EditOverlay {
        let cells = self
            .updates
            .values()
            .flat_map(|u| {
                u.cells
                    .iter()
                    .map(move |(col, c)| ((u.row, *col), DisplayCell::from_value(&c.value)))
            })
            .collect();
        let deleted = self.deletes.values().map(|d| d.row).collect();
        EditOverlay { cells, deleted }
    }

    /// A compact status-bar summary (`"2 edits · 1 delete · 3 new"`), or `None`
    /// when nothing is staged.
    pub(crate) fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        let edits: usize = self.updates.values().map(|u| u.cells.len()).sum();
        if edits > 0 {
            parts.push(format!("{edits} edit{}", plural(edits)));
        }
        if !self.deletes.is_empty() {
            parts.push(format!(
                "{} delete{}",
                self.deletes.len(),
                plural(self.deletes.len())
            ));
        }
        if !self.inserts.is_empty() {
            parts.push(format!("{} new", self.inserts.len()));
        }
        Some(parts.join(" · "))
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// The per-frame render overlay built by [`PendingChanges::overlay`]: staged cell
/// displays keyed by `(abs_row, data_col)`, and the rows marked for deletion.
pub(crate) struct EditOverlay {
    pub(crate) cells: HashMap<(usize, usize), DisplayCell>,
    pub(crate) deleted: HashSet<usize>,
}

/// The cell an open inline editor targets: an existing keyed row, or a draft
/// (insert) row identified by its index in [`PendingChanges::inserts`].
// `Row` is inherently far larger than `Draft` (it carries the cell's full identity:
// two `Value`s plus the FK write target); this enum is single-instance, short-lived
// app state (one open editor), so the size skew doesn't warrant boxing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub(crate) enum EditSlot {
    Row {
        row: usize,
        data_col: usize,
        pk_value: Value,
        original: Value,
        /// Set when the cell is an inline-expanded FK column: the referenced-table
        /// write target (Track B7). `None` for an ordinary base-table cell.
        foreign: Option<ForeignEdit>,
    },
    Draft {
        index: usize,
        data_col: usize,
    },
}

/// An open inline cell editor (Track B6): the `TextInput` hosted in the focused
/// cell, the slot it targets, and the column metadata used to coerce the typed
/// text. The event subscription is held (not detached) so dropping this closes the
/// editor and unsubscribes.
pub(crate) struct GridEdit {
    pub(crate) input: Entity<TextInput>,
    pub(crate) slot: EditSlot,
    pub(crate) decl_type: Option<String>,
    pub(crate) epoch: u64,
    _sub: Subscription,
}

impl ResultGrid {
    /// Build the ordered batch of [`EditOp`]s the staged change-set represents:
    /// updates, then deletes, then draft inserts. Empty (no-column) updates/inserts
    /// are skipped. Returns an empty vec when the result has no usable PK (it can't
    /// be edited); the caller treats that as nothing to submit.
    pub(in crate::result) fn build_edit_ops(&self) -> Vec<EditOp> {
        let Some((schema, name)) = self.table.clone() else {
            return Vec::new();
        };
        let Some(key) = self.key.as_ref() else {
            return Vec::new();
        };
        let pk_column = key.tiebreak.clone().unwrap_or_else(|| key.column.clone());
        let tref = || TableRef {
            schema: Some(schema.clone()),
            name: name.clone(),
        };
        // The column's `(name, declared type)`; the type rides along so the driver
        // can bind a text-decoded value (jsonb, timestamp, …) back into its column.
        let col_meta = |c: usize| {
            self.columns
                .get(c)
                .map(|col| (col.name.clone(), col.decl_type.clone()))
        };
        // A key column binds fine without a type (always int/text), so it carries None.
        let key_cv = |value: Value| ColumnValue {
            column: pk_column.clone(),
            value,
            decl_type: None,
        };
        let mut ops = Vec::new();

        for u in self.pending.updates.values() {
            // Base-table cells fold into one `UPDATE … WHERE pk = ?`; each inline-
            // expanded FK cell (Track B7) is its own `UPDATE <ref> … WHERE <fk key>`
            // against the referenced table it came from.
            let mut set: Vec<ColumnValue> = Vec::new();
            for (c, cell) in &u.cells {
                match &cell.foreign {
                    None => {
                        if let Some((column, decl_type)) = col_meta(*c) {
                            set.push(ColumnValue {
                                column,
                                value: cell.value.clone(),
                                decl_type,
                            });
                        }
                    }
                    Some(f) => ops.push(EditOp::Update {
                        table: f.table.clone(),
                        key: ColumnValue {
                            column: f.key_column.clone(),
                            value: f.key_value.clone(),
                            decl_type: f.key_type.clone(),
                        },
                        set: vec![ColumnValue {
                            column: f.set_column.clone(),
                            value: cell.value.clone(),
                            // The referenced column's type (the joined result column)
                            // rides along so a jsonb/uuid/timestamp value casts back.
                            decl_type: col_meta(*c).and_then(|(_, dt)| dt),
                        }],
                    }),
                }
            }
            if set.is_empty() {
                continue;
            }
            ops.push(EditOp::Update {
                table: tref(),
                key: key_cv(u.pk_value.clone()),
                set,
            });
        }
        for d in self.pending.deletes.values() {
            ops.push(EditOp::Delete {
                table: tref(),
                key: key_cv(d.pk_value.clone()),
            });
        }
        for draft in &self.pending.inserts {
            let values: Vec<ColumnValue> = draft
                .cells
                .iter()
                .filter_map(|(c, v)| {
                    col_meta(*c).map(|(column, decl_type)| ColumnValue {
                        column,
                        value: v.clone(),
                        decl_type,
                    })
                })
                .collect();
            if values.is_empty() {
                continue;
            }
            ops.push(EditOp::Insert {
                table: tref(),
                values,
            });
        }
        ops
    }
}

impl AppState {
    // --- inline editing ---

    /// Begin editing the focused result cell in place (Enter / F2 / double-click).
    /// No-op when the cell isn't editable (read-only connection, not a single-table
    /// keyed browse, the PK column, or a binary/clipped cell). Prefills with the
    /// cell's *effective* current value (a prior staged edit if there is one) so a
    /// tweak is one keystroke; Enter stages it, Esc abandons.
    pub(crate) fn begin_grid_edit(&mut self, cx: &mut Context<Self>) {
        let Some(ctx) = self.active_edit_target() else {
            return;
        };
        let Some(pk) = PkKey::from_value(&ctx.pk_value) else {
            return;
        };
        // Effective current value: a staged override wins over the resident original.
        let current = match &self.phase {
            Phase::Connected(active) => active
                .active_result()
                .and_then(|g| g.pending.cell_override(&pk, ctx.data_col).cloned())
                .unwrap_or_else(|| ctx.original.clone()),
            _ => ctx.original.clone(),
        };
        let slot = EditSlot::Row {
            row: ctx.row,
            data_col: ctx.data_col,
            pk_value: ctx.pk_value.clone(),
            original: ctx.original.clone(),
            foreign: ctx.foreign.clone(),
        };
        self.open_cell_editor(slot, ctx.decl_type.clone(), ctx.epoch, &current, cx);
    }

    /// Begin editing a draft (insert) row's cell, from a click in the draft zone.
    pub(crate) fn begin_draft_edit(
        &mut self,
        index: usize,
        data_col: usize,
        cx: &mut Context<Self>,
    ) {
        let (epoch, decl_type, current) = match &self.phase {
            Phase::Connected(active) => match active.active_result() {
                Some(g) if index < g.pending.inserts.len() => {
                    let decl = g.columns.get(data_col).and_then(|c| c.decl_type.clone());
                    let cur = g.pending.inserts[index]
                        .cells
                        .get(&data_col)
                        .cloned()
                        .unwrap_or(Value::Null);
                    (g.epoch, decl, cur)
                }
                _ => return,
            },
            _ => return,
        };
        self.open_cell_editor(
            EditSlot::Draft { index, data_col },
            decl_type,
            epoch,
            &current,
            cx,
        );
    }

    /// Stand up the inline `TextInput` for `slot`, prefilled with `current`'s text,
    /// and route its Enter/Esc to commit/cancel. Shared by the row and draft paths.
    fn open_cell_editor(
        &mut self,
        slot: EditSlot,
        decl_type: Option<String>,
        epoch: u64,
        current: &Value,
        cx: &mut Context<Self>,
    ) {
        let prefill = match current {
            Value::Null => String::new(),
            other => other.to_string(),
        };
        let input = cx.new(|cx| {
            // `bare`: no box of its own; it fills the grid cell, inheriting the
            // row's height, padding, font, and selection highlight, so the cell
            // itself becomes the input rather than a smaller box inside it.
            let mut input = TextInput::new(cx).bare();
            input.set_content(prefill, cx);
            input
        });
        let sub = cx.subscribe(&input, |this, _, event: &TextInputEvent, cx| match event {
            TextInputEvent::Submit => this.commit_grid_edit(cx),
            TextInputEvent::Cancel => this.cancel_grid_edit(cx),
            TextInputEvent::Change => {}
        });
        self.grid_edit = Some(GridEdit {
            input,
            slot,
            decl_type,
            epoch,
            _sub: sub,
        });
        // Drop any prior commit-on-blur listener so render re-registers it against
        // this new field's focus handle (moving straight from one cell to another).
        self.grid_edit_blur = None;
        self.focus_grid_edit = true;
        cx.notify();
    }

    /// Commit the open inline editor: coerce the typed text to the column's type and
    /// stage it (no DB round-trip). A coercion failure toasts the reason and keeps
    /// the editor open to fix.
    pub(crate) fn commit_grid_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.grid_edit.take() else {
            return;
        };
        let text = edit.input.read(cx).content().to_string();
        let value = match coerce_edit_value(&text, edit.decl_type.as_deref()) {
            Ok(v) => v,
            Err(reason) => {
                self.notify(ToastVariant::Error, reason, cx);
                self.grid_edit = Some(edit); // keep it open to correct the value
                return;
            }
        };
        match edit.slot {
            EditSlot::Row {
                row,
                data_col,
                pk_value,
                original,
                foreign,
            } => self.stage_existing_value(
                edit.epoch, row, data_col, pk_value, original, value, foreign,
            ),
            EditSlot::Draft { index, data_col } => {
                self.stage_draft_value(edit.epoch, index, data_col, value)
            }
        }
        // Hand focus back to the grid so the cell cursor (arrows, next edit) is live.
        self.pending_focus = Some(Pane::Grid);
        cx.notify();
    }

    /// Abandon the open inline editor without staging.
    pub(crate) fn cancel_grid_edit(&mut self, cx: &mut Context<Self>) {
        if self.grid_edit.take().is_some() {
            self.pending_focus = Some(Pane::Grid);
            cx.notify();
        }
    }

    /// The focus handle of the open inline editor, for the render-time focus drain.
    pub(crate) fn grid_edit_focus(&self, cx: &Context<Self>) -> Option<gpui::FocusHandle> {
        Some(self.grid_edit.as_ref()?.input.focus_handle(cx))
    }

    // --- staging ---

    /// Stage a new value for an existing keyed cell. A value equal to the resident
    /// original clears any prior staged edit (un-dirties the cell) rather than
    /// staging a no-op; otherwise it's recorded under the row's PK.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn stage_existing_value(
        &mut self,
        epoch: u64,
        row: usize,
        data_col: usize,
        pk_value: Value,
        original: Value,
        value: Value,
        foreign: Option<ForeignEdit>,
    ) {
        let Some(pk) = PkKey::from_value(&pk_value) else {
            return;
        };
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                if grid.epoch != epoch {
                    return; // the result was replaced under the in-flight edit
                }
                if value == original {
                    if let Some(u) = grid.pending.updates.get_mut(&pk) {
                        u.cells.remove(&data_col);
                        if u.cells.is_empty() {
                            grid.pending.updates.remove(&pk);
                        }
                    }
                } else {
                    let entry = grid
                        .pending
                        .updates
                        .entry(pk)
                        .or_insert_with(|| UpdatedRow {
                            pk_value,
                            row,
                            cells: HashMap::new(),
                        });
                    entry.row = row;
                    entry.cells.insert(data_col, StagedCell { value, foreign });
                }
            }
        }
    }

    /// Stage a value into a draft (insert) row's cell.
    fn stage_draft_value(&mut self, epoch: u64, index: usize, data_col: usize, value: Value) {
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                if grid.epoch == epoch {
                    if let Some(draft) = grid.pending.inserts.get_mut(index) {
                        draft.cells.insert(data_col, value);
                    }
                }
            }
        }
    }

    /// Set the focused cell to NULL and stage it (⌘⌥0 / context menu). No-op when
    /// the cell isn't editable.
    pub(crate) fn set_cell_null(&mut self, cx: &mut Context<Self>) {
        let Some(ctx) = self.active_edit_target() else {
            return;
        };
        self.stage_existing_value(
            ctx.epoch,
            ctx.row,
            ctx.data_col,
            ctx.pk_value,
            ctx.original,
            Value::Null,
            ctx.foreign,
        );
        cx.notify();
    }

    // --- row add / delete ---

    /// Add a fresh empty draft row to the insert zone (⌘⌥N / footer / palette).
    /// No-op when editing isn't enabled or the result isn't an editable browse.
    pub(crate) fn add_draft_row(&mut self, cx: &mut Context<Self>) {
        if !self.editing_enabled() {
            return;
        }
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                // Only a keyed single-table browse can be inserted into.
                if grid.editable_browse() {
                    grid.pending.inserts.push(DraftRow::default());
                }
            }
        }
        cx.notify();
    }

    /// Drop a draft row (its ✕). Cancels an open editor first so a shifting index
    /// can't leave the editor pointing at the wrong draft.
    pub(crate) fn remove_draft_row(&mut self, index: usize, cx: &mut Context<Self>) {
        self.grid_edit = None;
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                if index < grid.pending.inserts.len() {
                    grid.pending.inserts.remove(index);
                }
            }
        }
        cx.notify();
    }

    /// Toggle deletion of the selected rows (⌘⌫ / context menu): each editable row
    /// in the selection flips between marked-for-deletion and not. No-op when
    /// editing isn't enabled or no usable PK is resident for a row.
    pub(crate) fn toggle_delete_rows(&mut self, cx: &mut Context<Self>) {
        if !self.editing_enabled() {
            return;
        }
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                let Some(pk_idx) = grid.pk_column_index() else {
                    return;
                };
                let Some(sel) = grid.selection else { return };
                let (r0, _, r1, _) = sel.bounds();
                for row in r0..=r1 {
                    let Some(pk_value) = grid.cell_value(row, pk_idx) else {
                        continue;
                    };
                    let Some(pk) = PkKey::from_value(&pk_value) else {
                        continue;
                    };
                    if grid.pending.deletes.remove(&pk).is_none() {
                        grid.pending
                            .deletes
                            .insert(pk, DeletedRow { pk_value, row });
                    }
                }
            }
        }
        cx.notify();
    }

    // --- submit / revert ---

    /// Submit the staged change-set: build the batch, then open the count + combined
    /// preview confirm (the destructive-statement guard, kept by design). No-op with
    /// nothing staged; the caller (⌘↵ in the grid) falls back to running the query.
    pub(crate) fn submit_changes(&mut self, cx: &mut Context<Self>) {
        // Flush a half-typed cell first so it isn't silently dropped.
        if self.grid_edit.is_some() {
            self.commit_grid_edit(cx);
        }
        let staged = match &self.phase {
            Phase::Connected(active) => active
                .active_result()
                .map(|g| (g.epoch, g.build_edit_ops())),
            _ => None,
        };
        let Some((epoch, ops)) = staged else { return };
        if ops.is_empty() {
            return;
        }
        self.confirm_exec = Some(PendingWrite::Batch { ops, epoch });
        self.focus_modal = true;
        cx.notify();
    }

    /// Whether the active result has staged changes (for ⌘↵'s submit-vs-run choice
    /// and the footer controls).
    pub(crate) fn has_pending_changes(&self) -> bool {
        match &self.phase {
            Phase::Connected(active) => active
                .active_result()
                .is_some_and(|g| !g.pending.is_empty()),
            _ => false,
        }
    }

    /// Drop the whole staged change-set (Revert).
    pub(crate) fn revert_changes(&mut self, cx: &mut Context<Self>) {
        self.grid_edit = None;
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                grid.pending = PendingChanges::default();
            }
        }
        cx.notify();
    }

    /// A submitted batch committed (`BatchApplied`): clear the staged set and reflect
    /// the result. Updates-only batches patch the resident buffer in place (rows
    /// didn't move); a batch that deleted or inserted rows reloads the result so
    /// row positions, totals, and server-assigned values re-resolve.
    pub(crate) fn on_batch_applied(&mut self, epoch: u64, _applied: u64, cx: &mut Context<Self>) {
        let mut reload = false;
        if let Some(grid) = self.result_by_epoch(epoch) {
            // A foreign (inline-expanded FK) edit rewrites a referenced row that may
            // be shared by several base rows, so an in-place patch would leave the
            // other rows stale; reload so the whole denormalized view re-resolves,
            // same as a structural (delete/insert) change.
            let foreign = grid
                .pending
                .updates
                .values()
                .any(|u| u.cells.values().any(|c| c.foreign.is_some()));
            let structural = !grid.pending.deletes.is_empty() || !grid.pending.inserts.is_empty();
            if structural || foreign {
                reload = true;
            } else {
                let updates = std::mem::take(&mut grid.pending.updates);
                for u in updates.into_values() {
                    for (col, cell) in u.cells {
                        grid.patch_cell(u.row, col, cell.value);
                    }
                }
            }
            grid.pending = PendingChanges::default();
        }
        if reload {
            self.reload_active_result(cx);
        }
        self.notify(ToastVariant::Success, "Changes submitted", cx);
        cx.notify();
    }

    /// A submitted batch failed and rolled back (`BatchFailed`): keep the staged set
    /// (nothing was applied) and surface the engine/assertion message.
    pub(crate) fn on_batch_failed(&mut self, _epoch: u64, message: String, cx: &mut Context<Self>) {
        self.notify(ToastVariant::Error, message, cx);
        cx.notify();
    }

    /// Re-open the active result with its current sort + filter under a fresh epoch;
    /// used after a structural submit (deletes/inserts) or a foreign FK-column edit
    /// so the result re-resolves. Reuses [`ResultGrid::reopen_spec`] so the inline FK
    /// expansion (the `LEFT JOIN` set) is carried through the reload rather than lost.
    fn reload_active_result(&mut self, cx: &mut Context<Self>) {
        let reopen = match &mut self.phase {
            Phase::Connected(active) => active.active_result_mut().map(|grid| grid.reopen_spec()),
            _ => None,
        };
        self.apply_reopen(reopen, cx);
    }
}
