//! DataGrip-style staged grid editing.
//!
//! Editing is no longer a per-cell modal round-trip (it used to be a palette prompt per cell).
//! The user edits cells *in place*, the changes accumulate in a per-result
//! [`PendingChanges`] set (marked dirty in the grid), and one **Submit** flushes
//! the whole set to the backend as one batch (`ApplyBatch`); **Revert** drops it.
//!
//! The load-bearing decision: staged edits key by **row identity**, not row index,
//! so they survive the windowed buffer's eviction; a dirty cell is recognised by its
//! row's [`RowKey`] at paint time. The set is bounded by how many edits the user
//! made, never by result size, so it stays inside the perf budget.
//!
//! The identity is a *vector* of `(column, value)` pairs rather than one primary-key
//! value, which is what lets a composite-primary-key table be edited at all, and what
//! lets ClickHouse -- where nothing is unique and a row is addressed by a snapshot of
//! its values -- be edited under the best-effort contract (see
//! [`red_core::BatchMode`]).

use std::collections::{HashMap, HashSet};

use flint::{CellRange, TextInput, TextInputEvent, ToastVariant};
use gpui::{App, Context, Entity, Focusable, Subscription, prelude::*};
use red_core::{BatchMode, ColumnValue, EditMode, EditOp, TableRef, Value, coerce_edit_value};
use red_service::Command;

use super::ResultGrid;
use super::buffer::DisplayCell;
use crate::app::{AppState, ForeignEdit, Pane, PendingWrite, Phase};

/// A hashable snapshot of a row's identity values, so staged edits survive the
/// windowed buffer's eviction (they key by identity, not by row index).
///
/// A *vector*, because a row address is not one column: a composite primary key
/// needs its whole tuple, and on an engine with no unique key at all (ClickHouse)
/// the address is a snapshot of every comparable column. Only the value shapes that
/// can be compared in a `WHERE` are representable; a float/blob/clipped member
/// yields `None` and the cell simply isn't stageable.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct RowKey(Vec<(String, KeyPart)>);

/// One member of a [`RowKey`]. [`Value`] is `PartialEq` but not `Hash` (it carries a
/// float), so the hashable members are spelled out rather than derived.
#[derive(Clone, PartialEq, Eq, Hash)]
enum KeyPart {
    /// A null member; the driver renders it `IS NULL`, not `= NULL`.
    Null,
    Int(i64),
    Text(String),
}

impl RowKey {
    /// The identity for a row's `(column, value)` pairs, or `None` when there are
    /// none or a member can't be compared. The column names are part of the key: the
    /// usable set can differ between rows (a clipped cell here, a whole one there),
    /// and two rows whose *values* happen to line up under different column sets are
    /// not the same row.
    pub(crate) fn from_values(values: &[(String, Value)]) -> Option<RowKey> {
        if values.is_empty() {
            return None;
        }
        values
            .iter()
            .map(|(column, v)| {
                let part = match v {
                    Value::Null => KeyPart::Null,
                    Value::Integer(n) => KeyPart::Int(*n),
                    Value::Text(s) => KeyPart::Text(s.to_string()),
                    Value::Real(_) | Value::Blob(_) | Value::Capped(_) => return None,
                };
                Some((column.clone(), part))
            })
            .collect::<Option<Vec<_>>>()
            .map(RowKey)
    }
}

/// One staged cell change: the new value, plus, for an inline-expanded FK column
///, the referenced-table target the edit writes to. A base-table cell
/// carries `foreign = None` and is written via its row's PK; a joined cell carries
/// the [`ForeignEdit`] resolved when the edit began, so submit needn't re-resolve it
/// against a possibly-evicted buffer row.
pub(crate) struct StagedCell {
    pub(crate) value: Value,
    pub(crate) foreign: Option<ForeignEdit>,
}

/// One staged row update: the columns the user changed (data-column index → staged
/// cell), the row's identity values (to build the base `UPDATE`'s `WHERE`), and the
/// absolute row it sat at when staged. The row stays valid for an updates-only batch
/// (no rows move), so submit can patch the resident buffer in place without a
/// refetch.
pub(crate) struct UpdatedRow {
    /// The `(column, value)` pairs that address the row (see
    /// `ResultGrid::identity_values`).
    pub(crate) key_values: Vec<(String, Value)>,
    pub(crate) row: usize,
    pub(crate) cells: HashMap<usize, StagedCell>,
}

/// One row marked for deletion: its identity values (to build the `DELETE`) and the
/// absolute row it sat at when marked (to paint it struck-through; stays valid
/// until a structural submit reloads the result).
pub(crate) struct DeletedRow {
    pub(crate) key_values: Vec<(String, Value)>,
    pub(crate) row: usize,
}

/// A draft row the user is composing for `INSERT`: per-column staged values.
/// Columns left unset take the engine default; an all-unset draft is skipped at
/// submit (an empty `INSERT` is invalid).
#[derive(Default)]
pub(crate) struct DraftRow {
    pub(crate) cells: HashMap<usize, Value>,
}

/// All staged, not-yet-submitted edits for one result. Lives on the
/// [`ResultGrid`], so it's naturally scoped per result and cleared whenever the
/// result is (re)opened, sorted, or filtered.
#[derive(Default)]
pub(crate) struct PendingChanges {
    /// Row identity → the row's staged column changes.
    pub(crate) updates: HashMap<RowKey, UpdatedRow>,
    /// Row identity → the row marked for deletion.
    pub(crate) deletes: HashMap<RowKey, DeletedRow>,
    /// Locally-authored draft rows, rendered in the grid's bottom zone.
    pub(crate) inserts: Vec<DraftRow>,
}

impl PendingChanges {
    pub(crate) fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.deletes.is_empty() && self.inserts.is_empty()
    }

    /// The staged value for a resident row's `(identity, data_col)`, for the render
    /// overlay. `None` when that cell isn't dirty.
    pub(crate) fn cell_override(&self, key: &RowKey, col: usize) -> Option<&Value> {
        self.updates
            .get(key)
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
    if n == 1 { "" } else { "s" }
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
        /// The `(column, value)` pairs that address the row.
        key_values: Vec<(String, Value)>,
        original: Value,
        /// Set when the cell is an inline-expanded FK column: the referenced-table
        /// write target. `None` for an ordinary base-table cell.
        foreign: Option<ForeignEdit>,
    },
    Draft {
        index: usize,
        data_col: usize,
    },
}

/// An open inline cell editor: the `TextInput` hosted in the focused
/// cell, the slot it targets, and the column metadata used to coerce the typed
/// text. The event subscription is held (not detached) so dropping this closes the
/// editor and unsubscribes.
pub(crate) struct GridEdit {
    pub(crate) input: Entity<TextInput>,
    pub(crate) slot: EditSlot,
    pub(crate) decl_type: Option<String>,
    pub(crate) epoch: red_service::Epoch,
    _sub: Subscription,
}

/// Which staged change an [`EditOp`] came from. Recorded at submit time rather than
/// reconstructed from the op afterwards, so a *partial* batch can un-stage exactly
/// what landed: one staged row can produce several ops (its base update plus an
/// update per inline-expanded FK cell), and only the row whose ops *all* finished has
/// really been written.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum OpSource {
    Update(RowKey),
    Delete(RowKey),
    /// Index into [`PendingChanges::inserts`] when the batch was built.
    Insert(usize),
}

/// A submitted batch: the ops in order, and where each came from.
pub(crate) struct EditBatch {
    pub(crate) ops: Vec<EditOp>,
    /// Same length and order as `ops`.
    pub(crate) sources: Vec<OpSource>,
}

impl ResultGrid {
    /// Build the ordered batch of [`EditOp`]s the staged change-set represents:
    /// updates, then deletes, then draft inserts. Empty (no-column) updates/inserts
    /// are skipped. Returns an empty batch when the result has no usable row identity
    /// (it can't be edited); the caller treats that as nothing to submit.
    pub(in crate::result) fn build_edit_batch(&self) -> EditBatch {
        let empty = || EditBatch {
            ops: Vec::new(),
            sources: Vec::new(),
        };
        let Some((schema, name)) = self.table.clone() else {
            return empty();
        };
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
        let decl_of = |name: &str| {
            self.columns
                .iter()
                .find(|c| c.name == name)
                .and_then(|c| c.decl_type.clone())
        };
        // The identity conjunction: one pair per identity column, in the order the
        // backend reported (sorting key first on ClickHouse, so the engine can prune
        // parts). The declared type rides along for the same reason it does on an
        // assignment: a value the driver decoded to text (a uuid, a timestamp, a
        // Decimal) has to be cast back before it can be compared to its column.
        let key_cvs = |values: &[(String, Value)]| -> Vec<ColumnValue> {
            values
                .iter()
                .map(|(column, value)| ColumnValue {
                    column: column.clone(),
                    value: value.clone(),
                    decl_type: decl_of(column),
                })
                .collect()
        };
        let mut ops = Vec::new();
        let mut sources = Vec::new();

        for (key, u) in &self.pending.updates {
            // Base-table cells fold into one `UPDATE … WHERE <identity>`; each inline-
            // expanded FK cell is its own `UPDATE <ref> … WHERE <fk key>`
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
                    Some(f) => {
                        sources.push(OpSource::Update(key.clone()));
                        ops.push(EditOp::Update {
                            table: f.table.clone(),
                            // A followed FK always names a single-column unique key (the
                            // resolver refuses anything else), so this identity is one pair.
                            keys: vec![ColumnValue {
                                column: f.key_column.clone(),
                                value: f.key_value.clone(),
                                decl_type: f.key_type.clone(),
                            }],
                            set: vec![ColumnValue {
                                column: f.set_column.clone(),
                                value: cell.value.clone(),
                                // The referenced column's type (the joined result column)
                                // rides along so a jsonb/uuid/timestamp value casts back.
                                decl_type: col_meta(*c).and_then(|(_, dt)| dt),
                            }],
                        });
                    }
                }
            }
            if set.is_empty() {
                continue;
            }
            let keys = key_cvs(&u.key_values);
            if keys.is_empty() {
                continue; // no identity: an unqualified UPDATE is never what was meant
            }
            sources.push(OpSource::Update(key.clone()));
            ops.push(EditOp::Update {
                table: tref(),
                keys,
                set,
            });
        }
        for (key, d) in &self.pending.deletes {
            let keys = key_cvs(&d.key_values);
            if keys.is_empty() {
                continue;
            }
            sources.push(OpSource::Delete(key.clone()));
            ops.push(EditOp::Delete {
                table: tref(),
                keys,
            });
        }
        for (index, draft) in self.pending.inserts.iter().enumerate() {
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
            sources.push(OpSource::Insert(index));
            ops.push(EditOp::Insert {
                table: tref(),
                values,
            });
        }
        EditBatch { ops, sources }
    }

    /// Drop the staged changes whose ops **all** finished, leaving the rest staged so
    /// the user can fix and resubmit. `done` holds the batch positions that landed.
    ///
    /// The all-or-nothing rule per source is what keeps a resubmit safe: a row whose
    /// base update landed but whose referenced-table update failed stays staged
    /// whole, and re-running the base update is idempotent, whereas dropping it would
    /// silently lose the half that never got written.
    pub(in crate::result) fn unstage_finished(
        &mut self,
        sources: &[OpSource],
        done: &HashSet<usize>,
    ) {
        let mut drafts: Vec<usize> = Vec::new();
        for source in sources {
            if !source_finished(sources, done, source) {
                continue;
            }
            match source {
                OpSource::Update(key) => {
                    self.pending.updates.remove(key);
                }
                OpSource::Delete(key) => {
                    self.pending.deletes.remove(key);
                }
                OpSource::Insert(index) => drafts.push(*index),
            }
        }
        // Highest index first, so each removal leaves the lower ones addressable.
        drafts.sort_unstable();
        drafts.dedup();
        for index in drafts.into_iter().rev() {
            if index < self.pending.inserts.len() {
                self.pending.inserts.remove(index);
            }
        }
    }

    /// The `(row, data_col, value)` triples of the staged updates whose ops all
    /// landed, so an updates-only batch can patch the resident buffer instead of
    /// refetching. Read *before* [`unstage_finished`](Self::unstage_finished) drops
    /// them.
    pub(in crate::result) fn landed_update_cells(
        &self,
        sources: &[OpSource],
        done: &HashSet<usize>,
    ) -> Vec<(usize, usize, Value)> {
        let mut out = Vec::new();
        for source in sources {
            let OpSource::Update(key) = source else {
                continue;
            };
            if !source_finished(sources, done, source) {
                continue;
            }
            if let Some(u) = self.pending.updates.get(key) {
                out.extend(
                    u.cells
                        .iter()
                        .map(|(col, cell)| (u.row, *col, cell.value.clone())),
                );
            }
        }
        out
    }
}

/// Whether every op a staged change produced finished. A row that produced a base
/// update *and* a referenced-table update has landed only when both did; treating it
/// as done after one would silently drop the half that never got written.
fn source_finished(sources: &[OpSource], done: &HashSet<usize>, source: &OpSource) -> bool {
    sources
        .iter()
        .enumerate()
        .filter(|(_, s)| *s == source)
        .all(|(i, _)| done.contains(&i))
}

impl AppState {
    // --- inline editing ---

    /// Begin editing the focused result cell in place (Enter / F2 / double-click).
    /// No-op when the cell isn't editable (read-only connection, not a single-table
    /// keyed browse, the PK column, or a binary/clipped cell). Prefills with the
    /// cell's *effective* current value (a prior staged edit if there is one) so a
    /// tweak is one keystroke; Enter stages it, Esc abandons.
    pub(crate) fn begin_grid_edit(&mut self, cx: &mut Context<Self>) {
        let Some(ctx) = self.active_edit_target(cx) else {
            return;
        };
        let Some(key) = RowKey::from_values(&ctx.key_values) else {
            return;
        };
        // Effective current value: a staged override wins over the resident original.
        let current = match &self.phase {
            Phase::Connected(active) => active
                .active_result()
                .and_then(|g| {
                    g.read(cx)
                        .pending
                        .cell_override(&key, ctx.data_col)
                        .cloned()
                })
                .unwrap_or_else(|| ctx.original.clone()),
            _ => ctx.original.clone(),
        };
        let slot = EditSlot::Row {
            row: ctx.row,
            data_col: ctx.data_col,
            key_values: ctx.key_values.clone(),
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
            Phase::Connected(active) => match active.active_result().as_ref().map(|g| g.read(cx)) {
                // An engine-computed column takes no value on insert, so it has no
                // editor (see `ResultGrid::insertable_column`).
                Some(g) if index < g.pending.inserts.len() && g.insertable_column(data_col) => {
                    let decl = g.columns.get(data_col).and_then(|c| c.decl_type.clone());
                    let cur = g.pending.inserts[index]
                        .cells
                        .get(&data_col)
                        .cloned()
                        .unwrap_or(Value::Null);
                    // Tab off the last cell walks to the next draft (see
                    // `advance_grid_edit`), which may sit below the zone's fold.
                    g.draft_scroll.scroll_to_item(index);
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
        epoch: red_service::Epoch,
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
            // `emit_tab`: Tab/Shift-Tab surface as events so we advance to the next
            // editable cell (fast spreadsheet-style fill) rather than walking the
            // window's focus ring out of the grid.
            // `emit_nav`: Up/Down surface as events so they move the FK suggestion
            // highlight instead of leaking to the grid's row navigation.
            let mut input = TextInput::new(cx).bare().emit_tab().emit_nav();
            input.set_content(prefill, cx);
            input
        });
        let sub = cx.subscribe(&input, |this, _, event: &TextInputEvent, cx| match event {
            TextInputEvent::Submit => this.commit_grid_edit(cx),
            // Esc closes an open suggestion list first, then cancels the edit.
            TextInputEvent::Cancel => this.suggest_escape_or_cancel(cx),
            TextInputEvent::Tab => this.advance_grid_edit(true, cx),
            TextInputEvent::BackTab => this.advance_grid_edit(false, cx),
            // Drive the FK picker when one is open; otherwise no-op.
            TextInputEvent::Change => this.on_grid_edit_change(cx),
            TextInputEvent::Down => this.suggest_move(1, cx),
            TextInputEvent::Up => this.suggest_move(-1, cx),
        });
        let data_col = match &slot {
            EditSlot::Row { data_col, .. } | EditSlot::Draft { data_col, .. } => *data_col,
        };
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
        // Set up (or clear) the FK suggestion picker for this cell; needs `grid_edit`
        // in place so it can seed the filter from the field's current text.
        self.open_cell_suggest(epoch, data_col, cx);
        cx.notify();
    }

    /// Commit the open inline editor: coerce the typed text to the column's type and
    /// stage it (no DB round-trip). A coercion failure toasts the reason and keeps
    /// the editor open to fix.
    pub(crate) fn commit_grid_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.grid_edit.take() else {
            return;
        };
        // A highlighted FK suggestion wins over the typed text — its id is
        // already a typed `Value`, no coercion needed.
        let value = match self.suggest_selected_value() {
            Some(v) => v,
            None => {
                let text = edit.input.read(cx).content().to_string();
                match coerce_edit_value(&text, edit.decl_type.as_deref()) {
                    Ok(v) => v,
                    Err(reason) => {
                        self.notify(ToastVariant::Error, reason, cx);
                        self.grid_edit = Some(edit); // keep it open to correct the value
                        return;
                    }
                }
            }
        };
        self.cell_suggest = None;
        match edit.slot {
            EditSlot::Row {
                row,
                data_col,
                key_values,
                original,
                foreign,
            } => self.stage_existing_value(
                edit.epoch, row, data_col, key_values, original, value, foreign, cx,
            ),
            EditSlot::Draft { index, data_col } => {
                self.stage_draft_value(edit.epoch, index, data_col, value, cx)
            }
        }
        // Hand focus back to the grid so the cell cursor (arrows, next edit) is live.
        self.pending_focus = Some(Pane::Grid);
        cx.notify();
    }

    /// Abandon the open inline editor without staging.
    pub(crate) fn cancel_grid_edit(&mut self, cx: &mut Context<Self>) {
        if self.grid_edit.take().is_some() {
            self.cell_suggest = None;
            self.pending_focus = Some(Pane::Grid);
            cx.notify();
        }
    }

    /// The focus handle of the open inline editor, for the render-time focus drain.
    pub(crate) fn grid_edit_focus(&self, cx: &Context<Self>) -> Option<gpui::FocusHandle> {
        Some(self.grid_edit.as_ref()?.input.focus_handle(cx))
    }

    /// Tab / Shift-Tab from the open inline editor: commit the current cell, then
    /// open the editor on the next (`forward`) / previous editable cell so a row can
    /// be filled without the mouse. A coercion failure keeps the field open to fix
    /// (mirrors `commit_grid_edit`). Tab past the last cell of the last draft row
    /// starts a fresh draft; Shift-Tab off the first cell just returns to the grid.
    pub(crate) fn advance_grid_edit(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(edit) = self.grid_edit.take() else {
            return;
        };
        // A highlighted FK suggestion wins over the typed text (as in `commit`).
        let value = match self.suggest_selected_value() {
            Some(v) => v,
            None => {
                let text = edit.input.read(cx).content().to_string();
                match coerce_edit_value(&text, edit.decl_type.as_deref()) {
                    Ok(v) => v,
                    Err(reason) => {
                        self.notify(ToastVariant::Error, reason, cx);
                        self.grid_edit = Some(edit); // keep it open to correct the value
                        return;
                    }
                }
            }
        };
        // The next cell's `open_cell_editor` resets the picker; clear it here so an
        // intermediate frame can't show a stale list against the wrong field.
        self.cell_suggest = None;
        match edit.slot {
            EditSlot::Row {
                row,
                data_col,
                key_values,
                original,
                foreign,
            } => {
                self.stage_existing_value(
                    edit.epoch, row, data_col, key_values, original, value, foreign, cx,
                );
                self.advance_row_edit(row, data_col, forward, cx);
            }
            EditSlot::Draft { index, data_col } => {
                self.stage_draft_value(edit.epoch, index, data_col, value, cx);
                self.advance_draft_edit(index, data_col, forward, cx);
            }
        }
    }

    /// Move the grid cursor to the next editable cell after `(row, data_col)` and
    /// open the inline editor there. Steps cell by cell (wrapping across rows),
    /// skipping any cell the edit gate rejects (the PK column, a clipped/binary
    /// value, an unresolvable FK), and falls back to grid focus when none is found.
    fn advance_row_edit(
        &mut self,
        row: usize,
        data_col: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let gutter = self.gutter();
        let row_height = f32::from(self.settings.data.density.row_height());
        let (ncols, locked, total) = match &self.phase {
            Phase::Connected(active) => match active.active_result().as_ref().map(|g| g.read(cx)) {
                Some(g) => (
                    g.columns.len(),
                    (0..g.columns.len())
                        .map(|c| !g.updatable_column(c))
                        .collect::<Vec<_>>(),
                    g.total,
                ),
                None => return self.focus_grid(cx),
            },
            _ => return,
        };
        if ncols == 0 {
            return self.focus_grid(cx);
        }
        let (mut r, mut c) = (row, data_col);
        // Bounded so an all-non-editable stretch can't spin; one row's worth of
        // steps plus a wrap into the neighbouring row is ample.
        for _ in 0..(ncols * 2 + 2) {
            let stepped = if forward {
                if c + 1 < ncols {
                    c += 1;
                    true
                } else if r + 1 < total {
                    c = 0;
                    r += 1;
                    true
                } else {
                    false
                }
            } else if c > 0 {
                c -= 1;
                true
            } else if r > 0 {
                c = ncols - 1;
                r -= 1;
                true
            } else {
                false
            };
            if !stepped {
                break;
            }
            if locked.get(c).copied().unwrap_or(true) {
                continue; // identity / engine-computed column; skip without a probe
            }
            if let Phase::Connected(active) = &mut self.phase {
                active.with_active_result(cx, |grid| {
                    grid.selection = Some(CellRange::single(r, c + gutter));
                    grid.scroll_cursor_into_view(r, row_height);
                    grid.scroll_col_into_view(c + gutter, gutter);
                });
            }
            // `begin_grid_edit` re-resolves the edit target for the moved cursor and
            // no-ops on a non-editable cell; only open when it will actually take.
            if self.active_edit_target(cx).is_some() {
                self.begin_grid_edit(cx);
                return;
            }
        }
        self.focus_grid(cx);
    }

    /// Advance the inline editor across a draft (insert) row's cells. Tab past the
    /// last cell of the last draft appends a fresh draft and lands on its first
    /// cell, so a table can be filled with a continuous type-and-Tab rhythm.
    fn advance_draft_edit(
        &mut self,
        index: usize,
        data_col: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let (ncols, ndrafts, writable) = match &self.phase {
            Phase::Connected(active) => match active.active_result().as_ref().map(|g| g.read(cx)) {
                Some(g) => (
                    g.columns.len(),
                    g.pending.inserts.len(),
                    (0..g.columns.len())
                        .map(|c| g.insertable_column(c))
                        .collect::<Vec<_>>(),
                ),
                None => return self.focus_grid(cx),
            },
            _ => return,
        };
        if ncols == 0 || !writable.iter().any(|w| *w) {
            return self.focus_grid(cx);
        }
        // Tab skips over engine-computed columns, which have no editor to land in.
        let step = |mut c: usize| -> Option<usize> {
            loop {
                c = if forward { c + 1 } else { c.checked_sub(1)? };
                if c >= ncols {
                    return None;
                }
                if writable[c] {
                    return Some(c);
                }
            }
        };
        let first = writable.iter().position(|w| *w).unwrap_or(0);
        let last = writable.iter().rposition(|w| *w).unwrap_or(0);
        match (forward, step(data_col)) {
            (_, Some(next)) => self.begin_draft_edit(index, next, cx),
            (true, None) if index + 1 < ndrafts => self.begin_draft_edit(index + 1, first, cx),
            (true, None) => {
                self.add_draft_row(cx); // the new draft lands at the old length
                self.begin_draft_edit(ndrafts, first, cx);
            }
            (false, None) if index > 0 => self.begin_draft_edit(index - 1, last, cx),
            (false, None) => self.focus_grid(cx),
        }
    }

    /// Hand focus back to the grid (cursor navigation, next edit) with nothing open.
    fn focus_grid(&mut self, cx: &mut Context<Self>) {
        self.pending_focus = Some(Pane::Grid);
        cx.notify();
    }

    // --- staging ---

    /// Stage a new value for an existing keyed cell. A value equal to the resident
    /// original clears any prior staged edit (un-dirties the cell) rather than
    /// staging a no-op; otherwise it's recorded under the row's PK.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn stage_existing_value(
        &mut self,
        epoch: red_service::Epoch,
        row: usize,
        data_col: usize,
        key_values: Vec<(String, Value)>,
        original: Value,
        value: Value,
        foreign: Option<ForeignEdit>,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = RowKey::from_values(&key_values) else {
            return;
        };
        if let Phase::Connected(active) = &mut self.phase {
            active.with_active_result(cx, |grid| {
                if grid.epoch != epoch {
                    return; // the result was replaced under the in-flight edit
                }
                if value == original {
                    if let Some(u) = grid.pending.updates.get_mut(&key) {
                        u.cells.remove(&data_col);
                        if u.cells.is_empty() {
                            grid.pending.updates.remove(&key);
                        }
                    }
                } else {
                    let entry = grid
                        .pending
                        .updates
                        .entry(key)
                        .or_insert_with(|| UpdatedRow {
                            key_values,
                            row,
                            cells: HashMap::new(),
                        });
                    entry.row = row;
                    entry.cells.insert(data_col, StagedCell { value, foreign });
                }
            });
        }
    }

    /// Stage a value into a draft (insert) row's cell. An emptied cell (`Value::Null`,
    /// what `coerce_edit_value` returns for blank text) is *unset* rather than stored,
    /// so the column falls back to the engine default (rendered as a faint "default")
    /// instead of inserting an explicit `NULL` — clearing a draft cell means "leave it
    /// to the default", matching DataGrip's new-row behaviour.
    fn stage_draft_value(
        &mut self,
        epoch: red_service::Epoch,
        index: usize,
        data_col: usize,
        value: Value,
        cx: &mut Context<Self>,
    ) {
        if let Phase::Connected(active) = &mut self.phase {
            active.with_active_result(cx, |grid| {
                if grid.epoch != epoch {
                    return;
                }
                let Some(draft) = grid.pending.inserts.get_mut(index) else {
                    return;
                };
                match value {
                    Value::Null => {
                        draft.cells.remove(&data_col);
                    }
                    v => {
                        draft.cells.insert(data_col, v);
                    }
                }
            });
        }
    }

    /// Set the focused cell to NULL and stage it (⌘⌥0 / context menu). No-op when
    /// the cell isn't editable.
    pub(crate) fn set_cell_null(&mut self, cx: &mut Context<Self>) {
        let Some(ctx) = self.active_edit_target(cx) else {
            return;
        };
        self.stage_existing_value(
            ctx.epoch,
            ctx.row,
            ctx.data_col,
            ctx.key_values,
            ctx.original,
            Value::Null,
            ctx.foreign,
            cx,
        );
        cx.notify();
    }

    // --- row add / delete ---

    /// Add a fresh empty draft row to the insert zone (⌘⌥N / footer / palette).
    /// No-op when inserting isn't enabled or the result isn't a single-table browse.
    pub(crate) fn add_draft_row(&mut self, cx: &mut Context<Self>) {
        if !self.insert_enabled() {
            return;
        }
        if let Phase::Connected(active) = &mut self.phase {
            active.with_active_result(cx, |grid| {
                // A draft row names no existing row, so it needs a target table but no
                // resolved key (unlike an update or a delete).
                if grid.insertable_browse() {
                    grid.pending.inserts.push(DraftRow::default());
                    // Past the zone's visible rows the new draft lands below the fold,
                    // which reads as "+ Row did nothing" — so bring it into view.
                    grid.draft_scroll
                        .scroll_to_item(grid.pending.inserts.len() - 1);
                }
            });
        }
        cx.notify();
    }

    /// Drop a draft row (its ✕). Cancels an open editor first so a shifting index
    /// can't leave the editor pointing at the wrong draft.
    pub(crate) fn remove_draft_row(&mut self, index: usize, cx: &mut Context<Self>) {
        self.grid_edit = None;
        self.cell_suggest = None;
        if let Phase::Connected(active) = &mut self.phase {
            active.with_active_result(cx, |grid| {
                if index < grid.pending.inserts.len() {
                    grid.pending.inserts.remove(index);
                }
            });
        }
        cx.notify();
    }

    /// Toggle deletion of the selected rows (⌘⌫ / context menu): each editable row
    /// in the selection flips between marked-for-deletion and not. No-op when row
    /// editing isn't enabled or no usable PK is resident for a row.
    pub(crate) fn toggle_delete_rows(&mut self, cx: &mut Context<Self>) {
        if !self.row_edit_enabled(cx) {
            return;
        }
        if let Phase::Connected(active) = &mut self.phase {
            // The closure reports whether there was a selection, so the
            // no-selection case still skips the repaint below exactly as the
            // early `return` used to.
            let acted = active.with_active_result(cx, |grid| {
                let Some(sel) = grid.selection else {
                    return false;
                };
                let (r0, _, r1, _) = sel.bounds();
                for row in r0..=r1 {
                    let Some(key_values) = grid.identity_values(row) else {
                        continue;
                    };
                    let Some(key) = RowKey::from_values(&key_values) else {
                        continue;
                    };
                    if grid.pending.deletes.remove(&key).is_none() {
                        grid.pending
                            .deletes
                            .insert(key, DeletedRow { key_values, row });
                    }
                }
                true
            });
            if acted == Some(false) {
                return;
            }
        }
        cx.notify();
    }

    // --- submit / revert ---

    /// Submit the staged change-set: build the batch, then open the count + combined
    /// preview confirm (the destructive-statement guard, kept by design). No-op with
    /// nothing staged; the caller (⌘↵ in the grid) falls back to running the query.
    ///
    /// The confirm is **not** skippable, and deliberately does not consult
    /// `query.confirm_destructive` the way an editor statement does. That setting
    /// trades a prompt for a statement the user typed and can see; a staged batch is
    /// neither, and under the best-effort contract one keystroke can start an
    /// unbounded, unrollbackable rewrite.
    ///
    /// Under the best-effort contract the confirm is preceded by a **preflight** round
    /// trip, so the dialog can show the statement that will really run and how many
    /// rows each op currently matches. The atomic path needs none: the driver's own
    /// one-row assertion and rollback are a stronger guarantee than a count taken a
    /// moment earlier, so it opens the dialog straight away.
    pub(crate) fn submit_changes(&mut self, cx: &mut Context<Self>) {
        // Flush a half-typed cell first so it isn't silently dropped.
        if self.grid_edit.is_some() {
            self.commit_grid_edit(cx);
        }
        let staged = match &self.phase {
            Phase::Connected(active) => active.active_result().map(|g| {
                let g = g.read(cx);
                (g.epoch, g.build_edit_batch())
            }),
            _ => None,
        };
        let Some((epoch, batch)) = staged else { return };
        if batch.ops.is_empty() {
            return;
        }
        match self.batch_mode(cx) {
            BatchMode::Atomic => {
                self.confirm_exec = self.pending_confirm(PendingWrite::Batch {
                    ops: batch.ops,
                    sources: batch.sources,
                    epoch,
                    mode: BatchMode::Atomic,
                    plan: Vec::new(),
                });
                self.focus_modal = true;
            }
            mode => {
                self.send_active(Command::PreflightBatch {
                    epoch,
                    ops: batch.ops.clone(),
                });
                self.pending_batch = Some(PendingWrite::Batch {
                    ops: batch.ops,
                    sources: batch.sources,
                    epoch,
                    mode,
                    plan: Vec::new(),
                });
            }
        }
        cx.notify();
    }

    /// Which contract this connection's edits run under. The engine decides: an
    /// engine with transactions gets the guarded one, ClickHouse the best-effort one.
    /// The "apply to all matching rows" acknowledgement starts off -- the user grants
    /// it in the confirm dialog, if at all.
    pub(crate) fn batch_mode(&self, cx: &App) -> BatchMode {
        match self.row_edit_mode(cx) {
            EditMode::BestEffort => BatchMode::BestEffort {
                allow_multi_match: false,
            },
            _ => BatchMode::Atomic,
        }
    }

    /// The preflight came back: open the confirm dialog it was gathered for. A reply
    /// whose epoch no longer matches the waiting batch is dropped (the result was
    /// re-run or the submit abandoned).
    pub(crate) fn on_batch_preflight(
        &mut self,
        epoch: red_service::Epoch,
        plan: Vec<red_core::OpPlan>,
        cx: &mut Context<Self>,
    ) {
        let Some(PendingWrite::Batch { epoch: waiting, .. }) = &self.pending_batch else {
            return;
        };
        if *waiting != epoch {
            return;
        }
        if let Some(PendingWrite::Batch {
            plan: slot,
            mode,
            ops,
            sources,
            epoch,
        }) = self.pending_batch.take()
        {
            let _ = slot;
            self.confirm_exec = self.pending_confirm(PendingWrite::Batch {
                ops,
                sources,
                epoch,
                mode,
                plan,
            });
            self.focus_modal = true;
        }
        cx.notify();
    }

    /// The preflight couldn't be answered, so nothing is confirmed and nothing is
    /// written: drop the waiting batch (the staged changes stay) and say why.
    pub(crate) fn on_batch_preflight_failed(&mut self, message: String, cx: &mut Context<Self>) {
        self.pending_batch = None;
        self.notify(ToastVariant::Error, message, cx);
        cx.notify();
    }

    /// Whether the active result has staged changes (for ⌘↵'s submit-vs-run choice
    /// and the footer controls).
    pub(crate) fn has_pending_changes(&self, cx: &App) -> bool {
        match &self.phase {
            Phase::Connected(active) => active
                .active_result()
                .is_some_and(|g| !g.read(cx).pending.is_empty()),
            _ => false,
        }
    }

    /// Drop the whole staged change-set (Revert).
    pub(crate) fn revert_changes(&mut self, cx: &mut Context<Self>) {
        self.grid_edit = None;
        self.cell_suggest = None;
        if let Phase::Connected(active) = &mut self.phase {
            active.with_active_result(cx, |grid| {
                grid.pending = PendingChanges::default();
            });
        }
        cx.notify();
    }

    /// A submitted batch committed (`BatchApplied`): clear the staged set and reflect
    /// the result. Updates-only batches patch the resident buffer in place (rows
    /// didn't move); a batch that deleted or inserted rows reloads the result so
    /// row positions, totals, and server-assigned values re-resolve.
    pub(crate) fn on_batch_applied(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: red_service::Epoch,
        _applied: u64,
        cx: &mut Context<Self>,
    ) {
        self.submitted_batch = None;
        let mut reload = false;
        if let Some(grid) = self.result_by_epoch_in(session, epoch, cx) {
            grid.update(cx, |grid, _| {
                // A foreign (inline-expanded FK) edit rewrites a referenced row that may
                // be shared by several base rows, so an in-place patch would leave the
                // other rows stale; reload so the whole denormalized view re-resolves,
                // same as a structural (delete/insert) change.
                let foreign = grid
                    .pending
                    .updates
                    .values()
                    .any(|u| u.cells.values().any(|c| c.foreign.is_some()));
                let structural =
                    !grid.pending.deletes.is_empty() || !grid.pending.inserts.is_empty();
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
            });
        }
        if reload {
            // Reload the tab that *owns this epoch*, not the focused one: a
            // delete staged on tab A and submitted while tab B is focused must
            // reopen A (its rows moved), not reset B's buffer and scroll. Same for
            // the connection — hence `session`.
            self.reload_result_epoch(session, epoch, cx);
        }
        self.notify(ToastVariant::Success, "Changes submitted", cx);
        cx.notify();
    }

    /// A submitted batch failed and rolled back (`BatchFailed`): keep the staged set
    /// (nothing was applied) and surface the engine/assertion message.
    pub(crate) fn on_batch_failed(
        &mut self,
        _session: Option<red_service::SessionId>,
        _epoch: red_service::Epoch,
        message: String,
        cx: &mut Context<Self>,
    ) {
        self.submitted_batch = None;
        self.notify(ToastVariant::Error, message, cx);
        cx.notify();
    }

    /// A **best-effort** batch finished (`BatchPartial`): report per op what happened,
    /// drop the staged changes that landed, and keep the rest.
    ///
    /// There was no transaction, so "3 of 5 applied" is a real outcome and the report
    /// says so. A blanket "Changes submitted" toast here would be the single most
    /// misleading thing this feature could do.
    pub(crate) fn on_batch_partial(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: red_service::Epoch,
        outcomes: Vec<red_core::OpOutcome>,
        cx: &mut Context<Self>,
    ) {
        use red_core::OpStatus;
        let sources = match self.submitted_batch.take() {
            Some((submitted, sources)) if submitted == epoch => sources,
            // A reply for a batch this pane no longer owns: report it, change nothing.
            other => {
                self.submitted_batch = other;
                Vec::new()
            }
        };
        let done: HashSet<usize> = outcomes
            .iter()
            .filter(|o| !o.status.unfinished())
            .map(|o| o.index)
            .collect();

        // A landed delete or insert moves every row after it, and a referenced-table
        // edit is shared by each base row pointing at it, so both need a reload rather
        // than an in-place patch of the resident buffer.
        let structural = outcomes
            .iter()
            .any(|o| !o.status.unfinished() && matches!(o.verb, "Delete" | "Insert"));
        let mut reload = false;
        if let Some(grid) = self.result_by_epoch_in(session, epoch, cx) {
            grid.update(cx, |grid, _| {
                let foreign = grid
                    .pending
                    .updates
                    .values()
                    .any(|u| u.cells.values().any(|c| c.foreign.is_some()));
                reload = structural || foreign;
                if !reload {
                    for (row, col, value) in grid.landed_update_cells(&sources, &done) {
                        grid.patch_cell(row, col, value);
                    }
                }
                grid.unstage_finished(&sources, &done);
            });
        }
        if reload {
            // Reload the epoch's own tab on its own connection, not the focused
            // one (see `on_batch_applied`).
            self.reload_result_epoch(session, epoch, cx);
        }
        // The mutations this submit started are the connection's newest, so refresh
        // the listing whether or not the panel is open: the status-bar indicator
        // reads off it too.
        self.refresh_mutations(cx);

        let count = |f: fn(&OpStatus) -> bool| outcomes.iter().filter(|o| f(&o.status)).count();
        let applied = count(|s| matches!(s, OpStatus::Applied { .. }));
        let running = count(|s| matches!(s, OpStatus::Submitted));
        let mut reasons: Vec<String> = outcomes
            .iter()
            .filter_map(|o| o.status.reason().map(|r| format!("{}: {r}", o.verb)))
            .collect();
        reasons.dedup();

        let mut summary = format!("{applied} of {} changes applied", outcomes.len());
        if running > 0 {
            // Not an error, and explicitly not a retry prompt: the mutation was
            // accepted and is being applied: re-submitting would start a second full
            // part rewrite. The panel is where its progress lives.
            summary.push_str(&format!(
                ", {running} still running (see the Mutations panel)"
            ));
        }
        let variant = if reasons.is_empty() {
            ToastVariant::Success
        } else {
            ToastVariant::Warning
        };
        if !reasons.is_empty() {
            summary.push('\n');
            summary.push_str(&reasons.join("\n"));
        }
        self.notify(variant, summary, cx);
        cx.notify();
    }

    /// Re-open the active result with its current sort + filter under a fresh epoch;
    /// used after a structural submit (deletes/inserts) or a foreign FK-column edit
    /// so the result re-resolves. Reuses [`ResultGrid::reopen_spec`] so the inline FK
    /// expansion (the `LEFT JOIN` set) is carried through the reload rather than lost.
    /// Reopen the result carrying `epoch` wherever it lives — after a structural
    /// submit (delete/insert) or a foreign FK-column edit — with its current sort
    /// and filter, under a fresh epoch. Keyed by epoch rather than focus:
    /// `reopen_spec` rebinds that grid's own epoch, so the reopened rows route
    /// back to it even when another tab is on screen. Reuses
    /// [`ResultGrid::reopen_spec`] so the inline FK `LEFT JOIN` set is carried
    /// through rather than lost.
    fn reload_result_epoch(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: red_service::Epoch,
        cx: &mut Context<Self>,
    ) {
        let reopen = self
            .result_by_epoch_in(session, epoch, cx)
            .map(|grid| grid.update(cx, |grid, _| grid.reopen_spec()));
        self.apply_reopen_in(session, reopen, cx);
    }
}
