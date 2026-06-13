//! Settings, appearance, and self-update UI: installing the live-reload +
//! OS-appearance observers, reloading `settings.toml`, the theme/font pickers,
//! every `set_*` settings mutator, the file-first open-settings workflow, and
//! the updater controls. Split out of `mod.rs`.

use super::*;

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

        if let Some(store) = &self.settings_store {
            if let Some((watcher, mut rx)) =
                crate::settings_watch::SettingsWatcher::start(store.path().to_path_buf())
            {
                self.settings_watcher = Some(watcher);
                cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                    while rx.next().await.is_some() {
                        if this
                            .update(cx, |this, cx| this.reload_settings(cx))
                            .is_err()
                        {
                            break; // view dropped — window closed
                        }
                    }
                })
                .detach();
            }
        }

        // Reconnect to the most recently used connection on launch, when the user
        // opted in. The list arrives recency-sorted (newest first), so the first
        // entry that's actually been opened is the one to restore; credentials come
        // from the keychain inside `connect`.
        if self.settings.behavior.restore_last_session && matches!(self.phase, Phase::Disconnected)
        {
            if let Some(index) = self
                .connections
                .iter()
                .position(|c| c.last_accessed.is_some())
            {
                self.connect(index, cx);
            }
        }
    }

    /// Re-read `settings.toml` after an external edit and re-apply. Theme is
    /// reinstalled here; per-frame settings (density, null display, page size)
    /// take effect on the next render via `cx.notify`.
    pub(crate) fn reload_settings(&mut self, cx: &mut Context<Self>) {
        let Some(store) = &self.settings_store else {
            return;
        };
        let report = store.load_report();
        self.settings = report.settings;
        self.settings_warnings = report.warnings;
        // Push the reloaded sizes into the steppers so a hand-edit of the file is
        // reflected in the panel (set_value doesn't emit, so no write-back loop).
        let ui_size = self.settings.appearance.ui_font_size as f64;
        let editor_size = self.settings.editor.font_size as f64;
        self.ui_font_size_input
            .update(cx, |n, cx| n.set_value(ui_size, cx));
        self.editor_font_size_input
            .update(cx, |n, cx| n.set_value(editor_size, cx));
        // A hand-edit of the file changes these too — re-push to the backend.
        self.service
            .send_global(Command::SetStatementTimeout(self.settings.query.timeout()));
        self.service.send_global(Command::SetDisplayCellCap(
            self.settings.grid.max_cell_chars,
        ));
        // Re-arm the updater in case `[update]` changed (toggle / interval). The
        // backend only re-polls if the cadence actually moved.
        self.service
            .send_global(Command::ConfigureUpdates(update_config(&self.settings)));
        self.apply_theme(cx);
        cx.notify();
    }

    /// Force an update check now ("Check for updates" in the About tab). A no-op
    /// in effect when `auto_update = false` — the backend ignores `CheckNow`
    /// while disabled — so the button is only offered when updates are on.
    pub(crate) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.service.send_global(Command::CheckForUpdate);
        cx.notify();
    }

    /// Relaunch into the freshly-staged build (Phase 4). The new bundle is already
    /// swapped over `/Applications/Red.app`, so this just spawns it and exits —
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

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn restart_for_update(&mut self, _cx: &mut Context<Self>) {}

    /// Store the updater's latest state and, when a build has finished staging,
    /// surface a one-off toast so the user notices the pill. Other transitions
    /// (checking, up-to-date, background failures) stay quiet — they're visible
    /// in the About tab without nagging.
    pub(crate) fn on_update_state(&mut self, state: UpdateState, cx: &mut Context<Self>) {
        let became_ready = matches!(state, UpdateState::ReadyToRestart { .. })
            && !matches!(self.update, UpdateState::ReadyToRestart { .. });
        self.update = state;
        if became_ready {
            self.notify(
                ToastVariant::Success,
                "An update is ready — restart to apply it.",
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

    /// Open the bundled, fully-commented reference defaults — RED's settings docs.
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
        cx.notify();
    }

    /// Fill the five Appearance dropdowns with the current themes + installed fonts
    /// and mark the active option. Called when the settings panel opens and after
    /// the theme registry changes (import/remove). The font list is read from the
    /// warmed cache (see [`Self::open_settings`]) — never re-enumerated here.
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

    /// Open the settings panel on its About tab — the RED → About RED menu item.
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
        cx.notify();
    }

    pub(crate) fn set_settings_tab(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        self.settings_tab = tab;
        cx.notify();
    }

    /// Re-resolve the active theme from settings + OS appearance and install it.
    pub(crate) fn apply_theme(&self, cx: &mut Context<Self>) {
        cx.set_global(crate::theme::with_typography(
            self.themes
                .resolve(&self.settings.appearance.theme, self.os_dark),
            &self.settings.appearance,
        ));
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

    /// The active appearance mode (System / Light / Dark) — drives the segmented.
    pub(crate) fn theme_mode(&self) -> ThemeMode {
        self.theme_decompose().0
    }

    /// The currently-selected theme name for a family — drives the pickers.
    pub(crate) fn selected_theme(&self, light: bool) -> String {
        let (_, l, d) = self.theme_decompose();
        if light {
            l
        } else {
            d
        }
    }

    /// Store a full `(mode, light, dark)` pair, apply it, and persist.
    pub(crate) fn set_theme_pair(
        &mut self,
        mode: ThemeMode,
        light: String,
        dark: String,
        cx: &mut Context<Self>,
    ) {
        self.settings.appearance.theme = ThemeSetting::Modal { mode, light, dark };
        self.apply_theme(cx);
        self.save_settings();
        cx.notify();
    }

    /// Switch how the theme tracks the OS — `System` follows the OS light/dark,
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
            if let Ok(Ok(Some(paths))) = paths.await {
                if let Some(path) = paths.into_iter().next() {
                    this.update(cx, |this, cx| this.finish_import(&path, cx))
                        .ok();
                }
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

    /// Delete a user theme, reload the registry, and re-apply — a removed active
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

    pub(crate) fn set_density(&mut self, density: Density, cx: &mut Context<Self>) {
        self.settings.grid.density = density;
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_null_display(&mut self, value: &str, cx: &mut Context<Self>) {
        self.settings.grid.null_display = value.to_string();
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_auto_limit(&mut self, n: u32, cx: &mut Context<Self>) {
        self.settings.query.auto_limit = n;
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_confirm_destructive(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.query.confirm_destructive = on;
        self.save_settings();
        cx.notify();
    }

    /// Set the statement timeout (seconds; `0` disables) and push it to the backend
    /// so it applies to the next query and its page/run fetches.
    pub(crate) fn set_statement_timeout(&mut self, secs: u32, cx: &mut Context<Self>) {
        self.settings.query.statement_timeout = secs;
        self.save_settings();
        self.service
            .send_global(Command::SetStatementTimeout(self.settings.query.timeout()));
        cx.notify();
    }

    /// Set the fat-cell display cap (bytes) and push it to the driver; it applies to
    /// every subsequent display fetch. Clamped to a sane range.
    pub(crate) fn set_max_cell_chars(&mut self, bytes: usize, cx: &mut Context<Self>) {
        self.settings.grid.max_cell_chars = bytes.clamp(
            crate::settings::MIN_CELL_CHARS,
            crate::settings::MAX_CELL_CHARS,
        );
        self.save_settings();
        self.service.send_global(Command::SetDisplayCellCap(
            self.settings.grid.max_cell_chars,
        ));
        cx.notify();
    }

    /// Set the keyset/offset fetch window. Clamped; applies to results opened after
    /// the change (a live result keeps the page its buffer was built with).
    pub(crate) fn set_page_size(&mut self, rows: usize, cx: &mut Context<Self>) {
        self.settings.grid.page_size = rows.clamp(
            crate::settings::MIN_PAGE_SIZE,
            crate::settings::MAX_PAGE_SIZE,
        );
        self.save_settings();
        cx.notify();
    }

    /// Toggle the leading row-number gutter. The gutter is column `0` in the grid's
    /// coordinate system, so flipping it shifts the data-column offset — clear the
    /// active selection (stored in table-column coords) so it can't point off by one.
    pub(crate) fn set_row_numbers(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.grid.row_numbers = on;
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = active.active_result_mut() {
                grid.clear_selection();
            }
        }
        self.save_settings();
        cx.notify();
    }

    /// Toggle reconnect-on-launch. Takes effect next launch (see `ensure_observers`).
    pub(crate) fn set_restore_last_session(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.behavior.restore_last_session = on;
        self.save_settings();
        cx.notify();
    }

    /// Toggle background self-updates. Re-arms the backend updater immediately —
    /// turning it on kicks off a check; turning it off parks the timer + network.
    pub(crate) fn set_auto_update(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.update.auto_update = on;
        self.save_settings();
        self.service
            .send_global(Command::ConfigureUpdates(update_config(&self.settings)));
        cx.notify();
    }

    /// Open a URL (the update release page) with the OS default handler. Reuses
    /// the same shell-out seam as the settings-file workflow.
    pub(crate) fn open_external(&mut self, url: &str, cx: &mut Context<Self>) {
        if let Err(e) = open_in_os(std::path::Path::new(url)) {
            self.notify(ToastVariant::Error, format!("Couldn't open {url}: {e}"), cx);
        }
    }

    pub(crate) fn set_ui_font_family(&mut self, family: &str, cx: &mut Context<Self>) {
        self.settings.appearance.ui_font_family = family.to_string();
        self.save_settings();
        self.apply_theme(cx);
        cx.notify();
    }

    pub(crate) fn set_ui_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.settings.appearance.ui_font_size = size.clamp(
            crate::settings::MIN_FONT_SIZE,
            crate::settings::MAX_FONT_SIZE,
        );
        self.save_settings();
        self.apply_theme(cx);
        cx.notify();
    }

    pub(crate) fn set_ui_mono_family(&mut self, family: &str, cx: &mut Context<Self>) {
        self.settings.appearance.ui_mono_family = family.to_string();
        self.save_settings();
        // The UI mono family is a theme token (result grid, schema identifiers).
        self.apply_theme(cx);
        cx.notify();
    }

    pub(crate) fn set_editor_font_family(&mut self, family: &str, cx: &mut Context<Self>) {
        self.settings.editor.font_family = family.to_string();
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn set_editor_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.settings.editor.font_size = size.clamp(
            crate::settings::MIN_FONT_SIZE,
            crate::settings::MAX_FONT_SIZE,
        );
        self.save_settings();
        cx.notify();
    }

    /// Dismiss the settings-warning banner until the next problematic load.
    pub(crate) fn dismiss_settings_warnings(&mut self, cx: &mut Context<Self>) {
        self.settings_warnings.clear();
        cx.notify();
    }

    /// Persist the current preferences. A write failure is logged, not surfaced —
    /// preferences are convenience, and the in-memory value already took effect.
    /// The bytes are announced to the watcher first so the reload this write
    /// triggers is suppressed (no self-inflicted reload storm).
    pub(crate) fn save_settings(&self) {
        let Some(store) = &self.settings_store else {
            return;
        };
        if let Some(watcher) = &self.settings_watcher {
            if let Ok(serialized) = toml::to_string_pretty(&self.settings) {
                watcher.note_self_write(&serialized);
            }
        }
        if let Err(e) = store.save(&self.settings) {
            tracing::warn!("failed to save settings: {e}");
        }
    }

    /// Save the connection list, surfacing a write failure as a toast.
    pub(crate) fn persist(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = config::save(&self.connections) {
            tracing::warn!("failed to save connections: {e}");
            self.notify(
                ToastVariant::Error,
                format!("Couldn't save connections: {e}"),
                cx,
            );
        }
    }
}
