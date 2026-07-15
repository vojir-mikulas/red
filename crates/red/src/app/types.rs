//! Shared support types for the root app state (`crate::app`).
//!
//! Extracted from `app/mod.rs` per docs/plans/guidelines-conformance.md (Workstream
//! D): the ~35 `Phase`/`FormState`/`QueryTab`/`ActiveConn`/notification/pending-write
//! value types and their inherent impls live here, so `mod.rs` keeps just `AppState`
//! and the core state machine. Re-exported unchanged via `pub(crate) use types::*`,
//! so every existing `crate::app::Foo` path is unmoved.

use std::time::Duration;

use flint::prelude::*;
use flint::{CodeEditor, CodeEditorEvent};
use gpui::{Context, Entity, FocusHandle, Pixels, ScrollHandle, SharedString, prelude::*, px};
use red_core::kv::RecycledKey;
use red_core::{
    Column, ColumnMap, ColumnMeta, ConnectionConfig, CopyMode, DbKind, EditOp, FkEdge,
    ImportFormat, ProxyKind, TableRef,
};
use red_service::{OpId, SessionId};

use crate::result::ResultGrid;
use crate::schema::SchemaState;

use super::AppState;

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
    pub(crate) fn other(self) -> SplitHalf {
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
            if let Some(sec) = self.pane_active(SplitHalf::Secondary)
                && let Some(state) = self.ws_split_mut()
            {
                state.secondary = sec;
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
    /// Stable id of the saved connection being opened (`StoredConnection::id`),
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
    /// "Edit connection" action instead of a countdown. See `Event::ConnectFailed`.
    Failed { error: SharedString },
    /// The SSH jump host's key isn't trusted yet. The splash shows the fingerprint
    /// and offers "Trust & connect", which writes it to `known_hosts` and retries.
    /// Carries what the retry needs. See `Event::SshHostUnknown`.
    NeedsHostTrust {
        host: String,
        port: u16,
        fingerprint: SharedString,
        /// OpenSSH-encoded key, sent back via `Command::TrustSshHost` on trust.
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
    /// Reach the connection through a forward proxy. Off by default; only offered
    /// for network engines, and mutually exclusive with `ssh_enabled` in v1.
    pub proxy_enabled: bool,
    /// Which proxy protocol the form has selected (SOCKS5 / HTTP CONNECT).
    pub proxy_kind: ProxyKind,
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
    ProxyHost,
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
    Batch {
        ops: Vec<EditOp>,
        epoch: red_service::Epoch,
    },
    /// A confirmed data import (Track: data import): everything to fire
    /// `Command::Import` on confirm, plus the precomputed `prose`/`preview` the
    /// confirm dialog shows so the user sees the file→table mapping before any write.
    Import {
        path: std::path::PathBuf,
        format: ImportFormat,
        target: TableRef,
        mapping: Vec<ColumnMap>,
        id: OpId,
        prose: String,
        preview: String,
    },
    /// A confirmed table copy (result → another table). Carries everything to fire
    /// `Command::CopyToTable` on confirm plus the precomputed `prose`/`preview` (the
    /// name-based mapping) the dialog shows. The confirm offers two modes: Append (the
    /// default `mode`) and Truncate+insert (the danger button), so the destructive
    /// refresh is opt-in behind a distinct, clearly-labeled action.
    Copy {
        id: OpId,
        source_epoch: red_service::Epoch,
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
    pub id: OpId,
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
    pub id: OpId,
    pub source_epoch: red_service::Epoch,
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
    pub source_epoch: red_service::Epoch,
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
    pub epoch: red_service::Epoch,
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
pub(crate) const MAX_PARKED_SESSIONS: usize = 8;

/// Most persistent (error / warning) notifications retained at once. Transient
/// info/success toasts self-dismiss; persistent ones are removed only by a user
/// click, so a burst of query errors is capped here: the oldest persistent toast
/// is dropped past this. Visible toasts are already capped lower in the renderer.
pub(crate) const MAX_NOTIFICATIONS: usize = 50;

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
    pub id: OpId,
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
    /// Restore the keys a delete just captured into the recycle bin (the
    /// "Undo" button on the post-delete toast). Carries the recycle batch id.
    UndoDelete(u64),
}

/// One deleted batch held in the recycle bin for undo (see the Redis recycle
/// bin, `Command::KvRestoreKeys`): the keys' `DUMP` snapshots plus the session
/// and scan epoch they were deleted under, so an "Undo" can `RESTORE` them and
/// re-scan the right browse. Session-scoped and capped ([`RECYCLE_BIN_CAP`]);
/// not persisted across restarts (a lighter contract than a disk recycle bin).
pub(crate) struct RecycleBatch {
    pub id: u64,
    pub session: SessionId,
    pub epoch: red_service::Epoch,
    pub keys: Vec<RecycledKey>,
}

/// How many recent delete batches the recycle bin keeps before evicting the
/// oldest. A soft cap so a long session of deletes can't grow the bin unbounded.
pub(crate) const RECYCLE_BIN_CAP: usize = 25;

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
    /// (`StoredConnection::id`); the switcher matches warm/foreground sessions
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
    /// in-grid FK click-through. See `Command::LoadForeignKeys`.
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
    /// Live search box for the History dock (both SQL and Redis shells). Owned
    /// here so it survives re-renders; the adapter reads its text to narrow the
    /// list. See [`crate::history_panel`].
    pub history_search: Entity<TextInput>,
    /// Which SQL history time-buckets ("today"/"yesterday"/"earlier") are
    /// collapsed. In-memory (reset per session); empty means all expanded.
    pub history_bucket_collapsed: std::collections::HashSet<&'static str>,
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
    /// The data-compare (table diff) report overlay when open (a full-screen
    /// read-only report, so it hangs off the connection like the ER diagram).
    /// `None` when closed. See [`crate::diff_view`].
    pub diff: Option<crate::diff_view::DiffReport>,
    /// The Redis shell's dynamic tab set (see docs/plans/redis-workflow-parity.md);
    /// `Some` only for a `DbKind::Redis` session, set up in `on_connected`.
    /// `None` for every SQL engine. Constructed here (needs `cx` to make the
    /// default Browse tab's filter `TextInput`); `on_connected` fires that
    /// tab's initial `KvDbSize`/`KvFetchScan` once the session is live.
    pub kv_view: Option<crate::kvbrowse::RedisView>,
}

impl ActiveConn {
    pub(crate) fn new(
        session: SessionId,
        conn_id: String,
        config: ConnectionConfig,
        version: String,
        cx: &mut Context<AppState>,
    ) -> Self {
        let tab = QueryTab::new("query 1".to_string(), cx);
        let kv_view =
            (config.kind == DbKind::Redis).then(|| crate::kvbrowse::RedisView::new(session, cx));
        let history_search = cx.new(|cx| TextInput::new(cx).with_placeholder("Search history…"));
        // Re-render so the search narrows the dock live as the user types.
        cx.subscribe(
            &history_search,
            |_this, _input, _evt: &TextInputEvent, cx| cx.notify(),
        )
        .detach();
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
            history_search,
            history_bucket_collapsed: std::collections::HashSet::new(),
            history_w: px(240.),
            history_drag: None,
            columns_open: false,
            columns_w: px(260.),
            columns_drag: None,
            last_active_seq: 0,
            er: None,
            diff: None,
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
    pub(crate) fn result_by_epoch(&mut self, epoch: red_service::Epoch) -> Option<&mut ResultGrid> {
        self.tabs
            .iter_mut()
            .filter_map(|t| t.result.as_mut())
            .find(|g| g.epoch == epoch)
    }

    /// Find the open plan carrying `epoch`, across all tabs; `PlanReady`/
    /// `PlanFailed` route by epoch like result events.
    pub(crate) fn plan_by_epoch(
        &mut self,
        epoch: red_service::Epoch,
    ) -> Option<&mut crate::plan::PlanView> {
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
