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
    pub update: UpdateSettings,
    pub ai: AiSettings,
}

// --- appearance --------------------------------------------------------------

/// Theme and fonts. The accent is purely theme-defined (a theme file may set it);
/// the UI font family + size are applied live to the whole interface (the editor
/// has its own family/size under `[editor]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub theme: ThemeSetting,
    pub ui_font_family: String,
    /// The mono family for in-UI code + tabular data (result grid, schema
    /// identifiers). Shares [`ui_font_size`](Self::ui_font_size) with the sans
    /// family; the editor keeps its own family/size under `[editor]`.
    pub ui_mono_family: String,
    pub ui_font_size: f32,
    /// Suppress non-essential animation (currently the indeterminate progress
    /// sweep), for users who find motion distracting or vestibular-triggering.
    /// Off by default; RED has no OS "reduce motion" bridge yet, so this is the
    /// manual opt-in. Honored by Flint via its `ReduceMotion` global.
    pub reduce_motion: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: ThemeSetting::default(),
            ui_font_family: FONT_UI.to_string(),
            ui_mono_family: FONT_MONO.to_string(),
            // The design's base UI size.
            ui_font_size: 13.0,
            reduce_motion: false,
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
        // Follow the OS appearance out of the box, on Red's brand-red Ayu pair.
        ThemeSetting::Modal {
            mode: ThemeMode::System,
            light: default_light(),
            dark: default_dark(),
        }
    }
}

fn default_light() -> String {
    "Ayu Light".to_string()
}
fn default_dark() -> String {
    "Ayu Dark".to_string()
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

/// SQL editor typography, applied live to the `CodeEditor` surface (which
/// inherits the family / size / line-height set on its container).
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
    /// Abort a query (and each of its page/run fetches) that runs longer than this
    /// many seconds, so a runaway can't wedge the grid. `0` disables the cap.
    pub statement_timeout: u32,
}

impl Default for QuerySettings {
    fn default() -> Self {
        Self {
            auto_limit: 1000,
            confirm_destructive: true,
            statement_timeout: 0,
        }
    }
}

impl QuerySettings {
    /// The statement timeout as a duration, or `None` when disabled (`0`).
    pub fn timeout(&self) -> Option<std::time::Duration> {
        (self.statement_timeout > 0)
            .then(|| std::time::Duration::from_secs(self.statement_timeout as u64))
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

// --- update ------------------------------------------------------------------

/// macOS self-update behaviour (see docs/plans/self-update.md). `auto_update =
/// false` is the off-switch the plan promises: no poll timer, no network. The
/// interval is clamped to a sane floor so a stray `0` can't hammer GitHub.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    /// Poll GitHub Releases and stage newer notarized builds in the background.
    pub auto_update: bool,
    /// Hours between background checks (the first runs at launch).
    pub check_interval_hours: u32,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            auto_update: true,
            check_interval_hours: 6,
        }
    }
}

impl UpdateSettings {
    /// The poll cadence as a `Duration`, with a 1-hour floor so a hand-edited `0`
    /// (or a tiny value) can't turn the updater into a tight network loop.
    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(self.check_interval_hours.max(1)) * 3600)
    }
}

// --- ai ----------------------------------------------------------------------

/// AI assistant configuration (the right-docked chat sidebar). The API key does
/// **not** live here — it routes through the OS keyring (see `crate::secrets`),
/// the same secret store connection passwords use. Only the non-secret knobs
/// (provider, model, the thinking-display toggle) are persisted in `settings.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    /// The master switch. `false` is a true kill switch — no panel entry point,
    /// no MCP server, no agent process (M-S7). A connection can override it.
    pub enabled: bool,
    /// Which backend handles turns:
    /// - `"anthropic"` (default) — the Claude Messages API, billed to an API key.
    /// - `"subscription"` — Claude Code over ACP, billed to the user's Pro/Max
    ///   subscription (the agent owns its own login; no key needed).
    pub provider: String,
    /// Database access tier the assistant's tools run at (M-S7): `"off"` (no DB
    /// tools), `"schema"` (structure only, no row data), or `"read"` (the full
    /// read catalog). A connection can override it; unknown values resolve to
    /// `"read"`.
    pub tier: String,
    /// Model id, e.g. `claude-opus-4-8`. Empty falls back to the Opus default.
    /// (API-key path only; the subscription agent picks its own model.)
    pub model: String,
    /// Surface a summarized "thinking…" affordance while the model reasons.
    pub show_thinking: bool,
    /// Advanced: override the subscription agent's launch command. Empty falls
    /// back to the default `npx -y @agentclientprotocol/claude-agent-acp`.
    pub agent_command: String,
    /// Resource guards on the `read` tier (`[ai.limits]`, M-S7).
    pub limits: AiLimitsSettings,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "anthropic".to_string(),
            tier: "read".to_string(),
            model: "claude-opus-4-8".to_string(),
            show_thinking: false,
            agent_command: String::new(),
            limits: AiLimitsSettings::default(),
        }
    }
}

/// The `[ai.limits]` block: defense-in-depth caps the assistant's tools run
/// under, mirroring [`red_core::AiLimits`]. Defaults to the same sane ceilings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiLimitsSettings {
    /// Hard row ceiling on one `run_select`; a larger LIMIT is clamped.
    pub max_rows: usize,
    /// Per-tool-call statement timeout in milliseconds. `0` disables it.
    pub statement_timeout_ms: u64,
    /// Cap on the bytes of one tool result handed back to the model. `0` disables.
    pub max_result_bytes: usize,
    /// Cap on tool calls per conversation, bounding a runaway loop. `0` disables.
    pub max_tool_calls: usize,
}

impl Default for AiLimitsSettings {
    fn default() -> Self {
        // Mirror `red_core::AiLimits::default()` so the wired default matches the
        // backend's own fallback.
        let d = red_core::AiLimits::default();
        Self {
            max_rows: d.max_rows,
            statement_timeout_ms: d.statement_timeout_ms,
            max_result_bytes: d.max_result_bytes,
            max_tool_calls: d.max_tool_calls,
        }
    }
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
            update: section(&value, "update", &mut warnings),
            ai: section(&value, "ai", &mut warnings),
        };
        let migrated = apply_legacy(&mut settings, &value);

        // Keep typography in a sane range so a stray hand-edit (0, negative, NaN,
        // absurdly large) can't break layout. Silent — clamping isn't an "error".
        settings.appearance.ui_font_size = clamp_font_size(settings.appearance.ui_font_size);
        settings.editor.font_size = clamp_font_size(settings.editor.font_size);
        settings.editor.line_height = if settings.editor.line_height.is_finite() {
            settings.editor.line_height.clamp(1.0, 3.0)
        } else {
            1.5
        };
        // Grid fetch knobs feed the keyset window and the fat-cell rail directly,
        // so a stray value (0, absurdly large) must clamp to a sane range rather
        // than thrash memory or stall paging. Silent, like the typography clamp.
        settings.grid.page_size = settings.grid.page_size.clamp(MIN_PAGE_SIZE, MAX_PAGE_SIZE);
        settings.grid.max_cell_chars = settings
            .grid
            .max_cell_chars
            .clamp(MIN_CELL_CHARS, MAX_CELL_CHARS);

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

/// The font sizes (px) the UI will accept. A value outside this range — or a NaN
/// / infinity — falls back to the safe floor rather than breaking layout.
pub const MIN_FONT_SIZE: f32 = 8.0;
pub const MAX_FONT_SIZE: f32 = 32.0;

/// The keyset/offset fetch window (`grid.page_size`) the grid will accept. Below
/// the floor paging stalls; above the ceiling a single page can spike RAM (the
/// resident buffer is bounded by a multiple of the page).
pub const MIN_PAGE_SIZE: usize = 20;
pub const MAX_PAGE_SIZE: usize = 5_000;

/// The fat-cell display rail (`grid.max_cell_chars`). The floor keeps a cell
/// readable; the ceiling bounds the per-cell bytes the driver materializes for a
/// display page (export stays full-fidelity regardless).
pub const MIN_CELL_CHARS: usize = 256;
pub const MAX_CELL_CHARS: usize = 1_048_576;

fn clamp_font_size(size: f32) -> f32 {
    if size.is_finite() {
        size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    } else {
        13.0
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
    fn clamps_grid_fetch_knobs() {
        // Out-of-range fetch knobs clamp to the floor / ceiling rather than thrash.
        let t = temp_store();
        write(&t.store, "[grid]\npage_size = 0\nmax_cell_chars = 1\n");
        let g = t.store.load_report().settings.grid;
        assert_eq!(g.page_size, MIN_PAGE_SIZE);
        assert_eq!(g.max_cell_chars, MIN_CELL_CHARS);

        write(
            &t.store,
            "[grid]\npage_size = 99999999\nmax_cell_chars = 999999999\n",
        );
        let g = t.store.load_report().settings.grid;
        assert_eq!(g.page_size, MAX_PAGE_SIZE);
        assert_eq!(g.max_cell_chars, MAX_CELL_CHARS);
    }

    #[test]
    fn statement_timeout_parses_and_maps_to_duration() {
        let q: QuerySettings = toml::from_str("statement_timeout = 30").expect("timeout");
        assert_eq!(q.statement_timeout, 30);
        assert_eq!(q.timeout(), Some(std::time::Duration::from_secs(30)));
        // The default (and an explicit 0) disables the cap.
        assert_eq!(QuerySettings::default().timeout(), None);
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
