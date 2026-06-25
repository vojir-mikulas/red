//! Connection-form logic: filling the form's text inputs, reading them back into
//! a `ConnectionConfig`, the two-way sync between the structured fields and the
//! raw connection string, validation, and the test-connection probe.

use flint::prelude::*;
use gpui::{Context, Entity};
use red_core::{ConnectionConfig, DbKind, SshAuth, SshConfig};
use red_service::Command;

use crate::config::StoredConnection;

use super::{AppState, FormField, FormState, SshAuthMode, TestState};

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
        self.fill_ssh_inputs(config.ssh.as_ref(), cx);
    }

    /// Set the SSH text inputs from a config's optional tunnel. A `None` (direct
    /// connection) clears them so a reused form doesn't show a prior host's data.
    fn fill_ssh_inputs(&mut self, ssh: Option<&SshConfig>, cx: &mut Context<Self>) {
        let default = SshConfig::default();
        let ssh = ssh.unwrap_or(&default);
        let port = if ssh.port == 0 {
            String::new()
        } else {
            ssh.port.to_string()
        };
        let key_path = match &ssh.auth {
            SshAuth::Key { path } => path.clone(),
            _ => String::new(),
        };
        self.ssh_host_input
            .update(cx, |i, cx| i.set_content(ssh.host.clone(), cx));
        self.ssh_port_input
            .update(cx, |i, cx| i.set_content(port, cx));
        self.ssh_user_input
            .update(cx, |i, cx| i.set_content(ssh.user.clone(), cx));
        self.ssh_key_path_input
            .update(cx, |i, cx| i.set_content(key_path, cx));
        self.ssh_password_input
            .update(cx, |i, cx| i.set_content(ssh.password.clone(), cx));
        self.ssh_passphrase_input
            .update(cx, |i, cx| i.set_content(ssh.passphrase.clone(), cx));
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
            submitted: false,
            test: TestState::Idle,
            ssh_enabled: false,
            ssh_auth: SshAuthMode::Agent,
            ai_allow_writes: false,
        });
        self.focus_name_field = true;
        cx.notify();
    }

    pub(crate) fn open_edit_form(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(stored) = self.connections.get(index) else {
            return;
        };
        let id = stored.id.clone();
        let mut config = stored.config.clone();
        // Materialize the stored password from the keychain so the form shows it
        // (and the Test/Save paths, which read the input, reuse it). A genuine miss
        // (`Ok(None)`) leaves the field blank silently; a read *error* — most often
        // the OS keychain prompt being denied — is surfaced, so a blank field is
        // explained rather than mistaken for "no password saved".
        let mut keychain_err = None;
        if config.password.is_empty() && !config.kind.is_file() {
            match crate::secrets::get_password(&id) {
                Ok(Some(pw)) => config.password = pw,
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("failed to read credential from keychain: {e}");
                    keychain_err = Some(e);
                }
            }
        }
        // Materialize the SSH secret matching this connection's auth mode, the same
        // way as the DB password. Agent auth has no secret to fetch.
        let ssh_auth = config.ssh.as_ref().map(|s| auth_mode(&s.auth));
        if let Some(ssh) = &mut config.ssh {
            let read = match &ssh.auth {
                SshAuth::Password => Some(crate::secrets::get_ssh_password(&id)),
                SshAuth::Key { .. } => Some(crate::secrets::get_ssh_passphrase(&id)),
                SshAuth::Agent => None,
            };
            match read {
                Some(Ok(Some(secret))) => match &ssh.auth {
                    SshAuth::Password => ssh.password = secret,
                    _ => ssh.passphrase = secret,
                },
                Some(Err(e)) => {
                    tracing::warn!("failed to read SSH secret from keychain: {e}");
                    keychain_err = keychain_err.or(Some(e));
                }
                _ => {}
            }
        }
        self.fill_form_inputs(&config, cx);
        if keychain_err.is_some() {
            self.notify(
                ToastVariant::Error,
                "Couldn't read a saved secret from the keychain — re-enter it to update it.",
                cx,
            );
        }
        self.form = Some(FormState {
            kind: config.kind,
            color: config.color,
            read_only: config.read_only,
            editing: Some(index),
            submitted: false,
            test: TestState::Idle,
            ssh_enabled: config.ssh.is_some(),
            ssh_auth: ssh_auth.unwrap_or(SshAuthMode::Agent),
            ai_allow_writes: config.ai_tier == Some(red_core::AiTier::Write),
        });
        self.focus_name_field = true;
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
        // The tunnel is offered only for network engines; a file engine has no host.
        let ssh = (form.ssh_enabled && !form.kind.is_file()).then(|| {
            let auth = match form.ssh_auth {
                SshAuthMode::Agent => SshAuth::Agent,
                SshAuthMode::Password => SshAuth::Password,
                SshAuthMode::Key => SshAuth::Key {
                    path: read(&self.ssh_key_path_input),
                },
            };
            let ssh_port = read(&self.ssh_port_input).parse::<u16>().unwrap_or(0);
            SshConfig {
                host: read(&self.ssh_host_input),
                port: if ssh_port == 0 { 22 } else { ssh_port },
                user: read(&self.ssh_user_input),
                auth,
                // Secrets keep any surrounding spaces — don't trim.
                password: self.ssh_password_input.read(cx).content().to_string(),
                passphrase: self.ssh_passphrase_input.read(cx).content().to_string(),
            }
        });
        // Per-connection AI overrides (M-S7). `ai_enabled` and a stricter
        // `off`/`schema` tier are still hand-set in connections.toml; carry the
        // editing connection's values through so a save doesn't drop them. The
        // **write** opt-in (Feature B) IS surfaced — the form checkbox sets
        // `ai_tier = "write"`, and clearing it reverts to inherit (unless a hand-set
        // off/schema is present, which is preserved).
        let prior = form
            .editing
            .and_then(|i| self.connections.get(i))
            .map(|c| (c.config.ai_enabled, c.config.ai_tier))
            .unwrap_or((None, None));
        let ai_enabled = prior.0;
        let ai_tier = if form.ai_allow_writes {
            Some(red_core::AiTier::Write)
        } else {
            // Unchecked: keep a hand-set off/schema, but drop a prior write opt-in.
            prior.1.filter(|t| *t != red_core::AiTier::Write)
        };
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
            ai_enabled,
            ai_tier,
            ssh,
        })
    }

    /// Every unmet requirement for `config`, each tagged with the field it belongs
    /// to so the form can render the message beneath that input. Empty when the
    /// form is ready to save. A file engine needs only a path; a server needs a
    /// host (and Postgres a database, since it connects to one — MySQL can browse
    /// the whole server, so its database is optional).
    pub(crate) fn form_errors(config: &ConnectionConfig) -> Vec<(FormField, &'static str)> {
        let mut errors = Vec::new();
        if config.name.is_empty() {
            errors.push((FormField::Name, "A name is required"));
        }
        if config.kind.is_file() {
            if config.database.is_empty() {
                errors.push((FormField::Database, "A database file path is required"));
            }
        } else {
            if config.host.is_empty() {
                errors.push((FormField::Host, "Host is required"));
            }
            if config.kind == DbKind::Postgres && config.database.is_empty() {
                errors.push((FormField::Database, "Database is required"));
            }
        }
        // `ssh` is `Some` only when the tunnel toggle is on, so these fire only then.
        if let Some(ssh) = &config.ssh {
            if ssh.host.is_empty() {
                errors.push((FormField::SshHost, "SSH host is required"));
            }
            if ssh.user.is_empty() {
                errors.push((FormField::SshUser, "SSH user is required"));
            }
            if let SshAuth::Key { path } = &ssh.auth {
                if path.is_empty() {
                    errors.push((FormField::SshKeyPath, "Key file path is required"));
                }
            }
        }
        errors
    }

    /// Enter pressed in a form field — submit the connection form via its primary
    /// action (Save & connect). No-op when the form isn't open; `save_form` itself
    /// validates and keeps the modal up with a toast on a miss.
    pub(crate) fn submit_form(&mut self, cx: &mut Context<Self>) {
        if self.form.is_some() {
            self.save_form(true, cx);
        }
    }

    /// Persist the form. `connect` also opens the connection on success. On a
    /// validation miss the modal stays open with a toast so the user can fix it.
    pub(crate) fn save_form(&mut self, connect: bool, cx: &mut Context<Self>) {
        let Some(mut config) = self.form_config(cx) else {
            return;
        };
        // Missing fields keep the modal open and surface inline beneath each input
        // (gated by `submitted`) rather than as a transient toast.
        if !Self::form_errors(&config).is_empty() {
            if let Some(form) = &mut self.form {
                form.submitted = true;
            }
            cx.notify();
            return;
        }

        // Split the secrets off the stored config: the DB password and any SSH
        // secrets go to the OS keychain (below), never the config file or
        // long-term memory.
        let password = std::mem::take(&mut config.password);
        let ssh_secrets = config.ssh.as_mut().map(|s| {
            (
                std::mem::take(&mut s.password),
                std::mem::take(&mut s.passphrase),
            )
        });
        let is_file = config.kind.is_file();

        let editing = self.form.as_ref().and_then(|f| f.editing);
        let index = match editing {
            Some(index) if index < self.connections.len() => {
                self.connections[index].config = config;
                index
            }
            _ => {
                self.connections.push(StoredConnection {
                    id: crate::config::new_id(),
                    config,
                    last_accessed: None,
                    pinned: false,
                });
                self.connections.len() - 1
            }
        };

        self.store_credential(index, &password, is_file, cx);
        self.store_ssh_credentials(index, ssh_secrets, cx);

        self.form = None;
        self.persist(cx);
        if connect {
            // connect() re-materializes the password from the keychain (or the
            // in-memory fallback kept when a keychain write fails).
            self.connect(index, cx);
        }
        cx.notify();
    }

    /// Route a saved connection's password to the OS keychain, keyed by its id.
    /// A blank password (or a file engine, which has none) clears any prior
    /// entry. If the keychain write fails we keep the password in memory for this
    /// session and warn, so the connection still works until next launch.
    fn store_credential(
        &mut self,
        index: usize,
        password: &str,
        is_file: bool,
        cx: &mut Context<Self>,
    ) {
        let id = self.connections[index].id.clone();
        if !is_file && !password.is_empty() {
            if let Err(e) = crate::secrets::set_password(&id, password) {
                tracing::warn!("failed to store credential in keychain: {e}");
                self.notify(
                    ToastVariant::Error,
                    "Couldn't save the password to the OS keychain — it won't be remembered.",
                    cx,
                );
                self.connections[index].config.password = password.to_string();
            }
        } else if let Err(e) = crate::secrets::delete_password(&id) {
            tracing::warn!("failed to clear keychain credential: {e}");
        }
    }

    /// Route the SSH secrets to the keychain, keyed by connection id. `secrets` is
    /// `None` when the tunnel is off — which clears both SSH entries. A non-empty
    /// secret is stored; an empty one clears its entry (e.g. agent auth, which has
    /// no secret). A keychain write failure warns but doesn't block the save.
    fn store_ssh_credentials(
        &mut self,
        index: usize,
        secrets: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let id = self.connections[index].id.clone();
        let (password, passphrase) = secrets.unwrap_or_default();
        self.store_ssh_secret(
            crate::secrets::set_ssh_password,
            crate::secrets::delete_ssh_password,
            &id,
            &password,
            cx,
        );
        self.store_ssh_secret(
            crate::secrets::set_ssh_passphrase,
            crate::secrets::delete_ssh_passphrase,
            &id,
            &passphrase,
            cx,
        );
    }

    /// Store one SSH secret (or clear it when empty), warning on a keychain error.
    /// Generic over the set/delete pair so the password and passphrase share it.
    fn store_ssh_secret(
        &mut self,
        set: fn(&str, &str) -> anyhow::Result<()>,
        delete: fn(&str) -> anyhow::Result<()>,
        id: &str,
        secret: &str,
        cx: &mut Context<Self>,
    ) {
        let result = if secret.is_empty() {
            delete(id)
        } else {
            set(id, secret)
        };
        if let Err(e) = result {
            tracing::warn!("failed to store SSH secret in keychain: {e}");
            self.notify(
                ToastVariant::Error,
                "Couldn't save an SSH secret to the OS keychain — it won't be remembered.",
                cx,
            );
        }
    }

    pub(crate) fn set_form_ssh_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.ssh_enabled = enabled;
            form.test = TestState::Idle;
        }
        cx.notify();
    }

    pub(crate) fn set_form_ssh_auth(&mut self, mode: SshAuthMode, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.ssh_auth = mode;
            form.test = TestState::Idle;
        }
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

    pub(crate) fn set_form_ai_allow_writes(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.ai_allow_writes = on;
        }
        cx.notify();
    }

    /// Fire a throwaway connection probe for the current form values.
    pub(crate) fn test_connection(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.form_config(cx) else {
            return;
        };
        // A probe needs the connection coordinates but not a name — reveal any
        // missing-field messages inline, yet still allow testing a yet-unnamed
        // connection once host/database are filled.
        if Self::form_errors(&config)
            .iter()
            .any(|(field, _)| *field != FormField::Name)
        {
            if let Some(form) = &mut self.form {
                form.submitted = true;
            }
            cx.notify();
            return;
        }
        if let Some(form) = &mut self.form {
            form.test = TestState::Testing;
        }
        self.service.send_global(Command::TestConnection(config));
        cx.notify();
    }
}

/// Map a stored [`SshAuth`] onto the form's data-free [`SshAuthMode`].
fn auth_mode(auth: &SshAuth) -> SshAuthMode {
    match auth {
        SshAuth::Agent => SshAuthMode::Agent,
        SshAuth::Password => SshAuthMode::Password,
        SshAuth::Key { .. } => SshAuthMode::Key,
    }
}
