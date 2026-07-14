//! The root view and app state machine. `AppState` owns the backend handle, the
//! persisted connection list, and the current `Phase` (disconnected connect
//! screen ↔ connecting ↔ connected shell). Backend events are drained on a
//! foreground `cx.spawn` task into [`AppState::on_event`], the one place where
//! the service drives UI state. Screen rendering lives in `crate::connect` /
//! `crate::shell`.
//!
//! `AppState`'s methods are split across this module's submodules (each an
//! `impl AppState` block over the one struct defined here): [`form`] the
//! connection-form logic, [`render`] the root view + modals, [`connect`] the
//! session/connect lifecycle, [`switcher`] the Cmd-P switcher, and [`settings`]
//! the settings/appearance/update UI. What stays here is the struct, the shared
//! types, and the core state machine (events, notifications, tabs, focus).

mod connect;
mod form;
mod import_ui;
mod keymap_edit;
mod render;
mod settings;
mod switcher;
pub(crate) mod tabs;

use switcher::{switcher_footer, switcher_sections};

use std::collections::HashMap;
use std::time::Duration;

use flint::prelude::*;
use flint::{CodeEditor, CodeEditorEvent};
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use gpui::{
    prelude::*, px, AsyncApp, Context, ElementId, Entity, FocusHandle, Focusable, Hsla,
    PathPromptOptions, Pixels, ScrollHandle, SharedString, WeakEntity, Window, WindowAppearance,
};
use red_core::{
    Column, ColumnMap, ColumnMeta, ConnectionConfig, CopyMode, DbKind, EditOp, FkEdge,
    ImportFormat, TableRef, UpdateState,
};
use red_service::{AiAuthStatus, Command, Event, ServiceHandle, SessionId, UpdateConfig};

use crate::config::{self, StoredConnection};
use crate::palette::{Cmd, PromptKind};
use crate::result::ResultGrid;
use crate::schema::SchemaState;
use crate::settings::{Density, FileSettingsStore, Settings, ThemeMode, ThemeSetting};
use crate::settings_ui::{RevealTarget, SettingsTab};
use crate::theme::ThemeRegistry;

/// Shared slot for the focused settings control's window-space bounds, tagged with
/// which control it belongs to. Written by a canvas overlay during paint, read on
/// the next render to scroll the control into view. See [`AppState::settings_focus_box`].
type RevealBox = std::rc::Rc<std::cell::RefCell<Option<(RevealTarget, gpui::Bounds<Pixels>)>>>;

/// Which font-family picker (UI sans / UI mono / editor) a settings action refers
/// to; routes a choice to the matching setter and the matching combo box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FontSelect {
    Ui,
    UiMono,
    Editor,
}

/// Which of the connected shell's three panes holds keyboard focus. Tracked on
/// [`ActiveConn`] so the focus-cycling actions know where they are, and so the
/// pane chrome can draw a focus ring on the active one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pane {
    Schema,
    Editor,
    Grid,
}

/// Which tabs a tab-strip context-menu close action targets, relative to the
/// clicked tab's own pane. See [`AppState::close_tab_group`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabCloseScope {
    /// Just the clicked tab (the menu's plain "Close" item / the × button).
    One,
    All,
    Others,
    Left,
    Right,
}

/// Which half of a side-by-side split (see [`SplitState`]) the run/export/filter
/// actions target: the focused half. `Primary` is the left pane (`active_tab`);
/// `Secondary` is the right pane (`SplitState::secondary`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitHalf {
    Primary,
    Secondary,
}

impl SplitHalf {
    /// The other half, used by "focus other half" and the strip's swap logic.
    fn other(self) -> SplitHalf {
        match self {
            SplitHalf::Primary => SplitHalf::Secondary,
            SplitHalf::Secondary => SplitHalf::Primary,
        }
    }

    /// A 0/1 discriminant used to salt the two halves' element ids apart.
    pub(crate) fn index(self) -> usize {
        match self {
            SplitHalf::Primary => 0,
            SplitHalf::Secondary => 1,
        }
    }
}

/// The side-by-side split: the work area shows two query tabs at once. `active_tab`
/// is the left (primary) half; `secondary` indexes the right half's tab. `focus`
/// says which half receives run/export/filter (and draws the active outline);
/// `width` drives the resizable divider (left-half width). `None` on [`ActiveConn`]
/// is the ordinary single-pane layout. Always holds `secondary != active_tab` and
/// `secondary < tabs.len()`; a tab close/reorder that breaks either collapses it.
pub(crate) struct SplitState {
    pub secondary: usize,
    pub focus: SplitHalf,
    pub width: Pixels,
    pub drag: Option<DragAnchor>,
}

/// A tab that lives in a split workspace: the two structural facts the
/// pane-routing and split invariants need, independent of what the tab renders.
/// Implemented by both [`QueryTab`] (SQL) and `RedisTab` (Redis).
pub(crate) trait WorkspaceTab {
    fn pane(&self) -> SplitHalf;
    fn set_pane(&mut self, half: SplitHalf);
    fn pinned(&self) -> bool;
}

/// A view hosting a set of tabs in an optional side-by-side split. The
/// pane-routing and split-invariant logic lives here once, as provided methods,
/// for both the SQL query workspace ([`ActiveConn`]) and the Redis workspace
/// (`RedisView`) — which previously carried byte-for-byte copies that could
/// drift. An implementor supplies only the field accessors; everything below is
/// shared. The one deliberate difference (Redis orders pinned tabs first within
/// a strip; SQL renders pinned in a separate section and keeps raw order) is a
/// `pins_sort_first` hook rather than a forked method.
pub(crate) trait TabWorkspace {
    type Tab: WorkspaceTab;

    fn ws_tabs(&self) -> &[Self::Tab];
    fn ws_tabs_mut(&mut self) -> &mut Vec<Self::Tab>;
    fn ws_active(&self) -> usize;
    fn ws_set_active(&mut self, i: usize);
    fn ws_split(&self) -> Option<&SplitState>;
    fn ws_split_mut(&mut self) -> &mut Option<SplitState>;

    /// Whether pinned tabs sort ahead of the rest within a pane's strip.
    fn pins_sort_first(&self) -> bool {
        false
    }

    /// Which half currently receives actions/focus (`Primary` when unsplit).
    fn focused_half(&self) -> SplitHalf {
        self.ws_split()
            .map(|s| s.focus)
            .unwrap_or(SplitHalf::Primary)
    }

    /// The global tab index the focused half points at.
    fn focused_tab_index(&self) -> usize {
        match self.ws_split() {
            Some(s) if s.focus == SplitHalf::Secondary => s.secondary,
            _ => self.ws_active(),
        }
    }

    /// Record `i` as `half`'s active tab.
    fn set_pane_active(&mut self, half: SplitHalf, i: usize) {
        match half {
            SplitHalf::Primary => self.ws_set_active(i),
            SplitHalf::Secondary => {
                if let Some(s) = self.ws_split_mut() {
                    s.secondary = i;
                }
            }
        }
    }

    /// First tab belonging to `half`, if any.
    fn first_tab_in(&self, half: SplitHalf) -> Option<usize> {
        self.ws_tabs().iter().position(|t| t.pane() == half)
    }

    /// The active tab index of `half`: its stored index when that still names a
    /// tab in the half, else the first tab in the half (`None` if empty).
    fn pane_active(&self, half: SplitHalf) -> Option<usize> {
        let stored = match half {
            SplitHalf::Primary => Some(self.ws_active()),
            SplitHalf::Secondary => self.ws_split().map(|s| s.secondary),
        };
        match stored {
            Some(i) if self.ws_tabs().get(i).is_some_and(|t| t.pane() == half) => Some(i),
            _ => self.first_tab_in(half),
        }
    }

    /// Global indices of the tabs in `half`, in strip order (pinned first when
    /// [`pins_sort_first`](Self::pins_sort_first)).
    fn pane_tab_indices(&self, half: SplitHalf) -> Vec<usize> {
        let mut idx: Vec<usize> = self
            .ws_tabs()
            .iter()
            .enumerate()
            .filter(|(_, t)| t.pane() == half)
            .map(|(i, _)| i)
            .collect();
        if self.pins_sort_first() {
            // Stable sort: pinned (`true`) first, relative order preserved.
            idx.sort_by_key(|&i| !self.ws_tabs()[i].pinned());
        }
        idx
    }

    /// Restore the pane invariants after any tab add/close/move/reorder: collapse
    /// the split when a half has emptied, and re-point each pane's active index at
    /// a tab it actually owns. The safety net every mutation ends on.
    fn normalize_panes(&mut self) {
        if self.ws_tabs().is_empty() {
            self.ws_set_active(0);
            *self.ws_split_mut() = None;
            return;
        }
        if self.ws_split().is_some() {
            let has_primary = self
                .ws_tabs()
                .iter()
                .any(|t| t.pane() == SplitHalf::Primary);
            let has_secondary = self
                .ws_tabs()
                .iter()
                .any(|t| t.pane() == SplitHalf::Secondary);
            if !has_primary || !has_secondary {
                // A half emptied: collapse, keeping the surviving half's tab on screen.
                let survivor = if has_primary {
                    SplitHalf::Primary
                } else {
                    SplitHalf::Secondary
                };
                let keep = self.pane_active(survivor).unwrap_or(0);
                for t in self.ws_tabs_mut() {
                    t.set_pane(SplitHalf::Primary);
                }
                *self.ws_split_mut() = None;
                let clamped = keep.min(self.ws_tabs().len() - 1);
                self.ws_set_active(clamped);
                return;
            }
            // Both halves populated: clamp each active index into its own pane.
            if let Some(p) = self.pane_active(SplitHalf::Primary) {
                self.ws_set_active(p);
            }
            if let Some(sec) = self.pane_active(SplitHalf::Secondary) {
                if let Some(state) = self.ws_split_mut() {
                    state.secondary = sec;
                }
            }
        } else {
            // Single pane: every tab lives in `Primary`; keep the index in range.
            for t in self.ws_tabs_mut() {
                t.set_pane(SplitHalf::Primary);
            }
            if self.ws_active() >= self.ws_tabs().len() {
                let last = self.ws_tabs().len() - 1;
                self.ws_set_active(last);
            }
        }
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    struct TestTab {
        pane: SplitHalf,
        pinned: bool,
    }
    impl WorkspaceTab for TestTab {
        fn pane(&self) -> SplitHalf {
            self.pane
        }
        fn set_pane(&mut self, half: SplitHalf) {
            self.pane = half;
        }
        fn pinned(&self) -> bool {
            self.pinned
        }
    }

    struct TestWs {
        tabs: Vec<TestTab>,
        active: usize,
        split: Option<SplitState>,
        sort_pins: bool,
    }
    impl TabWorkspace for TestWs {
        type Tab = TestTab;
        fn ws_tabs(&self) -> &[TestTab] {
            &self.tabs
        }
        fn ws_tabs_mut(&mut self) -> &mut Vec<TestTab> {
            &mut self.tabs
        }
        fn ws_active(&self) -> usize {
            self.active
        }
        fn ws_set_active(&mut self, i: usize) {
            self.active = i;
        }
        fn ws_split(&self) -> Option<&SplitState> {
            self.split.as_ref()
        }
        fn ws_split_mut(&mut self) -> &mut Option<SplitState> {
            &mut self.split
        }
        fn pins_sort_first(&self) -> bool {
            self.sort_pins
        }
    }

    fn tab(pane: SplitHalf, pinned: bool) -> TestTab {
        TestTab { pane, pinned }
    }
    fn split(secondary: usize, focus: SplitHalf) -> SplitState {
        SplitState {
            secondary,
            focus,
            width: px(500.),
            drag: None,
        }
    }

    #[test]
    fn pane_active_falls_back_to_first_tab_in_half() {
        let ws = TestWs {
            tabs: vec![
                tab(SplitHalf::Primary, false),
                tab(SplitHalf::Secondary, false),
                tab(SplitHalf::Secondary, false),
            ],
            active: 0,
            // Secondary points at a Primary tab (index 0): should fall back to
            // the first Secondary tab (index 1).
            split: Some(split(0, SplitHalf::Secondary)),
            sort_pins: false,
        };
        assert_eq!(ws.pane_active(SplitHalf::Primary), Some(0));
        assert_eq!(ws.pane_active(SplitHalf::Secondary), Some(1));
        assert_eq!(ws.focused_tab_index(), 0); // stored secondary is 0
    }

    #[test]
    fn normalize_collapses_split_when_a_half_empties() {
        let mut ws = TestWs {
            // Both tabs in Primary; the split claims a Secondary that no tab owns.
            tabs: vec![
                tab(SplitHalf::Primary, false),
                tab(SplitHalf::Primary, false),
            ],
            active: 5, // out of range on purpose
            split: Some(split(1, SplitHalf::Secondary)),
            sort_pins: false,
        };
        ws.normalize_panes();
        assert!(ws.split.is_none(), "an emptied half collapses the split");
        assert!(ws.active < ws.tabs.len(), "active index clamped into range");
        assert!(ws.tabs.iter().all(|t| t.pane == SplitHalf::Primary));
    }

    #[test]
    fn pane_tab_indices_orders_pinned_first_only_when_requested() {
        let tabs = || {
            vec![
                tab(SplitHalf::Primary, false),
                tab(SplitHalf::Primary, true),
                tab(SplitHalf::Primary, false),
            ]
        };
        let raw = TestWs {
            tabs: tabs(),
            active: 0,
            split: None,
            sort_pins: false,
        };
        assert_eq!(raw.pane_tab_indices(SplitHalf::Primary), vec![0, 1, 2]);
        let pinned_first = TestWs {
            tabs: tabs(),
            active: 0,
            split: None,
            sort_pins: true,
        };
        // Pinned (index 1) moves ahead; relative order otherwise preserved.
        assert_eq!(
            pinned_first.pane_tab_indices(SplitHalf::Primary),
            vec![1, 0, 2]
        );
    }
}

/// Which key the welcome screen's saved-connection list is ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectSortField {
    Name,
    Recent,
}

/// How the welcome screen's saved-connection list is ordered: a key plus a
/// direction. `ascending` is the key's natural order: A→Z for `Name`, oldest
/// (and never-used) first for `Recent`. Each toolbar button selects its field;
/// clicking the active field again flips the direction. Default is `Recent`
/// descending (most-recently-used first), matching the on-load order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectSort {
    pub field: ConnectSortField,
    pub ascending: bool,
}

impl ConnectSort {
    /// The direction a field defaults to when first selected: names read A→Z,
    /// recency reads newest-first.
    fn default_ascending(field: ConnectSortField) -> bool {
        matches!(field, ConnectSortField::Name)
    }

    /// Select `field`, or (if it's already active) flip the direction. The
    /// welcome screen's sort buttons drive this.
    pub(crate) fn toggle(&mut self, field: ConnectSortField) {
        if self.field == field {
            self.ascending = !self.ascending;
        } else {
            self.field = field;
            self.ascending = Self::default_ascending(field);
        }
    }
}

/// Which top-level screen is showing.
pub(crate) enum Phase {
    Disconnected,
    // Both non-trivial variants are boxed to keep `Phase` small: `ActiveConn`
    // carries the whole schema model, and `Connecting` carries a full config +
    // status, dwarfing the unit `Disconnected`.
    Connecting(Box<Connecting>),
    Connected(Box<ActiveConn>),
}

/// State of an in-progress connection: which config we're dialing, how many
/// attempts we've made, and whether an attempt is in flight or we're waiting
/// out a backoff before the next retry. Drives the connecting splash (progress
/// bar / error / retry / cancel). See [`AppState::start_connect`].
pub(crate) struct Connecting {
    /// The session this connect is opening, minted UI-side so retries reuse it.
    pub session: SessionId,
    /// Stable id of the saved connection being opened ([`StoredConnection::id`]),
    /// so warm/foreground lookups match on identity rather than the display name
    /// (two saved connections may share a name).
    pub conn_id: String,
    /// The session that was foreground when this connect began (parked, kept
    /// warm). Restored on cancel; left parked on success (so both stay warm).
    pub previous: Option<SessionId>,
    pub config: ConnectionConfig,
    /// 1-based number of the attempt currently in flight or just failed.
    pub attempt: u32,
    pub status: ConnectStatus,
}

/// Where a [`Connecting`] is in its attempt/backoff cycle.
pub(crate) enum ConnectStatus {
    /// An attempt is in flight; the indeterminate progress bar sweeps.
    InProgress,
    /// The last attempt failed; we're waiting `delay` before the next retry,
    /// showing the error. `delay` is the wait we scheduled (shown to the user).
    Backoff {
        error: SharedString,
        delay: Duration,
    },
    /// The attempt failed for a user-correctable reason (bad credentials, missing
    /// database): terminal, no retry. The splash shows the error and offers an
    /// "Edit connection" action instead of a countdown. See [`Event::ConnectFailed`].
    Failed { error: SharedString },
    /// The SSH jump host's key isn't trusted yet. The splash shows the fingerprint
    /// and offers "Trust & connect", which writes it to `known_hosts` and retries.
    /// Carries what the retry needs. See [`Event::SshHostUnknown`].
    NeedsHostTrust {
        host: String,
        port: u16,
        fingerprint: SharedString,
        /// OpenSSH-encoded key, sent back via [`Command::TrustSshHost`] on trust.
        key: String,
    },
}

/// Whether a "Test connection" probe is in flight — drives the footer button's
/// "Testing…"/disabled state. The probe's *result* is reported as a toast (see
/// the `TestSucceeded`/`TestFailed` arms in `on_event`), not stored here.
pub(crate) enum TestState {
    Idle,
    Testing,
}

/// Add/edit connection form state. The field text lives in the shared `TextInput`
/// entities on `AppState`; this holds the rest (engine, label, flags). The
/// structured fields and the connection-string mirror are kept in sync live (see
/// `AppState::sync_conn_str_from_fields` / `sync_fields_from_conn_str`).
pub(crate) struct FormState {
    pub kind: DbKind,
    /// Label-palette index (see `connect::label_color`).
    pub color: u8,
    pub read_only: bool,
    /// Encrypt the connection with TLS (see `docs/plans/redis.md`'s TLS toggle
    /// item). Off by default; only offered for network engines.
    pub tls: bool,
    /// `Some(index)` when editing an existing connection, `None` when adding.
    pub editing: Option<usize>,
    /// Set once the user tries to Save/Connect (or Test) with missing fields.
    /// This is the gate for showing the inline per-field validation messages, so a fresh
    /// empty form isn't pre-littered with errors.
    pub submitted: bool,
    pub test: TestState,
    /// Tunnel the connection through an SSH jump host. Off by default; only
    /// offered for network engines (a file engine has no host to reach).
    pub ssh_enabled: bool,
    /// Which SSH auth method the form has selected. The key path and the secrets
    /// live in the shared SSH inputs; this only tracks the choice.
    pub ssh_auth: SshAuthMode,
    /// Opt this connection into the AI **write** tier (Feature B): the assistant may
    /// propose INSERT/UPDATE/DELETE, each gated by per-statement approval. Off by
    /// default; ignored on a read-only connection. Maps to `ai_tier = "write"`.
    pub ai_allow_writes: bool,
}

/// The SSH authentication method picked in the form. Mirrors `red_core::SshAuth`
/// but carries no data; the key path / secrets live in the form's inputs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshAuthMode {
    Agent,
    Password,
    Key,
}

impl SshAuthMode {
    /// Every mode, in the order the form's picker lists them.
    pub(crate) const fn all() -> &'static [SshAuthMode] {
        &[SshAuthMode::Agent, SshAuthMode::Password, SshAuthMode::Key]
    }

    /// The picker label for this mode.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            SshAuthMode::Agent => "Agent",
            SshAuthMode::Password => "Password",
            SshAuthMode::Key => "Key",
        }
    }
}

/// Which connection-form field a validation message belongs to, so it can render
/// directly beneath that input instead of as a detached toast.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormField {
    Name,
    Host,
    Database,
    SshHost,
    SshUser,
    SshKeyPath,
}

/// A write awaiting the confirm modal (Track B5 generalized the destructive-confirm
/// path to carry either). Confirming runs it; cancelling drops it.
#[derive(Clone)]
pub(crate) enum PendingWrite {
    /// A destructive editor statement (`UPDATE`/`DELETE`/… typed in the SQL editor),
    /// run verbatim via `execute_sql` on confirm.
    EditorSql(String),
    /// A staged grid edit batch (Track B6): the previewed, parameterized
    /// [`EditOp`]s sent as one `Command::ApplyBatch` on confirm. `epoch` scopes the
    /// reply to its result.
    Batch { ops: Vec<EditOp>, epoch: u64 },
    /// A confirmed data import (Track: data import): everything to fire
    /// `Command::Import` on confirm, plus the precomputed `prose`/`preview` the
    /// confirm dialog shows so the user sees the file→table mapping before any write.
    Import {
        path: std::path::PathBuf,
        format: ImportFormat,
        target: TableRef,
        mapping: Vec<ColumnMap>,
        id: u64,
        prose: String,
        preview: String,
    },
    /// A confirmed table copy (result → another table). Carries everything to fire
    /// `Command::CopyToTable` on confirm plus the precomputed `prose`/`preview` (the
    /// name-based mapping) the dialog shows. The confirm offers two modes: Append (the
    /// default `mode`) and Truncate+insert (the danger button), so the destructive
    /// refresh is opt-in behind a distinct, clearly-labeled action.
    Copy {
        id: u64,
        source_epoch: u64,
        target: TableRef,
        target_session: SessionId,
        mapping: Vec<ColumnMap>,
        mode: CopyMode,
        /// `Some` for "copy into a *new* table": the column shape to `create_table`
        /// the target from before streaming (types mapped into the target dialect).
        /// `None` for a copy into an existing table (the original same-shape path).
        create: Option<Vec<ColumnMeta>>,
        prose: String,
        preview: String,
    },
}

/// A pending `ImportColumns` peek: the chosen file + the target table/columns, held
/// while the backend reads the source header. When `ImportColumns` returns, the UI
/// builds a name-based mapping against `target_cols` and raises the import confirm.
pub(crate) struct PendingImportPeek {
    pub id: u64,
    pub path: std::path::PathBuf,
    pub format: ImportFormat,
    pub target: TableRef,
    pub target_cols: Vec<Column>,
}

/// One candidate copy **target** in the "Copy to…" picker: a table in an open
/// (writable) connection. The picker lists these across the foreground + parked
/// live sessions; activating one starts the target-column peek.
#[derive(Clone)]
pub(crate) struct CopyTargetCandidate {
    pub session: SessionId,
    pub conn_name: String,
    pub schema: String,
    pub table: TableRef,
}

/// A pending "Copy to…" target peek: the chosen source (the foreground result's
/// epoch + columns) and target (table + its session + a display label), held while
/// the backend describes the target's columns. When `CopyTargetColumns` returns, the
/// UI auto-maps source→target by name and raises the copy confirm.
pub(crate) struct PendingCopyPeek {
    pub id: u64,
    pub source_epoch: u64,
    pub source_cols: Vec<Column>,
    pub target: TableRef,
    pub target_session: SessionId,
    pub target_label: String,
}

/// A distinct writable namespace (a connection's schema/database) offered in the
/// "Copy to…" picker as a **"new table"** target. Selecting one prompts for a table
/// name; the source's column shape then creates it (`create_table`, types mapped into
/// the target dialect) before the rows stream in; the "copy into a *new* table" /
/// migration path. Mirrors [`CopyTargetCandidate`] but addresses a namespace, not an
/// existing table, so an *empty* database can be a target.
#[derive(Clone)]
pub(crate) struct CopyNamespace {
    pub session: SessionId,
    pub conn_name: String,
    pub schema: String,
}

/// A pending "new table" copy: the chosen source (the focused result's epoch +
/// columns) and the target namespace, held while the user types the new table's name
/// in the prompt. On submit the UI builds a `create_table` spec + an identity column
/// mapping and fires `CopyToTable { create: Some(..) }`.
pub(crate) struct PendingCopyNewTable {
    pub source_epoch: u64,
    pub source_cols: Vec<Column>,
    pub session: SessionId,
    pub conn_name: String,
    pub schema: String,
}

/// The editable grid cell under the cursor (Track B6): its identity (the row's PK
/// value), position (absolute row, data column), and the focused column's declared
/// type / current value. Built by [`ResultGrid::edit_target`] and consumed by the
/// inline editor + the inspector edit, which coerce a typed value against
/// `decl_type` and stage it under the row's PK.
#[derive(Clone)]
pub(crate) struct EditContext {
    pub epoch: u64,
    /// Absolute row ordinal and data-column index of the edited cell.
    pub row: usize,
    pub data_col: usize,
    pub pk_value: red_core::Value,
    pub decl_type: Option<String>,
    pub original: red_core::Value,
    /// Set when the edited cell is an inline-expanded foreign-key column (Track
    /// B7): the edit updates the *referenced* table's row, not this browse's base
    /// table. `None` for an ordinary base-table cell (updated via the row's PK).
    pub foreign: Option<ForeignEdit>,
}

/// The referenced-table target for editing an inline-expanded foreign-key column
/// (Track B7). A joined column shows a value from a *referenced* table, so writing
/// it back is an `UPDATE <ref> SET <col> = ? WHERE <ref key> = <fk value>`, a
/// different table and row than the base browse's PK edit. Resolved single-hop only
/// (the referenced row is identified by the FK value resident in the base row);
/// multi-hop / composite-key expansions stay read-only. Note the referenced row may
/// be shared by several base rows, so the edit changes all of them; the confirm
/// preview shows the literal `UPDATE`, and the batch reloads afterwards so the
/// denormalized view re-resolves.
#[derive(Clone)]
pub(crate) struct ForeignEdit {
    /// The referenced table being updated.
    pub table: red_core::TableRef,
    /// The referenced table's unique key column the foreign key points at (the
    /// join's target column); this is the `WHERE` predicate.
    pub key_column: String,
    /// The foreign-key value from the base row, identifying the referenced row.
    pub key_value: red_core::Value,
    /// The base foreign-key column's declared type, so the `WHERE` bind casts back
    /// to the key column's type (a uuid/text key needs the cast on Postgres).
    pub key_type: Option<String>,
    /// The referenced column being set: the join leaf (`name`), not the dotted
    /// output alias (`tier_id.name`) the result column carries.
    pub set_column: String,
}

/// How long a transient (info / success) toast stays up before it auto-dismisses.
/// Errors and warnings (and a live export) have no timer; they persist until the
/// user closes them or the operation resolves.
pub(crate) const TOAST_AUTO_DISMISS: Duration = Duration::from_secs(4);

/// Most warm parked sessions kept resident at once. Each is a heavy `ActiveConn`
/// (editor entities, schema detail map, result buffers), so the map is capped:
/// parking past this LRU-evicts the least-recently-foregrounded session (closing
/// its backend session too). The cap makes a missed backend `Disconnected` a
/// bounded annoyance instead of unbounded growth.
const MAX_PARKED_SESSIONS: usize = 8;

/// Most persistent (error / warning) notifications retained at once. Transient
/// info/success toasts self-dismiss; persistent ones are removed only by a user
/// click, so a burst of query errors is capped here: the oldest persistent toast
/// is dropped past this. Visible toasts are already capped lower in the renderer.
const MAX_NOTIFICATIONS: usize = 50;

/// Which streamed transfer a progress toast tracks; selects the right cancel
/// command for the toast's `✕` and the verb in its messages.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferKind {
    Export,
    Import,
    /// A table copy (result → another table), possibly across connections.
    Copy,
    /// A whole-schema migration (many tables → another database), reusing the copy
    /// backend + `Copy*` events but labelled "Migrate" in the toast.
    Migrate,
}

impl TransferKind {
    /// The (gerund, past, noun) verbs for the copy-family toasts: `Copy` and
    /// `Migrate` share the streaming backend and `Copy*` events but read differently.
    /// `Export`/`Import` have their own toasts and never call this.
    pub(crate) fn copy_verbs(self) -> (&'static str, &'static str, &'static str) {
        match self {
            TransferKind::Migrate => ("Migrating", "Migrated", "Migration"),
            _ => ("Copying", "Copied", "Copy"),
        }
    }
}

/// The live state of a streaming-transfer toast (export, import, *or* copy): how
/// many rows have moved, keyed by the transfer `id` so a cancel / progress update
/// targets the right one. `total` is the known row count for an export/copy (drives
/// a %); an import streams a file of unknown length, so `total` is 0 and the toast
/// shows a running count. `kind` selects the cancel command for the `✕`.
pub(crate) struct ExportProgress {
    pub id: u64,
    pub rows: usize,
    pub total: usize,
    pub kind: TransferKind,
}

/// One notification in the bottom-right stack. The stack is newest-last (nearest
/// the corner); `auto_dismiss` drives the per-toast timer (`None` = persists until
/// closed); `export` is set only on the export-progress toast.
///
/// `message` is the title; `detail` is an optional secondary body. When `detail`
/// is set we also build a `detail_label` (a selectable, copyable view of that
/// text) so the user can highlight part of a long message and ⌘/Ctrl+C it.
/// `expanded` toggles the collapse of a long body; `hovered` pauses the
/// auto-dismiss timer while the pointer is over the toast (`dismiss_gen` makes a
/// re-armed timer cancel any stale one).
pub(crate) struct Notification {
    pub id: u64,
    pub variant: ToastVariant,
    pub message: SharedString,
    pub detail: Option<SharedString>,
    pub detail_label: Option<Entity<SelectableLabel>>,
    pub auto_dismiss: Option<Duration>,
    pub export: Option<ExportProgress>,
    pub expanded: bool,
    pub hovered: bool,
    pub dismiss_gen: u64,
    /// An optional trailing call-to-action button (e.g. the post-update toast's
    /// "Show changelog"). `None` for an ordinary toast.
    pub action: Option<NotificationAction>,
}

/// The call-to-action a toast can offer beyond copy/close, rendered as a trailing
/// accent button in `render_notifications`. A toast carrying one of these is
/// content-specific enough that the generic copy button is skipped (see
/// `render_notifications`): there's nothing copy-worthy about "RED updated" or
/// an export's file path, and the action itself is the more useful affordance.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum NotificationAction {
    /// Open the "What's New" panel (the post-update announcement toast).
    ShowChangelog,
    /// Reveal the written file in the OS file manager (Finder / Explorer / the
    /// platform's file manager), selected. Carries the file's full path.
    RevealInFileManager(SharedString),
}

/// The default editor text a fresh query tab opens with. A tab still holding
/// exactly this (and no result) is "pristine"; closing it needs no confirmation.
#[cfg(target_os = "macos")]
pub(crate) const EMPTY_QUERY: &str = "-- Write SQL, ⌘↵ to run\n";
#[cfg(not(target_os = "macos"))]
pub(crate) const EMPTY_QUERY: &str = "-- Write SQL, Ctrl+Enter to run\n";

/// One query tab: its own SQL editor and result grid. A connection holds several
/// of these; the schema sidebar, split sizes, and query history are shared.
pub(crate) struct QueryTab {
    /// Tab label: "query N" for a blank tab, or "schema.table" for a preview.
    pub title: String,
    /// The SQL editor surface, with the RED highlighter installed.
    pub editor: Entity<CodeEditor>,
    /// The open result browsed in the grid: a table preview or an editor run.
    pub result: Option<ResultGrid>,
    /// The query plan (Track B4, EXPLAIN), when one is open. Occupies the result
    /// pane in place of the grid; running a query clears it. `None` is the grid.
    pub plan: Option<crate::plan::PlanView>,
    /// Which split half owns this tab (Zed-style): each pane's tab strip shows only
    /// its own tabs, so the two halves never duplicate. Always `Primary` while the
    /// work area is unsplit; a drag across the divider (or `split_right`) reassigns it.
    pub pane: SplitHalf,
    /// Pinned tabs render in a fixed section at the start of the strip, always
    /// visible regardless of scroll, and are skipped by the bulk close actions
    /// (Close Others / Close All / Close Left / Close Right).
    pub pinned: bool,
}

impl QueryTab {
    pub(crate) fn new(title: String, cx: &mut Context<AppState>) -> Self {
        let editor = cx.new(|cx| {
            // A play run marker in the gutter on each statement's first line.
            // gpui's `svg()` paints only when the svg element's *own* `text_color`
            // is set — it does not inherit the marker cell's colour — so we colour
            // the icon (and its hover accent) here rather than leaning on the cell.
            // The theme colours are captured at build; a live theme switch only
            // affects tabs opened afterwards.
            let (marker_fg, marker_accent) = (cx.theme().text_faint, cx.theme().accent);
            CodeEditor::new(cx)
                .highlighter(crate::sql::tokenize)
                .gutter_markers(crate::sql::statement_start_lines)
                .gutter_marker_icon(move || {
                    gpui::svg()
                        .path("icons/play.svg")
                        .size(px(11.))
                        .flex_none()
                        .text_color(marker_fg)
                        .hover(|s| s.text_color(marker_accent))
                        .into_any_element()
                })
                .corner_radius(px(0.))
                .resting_border(false)
                .a11y_label("Query editor")
                .with_content(EMPTY_QUERY)
        });
        // ⌘↵ runs the active tab's statement / selection; Esc (with no completion
        // open) jumps focus to the result grid, so run → inspect is a keyboard loop.
        cx.subscribe(
            &editor,
            |this, _editor, event: &CodeEditorEvent, cx| match event {
                CodeEditorEvent::Run => this.run_editor_query(cx),
                CodeEditorEvent::RunLine(line) => this.run_editor_line(*line, cx),
                CodeEditorEvent::Escape => {
                    this.pending_focus = Some(Pane::Grid);
                    cx.notify();
                }
                // The query editor never sends (it's not a `submit_on_enter`
                // composer); Enter inserts a line here.
                CodeEditorEvent::Submit => {}
            },
        )
        .detach();

        Self {
            title,
            editor,
            result: None,
            plan: None,
            // New tabs join the focused pane; `push_tab` reassigns this when split.
            pane: SplitHalf::Primary,
            pinned: false,
        }
    }

    /// A blank tab the user hasn't touched: no result and the default text still
    /// in the editor. Closing one of these doesn't warrant a confirmation.
    pub(crate) fn is_pristine(&self, cx: &Context<AppState>) -> bool {
        self.result.is_none() && self.editor.read(cx).content() == EMPTY_QUERY
    }
}

impl WorkspaceTab for QueryTab {
    fn pane(&self) -> SplitHalf {
        self.pane
    }
    fn set_pane(&mut self, half: SplitHalf) {
        self.pane = half;
    }
    fn pinned(&self) -> bool {
        self.pinned
    }
}

impl TabWorkspace for ActiveConn {
    type Tab = QueryTab;
    fn ws_tabs(&self) -> &[QueryTab] {
        &self.tabs
    }
    fn ws_tabs_mut(&mut self) -> &mut Vec<QueryTab> {
        &mut self.tabs
    }
    fn ws_active(&self) -> usize {
        self.active_tab
    }
    fn ws_set_active(&mut self, i: usize) {
        self.active_tab = i;
    }
    fn ws_split(&self) -> Option<&SplitState> {
        self.split.as_ref()
    }
    fn ws_split_mut(&mut self) -> &mut Option<SplitState> {
        &mut self.split
    }
    // SQL renders pinned tabs in a separate strip section, so `pane_tab_indices`
    // keeps raw order (`pins_sort_first` stays the default `false`).
}

/// The live-connection view state: which connection, its engine version, the
/// resizable split sizes (caller-owned, per `SplitPane`'s stateless contract),
/// the schema explorer, and the open query tabs.
pub(crate) struct ActiveConn {
    /// The backend session backing this workspace. Stays warm while parked, so a
    /// switch back is instant; binds this conn's `CommandSender`.
    pub session: SessionId,
    /// Stable id of the saved connection this workspace belongs to
    /// ([`StoredConnection::id`]); the switcher matches warm/foreground sessions
    /// by this, not by `config.name` (names aren't unique).
    pub conn_id: String,
    pub config: ConnectionConfig,
    pub version: String,
    /// Width of the Schema side-panel (the second left-dock column). Retained while
    /// it's hidden so toggling it back restores the previous width.
    pub sidebar_w: Pixels,
    pub sidebar_drag: Option<DragAnchor>,
    /// When set, the Schema panel is hidden; `sidebar_w` is retained so toggling it
    /// back restores the previous width.
    pub sidebar_collapsed: bool,
    pub editor_h: Pixels,
    pub editor_drag: Option<DragAnchor>,
    /// Width of the cell/row detail inspector when docked to the right of the
    /// grid; retained while the inspector is closed so reopening restores it.
    pub inspector_w: Pixels,
    pub inspector_drag: Option<DragAnchor>,
    pub schema: SchemaState,
    /// The connection-wide foreign-key graph (Track B7), prefetched once after
    /// connect. Empty until it lands (or when the engine has no FKs); drives the
    /// in-grid FK click-through. See [`Command::LoadForeignKeys`].
    pub fk_graph: Vec<FkEdge>,
    /// Cached FK lookup lists for the in-cell picker (Track B8), keyed by referenced
    /// `(schema, table)`. Populated lazily the first time an FK cell of that target is
    /// edited (`Command::FetchLookup` → `Event::LookupReady`); reused across edits and
    /// results on this connection. Bounded by the number of distinct FK targets touched.
    pub lookup_cache: std::collections::HashMap<(String, String), Vec<red_core::LookupRow>>,
    /// Cached enum columns per `(schema, table)` for the in-cell enum picker (Track B8):
    /// `{ column → [variant, …] }`, loaded once per table the first time one of its cells
    /// is edited (`Command::LoadEnums` → `Event::EnumsLoaded`). An absent table means "not
    /// loaded yet"; a present-but-column-absent means "not an enum".
    pub enum_cache:
        std::collections::HashMap<(String, String), std::collections::HashMap<String, Vec<String>>>,
    /// Tables whose `LoadEnums` is in flight, so the load fires at most once per table.
    pub enum_requested: std::collections::HashSet<(String, String)>,
    /// Open query tabs (never empty), and the index of the focused one.
    pub tabs: Vec<QueryTab>,
    pub active_tab: usize,
    /// Monotonic counter for naming blank tabs ("query 1", "query 2", …).
    pub query_seq: usize,
    /// While a tab is being dragged, the gap (insertion index `0..=tabs.len()`)
    /// where it would land; drives the drop indicator. Only meaningful when a
    /// drag is active; the strip gates rendering on `has_active_drag`.
    pub tab_drop_target: Option<usize>,
    /// Horizontal scroll position of the tab strip, so a crowded strip scrolls
    /// instead of squashing tabs. Persists across renders.
    pub tab_scroll: ScrollHandle,
    /// Focus anchors for the schema sidebar and result grid panes, so keyboard
    /// focus can move between panes and each can receive its own navigation keys.
    /// The editor pane focuses its own `CodeEditor` directly.
    pub schema_focus: FocusHandle,
    pub grid_focus: FocusHandle,
    /// Focus anchor for the *secondary* (right) half's result grid while the work
    /// area is split. The primary half keeps `grid_focus`; giving the second grid
    /// its own handle keeps keyboard focus unambiguous between the two grids.
    pub secondary_grid_focus: FocusHandle,
    /// Which pane currently holds focus; drives focus cycling and the pane ring.
    pub active_pane: Pane,
    /// When `Some`, the work area is split into two side-by-side query panes. See
    /// [`SplitState`]; `None` is the single-pane layout.
    pub split: Option<SplitState>,
    /// Whether the History panel is shown in the left dock. Entries live in the
    /// centralized [`AppState::query_history`]; this is per-connection UI state.
    pub history_open: bool,
    /// Focus anchor for the History panel's list (its ↑/↓/Enter navigation), and
    /// the keyboard-highlighted entry within it.
    pub history_focus: FocusHandle,
    pub history_sel: usize,
    /// Width of the History side-panel (the leftmost left-dock column, sitting to
    /// the left of the schema). Retained while it's closed so toggling it back
    /// restores the previous width.
    pub history_w: Pixels,
    pub history_drag: Option<DragAnchor>,
    /// Whether the Columns panel (inline FK expansion, Track B7) is shown in the left
    /// dock, i.e. the recursive tree that picks referenced columns into the active
    /// browse.
    /// Per-connection UI state; the picked columns live on the result grid.
    pub columns_open: bool,
    pub columns_w: Pixels,
    pub columns_drag: Option<DragAnchor>,
    /// Recency stamp: bumped from [`AppState::next_active_seq`] each time this
    /// workspace is parked (it was foreground until that moment). Drives LRU
    /// eviction when [`MAX_PARKED_SESSIONS`] is exceeded: the lowest stamp is the
    /// least-recently-foregrounded parked session.
    pub last_active_seq: u64,
    /// The read-only ER diagram overlay when open (schema-wide, so it hangs off the
    /// connection, not a tab). `None` when closed. See [`crate::er`].
    pub er: Option<crate::er::ErView>,
    /// The Redis shell's dynamic tab set (see docs/plans/redis-workflow-parity.md);
    /// `Some` only for a `DbKind::Redis` session, set up in `on_connected`.
    /// `None` for every SQL engine. Constructed here (needs `cx` to make the
    /// default Browse tab's filter `TextInput`); `on_connected` fires that
    /// tab's initial `KvDbSize`/`KvFetchScan` once the session is live.
    pub kv_view: Option<crate::kvbrowse::RedisView>,
}

impl ActiveConn {
    fn new(
        session: SessionId,
        conn_id: String,
        config: ConnectionConfig,
        version: String,
        cx: &mut Context<AppState>,
    ) -> Self {
        let tab = QueryTab::new("query 1".to_string(), cx);
        let kv_view =
            (config.kind == DbKind::Redis).then(|| crate::kvbrowse::RedisView::new(session, cx));
        Self {
            session,
            conn_id,
            config,
            version,
            sidebar_w: px(240.),
            sidebar_drag: None,
            sidebar_collapsed: false,
            editor_h: px(300.),
            editor_drag: None,
            inspector_w: px(360.),
            inspector_drag: None,
            schema: SchemaState::new(cx),
            fk_graph: Vec::new(),
            lookup_cache: std::collections::HashMap::new(),
            enum_cache: std::collections::HashMap::new(),
            enum_requested: std::collections::HashSet::new(),
            tabs: vec![tab],
            active_tab: 0,
            query_seq: 1,
            tab_drop_target: None,
            tab_scroll: ScrollHandle::new(),
            schema_focus: cx.focus_handle(),
            grid_focus: cx.focus_handle(),
            secondary_grid_focus: cx.focus_handle(),
            active_pane: Pane::Editor,
            split: None,
            history_open: false,
            history_focus: cx.focus_handle(),
            history_sel: 0,
            history_w: px(240.),
            history_drag: None,
            columns_open: false,
            columns_w: px(260.),
            columns_drag: None,
            last_active_seq: 0,
            er: None,
            kv_view,
        }
    }

    /// Point the focused half at tab `i` (a global index that already belongs to that
    /// half; each strip shows only its own tabs, so a strip click never crosses).
    pub(crate) fn set_focused_tab(&mut self, i: usize) {
        let half = self.focused_half();
        self.set_pane_active(half, i);
    }

    /// The focus handle for the result grid in `half`; the second half has its own
    /// so keyboard focus never lands on both grids at once.
    pub(crate) fn grid_focus_for(&self, half: SplitHalf) -> &FocusHandle {
        match half {
            SplitHalf::Primary => &self.grid_focus,
            SplitHalf::Secondary => &self.secondary_grid_focus,
        }
    }

    /// The focused tab, or `None` when the strip is empty (the user closed the
    /// last tab; the shell then shows an empty pane instead of a query editor).
    pub(crate) fn active(&self) -> Option<&QueryTab> {
        self.tabs.get(self.focused_tab_index())
    }

    pub(crate) fn active_mut(&mut self) -> Option<&mut QueryTab> {
        let i = self.focused_tab_index();
        self.tabs.get_mut(i)
    }

    /// The focused tab's open result, if any. Folds together "no tab" and "tab
    /// with no result", the common shape at most result call sites.
    pub(crate) fn active_result(&self) -> Option<&ResultGrid> {
        self.active().and_then(|t| t.result.as_ref())
    }

    pub(crate) fn active_result_mut(&mut self) -> Option<&mut ResultGrid> {
        self.active_mut().and_then(|t| t.result.as_mut())
    }

    /// Find the open result whose grid carries `epoch`, across all tabs; result
    /// events route by epoch so a background tab's query still populates.
    pub(crate) fn result_by_epoch(&mut self, epoch: u64) -> Option<&mut ResultGrid> {
        self.tabs
            .iter_mut()
            .filter_map(|t| t.result.as_mut())
            .find(|g| g.epoch == epoch)
    }

    /// Find the open plan carrying `epoch`, across all tabs; `PlanReady`/
    /// `PlanFailed` route by epoch like result events.
    pub(crate) fn plan_by_epoch(&mut self, epoch: u64) -> Option<&mut crate::plan::PlanView> {
        self.tabs
            .iter_mut()
            .filter_map(|t| t.plan.as_mut())
            .find(|p| p.epoch == epoch)
    }
}

/// A captured-but-not-yet-committed rebind in the Keymap settings tab: the row
/// being rebound, the chord the recorder caught (canonical `cmd-shift-f` form),
/// and the row it collides with, if any. Held only between capture and the user's
/// Confirm / Cancel; cleared on either.
pub(crate) struct KeymapCapture {
    /// Index into [`crate::keymap::action_defs`] of the row being rebound.
    pub(crate) row: usize,
    /// The captured keystroke, ready to write to `keymap.toml`.
    pub(crate) chord: String,
    /// The row this chord already binds in the same context, if it's a conflict.
    pub(crate) conflict: Option<usize>,
}

pub struct AppState {
    pub(crate) service: ServiceHandle,
    pub(crate) connections: Vec<StoredConnection>,
    pub(crate) phase: Phase,
    pub(crate) name_input: Entity<TextInput>,
    pub(crate) host_input: Entity<TextInput>,
    pub(crate) port_input: Entity<TextInput>,
    pub(crate) user_input: Entity<TextInput>,
    pub(crate) password_input: Entity<TextInput>,
    pub(crate) database_input: Entity<TextInput>,
    pub(crate) conn_str_input: Entity<TextInput>,
    /// SSH-tunnel fields, shown when the form's `ssh_enabled` is on (network
    /// engines only). The two secret inputs are obscured.
    pub(crate) ssh_host_input: Entity<TextInput>,
    pub(crate) ssh_port_input: Entity<TextInput>,
    pub(crate) ssh_user_input: Entity<TextInput>,
    pub(crate) ssh_key_path_input: Entity<TextInput>,
    pub(crate) ssh_password_input: Entity<TextInput>,
    pub(crate) ssh_passphrase_input: Entity<TextInput>,
    /// Numeric steppers for the two font sizes in the Appearance panel. Stateful
    /// (they own an editable field), so the panel renders these rather than
    /// rebuilding them per frame; `Change` writes straight through to settings.
    pub(crate) ui_font_size_input: Entity<NumberInput>,
    pub(crate) editor_font_size_input: Entity<NumberInput>,
    pub(crate) form: Option<FormState>,
    /// The bottom-right notification stack, oldest first. Rendered newest-last
    /// (nearest the corner) by `render`; capped on screen with a "+N more" line.
    pub(crate) notifications: Vec<Notification>,
    /// Monotonic id source for notifications, so a timer / close / export update
    /// targets the right toast regardless of stack churn.
    pub(crate) next_notification_id: u64,
    /// Monotonic id source for exports, so progress / finished / cancelled events
    /// and a `CancelExport` route to the right in-flight export.
    pub(crate) next_export_id: u64,
    /// Monotonic id source for clipboard re-fetches, matching a `CopyRowsLoaded`
    /// reply to the copy that asked for it.
    pub(crate) next_copy_id: u64,
    /// A copy of a selection that touches display-clipped text, waiting on its
    /// full-row re-fetch (`CopyRows`). The latest copy wins; an earlier reply is
    /// then stale and dropped.
    pub(crate) pending_copy: Option<crate::result::PendingCopy>,
    /// A pending "Copy to…" target peek, held while the backend describes the picked
    /// target table's columns (mirrors `pending_import`). When `CopyTargetColumns`
    /// returns, the UI builds the name-based mapping and raises the copy confirm.
    pub(crate) pending_copy_target: Option<PendingCopyPeek>,
    /// The candidate target tables backing the open "Copy to…" picker, indexed by the
    /// picker's `Cmd::CopyTarget(usize)` activation (mirrors `saved_queries`).
    pub(crate) copy_targets: Vec<CopyTargetCandidate>,
    /// The distinct writable namespaces backing the "✦ New table…" rows of the open
    /// "Copy to…" picker, indexed by `Cmd::CopyNewTable(usize)`.
    pub(crate) copy_new_namespaces: Vec<CopyNamespace>,
    /// A pending "new table" copy, held while the user types the table name in the
    /// prompt (mirrors `pending_copy_target`).
    pub(crate) pending_copy_new: Option<PendingCopyNewTable>,
    /// The target namespaces backing the open "Migrate to…" picker, indexed by
    /// `Cmd::MigrateTarget(usize)`.
    pub(crate) migrate_targets: Vec<CopyNamespace>,
    /// The source of a pending whole-schema migrate, `(source session, source schema,
    /// table names)`, held while the user picks a target namespace from the picker.
    pub(crate) pending_migrate: Option<(SessionId, String, Vec<String>)>,
    /// An in-flight FK click-through (Track B7), waiting on its single-row
    /// `CopyRows` re-fetch to read the typed key value before opening the target
    /// browse. The latest follow wins; an earlier reply is then stale and dropped.
    pub(crate) pending_fk: Option<crate::result::PendingFkFollow>,
    /// The cell detail inspector, when open (Track B1). Owns its scroll position
    /// and any on-demand full value fetched for a capped/evicted cell.
    pub(crate) inspector: Option<crate::inspector::InspectorState>,
    /// The AI assistant chat panel, when open (right-docked). Owns its input,
    /// transcript, and streaming state. Single panel across the workspace.
    pub(crate) assistant: Option<crate::assistant::AssistantState>,
    /// Set when the assistant panel just opened: the next render focuses its input.
    pub(crate) focus_assistant: bool,
    /// Set when an inline conversation rename just began: the next render focuses
    /// its edit field so the user types the new title at once.
    pub(crate) focus_rename: bool,
    /// Docked width of the assistant panel, retained while it's closed so reopening
    /// restores it. Resizable via the shell split.
    pub(crate) assistant_w: Pixels,
    pub(crate) assistant_drag: Option<DragAnchor>,
    /// Whether the assistant is usable at all: at least one configured agent is
    /// ready (an ACP agent, which needs no key, or an API agent with a key). Drives
    /// the panel's setup-vs-chat view. Recomputed at launch and on settings reload.
    pub(crate) ai_configured: bool,
    /// The usable agents in config order, the source for the panel's agent
    /// selector and the per-chat default. An API agent appears only once it has a
    /// key; an ACP agent always. Recomputed with `ai_configured`.
    pub(crate) usable_agents: Vec<AgentInfo>,
    /// Shared obscured field for entering an API agent's key in Settings → AI agent
    /// → Agents. One input reused across rows; `ai_key_editing` says which agent it's
    /// bound to (`None` = no row is editing). The key is written to the OS keyring
    /// under the agent's id, never to settings.toml.
    pub(crate) ai_key_input: Entity<TextInput>,
    /// The id of the API agent whose key row is currently open for editing, if any.
    pub(crate) ai_key_editing: Option<String>,
    /// Set when an agent key row just opened: the next render focuses `ai_key_input`.
    pub(crate) focus_ai_key: bool,
    /// Last-known subscription sign-in identity per ACP agent id, shown in Settings →
    /// AI. Filled by `AiAgentAuthStatus`; absent until the agent is first checked.
    pub(crate) ai_auth: HashMap<String, AiAuthStatus>,
    /// The in-flight interactive subscription sign-in (paste-code), if any; one at
    /// a time. The pasted code lives in the shared `ai_login_code` field.
    pub(crate) ai_login: Option<AiLoginFlow>,
    /// Shared field for the pasted OAuth code during an ACP sign-in (mirrors
    /// `ai_key_input`). Enter submits the code; Esc cancels the sign-in.
    pub(crate) ai_login_code: Entity<TextInput>,
    /// Set when a sign-in prompt just appeared: the next render focuses `ai_login_code`.
    pub(crate) focus_login_code: bool,
    /// Monotonic id source for assistant conversations, so the backend keeps each
    /// panel's turn history separate.
    pub(crate) next_conversation_id: u64,
    /// Whether the column-stats bar is on (a session-ephemeral toggle, like the
    /// filter bar's visibility). When on, selecting a column requests its
    /// pushed-down aggregate summary; the per-column result lives on the grid.
    pub(crate) stats_bar: bool,
    /// The result filter bar, when open (Track B2). The transient editing UI; the
    /// *applied* filter lives on the grid (`ResultGrid::filter`).
    pub(crate) filter_bar: Option<crate::filter::FilterBarState>,
    /// The find-in-result bar, when open (Track B2, Tier 1). Transient UI; it
    /// scans loaded rows and holds the matches + focused index in its own state.
    pub(crate) find_bar: Option<crate::find::FindBarState>,
    /// Window-coordinate anchor for the result cell's right-click context menu,
    /// when open. The right-click selects the cell first, so the menu's Inspect/
    /// Copy act on it; `None` keeps the menu closed.
    pub(crate) cell_menu: Option<gpui::Point<gpui::Pixels>>,
    /// Window-coordinate anchor for the result toolbar's "Export" dropdown, when
    /// open (CSV / JSON / HTML grouped into one menu); `None` keeps it closed.
    pub(crate) export_menu: Option<gpui::Point<gpui::Pixels>>,
    /// Window-coordinate anchor for the result toolbar's "More" dropdown, when
    /// open (Stats toggle · Copy to…); `None` keeps it closed.
    pub(crate) more_menu: Option<gpui::Point<gpui::Pixels>>,
    /// A middle-click-held autoscroll in progress over a result grid, browser-
    /// style: holding the button and moving away from the click point scrolls
    /// continuously toward it, at a speed proportional to the distance. `None`
    /// when idle. See [`crate::result::autoscroll`].
    pub(crate) autoscroll: Option<crate::result::Autoscroll>,
    /// Bumped each time a new autoscroll session starts, so a superseded
    /// session's still-running timer loop notices and exits instead of driving
    /// a scroll the user already cancelled/restarted elsewhere.
    pub(crate) autoscroll_epoch: u64,
    /// A pending write awaiting the user's confirmation before it runs: an editor
    /// destructive statement, or a staged grid edit batch (Track B6). See
    /// [`PendingWrite`].
    pub(crate) confirm_exec: Option<PendingWrite>,
    /// A data-import header peek in flight (file chosen, awaiting the source columns
    /// from the backend so the import confirm can be built). At most one at a time.
    pub(crate) pending_import: Option<PendingImportPeek>,
    /// The open inline cell editor (Track B6), when the user is editing a grid cell
    /// in place. `None` when no editor is open. The staged change-set itself lives
    /// on the result; this is just the live `TextInput`.
    pub(crate) grid_edit: Option<crate::result::GridEdit>,
    /// The in-cell foreign-key suggestion picker (Track B8), when the open editor
    /// targets an FK column: the fetched id/label list plus the live query and
    /// highlighted row. `None` for a plain cell. See [`crate::result::CellSuggest`].
    pub(crate) cell_suggest: Option<crate::result::CellSuggest>,
    /// The open editor cell's on-screen rect, recorded by a `canvas` in the cell so
    /// the suggestion dropdown can anchor below it (the ComboBox/CodeEditor pattern).
    /// One frame behind, like every `canvas`-measured bound; the popover just waits.
    pub(crate) cell_suggest_bounds: gpui::Entity<Option<gpui::Bounds<gpui::Pixels>>>,
    /// Render-time focus drain: focus the open inline editor's field on the next
    /// frame (set when one opens, like `focus_inspector_edit`).
    pub(crate) focus_grid_edit: bool,
    /// Focus-out listener on the open inline editor: clicking away commits (stages)
    /// the edit, like a spreadsheet. Held while an editor is open, dropped when it
    /// closes (mirrors `modal_focus_trap`).
    pub(crate) grid_edit_blur: Option<gpui::Subscription>,
    /// A non-pristine query tab the user asked to close, awaiting confirmation.
    pub(crate) confirm_close_tab: Option<usize>,
    /// A Redis key the user asked to delete from a browse list (via its
    /// right-click menu), awaiting the confirmation modal: `(session, key)`.
    pub(crate) confirm_kv_delete: Option<(SessionId, String)>,
    /// A bulk close (Close Others / Close All / Close Left / Close Right)
    /// awaiting confirmation because at least one target tab isn't pristine.
    pub(crate) confirm_close_batch: Option<Vec<usize>>,
    /// Window-coordinate anchor for a tab's right-click context menu (Close /
    /// Close Others / Close All / Close Left / Close Right / Pin), keyed by the
    /// tab's index; `None` keeps it closed.
    pub(crate) tab_context_menu: Option<(usize, gpui::Point<gpui::Pixels>)>,
    /// A saved connection the user asked to delete, awaiting confirmation.
    pub(crate) confirm_delete_conn: Option<usize>,
    /// Persisted UI preferences (theme, grid, query, the safety rail) + their store.
    pub(crate) settings: Settings,
    pub(crate) settings_store: Option<FileSettingsStore>,
    pub(crate) settings_open: bool,
    pub(crate) settings_tab: SettingsTab,
    /// Non-fatal problems from the last settings load (an unreadable section, a
    /// bad value), surfaced as a dismissible banner so a hand-edit gets feedback
    /// instead of a silent reset.
    pub(crate) settings_warnings: Vec<String>,
    /// Scroll state of the settings content pane, tracked so a control reached by
    /// Tab can be scrolled into view (see [`Self::update_settings_scroll`]).
    pub(crate) settings_scroll: ScrollHandle,
    /// The reveal-able Appearance control (a dropdown or font-size input) that
    /// currently holds keyboard focus, recomputed each render so the page can tag
    /// it for bounds capture. `None` when no such control is focused.
    pub(crate) settings_focused_reveal: Option<RevealTarget>,
    /// Window-space bounds of the focused reveal control, written by a canvas
    /// overlay during paint and read on the next frame to scroll it into view. The
    /// tag guards against acting on a stale capture in the frame focus moves.
    pub(crate) settings_focus_box: RevealBox,
    /// Whether the OS is in a dark appearance, for `theme = { mode = "system" }`.
    pub(crate) os_dark: bool,
    /// Installed once on first render: keeps the OS-appearance observer alive so
    /// `mode = system` re-themes when the user flips light/dark.
    pub(crate) appearance_sub: Option<gpui::Subscription>,
    /// Live-reload watcher over `settings.toml`, plus the self-write guard that
    /// suppresses the reload our own atomic save would otherwise trigger.
    pub(crate) settings_watcher: Option<crate::settings_watch::SettingsWatcher>,
    /// Store + live-reload watcher for the user keymap (`keymap.toml`). The
    /// overrides themselves live in GPUI's keymap once applied, so we keep only the
    /// store (to re-read on edit) and the watcher; no parsed copy is held here.
    pub(crate) keymap_store: Option<crate::keymap_config::KeymapStore>,
    pub(crate) keymap_watcher: Option<crate::settings_watch::SettingsWatcher>,
    /// Live-reload watcher over `connections.toml`, with the same self-write guard
    /// as the settings watcher so a UI-driven save never echoes back as a reload.
    pub(crate) connections_watcher: Option<crate::settings_watch::SettingsWatcher>,
    /// Non-fatal problems from the last keymap load (a bad keystroke, an unknown
    /// action), shown in the same banner as [`Self::settings_warnings`].
    pub(crate) keymap_warnings: Vec<String>,
    /// The Keymap tab's search box, filtering the bindable-action list by label or
    /// keystroke.
    pub(crate) keymap_search: Entity<TextInput>,
    /// The row currently capturing a chord (index into [`crate::keymap::action_defs`]),
    /// while the recorder's keystroke interceptor is live. `None` when not recording.
    pub(crate) keymap_recording: Option<usize>,
    /// The live keystroke interceptor for the recorder. Held exactly as long as
    /// [`Self::keymap_recording`] is `Some`; dropping it (on capture, cancel, tab
    /// switch, or panel close) ends capture so normal shortcuts resume; a leaked
    /// interceptor would eat every keystroke app-wide.
    pub(crate) keymap_intercept: Option<gpui::Subscription>,
    /// A captured chord awaiting the user's Confirm / Cancel (see [`KeymapCapture`]).
    pub(crate) keymap_capture: Option<KeymapCapture>,
    /// One-shot guard so the appearance observer + file-watcher install on the
    /// first render (when a `Window` exists) rather than on every frame.
    pub(crate) observers_installed: bool,
    /// Built-in + imported themes, resolved for the light/dark pickers and the
    /// theme manager. Rebuilt on import / remove.
    pub(crate) themes: ThemeRegistry,
    /// The five Appearance-panel dropdowns: searchable single-select combo boxes
    /// for the light/dark theme and the three font families. Their options are
    /// (re)filled from the theme registry + warmed font cache by
    /// [`Self::rebuild_settings_pickers`] when the panel opens; each routes its
    /// chosen label straight to the matching setter.
    pub(crate) theme_combo_light: Entity<ComboBox>,
    pub(crate) theme_combo_dark: Entity<ComboBox>,
    pub(crate) font_combo_ui: Entity<ComboBox>,
    pub(crate) font_combo_ui_mono: Entity<ComboBox>,
    pub(crate) font_combo_editor: Entity<ComboBox>,
    /// The connection-form engine picker: a searchable dropdown over
    /// [`DbKind::all`], each option carrying its engine tint dot. Replaces the old
    /// fixed segmented control so the list stays tidy as plugins add drivers. Its
    /// options are static; only the current selection changes, refreshed by
    /// [`Self::refresh_engine_combo`] when a form opens or its engine changes.
    pub(crate) engine_combo: Entity<ComboBox>,
    /// Installed font families, sorted + deduped. Enumerating these hits the OS
    /// text system (a CoreText scan of hundreds of faces), far too slow to do
    /// per render, so the Appearance panel reads this cache. Filled lazily when
    /// the settings panel first opens; fonts don't change during a session.
    pub(crate) font_names_cache: Option<Vec<String>>,
    /// Whether a repaint ticker is already running for the live query timer, so
    /// concurrent opens don't stack duplicate tickers.
    pub(crate) query_ticking: bool,
    /// Monotonic token for the current connect session. Bumped on every connect,
    /// retry, and cancel; a pending backoff timer only fires if its captured
    /// value still matches, so a cancel or manual retry abandons stale timers.
    pub(crate) connect_gen: u64,
    /// Focus anchor for the root view, so the global ⌘K binding dispatches even
    /// when nothing else is focused.
    pub(crate) root_focus: FocusHandle,
    /// Focus anchor handed to the keyboard-driven modals (the confirmations and
    /// the shortcuts overlay) so Flint's `Modal` hears their `Esc`/`Enter`.
    pub(crate) modal_focus: FocusHandle,
    /// Set when such a modal just opened: the next render focuses `modal_focus`.
    pub(crate) focus_modal: bool,
    /// Active while a modal is open: a focus-out listener on `modal_focus` that
    /// pulls focus back inside if Tab would carry it to the backdrop (the focus
    /// trap). Dropped (unsubscribing) when the modal closes.
    pub(crate) modal_focus_trap: Option<gpui::Subscription>,
    /// The command palette overlay, when open, plus the `id → Cmd` map for the
    /// commands it's currently showing (so an activation routes to the right one).
    /// The open command palette / prompt, paired with the `Subscription` to its
    /// events. Bundling the subscription with the entity makes the lifetime
    /// explicit: nulling this `Option` (only via `close_palette`) drops both, so a
    /// missed close can't orphan a detached subscription on `AppState`.
    pub(crate) palette: Option<(Entity<Palette>, gpui::Subscription)>,
    pub(crate) palette_cmds: Vec<(ElementId, Cmd)>,
    /// Which free-text prompt the palette slot is serving (go-to-row vs save), so
    /// a submit routes to the right handler. Only meaningful in prompt mode.
    pub(crate) palette_prompt: PromptKind,
    /// The saved queries shown by the open picker, held only while it's open so an
    /// activation can resolve its index. Loaded on demand, never at startup.
    pub(crate) saved_queries: Vec<crate::queries::SavedQuery>,
    /// The saved conversations shown by the open history picker (M-S5), held only
    /// while it's open so an activation can resolve its index. Loaded on demand.
    pub(crate) loaded_conversations: Vec<crate::conversations::Conversation>,
    /// The persistent query-history log, centralized across all connections and
    /// loaded once at startup. Each entry is connection-scoped; the run-bar
    /// popover filters to the active connection's `conn_id`.
    pub(crate) query_history: crate::history::QueryHistory,
    /// Persisted per-connection Redis keyspace-analysis reports (see
    /// `redis_analysis.rs`), loaded once at startup. Keyed by `conn_id`, so a
    /// saved report survives a restart and is shown when the Analysis panel
    /// reopens on that connection.
    pub(crate) redis_analysis: crate::redis_analysis::AnalysisStore,
    /// Persisted per-connection "recently viewed keys" (see `recent_keys.rs`),
    /// loaded once at startup and seeded into a Redis view when it connects, so
    /// the inspector's browsing history survives a restart.
    pub(crate) redis_recent_keys: crate::recent_keys::RecentKeysStore,
    /// App-managed local state (`state.json`): the last-seen version (for the
    /// update toast) and the per-agent config-selector cache that lets the
    /// assistant show its model/reasoning dropdowns before a chat opens a session.
    pub(crate) local_state: crate::local_state::LocalState,
    /// The connection switcher (⌘P): an always-mounted topbar trigger that opens a
    /// searchable, sectioned popover of the active + recent connections. Its
    /// sections are rebuilt from `connections` + `phase` via [`Self::rebuild_switcher`].
    pub(crate) switcher: Entity<Switcher>,
    /// Warm background connections, kept live so switching back is instant (no
    /// reconnect). The foreground connection lives in `phase` (`Phase::Connected`);
    /// these are the ones the user switched away from. Keyed by their backend
    /// session. An idle one is evicted backend-side after 10 min; its
    /// `Disconnected` event drops it here and demotes it to a plain recent.
    pub(crate) parked: HashMap<SessionId, Box<ActiveConn>>,
    /// The session the window currently shows: the `phase`'s session (connecting
    /// or connected), or `None` on the welcome screen. Mirrored to the backend via
    /// `SetActiveSession` so it's exempt from idle eviction.
    pub(crate) foreground_session: Option<SessionId>,
    /// Monotonic source of `SessionId`s. The UI mints them so it can address a
    /// connection (splash, cancel, retry) before the backend confirms it.
    pub(crate) next_session_id: u64,
    /// Monotonic source of parked-session recency stamps ([`ActiveConn::last_active_seq`]).
    /// Bumped each time a workspace is parked, so LRU eviction can pick the oldest.
    pub(crate) next_active_seq: u64,
    /// Set when an overlay closed: the next render pulls focus back to the root
    /// so the global ⌘K keeps dispatching (see `close_palette`).
    pub(crate) refocus_root: bool,
    /// Armed on mouse-down in the titlebar/drag strip; the first drag motion
    /// then starts a compositor window-move (client-side decorations only; see
    /// `window_chrome::draggable`). A plain click clears it without moving.
    /// Never armed on Windows, which drives window-move through the native caption.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) titlebar_drag: bool,
    /// Whether the keyboard-shortcuts reference overlay (`⌘/`) is showing.
    pub(crate) shortcuts_open: bool,
    /// Whether the "What's New" changelog overlay is showing (Help menu /
    /// `help: what's new` palette command / the post-update toast).
    pub(crate) whats_new_open: bool,
    /// The connection-import wizard while it's open: pick a source (DBeaver/
    /// DBGate), scan, then choose which discovered connections to import. `None`
    /// when no import is in flight (see [`import_ui`]).
    pub(crate) import_wizard: Option<import_ui::ImportWizard>,
    /// Set in [`Self::new`] when this build's version differs from the last one
    /// recorded: the version to announce in a one-shot "RED updated to X" toast,
    /// raised on the first render. `None` on a first-ever launch or an unchanged
    /// version.
    pub(crate) pending_update: Option<SharedString>,
    /// Keyboard-highlighted saved-connection card on the disconnected screen.
    /// Indexes the *visible* (filtered + sorted) list, not `connections`.
    pub(crate) connect_sel: usize,
    /// The welcome screen's connection search box. Filters the saved-connection
    /// list by name / target as the user types (see [`Self::visible_connections`]).
    pub(crate) connect_search: Entity<TextInput>,
    /// The active sort order for the welcome screen's connection list.
    pub(crate) connect_sort: ConnectSort,
    /// A pane to focus on the next render, when the focus move originates from a
    /// place without a `Window` (e.g. an editor `Escape` event). Drained in
    /// `render`, which has the `Window` `focus` needs.
    pub(crate) pending_focus: Option<Pane>,
    /// Set when the connection form just opened: the next render focuses the name
    /// field so the user can type straightaway (the form's `Window`-less opener
    /// can't focus directly).
    pub(crate) focus_name_field: bool,
    /// Set when the history popover just opened: the next render focuses it so its
    /// arrow-key navigation works.
    pub(crate) focus_history: bool,
    /// Set by ⌘F / the search command: the next render reveals the sidebar and
    /// focuses the schema filter field.
    pub(crate) focus_search: bool,
    /// Set when the result filter bar just opened: the next render focuses its
    /// input so the user can type immediately.
    pub(crate) focus_filter: bool,
    /// Set when the find bar just opened: the next render focuses its input so the
    /// user can type immediately.
    pub(crate) focus_find: bool,
    /// Set when an inline cell edit just opened in the inspector (Track B5): the
    /// next render focuses its field so the user types into it at once.
    pub(crate) focus_inspector_edit: bool,
    /// Set by the palette's "switch connection" command: the next render opens
    /// the switcher popover (its `toggle` needs a `Window` the palette lacks).
    pub(crate) open_switcher: bool,
    /// The self-updater's latest state, driving the titlebar pill + About-tab
    /// status line (Phases 3–4 of docs/plans/self-update.md). Updated only by
    /// `Event::UpdateState`; `Unknown` until the first check completes.
    pub(crate) update: UpdateState,
    /// Dev-only perf HUD collector; brackets `render` to read build time and
    /// allocation churn. Compiled only under the `dev-stats` feature.
    #[cfg(feature = "dev-stats")]
    pub(crate) dev_stats: crate::dev_stats::DevStats,
}

/// The GitHub `owner/repo` the self-updater polls (see docs/plans/self-update.md).
pub(crate) const UPDATE_REPO: &str = "vojir-mikulas/red";

/// Where the "report a bug" links point: the project's GitHub issue tracker.
/// Shared by the welcome-screen footer, the About tab, and the Help menu so the
/// three never drift.
pub(crate) const ISSUES_URL: &str = "https://github.com/vojir-mikulas/red/issues";

/// Build the backend's updater config from the persisted settings + this build's
/// version. Used at launch and on each settings reload.
fn update_config(settings: &Settings) -> UpdateConfig {
    UpdateConfig {
        enabled: settings.update.auto_update,
        repo: UPDATE_REPO.to_string(),
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        interval: settings.update.interval(),
    }
}

/// Build the backend's AI config from `[ai]` settings + the keyring-stored API
/// keys. Each configured agent profile (resolved from `[[ai.agents]]`, or the
/// synthesized legacy built-ins) becomes an [`AiAgentProfile`](red_service::AiAgentProfile);
/// for `api`-kind agents the key is read from the OS keychain under `ai-key:<id>`
/// (the `anthropic` built-in additionally falls back to the `ANTHROPIC_API_KEY`
/// env var for first-run/headless setup). An empty key leaves *that* agent off (a
/// turn on it then replies with a clear error). Used at launch and on reload.
pub(crate) fn ai_config(settings: &Settings) -> red_service::AiConfig {
    let agents = settings
        .ai
        .resolved_agents()
        .into_iter()
        .map(|a| {
            let kind = if a.kind.eq_ignore_ascii_case("acp") {
                red_service::AiAgentKind::Acp
            } else {
                red_service::AiAgentKind::Api
            };
            // Only api agents need a key; resolve it per-id, with the env-var
            // fallback scoped to the canonical `anthropic` built-in.
            let api_key = if matches!(kind, red_service::AiAgentKind::Api) {
                resolve_agent_key(&a.id)
            } else {
                String::new()
            };
            red_service::AiAgentProfile {
                id: a.id,
                name: a.name,
                kind,
                command: a.command,
                base_url: a.base_url,
                model: a.model,
                api_key,
            }
        })
        .collect();
    red_service::AiConfig {
        agents,
        default_agent: settings.ai.resolved_default_agent(),
        show_thinking: settings.ai.show_thinking,
        // The global AI access policy (M-S7); a connection's overrides layer over
        // it on the backend. The tier string parses leniently (a typo → `read`).
        enabled: settings.ai.enabled,
        tier: red_service::AiTier::parse(&settings.ai.tier),
        limits: red_service::AiLimits {
            max_rows: settings.ai.limits.max_rows,
            statement_timeout_ms: settings.ai.limits.statement_timeout_ms,
            max_result_bytes: settings.ai.limits.max_result_bytes,
            max_tool_calls: settings.ai.limits.max_tool_calls,
        },
    }
}

/// One configured, usable agent the panel can run a chat on. `is_acp` distinguishes
/// the two backends for UI that differs by kind (the re-auth/switch-account action,
/// the header label) without leaking the protocol enum into the panel.
#[derive(Debug, Clone)]
pub(crate) struct AgentInfo {
    pub id: String,
    pub name: String,
    pub is_acp: bool,
}

/// The state of an in-flight subscription sign-in shown inline in Settings → AI.
/// The pasted code itself lives in the shared `ai_login_code` field; this tracks
/// which agent the flow is for, the authorize URL once the agent CLI prints it, and
/// whether a code has been submitted (so the field/buttons disable while it
/// exchanges). Paste-code OAuth: the user authorizes at `url`, then pastes the code.
#[derive(Debug, Clone)]
pub(crate) struct AiLoginFlow {
    pub agent_id: String,
    /// The browser authorize URL, once known (the agent CLI also opens it itself).
    pub url: Option<String>,
    /// True after a code was submitted; disables the field until it resolves.
    pub submitting: bool,
    /// A failure from a prior submit (wrong/expired code), shown inline.
    pub error: Option<String>,
}

/// The usable agents in config order: an ACP agent is always usable (it owns its
/// auth); an API agent only once it has a key. Drives the panel's selector and the
/// setup-vs-chat gate. Built from [`ai_config`] so it agrees exactly with what the
/// backend was handed.
pub(crate) fn usable_agents(settings: &Settings) -> Vec<AgentInfo> {
    ai_config(settings)
        .agents
        .into_iter()
        .filter(|a| matches!(a.kind, red_service::AiAgentKind::Acp) || !a.api_key.is_empty())
        .map(|a| AgentInfo {
            is_acp: matches!(a.kind, red_service::AiAgentKind::Acp),
            id: a.id,
            name: a.name,
        })
        .collect()
}

/// The API key for an `api`-kind agent profile, read from the OS keychain under
/// `ai-key:<id>`. The canonical `anthropic` built-in additionally falls back to
/// the `ANTHROPIC_API_KEY` env var (first-run / headless convenience); other
/// agents do not, so a local/proxy agent never silently picks up that key.
pub(crate) fn resolve_agent_key(id: &str) -> String {
    crate::secrets::get_ai_key(id)
        .ok()
        .flatten()
        .or_else(|| {
            (id == crate::settings::BUILTIN_API_AGENT)
                .then(|| std::env::var("ANTHROPIC_API_KEY").ok())
                .flatten()
        })
        .unwrap_or_default()
}

impl AppState {
    pub fn new(
        cx: &mut Context<Self>,
        service: ServiceHandle,
        events: UnboundedReceiver<(Option<SessionId>, Event)>,
    ) -> Self {
        // Drain backend events on the foreground executor into `on_event`.
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let mut events = events;
            while let Some((session, event)) = events.next().await {
                if this
                    .update(cx, |state, cx| state.on_event(session, event, cx))
                    .is_err()
                {
                    break; // view dropped, window closed
                }
            }
        })
        .detach();

        // Load persisted preferences and apply the saved theme over the default
        // installed in `main` (a missing/malformed file degrades to defaults; a
        // legacy flat file is migrated and re-saved once into the new sections).
        let settings_store = FileSettingsStore::open_default();
        let report = settings_store
            .as_ref()
            .map(FileSettingsStore::load_report)
            .unwrap_or_default();
        let settings = report.settings;
        if report.migrated {
            if let Some(store) = &settings_store {
                if let Err(e) = store.save(&settings) {
                    tracing::warn!("failed to re-save migrated settings: {e}");
                }
            }
        }
        // Push the backend-side tuning knobs (statement timeout + the driver's
        // fat-cell display cap) so they're in effect before the first connect; the
        // setters and `reload_settings` re-push them when they change.
        service.send_global(Command::SetStatementTimeout(settings.query.timeout()));
        service.send_global(Command::SetDisplayCellCap(settings.grid.max_cell_chars));
        // Configure the AI assistant provider (key from the keyring / env). An
        // empty key leaves it off until one is set.
        service.send_global(Command::ConfigureAi(ai_config(&settings)));
        // Arm the self-updater (Phase 3): an initial check at launch, then on the
        // configured cadence, unless `auto_update = false`, which sends a disabled
        // config so the backend keeps the timer (and network) parked.
        service.send_global(Command::ConfigureUpdates(update_config(&settings)));

        // Load the user keymap and re-apply the full keymap over the defaults `main`
        // installed, so `keymap.toml` overrides take effect before the first paint.
        // Parse warnings and per-binding warnings are merged for the banner.
        let keymap_store = crate::keymap_config::KeymapStore::open_default();
        let keymap_report = keymap_store
            .as_ref()
            .map(crate::keymap_config::KeymapStore::load_report)
            .unwrap_or_default();
        let mut keymap_warnings = keymap_report.warnings;
        keymap_warnings.extend(crate::keymap::apply(cx, &keymap_report.blocks));

        let os_dark = matches!(
            cx.window_appearance(),
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
        );
        let themes = ThemeRegistry::load();
        cx.set_global(crate::theme::with_typography(
            themes.resolve(&settings.appearance.theme, os_dark),
            &settings.appearance,
        ));
        cx.set_global(flint::ReduceMotion(settings.appearance.reduce_motion));

        let name_input = cx.new(|cx| TextInput::new(cx).with_placeholder("my database"));
        let host_input = cx.new(|cx| TextInput::new(cx).with_placeholder("localhost"));
        let port_input = cx.new(TextInput::new);
        let user_input = cx.new(|cx| TextInput::new(cx).with_placeholder("postgres"));
        // Not obscured: the same value is echoed in plaintext in the generated
        // connection-string field right below, so masking it here buys nothing.
        let password_input = cx.new(TextInput::new);
        let database_input = cx.new(|cx| TextInput::new(cx).with_placeholder("analytics_prod"));
        let conn_str_input =
            cx.new(|cx| TextInput::new(cx).with_placeholder("postgres://user:pass@host:5432/db"));

        let ssh_host_input =
            cx.new(|cx| TextInput::new(cx).with_placeholder("bastion.example.com"));
        let ssh_port_input = cx.new(|cx| TextInput::new(cx).with_placeholder("22"));
        let ssh_user_input = cx.new(|cx| TextInput::new(cx).with_placeholder("ubuntu"));
        let ssh_key_path_input =
            cx.new(|cx| TextInput::new(cx).with_placeholder("~/.ssh/id_ed25519"));
        // SSH secrets are obscured; unlike the DB password they're not echoed in
        // the connection-string mirror, so masking them costs nothing.
        let ssh_password_input = cx.new(|cx| TextInput::new(cx).obscured());
        let ssh_passphrase_input = cx.new(|cx| TextInput::new(cx).obscured());

        // Live two-way sync: editing any structured field rebuilds the connection
        // string, and editing the string parses it back into the fields. Only user
        // edits emit `Change`; the programmatic `set_content` used by the sync does
        // not, so the mirror can't echo back into an infinite loop.
        // Field events drive the live two-way mirror, plus form-wide keyboard
        // submit/cancel: Enter (Submit) connects, Esc (Cancel) closes the modal.
        for field in [
            &host_input,
            &port_input,
            &user_input,
            &password_input,
            &database_input,
        ] {
            cx.subscribe(field, |this, _, event: &TextInputEvent, cx| match event {
                TextInputEvent::Change => this.sync_conn_str_from_fields(cx),
                TextInputEvent::Submit => this.submit_form(cx),
                TextInputEvent::Cancel => this.close_form(cx),
                TextInputEvent::Tab
                | TextInputEvent::BackTab
                | TextInputEvent::Up
                | TextInputEvent::Down => {}
            })
            .detach();
        }
        cx.subscribe(
            &conn_str_input,
            |this, _, event: &TextInputEvent, cx| match event {
                TextInputEvent::Change => this.sync_fields_from_conn_str(cx),
                TextInputEvent::Submit => this.submit_form(cx),
                TextInputEvent::Cancel => this.close_form(cx),
                TextInputEvent::Tab
                | TextInputEvent::BackTab
                | TextInputEvent::Up
                | TextInputEvent::Down => {}
            },
        )
        .detach();
        // The name field doesn't mirror, but still submits/cancels the form.
        cx.subscribe(
            &name_input,
            |this, _, event: &TextInputEvent, cx| match event {
                TextInputEvent::Submit => this.submit_form(cx),
                TextInputEvent::Cancel => this.close_form(cx),
                TextInputEvent::Change
                | TextInputEvent::Tab
                | TextInputEvent::BackTab
                | TextInputEvent::Up
                | TextInputEvent::Down => {}
            },
        )
        .detach();

        // Font-size steppers, seeded from the loaded settings. A `Change` (typing,
        // stepping, or Enter) writes straight through to the matching setter, which
        // re-clamps, persists, and re-themes: a live preview as the user edits.
        let font_size_input = |size: f32, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut n = NumberInput::new("font-size", cx)
                    .range(
                        crate::settings::MIN_FONT_SIZE as f64,
                        crate::settings::MAX_FONT_SIZE as f64,
                    )
                    .step(1.0)
                    .decimals(0);
                n.set_value(size as f64, cx);
                n
            })
        };
        let ui_font_size_input = font_size_input(settings.appearance.ui_font_size, cx);
        let editor_font_size_input = font_size_input(settings.editor.font_size, cx);
        cx.subscribe(
            &ui_font_size_input,
            |this, _, event: &NumberInputEvent, cx| {
                let NumberInputEvent::Change(size) = event;
                this.set_ui_font_size(*size as f32, cx);
            },
        )
        .detach();
        cx.subscribe(
            &editor_font_size_input,
            |this, _, event: &NumberInputEvent, cx| {
                let NumberInputEvent::Change(size) = event;
                this.set_editor_font_size(*size as f32, cx);
            },
        )
        .detach();

        let mut connections = config::load();

        // First launch (no connections file yet): seed a read-only "Sample
        // database" so the welcome screen has something to open immediately. We
        // gate on the file's absence, not an empty list, so a user who has
        // deliberately deleted every connection never gets the sample re-added.
        let first_run = config::config_path().is_some_and(|p| !p.exists());
        if first_run && connections.is_empty() {
            if let Some(sample) = crate::sample::first_run_connection() {
                connections.push(sample);
                if let Err(e) = config::save(&connections) {
                    tracing::warn!("could not persist the seeded sample connection: {e}");
                }
            }
        }

        // The welcome screen's connection search box. Bare (the styled toolbar
        // wraps it) and out of the Tab ring (it's a standalone filter, not a form
        // field). A `Change` re-renders the list and resets the keyboard highlight;
        // Enter connects to the highlighted card; Esc clears the query.
        let connect_search = cx.new(|cx| {
            TextInput::new(cx)
                .bare()
                .tab_stop(false)
                .with_placeholder("Search connections…")
        });
        cx.subscribe(
            &connect_search,
            |this, _, event: &TextInputEvent, cx| match event {
                TextInputEvent::Change => {
                    this.connect_sel = 0;
                    cx.notify();
                }
                TextInputEvent::Submit => {
                    let visible = this.visible_connections(cx);
                    if let Some(&ix) =
                        visible.get(this.connect_sel.min(visible.len().saturating_sub(1)))
                    {
                        this.connect(ix, cx);
                    }
                }
                TextInputEvent::Cancel => {
                    this.connect_search
                        .update(cx, |i, cx| i.set_content("", cx));
                    this.connect_sel = 0;
                    cx.notify();
                }
                TextInputEvent::Tab
                | TextInputEvent::BackTab
                | TextInputEvent::Up
                | TextInputEvent::Down => {}
            },
        )
        .detach();

        // The Keymap settings tab's search box. Bare (the row wraps it) and out of
        // the Tab ring; a `Change` just re-filters the action list.
        let keymap_search = cx.new(|cx| {
            TextInput::new(cx)
                .bare()
                .tab_stop(false)
                .with_placeholder("Search actions or shortcuts…")
        });
        cx.subscribe(
            &keymap_search,
            |this, _, event: &TextInputEvent, cx| match event {
                TextInputEvent::Change => cx.notify(),
                TextInputEvent::Cancel => {
                    this.keymap_search.update(cx, |i, cx| i.set_content("", cx));
                    cx.notify();
                }
                TextInputEvent::Submit
                | TextInputEvent::Tab
                | TextInputEvent::BackTab
                | TextInputEvent::Up
                | TextInputEvent::Down => {}
            },
        )
        .detach();

        // Shared obscured field for the Settings → AI agents key rows. Enter saves
        // the key for the row currently being edited; Esc closes the row.
        let ai_key_input = cx.new(|cx| TextInput::new(cx).obscured().with_placeholder("sk-ant-…"));
        cx.subscribe(
            &ai_key_input,
            |this, _, event: &TextInputEvent, cx| match event {
                TextInputEvent::Submit => this.save_agent_key(cx),
                TextInputEvent::Cancel => this.cancel_agent_key(cx),
                _ => {}
            },
        )
        .detach();

        // Shared field for the pasted OAuth code during an ACP subscription sign-in.
        // Enter submits the code; Esc cancels the sign-in.
        let ai_login_code =
            cx.new(|cx| TextInput::new(cx).with_placeholder("paste the code from your browser"));
        cx.subscribe(
            &ai_login_code,
            |this, _, event: &TextInputEvent, cx| match event {
                TextInputEvent::Submit => this.submit_login_code(cx),
                TextInputEvent::Cancel => this.cancel_login(cx),
                _ => {}
            },
        )
        .detach();

        // The connection switcher (⌘P). Seed its sections off the just-loaded
        // connections; `rebuild_switcher` refreshes them on every connect/disconnect.
        let switcher = cx.new(|cx| {
            let mut s = Switcher::new("connection-switcher", cx);
            s.set_placeholder("Search connections…", cx);
            // Match the topbar's other bordered controls (e.g. Disconnect), which
            // use `theme.border` rather than the switcher's default soft hairline.
            s.set_trigger_border(TriggerBorder::Normal, cx);
            // Lucide disclosure glyph, re-themed each render; muted to match the
            // topbar trigger's subtle look.
            s.set_chevron(
                |app| {
                    crate::icons::icon("chevron-down", app.theme().scale(14.), app.theme().text_dim)
                        .into_any_element()
                },
                cx,
            );
            let (label, dot, sections) = switcher_sections(
                &connections,
                &Phase::Disconnected,
                &HashMap::new(),
                cx.theme(),
            );
            s.set_trigger(label, dot, cx);
            s.set_sections(sections, cx);
            s.set_footer(switcher_footer(false), cx);
            s
        });
        cx.subscribe(&switcher, Self::on_switcher_event).detach();

        // The five Appearance-panel dropdowns (searchable combo boxes). They start
        // empty: their options are filled lazily by `rebuild_settings_pickers` when
        // the panel first opens; the installed-font list is a slow OS scan we keep
        // off the startup path. Each routes its chosen label to its setter.
        let new_combo = |cx: &mut Context<Self>, id: &'static str, search: &'static str| {
            cx.new(|cx| {
                let mut c = ComboBox::new(id, cx);
                c.set_search_placeholder(search, cx);
                // Lucide disclosure + check glyphs, re-themed each render from the
                // current app theme (accent colour, size scaled to the font ramp).
                c.set_chevron(
                    |app| {
                        crate::icons::icon(
                            "chevron-down",
                            app.theme().scale(14.),
                            app.theme().text_dim,
                        )
                        .into_any_element()
                    },
                    cx,
                );
                c.set_check(
                    |app| {
                        crate::icons::icon("check", app.theme().scale(13.), app.theme().accent)
                            .into_any_element()
                    },
                    cx,
                );
                c
            })
        };
        let theme_combo_light = new_combo(cx, "pick-light-theme", "Search themes…");
        let theme_combo_dark = new_combo(cx, "pick-dark-theme", "Search themes…");
        let font_combo_ui = new_combo(cx, "pick-ui-font", "Search fonts…");
        let font_combo_ui_mono = new_combo(cx, "pick-ui-mono-font", "Search fonts…");
        let font_combo_editor = new_combo(cx, "pick-editor-font", "Search fonts…");
        theme_combo_light.update(cx, |c, cx| c.set_placeholder("Select a theme…", cx));
        theme_combo_dark.update(cx, |c, cx| c.set_placeholder("Select a theme…", cx));
        for combo in [&font_combo_ui, &font_combo_ui_mono, &font_combo_editor] {
            combo.update(cx, |c, cx| c.set_placeholder("Select a font…", cx));
        }
        cx.subscribe(&theme_combo_light, |this, _, e: &ComboBoxEvent, cx| {
            if let ComboBoxEvent::Select(name) = e {
                this.set_light_theme(name.as_ref(), cx);
            }
        })
        .detach();
        cx.subscribe(&theme_combo_dark, |this, _, e: &ComboBoxEvent, cx| {
            if let ComboBoxEvent::Select(name) = e {
                this.set_dark_theme(name.as_ref(), cx);
            }
        })
        .detach();
        cx.subscribe(&font_combo_ui, |this, _, e: &ComboBoxEvent, cx| {
            if let ComboBoxEvent::Select(name) = e {
                this.set_ui_font_family(name.as_ref(), cx);
            }
        })
        .detach();
        cx.subscribe(&font_combo_ui_mono, |this, _, e: &ComboBoxEvent, cx| {
            if let ComboBoxEvent::Select(name) = e {
                this.set_ui_mono_family(name.as_ref(), cx);
            }
        })
        .detach();
        cx.subscribe(&font_combo_editor, |this, _, e: &ComboBoxEvent, cx| {
            if let ComboBoxEvent::Select(name) = e {
                this.set_editor_font_family(name.as_ref(), cx);
            }
        })
        .detach();

        // The connection-form engine dropdown. Unlike the appearance combos its
        // option list is static (every `DbKind`), so it's filled once here; each
        // row carries the engine's tint dot via `set_leading`, keyed by the option
        // index — which matches `DbKind::all` order. Full-width so it lines up with
        // the form's other inputs. `refresh_engine_combo` re-selects the current
        // engine when a form opens or its engine changes (e.g. a pasted DSN).
        let engine_combo = new_combo(cx, "pick-engine", "Search engines…");
        engine_combo.update(cx, |c, cx| {
            c.set_placeholder("Select an engine…", cx);
            c.set_full_width(true, cx);
            c.set_leading(
                |ix, app| {
                    let kind = red_core::DbKind::all().get(ix).copied().unwrap_or_default();
                    crate::connect::engine_dot(kind, app.theme()).into_any_element()
                },
                cx,
            );
            c.set_options(crate::connect::engine_combo_options(), None, cx);
        });
        cx.subscribe(&engine_combo, |this, _, e: &ComboBoxEvent, cx| {
            if let ComboBoxEvent::Select(name) = e {
                if let Some(kind) = red_core::DbKind::all()
                    .iter()
                    .copied()
                    .find(|k| k.to_string() == name.as_ref())
                {
                    this.set_form_kind(kind, cx);
                }
            }
        })
        .detach();

        // One-shot "updated to X" announcement: compare this build's version to
        // the last one we recorded. A first-ever launch records silently (there's
        // no prior version to have updated *from*); a changed version is remembered
        // so the first render can raise a toast. Either way we mark the current
        // version seen now, so the toast fires exactly once per update.
        let mut local_state = crate::local_state::LocalState::load();
        let current_version = crate::changelog::VERSION;
        let is_update = local_state
            .last_seen()
            .is_some_and(|seen| seen != current_version);
        local_state.mark_seen(current_version);
        let pending_update = is_update.then(|| SharedString::from(current_version));
        // Kept in the struct so the assistant can persist each agent's advertised
        // config selectors as they arrive (used to pre-fill the model dropdowns).

        Self {
            service,
            connections,
            phase: Phase::Disconnected,
            name_input,
            host_input,
            port_input,
            user_input,
            password_input,
            database_input,
            conn_str_input,
            ssh_host_input,
            ssh_port_input,
            ssh_user_input,
            ssh_key_path_input,
            ssh_password_input,
            ssh_passphrase_input,
            ui_font_size_input,
            editor_font_size_input,
            form: None,
            notifications: Vec::new(),
            next_notification_id: 0,
            next_export_id: 0,
            next_copy_id: 0,
            pending_copy: None,
            pending_copy_target: None,
            copy_targets: Vec::new(),
            copy_new_namespaces: Vec::new(),
            pending_copy_new: None,
            migrate_targets: Vec::new(),
            pending_migrate: None,
            pending_fk: None,
            inspector: None,
            assistant: None,
            focus_assistant: false,
            focus_rename: false,
            assistant_w: px(380.),
            assistant_drag: None,
            usable_agents: usable_agents(&settings),
            ai_configured: !usable_agents(&settings).is_empty(),
            ai_key_input,
            ai_key_editing: None,
            focus_ai_key: false,
            ai_auth: HashMap::new(),
            ai_login: None,
            ai_login_code,
            focus_login_code: false,
            next_conversation_id: 0,
            stats_bar: false,
            filter_bar: None,
            find_bar: None,
            autoscroll: None,
            autoscroll_epoch: 0,
            cell_menu: None,
            export_menu: None,
            more_menu: None,
            confirm_exec: None,
            pending_import: None,
            grid_edit: None,
            cell_suggest: None,
            cell_suggest_bounds: cx.new(|_| None),
            focus_grid_edit: false,
            grid_edit_blur: None,
            confirm_close_tab: None,
            confirm_kv_delete: None,
            confirm_close_batch: None,
            tab_context_menu: None,
            confirm_delete_conn: None,
            settings,
            settings_store,
            settings_open: false,
            settings_tab: SettingsTab::Appearance,
            settings_warnings: report.warnings,
            settings_scroll: ScrollHandle::new(),
            settings_focused_reveal: None,
            settings_focus_box: Default::default(),
            os_dark,
            appearance_sub: None,
            settings_watcher: None,
            keymap_store,
            keymap_watcher: None,
            connections_watcher: None,
            keymap_warnings,
            keymap_search,
            keymap_recording: None,
            keymap_intercept: None,
            keymap_capture: None,
            observers_installed: false,
            themes,
            theme_combo_light,
            theme_combo_dark,
            font_combo_ui,
            font_combo_ui_mono,
            font_combo_editor,
            engine_combo,
            font_names_cache: None,
            query_ticking: false,
            connect_gen: 0,
            root_focus: cx.focus_handle(),
            modal_focus: cx.focus_handle(),
            focus_modal: false,
            modal_focus_trap: None,
            palette: None,
            palette_cmds: Vec::new(),
            palette_prompt: PromptKind::GoToRow,
            saved_queries: Vec::new(),
            loaded_conversations: Vec::new(),
            query_history: crate::history::QueryHistory::load(),
            redis_analysis: crate::redis_analysis::AnalysisStore::load(),
            redis_recent_keys: crate::recent_keys::RecentKeysStore::load(),
            local_state,
            switcher,
            parked: HashMap::new(),
            foreground_session: None,
            next_session_id: 0,
            next_active_seq: 0,
            // Focus the root on first paint so the very first ⌘K dispatches.
            refocus_root: true,
            titlebar_drag: false,
            shortcuts_open: false,
            whats_new_open: false,
            import_wizard: None,
            pending_update,
            connect_sel: 0,
            connect_search,
            connect_sort: ConnectSort {
                field: ConnectSortField::Recent,
                ascending: false,
            },
            pending_focus: None,
            focus_name_field: false,
            focus_history: false,
            focus_search: false,
            focus_filter: false,
            focus_find: false,
            focus_inspector_edit: false,
            open_switcher: false,
            update: UpdateState::Unknown,
            #[cfg(feature = "dev-stats")]
            dev_stats: crate::dev_stats::DevStats::default(),
        }
    }

    /// Toggle the dev perf HUD overlay (the `cmd-alt-p` dev keybinding).
    #[cfg(feature = "dev-stats")]
    pub(crate) fn toggle_dev_stats(&mut self, cx: &mut Context<Self>) {
        self.dev_stats.toggle();
        cx.notify();
    }

    /// True while any open result grid is still running its query.
    fn any_query_running(&self) -> bool {
        matches!(&self.phase, Phase::Connected(active)
            if active
                .tabs
                .iter()
                .any(|t| t.result.as_ref().is_some_and(|g| !g.is_ready())))
    }

    /// Drive ~10 Hz repaints while a query runs so the live timer counts up.
    /// Self-terminating: the loop stops once no grid is running, and the guard
    /// prevents a second ticker stacking on top of a live one.
    pub(crate) fn start_query_ticker(&mut self, cx: &mut Context<Self>) {
        if self.query_ticking || !self.any_query_running() {
            return;
        }
        self.query_ticking = true;
        cx.spawn(
            async move |this: WeakEntity<Self>, cx: &mut AsyncApp| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let running = this.update(cx, |this, cx| {
                    let running = this.any_query_running();
                    if running {
                        cx.notify();
                    } else {
                        this.query_ticking = false;
                    }
                    running
                });
                if !matches!(running, Ok(true)) {
                    break;
                }
            },
        )
        .detach();
    }

    // --- notifications ---

    /// Push a notification onto the bottom-right stack and return its id. Info /
    /// success toasts auto-dismiss after [`TOAST_AUTO_DISMISS`]; warnings and
    /// errors persist until closed.
    pub(crate) fn notify(
        &mut self,
        variant: ToastVariant,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> u64 {
        let auto_dismiss = match variant {
            ToastVariant::Info | ToastVariant::Success => Some(TOAST_AUTO_DISMISS),
            ToastVariant::Warning | ToastVariant::Error => None,
        };
        self.push_notification(
            Notification {
                id: 0,
                variant,
                message: message.into(),
                detail: None,
                detail_label: None,
                auto_dismiss,
                export: None,
                expanded: false,
                hovered: false,
                dismiss_gen: 0,
                action: None,
            },
            cx,
        )
    }

    /// Like [`notify`](Self::notify), but with a secondary `detail` body. The
    /// detail becomes a selectable, copyable, collapsible block. Use it for the
    /// long, copy-worthy text (a query error, a driver message) while `title`
    /// stays a short headline.
    pub(crate) fn notify_detail(
        &mut self,
        variant: ToastVariant,
        title: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> u64 {
        let auto_dismiss = match variant {
            ToastVariant::Info | ToastVariant::Success => Some(TOAST_AUTO_DISMISS),
            ToastVariant::Warning | ToastVariant::Error => None,
        };
        self.push_notification(
            Notification {
                id: 0,
                variant,
                message: title.into(),
                detail: Some(detail.into()),
                detail_label: None,
                auto_dismiss,
                export: None,
                expanded: false,
                hovered: false,
                dismiss_gen: 0,
                action: None,
            },
            cx,
        )
    }

    /// Assign `notification` a fresh id, push it, and, for a transient toast,
    /// arm a `cx.spawn` timer that removes it by id once `auto_dismiss` elapses.
    /// Returns the assigned id so callers (the export toast) can update it later.
    pub(crate) fn push_notification(
        &mut self,
        mut notification: Notification,
        cx: &mut Context<Self>,
    ) -> u64 {
        let id = self.next_notification_id;
        self.next_notification_id += 1;
        notification.id = id;
        // Build the selectable view of the detail body once, up front, so the
        // renderer just clones the handle each frame.
        if notification.detail_label.is_none() {
            if let Some(detail) = notification.detail.clone() {
                notification.detail_label = Some(cx.new(|cx| SelectableLabel::new(detail, cx)));
            }
        }
        let auto_dismiss = notification.auto_dismiss;
        self.notifications.push(notification);
        // Persistent (error / warning) toasts are removed only by a user click, so
        // a burst of query errors could pile up unbounded. Cap the stack: drop the
        // oldest persistent, non-export toast first (transient ones self-dismiss;
        // an export toast owns live cancel state, so it's never auto-dropped).
        while self.notifications.len() > MAX_NOTIFICATIONS {
            let Some(stale) = self
                .notifications
                .iter()
                .position(|n| n.auto_dismiss.is_none() && n.export.is_none())
            else {
                break;
            };
            self.notifications.remove(stale);
        }
        if let Some(delay) = auto_dismiss {
            self.arm_dismiss(id, delay, cx);
        }
        cx.notify();
        id
    }

    /// Arm (or re-arm) the auto-dismiss timer for a transient toast. Bumping the
    /// notification's `dismiss_gen` invalidates any timer already in flight, so a
    /// hover-driven re-arm can't be undone by a stale one; the timer also no-ops
    /// if the toast is hovered when it fires (the un-hover will re-arm it).
    fn arm_dismiss(&mut self, id: u64, delay: Duration, cx: &mut Context<Self>) {
        let Some(notification) = self.notifications.iter_mut().find(|n| n.id == id) else {
            return;
        };
        notification.dismiss_gen += 1;
        let generation = notification.dismiss_gen;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                let still_armed = this
                    .notifications
                    .iter()
                    .find(|n| n.id == id)
                    .is_some_and(|n| n.dismiss_gen == generation && !n.hovered);
                if still_armed {
                    this.dismiss(id, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Pause/resume a transient toast's auto-dismiss as the pointer enters/leaves,
    /// so a message can be read, selected and copied without it vanishing.
    pub(crate) fn set_notification_hovered(
        &mut self,
        id: u64,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let re_arm = {
            let Some(notification) = self.notifications.iter_mut().find(|n| n.id == id) else {
                return;
            };
            if notification.hovered == hovered {
                return;
            }
            notification.hovered = hovered;
            // Leaving a transient toast restarts its full dwell timer.
            (!hovered).then_some(notification.auto_dismiss).flatten()
        };
        if let Some(delay) = re_arm {
            self.arm_dismiss(id, delay, cx);
        }
        cx.notify();
    }

    /// Flip the expand/collapse state of a toast with a long body.
    pub(crate) fn toggle_notification_expanded(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(notification) = self.notifications.iter_mut().find(|n| n.id == id) {
            notification.expanded = !notification.expanded;
            cx.notify();
        }
    }

    /// Remove the notification with `id` (its close button, or a fired timer).
    pub(crate) fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        self.notifications.retain(|n| n.id != id);
        cx.notify();
    }

    /// The notification's `✕`: dismiss a plain toast, or, for the export toast,
    /// abort the backend stream. The toast stays (now "Cancelling…") until the
    /// `ExportCancelled` event swaps it for a transient one.
    pub(crate) fn close_notification(&mut self, id: u64, cx: &mut Context<Self>) {
        let transfer = self
            .notifications
            .iter()
            .find(|n| n.id == id)
            .and_then(|n| n.export.as_ref())
            .map(|e| (e.id, e.kind));
        match transfer {
            Some((transfer_id, kind)) => {
                let (cancel, msg) = match kind {
                    TransferKind::Import => (
                        Command::CancelImport { id: transfer_id },
                        "Cancelling import…",
                    ),
                    TransferKind::Export => (
                        Command::CancelExport { id: transfer_id },
                        "Cancelling export…",
                    ),
                    TransferKind::Copy => {
                        (Command::CancelCopy { id: transfer_id }, "Cancelling copy…")
                    }
                    TransferKind::Migrate => (
                        Command::CancelCopy { id: transfer_id },
                        "Cancelling migration…",
                    ),
                };
                self.send_active(cancel);
                if let Some(n) = self.notifications.iter_mut().find(|n| n.id == id) {
                    n.message = msg.into();
                }
                cx.notify();
            }
            None => self.dismiss(id, cx),
        }
    }

    /// The single point where backend events drive UI state. `session` is the
    /// workspace the event belongs to (`None` for the session-less probe replies).
    fn on_event(&mut self, session: Option<SessionId>, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Connected { version } => self.on_connected(session, version, cx),
            Event::Disconnected => self.on_disconnected(session, cx),
            Event::TestSucceeded { version } => {
                // Clear the in-flight state (footer button back to "Test
                // connection") and report the result as a self-dismissing toast.
                if let Some(form) = &mut self.form {
                    form.test = TestState::Idle;
                }
                self.notify(
                    ToastVariant::Success,
                    format!("Connection successful · {version}"),
                    cx,
                );
            }
            Event::TestFailed { message } => {
                if let Some(form) = &mut self.form {
                    form.test = TestState::Idle;
                }
                // Engine errors can be long, so use the detail variant (copyable,
                // collapsible) and let it persist until dismissed.
                self.notify_detail(ToastVariant::Error, "Connection failed", message, cx);
            }
            Event::ConnectFailed { message, fatal } => {
                tracing::error!(?session, fatal, "{message}");
                // Only act if this reply is for the connect still on screen; a stale
                // reply from a superseded/cancelled attempt is ignored.
                if matches!(&self.phase, Phase::Connecting(c) if Some(c.session) == session) {
                    self.on_connect_failed(message, fatal, cx);
                }
            }
            Event::SshHostUnknown {
                host,
                port,
                fingerprint,
                key,
            } => {
                tracing::warn!(?session, %host, "unknown SSH host key ({fingerprint})");
                if matches!(&self.phase, Phase::Connecting(c) if Some(c.session) == session) {
                    self.on_ssh_host_unknown(host, port, fingerprint, key, cx);
                }
            }
            Event::Error(message) => {
                // Log the full text to stderr (RUST_LOG) too. The toast carries it
                // as a selectable, expandable detail body, so a long backend error
                // can be read in full, highlighted and copied straight from it.
                tracing::error!(?session, "{message}");
                self.on_result_error(session, &message);
                self.notify_detail(ToastVariant::Error, "Error", message, cx);
            }

            // --- schema explorer ---
            Event::ObjectsLoaded { schemas } => {
                if let Some(active) = self.conn_mut(session) {
                    active.schema.apply_objects(schemas);
                }
                // Completions / prefetch only matter for the on-screen connection.
                if session == self.foreground_session {
                    self.prefetch_table_details();
                    self.refresh_completions(cx);
                }
            }
            Event::TableDescribed {
                schema,
                table,
                detail,
            } => {
                if let Some(active) = self.conn_mut(session) {
                    active.schema.details.insert((schema, table), detail);
                }
                if session == self.foreground_session {
                    self.refresh_completions(cx);
                    // Repaint views that read the catalog (the schema tree and the
                    // Columns panel's lazily-expanded FK nodes) so a freshly-arrived
                    // description renders without waiting for an unrelated frame.
                    cx.notify();
                }
            }
            Event::ForeignKeysLoaded { graph } => {
                // Cache the graph and (re)mark FK columns on any already-open grids,
                // a result may have opened before the prefetch landed.
                if let Some(active) = self.conn_mut(session) {
                    active.fk_graph = graph;
                    for tab in &mut active.tabs {
                        if let Some(grid) = tab.result.as_mut() {
                            grid.set_fk_cols(&active.fk_graph);
                            // A browse may carry an expansion from before the graph
                            // landed; (re)resolve its joins now they're available.
                            grid.rebuild_joins(&active.fk_graph);
                        }
                    }
                }
                cx.notify();
            }

            // --- Redis keyspace browser (R1, see docs/plans/redis.md) ---
            Event::KvScanPage { epoch, page } => {
                self.on_kv_scan_page(session, epoch, page, cx);
            }
            Event::KvDbSizeReady { epoch, count } => {
                self.on_kv_db_size(session, epoch, count, cx);
            }
            Event::KvKeyProbed { .. } => {
                // Exact-key jump has no UI trigger yet (deferred past this R1
                // slice); the command/event round-trip is wired and tested
                // end to end at the driver layer, just not surfaced here.
            }
            Event::KvValueReady { key, value, .. } => {
                self.on_kv_value_ready(session, key, value, cx);
            }
            Event::KvValueError { key, message, .. } => {
                self.on_kv_value_error(session, key, message, cx);
            }
            Event::KvCollectionPageReady { key, page, .. } => {
                self.on_kv_collection_page_ready(session, key, page, cx);
            }
            Event::KvListWindowReady { key, values, .. } => {
                self.on_kv_list_window_ready(session, key, values, cx);
            }
            Event::KvStreamPageReady { key, page, .. } => {
                self.on_kv_stream_page_ready(session, key, page, cx);
            }
            Event::KvStreamGroupsReady { key, groups, .. } => {
                self.on_kv_stream_groups_ready(session, key, groups, cx);
            }
            Event::KvStreamConsumersReady {
                key,
                group,
                consumers,
                ..
            } => {
                self.on_kv_stream_consumers_ready(session, key, group, consumers, cx);
            }
            Event::KvStreamPendingReady {
                key,
                group,
                pending,
                ..
            } => {
                self.on_kv_stream_pending_ready(session, key, group, pending, cx);
            }
            Event::KvStreamActionDone {
                key,
                group,
                action,
                count,
                ..
            } => {
                self.on_kv_stream_action_done(session, key, group, action, count, cx);
            }
            Event::KvCommandResult {
                epoch,
                argv,
                result,
            } => {
                self.on_kv_command_result(session, epoch, argv, result, cx);
            }
            Event::KvEditApplied { epoch, edit } => {
                self.on_kv_edit_applied(session, epoch, edit, cx);
            }
            Event::KvMessage {
                epoch,
                channel,
                payload,
            } => {
                self.on_kv_message(session, epoch, channel, payload, cx);
            }
            Event::KvSlowlogReady { epoch, entries } => {
                self.on_kv_slowlog_ready(session, epoch, entries, cx);
            }
            Event::KvMonitorLine { epoch, line } => {
                self.on_kv_monitor_line(session, epoch, line, cx);
            }
            Event::KvClientListReady { epoch, clients } => {
                self.on_kv_client_list_ready(session, epoch, clients, cx);
            }
            Event::KvNotifyConfigReady { epoch, value } => {
                self.on_kv_notify_config_ready(session, epoch, value, cx);
            }

            // --- result grid ---
            Event::ResultReady {
                columns,
                total,
                epoch,
                key,
            } => self.on_result_ready(session, columns, total, epoch, key, cx),
            Event::ResultPageLoaded {
                offset,
                rows,
                epoch,
            } => self.on_result_page(session, offset, rows, epoch, cx),
            // Keyset runs: extend/relocate a grid's resident row run.
            Event::ResultRunLoaded {
                epoch,
                fetch,
                rows,
                estimated,
                seq,
            } => self.on_result_run(session, epoch, fetch, rows, estimated, seq, cx),
            Event::ResultRunFailed { epoch, seq } => self.on_result_run_failed(session, epoch, seq),
            Event::CopyRowsLoaded { id, rows } => self.on_copy_rows(id, rows, cx),
            // Column-stats bar (pushed-down aggregate summary).
            Event::ColumnStatsReady {
                epoch,
                column,
                stats,
            } => self.on_column_stats(session, epoch, column, stats, cx),
            Event::ColumnStatsFailed { epoch, column } => {
                self.on_column_stats_failed(session, epoch, column, cx)
            }
            Event::LookupReady {
                epoch,
                target,
                rows,
            } => self.on_lookup_ready(session, epoch, target, rows, cx),
            Event::LookupFailed { epoch, target } => self.on_lookup_failed(epoch, target, cx),
            Event::EnumsLoaded { table, columns } => {
                self.on_enums_loaded(session, table, columns, cx)
            }

            // --- export & writes ---
            Event::Executed { affected } => {
                self.notify(
                    ToastVariant::Success,
                    format!("{affected} row(s) affected"),
                    cx,
                );
                // A write may have changed the schema (CREATE/DROP); refresh the
                // tree of the session that ran it.
                if let Some(id) = session {
                    self.service.send_to(id, Command::LoadObjects);
                }
            }
            Event::ExportProgress { id, rows } => self.on_export_progress(id, rows, cx),
            Event::ExportFinished { id, path, rows } => self.on_export_finished(id, path, rows, cx),
            Event::ExportCancelled { id } => self.on_export_cancelled(id, cx),

            // --- data import (Track: data import) ---
            Event::ImportProgress { id, rows } => self.on_import_progress(id, rows, cx),
            Event::ImportFinished { id, rows } => self.on_import_finished(id, rows, cx),
            Event::ImportFailed { id, rows, message } => {
                self.on_import_failed(id, rows, message, cx)
            }
            Event::ImportCancelled { id, rows } => self.on_import_cancelled(id, rows, cx),
            Event::ImportColumns { id, columns } => self.on_import_columns(id, columns, cx),

            // --- table copy (result → another table) ---
            Event::CopyTargetColumns { id, columns } => {
                self.on_copy_target_columns(id, columns, cx)
            }
            Event::CopyProgress { id, rows } => self.on_copy_progress(id, rows, cx),
            Event::CopyFinished { id, rows } => self.on_copy_finished(id, rows, cx),
            Event::CopyFailed { id, rows, message } => self.on_copy_failed(id, rows, message, cx),
            Event::CopyCancelled { id, rows } => self.on_copy_cancelled(id, rows, cx),

            // --- query plan (Track B4) ---
            Event::PlanReady { epoch, plan } => self.on_plan_ready(session, epoch, plan),
            Event::PlanFailed { epoch, message } => self.on_plan_failed(session, epoch, message),

            // --- staged grid edits (Track B6) ---
            Event::BatchApplied { epoch, applied } => self.on_batch_applied(epoch, applied, cx),
            Event::BatchFailed { epoch, message, .. } => self.on_batch_failed(epoch, message, cx),

            // --- self-update (Phases 3–4) ---
            Event::UpdateState(state) => self.on_update_state(state, cx),

            // --- AI assistant ---
            Event::AiDelta {
                conversation_id,
                delta,
            } => self.on_ai_delta(conversation_id, delta, cx),
            Event::AiTurnFinished {
                conversation_id,
                usage,
            } => self.on_ai_finished(conversation_id, usage, cx),
            Event::AiError {
                conversation_id,
                message,
            } => self.on_ai_error(conversation_id, message, cx),
            Event::AiPermissionRequest {
                conversation_id,
                request_id,
                title,
                detail,
            } => self.on_ai_permission_request(conversation_id, request_id, title, detail, cx),
            Event::AiReportReady {
                conversation_id,
                path,
                title,
            } => self.on_ai_report_ready(conversation_id, path, title, cx),
            Event::AiOpenQuery {
                conversation_id,
                sql,
            } => self.on_ai_open_query(conversation_id, sql, cx),
            Event::AiSaveQuery {
                conversation_id,
                name,
                description,
                sql,
            } => self.on_ai_save_query(conversation_id, name, description, sql, cx),
            Event::AiCommandsAvailable {
                conversation_id,
                commands,
            } => self.on_ai_commands_available(conversation_id, commands, cx),
            Event::AiConfigOptionsAvailable {
                conversation_id,
                options,
            } => self.on_ai_config_options_available(conversation_id, options, cx),
            Event::AiLoginPrompt { agent_id, url } => self.on_ai_login_prompt(agent_id, url, cx),
            Event::AiLoginFinished {
                agent_id,
                ok,
                message,
            } => self.on_ai_login_finished(agent_id, ok, message, cx),
            Event::AiAgentAuthStatus { agent_id, status } => {
                self.on_ai_agent_auth_status(agent_id, status, cx)
            }

            // The streaming `Query`/`FetchMore` path stays in the protocol for
            // headless use + tests; the UI now drives results via `OpenResult`.
            Event::QueryStarted { .. }
            | Event::QueryRows(_)
            | Event::QueryFinished { .. }
            | Event::QueryCancelled => {}
        }
        cx.notify();
    }

    /// Show or hide the schema sidebar (toggled from the status-bar control).
    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(a) = &mut self.phase {
            a.sidebar_collapsed = !a.sidebar_collapsed;
            cx.notify();
        }
    }

    /// Run the pending write the user confirmed: a destructive editor statement
    /// or a guarded grid edit (Track B5).
    pub(crate) fn confirm_destructive(&mut self, cx: &mut Context<Self>) {
        match self.confirm_exec.take() {
            Some(PendingWrite::EditorSql(sql)) => self.execute_sql(sql, cx),
            Some(PendingWrite::Batch { ops, epoch }) => {
                self.send_active(Command::ApplyBatch { epoch, ops });
            }
            Some(PendingWrite::Import {
                path,
                format,
                target,
                mapping,
                id,
                ..
            }) => self.start_import(path, format, target, mapping, id, cx),
            Some(PendingWrite::Copy {
                id,
                source_epoch,
                target,
                target_session,
                mapping,
                mode,
                create,
                ..
            }) => self.start_copy(
                id,
                source_epoch,
                target,
                target_session,
                mapping,
                mode,
                create,
                cx,
            ),
            None => {}
        }
        // The modal is closing; return focus to the root for the next ⌘K etc.
        self.refocus_root = true;
        cx.notify();
    }

    /// Confirm a pending copy with an explicit `mode`, the copy dialog's two action
    /// buttons (Append / Replace all). Overrides the stored mode so "Replace all"
    /// truncates first; "Append" keeps the target's rows.
    pub(crate) fn confirm_copy(&mut self, mode: CopyMode, cx: &mut Context<Self>) {
        if let Some(PendingWrite::Copy {
            id,
            source_epoch,
            target,
            target_session,
            mapping,
            create,
            ..
        }) = self.confirm_exec.take()
        {
            self.start_copy(
                id,
                source_epoch,
                target,
                target_session,
                mapping,
                mode,
                create,
                cx,
            );
        }
        self.refocus_root = true;
        cx.notify();
    }

    pub(crate) fn cancel_destructive(&mut self, cx: &mut Context<Self>) {
        // Cancelling the submit preview keeps the staged change-set intact (it lives
        // on the result); only the confirm is dropped.
        self.confirm_exec = None;
        self.refocus_root = true;
        cx.notify();
    }

    /// Whether any modal that should trap focus is currently open. Drives the
    /// focus-trap subscription in `render`.
    pub(crate) fn any_modal_open(&self) -> bool {
        self.confirm_exec.is_some()
            || self.confirm_close_tab.is_some()
            || self.confirm_kv_delete.is_some()
            || self.confirm_close_batch.is_some()
            || self.confirm_delete_conn.is_some()
            || self.shortcuts_open
            || self.whats_new_open
            || self.settings_open
            || self.form.is_some()
            || self.import_wizard.is_some()
    }

    /// Open or close the keyboard-shortcuts overlay (`⌘/` / palette command).
    /// Opening focuses the modal so its Esc-to-close is heard; closing returns
    /// focus to the root.
    pub(crate) fn toggle_shortcuts(&mut self, cx: &mut Context<Self>) {
        self.shortcuts_open = !self.shortcuts_open;
        if self.shortcuts_open {
            self.focus_modal = true;
        } else {
            self.refocus_root = true;
        }
        cx.notify();
    }

    /// Open or close the "What's New" changelog overlay (Help menu / `help: what's
    /// new` palette command). Opening focuses the modal so Esc closes it; closing
    /// returns focus to the root, like the other keyboard-driven overlays.
    pub(crate) fn toggle_whats_new(&mut self, cx: &mut Context<Self>) {
        self.set_whats_new(!self.whats_new_open, cx);
    }

    /// Open the "What's New" overlay (the post-update toast's "Show changelog").
    pub(crate) fn open_whats_new(&mut self, cx: &mut Context<Self>) {
        self.set_whats_new(true, cx);
    }

    fn set_whats_new(&mut self, open: bool, cx: &mut Context<Self>) {
        self.whats_new_open = open;
        if open {
            self.focus_modal = true;
        } else {
            self.refocus_root = true;
        }
        cx.notify();
    }

    /// Raise the one-shot "RED updated to X" toast. Persistent (no auto-dismiss) so
    /// the user doesn't miss it, with a "Show changelog" action that opens the
    /// What's New panel. Called once from `render` when `pending_update` is set.
    pub(crate) fn notify_update(&mut self, version: SharedString, cx: &mut Context<Self>) {
        self.push_notification(
            Notification {
                id: 0,
                variant: ToastVariant::Info,
                message: format!("RED updated to {version}").into(),
                detail: Some("See what's new in this release.".into()),
                detail_label: None,
                auto_dismiss: None,
                export: None,
                expanded: false,
                hovered: false,
                dismiss_gen: 0,
                action: Some(NotificationAction::ShowChangelog),
            },
            cx,
        );
    }

    // --- pane focus ---

    /// Move keyboard focus to `pane` and remember it as the active pane (so the
    /// next focus-cycle starts from here and the pane chrome draws its ring).
    /// Focusing the schema pane also reveals the sidebar if it was collapsed.
    /// No-op outside the connected shell, or when the editor pane has no open tab.
    pub(crate) fn focus_pane(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        if pane == Pane::Schema {
            if let Phase::Connected(active) = &mut self.phase {
                active.sidebar_collapsed = false;
            }
        }
        let handle = match &self.phase {
            Phase::Connected(active) => match pane {
                Pane::Schema => Some(active.schema_focus.clone()),
                // The focused half's grid: the second half has its own handle so
                // the cell cursor never lands on both grids at once.
                Pane::Grid => Some(active.grid_focus_for(active.focused_half()).clone()),
                Pane::Editor => active.active().map(|t| t.editor.focus_handle(cx)),
            },
            _ => return,
        };
        let Some(handle) = handle else { return };
        window.focus(&handle, cx);
        if let Phase::Connected(active) = &mut self.phase {
            active.active_pane = pane;
        }
        cx.notify();
    }

    /// Reveal the schema sidebar and focus its filter field, so the user can type
    /// to search the schema (the ⌘F / "search schema" command). No-op outside the
    /// connected shell.
    pub(crate) fn open_schema_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let filter = match &mut self.phase {
            Phase::Connected(active) => {
                active.sidebar_collapsed = false;
                active.active_pane = Pane::Schema;
                active.schema.filter.clone()
            }
            _ => return,
        };
        window.focus(&filter.focus_handle(cx), cx);
        cx.notify();
    }

    /// Cycle focus to the next (or previous) pane in visual order
    /// schema → editor → grid. A collapsed/absent sidebar drops out of the cycle.
    pub(crate) fn cycle_focus(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (current, order) = match &self.phase {
            Phase::Connected(active) => {
                let mut order = Vec::with_capacity(3);
                if !active.sidebar_collapsed {
                    order.push(Pane::Schema);
                }
                order.push(Pane::Editor);
                order.push(Pane::Grid);
                (active.active_pane, order)
            }
            _ => return,
        };
        // Where the active pane sits in the cycle (default to the first if its
        // pane just dropped out, e.g. the sidebar was collapsed while focused).
        let at = order.iter().position(|p| *p == current).unwrap_or(0);
        let n = order.len();
        let next = if forward {
            (at + 1) % n
        } else {
            (at + n - 1) % n
        };
        self.focus_pane(order[next], window, cx);
    }

    // --- split view (two query tabs side by side) ---

    /// Default left-half width when a split first opens; the user drags from here.
    const SPLIT_DEFAULT_WIDTH: f32 = 560.;

    /// Toggle the side-by-side split: open it (the ⌘\ / palette action) or, when
    /// it's already open, collapse it. Routes to the Redis shell's own split for
    /// a Redis connection (which has tabs but no SQL editor/result panes).
    pub(crate) fn toggle_split(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(a) = &self.phase {
            if a.kv_view.is_some() {
                let session = a.session;
                self.kv_toggle_split(session, cx);
                return;
            }
        }
        let split = matches!(&self.phase, Phase::Connected(a) if a.split.is_some());
        if split {
            self.unsplit(cx);
        } else {
            self.split_right(cx);
        }
    }

    /// Open the split: a second query pane to the right, focused, holding a fresh
    /// blank tab. The left pane keeps all its tabs (each half owns its own tabs, so
    /// nothing is duplicated); drag a tab across the divider to move it over. No-op
    /// unless connected with a tab open, or when already split.
    pub(crate) fn split_right(&mut self, cx: &mut Context<Self>) {
        match &self.phase {
            Phase::Connected(active) if active.split.is_none() && active.active().is_some() => {}
            _ => return,
        }
        // Mint the blank tab's title (bumps the seq) outside the build, since
        // `QueryTab::new` needs `cx`.
        let title = match &mut self.phase {
            Phase::Connected(active) => {
                active.query_seq += 1;
                format!("query {}", active.query_seq)
            }
            _ => return,
        };
        let mut tab = QueryTab::new(title, cx);
        tab.pane = SplitHalf::Secondary;
        if let Phase::Connected(active) = &mut self.phase {
            active.tabs.push(tab);
            let secondary = active.tabs.len() - 1;
            active.split = Some(SplitState {
                secondary,
                focus: SplitHalf::Secondary,
                width: px(Self::SPLIT_DEFAULT_WIDTH),
                drag: None,
            });
            active.normalize_panes();
        }
        // The new half is now focused; seed its editor's completions and focus it.
        self.refresh_completions(cx);
        self.pending_focus = Some(Pane::Editor);
        cx.notify();
    }

    /// Collapse the split back to one pane: every tab folds into the single strip,
    /// keeping whichever half was focused on screen. No-op when not split.
    pub(crate) fn unsplit(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(s) = active.split.take() {
                let keep = if s.focus == SplitHalf::Secondary {
                    s.secondary
                } else {
                    active.active_tab
                };
                for t in &mut active.tabs {
                    t.pane = SplitHalf::Primary;
                }
                active.active_tab = keep.min(active.tabs.len().saturating_sub(1));
            } else {
                return;
            }
        } else {
            return;
        }
        self.pending_focus = Some(Pane::Editor);
        cx.notify();
    }

    /// Set the focused half (the strip click / a per-half interaction picks this so
    /// run/export/filter target the half the user just touched). Notifies only on a
    /// change; a no-op when not split.
    pub(crate) fn set_split_focus(&mut self, half: SplitHalf, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(s) = &mut active.split {
                if s.focus != half {
                    s.focus = half;
                    cx.notify();
                }
            }
        }
    }

    /// Move focus to the other half of the split, keeping the same pane within it
    /// (the ⌥⌘\ / palette action). No-op when not split. The actual keyboard focus
    /// move is deferred to the next render via `pending_focus`, so this needs no
    /// `Window` and works from the palette too.
    pub(crate) fn focus_other_half(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(a) = &self.phase {
            if a.kv_view.is_some() {
                let session = a.session;
                self.kv_focus_other_half(session, cx);
                return;
            }
        }
        let pane = match &self.phase {
            Phase::Connected(active) if active.split.is_some() => active.active_pane,
            _ => return,
        };
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(s) = &mut active.split {
                s.focus = s.focus.other();
            }
        }
        // Editor focus lives on the tab's own entity, the grid on the half's handle;
        // re-focusing the current pane lands it in the now-focused half.
        self.pending_focus = Some(pane);
        cx.notify();
    }

    /// Reconcile the split's focused half with where keyboard focus actually sits,
    /// so clicking into either half's editor or grid lights it as active (and aims
    /// run/export/filter there). Called at the top of `render`; no-op when not split
    /// or when focus is elsewhere (schema, assistant, a modal); the last half stays.
    pub(crate) fn sync_split_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let detected = match &self.phase {
            Phase::Connected(active) => match &active.split {
                Some(s) => {
                    let prim_editor = active
                        .tabs
                        .get(active.active_tab)
                        .map(|t| t.editor.focus_handle(cx));
                    let sec_editor = active
                        .tabs
                        .get(s.secondary)
                        .map(|t| t.editor.focus_handle(cx));
                    let prim = prim_editor.is_some_and(|h| h.contains_focused(window, cx))
                        || active.grid_focus.contains_focused(window, cx);
                    let sec = sec_editor.is_some_and(|h| h.contains_focused(window, cx))
                        || active.secondary_grid_focus.contains_focused(window, cx);
                    if sec && s.focus != SplitHalf::Secondary {
                        Some(SplitHalf::Secondary)
                    } else if prim && s.focus != SplitHalf::Primary {
                        Some(SplitHalf::Primary)
                    } else {
                        None
                    }
                }
                None => None,
            },
            _ => None,
        };
        if let Some(half) = detected {
            if let Phase::Connected(active) = &mut self.phase {
                if let Some(s) = &mut active.split {
                    s.focus = half;
                }
            }
        }
    }
}

/// Open `path` with the OS's default handler: the file-first "open in editor"
/// seam. Platform shell-out lives at the app edge. Uses `spawn` (fire-and-forget),
/// never `status`: callers run on the GPUI main thread, and waiting on the OS
/// opener to exit (a slow `xdg-open`/`cmd start` handler) would freeze the window.
/// `Ok` means the opener was launched, not that the file was successfully shown.
pub(crate) fn open_in_os(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        // The empty "" is `start`'s window-title argument, so a quoted path isn't
        // mistaken for the title.
        c.args(["/C", "start", ""]).arg(path);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    // Fire-and-forget: spawn the opener and return without waiting for it to exit.
    cmd.spawn().map(|_| ())
}

/// Reveal `path` in the OS file manager, selected, rather than opening it with
/// its default handler: the "Show in Finder/Explorer" affordance for a written
/// file (an export). Same fire-and-forget contract as [`open_in_os`]. Linux has
/// no universal "select a file" verb across file managers, so it falls back to
/// opening the containing folder.
pub(crate) fn reveal_in_file_manager(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg("-R").arg(path);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("explorer");
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(path.as_os_str());
        c.arg(arg);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path.parent().unwrap_or(path));
        c
    };
    cmd.spawn().map(|_| ())
}
