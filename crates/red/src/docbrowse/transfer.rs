//! Data movement for the Mongo shell: getting documents out of a collection and
//! back into one. The document arm of what the SQL result grid and the Redis
//! keyspace already have, riding the same `OpId`-keyed transfer toasts (progress,
//! cancel, "Show in folder") so an export reports identically whichever shell
//! started it.
//!
//! Scopes are resolved when a modal opens, not when the button is clicked: a tab's
//! filter can change while the dialog is up, and a label reading "matching the
//! current filter" has to mean the filter it counted.

use flint::prelude::*;
use gpui::{Context, Entity, div, prelude::*, px};
use red_core::doc::{DocCopyMode, DocExportFormat, DocImportFormat, DocImportMode};
use red_service::{Command, Epoch, SessionId};

use crate::app::{AppState, Phase};

/// The export formats the modal offers, in menu order, each with the one line that
/// decides whether it is the right choice.
pub(crate) const DOC_EXPORT_FORMATS: [(DocExportFormat, &str, &str); 4] = [
    (
        DocExportFormat::Json,
        "JSON (.json)",
        "One array of extended-JSON documents. Keeps every BSON type (ObjectId, \
         dates, decimals) and reads back through \"Import documents\".",
    ),
    (
        DocExportFormat::Ndjson,
        "NDJSON (.ndjson)",
        "One extended-JSON document per line, the format mongoimport and most \
         pipelines expect. Streams on both ends, so size is not a problem.",
    ),
    (
        DocExportFormat::Csv,
        "CSV (.csv)",
        "Flattened to dotted columns sampled from the collection. For a \
         spreadsheet: nested values become JSON text and BSON types become strings.",
    ),
    (
        DocExportFormat::Xlsx,
        "Excel (.xlsx)",
        "The CSV columns as a workbook sheet. Stops at Excel's 1,048,576-row limit \
         and says so.",
    ),
];

/// The import formats the modal offers, in menu order.
pub(crate) const DOC_IMPORT_FORMATS: [(DocImportFormat, &str); 3] = [
    (DocImportFormat::Json, "JSON array"),
    (DocImportFormat::Ndjson, "NDJSON"),
    (DocImportFormat::Csv, "CSV"),
];

/// The write modes the import modal offers.
pub(crate) const DOC_IMPORT_MODES: [(DocImportMode, &str, &str); 2] = [
    (
        DocImportMode::Insert,
        "Insert",
        "Add every document. A document whose _id already exists fails the chunk \
         it is in; earlier chunks stay written.",
    ),
    (
        DocImportMode::UpsertOnId,
        "Upsert on _id",
        "Replace the document with the same _id, insert it when there is none. \
         Re-importing the same file twice leaves the collection unchanged.",
    ),
];

/// Documents the dialog previews before an import runs. Enough to see the shape and
/// catch a wrong format, few enough to parse instantly.
const PREVIEW_LIMIT: usize = 5;

/// The "Import documents" modal's state.
pub(crate) struct DocImportState {
    pub(crate) db: String,
    pub(crate) coll: String,
    pub(crate) epoch: Epoch,
    /// The chosen source, or `None` before a file is picked.
    pub(crate) path: Option<std::path::PathBuf>,
    pub(crate) format: DocImportFormat,
    pub(crate) mode: DocImportMode,
    /// The first documents as the *target driver* parses them, so the preview shows
    /// what would be written rather than what the file happens to say.
    pub(crate) preview: Vec<String>,
    /// A read or parse failure from the peek, shown beside the file.
    pub(crate) error: Option<String>,
    /// Whether a peek is in flight (the preview is still unknown).
    pub(crate) peeking: bool,
}

/// The copy modes the dialog offers, in menu order.
pub(crate) const DOC_COPY_MODES: [(DocCopyMode, &str, &str); 3] = [
    (
        DocCopyMode::Append,
        "Append",
        "Add the source documents to whatever the target already holds. A repeated \
         _id fails the chunk it is in.",
    ),
    (
        DocCopyMode::UpsertOnId,
        "Upsert on _id",
        "Replace a target document with the same _id, insert it when there is \
         none. Safe to re-run.",
    ),
    (
        DocCopyMode::DropAndInsert,
        "Replace collection",
        "DROP the target collection first, then insert. Everything it holds now, \
         including its indexes, is gone.",
    ),
];

/// One connection a copy can write into: an open, writable MongoDB session.
#[derive(Clone)]
pub(crate) struct DocCopyTarget {
    pub(crate) session: SessionId,
    pub(crate) name: String,
    /// The databases that connection has listed, offered as suggestions under the
    /// name field. Empty for a connection whose catalog has not loaded.
    pub(crate) databases: Vec<String>,
}

/// The "Copy collection to…" modal's state.
pub(crate) struct DocCopyState {
    pub(crate) source_db: String,
    pub(crate) source_coll: String,
    /// The source tab's applied filter, carried so a filtered copy copies what the
    /// grid shows. `None` copies the whole collection.
    pub(crate) filter: Option<String>,
    /// Documents the copy expects to move, for the toast's percentage.
    pub(crate) total: usize,
    pub(crate) targets: Vec<DocCopyTarget>,
    pub(crate) target_ix: usize,
    pub(crate) db_input: Entity<TextInput>,
    pub(crate) coll_input: Entity<TextInput>,
    pub(crate) mode: DocCopyMode,
}

/// The "Export documents" modal's state, held on [`MongoView`] for the life of the
/// dialog.
pub(crate) struct DocExportState {
    /// The namespace being exported, captured at open so a tab switch behind the
    /// modal cannot repoint it.
    pub(crate) db: String,
    pub(crate) coll: String,
    /// The epoch the export runs under (the originating tab's, or the catalog's
    /// when started from the tree), so its `Slot::DocExport` and any `DocError`
    /// land where the request came from.
    pub(crate) epoch: Epoch,
    /// Offered scopes as `(label, filter)`; `None` is the whole collection.
    pub(crate) scopes: Vec<(String, Option<String>)>,
    pub(crate) scope_ix: usize,
    pub(crate) format: DocExportFormat,
    /// Documents the chosen scope holds, when known, for the toast's percentage.
    pub(crate) total: usize,
}

impl DocExportState {
    /// The filter for the selected scope.
    fn filter(&self) -> Option<String> {
        self.scopes.get(self.scope_ix).and_then(|(_, f)| f.clone())
    }
}

impl AppState {
    /// Open the export modal for a collection, resolving its scopes now.
    ///
    /// `tab` is the collection tab the request came from, when there is one: it
    /// carries the applied filter and the known document total, neither of which
    /// the catalog tree has.
    pub(crate) fn doc_open_export(
        &mut self,
        session: SessionId,
        db: String,
        coll: String,
        cx: &mut Context<Self>,
    ) {
        self.doc_close_actions_menu(session, cx);
        self.doc_close_coll_menu(session, cx);
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        // A tab open on this namespace supplies the live filter and count; the
        // tree alone supplies neither.
        let open = view.tabs.iter().find_map(|t| match &t.state {
            super::MongoTabState::Collection(c) if c.db == db && c.coll == coll => Some(&**c),
            _ => None,
        });
        let epoch = open.map_or(view.epoch, |c| c.epoch);
        let filtered = open.and_then(|c| c.filter.clone());
        let filtered_total = open.and_then(|c| c.window.borrow().total());
        let est = view
            .collections
            .get(&db)
            .and_then(|cs| cs.iter().find(|c| c.name == coll))
            .map(|c| c.est_count as usize)
            .unwrap_or(0);

        let mut scopes = Vec::new();
        if let Some(filter) = filtered {
            let label = match filtered_total {
                Some(n) => format!("Matching the current filter ({n} document(s))"),
                None => "Matching the current filter".to_string(),
            };
            scopes.push((label, Some(filter)));
        }
        scopes.push((format!("The whole collection (~{est} document(s))"), None));
        let total = match (&scopes[0].1, filtered_total) {
            (Some(_), Some(n)) => n,
            _ => est,
        };

        view.export = Some(DocExportState {
            db,
            coll,
            epoch,
            scopes,
            scope_ix: 0,
            format: DocExportFormat::Json,
            total,
        });
        self.focus_modal = true;
        cx.notify();
    }

    pub(crate) fn doc_close_export(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.export = None;
        }
        self.refocus_root = true;
        cx.notify();
    }

    pub(crate) fn doc_set_export_scope(
        &mut self,
        session: SessionId,
        ix: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(export) = self.doc_export_mut(session) {
            export.scope_ix = ix;
        }
        cx.notify();
    }

    pub(crate) fn doc_set_export_format(
        &mut self,
        session: SessionId,
        format: DocExportFormat,
        cx: &mut Context<Self>,
    ) {
        if let Some(export) = self.doc_export_mut(session) {
            export.format = format;
        }
        cx.notify();
    }

    /// Ask for a destination, then start the export.
    pub(crate) fn doc_choose_export_path(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(export) = self.doc_export_mut(session) else {
            return;
        };
        let name = format!("{}.{}", export.coll, export.format.extension());
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update(cx, |this, cx| this.doc_start_export(session, path, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn doc_start_export(
        &mut self,
        session: SessionId,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(export) = self.doc_export_mut(session) else {
            return;
        };
        let (db, coll, epoch, format, total) = (
            export.db.clone(),
            export.coll.clone(),
            export.epoch,
            export.format,
            export.total,
        );
        let filter = export.filter();
        let id = red_service::OpId::new(self.next_export_id);
        self.next_export_id += 1;
        self.service.send_to(
            session,
            Command::DocExport {
                epoch,
                id,
                db,
                coll,
                filter,
                format,
                path,
            },
        );
        self.push_transfer_toast(
            id,
            "Exporting…",
            total,
            crate::app::TransferKind::Export,
            cx,
        );
        // The standard export toast takes it from here; the modal is done.
        self.doc_close_export(session, cx);
    }

    fn doc_export_mut(&mut self, session: SessionId) -> Option<&mut DocExportState> {
        self.conn_mut(Some(session))?
            .doc_view
            .as_mut()?
            .export
            .as_mut()
    }

    // --- collection copy ---------------------------------------------------

    /// Every open, writable MongoDB connection a copy could target: the foreground
    /// one plus the parked ones. A read-only connection is excluded rather than
    /// offered and then refused.
    fn doc_copy_targets(&self) -> Vec<DocCopyTarget> {
        let mut out = Vec::new();
        let mut push = |session: SessionId, conn: &crate::app::ActiveConn| {
            if conn.config.read_only || conn.doc_view.is_none() {
                return;
            }
            let databases = conn
                .doc_view
                .as_ref()
                .map(|v| v.databases.iter().map(|d| d.name.clone()).collect())
                .unwrap_or_default();
            out.push(DocCopyTarget {
                session,
                name: conn.config.name.clone(),
                databases,
            });
        };
        if let Phase::Connected(active) = &self.phase {
            push(active.session, active);
        }
        for (id, conn) in &self.parked {
            push(*id, conn);
        }
        out
    }

    /// Open the copy modal for a collection, pre-filled with a same-database
    /// `<name>_copy` target: the common case is duplicating a collection to poke at
    /// it, and that should take one keystroke fewer than a rename.
    pub(crate) fn doc_open_copy(
        &mut self,
        session: SessionId,
        db: String,
        coll: String,
        cx: &mut Context<Self>,
    ) {
        self.doc_close_actions_menu(session, cx);
        self.doc_close_coll_menu(session, cx);
        let targets = self.doc_copy_targets();
        if targets.is_empty() {
            self.notify(
                ToastVariant::Warning,
                "No writable MongoDB connection is open to copy into",
                cx,
            );
            return;
        }
        // The source connection leads the list: copying within one connection is
        // the common case, and it is what the defaults below assume.
        let target_ix = targets
            .iter()
            .position(|t| t.session == session)
            .unwrap_or(0);
        let db_input = cx.new(|cx| TextInput::new(cx).with_content(db.clone()));
        let coll_input = cx.new(|cx| TextInput::new(cx).with_content(format!("{coll}_copy")));

        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let open = view.tabs.iter().find_map(|t| match &t.state {
            super::MongoTabState::Collection(c) if c.db == db && c.coll == coll => Some(&**c),
            _ => None,
        });
        let filter = open.and_then(|c| c.filter.clone());
        let total = open
            .and_then(|c| c.window.borrow().total())
            .or_else(|| {
                view.collections
                    .get(&db)
                    .and_then(|cs| cs.iter().find(|c| c.name == coll))
                    .map(|c| c.est_count as usize)
            })
            .unwrap_or(0);
        view.copy = Some(DocCopyState {
            source_db: db,
            source_coll: coll,
            filter,
            total,
            targets,
            target_ix,
            db_input,
            coll_input,
            mode: DocCopyMode::Append,
        });
        self.focus_modal = true;
        cx.notify();
    }

    pub(crate) fn doc_close_copy(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.copy = None;
        }
        self.refocus_root = true;
        cx.notify();
    }

    pub(crate) fn doc_set_copy_target(
        &mut self,
        session: SessionId,
        ix: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(copy) = self.doc_copy_mut(session) {
            copy.target_ix = ix;
        }
        cx.notify();
    }

    pub(crate) fn doc_set_copy_mode(
        &mut self,
        session: SessionId,
        mode: DocCopyMode,
        cx: &mut Context<Self>,
    ) {
        if let Some(copy) = self.doc_copy_mut(session) {
            copy.mode = mode;
        }
        cx.notify();
    }

    /// Fire the copy at the backend and hand reporting to the transfer toast.
    pub(crate) fn doc_start_copy(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(copy) = self.doc_copy_mut(session) else {
            return;
        };
        let Some(target) = copy.targets.get(copy.target_ix).cloned() else {
            return;
        };
        let (source_db, source_coll, filter, mode, total) = (
            copy.source_db.clone(),
            copy.source_coll.clone(),
            copy.filter.clone(),
            copy.mode,
            copy.total,
        );
        let (db_input, coll_input) = (copy.db_input.clone(), copy.coll_input.clone());
        let target_db = db_input.read(cx).content().trim().to_string();
        let target_coll = coll_input.read(cx).content().trim().to_string();
        if target_db.is_empty() || target_coll.is_empty() {
            self.notify(
                ToastVariant::Warning,
                "A copy needs a target database and collection name",
                cx,
            );
            return;
        }
        // Copying a collection onto itself would insert every document back into
        // the collection it came from, doubling it; refuse rather than explain it
        // afterwards.
        if target.session == session && target_db == source_db && target_coll == source_coll {
            self.notify(
                ToastVariant::Warning,
                "The target is the source collection; pick another name",
                cx,
            );
            return;
        }
        let id = red_service::OpId::new(self.next_export_id);
        self.next_export_id += 1;
        self.service.send_to(
            session,
            Command::DocCopyCollection {
                id,
                source_db,
                source_coll,
                filter,
                target_session: target.session,
                target_db,
                target_coll,
                mode,
            },
        );
        self.push_transfer_toast(id, "Copying…", total, crate::app::TransferKind::Copy, cx);
        self.doc_close_copy(session, cx);
    }

    fn doc_copy_mut(&mut self, session: SessionId) -> Option<&mut DocCopyState> {
        self.conn_mut(Some(session))?
            .doc_view
            .as_mut()?
            .copy
            .as_mut()
    }

    /// The "Copy collection to…" modal.
    pub(crate) fn render_doc_copy_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        use crate::connect::labeled_field;

        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let session = active.session;
        let copy = active.doc_view.as_ref()?.copy.as_ref()?;
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();

        let target_view = view.clone();
        let target_row = labeled_field("Connection", &theme).child(
            copy.targets
                .iter()
                .fold(Segmented::new("doc-copy-target"), |seg, t| {
                    seg.segment(t.name.clone())
                })
                .selected(copy.target_ix)
                .on_select(move |ix, _, cx| {
                    target_view
                        .update(cx, |this, cx| this.doc_set_copy_target(session, ix, cx))
                        .ok();
                }),
        );

        // The chosen connection's databases, as a hint under the name field: a
        // typo'd database silently creates a new one in Mongo, so seeing the real
        // list beats discovering the mistake in the tree afterwards.
        let known = copy
            .targets
            .get(copy.target_ix)
            .map(|t| t.databases.join(", "))
            .filter(|s| !s.is_empty())
            .map(|list| {
                div()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_faint)
                    .child(format!("Databases there: {list}"))
            });

        let mode_view = view.clone();
        let mode_ix = DOC_COPY_MODES
            .iter()
            .position(|(m, _, _)| *m == copy.mode)
            .unwrap_or(0);
        let mode_row = labeled_field("Mode", &theme).child(
            DOC_COPY_MODES
                .iter()
                .fold(Segmented::new("doc-copy-mode"), |seg, (_, label, _)| {
                    seg.segment(*label)
                })
                .selected(mode_ix)
                .on_select(move |ix, _, cx| {
                    let Some((mode, _, _)) = DOC_COPY_MODES.get(ix) else {
                        return;
                    };
                    let mode = *mode;
                    mode_view
                        .update(cx, |this, cx| this.doc_set_copy_mode(session, mode, cx))
                        .ok();
                }),
        );
        let destructive = copy.mode == DocCopyMode::DropAndInsert;
        let mode_hint = div()
            .text_size(theme.scale(11.))
            .text_color(if destructive {
                theme.red
            } else {
                theme.text_faint
            })
            .child(
                DOC_COPY_MODES
                    .iter()
                    .find(|(m, _, _)| *m == copy.mode)
                    .map(|(_, _, hint)| *hint)
                    .unwrap_or_default(),
            );

        let source_line = match &copy.filter {
            Some(f) => format!("From {}.{} matching {f}", copy.source_db, copy.source_coll),
            None => format!(
                "From {}.{} (all documents)",
                copy.source_db, copy.source_coll
            ),
        };

        let (run_view, cancel_view, close_view) = (view.clone(), view.clone(), view.clone());
        let footer = div()
            .flex()
            .flex_1()
            .justify_end()
            .gap_2()
            .child(
                Button::new("doc-copy-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        cancel_view
                            .update(cx, |this, cx| this.doc_close_copy(session, cx))
                            .ok();
                    }),
            )
            .child(
                Button::new("doc-copy-run", if destructive { "Replace" } else { "Copy" })
                    .variant(if destructive {
                        ButtonVariant::Danger
                    } else {
                        ButtonVariant::Primary
                    })
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        run_view
                            .update(cx, |this, cx| this.doc_start_copy(session, cx))
                            .ok();
                    }),
            );

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_color(theme.text_muted).child(source_line))
            .child(target_row)
            .children(known)
            .child(labeled_field("Database", &theme).child(copy.db_input.clone()))
            .child(labeled_field("Collection", &theme).child(copy.coll_input.clone()))
            .child(mode_row)
            .child(mode_hint);

        Some(
            Modal::new("doc-copy")
                .title(crate::i18n::tr!("doc.copy_collection", "Copy collection"))
                .width(px(600.))
                .focus_handle(self.modal_focus.clone())
                .on_close(move |_, cx| {
                    close_view
                        .update(cx, |this, cx| this.doc_close_copy(session, cx))
                        .ok();
                })
                .footer(footer)
                .child(body)
                .into_any_element(),
        )
    }

    // --- import ------------------------------------------------------------

    /// Open the import modal for a collection. No file is chosen yet: the dialog
    /// opens on the target, and picking a source is its first step.
    pub(crate) fn doc_open_import(
        &mut self,
        session: SessionId,
        db: String,
        coll: String,
        cx: &mut Context<Self>,
    ) {
        self.doc_close_actions_menu(session, cx);
        self.doc_close_coll_menu(session, cx);
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        if view.read_only {
            return;
        }
        let epoch = view
            .tabs
            .iter()
            .find_map(|t| match &t.state {
                super::MongoTabState::Collection(c) if c.db == db && c.coll == coll => {
                    Some(c.epoch)
                }
                _ => None,
            })
            .unwrap_or(view.epoch);
        view.import = Some(DocImportState {
            db,
            coll,
            epoch,
            path: None,
            format: DocImportFormat::Json,
            mode: DocImportMode::Insert,
            preview: Vec::new(),
            error: None,
            peeking: false,
        });
        self.focus_modal = true;
        cx.notify();
    }

    pub(crate) fn doc_close_import(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.import = None;
        }
        self.refocus_root = true;
        cx.notify();
    }

    /// Pick the source file, then peek it so the dialog can show what it holds.
    pub(crate) fn doc_choose_import_path(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose import file".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| this.doc_set_import_path(session, path, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn doc_set_import_path(
        &mut self,
        session: SessionId,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(import) = self.doc_import_mut(session) else {
            return;
        };
        // The extension is a guess the user can override, not a decision: a `.txt`
        // of NDJSON is common and a `.json` of NDJSON is not unheard of.
        if let Some(format) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(DocImportFormat::from_extension)
        {
            import.format = format;
        }
        import.path = Some(path);
        self.doc_peek_import(session, cx);
    }

    pub(crate) fn doc_set_import_format(
        &mut self,
        session: SessionId,
        format: DocImportFormat,
        cx: &mut Context<Self>,
    ) {
        if let Some(import) = self.doc_import_mut(session) {
            import.format = format;
        }
        // The preview is format-dependent, so re-read it rather than leave a stale
        // one that belongs to the format the user just changed away from.
        self.doc_peek_import(session, cx);
    }

    pub(crate) fn doc_set_import_mode(
        &mut self,
        session: SessionId,
        mode: DocImportMode,
        cx: &mut Context<Self>,
    ) {
        if let Some(import) = self.doc_import_mut(session) {
            import.mode = mode;
        }
        cx.notify();
    }

    /// Ask the service to parse the first documents of the chosen file.
    fn doc_peek_import(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(import) = self.doc_import_mut(session) else {
            return;
        };
        let Some(path) = import.path.clone() else {
            return;
        };
        let (epoch, format) = (import.epoch, import.format);
        import.preview.clear();
        import.error = None;
        import.peeking = true;
        self.service.send_to(
            session,
            Command::DocImportPeek {
                epoch,
                path,
                format,
                limit: PREVIEW_LIMIT,
            },
        );
        cx.notify();
    }

    /// `DocImportPreview`: land the parsed sample (or the failure) on the dialog.
    pub(crate) fn on_doc_import_preview(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        docs: Vec<String>,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(import) = self
            .conn_mut(session)
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.import.as_mut())
            // A reply for a dialog that has since been repointed is not this one's.
            .filter(|i| i.epoch == epoch)
        {
            import.preview = docs;
            import.error = error;
            import.peeking = false;
        }
        cx.notify();
    }

    /// Start the import and hand the reporting over to the transfer toast.
    pub(crate) fn doc_start_import(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(import) = self.doc_import_mut(session) else {
            return;
        };
        let Some(path) = import.path.clone() else {
            return;
        };
        let (db, coll, epoch, format, mode) = (
            import.db.clone(),
            import.coll.clone(),
            import.epoch,
            import.format,
            import.mode,
        );
        let id = red_service::OpId::new(self.next_export_id);
        self.next_export_id += 1;
        self.service.send_to(
            session,
            Command::DocImport {
                epoch,
                id,
                db,
                coll,
                path,
                format,
                mode,
            },
        );
        // An import streams a file of unknown length, so the toast counts rather
        // than reporting a percentage; `0` is what tells it to.
        self.push_transfer_toast(id, "Importing…", 0, crate::app::TransferKind::Import, cx);
        self.doc_close_import(session, cx);
    }

    fn doc_import_mut(&mut self, session: SessionId) -> Option<&mut DocImportState> {
        self.conn_mut(Some(session))?
            .doc_view
            .as_mut()?
            .import
            .as_mut()
    }

    /// The "Import documents" modal: pick a source, see what RED parses out of it,
    /// then choose whether a repeated `_id` is a collision or an update.
    pub(crate) fn render_doc_import_modal(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use crate::connect::labeled_field;

        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let session = active.session;
        let import = active.doc_view.as_ref()?.import.as_ref()?;
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();

        let pick_view = view.clone();
        let file_label = import
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "No file chosen".to_string());
        let file_row = labeled_field("File", &theme).child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_color(if import.path.is_some() {
                            theme.text
                        } else {
                            theme.text_faint
                        })
                        .child(file_label),
                )
                .child(
                    Button::new("doc-import-pick", "Choose file…")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Secondary)
                        .on_click(move |_, _, cx| {
                            pick_view
                                .update(cx, |this, cx| this.doc_choose_import_path(session, cx))
                                .ok();
                        }),
                ),
        );

        let format_view = view.clone();
        let format_ix = DOC_IMPORT_FORMATS
            .iter()
            .position(|(f, _)| *f == import.format)
            .unwrap_or(0);
        let format_row = labeled_field("Format", &theme).child(
            DOC_IMPORT_FORMATS
                .iter()
                .fold(Segmented::new("doc-import-format"), |seg, (_, label)| {
                    seg.segment(*label)
                })
                .selected(format_ix)
                .on_select(move |ix, _, cx| {
                    let Some((format, _)) = DOC_IMPORT_FORMATS.get(ix) else {
                        return;
                    };
                    let format = *format;
                    format_view
                        .update(cx, |this, cx| {
                            this.doc_set_import_format(session, format, cx)
                        })
                        .ok();
                }),
        );

        let mode_view = view.clone();
        let mode_ix = DOC_IMPORT_MODES
            .iter()
            .position(|(m, _, _)| *m == import.mode)
            .unwrap_or(0);
        let mode_row = labeled_field("On duplicate _id", &theme).child(
            DOC_IMPORT_MODES
                .iter()
                .fold(Segmented::new("doc-import-mode"), |seg, (_, label, _)| {
                    seg.segment(*label)
                })
                .selected(mode_ix)
                .on_select(move |ix, _, cx| {
                    let Some((mode, _, _)) = DOC_IMPORT_MODES.get(ix) else {
                        return;
                    };
                    let mode = *mode;
                    mode_view
                        .update(cx, |this, cx| this.doc_set_import_mode(session, mode, cx))
                        .ok();
                }),
        );
        let mode_hint = div()
            .text_size(theme.scale(11.))
            .text_color(theme.text_faint)
            .child(
                DOC_IMPORT_MODES
                    .iter()
                    .find(|(m, _, _)| *m == import.mode)
                    .map(|(_, _, hint)| *hint)
                    .unwrap_or_default(),
            );

        let preview = if import.path.is_none() {
            None
        } else if import.peeking {
            Some(
                div()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_faint)
                    .child("Reading…")
                    .into_any_element(),
            )
        } else {
            let body = import.preview.iter().fold(
                div().flex().flex_col().gap_1(),
                |list, doc: &String| {
                    list.child(
                        div()
                            .font_family(self.settings.editor.font_family.clone())
                            .text_size(theme.scale(11.))
                            .text_color(theme.text_muted)
                            .child(doc.clone()),
                    )
                },
            );
            Some(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .max_h(px(180.))
                    .overflow_hidden()
                    .p_2()
                    .rounded(px(4.))
                    .bg(theme.bg_panel)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_size(theme.scale(11.))
                            .text_color(theme.text_faint)
                            .child(if import.preview.is_empty() {
                                "No documents found in this file.".to_string()
                            } else {
                                format!(
                                    "First {} document(s), as they will be written:",
                                    import.preview.len()
                                )
                            }),
                    )
                    .child(body)
                    .into_any_element(),
            )
        };

        let error = import.error.as_ref().map(|e| {
            div()
                .text_size(theme.scale(11.))
                .text_color(theme.red)
                .child(e.clone())
        });

        // Nothing parsed means nothing to write: a run would only produce an empty
        // "imported 0" toast, so the button stays off until the peek proves there
        // is something in the file.
        let ready = import.path.is_some() && !import.peeking && !import.preview.is_empty();
        let (run_view, cancel_view, close_view) = (view.clone(), view.clone(), view.clone());
        let footer = div()
            .flex()
            .flex_1()
            .justify_end()
            .gap_2()
            .child(
                Button::new("doc-import-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        cancel_view
                            .update(cx, |this, cx| this.doc_close_import(session, cx))
                            .ok();
                    }),
            )
            .child(
                Button::new("doc-import-run", "Import")
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Sm)
                    .disabled(!ready)
                    .on_click(move |_, _, cx| {
                        run_view
                            .update(cx, |this, cx| this.doc_start_import(session, cx))
                            .ok();
                    }),
            );

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child(format!("Into {}.{}", import.db, import.coll)),
            )
            .child(file_row)
            .child(format_row)
            .child(mode_row)
            .child(mode_hint)
            .children(error)
            .children(preview);

        Some(
            Modal::new("doc-import")
                .title(crate::i18n::tr!("doc.import_documents", "Import documents"))
                .width(px(640.))
                .focus_handle(self.modal_focus.clone())
                .on_close(move |_, cx| {
                    close_view
                        .update(cx, |this, cx| this.doc_close_import(session, cx))
                        .ok();
                })
                .footer(footer)
                .child(body)
                .into_any_element(),
        )
    }

    /// The "Export documents" modal. Root-mounted like the Redis export dialog, so
    /// it overlays the whole shell rather than one pane.
    ///
    /// The format's caveat is on the dialog rather than in a tooltip: CSV and Excel
    /// flatten onto columns sampled from the collection, and finding that out from
    /// a missing column in a spreadsheet is finding it out too late.
    pub(crate) fn render_doc_export_modal(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use crate::connect::labeled_field;

        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let session = active.session;
        let export = active.doc_view.as_ref()?.export.as_ref()?;
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();

        let scope_view = view.clone();
        let scope_row = labeled_field("Scope", &theme).child(
            export
                .scopes
                .iter()
                .fold(Segmented::new("doc-export-scope"), |seg, (label, _)| {
                    seg.segment(label.clone())
                })
                .selected(export.scope_ix)
                .on_select(move |ix, _, cx| {
                    scope_view
                        .update(cx, |this, cx| this.doc_set_export_scope(session, ix, cx))
                        .ok();
                }),
        );

        let format_view = view.clone();
        let format_ix = DOC_EXPORT_FORMATS
            .iter()
            .position(|(f, _, _)| *f == export.format)
            .unwrap_or(0);
        let format_row = labeled_field("Format", &theme).child(
            DOC_EXPORT_FORMATS
                .iter()
                .fold(Segmented::new("doc-export-format"), |seg, (_, label, _)| {
                    seg.segment(*label)
                })
                .selected(format_ix)
                .on_select(move |ix, _, cx| {
                    let Some((format, _, _)) = DOC_EXPORT_FORMATS.get(ix) else {
                        return;
                    };
                    let format = *format;
                    format_view
                        .update(cx, |this, cx| {
                            this.doc_set_export_format(session, format, cx)
                        })
                        .ok();
                }),
        );
        let format_hint = div()
            .text_size(theme.scale(11.))
            .text_color(if export.format.is_tabular() {
                theme.yellow
            } else {
                theme.text_faint
            })
            .child(
                DOC_EXPORT_FORMATS
                    .iter()
                    .find(|(f, _, _)| *f == export.format)
                    .map(|(_, _, hint)| *hint)
                    .unwrap_or_default(),
            );

        let (save_view, cancel_view, close_view) = (view.clone(), view.clone(), view.clone());
        let footer = div()
            .flex()
            .flex_1()
            .justify_end()
            .gap_2()
            .child(
                Button::new("doc-export-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        cancel_view
                            .update(cx, |this, cx| this.doc_close_export(session, cx))
                            .ok();
                    }),
            )
            .child(
                Button::new("doc-export-save", "Choose file…")
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        save_view
                            .update(cx, |this, cx| this.doc_choose_export_path(session, cx))
                            .ok();
                    }),
            );

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child(format!("{}.{}", export.db, export.coll)),
            )
            .child(scope_row)
            .child(format_row)
            .child(format_hint);

        Some(
            Modal::new("doc-export")
                .title(crate::i18n::tr!("doc.export_documents", "Export documents"))
                .width(px(560.))
                .focus_handle(self.modal_focus.clone())
                .on_close(move |_, cx| {
                    close_view
                        .update(cx, |this, cx| this.doc_close_export(session, cx))
                        .ok();
                })
                .footer(footer)
                .child(body)
                .into_any_element(),
        )
    }
}
