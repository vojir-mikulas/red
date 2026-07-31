//! The "Export keys" modal: what it offers, and what it sends.
//!
//! The scopes are resolved when the modal opens, not when Export is clicked.
//! A browse tab can be auto-refreshing while the modal is up, so a label reading
//! "412 keys shown" has to export the 412 it counted rather than whatever the
//! grid holds a second later. State types come from the parent module.

use gpui::Context;
use red_core::kv::{KvExportFormat, KvExportOptions, KvExportScope};
use red_service::{Command, SessionId};

use crate::app::AppState;

use super::*;

/// The formats the modal offers, in menu order, each with the one line that
/// decides whether it is the right choice.
pub(crate) const KV_EXPORT_FORMATS: [(KvExportFormat, &str, &str); 3] = [
    (
        KvExportFormat::Commands,
        "Commands (.redis)",
        "redis-cli commands, one per line. Readable, works on any server version, \
         and imports straight back through \"Import keys\".",
    ),
    (
        KvExportFormat::Json,
        "JSON (.json)",
        "One object per key, for feeding another tool or attaching to a ticket. \
         Does not import back; binary values are marked, not carried.",
    ),
    (
        KvExportFormat::Dump,
        "DUMP (.rdbdump)",
        "Byte-exact serialized values, the only format that carries binary data. \
         NOT cross-version: RESTORE refuses a payload from a newer Redis.",
    ),
];

impl AppState {
    /// Open the modal, resolving the scopes it will offer from the browse tab.
    pub(crate) fn kv_open_export(&mut self, session: SessionId, cx: &mut Context<Self>) {
        self.kv_close_actions_menu(session, cx);
        let visible: Vec<String> = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .map(|b| b.visible_rows(cx).iter().map(|r| r.key.clone()).collect())
            .unwrap_or_default();
        let filter = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .map(|b| {
                (
                    b.pattern.clone(),
                    b.type_filter.as_ref().map(|t| t.wire_type().to_string()),
                )
            });
        let db_size = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.db_size);

        let mut scopes = Vec::new();
        if !visible.is_empty() {
            scopes.push((
                format!("The {} key(s) shown", visible.len()),
                KvExportScope::Selection(visible),
            ));
        }
        if let Some((pattern, type_filter)) = filter {
            // Only worth offering as its own scope when a filter is actually
            // narrowing something; otherwise it is the whole database twice.
            if pattern.is_some() || type_filter.is_some() {
                let scope = KvExportScope::Matching {
                    pattern,
                    type_filter,
                };
                scopes.push((
                    format!("Every key matching the filter ({})", scope.describe()),
                    scope,
                ));
            }
        }
        scopes.push((
            match db_size {
                Some(n) => format!("The whole database ({n} key(s))"),
                None => "The whole database".to_string(),
            },
            KvExportScope::Database,
        ));

        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.export = Some(KvExportState {
                scopes,
                scope_ix: 0,
                format: KvExportFormat::Commands,
                options: KvExportOptions::default(),
                running: false,
            });
        }
        self.focus_modal = true;
        cx.notify();
    }

    /// Open the modal pre-scoped to one key, from its context menu. The common
    /// case: one key, to a file, to send to someone.
    pub(crate) fn kv_export_one_key(
        &mut self,
        session: SessionId,
        key: String,
        cx: &mut Context<Self>,
    ) {
        self.kv_close_key_menu(session, cx);
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.export = Some(KvExportState {
                scopes: vec![(
                    format!("The key `{key}`"),
                    KvExportScope::Selection(vec![key]),
                )],
                scope_ix: 0,
                format: KvExportFormat::Commands,
                options: KvExportOptions::default(),
                running: false,
            });
        }
        self.focus_modal = true;
        cx.notify();
    }

    pub(crate) fn kv_close_export(&mut self, session: SessionId, cx: &mut Context<Self>) {
        // Closing the dialog stops the run too: leaving a spawned export writing
        // to a file with the UI showing nothing is what a Cancel must not do.
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            let running = view.export.as_ref().is_some_and(|e| e.running);
            view.export = None;
            if running {
                self.service
                    .send_to(session, Command::CancelKvExport { id: KV_EXPORT_OP });
            }
        }
        self.refocus_root = true;
        cx.notify();
    }

    pub(crate) fn kv_set_export_scope(
        &mut self,
        session: SessionId,
        ix: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(export) = self.kv_export_mut(session) {
            export.scope_ix = ix;
        }
        cx.notify();
    }

    pub(crate) fn kv_set_export_format(
        &mut self,
        session: SessionId,
        format: KvExportFormat,
        cx: &mut Context<Self>,
    ) {
        if let Some(export) = self.kv_export_mut(session) {
            export.format = format;
        }
        cx.notify();
    }

    /// Toggle one of the two export switches.
    pub(crate) fn kv_toggle_export_option(
        &mut self,
        session: SessionId,
        option: KvExportSwitch,
        cx: &mut Context<Self>,
    ) {
        if let Some(export) = self.kv_export_mut(session) {
            match option {
                KvExportSwitch::Ttls => export.options.ttls = !export.options.ttls,
                KvExportSwitch::DelFirst => {
                    export.options.del_first = !export.options.del_first;
                }
            }
        }
        cx.notify();
    }

    /// Ask for a destination, then start the export.
    pub(crate) fn kv_choose_export_path(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(export) = self.kv_export_mut(session) else {
            return;
        };
        let format = export.format;
        let name = format!("red-keys.{}", format.extension());
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update(cx, |this, cx| this.kv_start_export(session, path, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn kv_start_export(
        &mut self,
        session: SessionId,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(browse_epoch) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .map(|b| b.epoch)
        else {
            return;
        };
        let Some(export) = self.kv_export_mut(session) else {
            return;
        };
        export.running = true;
        let (format, scope, options) = (export.format, export.scope(), export.options);
        self.service.send_to(
            session,
            Command::KvExport {
                epoch: browse_epoch,
                id: KV_EXPORT_OP,
                path,
                format,
                scope,
                options,
            },
        );
        // The standard export toast takes it from here; the modal has nothing
        // left to show.
        self.kv_close_export(session, cx);
    }

    fn kv_export_mut(&mut self, session: SessionId) -> Option<&mut KvExportState> {
        self.conn_mut(Some(session))?
            .kv_view
            .as_mut()?
            .export
            .as_mut()
    }
}

/// Which of the modal's two switches a toggle targets. An enum rather than a
/// `bool` argument, so a call site cannot silently flip the wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KvExportSwitch {
    Ttls,
    DelFirst,
}

/// The op id every Redis key export runs under.
///
/// A constant rather than a fresh id per export: only one can be in flight (the
/// modal is the only way to start one and it closes on submit), and a fixed id
/// is what lets `Cancel` on the toast reach it without the UI tracking a
/// handle.
pub(crate) const KV_EXPORT_OP: red_service::OpId = red_service::OpId::new(u64::MAX);
