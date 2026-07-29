//! Settings, appearance, and self-update UI: installing the live-reload +
//! OS-appearance observers, reloading `settings.toml`, the theme/font pickers,
//! every `set_*` settings mutator, the file-first open-settings workflow, and
//! the updater controls. Split out of `mod.rs`.

use super::*;
use crate::settings::ConfirmThreshold;
use red_core::sql::RiskLevel;

impl AppState {
    // --- settings: live observers ---

    /// Install the OS-appearance observer and the `settings.toml` file-watcher on
    /// the first render, when a `Window` is available. The appearance observer
    /// keeps `mode = system` honest; the watcher re-applies hand edits live.
    pub(crate) fn ensure_observers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.observers_installed {
            return;
        }
        self.observers_installed = true;

        let weak = cx.entity().downgrade();
        let sub = window.observe_window_appearance(move |window, cx| {
            let dark = matches!(
                window.appearance(),
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            );
            weak.update(cx, |this, cx| {
                if dark != this.os_dark {
                    this.os_dark = dark;
                    this.apply_theme(cx);
                    cx.notify();
                }
            })
            .ok();
        });
        self.appearance_sub = Some(sub);

        if let Some(store) = &self.settings_store
            && let Some((watcher, mut rx)) =
                crate::settings_watch::SettingsWatcher::start(store.path().to_path_buf())
        {
            self.settings_watcher = Some(watcher);
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                while rx.next().await.is_some() {
                    if this
                        .update(cx, |this, cx| this.reload_settings(cx))
                        .is_err()
                    {
                        break; // view dropped (window closed)
                    }
                }
            })
            .detach();
        }

        // A second watcher over `keymap.toml`, reusing the same debounce + self-
        // write suppression. A hand-edit re-applies the whole keymap live.
        if let Some(store) = &self.keymap_store
            && let Some((watcher, mut rx)) =
                crate::settings_watch::SettingsWatcher::start(store.path().to_path_buf())
        {
            self.keymap_watcher = Some(watcher);
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                while rx.next().await.is_some() {
                    if this.update(cx, |this, cx| this.reload_keymap(cx)).is_err() {
                        break; // view dropped (window closed)
                    }
                }
            })
            .detach();
        }

        // A third watcher over `connections.toml`, so editing the saved-connection
        // file by hand (the welcome screen's "Edit file" affordance) re-reads the
        // list live; the same debounce + self-write suppression as the others.
        if let Some(path) = crate::config::config_path()
            && let Some((watcher, mut rx)) = crate::settings_watch::SettingsWatcher::start(path)
        {
            self.connections_watcher = Some(watcher);
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                while rx.next().await.is_some() {
                    if this
                        .update(cx, |this, cx| this.reload_connections(cx))
                        .is_err()
                    {
                        break; // view dropped (window closed)
                    }
                }
            })
            .detach();
        }

        // Reconnect to the most recently used connection on launch, when the user
        // opted in. The list arrives recency-sorted (newest first), so the first
        // entry that's actually been opened is the one to restore; credentials come
        // from the keychain inside `connect`.
        if self.settings.behavior.restore_last_session
            && matches!(self.phase, Phase::Disconnected)
            && let Some(index) = self
                .connections
                .iter()
                .position(|c| c.last_accessed.is_some())
        {
            self.connect(index, cx);
        }
    }

    // --- settings: the one mutation path ---

    /// **The** way settings change in-app: mutate through `f`, clamp, persist, and
    /// apply whatever the change implies.
    ///
    /// Every knob goes through here rather than through a bespoke setter, because
    /// the side effects are the part that's easy to forget: a statement timeout
    /// that isn't pushed to the backend, a cell cap the driver never hears about,
    /// an updater left armed at the old cadence. [`apply_settings_effects`] derives
    /// all of that from a before/after diff, so a new setting inherits correct
    /// behaviour by construction and the file-watcher path
    /// ([`reload_settings`](Self::reload_settings)) runs the *same* code as the
    /// panel instead of a hand-maintained copy of it.
    ///
    /// A no-op edit (a segmented control re-selecting its current value) writes
    /// nothing and repaints nothing.
    pub(crate) fn edit_settings(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Settings)) {
        let before = self.settings.clone();
        f(&mut self.settings);
        // Clamp here, not in each caller: `Settings::clamp` is the single
        // definition of a valid value, shared with the load path.
        self.settings.clamp();
        if self.settings == before {
            return;
        }
        self.save_settings();
        self.apply_settings_effects(&before, cx);
        cx.notify();
    }

    /// Everything that has to *happen* when settings change, derived from what
    /// actually moved between `before` and the current values.
    ///
    /// Each arm is guarded on its own section so an unrelated edit (a font size,
    /// say) doesn't re-arm the updater or re-push the AI config. Shared by the
    /// in-app edit path and the file watcher, which is the whole point: there is
    /// no way to change a setting and skip its consequence.
    fn apply_settings_effects(&mut self, before: &Settings, cx: &mut Context<Self>) {
        // Before the theme arm: the locale feeds every string the next render
        // resolves, so switching it after a re-render would leave one frame in
        // the old language.
        if before.appearance.locale != self.settings.appearance.locale {
            crate::i18n::apply(&self.settings.appearance.locale);
        }
        // Theme + typography are one visual unit; the mono family and the UI size
        // are theme tokens, so any appearance change reinstalls the theme.
        if before.appearance != self.settings.appearance {
            self.apply_theme(cx);
        }
        if before.sql.statement_timeout != self.settings.sql.statement_timeout {
            self.service
                .send_global(Command::SetStatementTimeout(self.settings.sql.timeout()));
        }
        if before.data.max_cell_chars != self.settings.data.max_cell_chars {
            self.service.send_global(Command::SetDisplayCellCap(
                self.settings.data.max_cell_chars,
            ));
        }
        if before.update != self.settings.update {
            // The backend only re-polls if the cadence actually moved.
            self.service
                .send_global(Command::ConfigureUpdates(update_config(&self.settings)));
        }
        if before.ai != self.settings.ai {
            // Recompute the usable-agent list the panel selector draws from before
            // pushing, so an edited `[[ai.agents]]` is reflected in both at once.
            self.usable_agents = crate::app::usable_agents(&self.settings);
            self.ai_configured = !self.usable_agents.is_empty();
            self.service
                .send_global(Command::ConfigureAi(crate::app::ai_config(&self.settings)));
            // If that just flipped the master switch off (M-S7), close any open
            // panel so the kill switch is immediate.
            if self.assistant.is_some() && !self.ai_enabled() {
                self.assistant = None;
            }
        }
        if before.data.row_numbers != self.settings.data.row_numbers {
            // The gutter is column `0` in the grid's coordinate system, so flipping
            // it shifts the data-column offset; clear the selection (stored in
            // table-column coords) so it can't point off by one.
            if let Phase::Connected(active) = &mut self.phase
                && let Some(grid) = active.active_result_mut()
            {
                grid.clear_selection();
            }
        }
    }

    /// Re-read `settings.toml` after an external edit and re-apply. Runs the same
    /// [`apply_settings_effects`](Self::apply_settings_effects) diff as an in-app
    /// edit, so a hand-edit can't reach a state the panel couldn't. Per-frame
    /// settings (density, null display, page size) take effect on the next render
    /// via `cx.notify`.
    pub(crate) fn reload_settings(&mut self, cx: &mut Context<Self>) {
        let Some(store) = &self.settings_store else {
            return;
        };
        let report = store.load_report();
        let before = std::mem::replace(&mut self.settings, report.settings);
        self.settings_warnings = report.warnings;
        // Push the reloaded sizes into the steppers so a hand-edit of the file is
        // reflected in the panel (set_value doesn't emit, so no write-back loop).
        // Only on this path: in-app edits come *from* the steppers, and writing
        // back mid-keystroke would fight the user's typing.
        let ui_size = self.settings.appearance.ui_font_size as f64;
        let editor_size = self.settings.editor.font_size as f64;
        self.ui_font_size_input
            .update(cx, |n, cx| n.set_value(ui_size, cx));
        self.editor_font_size_input
            .update(cx, |n, cx| n.set_value(editor_size, cx));
        self.apply_settings_effects(&before, cx);
        cx.notify();
    }

    /// Re-read `keymap.toml` after an external edit and re-apply the whole keymap
    /// (defaults + overrides). Reuses [`crate::keymap::apply`], so a removed or
    /// fixed override reverts cleanly to the default: no stale binding lingers.
    pub(crate) fn reload_keymap(&mut self, cx: &mut Context<Self>) {
        let Some(store) = &self.keymap_store else {
            return;
        };
        let report = store.load_report();
        let mut warnings = report.warnings;
        warnings.extend(crate::keymap::apply(cx, &report.blocks));
        self.keymap_warnings = warnings;
        cx.notify();
    }

    /// Re-read `connections.toml` after an external edit and swap in the new list.
    /// Only the saved-connection roster changes; live/parked sessions are keyed by
    /// `SessionId`, not list index, so a connected workspace is untouched. The
    /// welcome-screen selection is clamped in case the edit shortened the list.
    pub(crate) fn reload_connections(&mut self, cx: &mut Context<Self>) {
        self.connections = config::load();
        let max = self.connections.len().saturating_sub(1);
        self.connect_sel = self.connect_sel.min(max);
        cx.notify();
    }

    /// Force an update check now ("Check for updates" in the About tab). A no-op
    /// in effect when `auto_update = false` (the backend ignores `CheckNow`
    /// while disabled), so the button is only offered when updates are on.
    pub(crate) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.service.send_global(Command::CheckForUpdate);
        cx.notify();
    }

    /// Relaunch into the freshly-staged build (Phase 4). The new bundle is already
    /// swapped over `/Applications/RED.app`, so this just spawns it and exits;
    /// macOS replaces the running process with the new version.
    #[cfg(target_os = "macos")]
    pub(crate) fn restart_for_update(&mut self, _cx: &mut Context<Self>) {
        let app = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.ancestors().nth(3).map(std::path::Path::to_path_buf));
        if let Some(app) = app {
            // `open -n` launches a fresh instance of the swapped bundle; we then
            // exit so only the new version remains.
            let _ = std::process::Command::new("/usr/bin/open")
                .arg("-n")
                .arg(&app)
                .spawn();
        }
        std::process::exit(0);
    }

    /// Linux (AppImage): the new image was renamed over `$APPIMAGE`, so relaunch
    /// that path and exit, leaving only the new version. Falls back to the current
    /// exe if `$APPIMAGE` is somehow unset (it always is under the AppImage runtime
    /// that the updater requires before staging).
    #[cfg(target_os = "linux")]
    pub(crate) fn restart_for_update(&mut self, _cx: &mut Context<Self>) {
        let target = std::env::var_os("APPIMAGE")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_exe().ok());
        if let Some(target) = target {
            let _ = std::process::Command::new(target).spawn();
        }
        std::process::exit(0);
    }

    /// Windows (portable): the new exe was moved into the running exe's path (the
    /// old one renamed to `<exe>.old`, reaped on next launch). Relaunch and exit so
    /// only the new version remains.
    #[cfg(target_os = "windows")]
    pub(crate) fn restart_for_update(&mut self, _cx: &mut Context<Self>) {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).spawn();
        }
        std::process::exit(0);
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    pub(crate) fn restart_for_update(&mut self, _cx: &mut Context<Self>) {}

    /// Store the updater's latest state and, when a build has finished staging,
    /// surface a one-off toast so the user notices the pill. Other transitions
    /// (checking, up-to-date, background failures) stay quiet; they're visible
    /// in the About tab without nagging.
    pub(crate) fn on_update_state(&mut self, state: UpdateState, cx: &mut Context<Self>) {
        let became_ready = matches!(state, UpdateState::ReadyToRestart { .. })
            && !matches!(self.update, UpdateState::ReadyToRestart { .. });
        self.update = state;
        if became_ready {
            self.notify(
                ToastVariant::Success,
                "An update is ready. Restart to apply it.",
                cx,
            );
        }
    }

    // --- settings: file-first workflow ---

    /// Open `settings.toml` in the user's editor, seeding it with the commented
    /// reference defaults on first open so there's a full key set to edit.
    pub(crate) fn open_settings_file(&mut self, cx: &mut Context<Self>) {
        let Some(store) = &self.settings_store else {
            self.notify(
                ToastVariant::Error,
                "No config directory available on this platform.",
                cx,
            );
            return;
        };
        let path = store.path().to_path_buf();
        if !path.exists() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Announce the seed so the watcher doesn't echo it back as an edit.
            if let Some(watcher) = &self.settings_watcher {
                watcher.note_self_write(crate::assets::DEFAULT_SETTINGS);
            }
            if let Err(e) = std::fs::write(&path, crate::assets::DEFAULT_SETTINGS) {
                tracing::warn!("failed to seed settings file: {e}");
            }
        }
        self.reveal_path(&path, cx);
    }

    /// Open `keymap.toml` in the user's editor, seeding it with the commented
    /// reference on first open so there's a full key list + format to edit from.
    pub(crate) fn open_keymap_file(&mut self, cx: &mut Context<Self>) {
        let Some(store) = &self.keymap_store else {
            self.notify(
                ToastVariant::Error,
                "No config directory available on this platform.",
                cx,
            );
            return;
        };
        let path = store.path().to_path_buf();
        if !path.exists() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Announce the seed so the watcher doesn't echo it back as an edit.
            if let Some(watcher) = &self.keymap_watcher {
                watcher.note_self_write(crate::assets::DEFAULT_KEYMAP);
            }
            if let Err(e) = std::fs::write(&path, crate::assets::DEFAULT_KEYMAP) {
                tracing::warn!("failed to seed keymap file: {e}");
            }
        }
        self.reveal_path(&path, cx);
    }

    /// Open `connections.toml` in the user's editor, the file-first counterpart to
    /// the welcome screen's connection cards. The file is written from the current
    /// in-memory list first ([`Self::persist`] announces the write to the watcher,
    /// so it doesn't echo back as a reload) so there's always real content to edit,
    /// even on a fresh install with no saved connections yet.
    pub(crate) fn open_connections_file(&mut self, cx: &mut Context<Self>) {
        let Some(path) = crate::config::config_path() else {
            self.notify(
                ToastVariant::Error,
                "No config directory available on this platform.",
                cx,
            );
            return;
        };
        self.persist(cx);
        self.reveal_path(&path, cx);
    }

    /// Open the bundled, fully-commented reference defaults: RED's settings docs.
    pub(crate) fn open_default_settings(&mut self, cx: &mut Context<Self>) {
        let path = std::env::temp_dir().join("red-default-settings.toml");
        if let Err(e) = std::fs::write(&path, crate::assets::DEFAULT_SETTINGS) {
            tracing::warn!("failed to materialize default settings: {e}");
            self.notify(
                ToastVariant::Error,
                format!("Couldn't open default settings: {e}"),
                cx,
            );
            return;
        }
        self.reveal_path(&path, cx);
    }

    /// Hand `path` to the OS to open with its default handler (best-effort).
    pub(crate) fn reveal_path(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        if let Err(e) = open_in_os(path) {
            tracing::warn!("failed to open {}: {e}", path.display());
            self.notify(
                ToastVariant::Error,
                format!("Couldn't open {}: {e}", path.display()),
                cx,
            );
        }
    }

    /// Show `path` selected in the OS file manager (best-effort). The export
    /// toast's "Show in folder" action.
    pub(crate) fn reveal_in_file_manager(
        &mut self,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = crate::app::reveal_in_file_manager(path) {
            tracing::warn!("failed to reveal {}: {e}", path.display());
            self.notify(
                ToastVariant::Error,
                format!("Couldn't show {} in the file manager: {e}", path.display()),
                cx,
            );
        }
        cx.notify();
    }

    // --- settings panel ---

    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = true;
        // Warm the font-name cache once, off the render path (the Appearance tab
        // would otherwise re-enumerate every installed face on every frame).
        if self.font_names_cache.is_none() {
            let mut names = cx.text_system().all_font_names();
            names.sort_unstable();
            names.dedup();
            self.font_names_cache = Some(names);
        }
        // Now that the font cache is warm, fill the Appearance dropdowns.
        self.rebuild_settings_pickers(cx);
        // If we open straight onto the AI tab (its last-used tab), learn who is
        // signed in on each ACP agent so the rows can show identity.
        if self.settings_tab == SettingsTab::Ai {
            self.refresh_acp_auth(false);
        }
        // Focus the panel so its Esc-to-close is heard and Tab walks its controls
        // (the next render focuses `modal_focus`, the panel's scrim ancestor).
        self.focus_modal = true;
        cx.notify();
    }

    /// Fill the five Appearance dropdowns with the current themes + installed fonts
    /// and mark the active option. Called when the settings panel opens and after
    /// the theme registry changes (import/remove). The font list is read from the
    /// warmed cache (see [`Self::open_settings`]), never re-enumerated here.
    pub(crate) fn rebuild_settings_pickers(&mut self, cx: &mut Context<Self>) {
        for (light, combo) in [
            (true, self.theme_combo_light.clone()),
            (false, self.theme_combo_dark.clone()),
        ] {
            let names = self.themes.names(light);
            let current = self.selected_theme(light);
            let selected = names.iter().position(|n| *n == current);
            let options = names.into_iter().map(Into::into).collect();
            combo.update(cx, |c, cx| c.set_options(options, selected, cx));
        }

        for which in [FontSelect::Ui, FontSelect::UiMono, FontSelect::Editor] {
            let current = match which {
                FontSelect::Ui => self.settings.appearance.ui_font_family.clone(),
                FontSelect::UiMono => self.settings.appearance.ui_mono_family.clone(),
                FontSelect::Editor => self.settings.editor.font_family.clone(),
            };
            // Keep the configured family selectable even if it isn't installed (a
            // settings file referencing a font from another machine).
            let mut names = self.font_names().to_vec();
            if !names.contains(&current) {
                names.insert(0, current.clone());
            }
            let selected = names.iter().position(|n| *n == current);
            let options = names.into_iter().map(Into::into).collect();
            let combo = match which {
                FontSelect::Ui => self.font_combo_ui.clone(),
                FontSelect::UiMono => self.font_combo_ui_mono.clone(),
                FontSelect::Editor => self.font_combo_editor.clone(),
            };
            combo.update(cx, |c, cx| c.set_options(options, selected, cx));
        }
    }

    /// Open the settings panel on its About tab (the RED → About RED menu item).
    /// There's no standalone About modal yet; the panel's About tab is it.
    pub(crate) fn open_about(&mut self, cx: &mut Context<Self>) {
        self.open_settings(cx);
        self.set_settings_tab(SettingsTab::About, cx);
    }

    /// The cached sorted/deduped installed font families (see [`Self::open_settings`]).
    pub(crate) fn font_names(&self) -> &[String] {
        self.font_names_cache.as_deref().unwrap_or(&[])
    }

    pub(crate) fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        // Drop the search, so reopening lands on a category page rather than on
        // whatever was typed last time.
        self.settings_search
            .update(cx, |i, cx| i.set_content("", cx));
        // Tear down the keymap recorder if a capture was mid-flight; a leaked
        // keystroke interceptor would otherwise eat every keypress app-wide.
        self.keymap_recording = None;
        self.keymap_intercept = None;
        self.keymap_capture = None;
        // Return focus to the root so the app stays keyboard-driven after closing.
        self.refocus_root = true;
        cx.notify();
    }

    pub(crate) fn set_settings_tab(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        self.settings_tab = tab;
        // Picking a category means "show me that page", so it ends the search;
        // otherwise the results view would keep winning and the click would look
        // like it did nothing.
        self.settings_search
            .update(cx, |i, cx| i.set_content("", cx));
        // Entering the AI tab: learn who is signed in on each ACP agent (lazy: only
        // agents not yet checked), so the rows can show identity.
        if tab == SettingsTab::Ai {
            self.refresh_acp_auth(false);
        }
        // Leaving (or re-entering) a tab ends any in-flight keymap capture, so the
        // recorder's interceptor never outlives the Keymap tab being visible.
        self.keymap_recording = None;
        self.keymap_intercept = None;
        self.keymap_capture = None;
        // Start each category at the top, and drop any stale reveal capture from
        // the tab we're leaving.
        self.settings_scroll.set_offset(gpui::point(px(0.), px(0.)));
        *self.settings_focus_box.borrow_mut() = None;
        cx.notify();
    }

    /// Which reveal-able Appearance control (if any) holds keyboard focus now,
    /// checked in nav order. The dropdowns report focus in either state via
    /// [`ComboBox::is_focused`]; the size inputs via their text field.
    pub(crate) fn focused_reveal(&self, window: &Window, cx: &gpui::App) -> Option<RevealTarget> {
        let combos = [
            (RevealTarget::ThemeLight, &self.theme_combo_light),
            (RevealTarget::ThemeDark, &self.theme_combo_dark),
            (RevealTarget::FontUi, &self.font_combo_ui),
            (RevealTarget::FontUiMono, &self.font_combo_ui_mono),
            (RevealTarget::FontEditor, &self.font_combo_editor),
        ];
        for (target, combo) in combos {
            if combo.read(cx).is_focused(window, cx) {
                return Some(target);
            }
        }
        for (target, input) in [
            (RevealTarget::UiSize, &self.ui_font_size_input),
            (RevealTarget::EditorSize, &self.editor_font_size_input),
        ] {
            if input.read(cx).focus_handle(cx).is_focused(window) {
                return Some(target);
            }
        }
        None
    }

    /// On each settings render, scroll the content pane the minimal amount to bring
    /// the focused reveal-able control fully into view, only when it's actually
    /// off-screen, so tabbing between already-visible controls never jumps. The
    /// focused control tags its window-space bounds via a canvas overlay (see
    /// [`crate::settings_ui`]'s `reveal_wrap`); we read the previous frame's capture
    /// and act a frame after focus lands; the `target` tag rejects a stale box left
    /// by the control we just moved off.
    pub(crate) fn update_settings_scroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_open {
            self.settings_focused_reveal = None;
            return;
        }
        let focused = self.focused_reveal(window, cx);
        self.settings_focused_reveal = focused;
        let Some(target) = focused else { return };

        // Wait a frame if the capture isn't this control's yet (its canvas hasn't
        // painted since focus moved).
        let Some((tagged, rb)) = *self.settings_focus_box.borrow() else {
            cx.notify();
            return;
        };
        if tagged != target {
            cx.notify();
            return;
        }

        let vp = self.settings_scroll.bounds();
        if vp.size.height <= px(0.) {
            return;
        }
        let offset = self.settings_scroll.offset();
        let pad = px(8.);
        let mut new_y = offset.y;
        if rb.size.height >= vp.size.height {
            // Taller than the viewport: just align its top (aligning both edges is
            // impossible and would oscillate). Idempotent once the top is in place.
            if (rb.origin.y - vp.origin.y).as_f32().abs() > 1.0 {
                new_y += vp.origin.y - rb.origin.y;
            }
        } else if rb.origin.y < vp.origin.y {
            // Above the fold: bring its top into view.
            new_y += vp.origin.y - rb.origin.y + pad;
        } else if rb.origin.y + rb.size.height > vp.origin.y + vp.size.height {
            // Below the fold: bring its bottom into view.
            new_y -= rb.origin.y + rb.size.height - (vp.origin.y + vp.size.height) + pad;
        }
        // Clamp into the valid scroll range ([-max, 0]).
        let max = self.settings_scroll.max_offset().y;
        let new_y = new_y.clamp(-max, px(0.));
        if (new_y.as_f32() - offset.y.as_f32()).abs() > 0.5 {
            self.settings_scroll.set_offset(gpui::point(px(0.), new_y));
            cx.notify();
        }
    }

    /// Re-resolve the active theme from settings + OS appearance and install it.
    pub(crate) fn apply_theme(&self, cx: &mut Context<Self>) {
        cx.set_global(crate::theme::with_typography(
            self.themes
                .resolve(&self.settings.appearance.theme, self.os_dark),
            &self.settings.appearance,
        ));
        cx.set_global(flint::ReduceMotion(self.settings.appearance.reduce_motion));
    }

    /// The `(mode, light, dark)` the current setting implies. The panel always
    /// edits a light/dark pair, so a bare named theme is decomposed into one
    /// (filling the other slot from the registry's default for that family).
    pub(crate) fn theme_decompose(&self) -> (ThemeMode, String, String) {
        match &self.settings.appearance.theme {
            ThemeSetting::Modal { mode, light, dark } => (*mode, light.clone(), dark.clone()),
            ThemeSetting::Named(name) if self.themes.is_light(name) => (
                ThemeMode::Light,
                name.clone(),
                self.themes.default_name(false),
            ),
            ThemeSetting::Named(name) => (
                ThemeMode::Dark,
                self.themes.default_name(true),
                name.clone(),
            ),
        }
    }

    /// The active appearance mode (System / Light / Dark); drives the segmented.
    pub(crate) fn theme_mode(&self) -> ThemeMode {
        self.theme_decompose().0
    }

    /// The currently-selected theme name for a family, which drives the pickers.
    pub(crate) fn selected_theme(&self, light: bool) -> String {
        let (_, l, d) = self.theme_decompose();
        if light { l } else { d }
    }

    /// Store a full `(mode, light, dark)` pair, apply it, and persist.
    pub(crate) fn set_theme_pair(
        &mut self,
        mode: ThemeMode,
        light: String,
        dark: String,
        cx: &mut Context<Self>,
    ) {
        self.edit_settings(cx, |s| {
            s.appearance.theme = ThemeSetting::Modal { mode, light, dark };
        });
    }

    /// Switch how the theme tracks the OS: `System` follows the OS light/dark,
    /// `Light`/`Dark` pin that family. The pair carries across so the user's two
    /// choices survive a mode flip.
    pub(crate) fn set_theme_mode(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        let (_, light, dark) = self.theme_decompose();
        self.set_theme_pair(mode, light, dark, cx);
    }

    /// Choose the light-appearance theme (used in Light and System-on-light modes).
    pub(crate) fn set_light_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let (mode, _, dark) = self.theme_decompose();
        self.set_theme_pair(mode, name.to_string(), dark, cx);
    }

    /// Choose the dark-appearance theme (used in Dark and System-on-dark modes).
    pub(crate) fn set_dark_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let (mode, light, _) = self.theme_decompose();
        self.set_theme_pair(mode, light, name.to_string(), cx);
    }

    /// Pick a theme file from disk, validate + copy it into the user themes dir,
    /// then reload the registry. Async (the native file dialog runs off-thread).
    pub(crate) fn import_theme(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import theme".into()),
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| this.finish_import(&path, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Land an imported theme file: refresh the registry and re-apply (in case the
    /// import re-skinned the active theme). Toasts success or the validation error.
    pub(crate) fn finish_import(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        match ThemeRegistry::import(path) {
            Ok(name) => {
                self.themes = ThemeRegistry::load();
                self.apply_theme(cx);
                self.rebuild_settings_pickers(cx);
                self.notify(
                    ToastVariant::Success,
                    format!("Imported theme “{name}”"),
                    cx,
                );
            }
            Err(e) => {
                self.notify(
                    ToastVariant::Error,
                    format!("Couldn't import theme: {e}"),
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// Delete a user theme, reload the registry, and re-apply; a removed active
    /// theme falls back to the default rather than leaving a dangling reference.
    pub(crate) fn remove_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Err(e) = self.themes.remove(name) {
            self.notify(
                ToastVariant::Error,
                format!("Couldn't remove theme: {e}"),
                cx,
            );
            return;
        }
        self.themes = ThemeRegistry::load();
        self.apply_theme(cx);
        self.rebuild_settings_pickers(cx);
        self.notify(ToastVariant::Success, format!("Removed theme “{name}”"), cx);
    }

    /// Make `id` the default agent: the one a new chat opens on, and the one the
    /// confirmation dialog asks for a review. Persists to `[ai] default_agent`.
    pub(crate) fn set_default_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        let id = id.to_string();
        self.edit_settings(cx, |s| s.ai.default_agent = id);
    }

    /// The "Don't ask again" checkbox on a confirmation modal: start or stop asking
    /// about actions graded `level`, by moving the threshold to `level` or just past
    /// it.
    ///
    /// Takes the level being confirmed rather than flipping one switch, so silencing
    /// what is in front of you cannot silence something worse. The SQL modal only
    /// offers the checkbox below `Critical` and so can never reach `Never`; the Redis
    /// and MongoDB delete confirms pass `Critical` (destroying data outright is what
    /// they do) and so still can, which preserves what their checkbox has always
    /// meant.
    ///
    /// Re-ticking is approximate: it restores asking at `level`, not whatever
    /// stricter threshold was set before. That only loses a setting the user is
    /// actively overriding in the same breath, and the direction is toward asking.
    pub(crate) fn set_confirms_at(&mut self, level: RiskLevel, ask: bool, cx: &mut Context<Self>) {
        let threshold = match (level, ask) {
            (RiskLevel::Safe | RiskLevel::Write, true) => ConfirmThreshold::Write,
            (RiskLevel::Risky, true) => ConfirmThreshold::Risky,
            (RiskLevel::Critical, true) => ConfirmThreshold::Critical,
            (RiskLevel::Safe | RiskLevel::Write, false) => ConfirmThreshold::Risky,
            (RiskLevel::Risky, false) => ConfirmThreshold::Critical,
            (RiskLevel::Critical, false) => ConfirmThreshold::Never,
        };
        self.edit_settings(cx, |s| s.safety.confirm_from = threshold);
    }

    /// The tab-close modal's "Don't ask again" checkbox: flips off the
    /// unsaved-work confirmation for every future tab close.
    pub(crate) fn set_confirm_close_tab(&mut self, on: bool, cx: &mut Context<Self>) {
        self.edit_settings(cx, |s| s.safety.confirm_close_tab = on);
    }

    /// Flip vim mode (the palette command). Toasts the new state so the change is
    /// legible when triggered without the Settings toggle in view.
    pub(crate) fn toggle_vim_mode(&mut self, cx: &mut Context<Self>) {
        let on = !self.settings.keymap.vim_mode;
        self.edit_settings(cx, |s| s.keymap.vim_mode = on);
        self.notify(
            ToastVariant::Info,
            if on {
                "Vim navigation on"
            } else {
                "Vim navigation off"
            },
            cx,
        );
    }

    /// Whether vim-style navigation is active, read by the grid/history key handlers.
    pub(crate) fn vim_mode(&self) -> bool {
        self.settings.keymap.vim_mode
    }

    // --- settings: AI assistant ---

    /// Open the inline key editor for an API agent's row (Settings → AI agents).
    /// Binds the shared `ai_key_input` to this agent id, clears it, and focuses it so
    /// the user types the key at once. A second click on the same row closes it.
    pub(crate) fn edit_agent_key(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.ai_key_editing.as_deref() == Some(id) {
            self.cancel_agent_key(cx);
            return;
        }
        self.ai_key_editing = Some(id.to_string());
        self.ai_key_input.update(cx, |i, cx| i.set_content("", cx));
        self.focus_ai_key = true;
        cx.notify();
    }

    /// Save the key in the open agent-key row to the OS keyring (under the agent's
    /// id), then recompute the usable-agent list and re-push the config so the
    /// backend builds that agent's provider. A blank key is treated as Cancel.
    pub(crate) fn save_agent_key(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.ai_key_editing.clone() else {
            return;
        };
        let key = self.ai_key_input.read(cx).content().trim().to_string();
        if key.is_empty() {
            self.cancel_agent_key(cx);
            return;
        }
        if let Err(e) = crate::secrets::set_ai_key(&id, &key) {
            tracing::warn!("failed to store AI key in keychain: {e}");
            self.notify(
                ToastVariant::Error,
                "Couldn't store the key in the keychain",
                cx,
            );
            return;
        }
        self.ai_key_input.update(cx, |i, cx| i.set_content("", cx));
        self.ai_key_editing = None;
        self.refresh_ai_agents();
        self.notify(ToastVariant::Success, "API key saved", cx);
        cx.notify();
    }

    /// Close the inline key editor without saving.
    pub(crate) fn cancel_agent_key(&mut self, cx: &mut Context<Self>) {
        self.ai_key_editing = None;
        self.ai_key_input.update(cx, |i, cx| i.set_content("", cx));
        cx.notify();
    }

    /// Remove an API agent's stored key from the keyring, then refresh the usable
    /// list so the agent drops back to "no key".
    pub(crate) fn clear_agent_key(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Err(e) = crate::secrets::delete_ai_key(id) {
            tracing::warn!("failed to remove AI key from keychain: {e}");
        }
        if self.ai_key_editing.as_deref() == Some(id) {
            self.cancel_agent_key(cx);
        }
        self.refresh_ai_agents();
        self.notify(ToastVariant::Info, "API key removed", cx);
        cx.notify();
    }

    /// Recompute the usable-agent list and re-push the AI config after a key change,
    /// so the panel selector and backend providers reflect it immediately.
    fn refresh_ai_agents(&mut self) {
        self.usable_agents = crate::app::usable_agents(&self.settings);
        self.ai_configured = !self.usable_agents.is_empty();
        self.service
            .send_global(Command::ConfigureAi(crate::app::ai_config(&self.settings)));
    }

    /// Start an interactive subscription sign-in (or account switch) for an ACP agent
    /// from Settings. The agent's bundled CLI runs a paste-code OAuth flow: it opens
    /// the browser, then the user pastes the code shown there. We open the inline
    /// prompt and ask the backend to begin; `AiLoginPrompt`/`AiLoginFinished` drive
    /// the rest. Session-less. A no-op for an API agent (those carry a key, not a
    /// login). RED never sees the OAuth tokens.
    pub(crate) fn reauthenticate_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        self.ai_login_code.update(cx, |i, cx| i.set_content("", cx));
        self.ai_login = Some(crate::app::AiLoginFlow {
            agent_id: id.to_string(),
            url: None,
            submitting: false,
            error: None,
        });
        self.service.send_global(Command::AiReauthenticateAgent {
            agent_id: id.to_string(),
        });
        self.notify(
            ToastVariant::Info,
            "Starting sign-in. A browser window will open.",
            cx,
        );
        cx.notify();
    }

    /// Submit the OAuth code the user pasted from the browser, completing the sign-in.
    /// Ignored when no sign-in is open or the field is empty.
    pub(crate) fn submit_login_code(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.ai_login.as_mut() else {
            return;
        };
        // The code can't be submitted before the browser URL is even known.
        if flow.url.is_none() {
            return;
        }
        let code = self.ai_login_code.read(cx).content().trim().to_string();
        if code.is_empty() {
            return;
        }
        let agent_id = flow.agent_id.clone();
        flow.submitting = true;
        flow.error = None;
        self.service
            .send_global(Command::AiSubmitLoginCode { agent_id, code });
        cx.notify();
    }

    /// Abandon an in-flight sign-in (the user dismissed the prompt). Tells the backend
    /// to kill the CLI and closes the inline panel.
    pub(crate) fn cancel_login(&mut self, cx: &mut Context<Self>) {
        if let Some(flow) = self.ai_login.take() {
            self.service.send_global(Command::AiCancelLogin {
                agent_id: flow.agent_id,
            });
            self.ai_login_code.update(cx, |i, cx| i.set_content("", cx));
            cx.notify();
        }
    }

    /// Sign out of an ACP agent's subscription. The backend clears the credential and
    /// re-checks status, which updates the row. A no-op for an API agent.
    pub(crate) fn sign_out_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        self.service.send_global(Command::AiSignOutAgent {
            agent_id: id.to_string(),
        });
        self.notify(ToastVariant::Info, "Signing out…", cx);
    }

    /// Ask the backend who is signed in on each usable ACP agent, so Settings → AI can
    /// show identity. Lazy by default (skips agents already checked); `force` re-asks
    /// (after a sign-in/out). Called when the AI tab is shown.
    pub(crate) fn refresh_acp_auth(&mut self, force: bool) {
        for agent in &self.usable_agents {
            if agent.is_acp && (force || !self.ai_auth.contains_key(&agent.id)) {
                self.service.send_global(Command::AiCheckAuthStatus {
                    agent_id: agent.id.clone(),
                });
            }
        }
    }

    /// The agent CLI opened the browser to `url` for sign-in (paste-code flow). Stash
    /// it on the open flow so the panel can offer a manual "open" fallback, and focus
    /// the code field.
    pub(crate) fn on_ai_login_prompt(
        &mut self,
        agent_id: String,
        url: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(flow) = self.ai_login.as_mut()
            && flow.agent_id == agent_id
        {
            flow.url = Some(url);
            self.focus_login_code = true;
            cx.notify();
        }
    }

    /// A sign-in finished. On success close the panel and refresh identity; on failure
    /// keep the panel open with the error so the user can re-paste.
    pub(crate) fn on_ai_login_finished(
        &mut self,
        agent_id: String,
        ok: bool,
        message: String,
        cx: &mut Context<Self>,
    ) {
        let is_current = self
            .ai_login
            .as_ref()
            .is_some_and(|f| f.agent_id == agent_id);
        if ok {
            if is_current {
                self.ai_login = None;
                self.ai_login_code.update(cx, |i, cx| i.set_content("", cx));
            }
            self.notify(ToastVariant::Success, "Signed in", cx);
            // Pull the freshly signed-in identity for the row.
            self.service
                .send_global(Command::AiCheckAuthStatus { agent_id });
        } else if is_current {
            // Keep the prompt open so the user can try the code again.
            if let Some(flow) = self.ai_login.as_mut() {
                flow.submitting = false;
                flow.error = Some(message.clone());
            }
            self.notify(
                ToastVariant::Error,
                format!("Sign-in failed: {message}"),
                cx,
            );
        }
        cx.notify();
    }

    /// Store an agent's refreshed sign-in identity for the Settings row.
    pub(crate) fn on_ai_agent_auth_status(
        &mut self,
        agent_id: String,
        status: AiAuthStatus,
        cx: &mut Context<Self>,
    ) {
        self.ai_auth.insert(agent_id, status);
        cx.notify();
    }

    /// Pick the folder generated reports are written to (Settings → AI agent). Async:
    /// the native directory dialog runs off-thread; the choice is persisted on return.
    /// Not pushed to the backend: the report folder rides in each turn's `AiContext`,
    /// so the next report already picks it up (a subscription chat already running
    /// keeps its captured folder until it restarts, like the report theme).
    pub(crate) fn pick_ai_report_dir(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose report folder".into()),
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| {
                    let dir = path.display().to_string();
                    this.edit_settings(cx, |s| s.ai.report_dir = dir);
                })
                .ok();
            }
        })
        .detach();
    }

    /// Clear the configured report folder, so reports fall back to the system temp dir.
    pub(crate) fn clear_ai_report_dir(&mut self, cx: &mut Context<Self>) {
        self.edit_settings(cx, |s| s.ai.report_dir.clear());
    }

    /// Open a URL (the update release page) with the OS default handler. Reuses
    /// the same shell-out seam as the settings-file workflow.
    pub(crate) fn open_external(&mut self, url: &str, cx: &mut Context<Self>) {
        if let Err(e) = open_in_os(std::path::Path::new(url)) {
            self.notify(ToastVariant::Error, format!("Couldn't open {url}: {e}"), cx);
        }
    }

    // The typography setters keep their own names because they're driven by custom
    // controls (searchable font combos, numeric steppers) rather than by a
    // registry row. Reinstalling the theme is the `[appearance]` arm of the
    // effects diff; the clamp is `Settings::clamp`.

    pub(crate) fn set_ui_font_family(&mut self, family: &str, cx: &mut Context<Self>) {
        let family = family.to_string();
        self.edit_settings(cx, |s| s.appearance.ui_font_family = family);
    }

    pub(crate) fn set_ui_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.edit_settings(cx, |s| s.appearance.ui_font_size = size);
    }

    pub(crate) fn set_ui_mono_family(&mut self, family: &str, cx: &mut Context<Self>) {
        let family = family.to_string();
        self.edit_settings(cx, |s| s.appearance.ui_mono_family = family);
    }

    pub(crate) fn set_editor_font_family(&mut self, family: &str, cx: &mut Context<Self>) {
        let family = family.to_string();
        self.edit_settings(cx, |s| s.editor.font_family = family);
    }

    pub(crate) fn set_editor_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.edit_settings(cx, |s| s.editor.font_size = size);
    }

    /// Dismiss the settings-warning banner until the next problematic load. Clears
    /// both settings and keymap warnings (they share one banner).
    pub(crate) fn dismiss_settings_warnings(&mut self, cx: &mut Context<Self>) {
        self.settings_warnings.clear();
        self.keymap_warnings.clear();
        cx.notify();
    }

    /// Persist the current preferences. A write failure is logged, not surfaced:
    /// preferences are convenience, and the in-memory value already took effect.
    /// The bytes are announced to the watcher first so the reload this write
    /// triggers is suppressed (no self-inflicted reload storm).
    pub(crate) fn save_settings(&self) {
        let Some(store) = &self.settings_store else {
            return;
        };
        if let Some(watcher) = &self.settings_watcher
            && let Ok(serialized) = toml::to_string_pretty(&self.settings)
        {
            watcher.note_self_write(&serialized);
        }
        if let Err(e) = store.save(&self.settings) {
            tracing::warn!("failed to save settings: {e}");
        }
    }

    /// Save the connection list, surfacing a write failure as a toast. The bytes
    /// are announced to the watcher first so this UI-driven save doesn't echo back
    /// through `connections.toml` as a reload (mirrors [`Self::save_settings`]).
    pub(crate) fn persist(&mut self, cx: &mut Context<Self>) {
        if let Some(watcher) = &self.connections_watcher
            && let Ok(text) = config::serialize(&self.connections)
        {
            watcher.note_self_write(&text);
        }
        if let Err(e) = config::save(&self.connections) {
            tracing::warn!("failed to save connections: {e}");
            self.notify(
                ToastVariant::Error,
                format!("Couldn't save connections: {e}"),
                cx,
            );
        }
    }

    // --- remove all RED data (factory reset) ---

    /// Arm the "Remove all RED data" confirmation modal. The destructive work runs
    /// only from [`confirm_reset`](Self::confirm_reset_run) after the user accepts.
    ///
    /// Takes focus, like every other keyboard-driven modal: it opens *over* the
    /// settings panel, which shares `modal_focus` with it, so without this the
    /// panel underneath keeps the keyboard and the confirmation's own Esc/Enter
    /// never fire.
    pub(crate) fn open_reset_confirm(&mut self, cx: &mut Context<Self>) {
        self.confirm_reset = true;
        self.focus_modal = true;
        cx.notify();
    }

    /// Dismiss the reset confirmation without doing anything, handing the keyboard
    /// back to the settings panel it opened over.
    pub(crate) fn cancel_reset(&mut self, cx: &mut Context<Self>) {
        self.confirm_reset = false;
        if self.settings_open {
            self.focus_modal = true;
        }
        cx.notify();
    }

    /// The user confirmed: wipe every RED directory and keychain secret off the UI
    /// thread (the keychain calls block), then act on the result. A clean run quits
    /// the app — continuing in a half-wiped process is asking for trouble — while a
    /// run with per-step errors keeps the process alive and surfaces them so the
    /// user can retry.
    pub(crate) fn confirm_reset_run(&mut self, cx: &mut Context<Self>) {
        self.confirm_reset = false;
        cx.notify();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let report = cx
                .background_executor()
                .spawn(async { crate::reset::remove_all_data() })
                .await;
            this.update(cx, |this, cx| {
                if report.errors.is_empty() {
                    // Everything's gone; there's nothing left to run against.
                    cx.quit();
                } else {
                    let detail = report.errors.join("\n");
                    this.notify_detail(
                        ToastVariant::Error,
                        crate::i18n::tr!(
                            "notify.reset_problems",
                            "Reset finished with {n} problem(s); some data may remain",
                            n = report.errors.len()
                        ),
                        detail,
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
    }
}
