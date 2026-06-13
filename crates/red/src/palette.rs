//! The command palette — RED's ⌘K overlay. The generic chrome (search field,
//! fuzzy filter, keyboard navigation, styled rows) lives in Flint as
//! [`flint::Palette`]; this module owns the *domain* half: which commands exist
//! in the current [`Phase`], and what each one does.
//!
//! Flow: `toggle_palette` builds a phase-appropriate command list, hands the
//! labels/hints to a fresh `Palette` entity, and remembers the `id → Cmd`
//! mapping. When the palette emits [`PaletteEvent::Activate`], we look the id
//! back up and run the matching `AppState` method — the same one the equivalent
//! button calls.

use flint::{Palette, PaletteEvent, PaletteItem, ToastVariant};
use gpui::{actions, prelude::*, Context, ElementId, Entity, SharedString};
use red_core::{ColumnValue, EditOp, TableRef, Value};

use crate::app::{AppState, Phase};

actions!(red, [ToggleCommandPalette, GoToRow, CopyResult]);

/// A command the palette can run. Each maps to one existing `AppState` action.
#[derive(Clone, Copy)]
pub(crate) enum Cmd {
    OpenSettings,
    /// Open `settings.toml` in the user's editor (file-first workflow).
    OpenSettingsFile,
    /// Open the bundled, commented reference defaults (RED's settings docs).
    OpenDefaultSettings,
    /// Connect to the saved connection at this index (disconnected phase).
    Connect(usize),
    /// Open the connection switcher popover (the ⌘P switcher).
    SwitchConnection,
    RunQuery,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    ToggleHistory,
    ToggleSidebar,
    RefreshSchema,
    Disconnect,
    /// Move keyboard focus to the schema / editor / grid pane.
    FocusSchema,
    FocusEditor,
    FocusGrid,
    /// Reveal the sidebar and focus its filter field (search schema).
    SearchSchema,
    /// Copy the result grid's current selection (TSV).
    CopySelection,
    /// Open a new-connection form (disconnected phase).
    NewConnection,
    /// Open the "go to row…" prompt (only when a result is open).
    GoToRow,
    /// Open the keyboard-shortcuts reference overlay.
    ShowShortcuts,
    /// Save the active tab's query as a named snippet (opens the name prompt).
    SaveQuery,
    /// Open the saved-query picker.
    OpenSavedQueries,
    /// Open the saved query at this index (into a new tab) — picker activation.
    OpenSavedQuery(usize),
    /// EXPLAIN the active tab's query and open the plan view (B4).
    Explain,
    /// EXPLAIN ANALYZE the active tab's query (runs it — read queries only).
    ExplainAnalyze,
    /// Edit the focused result cell (Track B5) — opens the value prompt. Offered
    /// only on an editable cell of a writable, edit-enabled connection.
    EditCell,
}

/// Which free-text prompt the single palette slot is currently serving, so a
/// [`PaletteEvent::Submit`] routes to the right handler. Command-list palettes
/// (the default and the saved-query picker) ignore this — they emit `Activate`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptKind {
    GoToRow,
    SaveQuery,
    /// Entering a new value for the focused cell (Track B5). The target cell is
    /// stashed in [`AppState::pending_edit`] while this prompt is open.
    EditCell,
}

impl AppState {
    /// ⌘K: open the command palette, or close it if it's already open. The
    /// palette focuses its own field on first paint, so no `Window` is needed.
    pub(crate) fn toggle_palette(&mut self, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette();
            cx.notify();
            return;
        }

        let entries = self.palette_entries();
        self.palette_cmds = entries
            .iter()
            .map(|(item, cmd)| (item.id.clone(), *cmd))
            .collect();
        let items: Vec<PaletteItem> = entries.into_iter().map(|(item, _)| item).collect();

        let palette = cx.new(|cx| {
            let mut p = Palette::new(cx);
            p.set_placeholder("Execute a command…", cx);
            p.set_items(items, cx);
            p
        });
        let sub = cx.subscribe(&palette, Self::on_palette_event);
        self.palette = Some((palette, sub));
        cx.notify();
    }

    /// ⌃G (or the "go to row…" command): open a prompt for a row number. No-op
    /// when no result is open — there's nothing to navigate.
    pub(crate) fn open_goto_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(total) = self.active_result_total() else {
            return;
        };
        let placeholder = format!("Go to row 1–{}", total.max(1));
        let prompt = cx.new(|cx| {
            let mut p = Palette::new(cx).prompt();
            p.set_placeholder(placeholder, cx);
            p
        });
        let sub = cx.subscribe(&prompt, Self::on_palette_event);
        self.palette = Some((prompt, sub));
        self.palette_cmds.clear();
        self.palette_prompt = PromptKind::GoToRow;
        cx.notify();
    }

    /// Total rows of the active tab's open result, if any.
    fn active_result_total(&self) -> Option<usize> {
        match &self.phase {
            Phase::Connected(active) => active.active_result().map(|g| g.total_rows()),
            _ => None,
        }
    }

    /// Close whichever palette (command or prompt) is open, and ask the next
    /// render to pull focus back to the root. Without that, the just-dropped
    /// palette input leaves `window.focused()` dangling, so the *next* global
    /// ⌘K finds no dispatch target and the palette won't reopen.
    fn close_palette(&mut self) {
        self.palette = None;
        self.palette_cmds.clear();
        self.refocus_root = true;
    }

    fn on_palette_event(
        &mut self,
        _palette: Entity<Palette>,
        event: &PaletteEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            PaletteEvent::Activate(id) => {
                let cmd = self
                    .palette_cmds
                    .iter()
                    .find(|(eid, _)| eid == id)
                    .map(|(_, cmd)| *cmd);
                self.close_palette();
                if let Some(cmd) = cmd {
                    self.run_command(cmd, cx);
                }
            }
            // Prompt mode submits free text — route by which prompt is open.
            PaletteEvent::Submit(text) => {
                let kind = self.palette_prompt;
                self.close_palette();
                match kind {
                    PromptKind::GoToRow => self.submit_goto(text, cx),
                    PromptKind::SaveQuery => self.submit_save(text, cx),
                    PromptKind::EditCell => self.submit_edit(text, cx),
                }
            }
            PaletteEvent::Dismiss => self.close_palette(),
        }
        cx.notify();
    }

    /// Parse the go-to-row prompt's text and navigate, or toast on bad input.
    /// Digit-group separators (`,` `_` spaces) are tolerated so a pasted "1,000"
    /// works, but any *other* non-digit makes the input invalid (so "1abc2" is
    /// rejected rather than silently read as 12).
    fn submit_goto(&mut self, text: &str, cx: &mut Context<Self>) {
        let trimmed = text.trim();
        let is_sep = |c: char| matches!(c, ',' | '_' | ' ');
        let cleaned: Option<String> = (!trimmed.is_empty()
            && trimmed.chars().all(|c| c.is_ascii_digit() || is_sep(c)))
        .then(|| trimmed.chars().filter(|c| c.is_ascii_digit()).collect());
        match cleaned.as_deref().and_then(|s| s.parse::<usize>().ok()) {
            Some(n) if n >= 1 => self.go_to_row(n, cx),
            _ => {
                self.notify(
                    ToastVariant::Error,
                    format!("“{}” isn't a valid row number", text.trim()),
                    cx,
                );
            }
        }
    }

    fn run_command(&mut self, cmd: Cmd, cx: &mut Context<Self>) {
        match cmd {
            Cmd::OpenSettings => self.open_settings(cx),
            Cmd::OpenSettingsFile => self.open_settings_file(cx),
            Cmd::OpenDefaultSettings => self.open_default_settings(cx),
            Cmd::Connect(index) => self.connect(index, cx),
            // The switcher's `toggle` needs a `Window` to focus its field; defer
            // to the next render (drained there), like the pane-focus jumps.
            Cmd::SwitchConnection => self.open_switcher = true,
            Cmd::RunQuery => self.run_editor_query(cx),
            Cmd::NewTab => self.new_query(cx),
            Cmd::CloseTab => self.close_active_tab(cx),
            Cmd::NextTab => {
                self.step_active_tab(true, cx);
            }
            Cmd::PrevTab => {
                self.step_active_tab(false, cx);
            }
            Cmd::ToggleHistory => self.toggle_history(cx),
            Cmd::ToggleSidebar => self.toggle_sidebar(cx),
            Cmd::RefreshSchema => self.refresh_schema(),
            Cmd::Disconnect => self.disconnect(cx),
            // Pane focus needs a `Window`; defer it to the next render (drained
            // there) the same way the editor's Esc-to-grid does.
            Cmd::FocusSchema => self.pending_focus = Some(crate::app::Pane::Schema),
            Cmd::FocusEditor => self.pending_focus = Some(crate::app::Pane::Editor),
            Cmd::FocusGrid => self.pending_focus = Some(crate::app::Pane::Grid),
            // Deferred to the next render (needs a `Window`), like the focus jumps.
            Cmd::SearchSchema => self.focus_search = true,
            Cmd::CopySelection => self.copy_result_selection(cx),
            Cmd::NewConnection => self.open_new_form(cx),
            Cmd::GoToRow => self.open_goto_prompt(cx),
            Cmd::ShowShortcuts => self.toggle_shortcuts(cx),
            Cmd::SaveQuery => self.open_save_prompt(cx),
            Cmd::OpenSavedQueries => self.open_saved_picker(cx),
            Cmd::OpenSavedQuery(index) => self.open_saved_query(index, cx),
            Cmd::Explain => self.explain_query(false, cx),
            Cmd::ExplainAnalyze => self.explain_query(true, cx),
            Cmd::EditCell => self.open_edit_prompt(cx),
        }
    }

    /// Whether guarded in-grid editing is enabled for the active connection (Track
    /// B5): any writable (non-read-only) connection. Read-only is the safe default.
    pub(crate) fn editing_enabled(&self) -> bool {
        matches!(
            &self.phase,
            Phase::Connected(active) if !active.config.read_only
        )
    }

    /// The focused result cell's edit target, when editing is enabled and the cell
    /// is editable (a single-table keyed browse, non-PK, non-clipped). `None`
    /// otherwise — the entry point and palette gate both consult this.
    pub(crate) fn active_edit_target(&self) -> Option<crate::app::EditContext> {
        if !self.editing_enabled() {
            return None;
        }
        let gutter = self.gutter();
        match &self.phase {
            Phase::Connected(active) => active.active_result().and_then(|g| g.edit_target(gutter)),
            _ => None,
        }
    }

    /// Open the value prompt for the focused cell (Track B5). No-op when the cell
    /// isn't editable. Prefills the prompt with the cell's current text so a small
    /// tweak is one keystroke; submitting routes to [`Self::submit_edit`].
    pub(crate) fn open_edit_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(ctx) = self.active_edit_target() else {
            return;
        };
        // Prefill with the current value's text (empty for NULL — an empty submit
        // re-clears it). A blob/clipped cell never reaches here (filtered above).
        let current = match &ctx.original {
            red_core::Value::Null => String::new(),
            other => other.to_string(),
        };
        let placeholder = format!("New value for “{}”", ctx.column);
        let prompt = cx.new(|cx| {
            let mut p = Palette::new(cx).prompt();
            p.set_placeholder(placeholder, cx);
            p.set_query(&current, cx);
            p
        });
        let sub = cx.subscribe(&prompt, Self::on_palette_event);
        self.palette = Some((prompt, sub));
        self.palette_cmds.clear();
        self.palette_prompt = PromptKind::EditCell;
        self.pending_edit = Some(ctx);
        cx.notify();
    }

    /// Coerce the value typed into the edit prompt and stage it for confirmation
    /// (Track B5). A coercion failure (e.g. text in an integer column) toasts the
    /// reason instead of opening the preview.
    fn submit_edit(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(ctx) = self.pending_edit.take() else {
            return;
        };
        match red_core::coerce_edit_value(text, ctx.decl_type.as_deref()) {
            Ok(value) => self.stage_cell_edit(ctx, value, cx),
            Err(reason) => {
                self.notify(ToastVariant::Error, reason, cx);
            }
        }
    }

    /// Build the `UPDATE` [`EditOp`] for `ctx`'s cell with `value` and open the
    /// guarded confirm preview (Track B5) — the single staging point both the
    /// inspector's inline editor and the palette prompt funnel through. A value
    /// equal to the original is a no-op (toast, no write). The cell context (now
    /// carrying `value`) rides `pending_edit` through the confirm so a committed
    /// edit can patch the resident cell in place.
    pub(crate) fn stage_cell_edit(
        &mut self,
        mut ctx: crate::app::EditContext,
        value: Value,
        cx: &mut Context<Self>,
    ) {
        if value == ctx.original {
            self.notify(ToastVariant::Info, "No change — value is the same.", cx);
            return;
        }
        let op = EditOp::Update {
            table: TableRef {
                schema: Some(ctx.table.0.clone()),
                name: ctx.table.1.clone(),
            },
            key: ColumnValue {
                column: ctx.pk_column.clone(),
                value: ctx.pk_value.clone(),
            },
            set: vec![ColumnValue {
                column: ctx.column.clone(),
                value: value.clone(),
            }],
        };
        let epoch = ctx.epoch;
        ctx.new_value = Some(value);
        self.pending_edit = Some(ctx);
        self.confirm_exec = Some(crate::app::PendingWrite::Edit { op, epoch });
        self.focus_modal = true;
        cx.notify();
    }

    /// The commands available in the current phase, each paired with its `Cmd`.
    /// Phase-specific actions come first (what the user most likely wants), then
    /// the always-available ones.
    fn palette_entries(&self) -> Vec<(PaletteItem, Cmd)> {
        let mut out: Vec<(PaletteItem, Cmd)> = Vec::new();

        match &self.phase {
            Phase::Connected(active) => {
                out.push((
                    PaletteItem::new("cmd:run", "query: run").hint("⌘↵"),
                    Cmd::RunQuery,
                ));
                out.push((
                    PaletteItem::new("cmd:new-tab", "query: new tab").hint("⌘T"),
                    Cmd::NewTab,
                ));
                // Tab management — close needs an open tab; switching needs two.
                if active.active().is_some() {
                    out.push((
                        PaletteItem::new("cmd:close-tab", "query: close tab").hint("⌘W"),
                        Cmd::CloseTab,
                    ));
                }
                if active.tabs.len() > 1 {
                    out.push((
                        PaletteItem::new("cmd:next-tab", "query: next tab").hint("⌃Tab"),
                        Cmd::NextTab,
                    ));
                    out.push((
                        PaletteItem::new("cmd:prev-tab", "query: previous tab").hint("⌃⇧Tab"),
                        Cmd::PrevTab,
                    ));
                }
                // Only meaningful with rows on screen to navigate / copy.
                if active.active_result().is_some() {
                    out.push((
                        PaletteItem::new("cmd:goto-row", "go to row…").hint("⌃G"),
                        Cmd::GoToRow,
                    ));
                    out.push((
                        PaletteItem::new("cmd:copy", "result: copy selection").hint("⌘C"),
                        Cmd::CopySelection,
                    ));
                }
                // Guarded cell edit (B5) — only when the focused cell is editable
                // (writable + edit-enabled connection, single-table keyed browse).
                if self.active_edit_target().is_some() {
                    out.push((
                        PaletteItem::new("cmd:edit-cell", "data: edit cell…"),
                        Cmd::EditCell,
                    ));
                }
                out.push((
                    PaletteItem::new("cmd:history", "query: toggle history"),
                    Cmd::ToggleHistory,
                ));
                // Saved queries (B3) — save needs an open tab to save *from*; the
                // picker is always offered (it reports "none yet" when empty).
                if active.active().is_some() {
                    out.push((
                        PaletteItem::new("cmd:save-query", "query: save…").hint("⇧⌘S"),
                        Cmd::SaveQuery,
                    ));
                }
                out.push((
                    PaletteItem::new("cmd:open-saved", "query: open saved…").hint("⇧⌘O"),
                    Cmd::OpenSavedQueries,
                ));
                // EXPLAIN (B4) — explain needs a query to explain; analyze runs the
                // statement and is offered only on engines that support it (not
                // SQLite, which has no ANALYZE).
                if active.active().is_some() {
                    out.push((
                        PaletteItem::new("cmd:explain", "query: explain plan").hint("⇧⌘E"),
                        Cmd::Explain,
                    ));
                    if active.config.kind != red_core::DbKind::Sqlite {
                        out.push((
                            PaletteItem::new("cmd:explain-analyze", "query: explain analyze"),
                            Cmd::ExplainAnalyze,
                        ));
                    }
                }
                // Pane focus.
                out.push((
                    PaletteItem::new("cmd:focus-schema", "focus: schema sidebar").hint("⌥⌘1"),
                    Cmd::FocusSchema,
                ));
                out.push((
                    PaletteItem::new("cmd:focus-editor", "focus: editor").hint("⌥⌘2"),
                    Cmd::FocusEditor,
                ));
                out.push((
                    PaletteItem::new("cmd:focus-grid", "focus: result grid").hint("⌥⌘3"),
                    Cmd::FocusGrid,
                ));
                out.push((
                    PaletteItem::new("cmd:search-schema", "schema: search").hint("⌘F"),
                    Cmd::SearchSchema,
                ));
                out.push((
                    PaletteItem::new("cmd:sidebar", "view: toggle sidebar").hint("⌘B"),
                    Cmd::ToggleSidebar,
                ));
                out.push((
                    PaletteItem::new("cmd:refresh", "schema: refresh").hint("⌘R"),
                    Cmd::RefreshSchema,
                ));
                out.push((
                    PaletteItem::new("cmd:disconnect", "connection: disconnect"),
                    Cmd::Disconnect,
                ));
            }
            Phase::Disconnected => {
                for (index, conn) in self.connections.iter().enumerate() {
                    let id = ElementId::from(SharedString::from(format!("cmd:connect:{index}")));
                    out.push((
                        PaletteItem::new(id, format!("connect: {}", conn.config.name)),
                        Cmd::Connect(index),
                    ));
                }
                out.push((
                    PaletteItem::new("cmd:new-conn", "connection: new").hint("⌘N"),
                    Cmd::NewConnection,
                ));
            }
            // Mid-connect there's nothing query-shaped to do; only globals show.
            Phase::Connecting(_) => {}
        }

        out.push((
            PaletteItem::new("cmd:switch-conn", "connection: switch…").hint("⌘P"),
            Cmd::SwitchConnection,
        ));
        out.push((
            PaletteItem::new("cmd:shortcuts", "view: keyboard shortcuts").hint("⌘/"),
            Cmd::ShowShortcuts,
        ));
        out.push((
            PaletteItem::new("cmd:settings", "view: settings").hint("⌘,"),
            Cmd::OpenSettings,
        ));
        out.push((
            PaletteItem::new("cmd:settings-file", "settings: open file"),
            Cmd::OpenSettingsFile,
        ));
        out.push((
            PaletteItem::new("cmd:settings-default", "settings: open default settings"),
            Cmd::OpenDefaultSettings,
        ));
        out
    }

    /// ⇧⌘S / "query: save…": open a prompt to name the active tab's query, then
    /// persist it as a `.sql` file. The prompt's placeholder suggests a name
    /// derived from the SQL (the history label); submitting empty accepts it.
    pub(crate) fn open_save_prompt(&mut self, cx: &mut Context<Self>) {
        let sql = match &self.phase {
            Phase::Connected(active) => active.active().map(|t| t.editor.read(cx).content()),
            _ => None,
        };
        let Some(sql) = sql else { return };
        if sql.trim().is_empty() {
            self.notify(
                ToastVariant::Error,
                "Nothing to save — the editor is empty.",
                cx,
            );
            return;
        }
        let suggestion = crate::editor::history_label(&sql);
        let placeholder = if suggestion.is_empty() {
            "Name this query…".to_string()
        } else {
            format!("Save as “{suggestion}”")
        };
        let prompt = cx.new(|cx| {
            let mut p = Palette::new(cx).prompt();
            p.set_placeholder(placeholder, cx);
            p
        });
        let sub = cx.subscribe(&prompt, Self::on_palette_event);
        self.palette = Some((prompt, sub));
        self.palette_cmds.clear();
        self.palette_prompt = PromptKind::SaveQuery;
        cx.notify();
    }

    /// Write the active tab's query under `name` (or the suggested name when the
    /// prompt was submitted empty). Re-reads the editor at submit time so it can't
    /// save stale text.
    fn submit_save(&mut self, name: &str, cx: &mut Context<Self>) {
        let sql = match &self.phase {
            Phase::Connected(active) => active.active().map(|t| t.editor.read(cx).content()),
            _ => None,
        };
        let Some(sql) = sql.filter(|s| !s.trim().is_empty()) else {
            self.notify(ToastVariant::Error, "Nothing to save.", cx);
            return;
        };
        let name = match name.trim() {
            "" => crate::editor::history_label(&sql),
            typed => typed.to_string(),
        };
        if name.trim().is_empty() {
            self.notify(ToastVariant::Error, "Give the query a name.", cx);
            return;
        }
        match crate::queries::save(&name, &sql) {
            Ok(_) => {
                self.notify(ToastVariant::Success, format!("Saved query “{name}”."), cx);
            }
            Err(e) => {
                self.notify(ToastVariant::Error, format!("Couldn't save query: {e}"), cx);
            }
        }
    }

    /// ⇧⌘O / "query: open saved…": load the saved-query files and open a picker
    /// over them. Enumerating happens here, on demand — never at startup — so saved
    /// queries cost nothing at idle and external edits show up on each open.
    pub(crate) fn open_saved_picker(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Connected(_)) {
            return;
        }
        let queries = crate::queries::load();
        if queries.is_empty() {
            self.notify(
                ToastVariant::Info,
                "No saved queries yet — save one with ⇧⌘S.",
                cx,
            );
            return;
        }
        let entries: Vec<(PaletteItem, Cmd)> = queries
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let id = ElementId::from(SharedString::from(format!("saved:{i}")));
                let mut item = PaletteItem::new(id, q.name.clone());
                if let Some(desc) = &q.description {
                    item = item.hint(desc.clone());
                }
                (item, Cmd::OpenSavedQuery(i))
            })
            .collect();
        self.saved_queries = queries;
        self.palette_cmds = entries
            .iter()
            .map(|(item, cmd)| (item.id.clone(), *cmd))
            .collect();
        let items: Vec<PaletteItem> = entries.into_iter().map(|(item, _)| item).collect();

        let palette = cx.new(|cx| {
            let mut p = Palette::new(cx);
            p.set_placeholder("Open saved query…", cx);
            p.set_items(items, cx);
            p
        });
        let sub = cx.subscribe(&palette, Self::on_palette_event);
        self.palette = Some((palette, sub));
        cx.notify();
    }

    /// Open the picked saved query in a fresh tab titled with its name (rather than
    /// stomping the active editor), ready to run.
    fn open_saved_query(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(query) = self.saved_queries.get(index).cloned() else {
            return;
        };
        if !matches!(self.phase, Phase::Connected(_)) {
            return;
        }
        let tab = crate::app::QueryTab::new(query.name, cx);
        let at = self.push_tab(tab, cx);
        let editor = match &self.phase {
            Phase::Connected(active) => active.tabs.get(at).map(|t| t.editor.clone()),
            _ => None,
        };
        if let Some(editor) = editor {
            editor.update(cx, |editor, cx| editor.set_content(query.sql, cx));
        }
        cx.notify();
    }
}
