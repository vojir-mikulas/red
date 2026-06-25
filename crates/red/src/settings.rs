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
    /// Row threshold above which the column-stats bar withholds the (potentially
    /// full-scan) `count(distinct)` until the user explicitly asks for it — so
    /// selecting a column never silently launches a heavy query on a huge result.
    pub stats_distinct_max_rows: usize,
}

impl Default for GridSettings {
    fn default() -> Self {
        Self {
            density: Density::default(),
            row_numbers: true,
            null_display: "NULL".to_string(),
            max_cell_chars: 4096,
            page_size: 200,
            stats_distinct_max_rows: 1_000_000,
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
    /// Default subscription-agent model selector, as the agent's opaque value id
    /// (set from the composer's dropdown). Applied to each new chat's session; empty
    /// lets the agent keep its own default. Last choice wins — changing the dropdown
    /// rewrites this, but never retroactively changes already-open chats.
    pub subscription_model: String,
    /// Default subscription-agent reasoning-level selector (opaque value id), same
    /// semantics as `subscription_model`.
    pub subscription_reasoning: String,
    /// Default subscription-agent permission-mode selector (opaque value id) — the
    /// agent's accept policy (default / accept edits / auto / bypass). Same
    /// last-choice-wins semantics as `subscription_model`.
    pub subscription_mode: String,
    /// Surface a summarized "thinking…" affordance while the model reasons.
    pub show_thinking: bool,
    /// Advanced: override the subscription agent's launch command. Empty falls
    /// back to the default `npx -y @agentclientprotocol/claude-agent-acp`.
    /// Legacy: superseded by the matching built-in's `command` once `agents` is set.
    pub agent_command: String,
    /// User-defined agent profiles (`[[ai.agents]]`). When empty, profiles are
    /// synthesized from the legacy `provider`/`model`/`agent_command` keys (see
    /// [`AiSettings::resolved_agents`]) so a config written before agent profiles
    /// keeps working unchanged.
    pub agents: Vec<AiAgentSettings>,
    /// The agent id new chats start on. Empty (or naming a missing agent) resolves
    /// to the legacy provider's built-in, else the first agent.
    pub default_agent: String,
    /// Folder the agent writes generated HTML reports to (the `generate_report` tool).
    /// Empty (the default) uses the system temp dir; set it so reports land somewhere
    /// the user can find them. Created on demand; an unusable folder falls back to the
    /// temp dir rather than failing the report.
    pub report_dir: String,
    /// Resource guards on the `read` tier (`[ai.limits]`, M-S7).
    pub limits: AiLimitsSettings,
}

impl AiSettings {
    /// A fail-closed variant: the assistant off, everything else at defaults. Used
    /// when the `[ai]` section (or the whole settings file) can't be parsed, so a
    /// malformed hand-edit disables AI rather than silently reverting to the
    /// permissive default (`enabled = true`, read tier).
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "anthropic".to_string(),
            tier: "read".to_string(),
            model: "claude-opus-4-8".to_string(),
            subscription_model: String::new(),
            subscription_reasoning: String::new(),
            subscription_mode: String::new(),
            show_thinking: false,
            agent_command: String::new(),
            agents: Vec::new(),
            default_agent: String::new(),
            report_dir: String::new(),
            limits: AiLimitsSettings::default(),
        }
    }
}

/// The two built-in agent ids. Kept byte-stable: `"anthropic"` is the keyring
/// account (`ai-key:anthropic`) and the binding old saved chats persist, and both
/// are what legacy configs synthesize — renaming would orphan keys and chats.
pub const BUILTIN_API_AGENT: &str = "anthropic";
pub const BUILTIN_ACP_AGENT: &str = "subscription";

/// One user-defined agent profile (`[[ai.agents]]`). `kind` selects the backend:
/// `"api"` (the Messages API via `red-ai`, optionally at a custom `base_url`) or
/// `"acp"` (an external agent over ACP via `red-acp`, launched by `command` —
/// Claude Code, `codex acp`, a local agent). The API key never lives here; it's in
/// the OS keyring under `ai-key:<id>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiAgentSettings {
    /// Stable id: the keyring account (`ai-key:<id>`), the saved-chat binding, and
    /// the per-turn selector. The built-ins use `"anthropic"`/`"subscription"`.
    pub id: String,
    /// Display name shown in the selector and chat header.
    pub name: String,
    /// `"api"` or `"acp"`.
    pub kind: String,
    /// ACP: launch command; empty falls back to the default Claude Code invocation.
    pub command: String,
    /// API: wire format. `"anthropic"` is the only value in v1.
    pub wire: String,
    /// API: endpoint override; empty uses the default Anthropic base URL.
    pub base_url: String,
    /// API: model id; empty falls back to the Opus default.
    pub model: String,
}

impl Default for AiAgentSettings {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            kind: "api".to_string(),
            command: String::new(),
            wire: "anthropic".to_string(),
            base_url: String::new(),
            model: String::new(),
        }
    }
}

impl AiSettings {
    /// The effective agent profiles. An explicit `[[ai.agents]]` list wins (blank
    /// ids dropped, duplicate ids de-duped keeping the first). When absent, two
    /// profiles are synthesized from the legacy `provider`/`model`/`agent_command`
    /// keys so a config written before agent profiles keeps working unchanged.
    pub fn resolved_agents(&self) -> Vec<AiAgentSettings> {
        if !self.agents.is_empty() {
            let mut seen = std::collections::HashSet::new();
            let explicit: Vec<AiAgentSettings> = self
                .agents
                .iter()
                .filter(|a| !a.id.trim().is_empty())
                .filter(|a| seen.insert(a.id.trim().to_string()))
                // Store the trimmed id, not just dedup on it: the id is the keychain
                // account key and the built-in env-var match, so a stray-whitespace
                // id (`" anthropic"`) must resolve to the same identity downstream.
                .map(|a| AiAgentSettings {
                    id: a.id.trim().to_string(),
                    ..a.clone()
                })
                .collect();
            if !explicit.is_empty() {
                return explicit;
            }
        }
        // Legacy synthesis: the two built-ins, ids byte-stable (see the consts).
        vec![
            AiAgentSettings {
                id: BUILTIN_API_AGENT.to_string(),
                name: "Claude (API)".to_string(),
                kind: "api".to_string(),
                command: String::new(),
                wire: "anthropic".to_string(),
                base_url: String::new(),
                model: self.model.clone(),
            },
            AiAgentSettings {
                id: BUILTIN_ACP_AGENT.to_string(),
                name: "Claude (subscription)".to_string(),
                kind: "acp".to_string(),
                command: self.agent_command.clone(),
                wire: String::new(),
                base_url: String::new(),
                model: String::new(),
            },
        ]
    }

    /// The agent id new chats start on: an explicit `default_agent` when it names a
    /// resolved agent; else (legacy only) the old `provider` mapped to its built-in
    /// id; else the first resolved agent (empty when none).
    pub fn resolved_default_agent(&self) -> String {
        let agents = self.resolved_agents();
        let has = |id: &str| agents.iter().any(|a| a.id == id);
        let want = self.default_agent.trim();
        if !want.is_empty() && has(want) {
            return want.to_string();
        }
        // Legacy: map the old provider string onto a built-in id (only meaningful
        // when no explicit agents are configured, i.e. we synthesized them).
        if self.agents.is_empty() {
            let legacy = if self.provider.eq_ignore_ascii_case("subscription") {
                BUILTIN_ACP_AGENT
            } else {
                BUILTIN_API_AGENT
            };
            if has(legacy) {
                return legacy.to_string();
            }
        }
        agents.first().map(|a| a.id.clone()).unwrap_or_default()
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
                // Fail closed: if we can't read the user's file at all, don't assume
                // they wanted the assistant on — disable it rather than reverting to
                // the permissive `AiSettings::default()`.
                return LoadReport {
                    settings: Settings {
                        ai: AiSettings::disabled(),
                        ..Settings::default()
                    },
                    warnings: vec![format!(
                        "settings.toml isn't valid TOML ({e}) — using defaults; the assistant is \
                         disabled until it's fixed"
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
            ai: ai_section(&value, &mut warnings),
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

/// Like [`section`] but **fails closed** for the security-sensitive `[ai]` table.
/// A malformed section disables the assistant ([`AiSettings::disabled`]) instead of
/// reverting to the permissive default (`enabled = true`, read tier), so a stray
/// hand-edit — a typo'd key, a wrong-typed value — can't silently re-enable AI
/// access against the user's intent. A missing section still uses the normal
/// default (the shipped behavior).
fn ai_section(value: &toml::Value, warnings: &mut Vec<String>) -> AiSettings {
    match value.get("ai") {
        None => AiSettings::default(),
        Some(v) => match v.clone().try_into() {
            Ok(parsed) => parsed,
            Err(e) => {
                warnings.push(format!(
                    "settings.toml: couldn't read [ai] ({e}) — the assistant is disabled until \
                     it's fixed"
                ));
                AiSettings::disabled()
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
    fn legacy_ai_synthesizes_two_builtin_agents() {
        // No [[ai.agents]] → synthesize the API + subscription built-ins with
        // byte-stable ids, carrying the legacy model/command through.
        let ai = AiSettings {
            model: "claude-x".into(),
            agent_command: "my-agent".into(),
            ..AiSettings::default()
        };
        let agents = ai.resolved_agents();
        let ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, [BUILTIN_API_AGENT, BUILTIN_ACP_AGENT]);
        assert_eq!(agents[0].kind, "api");
        assert_eq!(agents[0].model, "claude-x");
        assert_eq!(agents[1].kind, "acp");
        assert_eq!(agents[1].command, "my-agent");
    }

    #[test]
    fn legacy_provider_drives_default_agent() {
        let api = AiSettings {
            provider: "anthropic".into(),
            ..AiSettings::default()
        };
        assert_eq!(api.resolved_default_agent(), BUILTIN_API_AGENT);
        let sub = AiSettings {
            provider: "subscription".into(),
            ..AiSettings::default()
        };
        assert_eq!(sub.resolved_default_agent(), BUILTIN_ACP_AGENT);
    }

    #[test]
    fn explicit_agents_win_over_legacy() {
        let toml = r#"
            provider = "anthropic"
            default_agent = "codex"
            [[agents]]
            id = "codex"
            name = "Codex"
            kind = "acp"
            command = "codex acp"
            [[agents]]
            id = "local"
            name = "Local"
            kind = "api"
            base_url = "http://127.0.0.1:8080"
            model = "llama"
        "#;
        let ai: AiSettings = toml::from_str(toml).expect("ai settings");
        let agents = ai.resolved_agents();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].id, "codex");
        assert_eq!(agents[1].base_url, "http://127.0.0.1:8080");
        // The explicit, valid default_agent is honored.
        assert_eq!(ai.resolved_default_agent(), "codex");
    }

    #[test]
    fn blank_and_duplicate_ids_are_dropped() {
        let toml = r#"
            [[agents]]
            id = "  "
            name = "Blank"
            [[agents]]
            id = "dup"
            name = "First"
            kind = "acp"
            [[agents]]
            id = "dup"
            name = "Second"
            kind = "api"
        "#;
        let ai: AiSettings = toml::from_str(toml).expect("ai settings");
        let agents = ai.resolved_agents();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, "dup");
        // First wins on a duplicate id.
        assert_eq!(agents[0].name, "First");
    }

    #[test]
    fn default_agent_at_missing_id_falls_back_to_first() {
        let toml = r#"
            default_agent = "ghost"
            [[agents]]
            id = "a"
            name = "A"
            kind = "acp"
            [[agents]]
            id = "b"
            name = "B"
            kind = "acp"
        "#;
        let ai: AiSettings = toml::from_str(toml).expect("ai settings");
        assert_eq!(ai.resolved_default_agent(), "a");
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
        // Everything but AI falls back to defaults; AI fails CLOSED (disabled) since
        // we couldn't read the user's intent for a security-sensitive control.
        assert_eq!(
            report.settings,
            Settings {
                ai: AiSettings::disabled(),
                ..Settings::default()
            }
        );
        assert!(!report.settings.ai.enabled);
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn malformed_ai_section_fails_closed() {
        // A wrong-typed key in [ai] (here a string for the bool `show_thinking`)
        // fails the whole section. It must disable the assistant rather than revert
        // to the permissive default — even though `enabled = true` is set here, a
        // parse failure must not leave AI on.
        let t = temp_store();
        write(
            &t.store,
            "[ai]\nenabled = true\ntier = \"read\"\nshow_thinking = \"yes\"\n",
        );
        let loaded = t.store.load_report();
        assert!(
            !loaded.settings.ai.enabled,
            "AI must fail closed, not stay enabled"
        );
        assert_eq!(loaded.warnings.len(), 1);
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
