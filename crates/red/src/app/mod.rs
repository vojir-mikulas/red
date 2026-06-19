//! The root view and app state machine. `AppState` owns the backend handle, the
//! persisted connection list, and the current `Phase` (disconnected connect
//! screen ↔ connecting ↔ connected shell). Backend events are drained on a
//! foreground `cx.spawn` task into [`AppState::on_event`] — the one place where
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
mod keymap_edit;
mod render;
mod settings;
mod switcher;
mod tabs;

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
use red_core::{ConnectionConfig, DbKind, EditOp, UpdateState};
use red_service::{Command, Event, ServiceHandle, SessionId, UpdateConfig};

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
/// to — routes a choice to the matching setter and the matching combo box.
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

/// Which key the welcome screen's saved-connection list is ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectSortField {
    Name,
    Recent,
}

/// How the welcome screen's saved-connection list is ordered: a key plus a
/// direction. `ascending` is the key's natural order — A→Z for `Name`, oldest
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

    /// Select `field`, or — if it's already active — flip the direction. The
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
    /// The session this connect is opening — minted UI-side so retries reuse it.
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
    /// An attempt is in flight — the indeterminate progress bar sweeps.
    InProgress,
    /// The last attempt failed; we're waiting `delay` before the next retry,
    /// showing the error. `delay` is the wait we scheduled (shown to the user).
    Backoff {
        error: SharedString,
        delay: Duration,
    },
    /// The attempt failed for a user-correctable reason (bad credentials, missing
    /// database) — terminal, no retry. The splash shows the error and offers an
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

/// The result of the latest "Test connection" probe, shown in the form footer.
pub(crate) enum TestState {
    Idle,
    Testing,
    Ok(SharedString),
    Fail(SharedString),
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
    /// `Some(index)` when editing an existing connection, `None` when adding.
    pub editing: Option<usize>,
    /// Set once the user tries to Save/Connect (or Test) with missing fields —
    /// the gate for showing the inline per-field validation messages, so a fresh
    /// empty form isn't pre-littered with errors.
    pub submitted: bool,
    pub test: TestState,
    /// Tunnel the connection through an SSH jump host. Off by default; only
    /// offered for network engines (a file engine has no host to reach).
    pub ssh_enabled: bool,
    /// Which SSH auth method the form has selected. The key path and the secrets
    /// live in the shared SSH inputs; this only tracks the choice.
    pub ssh_auth: SshAuthMode,
}

/// The SSH authentication method picked in the form. Mirrors `red_core::SshAuth`
/// but carries no data — the key path / secrets live in the form's inputs.
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
}

/// How long a transient (info / success) toast stays up before it auto-dismisses.
/// Errors and warnings (and a live export) have no timer — they persist until the
/// user closes them or the operation resolves.
const TOAST_AUTO_DISMISS: Duration = Duration::from_secs(4);

/// Most warm parked sessions kept resident at once. Each is a heavy `ActiveConn`
/// (editor entities, schema detail map, result buffers), so the map is capped:
/// parking past this LRU-evicts the least-recently-foregrounded session (closing
/// its backend session too). The cap makes a missed backend `Disconnected` a
/// bounded annoyance instead of unbounded growth.
const MAX_PARKED_SESSIONS: usize = 8;

/// Most persistent (error / warning) notifications retained at once. Transient
/// info/success toasts self-dismiss; persistent ones are removed only by a user
/// click, so a burst of query errors is capped here — the oldest persistent toast
/// is dropped past this. Visible toasts are already capped lower in the renderer.
const MAX_NOTIFICATIONS: usize = 50;

/// The live state of the export-progress toast: how many rows have streamed out
/// of the known `total`, keyed by the export `id` so a `CancelExport` / progress
/// update targets the right one. Only the export toast carries this.
pub(crate) struct ExportProgress {
    pub id: u64,
    pub rows: usize,
    pub total: usize,
}

/// One notification in the bottom-right stack. The stack is newest-last (nearest
/// the corner); `auto_dismiss` drives the per-toast timer (`None` = persists until
/// closed); `export` is set only on the export-progress toast.
pub(crate) struct Notification {
    pub id: u64,
    pub variant: ToastVariant,
    pub message: SharedString,
    pub auto_dismiss: Option<Duration>,
    pub export: Option<ExportProgress>,
}

/// The default editor text a fresh query tab opens with. A tab still holding
/// exactly this (and no result) is "pristine" — closing it needs no confirmation.
pub(crate) const EMPTY_QUERY: &str = "-- Write SQL, ⌘↵ to run\n";

/// One query tab: its own SQL editor and result grid. A connection holds several
/// of these; the schema sidebar, split sizes, and query history are shared.
pub(crate) struct QueryTab {
    /// Tab label: "query N" for a blank tab, or "schema.table" for a preview.
    pub title: String,
    /// The SQL editor surface, with the RED highlighter installed.
    pub editor: Entity<CodeEditor>,
    /// The open result browsed in the grid: a table preview or an editor run.
    pub result: Option<ResultGrid>,
    /// The query plan (Track B4 — EXPLAIN), when one is open. Occupies the result
    /// pane in place of the grid; running a query clears it. `None` is the grid.
    pub plan: Option<crate::plan::PlanView>,
}

impl QueryTab {
    pub(crate) fn new(title: String, cx: &mut Context<AppState>) -> Self {
        let editor = cx.new(|cx| {
            CodeEditor::new(cx)
                .highlighter(crate::sql::tokenize)
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
                CodeEditorEvent::Escape => {
                    this.pending_focus = Some(Pane::Grid);
                    cx.notify();
                }
            },
        )
        .detach();

        Self {
            title,
            editor,
            result: None,
            plan: None,
        }
    }

    /// A blank tab the user hasn't touched — no result and the default text still
    /// in the editor. Closing one of these doesn't warrant a confirmation.
    pub(crate) fn is_pristine(&self, cx: &Context<AppState>) -> bool {
        self.result.is_none() && self.editor.read(cx).content() == EMPTY_QUERY
    }
}

/// The live-connection view state: which connection, its engine version, the
/// resizable split sizes (caller-owned, per `SplitPane`'s stateless contract),
/// the schema explorer, and the open query tabs.
pub(crate) struct ActiveConn {
    /// The backend session backing this workspace. Stays warm while parked, so a
    /// switch back is instant; binds this conn's `CommandSender`.
    pub session: SessionId,
    /// Stable id of the saved connection this workspace belongs to
    /// ([`StoredConnection::id`]) — the switcher matches warm/foreground sessions
    /// by this, not by `config.name` (names aren't unique).
    pub conn_id: String,
    pub config: ConnectionConfig,
    pub version: String,
    pub sidebar_w: Pixels,
    pub sidebar_drag: Option<DragAnchor>,
    /// When set, the schema sidebar is hidden; `sidebar_w` is retained so toggling
    /// it back restores the previous width.
    pub sidebar_collapsed: bool,
    pub editor_h: Pixels,
    pub editor_drag: Option<DragAnchor>,
    /// Width of the cell/row detail inspector when docked to the right of the
    /// grid; retained while the inspector is closed so reopening restores it.
    pub inspector_w: Pixels,
    pub inspector_drag: Option<DragAnchor>,
    pub schema: SchemaState,
    /// Open query tabs (never empty), and the index of the focused one.
    pub tabs: Vec<QueryTab>,
    pub active_tab: usize,
    /// Monotonic counter for naming blank tabs ("query 1", "query 2", …).
    pub query_seq: usize,
    /// While a tab is being dragged, the gap (insertion index `0..=tabs.len()`)
    /// where it would land — drives the drop indicator. Only meaningful when a
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
    /// Which pane currently holds focus — drives focus cycling and the pane ring.
    pub active_pane: Pane,
    /// Recent queries (newest first), shared across all of this connection's
    /// tabs — re-run any of them from the history popover.
    pub history: Vec<String>,
    pub history_open: bool,
    /// Focus anchor for the open history popover, and the keyboard-highlighted
    /// entry within it.
    pub history_focus: FocusHandle,
    pub history_sel: usize,
    /// Recency stamp: bumped from [`AppState::next_active_seq`] each time this
    /// workspace is parked (it was foreground until that moment). Drives LRU
    /// eviction when [`MAX_PARKED_SESSIONS`] is exceeded — the lowest stamp is the
    /// least-recently-foregrounded parked session.
    pub last_active_seq: u64,
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
            tabs: vec![tab],
            active_tab: 0,
            query_seq: 1,
            tab_drop_target: None,
            tab_scroll: ScrollHandle::new(),
            schema_focus: cx.focus_handle(),
            grid_focus: cx.focus_handle(),
            active_pane: Pane::Editor,
            history: Vec::new(),
            history_open: false,
            history_focus: cx.focus_handle(),
            history_sel: 0,
            last_active_seq: 0,
        }
    }

    /// The focused tab, or `None` when the strip is empty (the user closed the
    /// last tab — the shell then shows an empty pane instead of a query editor).
    pub(crate) fn active(&self) -> Option<&QueryTab> {
        self.tabs.get(self.active_tab)
    }

    pub(crate) fn active_mut(&mut self) -> Option<&mut QueryTab> {
        self.tabs.get_mut(self.active_tab)
    }

    /// The focused tab's open result, if any. Folds together "no tab" and "tab
    /// with no result" — the common shape at most result call sites.
    pub(crate) fn active_result(&self) -> Option<&ResultGrid> {
        self.active().and_then(|t| t.result.as_ref())
    }

    pub(crate) fn active_result_mut(&mut self) -> Option<&mut ResultGrid> {
        self.active_mut().and_then(|t| t.result.as_mut())
    }

    /// Find the open result whose grid carries `epoch`, across all tabs — result
    /// events route by epoch so a background tab's query still populates.
    pub(crate) fn result_by_epoch(&mut self, epoch: u64) -> Option<&mut ResultGrid> {
        self.tabs
            .iter_mut()
            .filter_map(|t| t.result.as_mut())
            .find(|g| g.epoch == epoch)
    }

    /// The focused tab's open plan, if any (Track B4).
    pub(crate) fn active_plan(&self) -> Option<&crate::plan::PlanView> {
        self.active().and_then(|t| t.plan.as_ref())
    }

    /// Find the open plan carrying `epoch`, across all tabs — `PlanReady`/
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
    /// The cell detail inspector, when open (Track B1). Owns its scroll position
    /// and any on-demand full value fetched for a capped/evicted cell.
    pub(crate) inspector: Option<crate::inspector::InspectorState>,
    /// The AI assistant chat panel, when open (right-docked). Owns its input,
    /// transcript, and streaming state. Single panel across the workspace.
    pub(crate) assistant: Option<crate::assistant::AssistantState>,
    /// Set when the assistant panel just opened: the next render focuses its input.
    pub(crate) focus_assistant: bool,
    /// Docked width of the assistant panel, retained while it's closed so reopening
    /// restores it. Resizable via the shell split.
    pub(crate) assistant_w: Pixels,
    pub(crate) assistant_drag: Option<DragAnchor>,
    /// Whether the assistant is usable at all (a key is configured, or the default
    /// provider is the subscription, which needs none). Drives the panel's
    /// setup-vs-chat view. Recomputed at launch and on settings reload.
    pub(crate) ai_configured: bool,
    /// Whether an Anthropic API key is available (keyring or env), independent of
    /// the default provider. Gates offering an API-key chat alongside a
    /// subscription one (mixed providers, M-S6). Recomputed with `ai_configured`.
    pub(crate) ai_api_key_available: bool,
    /// Monotonic id source for assistant conversations, so the backend keeps each
    /// panel's turn history separate.
    pub(crate) next_conversation_id: u64,
    /// The result filter bar, when open (Track B2). The transient editing UI; the
    /// *applied* filter lives on the grid (`ResultGrid::filter`).
    pub(crate) filter_bar: Option<crate::filter::FilterBarState>,
    /// Window-coordinate anchor for the result cell's right-click context menu,
    /// when open. The right-click selects the cell first, so the menu's Inspect/
    /// Copy act on it; `None` keeps the menu closed.
    pub(crate) cell_menu: Option<gpui::Point<gpui::Pixels>>,
    /// A pending write awaiting the user's confirmation before it runs: an editor
    /// destructive statement, or a staged grid edit batch (Track B6). See
    /// [`PendingWrite`].
    pub(crate) confirm_exec: Option<PendingWrite>,
    /// The open inline cell editor (Track B6), when the user is editing a grid cell
    /// in place. `None` when no editor is open. The staged change-set itself lives
    /// on the result; this is just the live `TextInput`.
    pub(crate) grid_edit: Option<crate::result::GridEdit>,
    /// Render-time focus drain: focus the open inline editor's field on the next
    /// frame (set when one opens, like `focus_inspector_edit`).
    pub(crate) focus_grid_edit: bool,
    /// Focus-out listener on the open inline editor: clicking away commits (stages)
    /// the edit, like a spreadsheet. Held while an editor is open, dropped when it
    /// closes — mirrors `modal_focus_trap`.
    pub(crate) grid_edit_blur: Option<gpui::Subscription>,
    /// A non-pristine query tab the user asked to close, awaiting confirmation.
    pub(crate) confirm_close_tab: Option<usize>,
    /// A saved connection the user asked to delete, awaiting confirmation.
    pub(crate) confirm_delete_conn: Option<usize>,
    /// Persisted UI preferences (theme, grid, query, the safety rail) + their store.
    pub(crate) settings: Settings,
    pub(crate) settings_store: Option<FileSettingsStore>,
    pub(crate) settings_open: bool,
    pub(crate) settings_tab: SettingsTab,
    /// Non-fatal problems from the last settings load (an unreadable section, a
    /// bad value) — surfaced as a dismissible banner so a hand-edit gets feedback
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
    /// action) — shown in the same banner as [`Self::settings_warnings`].
    pub(crate) keymap_warnings: Vec<String>,
    /// The Keymap tab's search box, filtering the bindable-action list by label or
    /// keystroke.
    pub(crate) keymap_search: Entity<TextInput>,
    /// The row currently capturing a chord (index into [`crate::keymap::action_defs`]),
    /// while the recorder's keystroke interceptor is live. `None` when not recording.
    pub(crate) keymap_recording: Option<usize>,
    /// The live keystroke interceptor for the recorder. Held exactly as long as
    /// [`Self::keymap_recording`] is `Some`; dropping it (on capture, cancel, tab
    /// switch, or panel close) ends capture so normal shortcuts resume — a leaked
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
    /// Installed font families, sorted + deduped. Enumerating these hits the OS
    /// text system (a CoreText scan of hundreds of faces) — far too slow to do
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
    /// trap). Dropped — unsubscribing — when the modal closes.
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
    /// activation can resolve its index. Loaded on demand — never at startup.
    pub(crate) saved_queries: Vec<crate::queries::SavedQuery>,
    /// The saved conversations shown by the open history picker (M-S5), held only
    /// while it's open so an activation can resolve its index. Loaded on demand.
    pub(crate) loaded_conversations: Vec<crate::conversations::Conversation>,
    /// The connection switcher (⌘P): an always-mounted topbar trigger that opens a
    /// searchable, sectioned popover of the active + recent connections. Its
    /// sections are rebuilt from `connections` + `phase` via [`Self::rebuild_switcher`].
    pub(crate) switcher: Entity<Switcher>,
    /// Warm background connections, kept live so switching back is instant (no
    /// reconnect). The foreground connection lives in `phase` (`Phase::Connected`);
    /// these are the ones the user switched away from. Keyed by their backend
    /// session. An idle one is evicted backend-side after 10 min — its
    /// `Disconnected` event drops it here and demotes it to a plain recent.
    pub(crate) parked: HashMap<SessionId, Box<ActiveConn>>,
    /// The session the window currently shows — the `phase`'s session (connecting
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
    /// Whether the keyboard-shortcuts reference overlay (`⌘/`) is showing.
    pub(crate) shortcuts_open: bool,
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
    /// Dev-only perf HUD collector — brackets `render` to read build time and
    /// allocation churn. Compiled only under the `dev-stats` feature.
    #[cfg(feature = "dev-stats")]
    pub(crate) dev_stats: crate::dev_stats::DevStats,
}

/// The GitHub `owner/repo` the self-updater polls (see docs/plans/self-update.md).
pub(crate) const UPDATE_REPO: &str = "vojir-mikulas/red";

/// Where the "report a bug" links point — the project's GitHub issue tracker.
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
/// key. The key is read from the OS keychain (the same store connection passwords
/// use); as a convenience for first-run / headless setup it falls back to the
/// `ANTHROPIC_API_KEY` environment variable. An empty key leaves the assistant
/// off (a turn then replies with a clear error). Used at launch and on reload.
pub(crate) fn ai_config(settings: &Settings) -> red_service::AiConfig {
    let provider = if settings.ai.provider.is_empty() {
        "anthropic".to_string()
    } else {
        settings.ai.provider.clone()
    };
    // `subscription` drives Claude Code over ACP (no key); anything else is the
    // API-key path. `kind` is only the *default* for new chats now — turns carry
    // their own provider (M-S6), so a chat can be on either backend regardless.
    let kind = if provider.eq_ignore_ascii_case("subscription") {
        red_service::AiProviderKind::Subscription
    } else {
        red_service::AiProviderKind::ApiKey
    };
    // Always resolve the API key, even when the default is the subscription, so an
    // API-key chat works alongside a subscription one (mixed providers, M-S6). The
    // key lives under the canonical `anthropic` provider name (or the env var).
    let api_key = crate::secrets::get_ai_key("anthropic")
        .ok()
        .flatten()
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
        .unwrap_or_default();
    red_service::AiConfig {
        provider: kind,
        model: settings.ai.model.clone(),
        api_key,
        show_thinking: settings.ai.show_thinking,
        agent_command: settings.ai.agent_command.clone(),
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
                    break; // view dropped — window closed
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
        // configured cadence — unless `auto_update = false`, which sends a disabled
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
        // SSH secrets are obscured — unlike the DB password they're not echoed in
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
            })
            .detach();
        }
        cx.subscribe(
            &conn_str_input,
            |this, _, event: &TextInputEvent, cx| match event {
                TextInputEvent::Change => this.sync_fields_from_conn_str(cx),
                TextInputEvent::Submit => this.submit_form(cx),
                TextInputEvent::Cancel => this.close_form(cx),
            },
        )
        .detach();
        // The name field doesn't mirror, but still submits/cancels the form.
        cx.subscribe(
            &name_input,
            |this, _, event: &TextInputEvent, cx| match event {
                TextInputEvent::Submit => this.submit_form(cx),
                TextInputEvent::Cancel => this.close_form(cx),
                TextInputEvent::Change => {}
            },
        )
        .detach();

        // Font-size steppers, seeded from the loaded settings. A `Change` (typing,
        // stepping, or Enter) writes straight through to the matching setter, which
        // re-clamps, persists, and re-themes — a live preview as the user edits.
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

        let connections = config::load();

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
                TextInputEvent::Submit => {}
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
            s.set_footer(switcher_footer(), cx);
            s
        });
        cx.subscribe(&switcher, Self::on_switcher_event).detach();

        // The five Appearance-panel dropdowns (searchable combo boxes). They start
        // empty: their options are filled lazily by `rebuild_settings_pickers` when
        // the panel first opens — the installed-font list is a slow OS scan we keep
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
            inspector: None,
            assistant: None,
            focus_assistant: false,
            assistant_w: px(380.),
            assistant_drag: None,
            ai_configured: {
                let cfg = ai_config(&settings);
                cfg.provider == red_service::AiProviderKind::Subscription || !cfg.api_key.is_empty()
            },
            ai_api_key_available: !ai_config(&settings).api_key.is_empty(),
            next_conversation_id: 0,
            filter_bar: None,
            cell_menu: None,
            confirm_exec: None,
            grid_edit: None,
            focus_grid_edit: false,
            grid_edit_blur: None,
            confirm_close_tab: None,
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
            switcher,
            parked: HashMap::new(),
            foreground_session: None,
            next_session_id: 0,
            next_active_seq: 0,
            // Focus the root on first paint so the very first ⌘K dispatches.
            refocus_root: true,
            shortcuts_open: false,
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
                auto_dismiss,
                export: None,
            },
            cx,
        )
    }

    /// Assign `notification` a fresh id, push it, and — for a transient toast —
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
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                cx.background_executor().timer(delay).await;
                this.update(cx, |this, cx| this.dismiss(id, cx)).ok();
            })
            .detach();
        }
        cx.notify();
        id
    }

    /// Remove the notification with `id` (its close button, or a fired timer).
    pub(crate) fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        self.notifications.retain(|n| n.id != id);
        cx.notify();
    }

    /// The notification's `✕`: dismiss a plain toast, or — for the export toast —
    /// abort the backend stream. The toast stays (now "Cancelling…") until the
    /// `ExportCancelled` event swaps it for a transient one.
    pub(crate) fn close_notification(&mut self, id: u64, cx: &mut Context<Self>) {
        let export_id = self
            .notifications
            .iter()
            .find(|n| n.id == id)
            .and_then(|n| n.export.as_ref())
            .map(|e| e.id);
        match export_id {
            Some(export_id) => {
                self.send_active(Command::CancelExport { id: export_id });
                if let Some(n) = self.notifications.iter_mut().find(|n| n.id == id) {
                    n.message = "Cancelling export…".into();
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
                if let Some(form) = &mut self.form {
                    form.test = TestState::Ok(format!("Connection successful · {version}").into());
                }
            }
            Event::TestFailed { message } => {
                if let Some(form) = &mut self.form {
                    form.test = TestState::Fail(message.into());
                }
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
                // Log the full text to stderr (RUST_LOG) — a toast is ephemeral and
                // can truncate, so the console keeps the complete error to inspect.
                tracing::error!(?session, "{message}");
                self.on_result_error(session, &message);
                self.notify(ToastVariant::Error, message, cx);
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
                }
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

            // --- export & writes ---
            Event::Executed { affected } => {
                self.notify(
                    ToastVariant::Success,
                    format!("{affected} row(s) affected"),
                    cx,
                );
                // A write may have changed the schema (CREATE/DROP) — refresh the
                // tree of the session that ran it.
                if let Some(id) = session {
                    self.service.send_to(id, Command::LoadObjects);
                }
            }
            Event::ExportProgress { id, rows } => self.on_export_progress(id, rows, cx),
            Event::ExportFinished { id, path, rows } => self.on_export_finished(id, path, rows, cx),
            Event::ExportCancelled { id } => self.on_export_cancelled(id, cx),

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

    /// Run the pending write the user confirmed — a destructive editor statement
    /// or a guarded grid edit (Track B5).
    pub(crate) fn confirm_destructive(&mut self, cx: &mut Context<Self>) {
        match self.confirm_exec.take() {
            Some(PendingWrite::EditorSql(sql)) => self.execute_sql(sql, cx),
            Some(PendingWrite::Batch { ops, epoch }) => {
                self.send_active(Command::ApplyBatch { epoch, ops });
            }
            None => {}
        }
        // The modal is closing — return focus to the root for the next ⌘K etc.
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
            || self.confirm_delete_conn.is_some()
            || self.shortcuts_open
            || self.settings_open
            || self.form.is_some()
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
                Pane::Grid => Some(active.grid_focus.clone()),
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
}

/// Open `path` with the OS's default handler — the file-first "open in editor"
/// seam. Platform shell-out lives at the app edge; the UI never blocks on it.
fn open_in_os(path: &std::path::Path) -> std::io::Result<()> {
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
    cmd.status().map(|_| ())
}
