//! Persisted UI preferences — a structured, hand-editable, Zed-style config.
//!
//! These are app-wide presentation settings, not per-connection data, so they
//! live in their own `settings.toml` beside `connections.toml` in the platform
//! config dir. The flat key set grew into nested sections ([`AppearanceSettings`],
//! [`GridSettings`], …); each is `#[serde(default)]` so a partial — or slightly
//! wrong — file keeps every key it *can* read and defaults only the rest. A single
//! bad key must never reset the whole file.
//!
//! Writes go through a temp-file + atomic rename; reads **never** fail — a missing
//! or malformed file degrades to [`Settings::default`], because preferences are
//! convenience, not user data, and a bad file must never block launch. A
//! recoverable problem (one unreadable section, a typo'd value) surfaces as a
//! warning in [`LoadReport`] for a non-blocking banner, while last-good defaults
//! stay applied.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gpui::{px, Pixels};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::assets::{FONT_MONO, FONT_UI};

/// Persisted UI preferences, grouped into hand-editable sections.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: AppearanceSettings,
    pub editor: EditorSettings,
    pub grid: GridSettings,
    pub query: QuerySettings,
    pub behavior: BehaviorSettings,
}

// --- appearance --------------------------------------------------------------

/// Theme and fonts. The accent is purely theme-defined (a theme file may set it);
/// the font knobs are modeled for forward compatibility — live application of
/// UI/editor fonts depends on Flint font tokens (a follow-up), while `theme` is
/// applied today.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub theme: ThemeSetting,
    pub ui_font_family: String,
    pub ui_font_size: f32,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: ThemeSetting::default(),
            ui_font_family: FONT_UI.to_string(),
            ui_font_size: 14.0,
        }
    }
}

/// How the theme is chosen: a single named theme, or a mode-aware pair that
/// follows the OS appearance (or a forced light/dark).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThemeSetting {
    /// One theme name applied regardless of OS appearance (`theme = "One Dark"`).
    Named(String),
    /// Mode-aware (`theme = { mode = "system", light = "Ayu Light", dark = "One Dark" }`).
    Modal {
        #[serde(default)]
        mode: ThemeMode,
        #[serde(default = "default_light")]
        light: String,
        #[serde(default = "default_dark")]
        dark: String,
    },
}

impl Default for ThemeSetting {
    fn default() -> Self {
        ThemeSetting::Named(default_dark())
    }
}

fn default_light() -> String {
    "Ayu Light".to_string()
}
fn default_dark() -> String {
    "One Dark".to_string()
}

impl ThemeSetting {
    /// The concrete theme name to apply, given whether the OS is in dark mode.
    pub fn resolve(&self, os_dark: bool) -> &str {
        match self {
            ThemeSetting::Named(name) => name,
            ThemeSetting::Modal { mode, light, dark } => match mode {
                ThemeMode::Light => light,
                ThemeMode::Dark => dark,
                ThemeMode::System => {
                    if os_dark {
                        dark
                    } else {
                        light
                    }
                }
            },
        }
    }
}

/// Which theme of a [`ThemeSetting::Modal`] pair to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    /// Follow the OS light/dark appearance.
    System,
    Light,
    #[default]
    Dark,
}

// --- editor ------------------------------------------------------------------

/// SQL editor typography. Modeled for forward compatibility; live application
/// depends on Flint exposing editor font tokens (a follow-up).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorSettings {
    pub font_family: String,
    pub font_size: f32,
    pub line_height: f32,
    pub tab_width: u8,
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            font_family: FONT_MONO.to_string(),
            font_size: 13.0,
            line_height: 1.5,
            tab_width: 2,
        }
    }
}

// --- grid --------------------------------------------------------------------

/// Result-grid behaviour, tuned for fast browsing of large result sets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GridSettings {
    pub density: Density,
    /// Show the leading row-number gutter.
    pub row_numbers: bool,
    /// What a SQL `NULL` renders as (e.g. `∅`, `NULL`, or blank).
    pub null_display: String,
    /// Hard cap on the characters of any one cell the grid keeps resident — the
    /// fat-cell memory rail. Clamped to a sane floor on load.
    pub max_cell_chars: usize,
    /// The streaming/keyset fetch window: how many rows a page request pulls.
    pub page_size: usize,
}

impl Default for GridSettings {
    fn default() -> Self {
        Self {
            density: Density::default(),
            row_numbers: true,
            null_display: "NULL".to_string(),
            max_cell_chars: 4096,
            page_size: 200,
        }
    }
}

/// Result-grid row spacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    Compact,
    #[default]
    Comfortable,
    Spacious,
}

impl Density {
    pub const ALL: [Density; 3] = [Density::Compact, Density::Comfortable, Density::Spacious];

    /// Index into [`Self::ALL`] — drives the segmented control.
    pub fn index(self) -> usize {
        match self {
            Density::Compact => 0,
            Density::Comfortable => 1,
            Density::Spacious => 2,
        }
    }

    /// Map a legacy persisted index (`0`/`1`/`2`) onto a variant for migration.
    pub fn from_index(index: usize) -> Self {
        Self::ALL[index.min(Self::ALL.len() - 1)]
    }

    /// The grid row height for this density.
    pub fn row_height(self) -> Pixels {
        match self {
            Density::Compact => px(22.),
            Density::Comfortable => px(25.),
            Density::Spacious => px(30.),
        }
    }
}

// --- query -------------------------------------------------------------------

/// Query-execution safety rails — RED's on-brand big-result defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuerySettings {
    /// Append `LIMIT n` to a bare `SELECT *` so a fat table can't flood the grid.
    /// `0` disables the auto-limit.
    pub auto_limit: u32,
    /// Confirm destructive statements (DROP/TRUNCATE/…) before they run.
    pub confirm_destructive: bool,
}

impl Default for QuerySettings {
    fn default() -> Self {
        Self {
            auto_limit: 1000,
            confirm_destructive: true,
        }
    }
}

// --- behavior ----------------------------------------------------------------

/// Session behaviour. `restore_last_session` is modeled but not yet wired (it
/// touches the connection lifecycle + keychain — a follow-up). `false` is the
/// derived default.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorSettings {
    pub restore_last_session: bool,
}

// --- store -------------------------------------------------------------------

/// The outcome of a load: the resolved settings, plus any non-fatal warnings to
/// surface (an unreadable section, a value out of range) and whether a legacy
/// flat file was migrated and should be re-saved in the new shape.
#[derive(Debug, Clone, Default)]
pub struct LoadReport {
    pub settings: Settings,
    pub warnings: Vec<String>,
    pub migrated: bool,
}

/// Local on-disk settings store over a single `settings.toml`.
///
/// Reads never fail (missing/malformed → [`Settings::default`]); writes are
/// atomic (temp file + rename on the same volume).
#[derive(Debug, Clone)]
pub struct FileSettingsStore {
    path: PathBuf,
}

impl FileSettingsStore {
    /// Open the store at `<config_dir>/red/settings.toml`, beside the connection
    /// list. Returns `None` when the platform has no config dir.
    pub fn open_default() -> Option<Self> {
        let path = dirs::config_dir()?.join("red").join("settings.toml");
        Some(Self { path })
    }

    /// Open a store backed by an explicit file path (used in tests).
    #[cfg(test)]
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The backing file path, for the "open settings file" workflow.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read with diagnostics: resolves each section independently so one bad
    /// section can't reset the rest, and lifts any legacy top-level keys.
    pub fn load_report(&self) -> LoadReport {
        let Ok(contents) = std::fs::read_to_string(&self.path) else {
            return LoadReport::default();
        };
        let value: toml::Value = match contents.parse() {
            Ok(v) => v,
            Err(e) => {
                return LoadReport {
                    settings: Settings::default(),
                    warnings: vec![format!(
                        "settings.toml isn't valid TOML ({e}) — using defaults"
                    )],
                    migrated: false,
                };
            }
        };

        let mut warnings = Vec::new();
        let mut settings = Settings {
            appearance: section(&value, "appearance", &mut warnings),
            editor: section(&value, "editor", &mut warnings),
            grid: section(&value, "grid", &mut warnings),
            query: section(&value, "query", &mut warnings),
            behavior: section(&value, "behavior", &mut warnings),
        };
        let migrated = apply_legacy(&mut settings, &value);

        LoadReport {
            settings,
            warnings,
            migrated,
        }
    }

    /// Serialize and write atomically: a sibling temp file, flushed, then renamed
    /// over the target so a crash can't leave a partial file.
    pub fn save(&self, settings: &Settings) -> Result<()> {
        use std::io::Write;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).context("creating the config directory")?;
        }
        let serialized = toml::to_string_pretty(settings)?;

        let tmp = self
            .path
            .with_extension(format!("toml.tmp.{}", std::process::id()));
        let mut file = std::fs::File::create(&tmp).context("creating the settings temp file")?;
        file.write_all(serialized.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &self.path).context("renaming the settings temp file")?;
        Ok(())
    }
}

/// Deserialize one named section independently, defaulting (with a warning) if it
/// is present but unreadable — so a single mistyped value degrades just its own
/// section, never the whole file.
fn section<T: Default + DeserializeOwned>(
    value: &toml::Value,
    key: &str,
    warnings: &mut Vec<String>,
) -> T {
    match value.get(key) {
        None => T::default(),
        Some(v) => match v.clone().try_into() {
            Ok(parsed) => parsed,
            Err(e) => {
                warnings.push(format!(
                    "settings.toml: couldn't read [{key}] ({e}) — keeping defaults for that section"
                ));
                T::default()
            }
        },
    }
}

/// Lift the legacy flat keys (`theme` / `density` / `confirm_destructive`) into
/// the new sections once, so an old file upgrades cleanly. Returns `true` when
/// anything was migrated (the caller re-saves in the new shape). These keys only
/// existed at the top level in the old format; the new nested keys live under
/// their sections, so reading them here is unambiguous.
fn apply_legacy(settings: &mut Settings, value: &toml::Value) -> bool {
    let mut migrated = false;
    if let Some(theme) = value.get("theme").and_then(|v| v.as_str()) {
        settings.appearance.theme = ThemeSetting::Named(theme.to_string());
        migrated = true;
    }
    if let Some(density) = value.get("density").and_then(|v| v.as_integer()) {
        settings.grid.density = Density::from_index(density.max(0) as usize);
        migrated = true;
    }
    if let Some(confirm) = value.get("confirm_destructive").and_then(|v| v.as_bool()) {
        settings.query.confirm_destructive = confirm;
        migrated = true;
    }
    migrated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway store under a unique temp dir, dropped via [`TempStore`] which
    /// cleans up the directory on `Drop` (no `tempfile` dependency in the tree).
    struct TempStore {
        dir: PathBuf,
        store: FileSettingsStore,
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn temp_store() -> TempStore {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("red-settings-test-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = FileSettingsStore::with_path(dir.join("settings.toml"));
        TempStore { dir, store }
    }

    fn write(store: &FileSettingsStore, contents: &str) {
        std::fs::write(store.path(), contents).unwrap();
    }

    #[test]
    fn missing_file_is_default() {
        let t = temp_store();
        assert_eq!(t.store.load_report().settings, Settings::default());
    }

    #[test]
    fn round_trip() {
        let t = temp_store();
        let mut settings = Settings::default();
        settings.appearance.theme = ThemeSetting::Named("GitHub Dark".into());
        settings.grid.density = Density::Compact;
        settings.query.confirm_destructive = false;
        settings.grid.null_display = "∅".into();
        t.store.save(&settings).unwrap();
        assert_eq!(t.store.load_report().settings, settings);
    }

    #[test]
    fn malformed_file_is_default_with_warning() {
        let t = temp_store();
        write(&t.store, "this is = not valid toml ][");
        let report = t.store.load_report();
        assert_eq!(report.settings, Settings::default());
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn partial_section_takes_field_defaults() {
        // A file with only one grid key keeps every other default, in every section.
        let t = temp_store();
        write(&t.store, "[grid]\nnull_display = \"—\"\n");
        let loaded = t.store.load_report().settings;
        assert_eq!(loaded.grid.null_display, "—");
        assert_eq!(loaded.grid.density, Density::default());
        assert_eq!(loaded.grid.page_size, GridSettings::default().page_size);
        assert_eq!(loaded.query, QuerySettings::default());
        assert_eq!(loaded.appearance, AppearanceSettings::default());
    }

    #[test]
    fn one_bad_section_does_not_reset_the_rest() {
        // `density` wants a string; an integer fails *only* the grid section.
        let t = temp_store();
        write(
            &t.store,
            "[grid]\ndensity = 7\n\n[query]\nauto_limit = 50\n",
        );
        let report = t.store.load_report();
        assert_eq!(report.settings.grid, GridSettings::default());
        assert_eq!(report.settings.query.auto_limit, 50);
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn theme_parses_both_shapes() {
        let named: AppearanceSettings =
            toml::from_str("theme = \"GitHub Dark\"").expect("named theme");
        assert_eq!(named.theme, ThemeSetting::Named("GitHub Dark".into()));

        let modal: AppearanceSettings = toml::from_str(
            "theme = { mode = \"system\", light = \"Ayu Light\", dark = \"One Dark\" }",
        )
        .expect("modal theme");
        assert_eq!(
            modal.theme,
            ThemeSetting::Modal {
                mode: ThemeMode::System,
                light: "Ayu Light".into(),
                dark: "One Dark".into(),
            }
        );
        assert_eq!(modal.theme.resolve(true), "One Dark");
        assert_eq!(modal.theme.resolve(false), "Ayu Light");
    }

    #[test]
    fn migrates_legacy_flat_file() {
        // The old shape: bare top-level theme/density/confirm_destructive.
        let t = temp_store();
        write(
            &t.store,
            "theme = \"GitHub Dark\"\ndensity = 0\nconfirm_destructive = false\n",
        );
        let report = t.store.load_report();
        assert!(report.migrated);
        assert_eq!(
            report.settings.appearance.theme,
            ThemeSetting::Named("GitHub Dark".into())
        );
        assert_eq!(report.settings.grid.density, Density::Compact);
        assert!(!report.settings.query.confirm_destructive);
    }
}
