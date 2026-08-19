//! The "Copy to…" flow: stream a (filtered/sorted) result straight into another
//! table, in the **same** connection or **another open** one. This is the on-ideology
//! slice of dbgate's sprawling data-transfer suite: one gesture (pick a target,
//! confirm a name-based mapping), no wizard, no intermediary file, no saved job;
//! just a streamed, typed, cancellable operation.
//!
//! The UI half: build the candidate target list, peek the target's columns
//! (`CopyTargetColumns`), auto-map source→target by name, raise the copy confirm
//! ([`PendingWrite::Copy`]), and own the transfer toast. The streaming read→insert
//! loop lives in the backend (`red-service`). The picker itself is in
//! [`crate::palette`] so it reuses the command-palette plumbing.

use flint::prelude::*;
use gpui::App;
use gpui::Context;
use red_core::{Column, ColumnMap, ConnectionConfig, CopyMode, ObjectKind, TableRef};
use red_service::{Command, OpId, SessionId};

use crate::app::{
    AppState, CopyNamespace, CopyTargetCandidate, ExportProgress, Notification, PendingWrite,
    Phase, TransferKind,
};
use crate::schema::SchemaState;

/// Append one connection's writable tables to `out` as copy-target candidates.
/// Read-only connections and engines that can't accept inserts are skipped; they'd
/// refuse the copy, so they must never appear as a target.
fn collect_targets(
    out: &mut Vec<CopyTargetCandidate>,
    session: SessionId,
    config: &ConnectionConfig,
    schema: &SchemaState,
) {
    if config.read_only || !config.kind.write_caps().insert {
        return;
    }
    for ns in &schema.schemas {
        for obj in &ns.objects {
            // Write target: spelled `Table` deliberately, NOT `is_relation()`. A
            // view is a relation and is not an INSERT target, and neither is any
            // kind added later. See `ObjectKind::is_relation`'s doc comment.
            if matches!(obj.kind, ObjectKind::Table) {
                out.push(CopyTargetCandidate {
                    session,
                    conn_name: config.name.clone(),
                    schema: ns.name.clone(),
                    table: TableRef {
                        schema: Some(ns.name.clone()),
                        name: obj.name.clone(),
                    },
                });
            }
        }
    }
}

/// Append one connection's writable namespaces (schemas/databases) to `out` as
/// "new table" copy targets: *every* namespace the schema tree shows, including an
/// empty one, so you can migrate into a brand-new/empty database. Read-only
/// connections and engines that can't accept inserts are skipped (they can't be a
/// write target), mirroring [`collect_targets`].
fn collect_namespaces(
    out: &mut Vec<CopyNamespace>,
    session: SessionId,
    config: &ConnectionConfig,
    schema: &SchemaState,
) {
    if config.read_only || !config.kind.write_caps().insert {
        return;
    }
    for ns in &schema.schemas {
        out.push(CopyNamespace {
            session,
            conn_name: config.name.clone(),
            schema: ns.name.clone(),
        });
    }
}

/// Whether mapping a value from declared type `src` into `dst` is a *type change*
/// worth flagging in the preview (best-effort, non-blocking): same base type token is
/// fine, and any number→number pairing is fine (int→bigint, etc.). Everything else is
/// surfaced so the user consents before a possibly-lossy cross-engine cast (e.g.
/// `tags text[] → text`). The copy itself never refuses; it binds under the target
/// type and lets the engine reject a truly impossible bind.
fn type_changed(src: &str, dst: &str) -> bool {
    let base = |t: &str| {
        t.split(['(', ' '])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    if base(src) == base(dst) {
        return false;
    }
    !(red_core::is_numeric_type(Some(src)) && red_core::is_numeric_type(Some(dst)))
}

impl AppState {
    /// The candidate copy *target* tables across every open, writable connection
    /// (foreground + parked live sessions). The "Copy to…" picker offers only these,
    /// so a target the copy can't write into (read-only / ClickHouse) is never shown.
    pub(crate) fn copy_target_candidates(&self, cx: &App) -> Vec<CopyTargetCandidate> {
        let mut out = Vec::new();
        if let Phase::Connected(active) = &self.phase {
            collect_targets(
                &mut out,
                active.session,
                &active.config,
                active.schema.read(cx),
            );
        }
        for (id, conn) in &self.parked {
            collect_targets(&mut out, *id, &conn.config, conn.schema.read(cx));
        }
        out
    }

    /// The distinct writable namespaces (schemas/databases) across every open
    /// connection: the "✦ New table…" rows of the "Copy to…" picker. Selecting one
    /// then prompts for a name and *creates* the table from the source's column shape,
    /// so this covers "migrate into a different / same-connection database".
    pub(crate) fn copy_namespace_candidates(&self, cx: &App) -> Vec<CopyNamespace> {
        let mut out = Vec::new();
        if let Phase::Connected(active) = &self.phase {
            collect_namespaces(
                &mut out,
                active.session,
                &active.config,
                active.schema.read(cx),
            );
        }
        for (id, conn) in &self.parked {
            collect_namespaces(&mut out, *id, &conn.config, conn.schema.read(cx));
        }
        out
    }

    /// Whether `session` already has a table/view named `name` in `schema`: the
    /// collision guard for "new table" copies, so the create path never silently
    /// appends into a pre-existing (possibly mismatched) table.
    pub(crate) fn namespace_has_table(
        &self,
        session: SessionId,
        schema: &str,
        name: &str,
        cx: &App,
    ) -> bool {
        let has = |st: &SchemaState| {
            st.schemas.iter().any(|ns| {
                ns.name == schema && ns.objects.iter().any(|o| o.name.eq_ignore_ascii_case(name))
            })
        };
        if let Phase::Connected(active) = &self.phase
            && active.session == session
        {
            return has(active.schema.read(cx));
        }
        self.parked
            .get(&session)
            .is_some_and(|c| has(c.schema.read(cx)))
    }

    /// `CopyTargetColumns`: the picked target's columns arrived, so auto-map the source
    /// result's columns onto them **by name**, summarize the mapping (flagging lossy
    /// type changes, unmatched target columns, and ignored source columns), and raise
    /// the copy confirm so the user sees exactly what will move before any write.
    pub(crate) fn on_copy_target_columns(
        &mut self,
        id: OpId,
        target_cols: Vec<Column>,
        cx: &mut Context<Self>,
    ) {
        let Some(peek) = self.pending_copy_target.take().filter(|p| p.id == id) else {
            return;
        };
        let mut mapping = Vec::new();
        let mut unmatched_target = Vec::new();
        let mut lossy = Vec::new();
        for tcol in &target_cols {
            match peek
                .source_cols
                .iter()
                .position(|s| s.name.eq_ignore_ascii_case(&tcol.name))
            {
                Some(idx) => {
                    let scol = &peek.source_cols[idx];
                    if let (Some(sd), Some(td)) = (&scol.decl_type, &tcol.decl_type)
                        && type_changed(sd, td)
                    {
                        lossy.push(format!("{} ({sd} → {td})", tcol.name));
                    }
                    mapping.push(ColumnMap {
                        source: idx,
                        column: tcol.name.clone(),
                        decl_type: tcol.decl_type.clone(),
                    });
                }
                None => unmatched_target.push(tcol.name.clone()),
            }
        }
        if mapping.is_empty() {
            self.notify(
                ToastVariant::Error,
                "No source columns match this table's columns",
                cx,
            );
            return;
        }
        let ignored_source: Vec<String> = peek
            .source_cols
            .iter()
            .enumerate()
            .filter(|(i, _)| !mapping.iter().any(|m| m.source == *i))
            .map(|(_, c)| c.name.clone())
            .collect();
        let prose = format!(
            "Copy {} column(s), matched by name, from this result into {}. Rows stream \
             in chunks and commit per chunk; pick Append to add them or Replace all to \
             refresh the table first.",
            mapping.len(),
            peek.target_label
        );
        let mut preview = mapping
            .iter()
            .map(|m| {
                format!(
                    "{}  ←  {}",
                    m.column,
                    peek.source_cols
                        .get(m.source)
                        .map(|c| c.name.as_str())
                        .unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !lossy.is_empty() {
            preview.push_str(&format!(
                "\n\n⚠ Type change (best-effort cast): {}",
                lossy.join(", ")
            ));
        }
        if !unmatched_target.is_empty() {
            preview.push_str(&format!(
                "\nTarget columns left to default/NULL: {}",
                unmatched_target.join(", ")
            ));
        }
        if !ignored_source.is_empty() {
            preview.push_str(&format!(
                "\nSource columns ignored: {}",
                ignored_source.join(", ")
            ));
        }
        self.confirm_exec = self.pending_confirm(PendingWrite::Copy {
            id: peek.id,
            source_epoch: peek.source_epoch,
            target: peek.target,
            target_session: peek.target_session,
            mapping,
            mode: CopyMode::Append,
            create: None,
            prose,
            preview,
        });
        cx.notify();
    }

    /// Confirmed: fire `CopyToTable` at the source (foreground) session and stand up
    /// the transfer toast (its `✕` is a `CancelCopy`). `total` is the source result's
    /// row count, already known, so the toast shows a real percentage.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_copy(
        &mut self,
        id: OpId,
        source_epoch: red_service::Epoch,
        target: TableRef,
        target_session: SessionId,
        mapping: Vec<ColumnMap>,
        mode: CopyMode,
        create: Option<Vec<red_core::ColumnMeta>>,
        cx: &mut Context<Self>,
    ) {
        let total = match &self.phase {
            Phase::Connected(a) => a
                .active_result()
                .map(|g| g.read(cx).total_rows())
                .unwrap_or(0),
            _ => 0,
        };
        let creating = create.is_some();
        self.send_active(Command::CopyToTable {
            id,
            source_epoch,
            target,
            target_session,
            mapping,
            mode,
            create,
        });
        let verb = match (creating, mode) {
            (true, _) => "Migrating",
            (false, CopyMode::Append) => "Copying",
            (false, CopyMode::TruncateInsert) => "Replacing",
        };
        self.push_notification(
            Notification {
                id: 0,
                variant: ToastVariant::Info,
                message: format!("{verb}…").into(),
                detail: None,
                detail_label: None,
                auto_dismiss: None,
                export: Some(ExportProgress {
                    id,
                    rows: 0,
                    total,
                    kind: TransferKind::Copy,
                }),
                expanded: false,
                hovered: false,
                dismiss_gen: 0,
                action: None,
            },
            cx,
        );
    }

    /// The notification id of the transfer toast carrying copy `id`, if still shown.
    fn copy_notification_id(&self, copy_id: OpId) -> Option<u64> {
        self.notifications
            .iter()
            .find(|n| n.export.as_ref().is_some_and(|e| e.id == copy_id))
            .map(|n| n.id)
    }

    /// The kind (Copy vs Migrate) of the transfer toast carrying `copy_id`; the
    /// shared `Copy*` handlers read it for the right verb; defaults to `Copy` if the
    /// toast is already gone.
    fn copy_kind(&self, copy_id: OpId) -> TransferKind {
        self.notifications
            .iter()
            .find_map(|n| {
                n.export
                    .as_ref()
                    .filter(|e| e.id == copy_id)
                    .map(|e| e.kind)
            })
            .unwrap_or(TransferKind::Copy)
    }

    /// `CopyProgress`: advance the copy/migrate toast's row count + percentage.
    pub(crate) fn on_copy_progress(&mut self, id: OpId, rows: usize, cx: &mut Context<Self>) {
        let migrating = self.copy_kind(id) == TransferKind::Migrate;
        if let Some(n) = self
            .notifications
            .iter_mut()
            .find(|n| n.export.as_ref().is_some_and(|e| e.id == id))
            && let Some(export) = &mut n.export
        {
            export.rows = rows;
            n.message = match rows.saturating_mul(100).checked_div(export.total) {
                Some(pct) => {
                    let pct = pct.min(100);
                    if migrating {
                        crate::i18n::tr!("copy.migrating_pct", "Migrating… {pct}%", pct = pct)
                    } else {
                        crate::i18n::tr!("copy.copying_pct", "Copying… {pct}%", pct = pct)
                    }
                }
                None if migrating => {
                    crate::i18n::tr!(
                        "copy.migrating_rows",
                        "Migrating… {rows} row(s)",
                        rows = rows
                    )
                }
                None => {
                    crate::i18n::tr!("copy.copying_rows", "Copying… {rows} row(s)", rows = rows)
                }
            };
        }
        cx.notify();
    }

    /// `CopyFinished`: drop the progress toast, leave an auto-dismissing success.
    pub(crate) fn on_copy_finished(&mut self, id: OpId, rows: usize, cx: &mut Context<Self>) {
        let migrating = self.copy_kind(id) == TransferKind::Migrate;
        if let Some(nid) = self.copy_notification_id(id) {
            self.dismiss(nid, cx);
        }
        let msg = if migrating {
            crate::i18n::tr!("copy.migrated_rows", "Migrated {rows} row(s)", rows = rows)
        } else {
            crate::i18n::tr!("copy.copied_rows", "Copied {rows} row(s)", rows = rows)
        };
        self.notify(ToastVariant::Success, msg, cx);
    }

    /// `CopyFailed`: drop the progress toast, surface the error. Inserts commit per
    /// chunk, so the message says how far it got.
    pub(crate) fn on_copy_failed(
        &mut self,
        id: OpId,
        rows: usize,
        message: String,
        cx: &mut Context<Self>,
    ) {
        let migrating = self.copy_kind(id) == TransferKind::Migrate;
        if let Some(nid) = self.copy_notification_id(id) {
            self.dismiss(nid, cx);
        }
        let msg = match (migrating, rows > 0) {
            (true, true) => crate::i18n::tr!(
                "copy.migration_failed_after",
                "Migration failed after {rows} row(s): {message}",
                rows = rows,
                message = message,
            ),
            (true, false) => {
                crate::i18n::tr!(
                    "copy.migration_failed",
                    "Migration failed: {message}",
                    message = message
                )
            }
            (false, true) => crate::i18n::tr!(
                "copy.copy_failed_after",
                "Copy failed after {rows} row(s): {message}",
                rows = rows,
                message = message,
            ),
            (false, false) => {
                crate::i18n::tr!(
                    "copy.copy_failed",
                    "Copy failed: {message}",
                    message = message
                )
            }
        };
        self.notify(ToastVariant::Error, msg, cx);
    }

    /// `CopyCancelled`: drop the progress toast; earlier chunks stay committed.
    pub(crate) fn on_copy_cancelled(&mut self, id: OpId, rows: usize, cx: &mut Context<Self>) {
        let migrating = self.copy_kind(id) == TransferKind::Migrate;
        if let Some(nid) = self.copy_notification_id(id) {
            self.dismiss(nid, cx);
        }
        let msg = match (migrating, rows > 0) {
            (true, true) => crate::i18n::tr!(
                "copy.migration_cancelled_kept",
                "Migration cancelled ({rows} row(s) kept)",
                rows = rows,
            ),
            (true, false) => crate::i18n::tr!("copy.migration_cancelled", "Migration cancelled"),
            (false, true) => crate::i18n::tr!(
                "copy.copy_cancelled_kept",
                "Copy cancelled ({rows} row(s) kept)",
                rows = rows,
            ),
            (false, false) => crate::i18n::tr!("copy.copy_cancelled", "Copy cancelled"),
        };
        self.notify(ToastVariant::Info, msg, cx);
    }
}
