//! The root view and app state machine. `AppState` owns the backend handle, the
//! persisted connection list, and the current `Phase` (disconnected connect
//! screen ↔ connecting ↔ connected shell). Backend events are drained on a
//! foreground `cx.spawn` task into [`AppState::on_event`] — the one place where
//! the service drives UI state. Screen rendering lives in `connect.rs` / `shell.rs`.

use flint::prelude::*;
use flint::{CodeEditor, CodeEditorEvent};
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use gpui::{
    div, prelude::*, px, AsyncApp, Context, Entity, Pixels, SharedString, WeakEntity, Window,
};
use red_core::{ConnectionConfig, DbKind};
use red_service::{Command, Event, ServiceHandle};

use crate::assets::{FONT_MONO, FONT_UI};
use crate::config::{self, StoredConnection};
use crate::result::ResultGrid;
use crate::schema::SchemaState;
use crate::settings::{Density, FileSettingsStore, Settings};
use crate::settings_ui::SettingsTab;

/// Which top-level screen is showing.
pub(crate) enum Phase {
    Disconnected,
    Connecting { config: ConnectionConfig },
    // Boxed: `ActiveConn` carries the whole schema model, dwarfing the other
    // variants — box it to keep `Phase` small.
    Connected(Box<ActiveConn>),
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

/// The default editor stub a fresh query tab opens with. A tab still holding
/// exactly this (and no result) is "pristine" — closing it needs no confirmation.
pub(crate) const EMPTY_QUERY: &str = "-- Write SQL, ⌘↵ to run\n";

/// One query tab: its own SQL editor, result grid, and history. A connection
/// holds several of these (M9); the schema sidebar and split sizes are shared.
pub(crate) struct QueryTab {
    /// Tab label: "query N" for a blank tab, or "schema.table" for a preview.
    pub title: String,
    /// The SQL editor surface (M4), with the RED highlighter installed.
    pub editor: Entity<CodeEditor>,
    /// The open result browsed in the grid (M5): a table preview or an editor run.
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

    /// A blank tab the user hasn't touched — no result and the default stub still
    /// in the editor. Closing one of these doesn't warrant a confirmation.
    pub(crate) fn is_pristine(&self, cx: &Context<AppState>) -> bool {
        self.result.is_none() && self.editor.read(cx).content() == EMPTY_QUERY
    }
}

/// The live-connection view state: which connection, its engine version, the
/// resizable split sizes (caller-owned, per `SplitPane`'s stateless contract),
/// the schema explorer (M3), and the open query tabs (M9).
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
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
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
            }
        })
        .detach();
    }

    /// The single point where backend events drive UI state.
    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Connected { version } => {
                if let Phase::Connecting { config } =
                    std::mem::replace(&mut self.phase, Phase::Disconnected)
                {
                    self.phase = Phase::Connected(Box::new(ActiveConn::new(config, version, cx)));
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
                if matches!(self.phase, Phase::Connecting { .. }) {
                    self.phase = Phase::Disconnected;
                }
                self.on_result_error(&message);
                self.toast = Some((message.into(), ToastVariant::Error));
            }

            // --- schema explorer (M3) ---
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

            // --- result grid (M5) ---
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
            // Keyset runs (M10): extend/relocate a grid's resident row run.
            Event::ResultRunLoaded {
                epoch,
                fetch,
                rows,
                estimated,
                seq,
            } => self.on_result_run(epoch, fetch, rows, estimated, seq, cx),
            Event::ResultRunFailed { epoch, seq } => self.on_result_run_failed(epoch, seq),

            // --- export & writes (M6) ---
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

    /// Set every form text input in one go.
    fn fill_form_inputs(&mut self, config: &ConnectionConfig, cx: &mut Context<Self>) {
        let port = config.port.map(|p| p.to_string()).unwrap_or_default();
        self.name_input
            .update(cx, |i, cx| i.set_content(config.name.clone(), cx));
        self.host_input
            .update(cx, |i, cx| i.set_content(config.host.clone(), cx));
        self.port_input.update(cx, |i, cx| i.set_content(port, cx));
        self.user_input
            .update(cx, |i, cx| i.set_content(config.user.clone(), cx));
        self.password_input
            .update(cx, |i, cx| i.set_content(config.password.clone(), cx));
        self.database_input
            .update(cx, |i, cx| i.set_content(config.database.clone(), cx));
        // Seed the connection-string mirror for network engines once host/db are
        // set (an empty new form leaves it blank so the placeholder shows).
        let conn_str = if config.kind.is_file() || config.host.is_empty() {
            String::new()
        } else {
            config.dsn()
        };
        self.conn_str_input
            .update(cx, |i, cx| i.set_content(conn_str, cx));
    }

    pub(crate) fn open_new_form(&mut self, cx: &mut Context<Self>) {
        let kind = DbKind::Postgres;
        self.fill_form_inputs(
            &ConnectionConfig {
                kind,
                port: kind.default_port(),
                ..Default::default()
            },
            cx,
        );
        self.form = Some(FormState {
            kind,
            color: 3,
            // Read-only by default — RED's safe-by-default posture.
            read_only: true,
            editing: None,
            test: TestState::Idle,
        });
        cx.notify();
    }

    pub(crate) fn open_edit_form(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(stored) = self.connections.get(index) else {
            return;
        };
        let config = stored.config.clone();
        self.fill_form_inputs(&config, cx);
        self.form = Some(FormState {
            kind: config.kind,
            color: config.color,
            read_only: config.read_only,
            editing: Some(index),
            test: TestState::Idle,
        });
        cx.notify();
    }

    pub(crate) fn close_form(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        cx.notify();
    }

    /// Read the form's inputs + state into a `ConnectionConfig`. Unused fields for
    /// the current engine (host/port for SQLite) come through empty/`None`.
    pub(crate) fn form_config(&self, cx: &Context<Self>) -> Option<ConnectionConfig> {
        let form = self.form.as_ref()?;
        let read = |input: &Entity<TextInput>| input.read(cx).content().trim().to_string();
        let port = read(&self.port_input).parse::<u16>().ok();
        Some(ConnectionConfig {
            name: read(&self.name_input),
            kind: form.kind,
            host: read(&self.host_input),
            port: if form.kind.is_file() { None } else { port },
            user: read(&self.user_input),
            // Passwords may legitimately contain leading/trailing spaces — don't trim.
            password: self.password_input.read(cx).content().to_string(),
            database: read(&self.database_input),
            color: form.color,
            read_only: form.read_only,
        })
    }

    /// Whether `config` has the minimum to attempt a connection — `None` when ok,
    /// else the reason to surface. A file needs a path; a server needs a host; a
    /// Postgres connection also needs a database (it connects to one), while MySQL
    /// can browse the whole server so its database is optional.
    pub(crate) fn form_invalid_reason(config: &ConnectionConfig) -> Option<&'static str> {
        if config.kind.is_file() {
            return config
                .database
                .is_empty()
                .then_some("A database file path is required");
        }
        if config.host.is_empty() {
            return Some("Host is required");
        }
        if config.kind == DbKind::Postgres && config.database.is_empty() {
            return Some("Database is required");
        }
        None
    }

    /// Convenience predicate over [`Self::form_invalid_reason`] for Save/Connect
    /// button enablement.
    pub(crate) fn form_valid(config: &ConnectionConfig) -> bool {
        Self::form_invalid_reason(config).is_none()
    }

    /// Persist the form. `connect` also opens the connection on success. On a
    /// validation miss the modal stays open with a toast so the user can fix it.
    pub(crate) fn save_form(&mut self, connect: bool, cx: &mut Context<Self>) {
        let Some(config) = self.form_config(cx) else {
            return;
        };
        if config.name.is_empty() {
            self.form_error("A name is required", cx);
            return;
        }
        if let Some(reason) = Self::form_invalid_reason(&config) {
            self.form_error(reason, cx);
            return;
        }

        let editing = self.form.as_ref().and_then(|f| f.editing);
        let index = match editing {
            Some(index) if index < self.connections.len() => {
                self.connections[index].config = config;
                index
            }
            _ => {
                self.connections.push(StoredConnection {
                    config,
                    last_accessed: None,
                });
                self.connections.len() - 1
            }
        };
        self.form = None;
        self.persist();
        if connect {
            self.connect(index, cx);
        }
        cx.notify();
    }

    /// Surface a form validation error without closing the modal.
    fn form_error(&mut self, message: &str, cx: &mut Context<Self>) {
        self.toast = Some((message.to_string().into(), ToastVariant::Error));
        cx.notify();
    }

    pub(crate) fn set_form_kind(&mut self, kind: DbKind, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.kind = kind;
            form.test = TestState::Idle;
        }
        // Reset the port to the engine default so switching engines doesn't leave a
        // stale port behind (matches the design).
        let port = kind
            .default_port()
            .map(|p| p.to_string())
            .unwrap_or_default();
        self.port_input.update(cx, |i, cx| i.set_content(port, cx));
        // The scheme changed, so refresh the connection-string mirror.
        self.sync_conn_str_from_fields(cx);
        cx.notify();
    }

    /// Rebuild the connection-string mirror from the structured field inputs.
    /// Network engines only; a no-op while the form is closed or file-based.
    fn sync_conn_str_from_fields(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if form.kind.is_file() {
            return;
        }
        let Some(config) = self.form_config(cx) else {
            return;
        };
        let dsn = config.dsn();
        self.conn_str_input
            .update(cx, |i, cx| i.set_content(dsn, cx));
    }

    /// Parse the connection-string mirror back into the structured field inputs.
    /// Leaves the fields untouched while the string isn't yet a recognizable URL,
    /// so partial typing doesn't wipe them.
    fn sync_fields_from_conn_str(&mut self, cx: &mut Context<Self>) {
        if self.form.is_none() {
            return;
        }
        let raw = self.conn_str_input.read(cx).content().to_string();
        let Some(parsed) = ConnectionConfig::parse_conn_str(&raw) else {
            return;
        };
        let port = parsed.port.map(|p| p.to_string()).unwrap_or_default();
        self.host_input
            .update(cx, |i, cx| i.set_content(parsed.host, cx));
        self.port_input.update(cx, |i, cx| i.set_content(port, cx));
        self.user_input
            .update(cx, |i, cx| i.set_content(parsed.user, cx));
        self.password_input
            .update(cx, |i, cx| i.set_content(parsed.password, cx));
        self.database_input
            .update(cx, |i, cx| i.set_content(parsed.database, cx));
        if let Some(form) = &mut self.form {
            form.kind = parsed.kind;
            form.test = TestState::Idle;
        }
        cx.notify();
    }

    pub(crate) fn set_form_color(&mut self, color: u8, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.color = color;
        }
        cx.notify();
    }

    pub(crate) fn set_form_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.read_only = read_only;
        }
        cx.notify();
    }

    /// Fire a throwaway connection probe for the current form values.
    pub(crate) fn test_connection(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.form_config(cx) else {
            return;
        };
        if let Some(reason) = Self::form_invalid_reason(&config) {
            if let Some(form) = &mut self.form {
                form.test = TestState::Fail(reason.into());
            }
            cx.notify();
            return;
        }
        if let Some(form) = &mut self.form {
            form.test = TestState::Testing;
        }
        self.service.send(Command::TestConnection(config));
        cx.notify();
    }

    pub(crate) fn delete_connection(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.connections.len() {
            self.connections.remove(index);
            self.persist();
            cx.notify();
        }
    }

    pub(crate) fn connect(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(stored) = self.connections.get_mut(index) else {
            return;
        };
        stored.last_accessed = Some(config::now());
        let config = stored.config.clone();
        self.persist();
        self.service.send(Command::Connect(config.clone()));
        self.phase = Phase::Connecting { config };
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

    // --- query tabs (M9) ---

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

    fn render_connecting(&self, name: SharedString, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .bg(theme.bg_app)
            .font_family(FONT_UI)
            .child(
                div()
                    .text_color(theme.text)
                    .child(format!("Connecting to {name}…")),
            )
    }
}

impl Render for AppState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let screen = match &self.phase {
            Phase::Disconnected => self.render_connect(cx).into_any_element(),
            Phase::Connecting { config } => self
                .render_connecting(config.name.clone().into(), cx)
                .into_any_element(),
            Phase::Connected(active) => self.render_shell(active, cx).into_any_element(),
        };

        // Dismissible error toast, anchored bottom-center over whatever screen.
        let toast = self.toast.clone().map(|(message, variant)| {
            div()
                .absolute()
                .bottom_4()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("toast-dismiss")
                        .cursor_pointer()
                        .child(Toast::new(message).variant(variant))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toast = None;
                            cx.notify();
                        })),
                )
        });

        let confirm = self
            .confirm_exec
            .clone()
            .map(|sql| self.render_confirm(sql, cx));

        let confirm_close = self
            .confirm_close_tab
            .and_then(|i| self.tab_title(i))
            .map(|title| self.render_confirm_close(title, cx));

        let settings = self
            .settings_open
            .then(|| self.render_settings(cx).into_any_element());

        let theme = cx.theme();
        div()
            .size_full()
            .relative()
            .bg(theme.bg_app)
            .text_color(theme.text)
            .font_family(FONT_UI)
            // The design's base font size is 13px; GPUI defaults to 16px, so set
            // it once at the root and any unsized text inherits the right scale.
            .text_size(px(13.))
            .child(screen)
            .children(toast)
            .children(confirm)
            .children(confirm_close)
            .children(settings)
    }
}

impl AppState {
    /// The title of tab `index`, if it exists — for the close-confirm prompt.
    fn tab_title(&self, index: usize) -> Option<String> {
        match &self.phase {
            Phase::Connected(active) => active.tabs.get(index).map(|t| t.title.clone()),
            _ => None,
        }
    }

    /// Confirmation before closing a tab that holds real work (M9). Mirrors the
    /// destructive-statement modal's shape.
    fn render_confirm_close(&self, title: String, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let body = div().text_color(theme.text_muted).child(format!(
            "“{title}” has a query or result that will be lost. Close it?"
        ));
        let footer = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("close-cancel", "Keep tab")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_close(cx))),
            )
            .child(
                Button::new("close-confirm", "Close tab")
                    .variant(ButtonVariant::Danger)
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_close(cx))),
            );
        Modal::new("confirm-close-tab")
            .title("Close tab")
            .width(px(420.))
            .footer(footer)
            .on_close(move |_, cx| {
                close_view.update(cx, |this, cx| this.cancel_close(cx)).ok();
            })
            .child(body)
    }

    /// The destructive-statement confirmation modal (M6 safety rail).
    fn render_confirm(&self, sql: String, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let preview: String = sql.chars().take(200).collect();
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child("This statement modifies data and can't be undone. Run it?"),
            )
            .child(
                div()
                    .p_2()
                    .rounded(theme.radius_sm)
                    .bg(theme.bg_input)
                    .font_family(FONT_MONO)
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(preview),
            );
        let footer = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("confirm-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_destructive(cx))),
            )
            .child(
                Button::new("confirm-run", "Run statement")
                    .variant(ButtonVariant::Danger)
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_destructive(cx))),
            );
        Modal::new("confirm-destructive")
            .title("Confirm destructive statement")
            .width(px(440.))
            .footer(footer)
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.cancel_destructive(cx))
                    .ok();
            })
            .child(body)
    }
}
