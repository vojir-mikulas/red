//! Connection-form logic: filling the form's text inputs, reading them back into
//! a `ConnectionConfig`, the two-way sync between the structured fields and the
//! raw connection string, validation, and the test-connection probe.

use flint::prelude::*;
use gpui::{Context, Entity};
use red_core::{ConnectionConfig, DbKind};
use red_service::Command;

use crate::config::StoredConnection;

use super::{AppState, FormState, TestState};

impl AppState {
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
    pub(crate) fn sync_conn_str_from_fields(&mut self, cx: &mut Context<Self>) {
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
    pub(crate) fn sync_fields_from_conn_str(&mut self, cx: &mut Context<Self>) {
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
}
