//! Connection-import UI + commit (Phases 2–3 of `docs/plans/connection-import.md`).
//!
//! The parse/decrypt core lives in [`crate::import`]; this is the app-side glue:
//! turning an [`ImportReport`] into a preview modal, and — on confirm — committing
//! the imported connections into RED's store the same way a form save does
//! (keychain-routed secrets, never the config file).

use flint::prelude::*;
use gpui::{div, prelude::*, px, Context};
use red_core::ConnectionConfig;

use crate::config::StoredConnection;
use crate::import::ImportReport;

use super::AppState;

/// A pending import awaiting the user's confirmation: the report to apply plus a
/// title for the modal. Held on [`AppState`] while the preview modal is open.
pub(crate) struct ImportPreview {
    pub(crate) title: String,
    pub(crate) report: ImportReport,
}

impl AppState {
    /// Run the importer for the detected source at `index`, then open the preview
    /// modal (or toast when there's nothing to import). Errors reading the source
    /// surface as a toast rather than an open modal.
    pub(crate) fn run_import(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(found) = self.import_candidates.get(index).cloned() else {
            return;
        };
        match crate::import::run(found.source, &found.dir) {
            Ok(report) => {
                if report.imported.is_empty() {
                    let msg = if report.skipped.is_empty() {
                        format!("No connections found in {}.", found.source.label())
                    } else {
                        format!(
                            "{}: nothing importable ({} skipped).",
                            found.source.label(),
                            report.skipped.len()
                        )
                    };
                    self.notify(ToastVariant::Info, msg, cx);
                    return;
                }
                self.import_preview = Some(ImportPreview {
                    title: format!("Import from {}", found.source.label()),
                    report,
                });
                self.focus_modal = true;
                cx.notify();
            }
            Err(e) => {
                self.notify(
                    ToastVariant::Error,
                    format!("Couldn't read {} connections: {e}", found.source.label()),
                    cx,
                );
            }
        }
    }

    /// Apply the pending import: add each connection RED doesn't already have,
    /// routing its secrets to the keychain, then persist and summarize. Dedupe is
    /// by target coordinates so re-importing is safe.
    pub(crate) fn commit_import(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = self.import_preview.take() else {
            return;
        };
        let (mut added, mut duplicate) = (0usize, 0usize);
        for imported in preview.report.imported {
            let mut config = imported.config;
            if self
                .connections
                .iter()
                .any(|c| same_target(&c.config, &config))
            {
                duplicate += 1;
                continue;
            }
            // Split secrets off before the config is stored — they go to the OS
            // keychain, never `connections.toml` (mirrors `save_form`).
            let password = std::mem::take(&mut config.password);
            let ssh_secrets = config.ssh.as_mut().map(|s| {
                (
                    std::mem::take(&mut s.password),
                    std::mem::take(&mut s.passphrase),
                )
            });
            let is_file = config.kind.is_file();
            self.connections.push(StoredConnection {
                id: crate::config::new_id(),
                config,
                last_accessed: None,
                pinned: false,
            });
            let index = self.connections.len() - 1;
            self.store_credential(index, &password, is_file, cx);
            self.store_ssh_credentials(index, ssh_secrets, cx);
            added += 1;
        }

        self.persist(cx);
        self.rebuild_switcher(cx);
        self.refocus_root = true;

        let (variant, msg) = match (added, duplicate) {
            (0, _) => (
                ToastVariant::Info,
                "Nothing imported — those connections already exist.".to_string(),
            ),
            (n, 0) => (
                ToastVariant::Success,
                format!("Imported {n} connection{}.", plural(n)),
            ),
            (n, d) => (
                ToastVariant::Success,
                format!(
                    "Imported {n} connection{}, skipped {d} already present.",
                    plural(n)
                ),
            ),
        };
        self.notify(variant, msg, cx);
        cx.notify();
    }

    /// Dismiss the preview without importing.
    pub(crate) fn cancel_import(&mut self, cx: &mut Context<Self>) {
        self.import_preview = None;
        self.refocus_root = true;
        cx.notify();
    }

    /// The import preview modal: a scrollable list of what will import (with any
    /// per-connection caveats) and what was skipped and why, plus Cancel / Import.
    pub(crate) fn render_import_preview(
        &self,
        preview: &ImportPreview,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();

        let import_count = preview.report.imported.len();
        let skip_count = preview.report.skipped.len();

        let mut list = div().flex().flex_col().gap_1();
        for conn in &preview.report.imported {
            let mut row = div().flex().flex_col().gap_0p5().py_1().child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(div().text_color(theme.text).child(conn.source_name.clone()))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(theme.scale(11.))
                            .text_color(theme.text_faint)
                            .child(conn.config.kind.to_string()),
                    ),
            );
            // The target line (host/db) helps disambiguate similarly-named entries.
            let target = target_summary(&conn.config);
            if !target.is_empty() {
                row = row.child(
                    div()
                        .text_size(theme.scale(11.5))
                        .text_color(theme.text_muted)
                        .child(target),
                );
            }
            if let Some(warning) = &conn.warning {
                row = row.child(
                    div()
                        .text_size(theme.scale(11.5))
                        .text_color(theme.orange)
                        .child(format!("⚠ {warning}")),
                );
            }
            list = list.child(row);
        }

        // Skipped connections, each with its reason — the "nothing is dropped
        // silently" contract, made visible.
        if skip_count > 0 {
            list = list.child(
                div()
                    .pt_2()
                    .text_size(theme.scale(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_faint)
                    .child(format!("SKIPPED ({skip_count})")),
            );
            for (name, reason) in &preview.report.skipped {
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .py_0p5()
                        .text_size(theme.scale(11.5))
                        .text_color(theme.text_faint)
                        .child(div().child(name.clone()))
                        .child(div().flex_shrink_0().child(reason.clone())),
                );
            }
        }

        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_color(theme.text_muted).child(format!(
                "{import_count} connection{} ready to import{}.",
                plural(import_count),
                if skip_count > 0 {
                    format!(", {skip_count} skipped")
                } else {
                    String::new()
                }
            )))
            .child(
                div()
                    .id("import-list")
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded(theme.radius_sm)
                    .bg(theme.bg_input)
                    .child(list),
            );

        let footer = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("import-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_import(cx))),
            )
            .child(
                Button::new("import-confirm", format!("Import {import_count}"))
                    .variant(ButtonVariant::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.commit_import(cx))),
            );

        Modal::new("import-preview")
            .title(preview.title.clone())
            .width(px(480.))
            .focus_handle(self.modal_focus.clone())
            .footer(footer)
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.cancel_import(cx))
                    .ok();
            })
            .on_confirm(move |_, cx| {
                confirm_view
                    .update(cx, |this, cx| this.commit_import(cx))
                    .ok();
            })
            .child(body)
    }
}

/// Two connections address the same target — the dedupe key for import. File
/// engines compare by path (host/port/user are empty); network engines by the
/// full coordinate tuple.
fn same_target(a: &ConnectionConfig, b: &ConnectionConfig) -> bool {
    a.kind == b.kind
        && a.host.eq_ignore_ascii_case(&b.host)
        && a.port == b.port
        && a.database == b.database
        && a.user == b.user
}

/// A one-line "where does it point" summary for the preview row.
fn target_summary(config: &ConnectionConfig) -> String {
    if config.kind.is_file() {
        return config.database.clone();
    }
    let mut s = config.host.clone();
    if let Some(port) = config.port {
        s.push_str(&format!(":{port}"));
    }
    if !config.database.is_empty() {
        s.push('/');
        s.push_str(&config.database);
    }
    s
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}
