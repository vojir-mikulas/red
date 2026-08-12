//! Root rendering: the `Render` impl that picks the top-level screen, the
//! connecting splash, and the two confirmation modals (destructive statement,
//! close-with-unsaved-work).

use flint::prelude::*;
use gpui::{ClipboardItem, Focusable, KeyDownEvent, Render, Window, div, prelude::*, px};
use red_core::{BatchMode, CopyMode};

use super::{AppState, ConnectStatus, Connecting, Phase};
use crate::app::AiReviewState;
use crate::app::PreflightCount;
use crate::keymap::{
    About, AddRow, BeginEdit, CloseInspector, ClosePane, CloseTab, CycleFocusNext, CycleFocusPrev,
    DeleteRow, EqualizePanes, Explain, FindInResult, FocusOtherHalf, FormatSql, MaximizePane,
    NewConnection, NewTab, NextTab, OpenSavedQueries, PrevTab, RefreshSchema, ReportBug,
    RevertChanges, RunQuery, RunScript, SaveQuery, SearchSchema, SelectAll, SetNull, Settings,
    ShowChangelog, ShowErDiagram, ShowShortcuts, SplitDown, SubmitChanges, SwitchConnection,
    SwitchToConnectionSlot, SwitchToPreviousConnection, ToggleAssistant, ToggleColumnsPanel,
    ToggleFilter, ToggleHistory, ToggleInspector, ToggleSidebar, ToggleSplit,
};
use crate::palette::{CopyResult, GoToObject, GoToRow, ToggleCommandPalette};
use red_core::sql::RiskLevel;

impl AppState {
    /// The connecting splash: an indeterminate progress bar while an attempt is
    /// in flight, the error plus a backoff countdown between transient retries, a
    /// terminal error with "Edit connection" on a fatal failure (bad credentials,
    /// missing database), and always a Cancel button, plus "Retry now" while
    /// backing off.
    fn render_connecting(
        &self,
        conn: &Connecting,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let name = conn.config.name.clone();

        // The splash has no top bar, so float the window controls (Linux/Wayland
        // only) in the corner; they `None` out where the OS draws its own.
        let controls = crate::window_chrome::window_controls(window, theme)
            .map(|c| div().absolute().top(px(14.)).right(px(16.)).child(c));

        let status = div().flex().flex_col().items_center().gap_2().w(px(360.));
        let status = match &conn.status {
            ConnectStatus::InProgress => {
                let label = if conn.attempt > 1 {
                    format!("Connecting to {name}… (attempt {})", conn.attempt)
                } else {
                    format!("Connecting to {name}…")
                };
                status
                    .child(div().text_color(theme.text).child(label))
                    .child(ProgressBar::new("connect-progress", 0.0).indeterminate(true))
            }
            ConnectStatus::Backoff { error, delay } => status
                .child(
                    div()
                        .text_color(theme.text)
                        .child(format!("Couldn't connect to {name}")),
                )
                .child(
                    div()
                        .text_color(theme.text_muted)
                        .text_size(theme.scale(12.))
                        .child(error.clone()),
                )
                .child(
                    div()
                        .text_color(theme.text_muted)
                        .text_size(theme.scale(12.))
                        .child(format!("Retrying in {}s…", delay.as_secs())),
                )
                .child(ProgressBar::new("connect-progress", 0.0).indeterminate(true)),
            // Terminal: no countdown, no progress bar; the user must fix the
            // connection. The red tint marks it as a stop, not a transient wait.
            ConnectStatus::Failed { error } => status
                .child(
                    div()
                        .text_color(theme.text)
                        .child(format!("Couldn't connect to {name}")),
                )
                .child(
                    div()
                        .text_color(theme.red)
                        .text_size(theme.scale(12.))
                        .child(error.clone()),
                ),
            // Untrusted SSH host: show the fingerprint to verify before trusting.
            ConnectStatus::NeedsHostTrust {
                host, fingerprint, ..
            } => status
                .child(
                    div()
                        .text_color(theme.text)
                        .child(format!("Unknown SSH host: {host}")),
                )
                .child(
                    div()
                        .text_color(theme.text_muted)
                        .text_size(theme.scale(12.))
                        .child(format!("Key fingerprint: {fingerprint}")),
                )
                .child(
                    div()
                        .text_color(theme.text_muted)
                        .text_size(theme.scale(12.))
                        .child(crate::i18n::tr!("connect.trust_host_body", "Verify the fingerprint, then trust this host to add it to ~/.ssh/known_hosts.")),
                ),
        };

        let mut actions = div().flex().justify_center().gap_2();
        if matches!(conn.status, ConnectStatus::Backoff { .. }) {
            actions = actions.child(
                Button::new("connect-retry", "Retry now")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.retry_now(cx))),
            );
        }
        if matches!(conn.status, ConnectStatus::Failed { .. }) {
            actions = actions.child(
                Button::new("connect-edit", "Edit connection")
                    .variant(ButtonVariant::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.edit_failed_connection(cx))),
            );
        }
        if matches!(conn.status, ConnectStatus::NeedsHostTrust { .. }) {
            actions = actions.child(
                Button::new("connect-trust", "Trust & connect")
                    .variant(ButtonVariant::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.trust_host_and_retry(cx))),
            );
        }
        actions = actions.child(
            Button::new("connect-cancel", "Cancel")
                .variant(ButtonVariant::Secondary)
                .on_click(cx.listener(|this, _, _, cx| this.cancel_connect(cx))),
        );

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(theme.bg_app)
            .font_family(theme.font_family.clone())
            .child(status)
            .child(actions)
            .children(controls)
    }
}

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Dev perf HUD: time this render and tally its allocation churn. No-op
        // (compiled out) in a normal build.
        #[cfg(feature = "dev-stats")]
        self.dev_stats.begin_frame();

        // First frame after an update: raise the one-shot "RED updated to X" toast
        // (`pending_update` was set in `new`). `take` ensures it fires only once.
        if let Some(version) = self.pending_update.take() {
            self.notify_update(version, cx);
        }

        // Before anything else that touches focus: if the last frame left focus
        // pointing at an element that is no longer rendered, pull it back to the
        // root, or every `RedRoot`-scoped binding silently stops matching. See
        // `ensure_focus_anchored`.
        self.ensure_focus_anchored(window, cx);

        // An overlay just closed (or we're starting up): reclaim focus so the
        // global ⌘K binding has a live dispatch target again.
        if self.refocus_root {
            self.refocus_root = false;
            window.focus(&self.root_focus, cx);
        }

        // A focus move requested from a Window-less spot (e.g. the editor's Esc
        // event); apply it now that the Window is in hand.
        if let Some(pane) = self.pending_focus.take() {
            self.focus_pane(pane, window, cx);
        }
        if let Some(id) = self.pending_focus_target.take() {
            self.focus_target_by_id(id, window, cx);
        }

        // Hints just appeared: move focus onto their layer so the hint keys
        // reach it rather than a focused editor.
        self.take_focus_for_hints(window, cx);

        // Keep the split's focused half in step with where keyboard focus actually
        // sits, so clicking into either half (incl. its editor) lights it as active
        // and aims run/export/filter there. No-op when not split.
        self.sync_split_focus(window, cx);

        // The connection form just opened; focus its name field so the user can
        // type immediately (and Tab onward through the fields).
        if self.focus_name_field {
            self.focus_name_field = false;
            window.focus(&self.name_input.focus_handle(cx), cx);
        }

        // The Redis "New key" modal just opened; focus its name field.
        if self.focus_create_key {
            self.focus_create_key = false;
            if let Phase::Connected(active) = &self.phase
                && let Some(name) = active
                    .kv_view
                    .as_ref()
                    .and_then(|v| v.active_browse())
                    .and_then(|b| b.create_key.as_ref())
                    .map(|ck| ck.name.clone())
            {
                window.focus(&name.focus_handle(cx), cx);
            }
        }

        // The history popover just opened: focus it so its arrow keys work.
        if self.focus_history {
            self.focus_history = false;
            if let Phase::Connected(active) = &self.phase {
                window.focus(&active.history_panel.focus_handle(cx), cx);
            }
        }

        // ⌘F / search command: on the welcome screen, focus the connection search
        // box; in the connected shell, reveal the sidebar and focus the schema filter.
        if self.focus_search {
            self.focus_search = false;
            if matches!(self.phase, Phase::Disconnected) {
                window.focus(&self.connect_search.focus_handle(cx), cx);
            } else {
                self.open_schema_search(window, cx);
            }
        }

        // ⌘⇧F: the result filter bar just opened (or switched mode); focus the box
        // the current mode shows, so typing lands in it at once.
        if self.focus_filter {
            self.focus_filter = false;
            if let Some(bar) = &self.filter_bar {
                window.focus(&bar.focus_handle(cx), cx);
            }
        }

        // ⌘F (grid): the find bar just opened; focus its input to type at once.
        if self.focus_find {
            self.focus_find = false;
            if let Some(bar) = &self.find_bar {
                window.focus(&bar.input.focus_handle(cx), cx);
            }
        }

        // ⌘L: the assistant panel just opened; focus its prompt box.
        if self.focus_assistant {
            self.focus_assistant = false;
            if let Some(panel) = &self.assistant {
                window.focus(&panel.input.focus_handle(cx), cx);
            }
        }

        // An inline conversation rename just began; focus its edit field.
        if self.focus_rename {
            self.focus_rename = false;
            if let Some(rename) = self.assistant.as_ref().and_then(|p| p.renaming.as_ref()) {
                window.focus(&rename.input.focus_handle(cx), cx);
            }
        }

        // A Settings agent key row just opened; focus its field.
        if self.focus_ai_key {
            self.focus_ai_key = false;
            window.focus(&self.ai_key_input.focus_handle(cx), cx);
        }

        // A subscription sign-in prompt just appeared; focus its code field.
        if self.focus_login_code {
            self.focus_login_code = false;
            window.focus(&self.ai_login_code.focus_handle(cx), cx);
        }

        // An inline cell edit just opened in the inspector; focus its
        // field so the user types the new value immediately.
        if self.focus_inspector_edit {
            self.focus_inspector_edit = false;
            if let Some(handle) = self.inspector_edit_focus(cx) {
                window.focus(&handle, cx);
            }
        }

        // An inline cell edit just opened in the grid; focus its field.
        if self.focus_grid_edit {
            self.focus_grid_edit = false;
            if let Some(handle) = self.grid_edit_focus(cx) {
                window.focus(&handle, cx);
            }
        }

        // Commit-on-blur: while an inline editor is open, a focus-out listener on its
        // field stages the edit when the user clicks away (like a spreadsheet); the
        // cell then shows as dirty. Registered once when an editor opens, dropped when
        // it closes. Mirrors `modal_focus_trap`.
        if self.grid_edit.is_some() {
            if self.grid_edit_blur.is_none()
                && let Some(handle) = self.grid_edit_focus(cx)
            {
                let weak = cx.entity().downgrade();
                let sub = window.on_focus_out(&handle, cx, move |_event, _window, cx| {
                    if let Some(app) = weak.upgrade() {
                        // Commit only if an editor is still open (a Submit/Cancel
                        // already cleared it, so its focus move is a no-op here).
                        app.update(cx, |this, cx| {
                            if this.grid_edit.is_some() {
                                this.commit_grid_edit(cx);
                            }
                        });
                    }
                });
                self.grid_edit_blur = Some(sub);
            }
        } else {
            self.grid_edit_blur = None;
        }

        // The palette's "switch connection" command: open the switcher popover
        // now that the `Window` its field-focus needs is in hand.
        if self.open_switcher {
            self.open_switcher = false;
            self.toggle_switcher(window, cx);
        }

        // A keyboard-driven modal (a confirmation or the shortcuts overlay) just
        // opened. Focus it so Flint's `Modal` hears its Esc/Enter.
        if self.focus_modal {
            self.focus_modal = false;
            // A type-to-confirm box is the whole point of the modal it sits in, so
            // put the caret there rather than making the user Tab to it; the
            // knowledge editor is the same case (the modal *is* the editor).
            // Everything else focuses the modal root, where Flint's Enter/Esc
            // handling lives.
            let inner = self
                .confirm_input
                .as_ref()
                .map(|typed| typed.input.focus_handle(cx))
                .or_else(|| {
                    self.knowledge_editor
                        .as_ref()
                        .map(|(view, _)| view.focus_handle(cx))
                });
            match inner {
                Some(handle) => window.focus(&handle, cx),
                None => window.focus(&self.modal_focus.clone(), cx),
            }
        }

        // Focus trap: while a modal is open, a focus-out listener on `modal_focus`
        // pulls focus back inside if Tab would carry it to the backdrop. Registered
        // once when a modal opens (the modal's panel is a descendant of
        // `modal_focus`), and dropped (unsubscribing) when it closes.
        // Mirrored for the trap below, which cannot ask `self`: it fires from
        // gpui's focus dispatch, which can run while this entity is leased for
        // *this* render, and reading it back through its own handle there aborts
        // the process rather than returning an error.
        self.modal_open.set(self.any_modal_open());
        if self.any_modal_open() {
            if self.modal_focus_trap.is_none() {
                let handle = self.modal_focus.clone();
                let modal_open = self.modal_open.clone();
                let sub = window.on_focus_out(&handle.clone(), cx, move |_event, window, cx| {
                    // Re-enter only while a modal is genuinely still open (not mid-
                    // close) and focus actually left the modal subtree.
                    let open = modal_open.get();
                    if open && !handle.contains_focused(window, cx) {
                        // Bounce focus back to the modal root (the scrim, ancestor
                        // of every modal control). The next Tab then walks *into*
                        // the modal's children rather than the chrome behind it.
                        // (A `focus_next` here would defer and re-escape, since the
                        // out-of-modal element still holds focus this frame.)
                        window.focus(&handle, cx);
                    }
                });
                self.modal_focus_trap = Some(sub);
            }
        } else {
            self.modal_focus_trap = None;
        }

        // Keep a tabbed-to settings control on screen: detect the focused dropdown/
        // size input and scroll the content pane to it if it's off the fold. Runs
        // before the panel is built so the focused control can tag its bounds.
        self.update_settings_scroll(window, cx);

        // Detail inspector: drop a loaded/in-flight full value once the cursor has
        // moved off the cell it belonged to, so a big inspected value never outlives
        // the cursor sitting on it (the "bytes dropped when focus moves" promise).
        self.reconcile_inspector(cx);

        // First paint: install the OS-appearance observer and the settings
        // file-watcher (both need a live `Window`).
        self.ensure_observers(window, cx);

        let screen = match &self.phase {
            Phase::Disconnected => self.render_connect(window, cx).into_any_element(),
            Phase::Connecting(conn) => self.render_connecting(conn, window, cx).into_any_element(),
            // Redis has no SQL surface at all yet (R0; keyspace browsing lands
            // in R1) — a dedicated minimal shell
            // instead of the SQL workspace's editor/grid/schema tree, which
            // all assume a `DatabaseDriver` session.
            Phase::Connected(active) if active.config.kind == red_core::DbKind::Redis => self
                .render_redis_shell(active, window, cx)
                .into_any_element(),
            // MongoDB is a document store, not SQL: a dedicated browse/inspector shell
            // instead of the editor/grid/schema workspace (which assumes a
            // `DatabaseDriver` session).
            Phase::Connected(active) if active.config.kind == red_core::DbKind::Mongo => self
                .render_mongo_shell(active, window, cx)
                .into_any_element(),
            Phase::Connected(active) => self.render_shell(active, window, cx).into_any_element(),
        };

        // The notification stack, anchored bottom-right and growing upward:
        // oldest first in the column, so the newest sits nearest the corner. At
        // most `MAX_VISIBLE` show; the rest collapse into a "+N more" line on top.
        let toast = (!self.notifications.is_empty()).then(|| self.render_notifications(cx));

        let confirm = self
            .confirm_exec
            .clone()
            .map(|pending| self.render_confirm(pending.write, cx));

        let confirm_close = self
            .confirm_close_tab
            .and_then(|i| self.tab_title(i))
            .map(|title| self.render_confirm_close(title, cx));

        let confirm_kv_delete = self
            .confirm_kv_delete
            .as_ref()
            .map(|(_, key)| key.clone())
            .map(|key| self.render_kv_confirm_delete(key, cx));

        let confirm_close_batch = self
            .confirm_close_batch
            .clone()
            .map(|indices| self.render_confirm_close_batch(indices.len(), cx));

        let confirm_delete = self
            .confirm_delete_conn
            .and_then(|i| self.connections.get(i))
            .map(|c| c.config.name.clone())
            .map(|name| self.render_confirm_delete(name, cx));

        let confirm_reset = self.confirm_reset.then(|| self.render_confirm_reset(cx));

        let settings = self
            .settings_open
            .then(|| self.render_settings(cx).into_any_element());

        let shortcuts = self.shortcuts_open.then(|| self.render_shortcuts(cx));

        let whats_new = self.whats_new_open.then(|| self.render_whats_new(cx));

        let import_wizard = self
            .import_wizard
            .as_ref()
            .map(|w| self.render_import_wizard(w, cx));

        // The data-compare (table diff) report is a full-screen overlay hung
        // off the connection.
        let diff_report = match &self.phase {
            Phase::Connected(active) if active.diff.is_some() => Some(self.render_diff(active, cx)),
            _ => None,
        };

        let theme = cx.theme();
        // Copied out now (Hsla is Copy) so the client-decoration frame at the end
        // of this fn doesn't hold `theme`'s borrow of `cx` across the dev-stats
        // block's mutable `cx` use below.
        let frame_border = theme.border;
        // Same reasoning: copied out here so the autoscroll indicator built
        // near the end of this fn doesn't extend `theme`'s borrow of `cx`
        // across the mutable `cx` uses in the dropdown/overlay chain below.
        let (autoscroll_bg, autoscroll_border) = (theme.accent_ghost, theme.accent);
        let root = div()
            .size_full()
            .relative()
            // Anchor focus + the global ⌘K binding here so the palette toggles
            // from any phase, even when no field or editor is focused.
            .key_context("RedRoot")
            .track_focus(&self.root_focus)
            // Hold-to-reveal focus hints. GPUI dispatches modifier changes down
            // the focus path, so this fires from anywhere only because the root
            // is guaranteed to be on that path (see `ensure_focus_anchored`).
            .on_modifiers_changed(cx.listener(
                |this, ev: &gpui::ModifiersChangedEvent, window, cx| {
                    this.on_focus_modifiers(ev, window, cx);
                },
            ))
            // Every listener in this chain guards on `globals_enabled`: the root is
            // an *ancestor* of `modal_focus` and Flint's `Modal` does not swallow
            // action dispatch, so without it these all fire straight through an open
            // confirm. See `AppState::globals_enabled`.
            .on_action(cx.listener(|this, _: &ToggleCommandPalette, _, cx| {
                if this.globals_enabled() {
                    this.toggle_palette(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SwitchConnection, window, cx| {
                if this.globals_enabled() {
                    this.toggle_switcher(window, cx);
                }
            }))
            // ⌘⇧P flips to the previous connection; ⌘1–9 jump to the n-th in the
            // switcher's order. True globals (like ⌘P), so they fire from any focus —
            // but not from under a modal, which is what let a confirmed `DROP` land
            // on whichever connection the user had switched to meanwhile.
            .on_action(cx.listener(|this, _: &SwitchToPreviousConnection, _, cx| {
                if this.globals_enabled() {
                    this.switch_to_previous(cx);
                }
            }))
            .on_action(cx.listener(|this, action: &SwitchToConnectionSlot, _, cx| {
                if this.globals_enabled() {
                    this.switch_to_slot(action.0, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &GoToRow, _, cx| this.open_goto_prompt(cx)))
            .on_action(cx.listener(|this, _: &GoToObject, _, cx| this.open_object_picker(cx)))
            .on_action(cx.listener(|this, _: &CopyResult, _, cx| this.copy_result_selection(cx)))
            // ⌘I toggles the cell detail inspector; Esc dismisses the topmost
            // transient overlay: an open dropdown / cell menu first, then the
            // inspector (no-op when nothing is open).
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| this.toggle_inspector(cx)))
            .on_action(cx.listener(|this, _: &CloseInspector, _, cx| this.dismiss_overlay(cx)))
            .on_action(cx.listener(|this, _: &ToggleAssistant, window, cx| {
                this.toggle_assistant(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleFilter, window, cx| {
                if this.globals_enabled() {
                    this.toggle_filter_bar(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FindInResult, window, cx| {
                if this.globals_enabled() {
                    this.toggle_find_bar(window, cx);
                }
            }))
            // Saved queries (B3): ⇧⌘S opens the name prompt; ⇧⌘O the picker.
            .on_action(cx.listener(|this, _: &SaveQuery, _, cx| this.open_save_prompt(cx)))
            .on_action(cx.listener(|this, _: &OpenSavedQueries, _, cx| this.open_saved_picker(cx)))
            // EXPLAIN (B4): ⇧⌘E opens the plan view for the active query.
            .on_action(cx.listener(|this, _: &Explain, _, cx| this.explain_query(false, cx)))
            // Beautify the editor's SQL in place (⌥⌘F).
            .on_action(cx.listener(|this, _: &FormatSql, _, cx| this.format_active_sql(cx)))
            // App-chrome actions (tabs · sidebar · schema reload), bound in the
            // central keymap to `RedRoot` so they fire from any pane's focus.
            .on_action(cx.listener(|this, _: &NewTab, _, cx| {
                if this.globals_enabled() {
                    this.new_query(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| {
                if this.globals_enabled() {
                    this.close_active_tab(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                if this.globals_enabled() {
                    this.next_tab(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| {
                if this.globals_enabled() {
                    this.prev_tab(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleHistory, _, cx| this.toggle_history(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleColumnsPanel, _, cx| this.toggle_columns_panel(cx)),
            )
            .on_action(cx.listener(|this, _: &RefreshSchema, _, cx| this.refresh_active(cx)))
            .on_action(cx.listener(|this, _: &SearchSchema, window, cx| {
                if !this.globals_enabled() {
                    return;
                }
                // A Mongo connection's ⌘F (from the tree or root, where the grid's
                // Table-scoped FindInResult doesn't reach) focuses the sidebar
                // collection search, mirroring the SQL "search schema" idiom.
                if this.doc_focus_tree_filter(window, cx) {
                    return;
                }
                this.focus_search = true;
                cx.notify();
            }))
            // Focus cycling, in focus-target registry order. The direct
            // ⌥⌘1/2/3 pane jumps are retired in favour of the hint overlay.
            .on_action(cx.listener(|this, _: &CycleFocusNext, window, cx| {
                this.cycle_focus(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CycleFocusPrev, window, cx| {
                this.cycle_focus(false, window, cx)
            }))
            // Panes: ⌘\ splits the focused pane to the right (repeatable), ⌥⌘\
            // cycles focus. `ToggleSplit` keeps its action id so an existing user
            // keymap binding it still splits.
            .on_action(cx.listener(|this, _: &ToggleSplit, _, cx| {
                if this.globals_enabled() {
                    this.split_right(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SplitDown, _, cx| {
                if this.globals_enabled() {
                    this.split_down(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ClosePane, _, cx| {
                if this.globals_enabled() {
                    this.close_pane(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &MaximizePane, _, cx| {
                if this.globals_enabled() {
                    this.zoom_pane(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &EqualizePanes, _, cx| {
                if this.globals_enabled() {
                    this.equalize_panes(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FocusOtherHalf, _, cx| {
                if this.globals_enabled() {
                    this.focus_other_half(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ShowShortcuts, _, cx| this.toggle_shortcuts(cx)))
            .on_action(cx.listener(|this, _: &ShowChangelog, _, cx| this.toggle_whats_new(cx)))
            .on_action(cx.listener(|this, _: &ShowErDiagram, _, cx| {
                let ns = this.er_target_namespace(cx);
                this.open_er_diagram(ns, cx)
            }))
            // Settings panel: ⌘, and the RED → Settings… / About RED menu items.
            .on_action(cx.listener(|this, _: &Settings, _, cx| this.open_settings(cx)))
            .on_action(cx.listener(|this, _: &About, _, cx| this.open_about(cx)))
            // Help → Report a Bug…: open the issue tracker in the browser.
            .on_action(cx.listener(|this, _: &ReportBug, _, cx| {
                this.open_external(crate::app::ISSUES_URL, cx)
            }))
            // --- staged grid editing ---
            // Enter/F2 in the "Table" context: on a Mongo document grid or a Redis
            // key list it opens the inspector on the keyboard cursor; otherwise it
            // begins an in-place SQL cell edit (the same binding, the right thing
            // per pane).
            .on_action(cx.listener(|this, _: &BeginEdit, window, cx| {
                if !this.doc_activate_cursor(window, cx) && !this.kv_activate_cursor(window, cx) {
                    this.begin_grid_edit(cx);
                }
            }))
            // ⌘↵ in the grid submits staged changes; with nothing staged it falls
            // through to running the active query (so the key still does the
            // expected thing on a clean grid).
            .on_action(cx.listener(|this, _: &SubmitChanges, _, cx| {
                if !this.globals_enabled() {
                    return;
                }
                if this.has_pending_changes(cx) {
                    this.submit_changes(cx);
                } else {
                    this.run_editor_query(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RevertChanges, _, cx| this.revert_changes(cx)))
            .on_action(cx.listener(|this, _: &DeleteRow, _, cx| this.toggle_delete_rows(cx)))
            .on_action(cx.listener(|this, _: &AddRow, _, cx| this.add_draft_row(cx)))
            .on_action(cx.listener(|this, _: &SetNull, _, cx| this.set_cell_null(cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.result_select_all(cx)))
            // ⌘↵ runs the active tab's query from any pane, or tests the connection
            // while the form is open. ⌘N on the welcome screen adds a connection.
            .on_action(cx.listener(|this, _: &RunQuery, _, cx| {
                // The connection form is the one modal that wants ⌘↵: it means
                // "test this connection". Everywhere else, re-grading and replacing
                // the pending confirm mid-decision is the opposite of what a confirm
                // is for.
                if this.form.is_some() {
                    this.test_connection(cx);
                } else if this.globals_enabled() {
                    this.run_editor_query(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RunScript, _, cx| {
                if this.globals_enabled() {
                    this.run_editor_script(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NewConnection, _, cx| {
                if matches!(this.phase, Phase::Disconnected) && this.form.is_none() {
                    this.open_new_form(cx);
                }
            }))
            // Welcome-screen card navigation (the modals own their own Esc/Enter
            // via Flint's `Modal` focus handling). ↑/↓ move the highlight, Enter
            // connects. Only acts on the disconnected screen with no form open.
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // The command palette and connection switcher own the keyboard
                // while open: their input has focus, so single-letter card
                // shortcuts (e/⌫) must not fire underneath them.
                if !matches!(this.phase, Phase::Disconnected)
                    || this.form.is_some()
                    || this.any_modal_open()
                    || this.palette.is_some()
                    || this.switcher.read(cx).is_open()
                {
                    return;
                }
                // Navigate the *visible* (filtered + sorted) list; `connect_sel` is a
                // position within it, mapped back to the stored index for actions.
                let visible = this.visible_connections(cx);
                let n = visible.len();
                if n == 0 {
                    return;
                }
                // While the search box has focus, letters/backspace must edit the
                // query; only the navigation keys act as card shortcuts there.
                let search_focused = this.connect_search.focus_handle(cx).is_focused(window);
                // A bare keystroke. Shift is allowed (the hints spell these as
                // capitals), but a ⌘/⌥/⌃ combination belongs to a real binding —
                // ⌘/ opens the shortcut reference — and must not be swallowed
                // here as a card shortcut.
                let m = &event.keystroke.modifiers;
                let bare = !m.platform && !m.control && !m.alt && !m.function;
                let plain = bare && !search_focused;
                let sel = this.connect_sel.min(n - 1);
                match event.keystroke.key.as_str() {
                    "up" => {
                        this.connect_sel = sel.saturating_sub(1);
                        cx.stop_propagation();
                        cx.notify();
                    }
                    "down" => {
                        this.connect_sel = (sel + 1).min(n - 1);
                        cx.stop_propagation();
                        cx.notify();
                    }
                    "enter" => {
                        cx.stop_propagation();
                        this.connect(visible[sel], cx);
                    }
                    // ←/→ page the list, Home/End jump to its ends. Gated on the
                    // search box not having focus: there these keys move the
                    // caret, which is what a user typing expects them to do.
                    "left" | "right" if plain => {
                        cx.stop_propagation();
                        this.connect_page_step(event.keystroke.key == "right", cx);
                    }
                    "home" | "end" if plain => {
                        cx.stop_propagation();
                        this.connect_jump(event.keystroke.key == "end", cx);
                    }
                    // E edits the highlighted connection, ⌫/⌦ asks to remove it;
                    // the keyboard mirrors the hover edit/trash buttons on each card.
                    "e" if plain => {
                        cx.stop_propagation();
                        this.open_edit_form(visible[sel], cx);
                    }
                    "backspace" | "delete" if plain => {
                        cx.stop_propagation();
                        this.request_delete_connection(visible[sel], cx);
                    }
                    // "/" drops into the search box from anywhere on the screen,
                    // the shortcut every list-with-a-filter trains people to try.
                    "/" if plain => {
                        cx.stop_propagation();
                        window.focus(&this.connect_search.focus_handle(cx), cx);
                    }
                    _ => {}
                }
            }))
            .bg(theme.bg_app)
            .text_color(theme.text)
            // The UI font + size from settings, set once at the root so any unsized
            // text inherits the right family/scale (GPUI otherwise defaults to 16px
            // Helvetica). The editor overrides both on its own surface.
            .font_family(self.settings.appearance.ui_font_family.clone())
            .text_size(px(self.settings.appearance.ui_font_size))
            .child(screen)
            .children(toast)
            .children(confirm)
            .children(confirm_close)
            .children(confirm_kv_delete)
            .children(confirm_close_batch)
            .children(confirm_delete)
            .children(settings)
            // Above `settings`, not below it: this confirmation is only reachable
            // from the settings panel's Behavior page, so it is the one overlay
            // that is always layered over an open panel. Siblings paint in order,
            // so listing it first drew it *under* the panel that opened it.
            .children(confirm_reset)
            .children(shortcuts)
            .children(whats_new)
            .children(import_wizard)
            .children(diff_report)
            // The connection form modal is rendered at the root so it works in any
            // phase (the welcome screen *and* the connected shell, e.g. opened from
            // the switcher's "New connection…").
            .children(self.form.as_ref().map(|f| self.render_form(f, cx)))
            // The "Database knowledge" editor, root-mounted so it overlays the
            // whole shell like the other modals. A view: it renders itself, and
            // its own `cx.notify()` repaints it without touching this frame.
            .children(self.knowledge_editor.as_ref().map(|(view, _)| view.clone()))
            // The Redis "New key" modal, rooted here so it overlays the whole
            // shell (not just the browse pane) like the other modals.
            .children(self.render_kv_create_modal(cx))
            // The Redis "Import keys" modal, likewise root-mounted.
            .children(self.render_kv_import_modal(cx))
            .children(self.render_kv_export_modal(cx))
            // The Redis delete-key confirmation, likewise root-mounted.
            .children(self.render_kv_delete_modal(cx))
            // The palette renders its own full-screen overlay; last = on top.
            .children(self.palette.as_ref().map(|(p, _)| p.clone()))
            // The hint layer takes the keyboard while focus hints show. It paints
            // nothing — the badges render inside the surfaces they label — so its
            // place in the stack matters only for key dispatch.
            .children(
                self.focus_hints
                    .is_some()
                    .then(|| self.render_focus_hint_layer(cx)),
            )
            // The result-grid dropdowns (cell / export / more) mount here, above
            // every other overlay, each carrying a window-wide dismiss backdrop.
            // Rooting them at the window (not the result pane) is what lets a click
            // anywhere outside close them, and keeps them from lingering over a
            // modal — the backdrop's `inset_0` now spans the whole window.
            .children(self.cell_menu.map(|pos| self.render_cell_menu(pos, cx)))
            .children(self.export_menu.map(|pos| self.render_export_menu(pos, cx)))
            .children(self.more_menu.map(|pos| self.render_more_menu(pos, cx)))
            .children(
                self.tab_context_menu
                    .map(|(i, pos)| self.render_tab_menu(i, pos, cx)),
            )
            // The in-cell FK suggestion dropdown anchors to the editor
            // cell but mounts here so it paints above the grid and escapes its clip.
            .children(self.render_cell_suggest(window, cx))
            // The middle-click autoscroll origin marker: rooted at the window
            // (not the grid pane) so it positions from the click's window
            // coordinates the same way the cell/export/more dropdowns do.
            .children(self.autoscroll.as_ref().map(|a| {
                floating(crate::result::autoscroll::indicator(
                    autoscroll_bg,
                    autoscroll_border,
                ))
                .offset(gpui::point(px(-7.), px(-7.)))
                .at(a.origin)
            }));

        // Dev perf HUD: register its toggle, overlay the panel last (on top), and
        // close the frame so the rings capture this render's cost.
        #[cfg(feature = "dev-stats")]
        let root = {
            let root = root.on_action(
                cx.listener(|this, _: &crate::ToggleDevStats, _, cx| this.toggle_dev_stats(cx)),
            );
            let panel = self.render_dev_panel(cx);
            self.dev_stats.end_frame();
            root.children(panel)
        };

        // On a client-decorated window (Linux/Wayland) this wraps the app in its
        // own resize border, corner rounding, and shadow; elsewhere it returns
        // `root` untouched.
        crate::window_chrome::frame(window, frame_border, root)
    }
}

impl AppState {
    /// The self-update pill. Shown only mid-flight (`Downloading`) and when a
    /// build is staged (`ReadyToRestart`); the latter is clickable and relaunches
    /// into the new version. All other states are surfaced in the About tab, not
    /// here. Rendered inline by the callers (top bar / welcome screen), placed to
    /// the *left* of the settings + disconnect controls so it never covers them.
    pub(crate) fn render_update_pill(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        use red_core::UpdateState;
        let theme = cx.theme();

        let base = || {
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .px_2p5()
                .py_1()
                .rounded_full()
                .text_size(theme.scale(12.))
                .font_weight(gpui::FontWeight::MEDIUM)
        };

        match &self.update {
            UpdateState::Downloading { .. } => Some(
                base()
                    .bg(theme.bg_elevated)
                    .border_1()
                    .border_color(theme.border_soft)
                    .text_color(theme.text_muted)
                    .child(crate::i18n::tr!(
                        "notify.downloading_update",
                        "Downloading update…"
                    ))
                    .into_any_element(),
            ),
            UpdateState::ReadyToRestart { version } => Some(
                base()
                    .id("update-pill")
                    .bg(theme.accent)
                    .text_color(theme.on_accent)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.accent_hover))
                    .child(format!("Restart to update · {version}"))
                    .on_click(cx.listener(|this, _, _, cx| this.restart_for_update(cx)))
                    .into_any_element(),
            ),
            _ => None,
        }
    }
}

/// How many toasts show at once; older ones beyond this collapse to "+N more".
const MAX_VISIBLE_TOASTS: usize = 5;

impl AppState {
    /// The bottom-right notification stack: oldest first (top), newest last
    /// (nearest the corner). Each toast carries a close `✕` wired to
    /// [`AppState::close_notification`]; the export toast also shows its progress.
    fn render_notifications(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let total = self.notifications.len();
        let hidden = total.saturating_sub(MAX_VISIBLE_TOASTS);

        let mut col = div()
            .absolute()
            .bottom_4()
            .right_4()
            .flex()
            .flex_col()
            .items_end()
            .gap_2();

        if hidden > 0 {
            col = col.child(
                div()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_muted)
                    .child(format!("+{hidden} more")),
            );
        }

        let icon_size = theme.scale(14.);
        let action_size = theme.scale(13.);
        let action_tone = theme.text_muted;

        for n in self.notifications.iter().skip(hidden) {
            let id = n.id;
            // Variant → leading icon + tone. An in-flight export shows a download
            // glyph regardless of its (Info) variant.
            let (icon_name, tone) = if n.export.is_some() {
                ("download", theme.accent)
            } else {
                match n.variant {
                    ToastVariant::Error => ("alert-triangle", theme.red),
                    ToastVariant::Warning => ("alert-triangle", theme.yellow),
                    ToastVariant::Success => ("check", theme.green),
                    ToastVariant::Info => ("sparkles", theme.accent),
                }
            };

            let weak = cx.entity().downgrade();
            // Trailing controls: a copy button (plain toasts only) and a
            // close/cancel button (always). Export progress isn't worth copying,
            // and a toast with its own call-to-action (export-finished's "Show in
            // folder", the post-update "Show changelog") has nothing generic worth
            // copying either — the action *is* the useful affordance.
            let close = IconButton::new(
                ("toast-close", id),
                crate::icons::icon("x", action_size, action_tone),
            )
            .size(IconButtonSize::Sm)
            .on_click({
                let weak = weak.clone();
                move |_, _, cx| {
                    weak.update(cx, |this, cx| this.close_notification(id, cx))
                        .ok();
                }
            });
            let mut actions = div().flex().items_center().gap_1();
            if n.export.is_none() && n.action.is_none() {
                let copy_text = match &n.detail {
                    Some(detail) => format!("{}\n{}", n.message, detail),
                    None => n.message.to_string(),
                };
                actions = actions.child(
                    IconButton::new(
                        ("toast-copy", id),
                        crate::icons::icon("copy", action_size, action_tone),
                    )
                    .size(IconButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                    }),
                );
            }
            // A call-to-action button, accent-tinted to stand out from copy/close,
            // ahead of the close button.
            match &n.action {
                Some(crate::app::NotificationAction::ShowChangelog) => {
                    let weak = weak.clone();
                    actions = actions.child(
                        IconButton::new(
                            ("toast-changelog", id),
                            crate::icons::icon("view", action_size, theme.accent),
                        )
                        .size(IconButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            weak.update(cx, |this, cx| {
                                this.close_notification(id, cx);
                                this.open_whats_new(cx);
                            })
                            .ok();
                        }),
                    );
                }
                Some(crate::app::NotificationAction::RevealInFileManager(path)) => {
                    let path = std::path::PathBuf::from(path.to_string());
                    let weak = weak.clone();
                    actions = actions.child(
                        IconButton::new(
                            ("toast-reveal", id),
                            crate::icons::icon("folder-open", action_size, theme.accent),
                        )
                        .size(IconButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            weak.update(cx, |this, cx| this.reveal_in_file_manager(&path, cx))
                                .ok();
                        }),
                    );
                }
                Some(crate::app::NotificationAction::UndoDelete(batch)) => {
                    let batch = *batch;
                    let weak = weak.clone();
                    actions = actions.child(
                        IconButton::new(
                            ("toast-undo", id),
                            crate::icons::icon("restore", action_size, theme.accent),
                        )
                        .size(IconButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            weak.update(cx, |this, cx| {
                                this.close_notification(id, cx);
                                this.kv_undo_delete(batch, cx);
                            })
                            .ok();
                        }),
                    );
                }
                None => {}
            }
            actions = actions.child(close);

            // The notification id doubles as the toast's a11y id, so each toast
            // becomes a (polite/assertive) live region screen readers announce.
            let mut toast = Toast::new(n.message.clone())
                .id(("toast", id))
                .variant(n.variant)
                .width(px(280.))
                .icon(crate::icons::icon(icon_name, icon_size, tone))
                .actions(actions);

            if let Some(label) = &n.detail_label {
                toast = toast.detail_element(label.clone());
                // Only offer the toggle for a genuinely long body, so "Show more"
                // never reveals nothing.
                let long = n
                    .detail
                    .as_ref()
                    .is_some_and(|d| d.len() > 120 || d.contains('\n'));
                if long {
                    let weak = weak.clone();
                    toast = toast
                        .expandable(true)
                        .expanded(n.expanded)
                        .on_toggle(move |_, cx| {
                            weak.update(cx, |this, cx| this.toggle_notification_expanded(id, cx))
                                .ok();
                        });
                }
            }

            if let Some(export) = &n.export {
                let fraction = if export.total > 0 {
                    export.rows as f32 / export.total as f32
                } else {
                    0.0
                };
                toast = toast.progress(fraction);
            }

            // Wrap the toast so hovering it pauses the auto-dismiss timer (so a
            // message can be read / selected / copied without it vanishing).
            let hover_weak = cx.entity().downgrade();
            col = col.child(
                div()
                    .id(("toast-wrap", id))
                    .on_hover(move |hovered, _, cx| {
                        hover_weak
                            .update(cx, |this, cx| {
                                this.set_notification_hovered(id, *hovered, cx)
                            })
                            .ok();
                    })
                    .child(toast),
            );
        }

        // Defer the whole stack so it paints in the late pass, above the modals
        // (the connection form, settings, confirm dialogs) — which are plain
        // `.absolute()` siblings and would otherwise cover toasts by tree order.
        // Deferred, same-priority as Flint's `floating()` menus, so an open menu
        // still paints above a toast (menus sit later in the root child list).
        gpui::deferred(col)
    }

    /// The title of tab `index`, if it exists, for the close-confirm prompt.
    fn tab_title(&self, index: usize) -> Option<String> {
        match &self.phase {
            Phase::Connected(active) => active.tabs.get(index).map(|t| t.title.clone()),
            _ => None,
        }
    }

    /// Confirmation before closing a tab that holds real work. Mirrors the
    /// destructive-statement modal's shape.
    fn render_confirm_close(
        &self,
        title: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_color(theme.text_muted).child(format!(
                "“{title}” has a query or result that will be lost. Close it?"
            )))
            .child(self.dont_ask_close_tab_checkbox(cx));
        let footer = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("close-cancel", "Keep tab")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_close(cx))),
            )
            .child(
                Button::new("close-confirm", "Close tab")
                    .variant(ButtonVariant::Danger)
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_close(cx))),
            );
        Modal::new("confirm-close-tab")
            .title(crate::i18n::tr!("editor.confirm_close_tab", "Close tab"))
            .width(px(420.))
            .focus_handle(self.modal_focus.clone())
            .footer(footer)
            .on_close(move |_, cx| {
                close_view.update(cx, |this, cx| this.cancel_close(cx)).ok();
            })
            .on_confirm(move |_, cx| {
                confirm_view
                    .update(cx, |this, cx| this.confirm_close(cx))
                    .ok();
            })
            .child(body)
    }

    /// Confirmation before a bulk close (Close Others / Close All / Close Left /
    /// Close Right) that would drop at least one tab's unsaved work. Mirrors
    /// [`Self::render_confirm_close`]; `count` is the number of tabs the batch
    /// would close.
    fn render_confirm_close_batch(
        &self,
        count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();
        let noun = if count == 1 { "tab" } else { "tabs" };
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_color(theme.text_muted).child(format!(
                "This closes {count} {noun}; some hold a query or result that will be lost. Continue?"
            )))
            .child(self.dont_ask_close_tab_checkbox(cx));
        let footer = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("close-batch-cancel", "Keep tabs")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_close_batch(cx))),
            )
            .child(
                Button::new("close-batch-confirm", format!("Close {count} {noun}"))
                    .variant(ButtonVariant::Danger)
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_close_batch_accept(cx))),
            );
        Modal::new("confirm-close-tab-batch")
            .title(crate::i18n::tr!("editor.confirm_close_tabs", "Close tabs"))
            .width(px(420.))
            .focus_handle(self.modal_focus.clone())
            .footer(footer)
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.cancel_close_batch(cx))
                    .ok();
            })
            .on_confirm(move |_, cx| {
                confirm_view
                    .update(cx, |this, cx| this.confirm_close_batch_accept(cx))
                    .ok();
            })
            .child(body)
    }

    /// The "Don't ask again" checkbox shared by the single- and batch-tab-close
    /// confirmations: unticked whenever either modal is open (it can only open
    /// while the setting is still on), and flips `query.confirm_close_tab` off
    /// immediately on check so it applies to this close too.
    fn dont_ask_close_tab_checkbox(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        // Ticked means "stop asking", so the box reflects the *inverse* of the
        // still-live confirm setting: the modal opens while confirm is on (box
        // unticked); ticking calls `set_confirm_close_tab(false)`, which flips the
        // setting and re-renders, so the tick now shows instead of the box
        // staying blank with no feedback.
        let checked = !self.settings.safety.confirm_close_tab;
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Checkbox::new("close-tab-dont-ask", checked)
                    .mark(crate::icons::icon("check", px(12.), theme.on_accent))
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_confirm_close_tab(!checked, cx);
                    })),
            )
            .child(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text_muted)
                    .child(crate::i18n::tr!("common.dont_ask_again", "Don't ask again")),
            )
    }

    /// The "Don't ask again" checkbox shared by the delete/destructive confirmations
    /// across the SQL, Redis, and MongoDB shells: ticking it raises
    /// `query.confirm_from` past `level`, so it also applies to the action being
    /// confirmed right now but leaves anything more dangerous still gated. `id`
    /// distinguishes the modals so several can mount in one frame.
    pub(crate) fn dont_ask_destructive_checkbox(
        &self,
        id: &'static str,
        level: RiskLevel,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // Ticked = no longer confirms at `level`; the modal opens while it still
        // does, so the box starts unticked and reflects the flip after a click.
        let checked = !self.confirm_policy().requires(level);
        Self::dont_ask_checkbox_el(id, level, checked, cx.theme(), cx.entity().downgrade())
    }

    /// The checkbox itself, over a [`gpui::WeakEntity`] rather than `&self` + a live
    /// `cx`, so the Mongo confirm (built off a weak handle deep in its render
    /// chain, where no `Context` is in hand) shares this one builder instead of
    /// forking a copy. The `&self` [`dont_ask_destructive_checkbox`](Self::dont_ask_destructive_checkbox) above is the
    /// thin wrapper for the SQL/Redis confirms that do have a `cx`.
    pub(crate) fn dont_ask_checkbox_el(
        id: &'static str,
        level: RiskLevel,
        checked: bool,
        theme: &flint::Theme,
        view: gpui::WeakEntity<AppState>,
    ) -> gpui::AnyElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Checkbox::new(id, checked)
                    .mark(crate::icons::icon("check", px(12.), theme.on_accent))
                    // "Don't ask again" ticked means *stop* asking, hence the negation.
                    .on_change(move |checked: &bool, _, cx| {
                        view.update(cx, |this, cx| this.set_confirms_at(level, !*checked, cx))
                            .ok();
                    }),
            )
            .child(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text_muted)
                    .child(crate::i18n::tr!("common.dont_ask_again", "Don't ask again")),
            )
            .into_any_element()
    }

    /// Confirmation before deleting a saved connection. Deletion also drops the
    /// keychain credential, so this is the safety rail against accidental removal.
    fn render_confirm_delete(
        &self,
        name: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();
        let body = div().text_color(theme.text_muted).child(format!(
            "“{name}” and its saved password will be removed. This can't be undone."
        ));
        let footer = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("delete-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_delete_connection(cx))),
            )
            .child(
                Button::new("delete-confirm", "Delete connection")
                    .variant(ButtonVariant::Danger)
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_delete_connection(cx))),
            );
        Modal::new("confirm-delete-conn")
            .title(crate::i18n::tr!(
                "connect.confirm_delete",
                "Delete connection"
            ))
            .width(px(420.))
            .focus_handle(self.modal_focus.clone())
            .footer(footer)
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.cancel_delete_connection(cx))
                    .ok();
            })
            .on_confirm(move |_, cx| {
                confirm_view
                    .update(cx, |this, cx| this.confirm_delete_connection(cx))
                    .ok();
            })
            .child(body)
    }

    /// Confirmation modal for "Remove all RED data" — the factory reset. Spells out
    /// exactly what's about to go (the two directories, the connection + AI-key
    /// secret sets, that the binary is untouched) and that it can't be undone, so a
    /// destructive irreversible action is never one stray click away.
    fn render_confirm_reset(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();
        let conn_count = self.connections.len();
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_color(theme.text_muted).child(
                "This permanently removes everything RED stored on this machine and \
                 can't be undone:",
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pl_2()
                    .text_color(theme.text_muted)
                    .child(div().child(format!(
                        "• {conn_count} saved connection(s) and their keychain secrets \
                         (passwords, SSH keys)"
                    )))
                    .child(div().child(crate::i18n::tr!(
                        "settings.reset_item_ai_keys",
                        "• AI provider API keys in the keychain"
                    )))
                    .child(div().child(
                        "• the config and cached-data directories (settings, history, \
                         saved queries, themes)",
                    )),
            )
            .child(div().text_color(theme.text_muted).child(crate::i18n::tr!(
                "settings.reset_binary_kept",
                "The RED application binary itself is not removed."
            )));
        let footer = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("reset-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_reset(cx))),
            )
            .child(
                Button::new("reset-confirm", "Remove all RED data")
                    .variant(ButtonVariant::Danger)
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_reset_run(cx))),
            );
        Modal::new("confirm-reset")
            .title(crate::i18n::tr!(
                "settings.reset_title",
                "Remove all RED data"
            ))
            .width(px(460.))
            .focus_handle(self.modal_focus.clone())
            .footer(footer)
            .on_close(move |_, cx| {
                close_view.update(cx, |this, cx| this.cancel_reset(cx)).ok();
            })
            .on_confirm(move |_, cx| {
                confirm_view
                    .update(cx, |this, cx| this.confirm_reset_run(cx))
                    .ok();
            })
            .child(body)
    }

    /// Confirmation modal for deleting a Redis key straight from a browse list's
    /// right-click menu (see [`AppState::kv_request_delete_key`]). Enter deletes,
    /// Esc / Cancel backs out — the destructive action gets an explicit prompt
    /// rather than the inspector's quieter inline confirm bar.
    fn render_kv_confirm_delete(
        &self,
        key: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_color(theme.text_muted).child(
                crate::i18n::tr!("kv.confirm_delete_key_body", "This key and its value will be permanently deleted from Redis. This can't be undone."),
            ))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded(theme.radius_sm)
                    .bg(theme.bg_panel)
                    .border_1()
                    .border_color(theme.border)
                    .font_family(theme.mono_family.clone())
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .truncate()
                    .child(key),
            );
        let footer = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("kv-delete-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.kv_cancel_delete_key(cx))),
            )
            .child(
                Button::new("kv-delete-confirm", "Delete key")
                    .variant(ButtonVariant::Danger)
                    .on_click(cx.listener(|this, _, _, cx| this.kv_confirm_delete_key(cx))),
            );
        Modal::new("confirm-kv-delete")
            .title(crate::i18n::tr!("kv.confirm_delete_key", "Delete key"))
            .width(px(440.))
            .focus_handle(self.modal_focus.clone())
            .footer(footer)
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.kv_cancel_delete_key(cx))
                    .ok();
            })
            .on_confirm(move |_, cx| {
                confirm_view
                    .update(cx, |this, cx| this.kv_confirm_delete_key(cx))
                    .ok();
            })
            .child(body)
    }

    /// The keyboard-shortcuts reference overlay (`⌘/`). Built from
    /// [`crate::keymap::shortcuts`] so it never drifts from the real bindings.
    fn render_shortcuts(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let mut body = div().flex().flex_col().gap_4();
        for (title, rows) in crate::keymap::shortcuts() {
            let mut section = div().flex().flex_col().gap_1().child(
                div()
                    .pb_1()
                    .text_size(theme.scale(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_faint)
                    .child(title.to_uppercase()),
            );
            for (keys, desc) in rows {
                section = section.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(div().text_color(theme.text_muted).child(desc))
                        .child(
                            div()
                                .flex_shrink_0()
                                .font_family(theme.mono_family.clone())
                                .text_size(theme.scale(11.5))
                                .text_color(theme.text)
                                .child(crate::keymap::localize_hint(keys)),
                        ),
                );
            }
            body = body.child(section);
        }
        Modal::new("keyboard-shortcuts")
            .title(crate::i18n::tr!("shortcuts.title", "Keyboard shortcuts"))
            .width(px(460.))
            .focus_handle(self.modal_focus.clone())
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.toggle_shortcuts(cx))
                    .ok();
            })
            .child(body)
    }

    /// The danger card: an alert-marked panel carrying what the grading noticed and
    /// how many rows are at stake.
    ///
    /// One card rather than loose lines because the reasons and the row count are
    /// the same claim at two resolutions ("this removes every row in orders" /
    /// "1,142 rows"), and reading them as one block is what makes the magnitude
    /// land. Everything in here is a fact RED established itself, which is why it
    /// gets the red accent and the assistant's bubble below does not.
    fn render_risk_card(
        &self,
        assessment: &red_core::sql::Assessment,
        theme: &flint::Theme,
    ) -> Option<gpui::AnyElement> {
        let reasons: Vec<String> = assessment.risks.iter().map(describe_risk).collect();
        let count = self.preflight_line(assessment);
        if reasons.is_empty() && count.is_none() {
            return None;
        }
        let mut lines = div().flex().flex_col().gap_1().flex_1().min_w(px(0.));
        for reason in reasons {
            lines = lines.child(
                div()
                    .text_size(theme.scale(12.5))
                    .text_color(theme.text)
                    .child(reason),
            );
        }
        if let Some((text, emphatic)) = count {
            lines = lines.child(
                div()
                    .text_size(theme.scale(12.5))
                    .text_color(if emphatic {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .child(text),
            );
        }
        Some(
            div()
                .flex()
                .items_start()
                .gap_2()
                .p(px(10.))
                .rounded(theme.radius)
                .bg(theme.red.opacity(0.08))
                .border_l_2()
                .border_color(theme.red)
                .child(div().flex_none().pt(px(1.)).child(crate::icons::icon(
                    "alert-triangle",
                    theme.scale(14.),
                    theme.red,
                )))
                .child(lines)
                .into_any_element(),
        )
    }

    /// The row-count preflight as text, plus whether it deserves full-strength
    /// colour. `None` when no preflight was sent.
    ///
    /// A real figure is emphatic; "counting…", "unavailable", and a zero count are
    /// muted. Zero especially: it is the strongest evidence the statement is *not*
    /// the one the user is worried about, so it should not shout.
    fn preflight_line(&self, assessment: &red_core::sql::Assessment) -> Option<(String, bool)> {
        use red_core::sql::Risk;
        // A `DROP` does not "affect" rows so much as take them with it; say what is
        // in the table rather than implying a row-level operation.
        let drops = assessment
            .risks
            .iter()
            .any(|r| matches!(r, Risk::Drops { .. }));
        let table = assessment.table.as_deref().unwrap_or("this table");
        Some(match self.confirm_count? {
            PreflightCount::Pending => ("Counting affected rows…".to_string(), false),
            PreflightCount::Unavailable => ("Row count unavailable.".to_string(), false),
            PreflightCount::Rows(0) if drops => (format!("{table} is empty."), false),
            PreflightCount::Rows(0) => ("This affects no rows.".to_string(), false),
            PreflightCount::Rows(n) if drops => (
                format!(
                    "{table} holds {} rows.",
                    crate::result::group_digits(n.max(0) as usize)
                ),
                true,
            ),
            PreflightCount::Rows(n) => (
                format!(
                    "This affects {} rows.",
                    crate::result::group_digits(n.max(0) as usize)
                ),
                true,
            ),
        })
    }

    /// The assistant's advisory note, as an attributed message bubble.
    ///
    /// Styled as a distinct surface with the agent's name in its header rather than
    /// as another line of dialog text, because it is the one thing here RED did not
    /// establish itself. A reader should be able to tell at a glance which claims
    /// are the app's and which are a model's, and a bubble says "someone said this"
    /// in a way a sentence cannot.
    ///
    /// Every outcome renders. An earlier version showed nothing for "no concern"
    /// and nothing for "couldn't ask", which made a working review look identical
    /// to a broken one: "asking the assistant…" simply vanished. Note what is still
    /// *not* rendered: any claim that the statement is safe. `NoConcern` speaks
    /// about the assistant, never about the SQL.
    fn render_ai_review(&self, theme: &flint::Theme) -> Option<gpui::AnyElement> {
        let review = self.confirm_review.as_ref()?;
        let (text, muted) = match &review.state {
            AiReviewState::Pending => ("Reviewing…".to_string(), true),
            AiReviewState::Concern(note) => (note.clone(), false),
            AiReviewState::NoConcern => ("Nothing to add.".to_string(), true),
            AiReviewState::Unavailable(why) => (format!("Couldn't review this: {why}."), true),
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p(px(10.))
                .rounded(theme.radius)
                // The one square corner points at the speaker, the way a chat
                // bubble's tail does.
                .rounded_tl(px(2.))
                .bg(theme.bg_elevated)
                .border_1()
                .border_color(theme.border_soft)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(crate::icons::icon(
                            "sparkles",
                            theme.scale(12.),
                            theme.accent,
                        ))
                        .child(
                            div()
                                .text_size(theme.scale(11.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.accent)
                                .child(review.agent.clone()),
                        ),
                )
                .child(
                    div()
                        .text_size(theme.scale(12.5))
                        .text_color(if muted { theme.text_muted } else { theme.text })
                        .child(text),
                )
                .into_any_element(),
        )
    }

    /// The type-to-confirm box, when the pending write armed one. `None` otherwise,
    /// which is every confirmation below `Critical`.
    fn render_type_to_confirm(&self, theme: &flint::Theme) -> Option<gpui::AnyElement> {
        let typed = self.confirm_input.as_ref()?;
        // Set apart from the rest of the body: everything above is something to
        // read, this is the one thing to *do*. A caret sitting in an unlabelled box
        // under a wall of warnings is easy to miss, and missing it reads as a broken
        // dialog with a dead button.
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .pt_1()
                .border_t_1()
                .border_color(theme.border_soft)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(crate::icons::icon(
                            "lock",
                            theme.scale(12.),
                            theme.text_muted,
                        ))
                        .child(
                            div()
                                .text_size(theme.scale(12.5))
                                .text_color(theme.text_muted)
                                .child(format!("Type “{}” to confirm", typed.expect)),
                        ),
                )
                .child(typed.input.clone())
                .into_any_element(),
        )
    }

    /// The three caveats a best-effort submit carries, above the statements.
    ///
    /// These are not decoration. Under this contract the writes are asynchronous, so
    /// the grid may not show them the instant Submit returns; there is no
    /// transaction, so a failure halfway leaves the earlier changes in place; and the
    /// engine rewrites data *by part*, so a one-cell edit on a large table can be an
    /// expensive operation. A dialog that said only "are you sure" would be hiding all
    /// three.
    fn render_best_effort_card(
        &self,
        plan: &[red_core::OpPlan],
        theme: &flint::Theme,
    ) -> gpui::AnyElement {
        // Only a *mutation* carries the cost caveat; an insert is an ordinary write.
        let mutations = plan
            .iter()
            .filter(|p| p.blocked.is_none() && matches!(p.verb, "Update" | "Delete"))
            .count();
        let mut lines = vec![
            "Applied one statement at a time: a failure partway leaves the earlier \
             changes in place."
                .to_string(),
        ];
        if mutations > 0 {
            lines.push(
                "Updates and deletes are asynchronous mutations. ClickHouse rewrites \
                 data by part, so this can cost far more than the rows it changes."
                    .to_string(),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded(theme.radius_sm)
            .bg(theme.bg_input)
            .border_1()
            .border_color(theme.border_soft)
            .text_size(theme.scale(11.5))
            .text_color(theme.text_muted)
            .children(lines.into_iter().map(|line| {
                div()
                    .flex()
                    .items_start()
                    .gap_1p5()
                    .child(crate::icons::icon(
                        "alert-triangle",
                        theme.scale(12.),
                        theme.text_muted,
                    ))
                    .child(div().child(line))
            }))
            .into_any_element()
    }

    /// The explicit "apply to all N rows" acknowledgement, shown only when the
    /// preflight found an op whose identity matches more than one row.
    ///
    /// ClickHouse has no unique row address, so an identity that matches several rows
    /// is a real possibility rather than a bug, and the difference between "change
    /// this row" and "change these four" is not something to infer. Ticking this is
    /// what turns those ops from refused into runnable.
    fn render_apply_to_all(
        &self,
        plan: &[red_core::OpPlan],
        acknowledged: bool,
        theme: &flint::Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let extra: u64 = plan
            .iter()
            .filter_map(|p| p.matches.filter(|n| *n > 1))
            .sum();
        if extra == 0 {
            return None;
        }
        let view = cx.entity().downgrade();
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Checkbox::new("batch-apply-to-all", acknowledged)
                        .mark(crate::icons::icon("check", px(12.), theme.on_accent))
                        .on_change(move |checked: &bool, _, cx| {
                            let checked = *checked;
                            view.update(cx, |this, cx| this.set_apply_to_all(checked, cx))
                                .ok();
                        }),
                )
                .child(
                    div()
                        .text_size(theme.scale(12.5))
                        .text_color(theme.text)
                        .child(format!("Apply to all {extra} matching rows, not just one")),
                )
                .into_any_element(),
        )
    }

    /// The destructive-statement confirmation modal: the write safety rail.
    fn render_confirm(
        &self,
        pending: crate::app::PendingWrite,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        use crate::app::PendingWrite;
        // Owned so the body can still reference it after `dont_ask` borrows `cx`.
        let theme = cx.theme().clone();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();
        // The destructive editor statement and the guarded grid edit share this
        // modal; only the title, prose, preview text, and button label differ.
        let (title, prose, sql, run_label): (&str, String, String, &str) = match &pending {
            PendingWrite::EditorSql { sql, assessment } => (
                match assessment.level {
                    RiskLevel::Critical => "This destroys data",
                    _ => "Confirm this statement",
                },
                // The reasons below say what was noticed; the prose only has to say
                // what is being asked, so it does not restate them.
                match assessment.level {
                    RiskLevel::Critical => "This can't be undone.".to_string(),
                    _ => "This statement changes more than it names. Run it?".to_string(),
                },
                sql.clone(),
                "Run statement",
            ),
            // A script says how many statements it is up front: the count is the
            // fact the single-statement dialog has no need to state, and the one
            // that changes what the user is agreeing to.
            PendingWrite::Script {
                statements,
                assessment,
            } => (
                match assessment.level {
                    RiskLevel::Critical => "This script destroys data",
                    _ => "Confirm this script",
                },
                format!(
                    "{} statements, run in order and reported one by one. \
                     This is not one transaction: statements that succeed before a \
                     failure stay applied.",
                    statements.len()
                ),
                statements.join(";\n"),
                "Run script",
            ),
            // The atomic contract: one transaction, all or nothing. The generic
            // `preview_sql` is enough here because the driver binds the same op and
            // rolls the whole batch back if any of it surprises us.
            PendingWrite::Batch {
                ops,
                mode: BatchMode::Atomic,
                ..
            } => {
                let n = ops.len();
                let prose = if n == 1 {
                    "This will apply 1 staged change in a single transaction. Submit it?"
                        .to_string()
                } else {
                    format!(
                        "This will apply {n} staged changes in a single transaction. Submit them?"
                    )
                };
                let combined = ops
                    .iter()
                    .map(|op| op.preview_sql())
                    .collect::<Vec<_>>()
                    .join(";\n");
                ("Submit changes", prose, combined, "Submit")
            }
            // The best-effort contract: the preflight's real statements, so what is
            // approved is what runs. Blocked ops are listed and will be skipped.
            PendingWrite::Batch { plan, .. } => {
                let runnable = plan.iter().filter(|p| p.blocked.is_none()).count();
                let blocked = plan.len() - runnable;
                let mut prose = match runnable {
                    0 => "None of these can run as staged.".to_string(),
                    1 => "This will apply 1 staged change, one statement at a time.".to_string(),
                    n => format!("This will apply {n} staged changes, one statement at a time."),
                };
                if blocked > 0 {
                    prose.push_str(&format!(" {blocked} can't run and will be skipped."));
                }
                let combined = plan
                    .iter()
                    .map(|p| match (&p.blocked, p.matches) {
                        (Some(reason), _) => format!("-- skipped, {reason}\n{}", p.sql),
                        (None, Some(1)) => p.sql.clone(),
                        (None, Some(n)) => format!("-- matches {n} rows\n{}", p.sql),
                        (None, None) => p.sql.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(";\n");
                ("Submit changes", prose, combined, "Submit")
            }
            PendingWrite::Import { prose, preview, .. } => {
                ("Confirm import", prose.clone(), preview.clone(), "Import")
            }
            PendingWrite::Copy { prose, preview, .. } => {
                ("Copy to table", prose.clone(), preview.clone(), "Append")
            }
            PendingWrite::KillSession {
                mode, who, query, ..
            } => (
                match mode {
                    red_core::KillMode::Cancel => "Stop this query",
                    red_core::KillMode::Terminate => "Terminate this session",
                },
                match mode {
                    red_core::KillMode::Cancel => {
                        format!(
                            "This stops the statement {who} is running. The session stays open."
                        )
                    }
                    red_core::KillMode::Terminate => format!(
                        "This drops {who}'s whole session and rolls back its open \
                         transaction. It cannot be undone."
                    ),
                },
                query.clone(),
                mode.verb(),
            ),
        };
        // A copy offers two actions, Append (keep the target's rows) and Replace all
        // (truncate first, behind the danger styling), rather than one run button.
        let is_copy = matches!(&pending, PendingWrite::Copy { .. });
        // The batch preview can be many statements; show more than a single edit's
        // one-liner but still cap it so a huge change-set can't blow up the modal.
        let preview: String = sql.chars().take(1200).collect();
        // What the grading actually noticed, above the SQL. A modal that only says
        // "are you sure" teaches nothing and gets dismissed; one that says "no WHERE
        // clause: this rewrites every row in orders" is a different question.
        let risk_card = match &pending {
            PendingWrite::EditorSql { assessment, .. }
            | PendingWrite::Script { assessment, .. } => self.render_risk_card(assessment, &theme),
            _ => None,
        };
        // The "Don't ask again" opt-out belongs only on the settings-gated editor
        // path, and there only below `Critical`: silencing the routine case must not
        // silence `DROP TABLE`, which is exactly what the old single switch did. To
        // stop being asked about a drop you have to say so in settings. A production
        // connection withholds it entirely (`allow_quiet`), so the moment of hurry is
        // never when the guard comes off.
        let policy = self.confirm_policy();
        let dont_ask = match &pending {
            PendingWrite::EditorSql { assessment, .. }
                if policy.allow_quiet && assessment.level < RiskLevel::Critical =>
            {
                Some(self.dont_ask_destructive_checkbox(
                    "destructive-dont-ask",
                    assessment.level,
                    cx,
                ))
            }
            _ => None,
        };
        // The best-effort caveats and the "apply to all N rows" acknowledgement. Both
        // are specific to a contract that can't promise what the atomic one does, so
        // neither appears on the relational path.
        let (best_effort_card, apply_to_all) = match &pending {
            PendingWrite::Batch {
                mode: BatchMode::BestEffort { allow_multi_match },
                plan,
                ..
            } => (
                Some(self.render_best_effort_card(plan, &theme)),
                self.render_apply_to_all(plan, *allow_multi_match, &theme, cx),
            ),
            _ => (None, None),
        };
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(theme.scale(12.5))
                    .text_color(theme.text_muted)
                    .child(prose),
            )
            .children(risk_card)
            .children(best_effort_card)
            .children(self.render_ai_review(&theme))
            .child(
                // The statement itself, framed as a quoted artefact rather than more
                // dialog prose: the border and the mono face say "this is the thing,
                // verbatim", which matters when the point is to re-read it.
                div()
                    .p_2()
                    .rounded(theme.radius_sm)
                    .bg(theme.bg_input)
                    .border_1()
                    .border_color(theme.border_soft)
                    .font_family(theme.mono_family.clone())
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(preview),
            )
            .children(self.render_type_to_confirm(&theme))
            .children(apply_to_all)
            .children(dont_ask);
        let mut footer = div().flex().justify_end().gap_2().child(
            Button::new("confirm-cancel", "Cancel")
                .variant(ButtonVariant::Secondary)
                .on_click(cx.listener(|this, _, _, cx| this.cancel_destructive(cx))),
        );
        if is_copy {
            footer = footer
                .child(
                    Button::new("confirm-copy-replace", "Replace all")
                        .variant(ButtonVariant::Danger)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_copy(CopyMode::TruncateInsert, cx)
                        })),
                )
                .child(
                    // Enter (the modal's confirm) also runs Append, the safe default.
                    Button::new("confirm-copy-append", run_label)
                        .variant(ButtonVariant::Primary)
                        .on_click(
                            cx.listener(|this, _, _, cx| this.confirm_copy(CopyMode::Append, cx)),
                        ),
                );
        } else {
            footer = footer.child(
                Button::new("confirm-run", run_label)
                    .variant(ButtonVariant::Danger)
                    // Stays disabled until the object's name has been typed, when the
                    // grade called for that. `confirm_destructive` re-checks, so this
                    // is the affordance rather than the guarantee.
                    .disabled(!self.confirm_target_matches(cx) || !self.confirm_has_work())
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_destructive(cx))),
            );
        }
        Modal::new("confirm-destructive")
            .title(title)
            .width(px(440.))
            .focus_handle(self.modal_focus.clone())
            .footer(footer)
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.cancel_destructive(cx))
                    .ok();
            })
            .on_confirm(move |_, cx| {
                confirm_view
                    .update(cx, |this, cx| this.confirm_destructive(cx))
                    .ok();
            })
            .child(body)
    }
}

/// One [`Risk`] as a phrase for the confirm modal.
///
/// The wording lives here rather than on the type because `red-core` has no business
/// holding user-facing copy: the grading is data, and this is the one place that
/// decides how to say it.
///
/// [`Risk`]: red_core::sql::Risk
fn describe_risk(risk: &red_core::sql::Risk) -> String {
    use red_core::sql::{DropKind, MutateVerb, Risk};

    // "in orders" when the table could be named, "in the table" when it could not.
    let in_table = |table: &Option<String>| match table {
        Some(name) => format!("in {name}"),
        None => "in the table".to_string(),
    };
    let mutates = |verb: &MutateVerb| match verb {
        MutateVerb::Update => "rewrites",
        MutateVerb::Delete => "removes",
    };
    match risk {
        Risk::WholeTable { verb, table } => format!(
            "No WHERE clause: this {} every row {}.",
            mutates(verb),
            in_table(table)
        ),
        Risk::AlwaysTrue { verb, table } => format!(
            "The WHERE clause is always true, so this {} every row {}.",
            mutates(verb),
            in_table(table)
        ),
        Risk::Drops { object, name } => {
            let what = match object {
                DropKind::Table => "table",
                DropKind::Database => "database",
                DropKind::Schema => "schema",
                DropKind::View => "view",
                DropKind::Index => "index",
                DropKind::Other => "object",
            };
            match name {
                Some(name) => format!("Drops the {what} {name} and everything in it."),
                None => format!("Drops a whole {what}."),
            }
        }
        Risk::Truncates { table } => format!("Empties every row {}.", in_table(table)),
        Risk::DropsColumn { table } => format!(
            "Drops a column or constraint {}, discarding what it held.",
            in_table(table)
        ),
        Risk::PrivilegeChange => "Changes who can access this database.".to_string(),
        Risk::OpaqueExecution => {
            "Runs stored code, so what it changes can't be checked here.".to_string()
        }
        Risk::Merge { table } => format!("A MERGE can delete rows {}.", in_table(table)),
        Risk::DataModifyingCte => {
            "A CTE in this query writes, so it isn't the read it looks like.".to_string()
        }
        // 1-based for display: the user is counting statements, not indexing them.
        Risk::HiddenInBatch { index, total } => format!(
            "This is statement {} of {total}, so it's easy to miss.",
            index + 1
        ),
    }
}

#[cfg(feature = "dev-stats")]
impl AppState {
    /// The dev perf HUD overlay: a small bottom-right mono panel with the budget
    /// readouts (build time, allocs/frame, live + RSS bytes, the grid footprint).
    /// `None` while toggled off. Kept deliberately trivial: building it allocates
    /// and takes time, so it lightly perturbs its own reading (see the plan).
    fn render_dev_panel(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.dev_stats.visible() {
            return None;
        }
        let theme = cx.theme();
        let ds = &self.dev_stats;
        let mb = |bytes: usize| format!("{} MB", bytes / (1024 * 1024));
        let rss = ds.rss().map(&mb).unwrap_or_else(|| "—".into());
        // `gap` is the interval between renders, the repaint cadence during
        // interaction. Idle is notify-gated (no frame stream), so a large gap at
        // rest is correct, not a stall (see the plan's fps caveat).
        let line1 = format!(
            "build {:.2} ms · gap {:.0} ms · {:.0} allocs/f · live {} · rss {}",
            ds.build_ms(),
            ds.interval_ms(),
            ds.allocs_per_frame(),
            mb(ds.live_bytes()),
            rss,
        );

        let grid = match &self.phase {
            Phase::Connected(active) => active.active_result().map(|g| g.dev_snapshot()),
            _ => None,
        };
        let line2 = match grid {
            Some(g) => format!(
                "grid {} rows · {} · {} in-flight · q {:.0} ms",
                crate::result::group_digits(g.resident_rows),
                g.mode,
                g.in_flight,
                g.last_query_ms,
            ),
            None => "grid —".to_string(),
        };

        Some(
            div()
                .absolute()
                .bottom_2()
                .right_2()
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(theme.radius_sm)
                .bg(theme.bg_panel)
                .border_1()
                .border_color(theme.border)
                .font_family(theme.mono_family.clone())
                .text_size(theme.scale(10.))
                .text_color(theme.text_muted)
                .child(div().child(line1))
                .child(div().text_color(theme.text_faint).child(line2))
                .into_any_element(),
        )
    }
}
