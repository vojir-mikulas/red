//! The cell detail inspector: a focused, right-docked pane that shows the
//! *whole* value under the grid cursor (Track B1). The grid intentionally
//! truncates: fat cells arrive as [`Value::Capped`] and even resident whole cells
//! are bounded by the driver's display cap. The inspector is where you see a long
//! `TEXT` wrapped, a JSON document pretty-printed, or a `BLOB` as a hex dump.
//!
//! It is **non-modal**: the grid keeps focus, so arrowing through cells updates
//! the pane live. A small resident cell renders straight from the buffer; **no**
//! backend work. Only a *capped* or *evicted* cell needs its full value, and that
//! is fetched **on demand** behind an explicit "Load full value", reusing the
//! existing full-fidelity `CopyRows` path (`PageCap::Full`); the bytes live only
//! while the pane is open and are dropped when the cursor moves or it closes.
//!
//! The value→view formatting (JSON pretty-print, hex dump) is RED-domain and pure;
//! the generic scrollable viewer is a candidate to push down into Flint later.

use flint::prelude::*;
use gpui::{
    div, prelude::*, px, AnyElement, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    ScrollHandle, SharedString,
};
use red_core::{CappedCell, Value};

use crate::decode;
use red_service::Command;

use crate::app::{ActiveConn, AppState, EditContext, Phase};
use crate::result::group_digits;

/// Bytes of a blob rendered as hex before the dump is cut with a "more bytes"
/// note; the *display* stays bounded even when a "Load full value" pulled a big
/// blob, so the pane never pays to lay out megabytes of hex.
const HEX_MAX: usize = 4 * 1024;

/// Bytes per hex row. Eight (not the conventional sixteen) keeps a row narrow
/// enough to fit the pane without wrapping, which would shear the columns apart.
const HEX_COLS: usize = 8;

/// How the inspector renders the focused value's body. `Auto` picks per value
/// (JSON pretty-printed, blob hex-dumped, text as prose); the others force a lens so
/// the user can read the raw source, force JSON re-indent, or view any value's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ValueFormat {
    #[default]
    Auto,
    /// The stored text verbatim (no JSON re-indent); a blob still shows as hex.
    Raw,
    /// Force JSON re-indent (falls back to raw text when it isn't JSON).
    Json,
    /// A hex dump of the value's bytes (text or blob).
    Hex,
    /// Decode the bytes as MessagePack (falls back to raw/hex when they aren't).
    MsgPack,
    /// Decode the bytes as schemaless Protocol Buffers.
    Protobuf,
    /// Decode the bytes as a Python pickle.
    Pickle,
}

/// All the inspector's persistent state. Present iff the pane is open.
pub(crate) struct InspectorState {
    /// Scroll position of the value body, kept across frames (the body is
    /// otherwise stateless, like the grid's own scroll handles).
    pub(crate) scroll: ScrollHandle,
    /// The full value fetched for one capped/evicted cell, formatted once on
    /// arrival so a big value isn't re-formatted (or cloned) every frame. Cleared
    /// when the target cell changes (see [`AppState::reconcile_inspector`]).
    full: Option<InspectedFull>,
    /// The in-flight full-fetch, if any; its reply is matched by `id`.
    pending: Option<PendingInspect>,
    /// An open inline edit (Track B5): the value field + the cell it targets. The
    /// inspector becomes the editor: type a new value and Save. Cleared when the
    /// target cell moves off, on Save (the confirm takes over), or on Cancel.
    editing: Option<InspectorEdit>,
    /// A read-only editor mirroring the displayed value body, so the pane's body is
    /// *selectable* (drag / double-click word / ⌘C a portion), not just whole-cell
    /// Copy. Rebuilt only when [`PreviewKey`] changes (see
    /// [`AppState::reconcile_preview`]); absent while an edit is open.
    preview: Option<PreviewView>,
    /// When set, the pane is *pinned* to this `(epoch, row, col)`: it keeps showing
    /// (and holding the loaded bytes / open edit of) that cell even as the grid cursor
    /// moves, instead of following the cursor live. Cleared on unpin or a result swap.
    pinned: Option<(u64, usize, usize)>,
    /// The lens the body is rendered through (Auto / Raw / JSON / Hex).
    format: ValueFormat,
}

/// A read-only [`CodeEditor`] hosting the *displayed* value body so the user can
/// select and copy part of it. Reused across frames while the same value is shown
/// (so its selection + scroll survive), and rebuilt when [`key`](Self::key)
/// changes. The event `Subscription` is held here so it drops with the editor.
struct PreviewView {
    editor: Entity<CodeEditor>,
    key: PreviewKey,
    #[allow(dead_code)]
    sub: gpui::Subscription,
}

/// Identifies which value a [`PreviewView`] shows, so a stale one (the cursor
/// moved, or a capped cell's full value just loaded) is recognised and rebuilt.
#[derive(Clone, PartialEq)]
struct PreviewKey {
    epoch: u64,
    row: usize,
    col: usize,
    /// The body's byte length; distinguishes a capped head from the loaded full
    /// value (and a just-patched cell) at the same `(epoch, row, col)`.
    len: usize,
    wrap: bool,
    /// The lens; a format toggle rebuilds the preview even at the same length.
    format: ValueFormat,
}

/// An in-progress inline cell edit hosted in the inspector (Track B5). The editor
/// is a multiline [`CodeEditor`] (not a single-line field), so the value is edited
/// *as the pane shows it*: a pretty-printed JSON document stays formatted and a long
/// text keeps its line breaks. It's seeded with `prefill` (the displayed body), and
/// a Save whose content still equals `prefill` is treated as a no-op (so merely
/// opening the editor on a pretty-printed value and saving never restyles the cell).
/// The editor's event `Subscription` is held here, not detached, so clearing
/// `editing` (on move-off / Save / Cancel) drops it rather than orphaning it.
struct InspectorEdit {
    editor: Entity<CodeEditor>,
    ctx: EditContext,
    prefill: String,
    #[allow(dead_code)]
    sub: gpui::Subscription,
}

impl InspectorState {
    fn new() -> Self {
        Self {
            scroll: ScrollHandle::new(),
            full: None,
            pending: None,
            editing: None,
            preview: None,
            pinned: None,
            format: ValueFormat::Auto,
        }
    }
}

/// A loaded full value, addressed by the cell it belongs to so a stale one (the
/// cursor moved) is recognised and dropped. Holds the *formatted* body, not the
/// raw [`Value`], so rendering never re-formats or clones the (possibly large) value.
struct InspectedFull {
    epoch: u64,
    row: usize,
    col: usize,
    /// The full value itself, kept so a capped/large cell can be *edited*: its
    /// resident grid cell is display-clipped, so the edit must source the original
    /// from here. `view` is the formatted (cached) rendering of this same value.
    value: Value,
    view: ValueView,
    /// The lens `view` was built with; a format change re-renders it (see
    /// [`AppState::reconcile_inspector`]) so the cache stays correct without
    /// re-formatting a large value every frame.
    format: ValueFormat,
}

/// A `CopyRows` re-fetch issued for the inspector, awaiting its `CopyRowsLoaded`.
struct PendingInspect {
    id: u64,
    epoch: u64,
    row: usize,
    col: usize,
}

/// A value formatted for the pane: the body text, a one-line type/size summary,
/// and whether the body should soft-wrap (prose) or stay on fixed lines (hex/JSON).
#[derive(Clone)]
struct ValueView {
    body: SharedString,
    summary: String,
    wrap: bool,
}

/// What the focused cell resolves to right now.
enum CellState {
    /// A value ready to show: a small resident cell, or a loaded full value.
    Ready(ValueView),
    /// Resident but display-capped: only a head/length is known until loaded.
    Capped(CappedCell),
    /// Scrolled out of the resident window; load it to see anything.
    Evicted,
}

/// The fully-resolved inspector target for one frame.
struct InspectorView {
    col_name: String,
    decl_type: Option<String>,
    row: usize,
    state: CellState,
}

impl AppState {
    /// ⌘I / the toolbar button: open the detail inspector, or close it if open.
    pub(crate) fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        self.inspector = match self.inspector {
            Some(_) => None,
            None => Some(InspectorState::new()),
        };
        cx.notify();
    }

    /// Pin the inspector to the cell it's currently showing (so it holds that value —
    /// and any loaded bytes / open edit — while the grid cursor roams), or unpin it
    /// back to following the cursor. No-op when the pane is closed.
    pub(crate) fn toggle_inspector_pin(&mut self, cx: &mut Context<Self>) {
        let target = self.target_cell();
        if let Some(insp) = &mut self.inspector {
            insp.pinned = match insp.pinned {
                Some(_) => None,
                None => target,
            };
            cx.notify();
        }
    }

    /// Switch the render lens (Auto / Raw / JSON / Hex). The cached full value, if any,
    /// re-renders on the next reconcile; resident values re-format on the spot.
    pub(crate) fn set_inspector_format(&mut self, fmt: ValueFormat, cx: &mut Context<Self>) {
        if let Some(insp) = &mut self.inspector {
            insp.format = fmt;
            cx.notify();
        }
    }

    /// Open the inspector if it isn't already (the double-click-a-cell entry;
    /// double-click should reveal, never toggle-shut).
    pub(crate) fn open_inspector(&mut self, cx: &mut Context<Self>) {
        if self.inspector.is_none() {
            self.inspector = Some(InspectorState::new());
            cx.notify();
        }
    }

    /// Esc dismisses the topmost transient overlay: an open toolbar dropdown
    /// ("Export" / "More") or the cell context menu first, and only when none are
    /// open does it fall back to closing the detail inspector. So a menu yields to
    /// Esc before the inspector does, and Esc still does nothing from a bare grid.
    pub(crate) fn dismiss_overlay(&mut self, cx: &mut Context<Self>) {
        let mut closed = false;
        closed |= self.cell_menu.take().is_some();
        closed |= self.export_menu.take().is_some();
        closed |= self.more_menu.take().is_some();
        if closed {
            cx.notify();
            return;
        }
        self.close_inspector(cx);
    }

    /// Esc: close the inspector if open (a no-op otherwise, so Esc keeps doing
    /// nothing from the grid when the pane is closed).
    pub(crate) fn close_inspector(&mut self, cx: &mut Context<Self>) {
        if self.inspector.is_some() {
            self.inspector = None;
            cx.notify();
        }
    }

    /// The `(epoch, row, data-col)` of the cell under the grid cursor, mapping the
    /// selection focus through the gutter and clamping to the data columns. `None`
    /// when nothing is selected or no result is open.
    fn focused_cell(&self) -> Option<(u64, usize, usize)> {
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let grid = active.active_result()?;
        let (row, col) = grid.cursor_cell(self.gutter())?;
        Some((grid.epoch, row, col))
    }

    /// The cell the inspector is showing: the *pinned* cell when the pane is pinned,
    /// otherwise the one under the grid cursor. Everything the pane resolves (its
    /// value, preview, load, edit gate) goes through this, so a pin holds the view
    /// steady while the cursor roams the grid.
    fn target_cell(&self) -> Option<(u64, usize, usize)> {
        match self.inspector.as_ref().and_then(|i| i.pinned) {
            Some(pinned) => Some(pinned),
            None => self.focused_cell(),
        }
    }

    /// The active render lens (Auto when the pane is closed).
    fn inspector_format(&self) -> ValueFormat {
        self.inspector
            .as_ref()
            .map(|i| i.format)
            .unwrap_or_default()
    }

    /// Drop a loaded/in-flight full value once the cursor has moved off the cell it
    /// belonged to (or the result was replaced). Called once per frame before the
    /// shell renders, so the bytes of a big inspected value never outlive the
    /// cursor sitting on it. Keeps the "fetched bytes dropped when focus moves"
    /// budget promise without threading inspector state through every grid handler.
    pub(crate) fn reconcile_inspector(&mut self, cx: &mut Context<Self>) {
        // A pin to a since-replaced result (a re-run/sort/filter bumped the epoch)
        // can't be honored, so drop it and fall back to following the cursor.
        let live_epoch = self.focused_cell().map(|(e, _, _)| e);
        if let Some(insp) = &mut self.inspector {
            if let Some((pe, _, _)) = insp.pinned {
                if live_epoch != Some(pe) {
                    insp.pinned = None;
                }
            }
        }
        // Resolve against the *target* (pinned cell, else cursor): a pinned pane keeps
        // its loaded bytes / open edit even as the cursor roams.
        let cur = self.target_cell();
        let fmt = self.inspector_format();
        if let Some(insp) = &mut self.inspector {
            let matches = |epoch, row, col| cur == Some((epoch, row, col));
            if let Some(full) = &mut insp.full {
                if !matches(full.epoch, full.row, full.col) {
                    insp.full = None;
                } else if full.format != fmt {
                    // Lens changed: re-render the cached view (never per frame).
                    full.view = format_value(&full.value, fmt);
                    full.format = fmt;
                }
            }
            if let Some(p) = &insp.pending {
                if !matches(p.epoch, p.row, p.col) {
                    insp.pending = None;
                }
            }
            // An inline edit belongs to one cell; abandon it if the target moved off
            // (or the result was replaced) so a stray edit can't apply to a new cell.
            if let Some(edit) = &insp.editing {
                if !matches(edit.ctx.epoch, edit.ctx.row, edit.ctx.data_col) {
                    insp.editing = None;
                }
            }
        }
        // Keep the selectable read-only preview in step with the displayed value.
        self.reconcile_preview(cx);
    }

    /// Build/refresh/drop the read-only preview editor that makes the value body
    /// selectable. Done here (in a `&mut self` per-frame pass) because `render` is
    /// `&self` and can't create entities. Keyed by [`PreviewKey`] so the same value
    /// keeps its editor (and thus its in-progress selection and scroll) across
    /// frames; a cursor move or a just-loaded full value rebuilds it. While an edit
    /// is open the editor owns the body, so there's no preview.
    fn reconcile_preview(&mut self, cx: &mut Context<Self>) {
        if self.inspector.is_none() {
            return; // nothing to mirror while the pane is closed
        }
        let desired = self.preview_target();
        let editing = self.inspector.as_ref().is_some_and(|i| i.editing.is_some());
        let Some(insp) = &mut self.inspector else {
            return;
        };
        match desired {
            None => insp.preview = None,
            Some(_) if editing => insp.preview = None,
            Some((key, body, wrap)) => {
                if insp.preview.as_ref().map(|p| &p.key) == Some(&key) {
                    return; // unchanged; keep the editor and its selection
                }
                let editor = cx.new(|cx| {
                    let mut e = CodeEditor::new(cx)
                        .gutter(false)
                        .resting_border(false)
                        .corner_radius(px(0.))
                        .soft_wrap(wrap)
                        .a11y_label("Cell value")
                        .with_content(body);
                    e.set_read_only(true, cx);
                    e
                });
                // Esc from the focused preview closes the pane, matching Esc from the
                // grid (the editor's own Escape action swallows the key otherwise).
                let sub = cx.subscribe(&editor, |this, _, event: &CodeEditorEvent, cx| {
                    if matches!(event, CodeEditorEvent::Escape) {
                        this.close_inspector(cx);
                    }
                });
                insp.preview = Some(PreviewView { editor, key, sub });
            }
        }
    }

    /// The body to show in the selectable preview: a loaded full value, or a whole
    /// resident value. `None` for a capped (not-yet-loaded), evicted, or absent
    /// cell; those render their own non-selectable stand-in with a Load button.
    fn preview_target(&self) -> Option<(PreviewKey, String, bool)> {
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let grid = active.active_result()?;
        let (epoch, row, col) = self.target_cell()?;
        if epoch != grid.epoch {
            return None; // a pin to a since-replaced result (cleared next reconcile)
        }
        let fmt = self.inspector_format();
        // A loaded full value wins; it was formatted once at load / on a lens change.
        if let Some(full) = self.inspector.as_ref().and_then(|i| i.full.as_ref()) {
            if full.epoch == epoch && full.row == row && full.col == col {
                let body = full.view.body.to_string();
                let key = PreviewKey {
                    epoch,
                    row,
                    col,
                    len: body.len(),
                    wrap: full.view.wrap,
                    format: fmt,
                };
                return Some((key, body, full.view.wrap));
            }
        }
        // Otherwise the resident value, when it's whole. A capped cell only has its
        // head, so it isn't selectable here (load it first).
        match grid.cell_value(row, col)? {
            Value::Capped(_) => None,
            v => {
                let view = format_value(&v, fmt);
                let body = view.body.to_string();
                let key = PreviewKey {
                    epoch,
                    row,
                    col,
                    len: body.len(),
                    wrap: view.wrap,
                    format: fmt,
                };
                Some((key, body, view.wrap))
            }
        }
    }

    /// "Load full value": re-fetch the focused cell's row in full (reusing the
    /// clipboard's `CopyRows` path, `PageCap::Full`) so a capped or evicted cell
    /// can show its whole value. One row, on demand, behind an explicit click.
    pub(crate) fn load_inspector_full(&mut self, cx: &mut Context<Self>) {
        let Some((epoch, row, col)) = self.target_cell() else {
            return;
        };
        if self.inspector.is_none() {
            return;
        }
        let id = self.next_copy_id;
        self.next_copy_id += 1;
        if let Some(insp) = &mut self.inspector {
            insp.pending = Some(PendingInspect {
                id,
                epoch,
                row,
                col,
            });
        }
        // A single row at this ordinal, full fidelity; the driver's display cap
        // doesn't apply to `CopyRows`, so the whole cell comes back.
        self.send_active(Command::CopyRows {
            offset: row,
            limit: 1,
            epoch,
            id,
        });
        cx.notify();
    }

    /// A `CopyRows` reply whose id matches the inspector's in-flight request:
    /// format the one cell we care about and stash it. A stale reply (the cursor
    /// moved, clearing `pending`) finds no match and is dropped. Returns whether it
    /// claimed the reply, so the copy path only runs when it didn't.
    pub(crate) fn on_inspect_rows(&mut self, id: u64, rows: &[Vec<Value>]) -> bool {
        let fmt = self.inspector_format();
        let Some(insp) = &mut self.inspector else {
            return false;
        };
        let Some(p) = insp.pending.take_if(|p| p.id == id) else {
            return false;
        };
        if let Some(value) = rows.first().and_then(|r| r.get(p.col)) {
            insp.full = Some(InspectedFull {
                epoch: p.epoch,
                row: p.row,
                col: p.col,
                view: format_value(value, fmt),
                value: value.clone(),
                format: fmt,
            });
        }
        true
    }

    /// The edit target for the inspected cell, preferring a loaded full value so a
    /// large/capped `TEXT` or JSON cell is editable (its resident cell is clipped,
    /// which [`AppState::active_edit_target`] refuses). Falls back to the resident
    /// target. Clones the full value, so call it only when actually opening an edit;
    /// the per-frame "is it editable?" check uses [`Self::inspector_can_edit`].
    fn inspector_edit_context(&self) -> Option<EditContext> {
        if !self.editing_enabled() {
            return None;
        }
        // Editing resolves through the grid *cursor* (`active_edit_target`), so a pane
        // pinned to a different cell is view-only; move the cursor back to edit it.
        if self.target_cell() != self.focused_cell() {
            return None;
        }
        if let (Some(full), Some((epoch, row, col))) = (
            self.inspector.as_ref().and_then(|i| i.full.as_ref()),
            self.focused_cell(),
        ) {
            if full.epoch == epoch && full.row == row && full.col == col {
                let Phase::Connected(active) = &self.phase else {
                    return None;
                };
                return active
                    .active_result()?
                    .edit_target_full(self.gutter(), full.value.clone());
            }
        }
        self.active_edit_target()
    }

    /// Whether the inspected cell can be edited: the cheap predicate behind the
    /// footer "Edit" button, evaluated every frame. Unlike [`inspector_edit_context`]
    /// it never clones the (possibly large) full value.
    fn inspector_can_edit(&self) -> bool {
        if !self.editing_enabled() {
            return false;
        }
        // A pane pinned away from the cursor is view-only (see `inspector_edit_context`).
        if self.target_cell() != self.focused_cell() {
            return false;
        }
        if let (Some(full), Some((epoch, row, col))) = (
            self.inspector.as_ref().and_then(|i| i.full.as_ref()),
            self.focused_cell(),
        ) {
            if full.epoch == epoch && full.row == row && full.col == col {
                if matches!(full.value, Value::Blob(_)) {
                    return false; // no text round-trip for binary
                }
                return matches!(&self.phase, Phase::Connected(active)
                    if active.active_result().and_then(|g| g.edit_identity(self.gutter())).is_some());
            }
        }
        self.active_edit_target().is_some()
    }

    /// Begin an inline edit of the focused cell in the inspector (Track B5). No-op
    /// when the cell isn't editable (read-only / not edit-enabled connection, not a
    /// single-table keyed browse, the PK column, or a blob cell; but a *large* text
    /// cell loaded in full is now editable; see [`Self::inspector_edit_context`]).
    /// The body turns into a multiline editor
    /// seeded with the value *as it was being shown* (pretty JSON stays pretty), so
    /// editing it is in-place and WYSIWYG; ⌘↵ saves, Esc cancels, Enter adds a line.
    pub(crate) fn begin_inspector_edit(&mut self, cx: &mut Context<Self>) {
        let Some(ctx) = self.inspector_edit_context() else {
            return;
        };
        if self.inspector.is_none() {
            return;
        }
        // Seed the editor with the value *as the pane renders it* (a pretty-printed
        // JSON document, a wrapped text), not the raw one-line `to_string`, so editing
        // doesn't first flatten the formatting the user was just reading. A NULL opens
        // empty (an empty save sets NULL back). The Hex lens has no text round-trip, so
        // it seeds the raw text instead (editing bytes-as-hex would mis-coerce on save).
        let edit_fmt = match self.inspector_format() {
            ValueFormat::Hex => ValueFormat::Raw,
            other => other,
        };
        let view = format_value(&ctx.original, edit_fmt);
        let prefill = match &ctx.original {
            Value::Null => String::new(),
            _ => view.body.to_string(),
        };
        // A multiline surface that inherits the pane's mono typography (set on its
        // container at render time): no line-number gutter and no frame of its own, so
        // it reads as the value body becoming editable in place. Prose soft-wraps;
        // structured content (JSON) scrolls horizontally to keep its shape, mirroring
        // how the read-only body lays each out.
        let seed = prefill.clone();
        let wrap = view.wrap;
        let editor = cx.new(|cx| {
            CodeEditor::new(cx)
                .gutter(false)
                .resting_border(false)
                .corner_radius(px(0.))
                .soft_wrap(wrap)
                .a11y_label("Cell value editor")
                .with_content(seed)
        });
        // ⌘↵ (Run) saves and Esc cancels; Enter inserts a newline, so multi-line JSON
        // is editable. The footer carries the same Save / Cancel as buttons.
        let sub = cx.subscribe(
            &editor,
            |this, _, event: &CodeEditorEvent, cx| match event {
                CodeEditorEvent::Run => this.save_inspector_edit(cx),
                CodeEditorEvent::Escape => this.cancel_inspector_edit(cx),
                // No gutter markers on the cell editor, so this never fires.
                CodeEditorEvent::Submit | CodeEditorEvent::RunLine(_) => {}
            },
        );
        if let Some(insp) = &mut self.inspector {
            insp.editing = Some(InspectorEdit {
                editor,
                ctx,
                prefill,
                sub,
            });
        }
        self.focus_inspector_edit = true;
        cx.notify();
    }

    /// Abandon an open inline edit without writing.
    pub(crate) fn cancel_inspector_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(insp) = &mut self.inspector {
            insp.editing = None;
        }
        cx.notify();
    }

    /// Save the inline edit: coerce the typed value to the column's type and stage
    /// it into the result's change-set (Track B6), the same set the in-grid editor
    /// feeds. A coercion failure toasts the reason and keeps the field open to fix.
    pub(crate) fn save_inspector_edit(&mut self, cx: &mut Context<Self>) {
        let Some(insp) = &self.inspector else { return };
        let Some(edit) = &insp.editing else { return };
        let text = edit.editor.read(cx).content();
        let ctx = edit.ctx.clone();
        // Unchanged from what was seeded, including the case where the seed was a
        // *reformatted* (pretty JSON) rendering of the stored value. Close without
        // staging so opening the editor and saving never rewrites a cell with only
        // cosmetic whitespace.
        if text == edit.prefill {
            if let Some(insp) = &mut self.inspector {
                insp.editing = None;
            }
            cx.notify();
            return;
        }
        let value = match red_core::coerce_edit_value(&text, ctx.decl_type.as_deref()) {
            Ok(v) => v,
            Err(reason) => {
                self.notify(ToastVariant::Error, reason, cx);
                return;
            }
        };
        // Close the inline editor and stage the change (the footer Submit flushes it).
        if let Some(insp) = &mut self.inspector {
            insp.editing = None;
        }
        self.stage_existing_value(
            ctx.epoch,
            ctx.row,
            ctx.data_col,
            ctx.pk_value,
            ctx.original,
            value,
            ctx.foreign,
        );
        cx.notify();
    }

    /// The focus handle of the open inline-edit field, for the render-time focus
    /// drain (see `focus_inspector_edit`).
    pub(crate) fn inspector_edit_focus(&self, cx: &Context<Self>) -> Option<FocusHandle> {
        let edit = self.inspector.as_ref()?.editing.as_ref()?;
        Some(edit.editor.focus_handle(cx))
    }

    /// Resolve the cell under the cursor into something renderable: a loaded full
    /// value, a small resident value formatted on the spot, a capped stand-in, or
    /// an evicted (off-window) cell.
    fn inspector_cell(&self, active: &ActiveConn) -> Option<InspectorView> {
        let grid = active.active_result()?;
        let (epoch, row, col) = self.target_cell()?;
        if epoch != grid.epoch {
            return None; // a pin to a since-replaced result (cleared next reconcile)
        }
        let (col_name, decl_type) = grid.column_meta(col)?;
        let fmt = self.inspector_format();

        // A loaded full value wins (formatted once, re-rendered on a lens change).
        if let Some(full) = self.inspector.as_ref().and_then(|i| i.full.as_ref()) {
            if full.epoch == epoch && full.row == row && full.col == col {
                return Some(InspectorView {
                    col_name,
                    decl_type,
                    row,
                    state: CellState::Ready(full.view.clone()),
                });
            }
        }

        // Otherwise read the resident window. Whole cells (under-cap, or the key
        // column) format straight away; they're bounded, so this is cheap.
        let state = match grid.cell_value(row, col) {
            Some(Value::Capped(c)) => CellState::Capped(c),
            Some(v) => CellState::Ready(format_value(&v, fmt)),
            None => CellState::Evicted,
        };
        Some(InspectorView {
            col_name,
            decl_type,
            row,
            state,
        })
    }

    /// The right-docked inspector pane. Rendered as a sibling of the grid (the grid
    /// narrows; it does not occlude it), so the cursor and its live updates stay
    /// visible. Only called while `self.inspector` is `Some` and a result is ready.
    pub(crate) fn render_inspector(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (bg, border, text, muted, faint) = (
            theme.bg_panel,
            theme.border,
            theme.text,
            theme.text_muted,
            theme.text_faint,
        );
        let ui_family = theme.font_family.clone();
        let mono_family = theme.mono_family.clone();
        let (s11, s12) = (theme.scale(11.), theme.scale(12.));
        let body_size = theme.font_size;

        // Header: column name + type, a pin toggle, a close ✕.
        let pinned = self.inspector.as_ref().is_some_and(|i| i.pinned.is_some());
        let resolved = self.inspector_cell(active);
        let (title, subtitle) = match &resolved {
            Some(v) => {
                let ty = v
                    .decl_type
                    .as_deref()
                    .filter(|t| !t.is_empty())
                    .map(|t| t.to_lowercase());
                let sub = match (&v.state, ty) {
                    (CellState::Ready(view), Some(ty)) => format!("{ty} · {}", view.summary),
                    (CellState::Ready(view), None) => view.summary.clone(),
                    (CellState::Capped(c), ty) => {
                        let kind = if c.blob { "blob" } else { "text" };
                        let ty = ty.unwrap_or_else(|| kind.into());
                        format!("{ty} · {} bytes · capped", group_digits(c.len))
                    }
                    (CellState::Evicted, ty) => ty.unwrap_or_else(|| "—".into()),
                };
                (
                    v.col_name.clone(),
                    format!("row {} · {sub}", group_digits(v.row + 1)),
                )
            }
            None => (
                "Cell inspector".to_string(),
                "Select a cell to inspect".to_string(),
            ),
        };

        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py(px(6.))
            .border_b_1()
            .border_color(border)
            .font_family(ui_family.clone())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(div().text_size(s12).text_color(text).child(title))
                    .child(div().text_size(s11).text_color(faint).child(subtitle)),
            )
            .child(
                // Pin holds the pane on this cell while the cursor roams; active when set.
                Button::new("inspector-pin", if pinned { "Pinned" } else { "Pin" })
                    .variant(if pinned {
                        ButtonVariant::Secondary
                    } else {
                        ButtonVariant::Ghost
                    })
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_inspector_pin(cx))),
            )
            .child(
                Button::new("inspector-close", "✕")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.close_inspector(cx))),
            );

        // While an inline edit is open (Track B5) the body *is* the value editor and
        // the footer offers Save / Cancel; the inspector becomes the editor. The
        // editor inherits the body's mono typography from its container, so the value
        // is edited in the very font/size it was just shown in.
        if let Some(edit) = self.inspector.as_ref().and_then(|i| i.editing.as_ref()) {
            let field = div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .font_family(mono_family.clone())
                        .text_size(body_size)
                        .line_height(body_size * 1.5)
                        .child(edit.editor.clone()),
                )
                .into_any_element();
            let footer = div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_end()
                .gap_1()
                .px_3()
                .py(px(6.))
                .border_t_1()
                .border_color(border)
                .child(
                    Button::new("inspector-edit-cancel", "Cancel")
                        .variant(ButtonVariant::Ghost)
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_inspector_edit(cx))),
                )
                .child(
                    Button::new("inspector-edit-save", "Save")
                        .variant(ButtonVariant::Primary)
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| this.save_inspector_edit(cx))),
                );
            return div()
                .id("inspector")
                .size_full()
                .flex()
                .flex_col()
                .bg(bg)
                .text_color(muted)
                .child(header)
                .child(field)
                .child(footer)
                .into_any_element();
        }

        // Whether the focused cell is editable; drives the footer "Edit" button.
        // (A large/capped text loaded in full is editable too, see
        // `inspector_can_edit`.)
        let editable = self.inspector_can_edit();
        // The lens toggle only makes sense once there's a value to re-render; a capped
        // or evicted cell must be loaded first.
        let is_ready = matches!(&resolved, Some(v) if matches!(v.state, CellState::Ready(_)));

        // Body + actions vary by state.
        let (body, action): (AnyElement, Option<AnyElement>) = match resolved.map(|v| v.state) {
            Some(CellState::Ready(view)) => {
                let copy = view.body.clone();
                (
                    self.inspector_ready_body(&view, mono_family.clone(), body_size),
                    Some(
                        Button::new("inspector-copy", "Copy")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.to_string()));
                            }))
                            .into_any_element(),
                    ),
                )
            }
            Some(CellState::Capped(c)) => {
                let note = if c.blob {
                    format!("{} bytes. Load to view as hex.", group_digits(c.len))
                } else {
                    format!(
                        "Showing the first {} bytes of {}.",
                        group_digits(c.head.len()),
                        group_digits(c.len)
                    )
                };
                let preview = (!c.head.is_empty()).then(|| {
                    div()
                        .id("inspector-body")
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .p_3()
                        .font_family(mono_family.clone())
                        .text_size(body_size)
                        .text_color(text)
                        .child(SharedString::from(c.head))
                });
                (
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex_shrink_0()
                                .px_3()
                                .py_2()
                                .text_size(s11)
                                .text_color(faint)
                                .font_family(ui_family.clone())
                                .child(note),
                        )
                        .children(preview)
                        .into_any_element(),
                    Some(self.load_button(cx)),
                )
            }
            Some(CellState::Evicted) => (
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_3()
                    .text_size(s12)
                    .text_color(faint)
                    .font_family(ui_family.clone())
                    .child("This row scrolled out of view; load it to inspect.")
                    .into_any_element(),
                Some(self.load_button(cx)),
            ),
            None => (
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_3()
                    .text_size(s12)
                    .text_color(faint)
                    .font_family(ui_family.clone())
                    .child("Select a cell to inspect its full value.")
                    .into_any_element(),
                None,
            ),
        };

        // Footer actions: an "Edit" affordance when the cell is editable (B5), then
        // the state's own action (Copy / Load full value).
        let mut actions: Vec<AnyElement> = Vec::new();
        if editable {
            actions.push(
                Button::new("inspector-edit", "Edit")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.begin_inspector_edit(cx)))
                    .into_any_element(),
            );
        }
        actions.extend(action);
        let footer = (!actions.is_empty()).then(|| {
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_end()
                .gap_1()
                .px_3()
                .py(px(6.))
                .border_t_1()
                .border_color(border)
                .children(actions)
        });

        // Fills the trailing pane of the result split; the split's divider is the
        // left separator and the user-draggable resize handle, so no border/width
        // here.
        div()
            .id("inspector")
            .size_full()
            .flex()
            .flex_col()
            .bg(bg)
            .text_color(muted)
            .child(header)
            .children(is_ready.then(|| self.format_bar(cx)))
            .child(body)
            .children(footer)
            .into_any_element()
    }

    /// The lens toggle bar (Auto / Raw / JSON / Hex), shown above a resolved value so
    /// the user can read it as prose, forced JSON, or a hex dump. The active lens reads
    /// as a filled button.
    fn format_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let border = cx.theme().border;
        let cur = self.inspector_format();
        let opt =
            |id: &'static str, label: &'static str, fmt: ValueFormat, cx: &mut Context<Self>| {
                Button::new(id, label)
                    .variant(if cur == fmt {
                        ButtonVariant::Secondary
                    } else {
                        ButtonVariant::Ghost
                    })
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_inspector_format(fmt, cx)))
            };
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .py(px(4.))
            .border_b_1()
            .border_color(border)
            .child(opt("insp-fmt-auto", "Auto", ValueFormat::Auto, cx))
            .child(opt("insp-fmt-raw", "Raw", ValueFormat::Raw, cx))
            .child(opt("insp-fmt-json", "JSON", ValueFormat::Json, cx))
            .child(opt("insp-fmt-hex", "Hex", ValueFormat::Hex, cx))
            .child(opt("insp-fmt-msgpack", "MsgPack", ValueFormat::MsgPack, cx))
            .child(opt(
                "insp-fmt-protobuf",
                "Protobuf",
                ValueFormat::Protobuf,
                cx,
            ))
            .child(opt("insp-fmt-pickle", "Pickle", ValueFormat::Pickle, cx))
            .into_any_element()
    }

    /// The shared "Load full value" button: fetches the focused cell in full.
    fn load_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let pending = self.inspector.as_ref().is_some_and(|i| i.pending.is_some());
        Button::new(
            "inspector-load",
            if pending {
                "Loading…"
            } else {
                "Load full value"
            },
        )
        .variant(ButtonVariant::Primary)
        .size(ButtonSize::Sm)
        .disabled(pending)
        .on_click(cx.listener(|this, _, _, cx| this.load_inspector_full(cx)))
        .into_any_element()
    }

    /// The Ready body: the read-only preview editor when it's built (so the value is
    /// selectable: drag, double-click a word, ⌘C a portion), else the plain
    /// non-selectable body as a fallback (a frame can render before `reconcile_preview`
    /// has built the editor; in practice it runs first, so this is just a safety net).
    /// The editor inherits the body's mono typography from this container, exactly as
    /// the inline-edit field does, so read and edit look identical.
    fn inspector_ready_body(
        &self,
        view: &ValueView,
        mono_family: SharedString,
        size: gpui::Pixels,
    ) -> AnyElement {
        match self.inspector.as_ref().and_then(|i| i.preview.as_ref()) {
            Some(p) => div()
                .flex_1()
                .min_h(px(0.))
                .font_family(mono_family)
                .text_size(size)
                .line_height(size * 1.5)
                .child(p.editor.clone())
                .into_any_element(),
            None => self.inspector_body(view, mono_family, size),
        }
    }

    /// The scrollable value body: prose soft-wraps; hex/JSON stay on fixed lines
    /// (so columns line up) and scroll horizontally if a line overflows.
    fn inspector_body(
        &self,
        view: &ValueView,
        mono_family: SharedString,
        size: gpui::Pixels,
    ) -> AnyElement {
        let theme_scroll = self.inspector.as_ref().map(|i| &i.scroll);
        let mut body = div()
            .id("inspector-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p_3()
            .font_family(mono_family)
            .text_size(size);
        if let Some(scroll) = theme_scroll {
            body = body.track_scroll(scroll);
        }
        if view.wrap {
            body.child(view.body.clone()).into_any_element()
        } else {
            // Fixed-line content (hex, pretty JSON): one non-shrinking row per line
            // inside a horizontally-scrollable column, so a long line scrolls rather
            // than wrapping and shearing the layout. Bounded: this non-virtualized
            // fallback only renders for the frame or two before `reconcile_preview`
            // builds the (scrollable) editor, so a pathological value can't lay out
            // millions of line-divs. The editor path shows the whole value.
            const MAX_LINES: usize = 5_000;
            let total = view.body.lines().count();
            let mut col = body.overflow_x_scroll().flex().flex_col().children(
                view.body.lines().take(MAX_LINES).map(|line| {
                    div()
                        .flex_shrink_0()
                        .child(SharedString::from(line.to_string()))
                }),
            );
            if total > MAX_LINES {
                col = col.child(div().flex_shrink_0().child(SharedString::from(format!(
                    "… {} more lines",
                    group_digits(total - MAX_LINES)
                ))));
            }
            col.into_any_element()
        }
    }
}

/// Format a value for the pane through `fmt`: the body text, a one-line summary, and
/// whether it should soft-wrap. Pure and small: the RED-domain half of the inspector.
/// `fmt` only affects text/blob (a scalar has one rendering); `Auto` is the per-value
/// default (JSON pretty-printed, blob hex, text as prose).
/// The formatted body text for `value` through `fmt`, for callers outside the
/// inspector (the Redis string view) that reuse the same lens without the full
/// `ValueView`. Returns `(body, summary)`.
pub(crate) fn format_value_body(value: &Value, fmt: ValueFormat) -> (String, String) {
    let v = format_value(value, fmt);
    (v.body.to_string(), v.summary)
}

fn format_value(value: &Value, fmt: ValueFormat) -> ValueView {
    match value {
        Value::Null => ValueView {
            body: "NULL".into(),
            summary: "null".into(),
            wrap: true,
        },
        Value::Integer(n) => ValueView {
            body: n.to_string().into(),
            summary: "integer".into(),
            wrap: true,
        },
        Value::Real(x) => ValueView {
            body: x.to_string().into(),
            summary: "real".into(),
            wrap: true,
        },
        Value::Text(s) => {
            let chars = group_digits(s.chars().count());
            let raw = || ValueView {
                body: s.clone().into(),
                summary: format!("{chars} chars · text"),
                wrap: true,
            };
            let json = |pretty: String| ValueView {
                body: pretty.into(),
                summary: format!("{chars} chars · JSON"),
                wrap: false,
            };
            match fmt {
                ValueFormat::Hex => hex_view(s.as_bytes()),
                ValueFormat::Raw => raw(),
                // Force a re-indent; if it isn't JSON, show the raw text.
                ValueFormat::Json => pretty_json(s).map(json).unwrap_or_else(raw),
                // Default: JSON if it parses, else prose.
                ValueFormat::Auto => pretty_json(s).map(json).unwrap_or_else(raw),
                // Binary decoders over the string's bytes; fall back to raw text
                // when the bytes aren't that format.
                ValueFormat::MsgPack => {
                    decoded_or(decode::decode_msgpack(s.as_bytes()), "MessagePack", raw)
                }
                ValueFormat::Protobuf => {
                    decoded_or(decode::decode_protobuf(s.as_bytes()), "protobuf", raw)
                }
                ValueFormat::Pickle => {
                    decoded_or(decode::decode_pickle(s.as_bytes()), "pickle", raw)
                }
            }
        }
        // A blob has no textual rendering; the plain lenses show its bytes as
        // hex, while the binary-decoder lenses try to decode the exact bytes and
        // fall back to hex.
        Value::Blob(b) => {
            let hex = || ValueView {
                body: hex_dump(b, HEX_MAX).into(),
                summary: format!("{} bytes · blob", group_digits(b.len())),
                wrap: false,
            };
            match fmt {
                ValueFormat::MsgPack => decoded_or(decode::decode_msgpack(b), "MessagePack", hex),
                ValueFormat::Protobuf => decoded_or(decode::decode_protobuf(b), "protobuf", hex),
                ValueFormat::Pickle => decoded_or(decode::decode_pickle(b), "pickle", hex),
                _ => hex(),
            }
        }
        // A capped value only reaches here if formatted directly (defensive; the
        // pane normally branches on `CellState::Capped` before formatting).
        Value::Capped(c) if c.blob => ValueView {
            body: format!("<{} bytes>", c.len).into(),
            summary: format!("{} bytes · blob (capped)", group_digits(c.len)),
            wrap: true,
        },
        Value::Capped(c) => ValueView {
            body: format!("{}…", c.head).into(),
            summary: format!("{} bytes · text (capped)", group_digits(c.len)),
            wrap: true,
        },
    }
}

/// Re-indent JSON-looking text for readability. Returns `None` when the input
/// doesn't open like JSON (so the caller shows it as plain text). A tolerant
/// re-formatter, not a validator: it walks the text honoring string literals and
/// escapes, collapses existing whitespace, and re-emits newlines + indentation.
/// ~40 lines beats pulling in a JSON crate (the project ships none).
fn pretty_json(s: &str) -> Option<String> {
    let t = s.trim();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    let mut out = String::with_capacity(t.len() + t.len() / 4);
    let mut depth: usize = 0;
    let mut in_str = false;
    let mut esc = false;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                // Keep an empty container on one line: `{}` / `[]`.
                if matches!(chars.peek(), Some('}') | Some(']')) {
                    out.push(chars.next().unwrap());
                } else {
                    depth += 1;
                    indent(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                indent(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                indent(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {} // collapse; structure drives the layout
            c => out.push(c),
        }
    }
    Some(out)
}

/// Push a newline and `depth` levels of two-space indentation.
fn indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// Wrap a binary decoder's result in a [`ValueView`], or fall back to
/// `fallback` (raw text / hex) when the bytes weren't that format. `label`
/// names the decoder for the one-line summary.
fn decoded_or(
    decoded: Option<String>,
    label: &str,
    fallback: impl FnOnce() -> ValueView,
) -> ValueView {
    match decoded {
        Some(body) => ValueView {
            summary: format!("{} · decoded", label),
            body: body.into(),
            wrap: false,
        },
        None => fallback(),
    }
}

/// A [`ValueView`] wrapping a hex dump of `bytes` (the Hex lens over any value): the
/// dump stays on fixed lines (no wrap) and carries a byte-count summary.
fn hex_view(bytes: &[u8]) -> ValueView {
    ValueView {
        body: hex_dump(bytes, HEX_MAX).into(),
        summary: format!("{} bytes · hex", group_digits(bytes.len())),
        wrap: false,
    }
}

/// A classic `offset  hex  |ascii|` hex dump over at most `max` bytes (the rest is
/// summarized), [`HEX_COLS`] bytes per row so a row fits the pane unwrapped.
fn hex_dump(bytes: &[u8], max: usize) -> String {
    let shown = bytes.len().min(max);
    let mut out = String::with_capacity(shown * 4);
    for (i, chunk) in bytes[..shown].chunks(HEX_COLS).enumerate() {
        out.push_str(&format!("{:08x}  ", i * HEX_COLS));
        for b in chunk {
            out.push_str(&format!("{b:02x} "));
        }
        for _ in chunk.len()..HEX_COLS {
            out.push_str("   ");
        }
        out.push_str(" |");
        for b in chunk {
            let c = if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            };
            out.push(c);
        }
        out.push_str("|\n");
    }
    if bytes.len() > shown {
        out.push_str(&format!(
            "\n… {} more bytes (showing the first {shown})",
            bytes.len() - shown
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_json_indents_objects_and_arrays() {
        let got = pretty_json(r#"{"a":1,"b":[2,3],"c":{}}"#).unwrap();
        assert_eq!(
            got,
            "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ],\n  \"c\": {}\n}"
        );
    }

    #[test]
    fn pretty_json_keeps_commas_and_colons_inside_strings_untouched() {
        // A `:` and `,` and brace inside a string must not trigger layout.
        let got = pretty_json(r#"{"k":"a,b: {x}"}"#).unwrap();
        assert_eq!(got, "{\n  \"k\": \"a,b: {x}\"\n}");
    }

    #[test]
    fn pretty_json_handles_escaped_quote_in_string() {
        let got = pretty_json(r#"{"k":"a\"b"}"#).unwrap();
        assert_eq!(got, "{\n  \"k\": \"a\\\"b\"\n}");
    }

    #[test]
    fn pretty_json_rejects_non_json() {
        assert!(pretty_json("hello world").is_none());
        // Leading whitespace is trimmed; this still opens with a non-brace → None.
        assert!(pretty_json("  not json {nope").is_none());
        assert!(pretty_json("42").is_none());
    }

    #[test]
    fn hex_dump_formats_offset_hex_and_ascii() {
        let dump = hex_dump(b"AB\x00\xff", 64);
        // 4 of 8 columns filled, then 4 columns of blank padding before the ascii.
        assert_eq!(dump, "00000000  41 42 00 ff              |AB..|\n");
    }

    #[test]
    fn hex_dump_caps_and_notes_the_remainder() {
        let bytes = vec![0u8; 20];
        let dump = hex_dump(&bytes, 8);
        assert!(dump.contains("12 more bytes (showing the first 8)"));
        // Only the first 8 bytes (one row) are dumped.
        assert_eq!(
            dump.lines().filter(|l| l.starts_with("00000000")).count(),
            1
        );
    }

    #[test]
    fn format_value_classifies_text_json_and_blob() {
        use ValueFormat::Auto;
        assert!(
            matches!(format_value(&Value::Text(r#"{"a":1}"#.into()), Auto), v if v.summary.contains("JSON") && !v.wrap)
        );
        assert!(
            matches!(format_value(&Value::Text("plain".into()), Auto), v if v.summary.contains("text") && v.wrap)
        );
        assert!(
            matches!(format_value(&Value::Blob(vec![1, 2, 3]), Auto), v if v.summary.contains("blob") && !v.wrap)
        );
        assert!(matches!(format_value(&Value::Null, Auto), v if v.body.as_ref() == "NULL"));
    }

    #[test]
    fn format_lens_forces_raw_json_and_hex() {
        let json = Value::Text(r#"{"a":1}"#.into());
        // Raw shows the source verbatim (no re-indent).
        assert_eq!(
            format_value(&json, ValueFormat::Raw).body.as_ref(),
            r#"{"a":1}"#
        );
        // Json re-indents.
        assert!(format_value(&json, ValueFormat::Json)
            .body
            .contains("\n  \"a\": 1"));
        // Hex dumps the text's bytes and never wraps.
        let hex = format_value(&Value::Text("AB".into()), ValueFormat::Hex);
        assert!(hex.body.starts_with("00000000  41 42"));
        assert!(!hex.wrap && hex.summary.contains("hex"));
        // Json on non-JSON text falls back to raw prose.
        assert_eq!(
            format_value(&Value::Text("plain".into()), ValueFormat::Json)
                .body
                .as_ref(),
            "plain"
        );
    }
}
