//! Root rendering: the `Render` impl that picks the top-level screen, the
//! connecting splash, and the two confirmation modals (destructive statement,
//! close-with-unsaved-work).

use flint::prelude::*;
use gpui::{div, prelude::*, px, ClipboardItem, Focusable, KeyDownEvent, Render, Window};
use red_core::CopyMode;

use super::{AppState, ConnectStatus, Connecting, Pane, Phase};
use crate::keymap::{
    About, AddRow, BeginEdit, CloseInspector, CloseTab, CycleFocusNext, CycleFocusPrev, DeleteRow,
    Explain, FindInResult, FocusEditor, FocusGrid, FocusOtherHalf, FocusSchema, FormatSql,
    NewConnection, NewTab, NextTab, OpenSavedQueries, PrevTab, RefreshSchema, ReportBug,
    RevertChanges, RunQuery, SaveQuery, SearchSchema, SelectAll, SetNull, Settings, ShowChangelog,
    ShowErDiagram, ShowShortcuts, SubmitChanges, SwitchConnection, SwitchToConnectionSlot,
    SwitchToPreviousConnection, ToggleAssistant, ToggleColumnsPanel, ToggleFilter, ToggleHistory,
    ToggleInspector, ToggleSidebar, ToggleSplit,
};
use crate::palette::{CopyResult, GoToRow, ToggleCommandPalette};

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
    ) -> impl IntoElement {
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
                        .child("Verify the fingerprint, then trust this host to add it to ~/.ssh/known_hosts."),
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
            if let Phase::Connected(active) = &self.phase {
                if let Some(name) = active
                    .kv_view
                    .as_ref()
                    .and_then(|v| v.active_browse())
                    .and_then(|b| b.create_key.as_ref())
                    .map(|ck| ck.name.clone())
                {
                    window.focus(&name.focus_handle(cx), cx);
                }
            }
        }

        // The history popover just opened: focus it so its arrow keys work.
        if self.focus_history {
            self.focus_history = false;
            if let Phase::Connected(active) = &self.phase {
                window.focus(&active.history_focus.clone(), cx);
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

        // ⌘⇧F: the result filter bar just opened; focus its input to type at once.
        if self.focus_filter {
            self.focus_filter = false;
            if let Some(bar) = &self.filter_bar {
                window.focus(&bar.input.focus_handle(cx), cx);
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

        // An inline cell edit just opened in the inspector (Track B5); focus its
        // field so the user types the new value immediately.
        if self.focus_inspector_edit {
            self.focus_inspector_edit = false;
            if let Some(handle) = self.inspector_edit_focus(cx) {
                window.focus(&handle, cx);
            }
        }

        // An inline cell edit just opened in the grid (Track B6); focus its field.
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
            if self.grid_edit_blur.is_none() {
                if let Some(handle) = self.grid_edit_focus(cx) {
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
            window.focus(&self.modal_focus.clone(), cx);
        }

        // Focus trap: while a modal is open, a focus-out listener on `modal_focus`
        // pulls focus back inside if Tab would carry it to the backdrop. Registered
        // once when a modal opens (the modal's panel is a descendant of
        // `modal_focus`), and dropped (unsubscribing) when it closes.
        if self.any_modal_open() {
            if self.modal_focus_trap.is_none() {
                let handle = self.modal_focus.clone();
                let weak = cx.entity().downgrade();
                let sub = window.on_focus_out(&handle.clone(), cx, move |_event, window, cx| {
                    // Re-enter only while a modal is genuinely still open (not mid-
                    // close) and focus actually left the modal subtree.
                    let open = weak.upgrade().is_some_and(|e| e.read(cx).any_modal_open());
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
            // in R1, see docs/plans/redis.md) — a dedicated minimal shell
            // instead of the SQL workspace's editor/grid/schema tree, which
            // all assume a `DatabaseDriver` session.
            Phase::Connected(active) if active.config.kind == red_core::DbKind::Redis => self
                .render_redis_shell(active, window, cx)
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
            .map(|pending| self.render_confirm(pending, cx));

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

        let settings = self
            .settings_open
            .then(|| self.render_settings(cx).into_any_element());

        let shortcuts = self.shortcuts_open.then(|| self.render_shortcuts(cx));

        let whats_new = self.whats_new_open.then(|| self.render_whats_new(cx));

        let import_wizard = self
            .import_wizard
            .as_ref()
            .map(|w| self.render_import_wizard(w, cx));

        // The read-only ER diagram is a full-screen overlay hung off the connection
        // (schema-wide), so it renders whenever the active connection has one open.
        let er_diagram = match &self.phase {
            Phase::Connected(active) if active.er.is_some() => Some(self.render_er(active, cx)),
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
            .on_action(cx.listener(|this, _: &ToggleCommandPalette, _, cx| this.toggle_palette(cx)))
            .on_action(cx.listener(|this, _: &SwitchConnection, window, cx| {
                this.toggle_switcher(window, cx)
            }))
            // ⌘⇧P flips to the previous connection; ⌘1–9 jump to the n-th in the
            // switcher's order. True globals (like ⌘P), so they fire from any focus.
            .on_action(cx.listener(|this, _: &SwitchToPreviousConnection, _, cx| {
                this.switch_to_previous(cx)
            }))
            .on_action(cx.listener(|this, action: &SwitchToConnectionSlot, _, cx| {
                this.switch_to_slot(action.0, cx)
            }))
            .on_action(cx.listener(|this, _: &GoToRow, _, cx| this.open_goto_prompt(cx)))
            .on_action(cx.listener(|this, _: &CopyResult, _, cx| this.copy_result_selection(cx)))
            // ⌘I toggles the cell detail inspector; Esc dismisses the topmost
            // transient overlay: an open dropdown / cell menu first, then the
            // inspector (no-op when nothing is open).
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| this.toggle_inspector(cx)))
            .on_action(cx.listener(|this, _: &CloseInspector, _, cx| this.dismiss_overlay(cx)))
            .on_action(cx.listener(|this, _: &ToggleAssistant, window, cx| {
                this.toggle_assistant(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleFilter, _, cx| this.toggle_filter_bar(cx)))
            .on_action(
                cx.listener(|this, _: &FindInResult, window, cx| this.toggle_find_bar(window, cx)),
            )
            // Saved queries (B3): ⇧⌘S opens the name prompt; ⇧⌘O the picker.
            .on_action(cx.listener(|this, _: &SaveQuery, _, cx| this.open_save_prompt(cx)))
            .on_action(cx.listener(|this, _: &OpenSavedQueries, _, cx| this.open_saved_picker(cx)))
            // EXPLAIN (B4): ⇧⌘E opens the plan view for the active query.
            .on_action(cx.listener(|this, _: &Explain, _, cx| this.explain_query(false, cx)))
            // Beautify the editor's SQL in place (⌥⌘F).
            .on_action(cx.listener(|this, _: &FormatSql, _, cx| this.format_active_sql(cx)))
            // App-chrome actions (tabs · sidebar · schema reload), bound in the
            // central keymap to `RedRoot` so they fire from any pane's focus.
            .on_action(cx.listener(|this, _: &NewTab, _, cx| this.new_query(cx)))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| this.close_active_tab(cx)))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.next_tab(window, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| this.prev_tab(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleHistory, _, cx| this.toggle_history(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleColumnsPanel, _, cx| this.toggle_columns_panel(cx)),
            )
            .on_action(cx.listener(|this, _: &RefreshSchema, _, cx| this.refresh_active(cx)))
            .on_action(cx.listener(|this, _: &SearchSchema, _, cx| {
                this.focus_search = true;
                cx.notify();
            }))
            // Pane focus jumps + cycle.
            .on_action(cx.listener(|this, _: &FocusSchema, window, cx| {
                this.focus_pane(Pane::Schema, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusEditor, window, cx| {
                this.focus_pane(Pane::Editor, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusGrid, window, cx| {
                this.focus_pane(Pane::Grid, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CycleFocusNext, window, cx| {
                this.cycle_focus(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CycleFocusPrev, window, cx| {
                this.cycle_focus(false, window, cx)
            }))
            // Side-by-side split (⌘\ toggles it, ⌥⌘\ jumps to the other half).
            .on_action(cx.listener(|this, _: &ToggleSplit, _, cx| this.toggle_split(cx)))
            .on_action(cx.listener(|this, _: &FocusOtherHalf, _, cx| this.focus_other_half(cx)))
            .on_action(cx.listener(|this, _: &ShowShortcuts, _, cx| this.toggle_shortcuts(cx)))
            .on_action(cx.listener(|this, _: &ShowChangelog, _, cx| this.toggle_whats_new(cx)))
            .on_action(cx.listener(|this, _: &ShowErDiagram, _, cx| this.open_er_diagram(cx)))
            // Settings panel: ⌘, and the RED → Settings… / About RED menu items.
            .on_action(cx.listener(|this, _: &Settings, _, cx| this.open_settings(cx)))
            .on_action(cx.listener(|this, _: &About, _, cx| this.open_about(cx)))
            // Help → Report a Bug…: open the issue tracker in the browser.
            .on_action(cx.listener(|this, _: &ReportBug, _, cx| {
                this.open_external(crate::app::ISSUES_URL, cx)
            }))
            // --- staged grid editing (Track B6) ---
            // Enter/F2 in the "Table" context: on a Redis key list it opens the
            // value inspector on the keyboard cursor; otherwise it begins an
            // in-place SQL cell edit (the same binding, the right thing per pane).
            .on_action(cx.listener(|this, _: &BeginEdit, window, cx| {
                if !this.kv_activate_cursor(window, cx) {
                    this.begin_grid_edit(cx);
                }
            }))
            // ⌘↵ in the grid submits staged changes; with nothing staged it falls
            // through to running the active query (so the key still does the
            // expected thing on a clean grid).
            .on_action(cx.listener(|this, _: &SubmitChanges, _, cx| {
                if this.has_pending_changes() {
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
                if this.form.is_some() {
                    this.test_connection(cx);
                } else {
                    this.run_editor_query(cx);
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
                    // E edits the highlighted connection, ⌫/⌦ asks to remove it;
                    // the keyboard mirrors the hover edit/trash buttons on each card.
                    "e" if !search_focused => {
                        cx.stop_propagation();
                        this.open_edit_form(visible[sel], cx);
                    }
                    "backspace" | "delete" if !search_focused => {
                        cx.stop_propagation();
                        this.request_delete_connection(visible[sel], cx);
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
            .children(shortcuts)
            .children(whats_new)
            .children(import_wizard)
            .children(er_diagram)
            // The connection form modal is rendered at the root so it works in any
            // phase (the welcome screen *and* the connected shell, e.g. opened from
            // the switcher's "New connection…").
            .children(self.form.as_ref().map(|f| self.render_form(f, cx)))
            // The Redis "New key" modal, rooted here so it overlays the whole
            // shell (not just the browse pane) like the other modals.
            .children(self.render_kv_create_modal(cx))
            // The Redis "Import keys" modal, likewise root-mounted.
            .children(self.render_kv_import_modal(cx))
            // The palette renders its own full-screen overlay; last = on top.
            .children(self.palette.as_ref().map(|(p, _)| p.clone()))
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
            // The in-cell FK suggestion dropdown (Track B8) anchors to the editor
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
                    .child("Downloading update…")
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
    fn render_notifications(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
    fn render_confirm_close(&self, title: String, cx: &mut Context<Self>) -> impl IntoElement {
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
            .title("Close tab")
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
    fn render_confirm_close_batch(&self, count: usize, cx: &mut Context<Self>) -> impl IntoElement {
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
            .title("Close tabs")
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
    fn dont_ask_close_tab_checkbox(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Checkbox::new("close-tab-dont-ask", false)
                    .mark(crate::icons::icon("check", px(12.), theme.on_accent))
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_confirm_close_tab(!checked, cx);
                    })),
            )
            .child(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text_muted)
                    .child("Don't ask again"),
            )
    }

    /// Confirmation before deleting a saved connection. Deletion also drops the
    /// keychain credential, so this is the safety rail against accidental removal.
    fn render_confirm_delete(&self, name: String, cx: &mut Context<Self>) -> impl IntoElement {
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
            .title("Delete connection")
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

    /// Confirmation modal for deleting a Redis key straight from a browse list's
    /// right-click menu (see [`AppState::kv_request_delete_key`]). Enter deletes,
    /// Esc / Cancel backs out — the destructive action gets an explicit prompt
    /// rather than the inspector's quieter inline confirm bar.
    fn render_kv_confirm_delete(&self, key: String, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_color(theme.text_muted).child(
                "This key and its value will be permanently deleted from Redis. This can't be undone.",
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
            .title("Delete key")
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
    fn render_shortcuts(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .title("Keyboard shortcuts")
            .width(px(460.))
            .focus_handle(self.modal_focus.clone())
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.toggle_shortcuts(cx))
                    .ok();
            })
            .child(body)
    }

    /// The destructive-statement confirmation modal: the write safety rail.
    fn render_confirm(
        &self,
        pending: crate::app::PendingWrite,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use crate::app::PendingWrite;
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();
        // The destructive editor statement and the guarded grid edit share this
        // modal; only the title, prose, preview text, and button label differ.
        let (title, prose, sql, run_label): (&str, String, String, &str) = match &pending {
            PendingWrite::EditorSql(sql) => (
                "Confirm destructive statement",
                "This statement modifies data and can't be undone. Run it?".to_string(),
                sql.clone(),
                "Run statement",
            ),
            PendingWrite::Batch { ops, .. } => {
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
            PendingWrite::Import { prose, preview, .. } => {
                ("Confirm import", prose.clone(), preview.clone(), "Import")
            }
            PendingWrite::Copy { prose, preview, .. } => {
                ("Copy to table", prose.clone(), preview.clone(), "Append")
            }
        };
        // A copy offers two actions, Append (keep the target's rows) and Replace all
        // (truncate first, behind the danger styling), rather than one run button.
        let is_copy = matches!(&pending, PendingWrite::Copy { .. });
        // The batch preview can be many statements; show more than a single edit's
        // one-liner but still cap it so a huge change-set can't blow up the modal.
        let preview: String = sql.chars().take(1200).collect();
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_color(theme.text_muted).child(prose))
            .child(
                div()
                    .p_2()
                    .rounded(theme.radius_sm)
                    .bg(theme.bg_input)
                    .font_family(theme.mono_family.clone())
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(preview),
            );
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
