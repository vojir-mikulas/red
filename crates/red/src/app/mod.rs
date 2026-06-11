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
    prelude::*, px, AsyncApp, Context, ElementId, Entity, FocusHandle, PathPromptOptions, Pixels,
    ScrollHandle, SharedString, WeakEntity, Window, WindowAppearance,
};
use red_core::{ConnectionConfig, DbKind};
use red_service::{Command, Event, ServiceHandle};

use crate::config::{self, StoredConnection};
use crate::palette::Cmd;
use crate::result::ResultGrid;
use crate::schema::SchemaState;
use crate::settings::{Density, FileSettingsStore, Settings, ThemeMode, ThemeSetting};
use crate::settings_ui::SettingsTab;
use crate::theme::ThemeRegistry;

/// Which theme picker (light / dark) is open in the settings panel, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemeSelect {
    Light,
    Dark,
}

/// Which font-family picker (UI / editor) is open in the settings panel, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FontSelect {
    Ui,
    Editor,
}

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
    /// While a tab is being dragged, the gap (insertion index `0..=tabs.len()`)
    /// where it would land — drives the drop indicator. Only meaningful when a
    /// drag is active; the strip gates rendering on `has_active_drag`.
    pub tab_drop_target: Option<usize>,
    /// Horizontal scroll position of the tab strip, so a crowded strip scrolls
    /// instead of squashing tabs. Persists across renders.
    pub tab_scroll: ScrollHandle,
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
            tab_drop_target: None,
            tab_scroll: ScrollHandle::new(),
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
    /// Persisted UI preferences (theme, grid, query, the safety rail) + their store.
    pub(crate) settings: Settings,
    pub(crate) settings_store: Option<FileSettingsStore>,
    pub(crate) settings_open: bool,
    pub(crate) settings_tab: SettingsTab,
    /// Non-fatal problems from the last settings load (an unreadable section, a
    /// bad value) — surfaced as a dismissible banner so a hand-edit gets feedback
    /// instead of a silent reset.
    pub(crate) settings_warnings: Vec<String>,
    /// Whether the OS is in a dark appearance, for `theme = { mode = "system" }`.
    pub(crate) os_dark: bool,
    /// Installed once on first render: keeps the OS-appearance observer alive so
    /// `mode = system` re-themes when the user flips light/dark.
    pub(crate) appearance_sub: Option<gpui::Subscription>,
    /// Live-reload watcher over `settings.toml`, plus the self-write guard that
    /// suppresses the reload our own atomic save would otherwise trigger.
    pub(crate) settings_watcher: Option<crate::settings_watch::SettingsWatcher>,
    /// One-shot guard so the appearance observer + file-watcher install on the
    /// first render (when a `Window` exists) rather than on every frame.
    pub(crate) observers_installed: bool,
    /// Built-in + imported themes, resolved for the light/dark pickers and the
    /// theme manager. Rebuilt on import / remove.
    pub(crate) themes: ThemeRegistry,
    /// Which theme picker dropdown is open in the panel, if any.
    pub(crate) theme_select_open: Option<ThemeSelect>,
    /// Which font-family picker dropdown is open in the panel, if any.
    pub(crate) font_select_open: Option<FontSelect>,
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
    /// The command palette overlay, when open, plus the `id → Cmd` map for the
    /// commands it's currently showing (so an activation routes to the right one).
    pub(crate) palette: Option<Entity<Palette>>,
    pub(crate) palette_cmds: Vec<(ElementId, Cmd)>,
    /// Set when an overlay closed: the next render pulls focus back to the root
    /// so the global ⌘K keeps dispatching (see `close_palette`).
    pub(crate) refocus_root: bool,
    /// Dev-only perf HUD collector — brackets `render` to read build time and
    /// allocation churn. Compiled only under the `dev-stats` feature.
    #[cfg(feature = "dev-stats")]
    pub(crate) dev_stats: crate::dev_stats::DevStats,
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
        let os_dark = matches!(
            cx.window_appearance(),
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
        );
        let themes = ThemeRegistry::load();
        cx.set_global(themes.resolve(&settings.appearance.theme, os_dark));

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
            settings_warnings: report.warnings,
            os_dark,
            appearance_sub: None,
            settings_watcher: None,
            observers_installed: false,
            themes,
            theme_select_open: None,
            font_select_open: None,
            font_names_cache: None,
            query_ticking: false,
            connect_gen: 0,
            root_focus: cx.focus_handle(),
            palette: None,
            palette_cmds: Vec::new(),
            // Focus the root on first paint so the very first ⌘K dispatches.
            refocus_root: true,
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
                self.prefetch_table_details();
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

    /// Point the drop indicator at `gap` (an insertion index `0..=tabs.len()`)
    /// while a tab drag hovers the strip. Notifies only on change to keep the
    /// per-move churn cheap.
    pub(crate) fn set_tab_drop_target(&mut self, gap: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if active.tab_drop_target != Some(gap) {
                active.tab_drop_target = Some(gap);
                cx.notify();
            }
        }
    }

    /// Drop the drop indicator (cursor left the tab strip mid-drag). Notifies
    /// only when something was showing.
    pub(crate) fn clear_tab_drop_target(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if active.tab_drop_target.take().is_some() {
                cx.notify();
            }
        }
    }

    /// Finish a tab-strip drag: move the dragged tab (`from`) into the gap the
    /// indicator settled on. The dragged tab follows the cursor and stays
    /// focused. Clears the indicator regardless.
    pub(crate) fn drop_tab(&mut self, from: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(gap) = active.tab_drop_target.take() {
                if from < active.tabs.len() {
                    // `gap` indexes the pre-removal strip; shift left when the
                    // dragged tab sat before the gap.
                    let dest = if from < gap { gap - 1 } else { gap };
                    let dest = dest.min(active.tabs.len() - 1);
                    let tab = active.tabs.remove(from);
                    active.tabs.insert(dest, tab);
                    active.active_tab = dest;
                }
            }
        }
        cx.notify();
    }

    /// Push a freshly-built tab, focus it, and seed its completions. Returns the
    /// new index. Callers supply the tab (a blank query or a table preview).
    /// Eagerly describe every table once the skeleton lands, so column and
    /// `table.` completion covers the whole schema without the user expanding
    /// each node first. Details arrive as `TableDescribed` events that refresh the
    /// completion index. Capped so a pathological schema can't flood the backend —
    /// past the cap, tables still load lazily on tree expansion.
    pub(crate) fn prefetch_table_details(&mut self) {
        const MAX_PREFETCH: usize = 200;
        let pending: Vec<(String, String)> = match &self.phase {
            Phase::Connected(active) => {
                let s = &active.schema;
                s.schemas
                    .iter()
                    .flat_map(|sc| {
                        sc.objects
                            .iter()
                            .map(move |obj| (sc.name.clone(), obj.name.clone()))
                    })
                    .filter(|key| !s.details.contains_key(key))
                    .take(MAX_PREFETCH)
                    .collect()
            }
            _ => return,
        };
        for (schema, table) in pending {
            self.service.send(Command::DescribeTable { schema, table });
        }
    }

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

    // --- settings: live observers ---

    /// Install the OS-appearance observer and the `settings.toml` file-watcher on
    /// the first render, when a `Window` is available. The appearance observer
    /// keeps `mode = system` honest; the watcher re-applies hand edits live.
    pub(crate) fn ensure_observers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.observers_installed {
            return;
        }
        self.observers_installed = true;

        let weak = cx.entity().downgrade();
        let sub = window.observe_window_appearance(move |window, cx| {
            let dark = matches!(
                window.appearance(),
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            );
            weak.update(cx, |this, cx| {
                if dark != this.os_dark {
                    this.os_dark = dark;
                    this.apply_theme(cx);
                    cx.notify();
                }
            })
            .ok();
        });
        self.appearance_sub = Some(sub);

        if let Some(store) = &self.settings_store {
            if let Some((watcher, mut rx)) =
                crate::settings_watch::SettingsWatcher::start(store.path().to_path_buf())
            {
                self.settings_watcher = Some(watcher);
                cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                    while rx.next().await.is_some() {
                        if this
                            .update(cx, |this, cx| this.reload_settings(cx))
                            .is_err()
                        {
                            break; // view dropped — window closed
                        }
                    }
                })
                .detach();
            }
        }
    }

    /// Re-read `settings.toml` after an external edit and re-apply. Theme is
    /// reinstalled here; per-frame settings (density, null display, page size)
    /// take effect on the next render via `cx.notify`.
    pub(crate) fn reload_settings(&mut self, cx: &mut Context<Self>) {
        let Some(store) = &self.settings_store else {
            return;
        };
        let report = store.load_report();
        self.settings = report.settings;
        self.settings_warnings = report.warnings;
        self.apply_theme(cx);
        cx.notify();
    }

    // --- settings: file-first workflow ---

    /// Open `settings.toml` in the user's editor, seeding it with the commented
    /// reference defaults on first open so there's a full key set to edit.
    pub(crate) fn open_settings_file(&mut self, cx: &mut Context<Self>) {
        let Some(store) = &self.settings_store else {
            self.toast = Some((
                "No config directory available on this platform.".into(),
                ToastVariant::Error,
            ));
            cx.notify();
            return;
        };
        let path = store.path().to_path_buf();
        if !path.exists() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Announce the seed so the watcher doesn't echo it back as an edit.
            if let Some(watcher) = &self.settings_watcher {
                watcher.note_self_write(crate::assets::DEFAULT_SETTINGS);
            }
            if let Err(e) = std::fs::write(&path, crate::assets::DEFAULT_SETTINGS) {
                tracing::warn!("failed to seed settings file: {e}");
            }
        }
        self.reveal_path(&path, cx);
    }

    /// Open the bundled, fully-commented reference defaults — RED's settings docs.
    pub(crate) fn open_default_settings(&mut self, cx: &mut Context<Self>) {
        let path = std::env::temp_dir().join("red-default-settings.toml");
        if let Err(e) = std::fs::write(&path, crate::assets::DEFAULT_SETTINGS) {
            tracing::warn!("failed to materialize default settings: {e}");
            self.toast = Some((
                format!("Couldn't open default settings: {e}").into(),
                ToastVariant::Error,
            ));
            cx.notify();
            return;
        }
        self.reveal_path(&path, cx);
    }

    /// Hand `path` to the OS to open with its default handler (best-effort).
    fn reveal_path(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        if let Err(e) = open_in_os(path) {
            tracing::warn!("failed to open {}: {e}", path.display());
            self.toast = Some((
                format!("Couldn't open {}: {e}", path.display()).into(),
                ToastVariant::Error,
            ));
        }
        cx.notify();
    }

    // --- settings panel ---

    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = true;
        // Warm the font-name cache once, off the render path (the Appearance tab
        // would otherwise re-enumerate every installed face on every frame).
        if self.font_names_cache.is_none() {
            let mut names = cx.text_system().all_font_names();
            names.sort_unstable();
            names.dedup();
            self.font_names_cache = Some(names);
        }
        cx.notify();
    }

    /// The cached sorted/deduped installed font families (see [`Self::open_settings`]).
    pub(crate) fn font_names(&self) -> &[String] {
        self.font_names_cache.as_deref().unwrap_or(&[])
    }

    pub(crate) fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        cx.notify();
    }

    pub(crate) fn set_settings_tab(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        self.settings_tab = tab;
        cx.notify();
    }

    /// Re-resolve the active theme from settings + OS appearance and install it.
    pub(crate) fn apply_theme(&self, cx: &mut Context<Self>) {
        cx.set_global(
            self.themes
                .resolve(&self.settings.appearance.theme, self.os_dark),
        );
    }

    /// The `(mode, light, dark)` the current setting implies. The panel always
    /// edits a light/dark pair, so a bare named theme is decomposed into one
    /// (filling the other slot from the registry's default for that family).
    fn theme_decompose(&self) -> (ThemeMode, String, String) {
        match &self.settings.appearance.theme {
            ThemeSetting::Modal { mode, light, dark } => (*mode, light.clone(), dark.clone()),
            ThemeSetting::Named(name) if self.themes.is_light(name) => (
                ThemeMode::Light,
                name.clone(),
                self.themes.default_name(false),
            ),
            ThemeSetting::Named(name) => (
                ThemeMode::Dark,
                self.themes.default_name(true),
                name.clone(),
            ),
        }
    }

    /// The active appearance mode (System / Light / Dark) — drives the segmented.
    pub(crate) fn theme_mode(&self) -> ThemeMode {
        self.theme_decompose().0
    }

    /// The currently-selected theme name for a family — drives the pickers.
    pub(crate) fn selected_theme(&self, light: bool) -> String {
        let (_, l, d) = self.theme_decompose();
        if light {
            l
        } else {
            d
        }
    }

    /// Store a full `(mode, light, dark)` pair, apply it, and persist.
    fn set_theme_pair(
        &mut self,
        mode: ThemeMode,
        light: String,
        dark: String,
        cx: &mut Context<Self>,
    ) {
        self.settings.appearance.theme = ThemeSetting::Modal { mode, light, dark };
        self.apply_theme(cx);
        self.save_settings();
        cx.notify();
    }

    /// Switch how the theme tracks the OS — `System` follows the OS light/dark,
    /// `Light`/`Dark` pin that family. The pair carries across so the user's two
    /// choices survive a mode flip.
    pub(crate) fn set_theme_mode(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        let (_, light, dark) = self.theme_decompose();
        self.set_theme_pair(mode, light, dark, cx);
    }

    /// Choose the light-appearance theme (used in Light and System-on-light modes).
    pub(crate) fn set_light_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let (mode, _, dark) = self.theme_decompose();
        self.theme_select_open = None;
        self.set_theme_pair(mode, name.to_string(), dark, cx);
    }

    /// Choose the dark-appearance theme (used in Dark and System-on-dark modes).
    pub(crate) fn set_dark_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let (mode, light, _) = self.theme_decompose();
        self.theme_select_open = None;
        self.set_theme_pair(mode, light, name.to_string(), cx);
    }

    /// Open/close a theme picker dropdown (the panel owns the open flag).
    pub(crate) fn toggle_theme_select(&mut self, which: ThemeSelect, cx: &mut Context<Self>) {
        self.theme_select_open = if self.theme_select_open == Some(which) {
            None
        } else {
            Some(which)
        };
        cx.notify();
    }

    /// Pick a theme file from disk, validate + copy it into the user themes dir,
    /// then reload the registry. Async (the native file dialog runs off-thread).
    pub(crate) fn import_theme(&mut self, cx: &mut Context<Self>) {
        self.theme_select_open = None;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import theme".into()),
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            if let Ok(Ok(Some(paths))) = paths.await {
                if let Some(path) = paths.into_iter().next() {
                    this.update(cx, |this, cx| this.finish_import(&path, cx))
                        .ok();
                }
            }
        })
        .detach();
    }

    /// Land an imported theme file: refresh the registry and re-apply (in case the
    /// import re-skinned the active theme). Toasts success or the validation error.
    fn finish_import(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        match ThemeRegistry::import(path) {
            Ok(name) => {
                self.themes = ThemeRegistry::load();
                self.apply_theme(cx);
                self.toast = Some((
                    format!("Imported theme “{name}”").into(),
                    ToastVariant::Success,
                ));
            }
            Err(e) => {
                self.toast = Some((
                    format!("Couldn't import theme: {e}").into(),
                    ToastVariant::Error,
                ));
            }
        }
        cx.notify();
    }

    /// Delete a user theme, reload the registry, and re-apply — a removed active
    /// theme falls back to the default rather than leaving a dangling reference.
    pub(crate) fn remove_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Err(e) = self.themes.remove(name) {
            self.toast = Some((
                format!("Couldn't remove theme: {e}").into(),
                ToastVariant::Error,
            ));
            cx.notify();
            return;
        }
        self.themes = ThemeRegistry::load();
        self.apply_theme(cx);
        self.toast = Some((
            format!("Removed theme “{name}”").into(),
            ToastVariant::Success,
        ));
        cx.notify();
    }

    pub(crate) fn set_density(&mut self, density: Density, cx: &mut Context<Self>) {
        self.settings.grid.density = density;
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_null_display(&mut self, value: &str, cx: &mut Context<Self>) {
        self.settings.grid.null_display = value.to_string();
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_auto_limit(&mut self, n: u32, cx: &mut Context<Self>) {
        self.settings.query.auto_limit = n;
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_confirm_destructive(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.query.confirm_destructive = on;
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_ui_font_family(&mut self, family: &str, cx: &mut Context<Self>) {
        self.settings.appearance.ui_font_family = family.to_string();
        self.font_select_open = None;
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_ui_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.settings.appearance.ui_font_size = size.clamp(
            crate::settings::MIN_FONT_SIZE,
            crate::settings::MAX_FONT_SIZE,
        );
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_editor_font_family(&mut self, family: &str, cx: &mut Context<Self>) {
        self.settings.editor.font_family = family.to_string();
        self.font_select_open = None;
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_editor_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.settings.editor.font_size = size.clamp(
            crate::settings::MIN_FONT_SIZE,
            crate::settings::MAX_FONT_SIZE,
        );
        self.save_settings();
        cx.notify();
    }

    /// Open/close a font-family picker dropdown (the panel owns the open flag).
    pub(crate) fn toggle_font_select(&mut self, which: FontSelect, cx: &mut Context<Self>) {
        self.font_select_open = if self.font_select_open == Some(which) {
            None
        } else {
            Some(which)
        };
        cx.notify();
    }

    /// Dismiss the settings-warning banner until the next problematic load.
    pub(crate) fn dismiss_settings_warnings(&mut self, cx: &mut Context<Self>) {
        self.settings_warnings.clear();
        cx.notify();
    }

    /// Persist the current preferences. A write failure is logged, not surfaced —
    /// preferences are convenience, and the in-memory value already took effect.
    /// The bytes are announced to the watcher first so the reload this write
    /// triggers is suppressed (no self-inflicted reload storm).
    pub(crate) fn save_settings(&self) {
        let Some(store) = &self.settings_store else {
            return;
        };
        if let Some(watcher) = &self.settings_watcher {
            if let Ok(serialized) = toml::to_string_pretty(&self.settings) {
                watcher.note_self_write(&serialized);
            }
        }
        if let Err(e) = store.save(&self.settings) {
            tracing::warn!("failed to save settings: {e}");
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
