// SPDX-License-Identifier: GPL-3.0-or-later

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

/// Which top-level screen is showing.
pub(crate) enum Phase {
    Disconnected,
    Connecting { config: ConnectionConfig },
    // Boxed: `ActiveConn` carries the whole schema model, dwarfing the other
    // variants — box it to keep `Phase` small.
    Connected(Box<ActiveConn>),
}

/// Add/edit connection form state. The name + DSN text live in the shared
/// `TextInput` entities on `AppState`; this holds the rest.
pub(crate) struct FormState {
    pub kind: DbKind,
    pub kind_open: bool,
    pub read_only: bool,
    /// `Some(index)` when editing an existing connection, `None` when adding.
    pub editing: Option<usize>,
}

/// The live-connection view state: which connection, its engine version, the
/// resizable split sizes (caller-owned, per `SplitPane`'s stateless contract),
/// the schema explorer (M3), and the current table preview (M3 interim results).
pub(crate) struct ActiveConn {
    pub config: ConnectionConfig,
    pub version: String,
    pub sidebar_w: Pixels,
    pub sidebar_drag: Option<DragAnchor>,
    pub editor_h: Pixels,
    pub editor_drag: Option<DragAnchor>,
    pub schema: SchemaState,
    /// The open result browsed in the grid (M5): a table preview or an editor run.
    pub result: Option<ResultGrid>,
    /// The SQL editor surface (M4), with the RED highlighter installed.
    pub editor: Entity<CodeEditor>,
    /// Recent queries (newest first), for re-run from the history popover.
    pub history: Vec<String>,
    pub history_open: bool,
}

impl ActiveConn {
    fn new(config: ConnectionConfig, version: String, cx: &mut Context<AppState>) -> Self {
        let editor = cx.new(|cx| {
            CodeEditor::new(cx)
                .highlighter(crate::sql::tokenize)
                .with_content("-- Write SQL, ⌘↵ to run\n")
        });
        // ⌘↵ in the editor runs the current statement / selection.
        cx.subscribe(&editor, |this, _editor, _event: &CodeEditorEvent, cx| {
            this.run_editor_query(cx)
        })
        .detach();

        Self {
            config,
            version,
            sidebar_w: px(240.),
            sidebar_drag: None,
            editor_h: px(300.),
            editor_drag: None,
            schema: SchemaState::new(cx),
            result: None,
            editor,
            history: Vec::new(),
            history_open: false,
        }
    }
}

pub struct AppState {
    pub(crate) service: ServiceHandle,
    pub(crate) connections: Vec<StoredConnection>,
    pub(crate) phase: Phase,
    pub(crate) name_input: Entity<TextInput>,
    pub(crate) dsn_input: Entity<TextInput>,
    pub(crate) form: Option<FormState>,
    pub(crate) toast: Option<(SharedString, ToastVariant)>,
    /// A destructive statement awaiting the user's confirmation before it runs.
    pub(crate) confirm_exec: Option<String>,
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

        Self {
            service,
            connections: config::load(),
            phase: Phase::Disconnected,
            name_input: cx.new(|cx| TextInput::new(cx).with_placeholder("My database")),
            dsn_input: cx.new(TextInput::new),
            form: None,
            toast: None,
            confirm_exec: None,
        }
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
            Event::ResultReady { columns, total } => self.on_result_ready(columns, total, cx),
            Event::ResultPageLoaded { offset, rows } => self.on_result_page(offset, rows, cx),

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

    pub(crate) fn open_new_form(&mut self, cx: &mut Context<Self>) {
        self.name_input.update(cx, |i, cx| i.set_content("", cx));
        self.dsn_input.update(cx, |i, cx| i.set_content("", cx));
        self.form = Some(FormState {
            kind: DbKind::Sqlite,
            kind_open: false,
            read_only: false,
            editing: None,
        });
        cx.notify();
    }

    pub(crate) fn open_edit_form(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(stored) = self.connections.get(index) else {
            return;
        };
        let config = stored.config.clone();
        self.name_input
            .update(cx, |i, cx| i.set_content(config.name.clone(), cx));
        self.dsn_input
            .update(cx, |i, cx| i.set_content(config.dsn.clone(), cx));
        self.form = Some(FormState {
            kind: config.kind,
            kind_open: false,
            read_only: config.read_only,
            editing: Some(index),
        });
        cx.notify();
    }

    pub(crate) fn close_form(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        cx.notify();
    }

    pub(crate) fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.take() else {
            return;
        };
        let name = self.name_input.read(cx).content().to_string();
        let dsn = self.dsn_input.read(cx).content().to_string();
        if name.trim().is_empty() || dsn.trim().is_empty() {
            self.toast = Some((
                "Name and connection target are required".into(),
                ToastVariant::Error,
            ));
            self.form = Some(form); // keep the modal open so the user can fix it
            cx.notify();
            return;
        }

        let config = ConnectionConfig {
            name,
            kind: form.kind,
            dsn,
            read_only: form.read_only,
        };
        match form.editing {
            Some(index) if index < self.connections.len() => {
                self.connections[index].config = config;
            }
            _ => self.connections.push(StoredConnection {
                config,
                last_accessed: None,
            }),
        }
        self.persist();
        cx.notify();
    }

    pub(crate) fn set_form_kind(&mut self, kind: DbKind, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.kind = kind;
            form.kind_open = false;
        }
        cx.notify();
    }

    pub(crate) fn toggle_form_kind_open(&mut self, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.kind_open = !form.kind_open;
        }
        cx.notify();
    }

    pub(crate) fn set_form_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.read_only = read_only;
        }
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

    pub(crate) fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        let next = match cx.theme().name.as_str() {
            "One Dark" => crate::theme::github_dark(),
            _ => crate::theme::one_dark(),
        };
        cx.set_global(next);
        cx.notify();
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
    }
}

impl AppState {
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
