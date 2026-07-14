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
use gpui::{actions, prelude::*, Context, ElementId, Entity, SharedString};
use red_core::{ColumnMap, ColumnMeta, CopyMode, TableRef};
use red_service::Command;

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
    /// Open the read-only schema ER diagram overlay.
    ErDiagram,
    /// EXPLAIN the active tab's query and open the plan view (B4).
    Explain,
    /// EXPLAIN ANALYZE the active tab's query (runs it; read queries only).
    ExplainAnalyze,
    /// Beautify the active editor's SQL in place.
    FormatSql,
    /// Submit the staged grid edits as one batch (Track B6). Opens the confirm.
    SubmitChanges,
    /// Discard the staged grid edits (Track B6).
    RevertChanges,
    /// Append a new draft (insert) row to the result (Track B6).
    AddRow,
    /// Open the assistant's conversation-history picker (M-S5).
    AssistantHistory,
    /// Start a fresh assistant chat, saving the current one (M-S5).
    AssistantNewChat,
    /// Start a fresh assistant chat on a specific agent, by index into
    /// `usable_agents` (the "New chat with <agent>" entries).
    AssistantNewChatWith(usize),
    /// Reveal the conversations directory in the OS file manager (M-S5).
    RevealConversationStorage,
    /// Open the side-by-side split (a second query pane on the right).
    SplitRight,
    /// Collapse the split back to a single pane.
    Unsplit,
    /// Move focus to the other half of the split.
    FocusOtherHalf,
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
            Cmd::ErDiagram => self.open_er_diagram(cx),
            Cmd::Explain => self.explain_query(false, cx),
            Cmd::ExplainAnalyze => self.explain_query(true, cx),
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
            Cmd::SplitRight => self.split_right(cx),
            Cmd::Unsplit => self.unsplit(cx),
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

    /// Whether guarded in-grid editing is enabled for the active connection (Track
    /// B5): a writable (non-read-only) connection whose engine supports the
    /// transactional, exactly-one-row edit contract. Read-only is the safe default,
    /// and an OLAP engine (ClickHouse) is excluded even when writable: its async,
    /// non-atomic mutations can't honor the guarded-edit guarantees.
    pub(crate) fn editing_enabled(&self) -> bool {
        matches!(
            &self.phase,
            Phase::Connected(active)
                if !active.config.read_only && active.config.kind.write_caps().guarded_edit
        )
    }

    /// The focused result cell's edit target, when editing is enabled and the cell
    /// is editable (a single-table keyed browse, non-PK, non-clipped). `None`
    /// otherwise; the entry point and palette gate both consult this.
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

    /// The commands available in the current phase, each paired with its `Cmd`.
    /// Phase-specific actions come first (what the user most likely wants), then
    /// the always-available ones.
    fn palette_entries(&self) -> Vec<(PaletteItem, Cmd)> {
        let mut out: Vec<(PaletteItem, Cmd)> = Vec::new();

        match &self.phase {
            // A Redis connection has its own workspace (panels/tabs, no SQL
            // editor), so it gets its own command set instead of the query ones.
            Phase::Connected(active) if active.kv_view.is_some() => {
                use crate::kvbrowse::KvPanel;
                out.push((
                    PaletteItem::new("cmd:kv-new-tab", "redis: new tab").hint("⌘T"),
                    Cmd::KvNewTab,
                ));
                out.push((
                    PaletteItem::new("cmd:kv-tree", "redis: toggle namespace tree"),
                    Cmd::KvToggleTree,
                ));
                out.push((
                    PaletteItem::new("cmd:kv-bigkeys", "redis: find biggest keys"),
                    Cmd::KvFindBigKeys,
                ));
                out.push((
                    PaletteItem::new("cmd:kv-analyze", "redis: analyze keyspace"),
                    Cmd::KvOpenPanel(KvPanel::Analysis),
                ));
                out.push((
                    PaletteItem::new("cmd:kv-console", "redis: open console"),
                    Cmd::KvOpenPanel(KvPanel::Console),
                ));
                out.push((
                    PaletteItem::new("cmd:kv-monitor", "redis: open monitor (slow log · clients)"),
                    Cmd::KvOpenPanel(KvPanel::Monitor),
                ));
                out.push((
                    PaletteItem::new("cmd:kv-keyspace", "redis: watch keyspace notifications"),
                    Cmd::KvOpenPanel(KvPanel::Keyspace),
                ));
                out.push((
                    PaletteItem::new("cmd:kv-pubsub", "redis: open pub/sub"),
                    Cmd::KvOpenPanel(KvPanel::PubSub),
                ));
                // Connection switching, settings, shortcuts, etc. come from the
                // shared tail appended after this match.
            }
            Phase::Connected(active) => {
                out.push((
                    PaletteItem::new("cmd:run", "query: run").hint("⌘↵"),
                    Cmd::RunQuery,
                ));
                out.push((
                    PaletteItem::new("cmd:new-tab", "query: new tab").hint("⌘T"),
                    Cmd::NewTab,
                ));
                // Tab management: close needs an open tab; switching needs two.
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
                // Side-by-side split: offer open/focus while split, else open.
                if active.split.is_some() {
                    out.push((
                        PaletteItem::new("cmd:unsplit", "view: unsplit").hint("⌘\\"),
                        Cmd::Unsplit,
                    ));
                    out.push((
                        PaletteItem::new("cmd:focus-other-half", "view: focus other split half")
                            .hint("⌥⌘\\"),
                        Cmd::FocusOtherHalf,
                    ));
                } else if active.active().is_some() {
                    out.push((
                        PaletteItem::new("cmd:split-right", "view: split right").hint("⌘\\"),
                        Cmd::SplitRight,
                    ));
                }
                // Whole-schema migration, offered only when the selected/only schema
                // has tables to move (the handler picks the target database).
                if self.migrate_source().is_some() {
                    out.push((
                        PaletteItem::new("cmd:migrate-schema", "schema: migrate to…"),
                        Cmd::MigrateSchema,
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
                    out.push((
                        PaletteItem::new("cmd:copy-to-table", "result: copy to table…"),
                        Cmd::CopyToTable,
                    ));
                }
                // Staged data editing (B6), offered on a writable, edit-enabled
                // connection browsing an editable (single-table keyed) result. Add
                // row is always available there; submit/revert only with changes.
                if self.editing_enabled()
                    && active.active_result().is_some_and(|g| g.editable_browse())
                {
                    out.push((
                        PaletteItem::new("cmd:add-row", "data: add row").hint("⌥⌘N"),
                        Cmd::AddRow,
                    ));
                    if self.has_pending_changes() {
                        out.push((
                            PaletteItem::new("cmd:submit-changes", "data: submit changes")
                                .hint("⌘↵"),
                            Cmd::SubmitChanges,
                        ));
                        out.push((
                            PaletteItem::new("cmd:revert-changes", "data: revert changes")
                                .hint("⌥⌘Z"),
                            Cmd::RevertChanges,
                        ));
                    }
                }
                out.push((
                    PaletteItem::new("cmd:history", "query: toggle history"),
                    Cmd::ToggleHistory,
                ));
                out.push((
                    PaletteItem::new("cmd:clear-history", "query: clear history"),
                    Cmd::ClearHistory,
                ));
                // Saved queries (B3): save needs an open tab to save *from*; the
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
                // EXPLAIN (B4): explain needs a query to explain; analyze runs the
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
                    out.push((
                        PaletteItem::new("cmd:format-sql", "editor: format SQL").hint("⌥⌘F"),
                        Cmd::FormatSql,
                    ));
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
                    PaletteItem::new("cmd:columns", "view: toggle columns panel").hint("⇧⌘C"),
                    Cmd::ToggleColumnsPanel,
                ));
                out.push((
                    PaletteItem::new("cmd:refresh", "schema: refresh").hint("⌘R"),
                    Cmd::RefreshSchema,
                ));
                out.push((
                    PaletteItem::new("cmd:er-diagram", "schema: ER diagram"),
                    Cmd::ErDiagram,
                ));
                out.push((
                    PaletteItem::new("cmd:disconnect", "connection: disconnect"),
                    Cmd::Disconnect,
                ));
                // Assistant conversation history (M-S5), only with the panel open.
                if self.assistant.is_some() {
                    out.push((
                        PaletteItem::new("cmd:ai-new-chat", "agent: new chat"),
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
                        PaletteItem::new("cmd:ai-history", "agent: conversation history…"),
                        Cmd::AssistantHistory,
                    ));
                    out.push((
                        PaletteItem::new("cmd:ai-storage", "agent: open conversation storage"),
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
                    PaletteItem::new("cmd:new-conn", "connection: new").hint("⌘N"),
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

        out.push((
            PaletteItem::new("cmd:switch-conn", "connection: switch…").hint("⌘P"),
            Cmd::SwitchConnection,
        ));
        out.push((
            PaletteItem::new("cmd:shortcuts", "view: keyboard shortcuts").hint("⌘/"),
            Cmd::ShowShortcuts,
        ));
        out.push((
            PaletteItem::new("cmd:whats-new", "help: what's new"),
            Cmd::ShowChangelog,
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
        out.push((
            PaletteItem::new("cmd:keymap-file", "keymap: customize keybindings"),
            Cmd::OpenKeymapFile,
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
        match crate::queries::save(&name, None, &sql) {
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
        let queries = crate::queries::load();
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
        let candidates = self.copy_target_candidates();
        let namespaces = self.copy_namespace_candidates();
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
        let id = self.next_export_id;
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
    /// table's name. On submit, [`submit_copy_new_table`] creates the table from the
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
        if self.namespace_has_table(pending.session, &pending.schema, name) {
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
        let id = self.next_export_id;
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
    /// database). On pick, [`pick_migrate_target`] fires the whole-schema migration.
    /// No-op (with a hint) when nothing is migratable / no target is open.
    pub(crate) fn open_migrate_picker(&mut self, cx: &mut Context<Self>) {
        let Some((session, schema, tables)) = self.migrate_source() else {
            self.notify(
                ToastVariant::Info,
                "Select a schema with tables to migrate",
                cx,
            );
            return;
        };
        // Targets: every writable namespace except the source schema itself.
        let targets: Vec<_> = self
            .copy_namespace_candidates()
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
                let item = PaletteItem::new(id, format!("{} ({table_count} table(s))", ns.schema))
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
        let id = self.next_export_id;
        self.next_export_id += 1;
        self.start_migrate(id, source_schema, tables, target.session, target.schema, cx);
    }
}
