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
    RunQuery,
    NewTab,
    ToggleHistory,
    ToggleSidebar,
    RefreshSchema,
    Disconnect,
    /// Open the "go to row…" prompt (only when a result is open).
    GoToRow,
    /// Open the keyboard-shortcuts reference overlay.
    ShowShortcuts,
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
        cx.subscribe(&palette, Self::on_palette_event).detach();
        self.palette = Some(palette);
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
        cx.subscribe(&prompt, Self::on_palette_event).detach();
        self.palette = Some(prompt);
        self.palette_cmds.clear();
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
            // Prompt mode (the "go to row" input) submits free text.
            PaletteEvent::Submit(text) => {
                self.close_palette();
                self.submit_goto(text, cx);
            }
            PaletteEvent::Dismiss => self.close_palette(),
        }
        cx.notify();
    }

    /// Parse the go-to-row prompt's text and navigate, or toast on bad input.
    /// Digit separators (`,` `_` spaces) are tolerated so a pasted "1,000" works.
    fn submit_goto(&mut self, text: &str, cx: &mut Context<Self>) {
        let cleaned: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
        match cleaned.parse::<usize>() {
            Ok(n) if n >= 1 => self.go_to_row(n, cx),
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
            Cmd::RunQuery => self.run_editor_query(cx),
            Cmd::NewTab => self.new_query(cx),
            Cmd::ToggleHistory => self.toggle_history(cx),
            Cmd::ToggleSidebar => self.toggle_sidebar(cx),
            Cmd::RefreshSchema => self.refresh_schema(),
            Cmd::Disconnect => self.disconnect(cx),
            Cmd::GoToRow => self.open_goto_prompt(cx),
            Cmd::ShowShortcuts => self.toggle_shortcuts(cx),
        }
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
                // Only meaningful with rows on screen to navigate.
                if active.active_result().is_some() {
                    out.push((
                        PaletteItem::new("cmd:goto-row", "go to row…").hint("⌃G"),
                        Cmd::GoToRow,
                    ));
                }
                out.push((
                    PaletteItem::new("cmd:history", "query: toggle history"),
                    Cmd::ToggleHistory,
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
            }
            // Mid-connect there's nothing query-shaped to do; only globals show.
            Phase::Connecting(_) => {}
        }

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
}
