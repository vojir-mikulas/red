//! The root view and app state machine. `AppState` owns the backend handle, the
//! persisted connection list, and the current `Phase` (disconnected connect
//! screen ↔ connecting ↔ connected shell). Backend events are drained on a
//! foreground `cx.spawn` task into [`AppState::on_event`] — the one place where
//! the service drives UI state. Screen rendering lives in `connect.rs` / `shell.rs`;
//! within this module, [`form`] holds the connection-form logic and [`render`]
//! the root view + confirmation modals.

mod form;
mod render;

use std::time::Duration;

use flint::prelude::*;
use flint::{CodeEditor, CodeEditorEvent};
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use gpui::{
    prelude::*, px, AsyncApp, Context, ElementId, Entity, FocusHandle, Pixels, SharedString,
    WeakEntity,
};
use red_core::{ConnectionConfig, DbKind};
use red_service::{Command, Event, ServiceHandle};

use crate::config::{self, StoredConnection};
use crate::palette::Cmd;
use crate::result::ResultGrid;
use crate::schema::SchemaState;
use crate::settings::{Density, FileSettingsStore, Settings};
use crate::settings_ui::SettingsTab;

/// Which top-level screen is showing.
pub(crate) enum Phase {
    Disconnected,
    Connecting(Connecting),
    // Boxed: `ActiveConn` carries the whole schema model, dwarfing the other
    // variants — box it to keep `Phase` small.
    Connected(Box<ActiveConn>),
}

/// State of an in-progress connection: which config we're dialing, how many
/// attempts we've made, and whether an attempt is in flight or we're waiting
/// out a backoff before the next retry. Drives the connecting splash (progress
/// bar / error / retry / cancel). See [`AppState::start_connect`].
pub(crate) struct Connecting {
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
    pub test: TestState,
}

/// The default editor text a fresh query tab opens with. A tab still holding
/// exactly this (and no result) is "pristine" — closing it needs no confirmation.
pub(crate) const EMPTY_QUERY: &str = "-- Write SQL, ⌘↵ to run\n";

/// One query tab: its own SQL editor, result grid, and history. A connection
/// holds several of these; the schema sidebar and split sizes are shared.
pub(crate) struct QueryTab {
    /// Tab label: "query N" for a blank tab, or "schema.table" for a preview.
    pub title: String,
    /// The SQL editor surface, with the RED highlighter installed.
    pub editor: Entity<CodeEditor>,
    /// The open result browsed in the grid: a table preview or an editor run.
    pub result: Option<ResultGrid>,
    /// Recent queries (newest first), for re-run from the history popover.
    pub history: Vec<String>,
    pub history_open: bool,
}

impl QueryTab {
    pub(crate) fn new(title: String, cx: &mut Context<AppState>) -> Self {
        let editor = cx.new(|cx| {
            CodeEditor::new(cx)
                .highlighter(crate::sql::tokenize)
                .with_content(EMPTY_QUERY)
        });
        // ⌘↵ in the (focused) editor runs the active tab's statement / selection.
        cx.subscribe(&editor, |this, _editor, _event: &CodeEditorEvent, cx| {
            this.run_editor_query(cx)
        })
        .detach();

        Self {
            title,
            editor,
            result: None,
            history: Vec::new(),
            history_open: false,
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
    pub config: ConnectionConfig,
    pub version: String,
    pub sidebar_w: Pixels,
    pub sidebar_drag: Option<DragAnchor>,
    pub editor_h: Pixels,
    pub editor_drag: Option<DragAnchor>,
    pub schema: SchemaState,
    /// Open query tabs (never empty), and the index of the focused one.
    pub tabs: Vec<QueryTab>,
    pub active_tab: usize,
    /// Monotonic counter for naming blank tabs ("query 1", "query 2", …).
    pub query_seq: usize,
}

impl ActiveConn {
    fn new(config: ConnectionConfig, version: String, cx: &mut Context<AppState>) -> Self {
        let tab = QueryTab::new("query 1".to_string(), cx);
        Self {
            config,
            version,
            sidebar_w: px(240.),
            sidebar_drag: None,
            editor_h: px(300.),
            editor_drag: None,
            schema: SchemaState::new(cx),
            tabs: vec![tab],
            active_tab: 0,
            query_seq: 1,
        }
    }

    /// The focused tab. `active_tab` is kept in range, so these never panic.
    pub(crate) fn active(&self) -> &QueryTab {
        &self.tabs[self.active_tab]
    }

    pub(crate) fn active_mut(&mut self) -> &mut QueryTab {
        &mut self.tabs[self.active_tab]
    }

    /// Find the open result whose grid carries `epoch`, across all tabs — result
    /// events route by epoch so a background tab's query still populates.
    pub(crate) fn result_by_epoch(&mut self, epoch: u64) -> Option<&mut ResultGrid> {
        self.tabs
            .iter_mut()
            .filter_map(|t| t.result.as_mut())
            .find(|g| g.epoch == epoch)
    }
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
    pub(crate) form: Option<FormState>,
    pub(crate) toast: Option<(SharedString, ToastVariant)>,
    /// A destructive statement awaiting the user's confirmation before it runs.
    pub(crate) confirm_exec: Option<String>,
    /// A non-pristine query tab the user asked to close, awaiting confirmation.
    pub(crate) confirm_close_tab: Option<usize>,
    /// Persisted UI preferences (theme, grid density, the safety rail) + their store.
    pub(crate) settings: Settings,
    pub(crate) settings_store: Option<FileSettingsStore>,
    pub(crate) settings_open: bool,
    pub(crate) settings_tab: SettingsTab,
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
    /// The command palette overlay, when open, plus the `id → Cmd` map for the
    /// commands it's currently showing (so an activation routes to the right one).
    pub(crate) palette: Option<Entity<Palette>>,
    pub(crate) palette_cmds: Vec<(ElementId, Cmd)>,
}
impl AppState {
    pub fn new(
        cx: &mut Context<Self>,
        service: ServiceHandle,
        events: UnboundedReceiver<Event>,
    ) -> Self {
        // Drain backend events on the foreground executor into `on_event`.
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let mut events = events;
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |state, cx| state.on_event(event, cx))
                    .is_err()
                {
                    break; // view dropped — window closed
                }
            }
        })
        .detach();

        // Load persisted preferences and apply the saved theme over the default
        // installed in `main` (a missing/malformed file degrades to defaults).
        let settings_store = FileSettingsStore::open_default();
        let settings = settings_store
            .as_ref()
            .map(FileSettingsStore::load)
            .unwrap_or_default();
        cx.set_global(crate::theme::by_name(&settings.theme));

        let name_input = cx.new(|cx| TextInput::new(cx).with_placeholder("my database"));
        let host_input = cx.new(|cx| TextInput::new(cx).with_placeholder("localhost"));
        let port_input = cx.new(TextInput::new);
        let user_input = cx.new(|cx| TextInput::new(cx).with_placeholder("postgres"));
        let password_input = cx.new(|cx| TextInput::new(cx).obscured());
        let database_input = cx.new(|cx| TextInput::new(cx).with_placeholder("analytics_prod"));
        let conn_str_input =
            cx.new(|cx| TextInput::new(cx).with_placeholder("postgres://user:pass@host:5432/db"));

        // Live two-way sync: editing any structured field rebuilds the connection
        // string, and editing the string parses it back into the fields. Only user
        // edits emit `Change`; the programmatic `set_content` used by the sync does
        // not, so the mirror can't echo back into an infinite loop.
        for field in [
            &host_input,
            &port_input,
            &user_input,
            &password_input,
            &database_input,
        ] {
            cx.subscribe(field, |this, _, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Change) {
                    this.sync_conn_str_from_fields(cx);
                }
            })
            .detach();
        }
        cx.subscribe(&conn_str_input, |this, _, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Change) {
                this.sync_fields_from_conn_str(cx);
            }
        })
        .detach();

        Self {
            service,
            connections: config::load(),
            phase: Phase::Disconnected,
            name_input,
            host_input,
            port_input,
            user_input,
            password_input,
            database_input,
            conn_str_input,
            form: None,
            toast: None,
            confirm_exec: None,
            confirm_close_tab: None,
            settings,
            settings_store,
            settings_open: false,
            settings_tab: SettingsTab::Appearance,
            query_ticking: false,
            connect_gen: 0,
            root_focus: cx.focus_handle(),
            palette: None,
            palette_cmds: Vec::new(),
        }
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

    /// The single point where backend events drive UI state.
    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Connected { version } => {
                if let Phase::Connecting(conn) =
                    std::mem::replace(&mut self.phase, Phase::Disconnected)
                {
                    // Invalidate any pending backoff timer from a prior attempt.
                    self.connect_gen += 1;
                    self.phase =
                        Phase::Connected(Box::new(ActiveConn::new(conn.config, version, cx)));
                    // Kick off the schema-tree skeleton load for the sidebar.
                    self.service.send(Command::LoadObjects);
                }
            }
            Event::Disconnected => self.phase = Phase::Disconnected,
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
            Event::Error(message) => {
                // While connecting, the only thing in flight is the connect — so
                // an error is a failed attempt: keep the splash and schedule a
                // backoff retry instead of dropping to the connect screen.
                if matches!(self.phase, Phase::Connecting(_)) {
                    self.on_connect_failed(message, cx);
                } else {
                    self.on_result_error(&message);
                    self.toast = Some((message.into(), ToastVariant::Error));
                }
            }

            // --- schema explorer ---
            Event::ObjectsLoaded { schemas } => {
                if let Phase::Connected(active) = &mut self.phase {
                    active.schema.apply_objects(schemas);
                }
                self.refresh_completions(cx);
            }
            Event::TableDescribed {
                schema,
                table,
                detail,
            } => {
                if let Phase::Connected(active) = &mut self.phase {
                    active.schema.details.insert((schema, table), detail);
                }
                self.refresh_completions(cx);
            }

            // --- result grid ---
            Event::ResultReady {
                columns,
                total,
                epoch,
                key,
            } => self.on_result_ready(columns, total, epoch, key, cx),
            Event::ResultPageLoaded {
                offset,
                rows,
                epoch,
            } => self.on_result_page(offset, rows, epoch, cx),
            // Keyset runs: extend/relocate a grid's resident row run.
            Event::ResultRunLoaded {
                epoch,
                fetch,
                rows,
                estimated,
                seq,
            } => self.on_result_run(epoch, fetch, rows, estimated, seq, cx),
            Event::ResultRunFailed { epoch, seq } => self.on_result_run_failed(epoch, seq),

            // --- export & writes ---
            Event::Executed { affected } => {
                self.toast = Some((
                    format!("{affected} row(s) affected").into(),
                    ToastVariant::Success,
                ));
                // A write may have changed the schema (CREATE/DROP) — refresh the tree.
                self.service.send(Command::LoadObjects);
            }
            Event::ExportFinished { path, rows } => {
                self.toast = Some((
                    format!("Exported {rows} row(s) to {path}").into(),
                    ToastVariant::Success,
                ));
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

    // --- connection-manager actions ---

    pub(crate) fn delete_connection(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.connections.len() {
            let removed = self.connections.remove(index);
            // Drop the connection's keychain credential too, so deleting a
            // connection doesn't orphan its password.
            if let Err(e) = crate::secrets::delete_password(&removed.id) {
                tracing::warn!("failed to remove keychain credential: {e}");
            }
            self.persist();
            cx.notify();
        }
    }

    pub(crate) fn connect(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(stored) = self.connections.get_mut(index) else {
            return;
        };
        stored.last_accessed = Some(config::now());
        let id = stored.id.clone();
        let mut config = stored.config.clone();
        self.persist();
        // Materialize the password from the keychain unless we already hold it in
        // memory (a keychain write that failed earlier this session keeps it there).
        if config.password.is_empty() && !config.kind.is_file() {
            match crate::secrets::get_password(&id) {
                Ok(Some(pw)) => config.password = pw,
                Ok(None) => {}
                Err(e) => tracing::warn!("failed to read credential from keychain: {e}"),
            }
        }
        self.start_connect(config, cx);
    }

    /// Open a fresh connect session: bump the generation (abandoning any pending
    /// retry from a previous session), show the splash, and fire the first
    /// attempt.
    fn start_connect(&mut self, config: ConnectionConfig, cx: &mut Context<Self>) {
        self.connect_gen += 1;
        self.service.send(Command::Connect(config.clone()));
        self.phase = Phase::Connecting(Connecting {
            config,
            attempt: 1,
            status: ConnectStatus::InProgress,
        });
        cx.notify();
    }

    /// Exponential backoff between connect retries: 1s, 2s, 4s, 8s, 16s, then
    /// capped at 30s. `attempt` is the number of the attempt that just failed.
    fn backoff_delay(attempt: u32) -> Duration {
        let secs = 1u64 << attempt.saturating_sub(1).min(5);
        Duration::from_secs(secs.min(30))
    }

    /// A connect attempt failed: record the error on the splash and schedule a
    /// backoff retry. No-op if we've left the connecting phase meanwhile.
    fn on_connect_failed(&mut self, message: String, cx: &mut Context<Self>) {
        let delay = match &mut self.phase {
            Phase::Connecting(conn) => {
                let delay = Self::backoff_delay(conn.attempt);
                conn.status = ConnectStatus::Backoff {
                    error: message.into(),
                    delay,
                };
                delay
            }
            _ => return,
        };
        self.schedule_retry(delay, cx);
    }

    /// Arm a one-shot timer that retries the connection after `delay`, unless a
    /// newer generation (cancel, manual retry, or a fresh connect) supersedes it.
    fn schedule_retry(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let generation = self.connect_gen;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| this.retry_connect(generation, cx))
                .ok();
        })
        .detach();
    }

    /// A backoff timer fired — start the next attempt if its generation is still
    /// current (i.e. not cancelled or already retried via "Retry now").
    fn retry_connect(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation == self.connect_gen {
            self.begin_attempt(cx);
        }
    }

    /// "Retry now" on the splash — skip the remaining backoff wait.
    pub(crate) fn retry_now(&mut self, cx: &mut Context<Self>) {
        if matches!(self.phase, Phase::Connecting(_)) {
            self.begin_attempt(cx);
        }
    }

    /// Fire the next attempt for the in-flight connection: bump the generation
    /// (abandoning any pending backoff timer), advance the counter, and re-send
    /// the Connect command.
    fn begin_attempt(&mut self, cx: &mut Context<Self>) {
        let config = match &mut self.phase {
            Phase::Connecting(conn) => {
                conn.attempt += 1;
                conn.status = ConnectStatus::InProgress;
                conn.config.clone()
            }
            _ => return,
        };
        self.connect_gen += 1;
        self.service.send(Command::Connect(config));
        cx.notify();
    }

    /// Abandon an in-progress connection (the splash "Cancel" button): bump the
    /// generation so any pending retry is dropped, tell the backend to discard
    /// the session it may still be opening, and return to the connect screen.
    pub(crate) fn cancel_connect(&mut self, cx: &mut Context<Self>) {
        self.connect_gen += 1;
        self.service.send(Command::Disconnect);
        self.phase = Phase::Disconnected;
        cx.notify();
    }

    pub(crate) fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.service.send(Command::Disconnect);
        cx.notify();
    }

    /// Run the destructive statement the user confirmed.
    pub(crate) fn confirm_destructive(&mut self, cx: &mut Context<Self>) {
        if let Some(sql) = self.confirm_exec.take() {
            self.execute_sql(sql, cx);
        }
        cx.notify();
    }

    pub(crate) fn cancel_destructive(&mut self, cx: &mut Context<Self>) {
        self.confirm_exec = None;
        cx.notify();
    }

    // --- query tabs ---

    /// Focus tab `index`. Its editor and result become the visible ones.
    pub(crate) fn set_active_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if index < active.tabs.len() {
                active.active_tab = index;
            }
        }
        cx.notify();
    }

    /// Push a freshly-built tab, focus it, and seed its completions. Returns the
    /// new index. Callers supply the tab (a blank query or a table preview).
    pub(crate) fn push_tab(&mut self, tab: QueryTab, cx: &mut Context<Self>) -> usize {
        let index = match &mut self.phase {
            Phase::Connected(active) => {
                active.tabs.push(tab);
                active.active_tab = active.tabs.len() - 1;
                active.active_tab
            }
            _ => return 0,
        };
        // New editor needs the current schema's completion candidates installed.
        self.refresh_completions(cx);
        index
    }

    /// Open a blank query tab (the tab-strip "＋" action).
    pub(crate) fn new_query(&mut self, cx: &mut Context<Self>) {
        let tab = match &mut self.phase {
            Phase::Connected(active) => {
                active.query_seq += 1;
                QueryTab::new(format!("query {}", active.query_seq), cx)
            }
            _ => return,
        };
        self.push_tab(tab, cx);
        cx.notify();
    }

    /// The tab-strip "×": close immediately if pristine, else ask first.
    pub(crate) fn request_close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let pristine = match &self.phase {
            Phase::Connected(active) => active
                .tabs
                .get(index)
                .map(|t| t.is_pristine(cx))
                .unwrap_or(true),
            _ => return,
        };
        if pristine {
            self.close_tab(index, cx);
        } else {
            self.confirm_close_tab = Some(index);
            cx.notify();
        }
    }

    /// Confirmation accepted — close the tab that was awaiting it.
    pub(crate) fn confirm_close(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.confirm_close_tab.take() {
            self.close_tab(index, cx);
        }
    }

    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.confirm_close_tab = None;
        cx.notify();
    }

    /// Drop tab `index`, freeing its backend result. The last tab never vanishes —
    /// closing it leaves one fresh blank tab behind, so the shell always has one.
    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        self.confirm_close_tab = None;
        let (closed_epoch, replace) = match &mut self.phase {
            Phase::Connected(active) if index < active.tabs.len() => {
                let removed = active.tabs.remove(index);
                let closed_epoch = removed.result.map(|g| g.epoch);
                let replace = if active.tabs.is_empty() {
                    active.query_seq = 1;
                    true
                } else {
                    // Keep the focus stable: clamp, and shift left if we removed a
                    // tab at or before the focused one.
                    if active.active_tab >= index && active.active_tab > 0 {
                        active.active_tab -= 1;
                    }
                    active.active_tab = active.active_tab.min(active.tabs.len() - 1);
                    false
                };
                (closed_epoch, replace)
            }
            _ => return,
        };
        // Free the backend result that backed the closed tab's grid.
        if let Some(epoch) = closed_epoch {
            self.service.send(Command::CloseResult { epoch });
        }
        if replace {
            let tab = QueryTab::new("query 1".to_string(), cx);
            self.push_tab(tab, cx);
        }
        cx.notify();
    }

    // --- settings panel ---

    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = true;
        cx.notify();
    }

    pub(crate) fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        cx.notify();
    }

    pub(crate) fn set_settings_tab(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        self.settings_tab = tab;
        cx.notify();
    }

    /// Make `name` the active theme, persist the choice, and re-render.
    pub(crate) fn select_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        cx.set_global(crate::theme::by_name(name));
        self.settings.theme = name.to_string();
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_density(&mut self, density: Density, cx: &mut Context<Self>) {
        self.settings.density = density.index() as u8;
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_confirm_destructive(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.confirm_destructive = on;
        self.save_settings();
        cx.notify();
    }

    /// Persist the current preferences. A write failure is logged, not surfaced —
    /// preferences are convenience, and the in-memory value already took effect.
    fn save_settings(&self) {
        if let Some(store) = &self.settings_store {
            if let Err(e) = store.save(&self.settings) {
                tracing::warn!("failed to save settings: {e}");
            }
        }
    }

    /// Save the connection list, surfacing a write failure as a toast.
    fn persist(&mut self) {
        if let Err(e) = config::save(&self.connections) {
            tracing::warn!("failed to save connections: {e}");
            self.toast = Some((
                format!("Couldn't save connections: {e}").into(),
                ToastVariant::Error,
            ));
        }
    }
}
