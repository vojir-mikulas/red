//! The command palette, RED's ⌘K overlay. The generic chrome (search field,
//! fuzzy filter, keyboard navigation, styled rows) lives in Flint as
//! [`flint::Palette`]; this module owns the *domain* half: which commands exist
//! in the current [`Phase`], and what each one does.
//!
//! Flow: `toggle_palette` builds a phase-appropriate command list, hands the
//! labels/hints to a fresh `Palette` entity, and remembers the `id → Cmd`
//! mapping. When the palette emits [`PaletteEvent::Activate`], we look the id
//! back up and run the matching `AppState` method, the same one the equivalent
//! button calls.

use flint::{Palette, PaletteEvent, PaletteItem, ToastVariant};
use gpui::{App, Context, ElementId, Entity, SharedString, actions, prelude::*};
use red_core::{ColumnMap, ColumnMeta, CopyMode, TableRef};
use red_service::{Command, SessionId};

use crate::app::{AppState, PendingCopyNewTable, PendingCopyPeek, Phase};

actions!(red, [ToggleCommandPalette, GoToRow, CopyResult]);

/// A command the palette can run. Each maps to one existing `AppState` action.
#[derive(Clone, Copy)]
pub(crate) enum Cmd {
    OpenSettings,
    /// Open `settings.toml` in the user's editor (file-first workflow).
    OpenSettingsFile,
    /// Open the bundled, commented reference defaults (RED's settings docs).
    OpenDefaultSettings,
    /// Open `keymap.toml` to customize keybindings (file-first workflow).
    OpenKeymapFile,
    /// Open the "Remove all RED data" factory-reset confirmation.
    RemoveAllData,
    /// Toggle vim-style navigation (the `[keymap] vim_mode` setting).
    ToggleVimMode,
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
    /// Clear the active connection's query history.
    ClearHistory,
    ToggleSidebar,
    ToggleColumnsPanel,
    /// Show/hide the Server panel: the server's live sessions, plus (on an engine
    /// that has them) its in-flight background mutations.
    ToggleServerPanel,
    RefreshSchema,
    Disconnect,
    /// Move keyboard focus to one of the surfaces the focus-target registry
    /// lists. Carries the target's identity rather than its position, so the
    /// command still names the right surface if the layout shifts between the
    /// palette opening and the command running.
    FocusTarget(crate::focus::FocusTargetId),
    /// Reveal the sidebar and focus its filter field (search schema).
    SearchSchema,
    /// Copy the result grid's current selection (TSV).
    CopySelection,
    /// Open a new-connection form (disconnected phase).
    NewConnection,
    /// Open the connection-import wizard (disconnected phase): pick a source
    /// (DBeaver/DBGate), scan, then choose which connections to import.
    ImportConnections,
    /// Open the "go to row…" prompt (only when a result is open).
    GoToRow,
    /// Open the keyboard-shortcuts reference overlay.
    ShowShortcuts,
    /// Save the active tab's query as a named snippet (opens the name prompt).
    SaveQuery,
    /// Open the saved-query picker.
    OpenSavedQueries,
    /// Open the saved query at this index (into a new tab); picker activation.
    OpenSavedQuery(usize),
    /// Open the "Copy to…" target picker for the current result.
    CopyToTable,
    /// Copy the current result into the candidate table at this index, the
    /// "Copy to…" target-picker activation.
    CopyTarget(usize),
    /// Copy the current result into a *new* table in the writable namespace at this
    /// index: the "✦ New table…" rows of the "Copy to…" picker. Opens a name prompt,
    /// then creates the table from the source's column shape before streaming.
    CopyNewTable(usize),
    /// Open the "Migrate schema to…" picker for the foreground connection's selected
    /// schema (all its tables → another database).
    MigrateSchema,
    /// Migrate the pending source schema into the target namespace at this index; the
    /// "Migrate to…" picker activation.
    MigrateTarget(usize),
    /// Open the "Compare table against…" picker: pick the left table (data-diff).
    CompareTable,
    /// A left table was picked at this index; open the picker for the right table.
    CompareLeft(usize),
    /// A right table was picked at this index; fire the diff (left is remembered).
    CompareRight(usize),
    /// Open the read-only schema ER diagram overlay.
    ErDiagram,
    /// Open the connection's health report.
    HealthReport,
    /// Open the schema-comparison picker: choose the namespace to compare the
    /// current one against.
    CompareSchema,
    /// Compare the current namespace against the namespace at this index; the
    /// schema-comparison picker's activation.
    CompareSchemaTarget(usize),
    /// EXPLAIN the active tab's query and open the plan view (B4).
    Explain,
    /// EXPLAIN ANALYZE the active tab's query (runs it; read queries only).
    ExplainAnalyze,
    /// Toggle watch mode on the active tab's result (re-run on an interval).
    ToggleWatch,
    /// Beautify the active editor's SQL in place.
    FormatSql,
    /// Submit the staged grid edits as one batch. Opens the confirm.
    SubmitChanges,
    /// Discard the staged grid edits.
    RevertChanges,
    /// Append a new draft (insert) row to the result.
    AddRow,
    /// Open the assistant's conversation-history picker.
    AssistantHistory,
    /// Start a fresh assistant chat, saving the current one.
    AssistantNewChat,
    /// Start a fresh assistant chat on a specific agent, by index into
    /// `usable_agents` (the "New chat with \<agent\>" entries).
    AssistantNewChatWith(usize),
    /// Reveal the conversations directory in the OS file manager.
    RevealConversationStorage,
    /// Open the connection's knowledge file (the agent's semantic layer) in the
    /// in-app markdown editor.
    EditKnowledge,
    /// Ask the agent to draft a knowledge file for this connection.
    LearnDatabase,
    /// Split the focused pane to the right (a new pane beside it).
    SplitRight,
    /// Split the focused pane downward (a new pane under it).
    SplitDown,
    /// Fold every pane back into a single one.
    Unsplit,
    /// Move focus to the next pane in visual order.
    FocusOtherHalf,
    /// Zoom the focused pane to fill the work area, or restore the layout.
    MaximizePane,
    /// Reset every pane divider to even shares.
    EqualizePanes,
    /// Open the "What's New" changelog overlay.
    ShowChangelog,
    // --- Redis (KV) commands, offered only on a Redis connection ---
    /// Open a new (blank) Redis tab, showing the panel chooser.
    KvNewTab,
    /// Toggle the browse list between the flat grid and the namespace tree.
    KvToggleTree,
    /// Run the biggest-keys sampler in the active Browse tab.
    KvFindBigKeys,
    /// Open a specific Redis panel in a new tab (Analysis, Console, …).
    KvOpenPanel(crate::kvbrowse::KvPanel),
}

/// Which free-text prompt the single palette slot is currently serving, so a
/// [`PaletteEvent::Submit`] routes to the right handler. Command-list palettes
/// (the default and the saved-query picker) ignore this; they emit `Activate`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptKind {
    GoToRow,
    SaveQuery,
    /// Naming the target of a "copy into a *new* table" (see [`Cmd::CopyNewTable`]).
    CopyNewTable,
}

/// A palette row with a translated label, keyed on the row's own stable id.
///
/// Every command already carries an id the palette uses for identity (`cmd:run`),
/// so the catalog key derives from it rather than being written out a second time
/// beside the text: there is no pair to keep in sync, and renaming a command
/// moves its id and its key together. `scripts/i18n-extract.py` reads these call
/// sites the same way it reads the settings and keymap tables.
///
/// Rows whose label interpolates data (`connect: {name}`) are not built here yet;
/// they need the placeholder story, which is a separate decision.
fn item(id: &'static str, en_label: &'static str) -> PaletteItem {
    PaletteItem::new(id, crate::i18n::tr_or(&item_key(id), en_label))
}

/// The catalog key for a palette row id: `cmd:kv-new-tab` becomes
/// `palette.cmd_kv_new_tab`. Mirrored by `slug()` in the extractor; the drift test
/// fails if the two ever disagree.
fn item_key(id: &str) -> String {
    let mut key = String::with_capacity(id.len() + 8);
    key.push_str("palette.");
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() {
            key.push(ch.to_ascii_lowercase());
        } else if !key.ends_with('_') {
            key.push('_');
        }
    }
    key
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

        let entries = self.palette_entries(cx);
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
    /// when no result is open, since there's nothing to navigate.
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
            // Prompt mode submits free text; route by which prompt is open.
            PaletteEvent::Submit(text) => {
                let kind = self.palette_prompt;
                self.close_palette();
                match kind {
                    PromptKind::GoToRow => self.submit_goto(text, cx),
                    PromptKind::SaveQuery => self.submit_save(text, cx),
                    PromptKind::CopyNewTable => self.submit_copy_new_table(text, cx),
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
            Cmd::OpenKeymapFile => self.open_keymap_file(cx),
            Cmd::RemoveAllData => self.open_reset_confirm(cx),
            Cmd::ToggleVimMode => self.toggle_vim_mode(cx),
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
            Cmd::ClearHistory => self.clear_history(cx),
            Cmd::ToggleSidebar => self.toggle_sidebar(cx),
            Cmd::ToggleColumnsPanel => self.toggle_columns_panel(cx),
            Cmd::ToggleServerPanel => self.toggle_server_panel(cx),
            Cmd::RefreshSchema => self.refresh_schema(cx),
            Cmd::Disconnect => self.disconnect(cx),
            // Pane focus needs a `Window`; defer it to the next render (drained
            // there) the same way the editor's Esc-to-grid does.
            // Deferred to the next render, like every other focus move made
            // without a `Window` in hand: the palette is closing this frame, and
            // focusing before it unmounts would just be undone by its teardown.
            Cmd::FocusTarget(id) => self.pending_focus_target = Some(id),
            // Deferred to the next render (needs a `Window`), like the focus jumps.
            Cmd::SearchSchema => self.focus_search = true,
            Cmd::CopySelection => self.copy_result_selection(cx),
            Cmd::NewConnection => self.open_new_form(cx),
            Cmd::ImportConnections => self.open_import_wizard(cx),
            Cmd::GoToRow => self.open_goto_prompt(cx),
            Cmd::ShowShortcuts => self.toggle_shortcuts(cx),
            Cmd::ShowChangelog => self.toggle_whats_new(cx),
            Cmd::SaveQuery => self.open_save_prompt(cx),
            Cmd::OpenSavedQueries => self.open_saved_picker(cx),
            Cmd::OpenSavedQuery(index) => self.open_saved_query(index, cx),
            Cmd::CopyToTable => self.open_copy_picker(cx),
            Cmd::CopyTarget(index) => self.pick_copy_target(index, cx),
            Cmd::CopyNewTable(index) => self.pick_copy_new_table(index, cx),
            Cmd::MigrateSchema => self.open_migrate_picker(cx),
            Cmd::MigrateTarget(index) => self.pick_migrate_target(index, cx),
            Cmd::CompareTable => self.open_compare_picker(cx),
            Cmd::CompareLeft(index) => self.pick_compare_left(index, cx),
            Cmd::CompareRight(index) => self.pick_compare_right(index, cx),
            Cmd::ErDiagram => self.open_er_diagram(self.er_target_namespace(cx), cx),
            Cmd::HealthReport => self.open_health_report(cx),
            Cmd::CompareSchema => self.open_schema_compare_picker(cx),
            Cmd::CompareSchemaTarget(index) => self.pick_schema_compare_target(index, cx),
            Cmd::Explain => self.explain_query(false, cx),
            Cmd::ExplainAnalyze => self.explain_query(true, cx),
            Cmd::ToggleWatch => self.toggle_watch(cx),
            Cmd::FormatSql => self.format_active_sql(cx),
            Cmd::SubmitChanges => self.submit_changes(cx),
            Cmd::RevertChanges => self.revert_changes(cx),
            Cmd::AddRow => self.add_draft_row(cx),
            Cmd::AssistantHistory => self.open_history_sidebar(cx),
            Cmd::AssistantNewChat => self.new_chat(cx),
            Cmd::AssistantNewChatWith(index) => {
                if let Some(agent) = self.usable_agents.get(index) {
                    let id = agent.id.clone();
                    self.new_chat_with(id, cx);
                }
            }
            Cmd::RevealConversationStorage => self.reveal_conversation_storage(cx),
            Cmd::EditKnowledge => self.open_knowledge_editor(cx),
            Cmd::LearnDatabase => self.learn_this_database(cx),
            Cmd::SplitRight => self.split_right(cx),
            Cmd::SplitDown => self.split_down(cx),
            Cmd::Unsplit => self.unsplit(cx),
            Cmd::MaximizePane => self.zoom_pane(cx),
            Cmd::EqualizePanes => self.equalize_panes(cx),
            Cmd::FocusOtherHalf => self.focus_other_half(cx),
            Cmd::KvNewTab => {
                if let Some(s) = self.kv_active_session() {
                    self.kv_new_empty_tab(s, cx);
                }
            }
            Cmd::KvToggleTree => {
                if let Some(s) = self.kv_active_session() {
                    self.kv_toggle_tree_mode(s, cx);
                }
            }
            Cmd::KvFindBigKeys => {
                if let Some(s) = self.kv_active_session() {
                    self.kv_start_big_keys_sample(s, cx);
                }
            }
            Cmd::KvOpenPanel(panel) => {
                if let Some(s) = self.kv_active_session() {
                    self.kv_open_panel(s, panel, cx);
                }
            }
        }
    }

    /// Which contract the **active result's** existing rows can be edited under
    ///. Three gates in one, all of which must agree: the connection is
    /// writable, the engine has *some* edit contract, and the table this result
    /// browses reported one (see [`red_core::RowEditCaps`]).
    ///
    /// [`EditMode::None`](red_core::EditMode::None) means no update/delete affordances at all -- read-only is
    /// the safe default at every level.
    pub(crate) fn row_edit_mode(&self) -> red_core::EditMode {
        let Phase::Connected(active) = &self.phase else {
            return red_core::EditMode::None;
        };
        let caps = active.config.kind.write_caps();
        if active.config.read_only || !(caps.guarded_edit || caps.best_effort_edit) {
            return red_core::EditMode::None;
        }
        active
            .active_result()
            .map(|g| g.edit_mode())
            .unwrap_or(red_core::EditMode::None)
    }

    /// Whether the active result's existing rows can be edited at all.
    pub(crate) fn row_edit_enabled(&self) -> bool {
        !matches!(self.row_edit_mode(), red_core::EditMode::None)
    }

    /// Whether in-grid **inserting** (the draft-row zone, "+ Row", file import) is
    /// enabled for the active connection: a writable connection whose engine accepts
    /// a bulk `INSERT`. Deliberately a separate gate from
    /// [`row_edit_enabled`](Self::row_edit_enabled): an insert needs none of the
    /// row-identity or rollback guarantees an update or a delete does, so ClickHouse
    /// -- which can be an insert target but has no guarded edit -- passes this one
    /// and fails that one.
    pub(crate) fn insert_enabled(&self) -> bool {
        matches!(
            &self.phase,
            Phase::Connected(active)
                if !active.config.read_only && active.config.kind.write_caps().insert
        )
    }

    /// The focused result cell's edit target, when editing is enabled and the cell
    /// is editable (a single-table keyed browse, non-PK, non-clipped). `None`
    /// otherwise; the entry point and palette gate both consult this.
    pub(crate) fn active_edit_target(&self) -> Option<crate::app::EditContext> {
        if !self.row_edit_enabled() {
            return None;
        }
        let gutter = self.gutter();
        match &self.phase {
            Phase::Connected(active) => active.active_result().and_then(|g| g.edit_target(gutter)),
            _ => None,
        }
    }

    /// The commands available in the current phase, each paired with its `Cmd`.
    /// Phase-specific actions come first (what the user most likely wants), then
    /// the always-available ones.
    fn palette_entries(&self, cx: &gpui::App) -> Vec<(PaletteItem, Cmd)> {
        let mut out: Vec<(PaletteItem, Cmd)> = Vec::new();

        match &self.phase {
            // A Redis connection has its own workspace (panels/tabs, no SQL
            // editor), so it gets its own command set instead of the query ones.
            Phase::Connected(active) if active.kv_view.is_some() => {
                use crate::kvbrowse::KvPanel;
                out.push((
                    item("cmd:kv-new-tab", "redis: new tab").hint("⌘T"),
                    Cmd::KvNewTab,
                ));
                out.push((
                    item("cmd:kv-tree", "redis: toggle namespace tree"),
                    Cmd::KvToggleTree,
                ));
                out.push((
                    item("cmd:kv-bigkeys", "redis: find biggest keys"),
                    Cmd::KvFindBigKeys,
                ));
                out.push((
                    item("cmd:kv-analyze", "redis: analyze keyspace"),
                    Cmd::KvOpenPanel(KvPanel::Analysis),
                ));
                out.push((
                    item("cmd:kv-console", "redis: open console"),
                    Cmd::KvOpenPanel(KvPanel::Console),
                ));
                out.push((
                    item("cmd:kv-monitor", "redis: open monitor (slow log · clients)"),
                    Cmd::KvOpenPanel(KvPanel::Monitor),
                ));
                out.push((
                    item("cmd:kv-keyspace", "redis: watch keyspace notifications"),
                    Cmd::KvOpenPanel(KvPanel::Keyspace),
                ));
                out.push((
                    item("cmd:kv-pubsub", "redis: open pub/sub"),
                    Cmd::KvOpenPanel(KvPanel::PubSub),
                ));
                // Connection switching, settings, shortcuts, etc. come from the
                // shared tail appended after this match.
            }
            Phase::Connected(active) => {
                out.push((item("cmd:run", "query: run").hint("⌘↵"), Cmd::RunQuery));
                out.push((
                    item("cmd:new-tab", "query: new tab").hint("⌘T"),
                    Cmd::NewTab,
                ));
                // Tab management: close needs an open tab; switching needs two.
                if active.active().is_some() {
                    out.push((
                        item("cmd:close-tab", "query: close tab").hint("⌘W"),
                        Cmd::CloseTab,
                    ));
                }
                if active.tabs.len() > 1 {
                    out.push((
                        item("cmd:next-tab", "query: next tab").hint("⌃Tab"),
                        Cmd::NextTab,
                    ));
                    out.push((
                        item("cmd:prev-tab", "query: previous tab").hint("⌃⇧Tab"),
                        Cmd::PrevTab,
                    ));
                }
                // Panes: splitting is always on offer; the rest only once the
                // work area is actually divided.
                out.push((
                    item("cmd:split-right", "view: split pane right").hint("⌘\\"),
                    Cmd::SplitRight,
                ));
                out.push((
                    item("cmd:split-down", "view: split pane down").hint("⌘⇧\\"),
                    Cmd::SplitDown,
                ));
                if active.layout.is_split() {
                    out.push((
                        item("cmd:focus-other-half", "view: focus next pane").hint("⌥⌘\\"),
                        Cmd::FocusOtherHalf,
                    ));
                    out.push((
                        item("cmd:maximize-pane", "view: maximize / restore pane").hint("⌘⇧↩"),
                        Cmd::MaximizePane,
                    ));
                    out.push((
                        item("cmd:equalize-panes", "view: equalize pane sizes").hint("⌥⌘0"),
                        Cmd::EqualizePanes,
                    ));
                    out.push((
                        item("cmd:unsplit", "view: unsplit (fold panes back into one)"),
                        Cmd::Unsplit,
                    ));
                }
                // Whole-schema migration, offered only when the selected/only schema
                // has tables to move (the handler picks the target database).
                if self.migrate_source(cx).is_some() {
                    out.push((
                        item("cmd:migrate-schema", "schema: migrate to…"),
                        Cmd::MigrateSchema,
                    ));
                }
                // Data-compare (table diff), offered when the connection has at least
                // two tables to compare (the handler picks left then right).
                if self.compare_candidates(cx).len() >= 2 {
                    out.push((
                        item("cmd:compare-table", "table: compare against…"),
                        Cmd::CompareTable,
                    ));
                }
                // Only meaningful with rows on screen to navigate / copy.
                if active.active_result().is_some() {
                    out.push((item("cmd:goto-row", "go to row…").hint("⌃G"), Cmd::GoToRow));
                    out.push((
                        item("cmd:copy", "result: copy selection").hint("⌘C"),
                        Cmd::CopySelection,
                    ));
                    out.push((
                        item("cmd:copy-to-table", "result: copy to table…"),
                        Cmd::CopyToTable,
                    ));
                }
                // Staged data editing (B6), offered on a writable connection browsing
                // a single-table result. "Add row" rides the *insert* gate, so it is
                // offered on an engine that accepts inserts without supporting guarded
                // row editing (ClickHouse); submit/revert follow whatever is staged.
                if self.insert_enabled()
                    && active
                        .active_result()
                        .is_some_and(|g| g.insertable_browse())
                {
                    out.push((
                        item("cmd:add-row", "data: add row").hint("⌥⌘N"),
                        Cmd::AddRow,
                    ));
                }
                if self.has_pending_changes() {
                    out.push((
                        item("cmd:submit-changes", "data: submit changes").hint("⌘↵"),
                        Cmd::SubmitChanges,
                    ));
                    out.push((
                        item("cmd:revert-changes", "data: revert changes").hint("⌥⌘Z"),
                        Cmd::RevertChanges,
                    ));
                }
                out.push((
                    item("cmd:history", "query: toggle history"),
                    Cmd::ToggleHistory,
                ));
                out.push((
                    item("cmd:clear-history", "query: clear history"),
                    Cmd::ClearHistory,
                ));
                // Saved queries (B3): save needs an open tab to save *from*; the
                // picker is always offered (it reports "none yet" when empty).
                if active.active().is_some() {
                    out.push((
                        item("cmd:save-query", "query: save…").hint("⇧⌘S"),
                        Cmd::SaveQuery,
                    ));
                }
                out.push((
                    item("cmd:open-saved", "query: open saved…").hint("⇧⌘O"),
                    Cmd::OpenSavedQueries,
                ));
                // EXPLAIN (B4): explain needs a query to explain; analyze runs the
                // statement and is offered only on engines that support it (not
                // SQLite, which has no ANALYZE).
                if active.active().is_some() {
                    out.push((
                        item("cmd:explain", "query: explain plan").hint("⇧⌘E"),
                        Cmd::Explain,
                    ));
                    if active.config.kind != red_core::DbKind::Sqlite {
                        out.push((
                            item("cmd:explain-analyze", "query: explain analyze"),
                            Cmd::ExplainAnalyze,
                        ));
                    }
                    // Watch: offered only where it could act (a result is open),
                    // so the palette never lists a command that would just toast.
                    if active.active().is_some_and(|t| t.result.is_some()) {
                        let on = active.active().is_some_and(|t| t.watch.is_some());
                        let label = if on {
                            "result: stop watching"
                        } else {
                            "result: watch (re-run on an interval)"
                        };
                        out.push((PaletteItem::new("cmd:watch", label), Cmd::ToggleWatch));
                    }
                    out.push((
                        item("cmd:format-sql", "editor: format SQL").hint("⌥⌘F"),
                        Cmd::FormatSql,
                    ));
                }
                // Focus: one entry per live surface, generated from the registry
                // so every seam offers its own (Redis lists its key lists, Mongo
                // its collection tree) with no per-shell branch here. The hint is
                // the digit the hold-to-reveal overlay paints on that surface,
                // which is positional — so the palette and the overlay always
                // agree on what "3" means.
                let alphabet =
                    crate::focus::hint_alphabet(self.settings.keymap.focus_overlay_hints);
                for (i, target) in self.focus_targets(cx).into_iter().enumerate() {
                    let label = format!("focus: {}", target.label);
                    let mut entry = PaletteItem::new(
                        SharedString::from(format!("cmd:focus-{i}")),
                        SharedString::from(label),
                    );
                    if let Some(hint) = alphabet.get(i) {
                        entry = entry.hint(SharedString::from(hint.to_uppercase().to_string()));
                    }
                    out.push((entry, Cmd::FocusTarget(target.id)));
                }
                out.push((
                    item("cmd:search-schema", "schema: search").hint("⌘F"),
                    Cmd::SearchSchema,
                ));
                out.push((
                    item("cmd:sidebar", "view: toggle sidebar").hint("⌘B"),
                    Cmd::ToggleSidebar,
                ));
                out.push((
                    item("cmd:columns", "view: toggle columns panel").hint("⇧⌘C"),
                    Cmd::ToggleColumnsPanel,
                ));
                // Only where there is a server behind the connection: SQLite is a
                // file, with no other sessions and no background work to watch.
                out.push((
                    item("cmd:health", "connection: health report"),
                    Cmd::HealthReport,
                ));
                // Per-connection, not per-panel: the file is worth editing whether
                // or not the assistant is open right now.
                out.push((
                    item("cmd:knowledge", "connection: database knowledge…"),
                    Cmd::EditKnowledge,
                ));
                if active.schema.read(cx).schemas.len() > 1 {
                    out.push((
                        item("cmd:compare-schema", "schema: compare against…"),
                        Cmd::CompareSchema,
                    ));
                }
                out.push((
                    item("cmd:refresh", "schema: refresh").hint("⌘R"),
                    Cmd::RefreshSchema,
                ));
                out.push((item("cmd:er-diagram", "schema: ER diagram"), Cmd::ErDiagram));
                out.push((
                    item("cmd:disconnect", "connection: disconnect"),
                    Cmd::Disconnect,
                ));
                // Assistant conversation history, only with the panel open.
                if self.assistant.is_some() {
                    out.push((
                        item("cmd:ai-new-chat", "agent: new chat"),
                        Cmd::AssistantNewChat,
                    ));
                    // With more than one agent configured, offer a direct
                    // "new chat with <agent>" so you can pick without opening the
                    // composer's agent dropdown.
                    if self.usable_agents.len() > 1 {
                        for (i, agent) in self.usable_agents.iter().enumerate() {
                            let id =
                                ElementId::from(SharedString::from(format!("cmd:ai-new-chat:{i}")));
                            out.push((
                                PaletteItem::new(
                                    id,
                                    format!("agent: new chat with {}", agent.name),
                                ),
                                Cmd::AssistantNewChatWith(i),
                            ));
                        }
                    }
                    out.push((
                        item("cmd:ai-history", "agent: conversation history…"),
                        Cmd::AssistantHistory,
                    ));
                    // Withheld below `read` tier: with schema-only tools the agent
                    // can't sample a value, so the "glossary" would be column-name
                    // inference - the failure mode the knowledge file exists to fix.
                    if self.can_learn_database() {
                        out.push((
                            item("cmd:ai-learn", "agent: learn this database"),
                            Cmd::LearnDatabase,
                        ));
                    }
                    out.push((
                        item("cmd:ai-storage", "agent: open conversation storage"),
                        Cmd::RevealConversationStorage,
                    ));
                }
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
                    item("cmd:new-conn", "connection: new").hint("⌘N"),
                    Cmd::NewConnection,
                ));
                out.push((
                    PaletteItem::new(
                        "cmd:import-conns",
                        "connection: import from other database tools…",
                    ),
                    Cmd::ImportConnections,
                ));
            }
            // Mid-connect there's nothing query-shaped to do; only globals show.
            Phase::Connecting(_) => {}
        }

        // In the shared tail rather than a per-engine branch: the Server panel is
        // one dock over all three seams, and its own gate already answers for the
        // engines that have no server (SQLite is a file).
        if self.has_server_panel() {
            out.push((
                item("cmd:server-panel", "view: toggle server panel"),
                Cmd::ToggleServerPanel,
            ));
        }
        out.push((
            item("cmd:switch-conn", "connection: switch…").hint("⌘P"),
            Cmd::SwitchConnection,
        ));
        out.push((
            item("cmd:shortcuts", "view: keyboard shortcuts").hint("⌘/"),
            Cmd::ShowShortcuts,
        ));
        out.push((
            item("cmd:whats-new", "help: what's new"),
            Cmd::ShowChangelog,
        ));
        out.push((
            item("cmd:settings", "view: settings").hint("⌘,"),
            Cmd::OpenSettings,
        ));
        out.push((
            item("cmd:settings-file", "settings: open file"),
            Cmd::OpenSettingsFile,
        ));
        out.push((
            item("cmd:settings-default", "settings: open default settings"),
            Cmd::OpenDefaultSettings,
        ));
        out.push((
            item("cmd:keymap-file", "keymap: customize keybindings"),
            Cmd::OpenKeymapFile,
        ));
        out.push((
            PaletteItem::new(
                "cmd:vim-mode",
                if self.settings.keymap.vim_mode {
                    "keymap: turn off vim navigation"
                } else {
                    "keymap: turn on vim navigation"
                },
            ),
            Cmd::ToggleVimMode,
        ));
        out.push((
            item("cmd:remove-all-data", "danger: remove all RED data…"),
            Cmd::RemoveAllData,
        ));

        // The hints above are written as macOS glyphs; localize them to the host
        // platform (a no-op on macOS) so Windows/Linux show `Ctrl+…`, matching the
        // keys that actually fire. Harmless on non-shortcut hints (left untouched).
        for (item, _) in &mut out {
            if let Some(hint) = item.hint.take() {
                item.hint = Some(crate::keymap::localize_hint(&hint).into());
            }
        }
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
                "Nothing to save: the editor is empty.",
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
        match red_config::queries::save(&name, None, &sql) {
            Ok(_) => {
                self.notify(ToastVariant::Success, format!("Saved query “{name}”."), cx);
            }
            Err(e) => {
                self.notify(ToastVariant::Error, format!("Couldn't save query: {e}"), cx);
            }
        }
    }

    /// ⇧⌘O / "query: open saved…": load the saved-query files and open a picker
    /// over them. Enumerating happens here, on demand (never at startup), so saved
    /// queries cost nothing at idle and external edits show up on each open.
    pub(crate) fn open_saved_picker(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Connected(_)) {
            return;
        }
        let queries = red_config::queries::load();
        if queries.is_empty() {
            self.notify(
                ToastVariant::Info,
                "No saved queries yet. Save one with ⇧⌘S.",
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
        let tab = crate::app::QueryTab::new(query.name, self.active_dialect(), cx);
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

    /// "Copy to…" (the result toolbar): open a picker over every writable table in
    /// every open connection (the foreground + parked live sessions), so the user
    /// names a target for the copy. The source is *implicit*: the focused result
    /// (filter included). No-op (with a hint) when nothing's open to copy from / into.
    pub(crate) fn open_copy_picker(&mut self, cx: &mut Context<Self>) {
        // Source must be an open result (the thing you're looking at).
        let has_source = matches!(
            &self.phase,
            Phase::Connected(active) if active.active_result().is_some()
        );
        if !has_source {
            self.notify(ToastVariant::Info, "Open a result to copy from", cx);
            return;
        }
        let candidates = self.copy_target_candidates(cx);
        let namespaces = self.copy_namespace_candidates(cx);
        if candidates.is_empty() && namespaces.is_empty() {
            self.notify(
                ToastVariant::Info,
                "No writable connection to copy into; open one first",
                cx,
            );
            return;
        }
        let mut entries: Vec<(PaletteItem, Cmd)> = Vec::new();
        // "✦ New table…" rows first: create a fresh table in any writable namespace
        // (same connection's other schema/database, or another open connection).
        for (i, ns) in namespaces.iter().enumerate() {
            let id = ElementId::from(SharedString::from(format!("copy-new:{i}")));
            let item = PaletteItem::new(id, format!("✦ New table in {}…", ns.schema))
                .hint(ns.conn_name.clone());
            entries.push((item, Cmd::CopyNewTable(i)));
        }
        // …then every existing writable table (copy into it, mapped by name).
        for (i, c) in candidates.iter().enumerate() {
            let id = ElementId::from(SharedString::from(format!("copy-target:{i}")));
            let item = PaletteItem::new(id, format!("{}.{}", c.schema, c.table.name))
                .hint(c.conn_name.clone());
            entries.push((item, Cmd::CopyTarget(i)));
        }
        self.copy_targets = candidates;
        self.copy_new_namespaces = namespaces;
        self.palette_cmds = entries
            .iter()
            .map(|(item, cmd)| (item.id.clone(), *cmd))
            .collect();
        let items: Vec<PaletteItem> = entries.into_iter().map(|(item, _)| item).collect();

        let palette = cx.new(|cx| {
            let mut p = Palette::new(cx);
            p.set_placeholder("Copy into table…", cx);
            p.set_items(items, cx);
            p
        });
        let sub = cx.subscribe(&palette, Self::on_palette_event);
        self.palette = Some((palette, sub));
        cx.notify();
    }

    /// A target table was picked: stash the source (the focused result's epoch +
    /// columns) and target, then peek the target's columns so the copy can be mapped
    /// by name and confirmed before any write (mirrors the import file-header peek).
    fn pick_copy_target(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(candidate) = self.copy_targets.get(index).cloned() else {
            return;
        };
        let source = match &self.phase {
            Phase::Connected(active) => active
                .active_result()
                .map(|g| (g.epoch, g.columns().to_vec())),
            _ => None,
        };
        let Some((source_epoch, source_cols)) = source else {
            self.notify(
                ToastVariant::Error,
                "The source result is no longer open",
                cx,
            );
            return;
        };
        let id = red_service::OpId::new(self.next_export_id);
        self.next_export_id += 1;
        let target_label = format!(
            "{} · {}.{}",
            candidate.conn_name, candidate.schema, candidate.table.name
        );
        self.pending_copy_target = Some(PendingCopyPeek {
            id,
            source_epoch,
            source_cols,
            target: candidate.table.clone(),
            target_session: candidate.session,
            target_label,
        });
        self.service.send_to(
            candidate.session,
            Command::CopyTargetColumns {
                id,
                target: candidate.table,
            },
        );
        cx.notify();
    }

    /// A "✦ New table…" namespace was picked: stash the source (the focused result's
    /// epoch + columns) and the target namespace, then open a prompt for the new
    /// table's name. On submit, `submit_copy_new_table` creates the table from the
    /// source's column shape and streams the rows in.
    fn pick_copy_new_table(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(ns) = self.copy_new_namespaces.get(index).cloned() else {
            return;
        };
        let source = match &self.phase {
            Phase::Connected(active) => active
                .active_result()
                .map(|g| (g.epoch, g.columns().to_vec())),
            _ => None,
        };
        let Some((source_epoch, source_cols)) = source else {
            self.notify(
                ToastVariant::Error,
                "The source result is no longer open",
                cx,
            );
            return;
        };
        let placeholder = format!("New table name in {} · {}", ns.conn_name, ns.schema);
        self.pending_copy_new = Some(PendingCopyNewTable {
            source_epoch,
            source_cols,
            session: ns.session,
            conn_name: ns.conn_name,
            schema: ns.schema,
        });
        let prompt = cx.new(|cx| {
            let mut p = Palette::new(cx).prompt();
            p.set_placeholder(placeholder, cx);
            p
        });
        let sub = cx.subscribe(&prompt, Self::on_palette_event);
        self.palette = Some((prompt, sub));
        self.palette_cmds.clear();
        self.palette_prompt = PromptKind::CopyNewTable;
        cx.notify();
    }

    /// Submit of the new-table-name prompt: validate the name, guard against a name
    /// collision (so the create path never silently appends into an existing,
    /// possibly mismatched table), build a `create_table` spec + an identity column
    /// mapping from the source's columns, and fire the streamed create-then-copy.
    /// Creating a brand-new table destroys nothing, so this skips the destructive
    /// copy confirm and goes straight to the transfer toast.
    fn submit_copy_new_table(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_copy_new.take() else {
            return;
        };
        let name = text.trim();
        if name.is_empty() {
            self.notify(ToastVariant::Error, "Enter a name for the new table", cx);
            return;
        }
        if self.namespace_has_table(pending.session, &pending.schema, name, cx) {
            self.notify(
                ToastVariant::Error,
                format!(
                    "“{name}” already exists in {} · {}; use Copy to… to copy into it",
                    pending.conn_name, pending.schema
                ),
                cx,
            );
            return;
        }
        if pending.source_cols.is_empty() {
            self.notify(
                ToastVariant::Error,
                "The source result has no columns to copy",
                cx,
            );
            return;
        }
        // Identity mapping + a create spec from the source columns. A result carries no
        // PK / not-null / default, so the new table's columns are plain and nullable;
        // their declared types are mapped into the target dialect by `create_table`.
        let mapping: Vec<ColumnMap> = pending
            .source_cols
            .iter()
            .enumerate()
            .map(|(i, c)| ColumnMap {
                source: i,
                column: c.name.clone(),
                decl_type: c.decl_type.clone(),
            })
            .collect();
        let create: Vec<ColumnMeta> = pending
            .source_cols
            .iter()
            .map(|c| ColumnMeta {
                name: c.name.clone(),
                type_name: c.decl_type.clone(),
                not_null: false,
                primary_key: false,
                default: None,
                auto_increment: false,
            })
            .collect();
        let target = TableRef {
            schema: Some(pending.schema.clone()),
            name: name.to_string(),
        };
        let id = red_service::OpId::new(self.next_export_id);
        self.next_export_id += 1;
        self.start_copy(
            id,
            pending.source_epoch,
            target,
            pending.session,
            mapping,
            CopyMode::Append,
            Some(create),
            cx,
        );
    }

    /// "schema: migrate to…": take the foreground connection's selected schema (all its
    /// tables) and open a picker over every *other* writable namespace (a target
    /// database). On pick, `pick_migrate_target` fires the whole-schema migration.
    /// No-op (with a hint) when nothing is migratable / no target is open.
    pub(crate) fn open_migrate_picker(&mut self, cx: &mut Context<Self>) {
        let Some((session, schema, tables)) = self.migrate_source(cx) else {
            self.notify(
                ToastVariant::Info,
                "Select a schema with tables to migrate",
                cx,
            );
            return;
        };
        // Targets: every writable namespace except the source schema itself.
        let targets: Vec<_> = self
            .copy_namespace_candidates(cx)
            .into_iter()
            .filter(|ns| !(ns.session == session && ns.schema == schema))
            .collect();
        if targets.is_empty() {
            self.notify(
                ToastVariant::Info,
                "No other writable database to migrate into. Open one first",
                cx,
            );
            return;
        }
        let table_count = tables.len();
        let entries: Vec<(PaletteItem, Cmd)> = targets
            .iter()
            .enumerate()
            .map(|(i, ns)| {
                let id = ElementId::from(SharedString::from(format!("migrate-target:{i}")));
                let item = PaletteItem::new(
                    id,
                    crate::i18n::tr!(
                        "palette.migrate_target",
                        "{schema} ({table_count} table(s))",
                        schema = ns.schema,
                        table_count = table_count
                    ),
                )
                .hint(ns.conn_name.clone());
                (item, Cmd::MigrateTarget(i))
            })
            .collect();
        self.pending_migrate = Some((session, schema, tables));
        self.migrate_targets = targets;
        self.palette_cmds = entries
            .iter()
            .map(|(item, cmd)| (item.id.clone(), *cmd))
            .collect();
        let items: Vec<PaletteItem> = entries.into_iter().map(|(item, _)| item).collect();
        let palette = cx.new(|cx| {
            let mut p = Palette::new(cx);
            p.set_placeholder("Migrate schema into database…", cx);
            p.set_items(items, cx);
            p
        });
        let sub = cx.subscribe(&palette, Self::on_palette_event);
        self.palette = Some((palette, sub));
        cx.notify();
    }

    /// A target namespace was picked for a migrate: fire the whole-schema migration
    /// (the source is the foreground connection's chosen schema, stashed in
    /// `pending_migrate`).
    fn pick_migrate_target(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(target) = self.migrate_targets.get(index).cloned() else {
            return;
        };
        // The source is the foreground session (`start_migrate` uses `send_active`).
        let Some((_source_session, source_schema, tables)) = self.pending_migrate.take() else {
            return;
        };
        let id = red_service::OpId::new(self.next_export_id);
        self.next_export_id += 1;
        self.start_migrate(id, source_schema, tables, target.session, target.schema, cx);
    }

    /// The foreground connection's tables (`(session, schema, name)`): the pool the
    /// "Compare table against…" picker draws both sides from. Same-connection only
    /// for the shipped scope (D0–D2); cross-connection diff is a later phase.
    pub(crate) fn compare_candidates(&self, cx: &App) -> Vec<(SessionId, Option<String>, String)> {
        let Phase::Connected(active) = &self.phase else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for ns in &active.schema.read(cx).schemas {
            for o in &ns.objects {
                // Any relation can be browsed, including a view or a matview;
                // only the columnless programmatic kinds are excluded.
                if o.kind.is_relation() {
                    let schema = (!ns.name.is_empty()).then(|| ns.name.clone());
                    out.push((active.session, schema, o.name.clone()));
                }
            }
        }
        out
    }

    /// "schema: compare against…": pick which namespace to compare the tree's
    /// current namespace against.
    ///
    /// Scoped to one connection for now. The backend command already takes a
    /// `right_session`, so a cross-connection picker is a UI addition rather than
    /// a protocol change, but same-connection is the case worth having first
    /// (comparing `staging` against `public` on one server).
    pub(crate) fn open_schema_compare_picker(&mut self, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let Some(current) = self.er_target_namespace(cx) else {
            self.notify(ToastVariant::Info, "Select a schema in the tree first", cx);
            return;
        };
        let others: Vec<String> = active
            .schema
            .read(cx)
            .schemas
            .iter()
            .map(|s| s.name.clone())
            .filter(|n| *n != current)
            .collect();
        if others.is_empty() {
            self.notify(
                ToastVariant::Info,
                "This connection has only one schema to compare",
                cx,
            );
            return;
        }
        let entries: Vec<(PaletteItem, Cmd)> = others
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let id = ElementId::from(SharedString::from(format!("compare-schema:{i}")));
                (
                    PaletteItem::new(id, format!("against {name}")),
                    Cmd::CompareSchemaTarget(i),
                )
            })
            .collect();
        self.compare_schemas = others;
        self.open_command_picker(&format!("Compare {current} against…"), entries, cx);
    }

    /// Fire the comparison against the picked namespace.
    fn pick_schema_compare_target(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(right) = self.compare_schemas.get(index).cloned() else {
            return;
        };
        let Some(left) = self.er_target_namespace(cx) else {
            return;
        };
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let session = active.session;
        self.close_palette();
        self.next_export_id += 1;
        self.send_active(Command::DiffSchemas {
            id: red_service::OpId::new(self.next_export_id),
            left_namespace: left,
            right_session: session,
            right_namespace: right,
        });
        self.notify(ToastVariant::Info, "Comparing schemas…", cx);
        cx.notify();
    }

    /// "table: compare against…": open a picker over the connection's tables to
    /// choose the **left** side of a data-diff.
    pub(crate) fn open_compare_picker(&mut self, cx: &mut Context<Self>) {
        let tables = self.compare_candidates(cx);
        if tables.len() < 2 {
            self.notify(
                ToastVariant::Info,
                "Need at least two tables in this connection to compare",
                cx,
            );
            return;
        }
        let entries: Vec<(PaletteItem, Cmd)> = tables
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let id = ElementId::from(SharedString::from(format!("compare-left:{i}")));
                (
                    PaletteItem::new(id, format!("compare {}", compare_label(t))),
                    Cmd::CompareLeft(i),
                )
            })
            .collect();
        self.compare_tables = tables;
        self.compare_left = None;
        self.open_command_picker("Compare which table?", entries, cx);
    }

    /// The left table was picked: open a picker over the *other* tables for the right
    /// side.
    fn pick_compare_left(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(left) = self.compare_tables.get(index).cloned() else {
            return;
        };
        self.compare_left = Some(index);
        let placeholder = format!("Compare {} against…", compare_label(&left));
        let entries: Vec<(PaletteItem, Cmd)> = self
            .compare_tables
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != index)
            .map(|(j, t)| {
                let id = ElementId::from(SharedString::from(format!("compare-right:{j}")));
                (
                    PaletteItem::new(id, format!("against {}", compare_label(t))),
                    Cmd::CompareRight(j),
                )
            })
            .collect();
        self.open_command_picker(&placeholder, entries, cx);
    }

    /// The right table was picked: fire the diff (the backend aligns on the left
    /// table's primary key).
    fn pick_compare_right(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(left_idx) = self.compare_left.take() else {
            return;
        };
        let (Some(left), Some(right)) = (
            self.compare_tables.get(left_idx).cloned(),
            self.compare_tables.get(index).cloned(),
        ) else {
            return;
        };
        self.compare_tables = Vec::new();
        let (session, l_schema, l_name) = left;
        let (_, r_schema, r_name) = right;
        self.start_diff(
            session,
            TableRef {
                schema: l_schema,
                name: l_name,
            },
            session,
            TableRef {
                schema: r_schema,
                name: r_name,
            },
            cx,
        );
    }

    /// Build and open a command-picker palette from `entries` (id → `Cmd`), the shape
    /// the migrate/compare pickers share.
    fn open_command_picker(
        &mut self,
        placeholder: &str,
        entries: Vec<(PaletteItem, Cmd)>,
        cx: &mut Context<Self>,
    ) {
        self.palette_cmds = entries
            .iter()
            .map(|(item, cmd)| (item.id.clone(), *cmd))
            .collect();
        let items: Vec<PaletteItem> = entries.into_iter().map(|(item, _)| item).collect();
        let placeholder = placeholder.to_string();
        let palette = cx.new(|cx| {
            let mut p = Palette::new(cx);
            p.set_placeholder(&placeholder, cx);
            p.set_items(items, cx);
            p
        });
        let sub = cx.subscribe(&palette, Self::on_palette_event);
        self.palette = Some((palette, sub));
        cx.notify();
    }
}

/// Display label for a compare candidate (`schema.name`, or bare `name`).
fn compare_label(t: &(SessionId, Option<String>, String)) -> String {
    match &t.1 {
        Some(s) => format!("{s}.{}", t.2),
        None => t.2.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The English catalog has to reproduce every palette label exactly.
    ///
    /// `assets/i18n/palette/en.ftl` is generated from these call sites, so the
    /// two drift the moment someone edits a label without re-running the
    /// extractor. This also pins [`item_key`] against the extractor's `slug()`:
    /// if the two ever derive a different key from the same id, the lookup misses
    /// and the assertion below reports the key echoing itself.
    ///
    /// Reads the source rather than a table, because the rows are built inside a
    /// long `match` on [`Phase`] and no single call enumerates them all.
    #[test]
    fn every_palette_label_is_in_the_english_catalog() {
        crate::i18n::apply(crate::i18n::DEFAULT);

        let src = include_str!("palette.rs");
        let mut checked = 0;
        let mut stale = Vec::new();

        for (idx, _) in src.match_indices("item(\"") {
            let rest = &src[idx + "item(\"".len()..];
            let Some((id, rest)) = rest.split_once('"') else {
                continue;
            };
            // Only the two-literal form; a row with an interpolated label is not
            // built through `item` and has nothing to check here.
            let Some(rest) = rest.trim_start().strip_prefix(",") else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some((en_label, _)) = rest.split_once('"') else {
                continue;
            };

            checked += 1;
            let key = item_key(id);
            let got = crate::i18n::lookup(&key);
            if got.as_ref() != en_label {
                stale.push(format!(
                    "  {key}\n    catalog: {got}\n    code:    {en_label}"
                ));
            }
        }

        assert!(
            checked > 40,
            "only found {checked} palette rows; the `item(..)` shape changed and \
             this test stopped covering anything"
        );
        assert!(
            stale.is_empty(),
            "assets/i18n/palette/en.ftl is out of date with palette.rs:\n{}\n\n\
             Re-run: python3 scripts/i18n-extract.py",
            stale.join("\n")
        );
    }

    #[test]
    fn item_key_slugs_the_command_id() {
        assert_eq!(item_key("cmd:kv-new-tab"), "palette.cmd_kv_new_tab");
        assert_eq!(item_key("cmd:run"), "palette.cmd_run");
    }
}
