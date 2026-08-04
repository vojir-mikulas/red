//! Persisted UI preferences: a structured, hand-editable, Zed-style config.
//!
//! These are app-wide presentation settings, not per-connection data, so they
//! live in their own `settings.toml` beside `connections.toml` in the platform
//! config dir. The flat key set grew into nested sections cut by *scope* rather
//! than by engine (see [`Settings`]); each is `#[serde(default)]` so a partial
//! (or slightly wrong) file keeps every key it *can* read and defaults only the
//! rest. A single bad key must never reset the whole file.
//!
//! Section names changed in 0.20 (`[grid]` → `[data]`, `[redis]` → `[kv]`,
//! `[query]` → `[sql]` + `[safety]`). [`apply_legacy`] lifts every older shape
//! forward on load and the file is re-saved in the new one, so an existing
//! hand-written config keeps working untouched.
//!
//! Writes go through a temp-file + atomic rename; reads **never** fail. A missing
//! or malformed file degrades to [`Settings::default`], because preferences are
//! convenience, not user data, and a bad file must never block launch. A
//! recoverable problem (one unreadable section, a typo'd value) surfaces as a
//! warning in [`LoadReport`] for a non-blocking banner, while last-good defaults
//! stay applied.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gpui::{Pixels, px};
use red_core::ConnEnv;
use red_core::sql::RiskLevel;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::assets::{FONT_MONO, FONT_UI};

/// Persisted UI preferences, grouped into hand-editable sections.
///
/// Sections are cut by *what a setting is about*, not by which engine shipped
/// first. Five scopes:
///
/// - **app** — [`appearance`](Self::appearance), [`editor`](Self::editor),
///   [`keymap`](Self::keymap), [`behavior`](Self::behavior),
///   [`update`](Self::update).
/// - **data view** — [`data`](Self::data): every grid, whatever the seam.
/// - **safety** — [`safety`](Self::safety): every write path, every engine.
/// - **seam** — [`sql`](Self::sql) / [`kv`](Self::kv) / [`doc`](Self::doc),
///   named for the driver seam (`DatabaseDriver` / `KvDriver` / `DocDriver`)
///   rather than for one engine, so a second key-value or document engine
///   inherits a configured section instead of starting empty.
/// - **agent** — [`ai`](Self::ai).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: AppearanceSettings,
    pub editor: EditorSettings,
    pub data: DataSettings,
    pub safety: SafetySettings,
    pub sql: SqlSettings,
    pub kv: KvSettings,
    pub doc: DocSettings,
    pub behavior: BehaviorSettings,
    pub update: UpdateSettings,
    pub ai: AiSettings,
    pub keymap: KeymapSettings,
}

/// The published copy of [`Settings`], readable by any view from an `&App`.
///
/// [`AppState`](crate::app::AppState) remains the owner and the only writer;
/// this is a mirror it republishes from the two paths every settings change
/// already funnels through (see [`Settings::publish`]). It exists because RED's
/// surfaces are becoming their own views, and a view that needs a setting
/// should not have to be handed it at construction and told again on every
/// change — that plumbing is per-view, per-setting, and easy to forget.
///
/// Mirrors Zed's `SettingsStore` global, minus the layered sources and the
/// per-setting registration RED does not need: `settings_reg.rs` already covers
/// what those buy.
struct GlobalSettings(Settings);

impl gpui::Global for GlobalSettings {}

impl Settings {
    /// The current settings, from anywhere with an `&App`.
    ///
    /// # Panics
    ///
    /// If called before the first [`publish`](Self::publish). `AppState::new`
    /// publishes before it builds anything, so every view is safe; a test that
    /// renders a view in isolation has to publish first.
    pub(crate) fn global(cx: &gpui::App) -> &Settings {
        &cx.global::<GlobalSettings>().0
    }

    /// Republish these settings to every reader.
    ///
    /// **Call this from anywhere that changes `AppState::settings`.** Today that
    /// is `AppState::new` (startup), `apply_settings_effects` (which both the
    /// in-app edit funnel and the file watcher run through), and the ACP
    /// subscription defaults, which write and save without a full effects pass.
    /// Publishing is idempotent, so overlapping call sites are fine and belt-
    /// and-braces is the right default: a missed one means a view silently reads
    /// a stale value.
    ///
    /// `cx.set_global` notifies `cx.observe_global::<GlobalSettings>` watchers,
    /// so a view that must *re-render* on a change observes rather than polls.
    pub(crate) fn publish(&self, cx: &mut gpui::App) {
        cx.set_global(GlobalSettings(self.clone()));
    }

    /// Clamp every bounded knob into the range the app will accept, so a stray
    /// hand-edit (`0`, negative, NaN, absurdly large) can't break layout, thrash
    /// memory, or spin a scanner.
    ///
    /// Called on every load *and* after every in-app edit, so the two paths can't
    /// disagree about what a valid value is. Silent by design: clamping isn't an
    /// error, it's the floor of a knob the user reached past.
    pub fn clamp(&mut self) {
        self.appearance.ui_font_size = clamp_font_size(self.appearance.ui_font_size);
        self.editor.font_size = clamp_font_size(self.editor.font_size);
        self.editor.line_height = if self.editor.line_height.is_finite() {
            self.editor.line_height.clamp(1.0, 3.0)
        } else {
            1.5
        };
        self.data.page_size = self.data.page_size.clamp(MIN_PAGE_SIZE, MAX_PAGE_SIZE);
        self.data.max_cell_chars = self
            .data
            .max_cell_chars
            .clamp(MIN_CELL_CHARS, MAX_CELL_CHARS);
        self.data.copy_row_limit = self
            .data
            .copy_row_limit
            .clamp(MIN_COPY_ROW_LIMIT, MAX_COPY_ROW_LIMIT);
        self.kv.max_resident_keys = self
            .kv
            .max_resident_keys
            .clamp(MIN_RESIDENT_KEYS, MAX_RESIDENT_KEYS);
        self.kv.preview_count = self
            .kv
            .preview_count
            .clamp(MIN_PREVIEW_COUNT, MAX_PREVIEW_COUNT);
        self.doc.max_columns = self.doc.max_columns.clamp(MIN_DOC_COLUMNS, MAX_DOC_COLUMNS);
    }
}

// --- keymap ------------------------------------------------------------------

/// Keyboard-behaviour settings. `vim_mode` layers vim-style navigation
/// (`hjkl`, `g`/`G`, `Ctrl-d`/`Ctrl-u`) onto the result grid and the history dock
/// on top of the existing arrow-key navigation; off by default so modality is
/// never imposed on anyone who didn't ask for it. Live-applied, so flipping it
/// takes effect without a restart. (Per-key rebinds still live in `keymap.toml`.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeymapSettings {
    pub vim_mode: bool,
    /// The modifier that, held alone, reveals a jump hint on every focusable
    /// surface. [`FocusTrigger::Off`] disables the overlay entirely.
    pub focus_overlay: FocusTrigger,
    /// How long the trigger must be held before hints appear. A short delay is
    /// what keeps the overlay out of the way of ordinary chords: the modifier is
    /// released (or joined by a second one) long before it elapses.
    pub focus_overlay_delay_ms: u64,
    /// Which characters the hints are drawn from. Letters by default because
    /// they are the only ones typable without Shift on every layout; see
    /// [`HintAlphabet`].
    pub focus_overlay_hints: HintAlphabet,
}

impl Default for KeymapSettings {
    fn default() -> Self {
        Self {
            vim_mode: false,
            focus_overlay: FocusTrigger::Alt,
            focus_overlay_delay_ms: 250,
            focus_overlay_hints: HintAlphabet::Letters,
        }
    }
}

/// Which characters the focus hints are drawn from.
///
/// Digits look like the obvious choice and are the wrong default: several Latin
/// layouts (Czech, French AZERTY) put the number row's digits on the *shifted*
/// level, so a digit hint on those keyboards cannot be typed without adding a
/// modifier to a gesture that is defined by holding exactly one. Letters are
/// unshifted everywhere. Offered as a setting because on a layout with unshifted
/// digits they are genuinely nicer to aim at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HintAlphabet {
    #[default]
    Letters,
    Digits,
}

/// Which held modifier reveals the focus hints.
///
/// Defaults to Alt rather than the platform-primary modifier because Cmd/Ctrl
/// prefixes nearly every binding RED has: armed on that, the overlay would be
/// racing the user's own chords all day, and the delay would only mask it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusTrigger {
    Off,
    #[default]
    Alt,
    /// Cmd on macOS, Ctrl elsewhere — the same mapping the keymap's `cmd-` prefix
    /// uses, so this reads as "the modifier my shortcuts start with".
    Primary,
    Shift,
    Control,
}

impl FocusTrigger {
    /// Whether this trigger's own modifier is down.
    fn is_down(self, mods: gpui::Modifiers) -> bool {
        match self {
            FocusTrigger::Off => false,
            FocusTrigger::Alt => mods.alt,
            // Mirrors the keymap's `cmd-` → `secondary` rewrite, so "Cmd/Ctrl"
            // means the modifier the rest of RED's shortcuts start with.
            FocusTrigger::Primary => {
                if cfg!(target_os = "macos") {
                    mods.platform
                } else {
                    mods.control
                }
            }
            FocusTrigger::Shift => mods.shift,
            FocusTrigger::Control => mods.control,
        }
    }

    /// Whether `mods` is this trigger held on its own — the gesture that reveals
    /// the hints. Any *other* modifier joining means the user is building a
    /// chord, so ⌥⌘\ never flashes hints on its way to splitting a pane.
    ///
    /// Shift is the deliberate exception: it is tolerated alongside the trigger
    /// rather than counted as a second modifier. On a layout whose digits live
    /// on the shifted level, typing a digit hint *requires* Shift, and treating
    /// that as a chord would put those hints permanently out of reach. Nothing
    /// is shadowed by the allowance, since RED binds no trigger+Shift chord.
    pub fn held_alone(self, mods: gpui::Modifiers) -> bool {
        if self == FocusTrigger::Off || !self.is_down(mods) {
            return false;
        }
        // Everything except Shift and the trigger's own flag. Anything left
        // standing is a real second modifier.
        let mut rest = gpui::Modifiers {
            shift: false,
            ..mods
        };
        match self {
            FocusTrigger::Alt => rest.alt = false,
            FocusTrigger::Primary => {
                if cfg!(target_os = "macos") {
                    rest.platform = false;
                } else {
                    rest.control = false;
                }
            }
            FocusTrigger::Control => rest.control = false,
            FocusTrigger::Shift | FocusTrigger::Off => {}
        }
        rest.number_of_modifiers() == 0
    }
}

#[cfg(test)]
mod focus_trigger_tests {
    use super::FocusTrigger;
    use gpui::Modifiers;

    fn mods(alt: bool, shift: bool, platform: bool, control: bool) -> Modifiers {
        Modifiers {
            alt,
            shift,
            platform,
            control,
            function: false,
        }
    }

    #[test]
    fn the_bare_trigger_reveals_hints() {
        assert!(FocusTrigger::Alt.held_alone(mods(true, false, false, false)));
        assert!(FocusTrigger::Control.held_alone(mods(false, false, false, true)));
        assert!(FocusTrigger::Shift.held_alone(mods(false, true, false, false)));
    }

    /// The regression this exists for: a Czech keyboard needs Shift to type a
    /// digit, so trigger+Shift has to keep the hints up.
    #[test]
    fn shift_alongside_the_trigger_is_tolerated() {
        assert!(FocusTrigger::Alt.held_alone(mods(true, true, false, false)));
        assert!(FocusTrigger::Control.held_alone(mods(false, true, false, true)));
    }

    /// Any other modifier means a chord is being built, so the hints stay away.
    #[test]
    fn a_second_modifier_cancels() {
        assert!(!FocusTrigger::Alt.held_alone(mods(true, false, true, false)));
        assert!(!FocusTrigger::Alt.held_alone(mods(true, true, false, true)));
        assert!(!FocusTrigger::Control.held_alone(mods(true, false, false, true)));
    }

    #[test]
    fn a_trigger_that_is_not_down_never_matches() {
        assert!(!FocusTrigger::Alt.held_alone(mods(false, true, false, false)));
        assert!(!FocusTrigger::Off.held_alone(mods(true, false, false, false)));
        assert!(!FocusTrigger::Off.held_alone(Modifiers::default()));
    }
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
    /// The UI language: `"system"` to follow the OS, or a locale code with a
    /// catalog compiled in (`crate::i18n::available`). An unrecognised value
    /// degrades to English rather than failing the load, so a typo here costs a
    /// language and not the app.
    pub locale: String,
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
            locale: "system".to_string(),
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
        // Follow the OS appearance out of the box, on RED's brand-red Ayu pair.
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

// --- data view ---------------------------------------------------------------

/// Data-view behaviour, tuned for fast browsing of large result sets, and shared
/// by **every** grid: the SQL result grid, the Redis key list, and the MongoDB
/// document grid. A knob that only one seam can honour says so on its field.
///
/// Was `[grid]` before 0.20, when the only grid was the SQL one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DataSettings {
    pub density: Density,
    /// Show the leading row-number gutter. SQL results only; the key list and the
    /// document grid have no gutter to show.
    pub row_numbers: bool,
    /// What a SQL `NULL` renders as (e.g. `∅`, `NULL`, or blank). SQL results
    /// only: a missing BSON field renders as an empty cell and a Redis value is
    /// never null.
    pub null_display: String,
    /// Hard cap on the characters of any one cell a grid keeps resident, the
    /// fat-cell memory rail. Honored by the SQL grid (pushed to the driver as the
    /// display cell cap) and by the document grid's extended-JSON cells. Clamped
    /// to a sane range.
    pub max_cell_chars: usize,
    /// The streaming/keyset fetch window: how many rows a page request pulls.
    /// Honored by the SQL result cursor, the document keyset window, and (as the
    /// soft target per `SCAN` round trip) the Redis key list.
    pub page_size: usize,
    /// Row threshold above which the column-stats bar withholds the (potentially
    /// full-scan) `count(distinct)` until the user explicitly asks for it, so
    /// selecting a column never silently launches a heavy query on a huge result.
    /// SQL results only (the stats bar is a SQL surface).
    pub stats_distinct_max_rows: usize,
    /// Ceiling on rows a select-all / whole-column copy pulls into the clipboard.
    /// A clipboard is held whole in memory, so this bounds the worst-case spike of
    /// a runaway copy; the copy path warns the user when a selection is clipped.
    /// Clamped to a sane range.
    pub copy_row_limit: usize,
}

impl Default for DataSettings {
    fn default() -> Self {
        Self {
            density: Density::default(),
            row_numbers: true,
            null_display: "NULL".to_string(),
            max_cell_chars: 4096,
            page_size: 200,
            stats_distinct_max_rows: 1_000_000,
            copy_row_limit: 100_000,
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

    /// The value's spelling in `settings.toml`. The settings panel round-trips
    /// enums through these strings, so a control and the file always agree on the
    /// vocabulary, and a panel row can name the exact key a user would hand-edit.
    pub fn as_str(self) -> &'static str {
        match self {
            Density::Compact => "compact",
            Density::Comfortable => "comfortable",
            Density::Spacious => "spacious",
        }
    }

    /// Parse a spelling from [`Self::as_str`]; anything else takes the default.
    pub fn from_str(s: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|d| d.as_str() == s)
            .unwrap_or_default()
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

// --- safety ------------------------------------------------------------------

/// The lowest [`RiskLevel`] that has to be confirmed before it runs.
///
/// A threshold rather than a boolean because a prompt that fires on ordinary work
/// is the thing that gets safety rails switched off: with one switch, silencing the
/// routine `UPDATE … WHERE id = 42` also silenced `DROP DATABASE`. Each level here
/// silences strictly less than the one below it.
/// Variants are ordered least to most permissive, so `min` is "the stricter of the
/// two" and `max` is "the more relaxed of the two". [`ConfirmPolicy::resolve`]
/// leans on that to clamp the user's setting per environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmThreshold {
    /// Confirm anything that writes at all, including a filtered `UPDATE`/`DELETE`
    /// and plain DDL. The most cautious setting, and noisy by design.
    Write,
    /// Confirm anything that reaches further than it names: an unfiltered
    /// `UPDATE`/`DELETE`, a privilege change, a `DROP`. The default.
    #[default]
    Risky,
    /// Confirm only what destroys a whole object (`DROP TABLE`, `TRUNCATE`).
    Critical,
    /// Never confirm. Only reachable from settings, never from a modal's
    /// "Don't ask again", so it cannot be arrived at by reflex.
    Never,
}

impl ConfirmThreshold {
    /// Least to most permissive, the order the settings control lays them out in,
    /// so moving right is always "ask me less".
    pub const ALL: [ConfirmThreshold; 4] = [
        ConfirmThreshold::Write,
        ConfirmThreshold::Risky,
        ConfirmThreshold::Critical,
        ConfirmThreshold::Never,
    ];

    /// The value's spelling in `settings.toml` (see [`Density::as_str`]).
    pub fn as_str(self) -> &'static str {
        match self {
            ConfirmThreshold::Write => "write",
            ConfirmThreshold::Risky => "risky",
            ConfirmThreshold::Critical => "critical",
            ConfirmThreshold::Never => "never",
        }
    }

    /// Parse a spelling from [`Self::as_str`]; anything else takes the default,
    /// which is the *stricter* end of the scale rather than the permissive one.
    pub fn from_str(s: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == s)
            .unwrap_or_default()
    }

    /// Whether a statement graded `level` must be confirmed before it runs.
    pub fn requires(self, level: RiskLevel) -> bool {
        match self {
            Self::Write => level >= RiskLevel::Write,
            Self::Risky => level >= RiskLevel::Risky,
            Self::Critical => level >= RiskLevel::Critical,
            Self::Never => false,
        }
    }
}

/// How confirmations behave for one connection: the global setting, clamped by
/// where that connection points.
///
/// Resolved once at the point a statement is about to run, so no caller has to
/// remember the environment rules, and so a `Prod` connection cannot be made lenient
/// by a setting or a checkbox somewhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfirmPolicy {
    /// The threshold actually in force here.
    pub threshold: ConfirmThreshold,
    /// The lowest grade whose confirmation must be typed out rather than clicked.
    pub type_from: RiskLevel,
    /// Whether a confirmation may offer "Don't ask again".
    pub allow_quiet: bool,
}

impl ConfirmPolicy {
    /// Combine the configured threshold with a connection's environment.
    ///
    /// The clamps are one-directional on purpose. `Local` may only *relax* the
    /// setting and `Prod` may only *tighten* it, so neither can be surprised by a
    /// global preference: someone who set `write` because they want to be asked
    /// about everything still gets that on production, and someone who set `never`
    /// still gets asked before dropping a production table.
    pub fn resolve(threshold: ConfirmThreshold, env: ConnEnv) -> Self {
        match env {
            // No marker, or an environment where a mistake is recoverable: exactly
            // what the user configured.
            ConnEnv::Unset | ConnEnv::Dev | ConnEnv::Staging => Self {
                threshold,
                type_from: RiskLevel::Critical,
                allow_quiet: true,
            },
            // Scratch: never ask about anything short of destroying an object. This
            // is the release valve that makes strictness elsewhere tolerable.
            ConnEnv::Local => Self {
                threshold: threshold.max(ConfirmThreshold::Critical),
                type_from: RiskLevel::Critical,
                allow_quiet: true,
            },
            // Production: ask from `Risky` up whatever the setting says, make every
            // one of those confirmations typed rather than clicked, and offer no way
            // to switch them off from the dialog. Changing this is a deliberate act
            // in the connection's settings, not a checkbox at the moment of hurry.
            ConnEnv::Prod => Self {
                threshold: threshold.min(ConfirmThreshold::Risky),
                type_from: RiskLevel::Risky,
                allow_quiet: false,
            },
        }
    }

    /// Whether a statement graded `level` must be confirmed before it runs.
    pub fn requires(&self, level: RiskLevel) -> bool {
        self.threshold.requires(level)
    }

    /// Whether confirming `level` means typing the object's name out.
    pub fn requires_typing(&self, level: RiskLevel) -> bool {
        level >= self.type_from
    }

    /// Whether a delete outside the SQL editor (a Redis key, a MongoDB document)
    /// should confirm first.
    ///
    /// Asked at [`RiskLevel::Risky`], because that is what those actions are: they
    /// destroy specific, named data rather than a whole object. That grading is what
    /// keeps their "Don't ask again" from reaching past them, since silencing `Risky`
    /// leaves `Critical` (and so `DROP TABLE`) still gated.
    pub fn confirms_delete(&self) -> bool {
        self.requires(RiskLevel::Risky)
    }
}

/// The guards that stand between the user and losing data. Cross-engine on
/// purpose: [`confirm_from`](Self::confirm_from) grades a SQL statement, a Redis
/// key delete, and a MongoDB document delete on one scale, so "how much does RED
/// ask me" is one answer rather than three.
///
/// Was part of `[query]` before 0.20, which filed the cross-engine guards under a
/// SQL-shaped name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetySettings {
    /// How dangerous an action has to be before RED asks first. Replaces the old
    /// `confirm_destructive` boolean, which [`apply_legacy`] migrates.
    pub confirm_from: ConfirmThreshold,
    /// Confirm before closing a tab that holds unsaved work. The tab-close modal's
    /// "Don't ask again" checkbox flips this off.
    pub confirm_close_tab: bool,
    /// Ask the configured AI agent for a second opinion on a statement the confirm
    /// dialog already stopped, shown as one advisory line.
    ///
    /// Off by default, and off is the honest default: enabling it sends the
    /// statement and a summary of the schema to the configured provider. It is
    /// also never a gate. The verdict is displayed and nothing more, so an
    /// unavailable, slow, or mistaken model can only ever cost a line of text.
    pub ai_review: bool,
}

impl Default for SafetySettings {
    fn default() -> Self {
        Self {
            confirm_from: ConfirmThreshold::default(),
            confirm_close_tab: true,
            // Opt-in: it sends SQL off the machine.
            ai_review: false,
        }
    }
}

// --- sql ---------------------------------------------------------------------

/// The SQL seam: knobs that only mean something for a `DatabaseDriver` engine.
/// RED's on-brand big-result defaults live here.
///
/// Was the rest of `[query]` before 0.20.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SqlSettings {
    /// Append `LIMIT n` to a bare `SELECT *` so a fat table can't flood the grid.
    /// `0` disables the auto-limit.
    pub auto_limit: u32,
    /// Abort a query (and each of its page/run fetches) that runs longer than this
    /// many seconds, so a runaway can't wedge the grid. `0` disables the cap.
    pub statement_timeout: u32,
    /// Interval a newly-opened watch starts at, in seconds. `0` leaves watch off
    /// until asked for, which is the default: an interval that arms itself is a
    /// repeated query nobody requested.
    pub watch_default_secs: u64,
    /// Floor under any watch interval, in seconds. A watch is a query on a loop,
    /// so this is a load guard, not a preference; a production connection raises
    /// it further (see `AppState::set_watch`).
    pub watch_min_secs: u64,
}

impl Default for SqlSettings {
    fn default() -> Self {
        Self {
            auto_limit: 1000,
            statement_timeout: 0,
            // Off: a watch is opt-in per tab.
            watch_default_secs: 0,
            watch_min_secs: 2,
        }
    }
}

impl SqlSettings {
    /// The default watch interval, or `None` when watch is off by default.
    pub fn watch_default(&self) -> Option<std::time::Duration> {
        (self.watch_default_secs > 0).then(|| {
            std::time::Duration::from_secs(self.watch_default_secs.max(self.watch_min_secs))
        })
    }

    /// The statement timeout as a duration, or `None` when disabled (`0`).
    pub fn timeout(&self) -> Option<std::time::Duration> {
        (self.statement_timeout > 0)
            .then(|| std::time::Duration::from_secs(self.statement_timeout as u64))
    }
}

// --- behavior ----------------------------------------------------------------

/// Session behaviour. `restore_last_session` is modeled but not yet wired (it
/// touches the connection lifecycle + keychain, a follow-up). `false` is the
/// derived default.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorSettings {
    pub restore_last_session: bool,
    /// The interval the Server panel starts auto-refreshing at (`0` = off, the
    /// default); changeable per connection from the panel's own pill.
    ///
    /// Lives under `[behavior]` rather than a seam section because the panel is
    /// one dock over all three seams. Off by default and floored by
    /// [`Self::server_refresh_interval`]: a five-second poll of `CLIENT LIST` or
    /// `pg_stat_activity` against production is a real second workload, so this
    /// is opt-in and the live interval is shown in the panel rather than only
    /// here.
    pub server_refresh_secs: u64,
}

impl BehaviorSettings {
    /// The Server panel's default auto-refresh interval, or `None` when off.
    /// A non-zero value is clamped to
    /// [`MIN_REFRESH_SECS`](crate::server_panel::MIN_REFRESH_SECS) so a stray
    /// hand-edit cannot turn the panel into a poller.
    pub fn server_refresh_interval(&self) -> Option<std::time::Duration> {
        (self.server_refresh_secs > 0).then(|| {
            std::time::Duration::from_secs(
                self.server_refresh_secs
                    .max(crate::server_panel::MIN_REFRESH_SECS),
            )
        })
    }
}

// --- kv ----------------------------------------------------------------------

/// The key-value seam: the Redis key browser today, and any future `KvDriver`
/// engine unchanged (Valkey, DragonflyDB), which is why the section is named for
/// the seam rather than for Redis.
///
/// Was `[redis]` before 0.20, where it held exactly one key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KvSettings {
    /// The interval a new Browse tab starts auto-refreshing its key scan at
    /// (`0` = off, the default); changeable per-tab from the browse toolbar's
    /// actions menu. Clamped to a small floor by
    /// [`Self::auto_refresh_interval`] so a hand-edited tiny value can't turn
    /// the scan into a tight loop.
    pub auto_refresh_secs: u64,
    /// The query mode a new Browse tab's filter box starts in.
    pub default_query_mode: KvQueryMode,
    /// Soft cap on the keys one browse tab keeps resident. The key list is
    /// append-only with evict-oldest beyond this, so a very long unfiltered
    /// browse session can't grow the list forever. Clamped.
    pub max_resident_keys: usize,
    /// How many elements of a collection value (list, stream) the inspector
    /// pulls per preview window. Clamped.
    pub preview_count: usize,
}

impl Default for KvSettings {
    fn default() -> Self {
        Self {
            auto_refresh_secs: 0,
            default_query_mode: KvQueryMode::default(),
            max_resident_keys: 20_000,
            preview_count: 200,
        }
    }
}

impl KvSettings {
    /// The default auto-refresh interval as a `Duration`, or `None` when off
    /// (`0`). A non-zero value is clamped to a 1-second floor so a stray tiny
    /// setting can't spin the scanner.
    pub fn auto_refresh_interval(&self) -> Option<std::time::Duration> {
        (self.auto_refresh_secs > 0)
            .then(|| std::time::Duration::from_secs(self.auto_refresh_secs.max(1)))
    }
}

/// How the key browser's filter box reads its text. The persisted mirror of the
/// browse toolbar's query-mode dropdown; the config layer owns the vocabulary
/// (like [`Density`]) and the UI maps onto it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KvQueryMode {
    /// A raw `SCAN … MATCH` glob (`user:*`). How the box has always behaved.
    #[default]
    Glob,
    /// A literal prefix, scanned as `MATCH <escaped>*`.
    Prefix,
    /// One exact key name, resolved directly (bypasses `SCAN`).
    Exact,
    /// Client-side fuzzy match over loaded keys.
    Fuzzy,
    /// Substring search over string *values*; runs on Enter.
    Value,
}

impl KvQueryMode {
    /// The modes in dropdown order.
    pub const ALL: [KvQueryMode; 5] = [
        KvQueryMode::Glob,
        KvQueryMode::Prefix,
        KvQueryMode::Exact,
        KvQueryMode::Fuzzy,
        KvQueryMode::Value,
    ];

    /// The value's spelling in `settings.toml` (see [`Density::as_str`]).
    pub fn as_str(self) -> &'static str {
        match self {
            KvQueryMode::Glob => "glob",
            KvQueryMode::Prefix => "prefix",
            KvQueryMode::Exact => "exact",
            KvQueryMode::Fuzzy => "fuzzy",
            KvQueryMode::Value => "value",
        }
    }

    /// Parse a spelling from [`Self::as_str`]; anything else takes the default.
    pub fn from_str(s: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|m| m.as_str() == s)
            .unwrap_or_default()
    }
}

// --- doc ---------------------------------------------------------------------

/// The document seam: the MongoDB browser today, and any future `DocDriver`
/// engine unchanged. The page size and the cell cap are **not** here; those are
/// [`DataSettings`], shared with every other grid.
///
/// New in 0.20; before that the document browser had no settings at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DocSettings {
    /// The view a newly-opened collection tab starts in.
    pub default_view: DocView,
    /// The most top-level fields the sampled-column table shows. A wider document
    /// is still whole in List/JSON and the inspector; this keeps the table
    /// readable on documents with dozens of fields. Clamped.
    pub max_columns: usize,
}

impl Default for DocSettings {
    fn default() -> Self {
        Self {
            default_view: DocView::default(),
            max_columns: 12,
        }
    }
}

/// How a document collection renders. The persisted mirror of the collection
/// toolbar's view toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocView {
    /// A sampled-column table: the compact, spreadsheet-like default.
    #[default]
    Table,
    /// One expandable card per document.
    List,
    /// One pretty extended-JSON block per document.
    Json,
}

impl DocView {
    /// The views in toggle order.
    pub const ALL: [DocView; 3] = [DocView::Table, DocView::List, DocView::Json];

    /// The value's spelling in `settings.toml` (see [`Density::as_str`]).
    pub fn as_str(self) -> &'static str {
        match self {
            DocView::Table => "table",
            DocView::List => "list",
            DocView::Json => "json",
        }
    }

    /// Parse a spelling from [`Self::as_str`]; anything else takes the default.
    pub fn from_str(s: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|v| v.as_str() == s)
            .unwrap_or_default()
    }
}

// --- update ------------------------------------------------------------------

/// macOS self-update behaviour. `auto_update =
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
/// **not** live here; it routes through the OS keyring (see `crate::secrets`),
/// the same secret store connection passwords use. Only the non-secret knobs
/// (provider, model, the thinking-display toggle) are persisted in `settings.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    /// The master switch. `false` is a true kill switch: no panel entry point,
    /// no MCP server, no agent process. A connection can override it.
    pub enabled: bool,
    /// Which backend handles turns:
    /// - `"anthropic"` (default): the Claude Messages API, billed to an API key.
    /// - `"subscription"`: Claude Code over ACP, billed to the user's Pro/Max
    ///   subscription (the agent owns its own login; no key needed).
    pub provider: String,
    /// Database access tier the assistant's tools run at: `"off"` (no DB
    /// tools), `"schema"` (structure only, no row data), or `"read"` (the full
    /// read catalog). A connection can override it; unknown values resolve to
    /// `"read"`.
    pub tier: String,
    /// Model id, e.g. `claude-opus-4-8`. Empty falls back to the Opus default.
    /// (API-key path only; the subscription agent picks its own model.)
    pub model: String,
    /// Default subscription-agent model selector, as the agent's opaque value id
    /// (set from the composer's dropdown). Applied to each new chat's session; empty
    /// lets the agent keep its own default. Last choice wins: changing the dropdown
    /// rewrites this, but never retroactively changes already-open chats.
    pub subscription_model: String,
    /// Default subscription-agent reasoning-level selector (opaque value id), same
    /// semantics as `subscription_model`.
    pub subscription_reasoning: String,
    /// Default subscription-agent permission-mode selector (opaque value id): the
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
    /// Count the rows an agent write would affect and show them (with a few of
    /// them) in the approval prompt. On by default: "Allow this?" over a statement
    /// whose blast radius is invisible is a question nobody can answer, so the
    /// realistic outcome is a rubber stamp. Turn it off on a slow link, where the
    /// round-trips before every prompt cost more than the number is worth.
    pub preview_writes: bool,
    /// How long a review transaction may sit unresolved before RED rolls it back,
    /// in seconds. An open transaction holds locks, so a user who walks away
    /// mid-review can block production writes; rolling back is the only defensible
    /// expiry. Floored at 30s so the card is always answerable.
    pub sandbox_timeout_secs: u64,
    /// Resource guards on the `read` tier (`[ai.limits]`).
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
            preview_writes: true,
            sandbox_timeout_secs: 120,
            limits: AiLimitsSettings::default(),
        }
    }
}

/// The two built-in agent ids. Kept byte-stable: `"anthropic"` is the keyring
/// account (`ai-key:anthropic`) and the binding old saved chats persist, and both
/// are what legacy configs synthesize; renaming would orphan keys and chats.
pub const BUILTIN_API_AGENT: &str = "anthropic";
pub const BUILTIN_ACP_AGENT: &str = "subscription";

/// One user-defined agent profile (`[[ai.agents]]`). `kind` selects the backend:
/// `"api"` (the Messages API via `red-ai`, optionally at a custom `base_url`) or
/// `"acp"` (an external agent over ACP via `red-acp`, launched by `command`:
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
    /// Ceiling on the tokens one model reply may generate.
    pub max_output_tokens: u32,
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
            max_output_tokens: d.max_output_tokens,
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
                // they wanted the assistant on; disable it rather than reverting to
                // the permissive `AiSettings::default()`.
                return LoadReport {
                    settings: Settings {
                        ai: AiSettings::disabled(),
                        ..Settings::default()
                    },
                    warnings: vec![format!(
                        "settings.toml isn't valid TOML ({e}): using defaults; the assistant is \
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
            data: section(&value, "data", &mut warnings),
            safety: section(&value, "safety", &mut warnings),
            sql: section(&value, "sql", &mut warnings),
            kv: section(&value, "kv", &mut warnings),
            doc: section(&value, "doc", &mut warnings),
            behavior: section(&value, "behavior", &mut warnings),
            update: section(&value, "update", &mut warnings),
            ai: ai_section(&value, &mut warnings),
            keymap: section(&value, "keymap", &mut warnings),
        };
        let migrated = apply_legacy(&mut settings, &value, &mut warnings);
        settings.clamp();

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

/// The font sizes (px) the UI will accept. A value outside this range (or a NaN
/// / infinity) falls back to the safe floor rather than breaking layout.
pub const MIN_FONT_SIZE: f32 = 8.0;
pub const MAX_FONT_SIZE: f32 = 32.0;

/// The keyset/offset fetch window (`data.page_size`) a grid will accept. Below
/// the floor paging stalls; above the ceiling a single page can spike RAM (the
/// resident buffer is bounded by a multiple of the page).
pub const MIN_PAGE_SIZE: usize = 20;
pub const MAX_PAGE_SIZE: usize = 5_000;

/// The fat-cell display rail (`data.max_cell_chars`). The floor keeps a cell
/// readable; the ceiling bounds the per-cell bytes the driver materializes for a
/// display page (export stays full-fidelity regardless).
pub const MIN_CELL_CHARS: usize = 256;
pub const MAX_CELL_CHARS: usize = 1_048_576;

/// The clipboard copy ceiling (`data.copy_row_limit`). The floor keeps a copy
/// useful; the ceiling matches the backend's hard `MAX_COPY_ROWS` backstop so the
/// user-facing limit can never ask for more than the service will hand back.
pub const MIN_COPY_ROW_LIMIT: usize = 1_000;
pub const MAX_COPY_ROW_LIMIT: usize = 1_000_000;

/// The key browser's resident-row rail (`kv.max_resident_keys`). The floor keeps
/// a scroll-back worth having; the ceiling is the memory rail itself.
pub const MIN_RESIDENT_KEYS: usize = 1_000;
pub const MAX_RESIDENT_KEYS: usize = 500_000;

/// The collection-preview window (`kv.preview_count`): how many list/stream
/// elements one inspector fetch pulls.
pub const MIN_PREVIEW_COUNT: usize = 10;
pub const MAX_PREVIEW_COUNT: usize = 5_000;

/// The document table's column budget (`doc.max_columns`). One column is
/// useless; past the ceiling the table stops being readable and List/JSON is the
/// right view anyway.
pub const MIN_DOC_COLUMNS: usize = 2;
pub const MAX_DOC_COLUMNS: usize = 64;

fn clamp_font_size(size: f32) -> f32 {
    if size.is_finite() {
        size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    } else {
        13.0
    }
}

/// Deserialize one named section independently, defaulting (with a warning) if it
/// is present but unreadable, so a single mistyped value degrades just its own
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
                    "settings.toml: couldn't read [{key}] ({e}); keeping defaults for that section"
                ));
                T::default()
            }
        },
    }
}

/// Like [`section`] but **fails closed** for the security-sensitive `[ai]` table.
/// A malformed section disables the assistant ([`AiSettings::disabled`]) instead of
/// reverting to the permissive default (`enabled = true`, read tier), so a stray
/// hand-edit (a typo'd key, a wrong-typed value) can't silently re-enable AI
/// access against the user's intent. A missing section still uses the normal
/// default (the shipped behavior).
fn ai_section(value: &toml::Value, warnings: &mut Vec<String>) -> AiSettings {
    match value.get("ai") {
        None => AiSettings::default(),
        Some(v) => match v.clone().try_into() {
            Ok(parsed) => parsed,
            Err(e) => {
                warnings.push(format!(
                    "settings.toml: couldn't read [ai] ({e}); the assistant is disabled until \
                     it's fixed"
                ));
                AiSettings::disabled()
            }
        },
    }
}

/// Lift keys from older file shapes into the current sections once, so an old file
/// upgrades cleanly. Returns `true` when anything was migrated (the caller re-saves
/// in the new shape).
///
/// Three generations are handled, oldest first.
///
/// **Gen 1 (flat).** `theme` / `density` / `confirm_destructive` only ever existed
/// at the *top* level, so reading them there is unambiguous against today's nested
/// keys.
///
/// **Gen 2 (`[query]`).** The `confirm_destructive` boolean had a second life
/// under `[query]`, which is where all but the oldest files carry it. Both
/// spellings map onto [`ConfirmThreshold`] the same way, and `false` deliberately
/// becomes [`ConfirmThreshold::Critical`] rather than [`ConfirmThreshold::Never`]:
/// the old switch was all-or-nothing, so almost everyone who turned it off did so
/// to stop being asked about routine writes, not to consent to an unprompted
/// `DROP DATABASE`. Anyone who did mean the latter can still say so explicitly.
///
/// **Gen 3 (pre-0.20 section names).** `[grid]` → `[data]`, `[redis]` → `[kv]`,
/// and `[query]` split into `[sql]` (execution) + `[safety]` (the cross-engine
/// guards). A *present* new section always wins: once a file has been migrated
/// and re-saved, the old table is inert, so a stale `[grid]` left behind by hand
/// can't fight the `[data]` the user is actually editing.
///
/// The lifts are field-wise, not table-wise, for the sections that split. A
/// table-wise lift would drop the new section's other fields on the floor, and
/// `[query]` has to feed two destinations.
fn apply_legacy(settings: &mut Settings, value: &toml::Value, warnings: &mut Vec<String>) -> bool {
    let mut migrated = false;

    // --- gen 1: flat top-level keys ---
    if let Some(theme) = value.get("theme").and_then(|v| v.as_str()) {
        settings.appearance.theme = ThemeSetting::Named(theme.to_string());
        migrated = true;
    }
    if let Some(density) = value.get("density").and_then(|v| v.as_integer()) {
        settings.data.density = Density::from_index(density.max(0) as usize);
        migrated = true;
    }

    // --- gen 3: [grid] -> [data] ---
    if value.get("data").is_none()
        && let Some(grid) = value.get("grid")
    {
        settings.data = lift(grid, "grid", "data", warnings);
        migrated = true;
    }

    // --- gen 3: [redis] -> [kv] ---
    // Field-wise: `[kv]` carries knobs `[redis]` never had, and a table-wise
    // decode of the old shape would reset them to defaults rather than keep them.
    if value.get("kv").is_none()
        && let Some(secs) = value
            .get("redis")
            .and_then(|r| r.get("auto_refresh_secs"))
            .and_then(toml::Value::as_integer)
    {
        settings.kv.auto_refresh_secs = secs.max(0) as u64;
        migrated = true;
    }

    // --- gen 3: [query] -> [sql] + [safety] ---
    if let Some(query) = value.get("query") {
        if value.get("sql").is_none() {
            if let Some(v) = query.get("auto_limit").and_then(toml::Value::as_integer) {
                settings.sql.auto_limit = v.clamp(0, i64::from(u32::MAX)) as u32;
                migrated = true;
            }
            if let Some(v) = query
                .get("statement_timeout")
                .and_then(toml::Value::as_integer)
            {
                settings.sql.statement_timeout = v.clamp(0, i64::from(u32::MAX)) as u32;
                migrated = true;
            }
        }
        if value.get("safety").is_none() {
            if let Some(v) = query.get("confirm_from").cloned()
                && let Ok(threshold) = v.try_into()
            {
                settings.safety.confirm_from = threshold;
                migrated = true;
            }
            if let Some(v) = query
                .get("confirm_close_tab")
                .and_then(toml::Value::as_bool)
            {
                settings.safety.confirm_close_tab = v;
                migrated = true;
            }
            if let Some(v) = query.get("ai_review").and_then(toml::Value::as_bool) {
                settings.safety.ai_review = v;
                migrated = true;
            }
        }
    }

    // --- gen 1 + 2: the confirm_destructive boolean, in either home ---
    // Last, so it loses to an explicit `confirm_from` lifted just above: a file
    // carrying both is one that already moved on from the boolean.
    let legacy_confirm = value
        .get("confirm_destructive")
        .or_else(|| value.get("query")?.get("confirm_destructive"))
        .and_then(toml::Value::as_bool);
    if let Some(confirm) = legacy_confirm
        && value
            .get("safety")
            .and_then(|s| s.get("confirm_from"))
            .is_none()
        && value
            .get("query")
            .and_then(|q| q.get("confirm_from"))
            .is_none()
    {
        settings.safety.confirm_from = if confirm {
            ConfirmThreshold::Risky
        } else {
            ConfirmThreshold::Critical
        };
        migrated = true;
    }

    migrated
}

/// Decode a whole legacy table into its renamed section, warning (and keeping
/// defaults) if it won't read — the same degrade-in-place contract as [`section`],
/// but naming both the old and new section so the message is actionable.
fn lift<T: Default + DeserializeOwned>(
    value: &toml::Value,
    from: &str,
    to: &str,
    warnings: &mut Vec<String>,
) -> T {
    match value.clone().try_into() {
        Ok(parsed) => parsed,
        Err(e) => {
            warnings.push(format!(
                "settings.toml: couldn't migrate [{from}] into [{to}] ({e}); keeping defaults for \
                 that section"
            ));
            T::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The invariant the whole published-global design rests on: `publish`
    /// replaces what `global` returns, and `set_global` wakes `observe_global`
    /// watchers so a view re-renders rather than reading a stale value.
    ///
    /// If this breaks, views silently render yesterday's settings. The three
    /// call sites that must publish are listed on [`Settings::publish`].
    #[gpui::test]
    fn publishing_replaces_the_global_and_notifies_observers(cx: &mut gpui::TestAppContext) {
        use std::cell::Cell;
        use std::rc::Rc;

        let woke = Rc::new(Cell::new(0usize));
        let _sub = cx.update(|cx| {
            let mut settings = Settings::default();
            settings.keymap.vim_mode = false;
            settings.publish(cx);
            assert!(!Settings::global(cx).keymap.vim_mode);

            let woke = woke.clone();
            cx.observe_global::<GlobalSettings>(move |_| woke.set(woke.get() + 1))
        });

        cx.update(|cx| {
            let mut settings = Settings::default();
            settings.keymap.vim_mode = true;
            settings.publish(cx);
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(
                Settings::global(cx).keymap.vim_mode,
                "the reader sees the new value"
            );
        });
        assert_eq!(woke.get(), 1, "and an observer was woken exactly once");
    }

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
        settings.data.density = Density::Compact;
        settings.safety.confirm_from = ConfirmThreshold::Never;
        settings.data.null_display = "∅".into();
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
        // to the permissive default; even though `enabled = true` is set here, a
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
        // A file with only one data key keeps every other default, in every section.
        let t = temp_store();
        write(&t.store, "[data]\nnull_display = \"—\"\n");
        let loaded = t.store.load_report().settings;
        assert_eq!(loaded.data.null_display, "—");
        assert_eq!(loaded.data.density, Density::default());
        assert_eq!(loaded.data.page_size, DataSettings::default().page_size);
        assert_eq!(loaded.sql, SqlSettings::default());
        assert_eq!(loaded.safety, SafetySettings::default());
        assert_eq!(loaded.appearance, AppearanceSettings::default());
    }

    #[test]
    fn clamps_bounded_knobs_in_every_seam() {
        // Out-of-range knobs clamp to the floor / ceiling rather than thrash, in
        // each seam's section as well as the shared one.
        let t = temp_store();
        write(
            &t.store,
            "[data]\npage_size = 0\nmax_cell_chars = 1\ncopy_row_limit = 1\n\
             \n[kv]\nmax_resident_keys = 1\npreview_count = 0\n\
             \n[doc]\nmax_columns = 0\n",
        );
        let s = t.store.load_report().settings;
        assert_eq!(s.data.page_size, MIN_PAGE_SIZE);
        assert_eq!(s.data.max_cell_chars, MIN_CELL_CHARS);
        assert_eq!(s.data.copy_row_limit, MIN_COPY_ROW_LIMIT);
        assert_eq!(s.kv.max_resident_keys, MIN_RESIDENT_KEYS);
        assert_eq!(s.kv.preview_count, MIN_PREVIEW_COUNT);
        assert_eq!(s.doc.max_columns, MIN_DOC_COLUMNS);

        write(
            &t.store,
            "[data]\npage_size = 99999999\nmax_cell_chars = 999999999\ncopy_row_limit = 999999999\n\
             \n[kv]\nmax_resident_keys = 999999999\npreview_count = 999999999\n\
             \n[doc]\nmax_columns = 999999999\n",
        );
        let s = t.store.load_report().settings;
        assert_eq!(s.data.page_size, MAX_PAGE_SIZE);
        assert_eq!(s.data.max_cell_chars, MAX_CELL_CHARS);
        assert_eq!(s.data.copy_row_limit, MAX_COPY_ROW_LIMIT);
        assert_eq!(s.kv.max_resident_keys, MAX_RESIDENT_KEYS);
        assert_eq!(s.kv.preview_count, MAX_PREVIEW_COUNT);
        assert_eq!(s.doc.max_columns, MAX_DOC_COLUMNS);
    }

    /// `clamp` is the single definition of "a valid value", so the in-app edit
    /// path (which calls it directly) can't disagree with the load path.
    #[test]
    fn clamp_is_idempotent_and_repairs_nonsense() {
        let mut s = Settings::default();
        s.appearance.ui_font_size = f32::NAN;
        s.editor.line_height = 0.1;
        s.data.page_size = usize::MAX;
        s.clamp();
        assert!(s.appearance.ui_font_size.is_finite());
        assert_eq!(s.editor.line_height, 1.0);
        assert_eq!(s.data.page_size, MAX_PAGE_SIZE);
        let once = s.clone();
        s.clamp();
        assert_eq!(s, once);
    }

    #[test]
    fn statement_timeout_parses_and_maps_to_duration() {
        let q: SqlSettings = toml::from_str("statement_timeout = 30").expect("timeout");
        assert_eq!(q.statement_timeout, 30);
        assert_eq!(q.timeout(), Some(std::time::Duration::from_secs(30)));
        // The default (and an explicit 0) disables the cap.
        assert_eq!(SqlSettings::default().timeout(), None);
    }

    #[test]
    fn one_bad_section_does_not_reset_the_rest() {
        // `density` wants a string; an integer fails *only* the data section.
        let t = temp_store();
        write(&t.store, "[data]\ndensity = 7\n\n[sql]\nauto_limit = 50\n");
        let report = t.store.load_report();
        assert_eq!(report.settings.data, DataSettings::default());
        assert_eq!(report.settings.sql.auto_limit, 50);
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn seam_sections_round_trip() {
        let t = temp_store();
        let mut settings = Settings::default();
        settings.kv.default_query_mode = KvQueryMode::Fuzzy;
        settings.kv.auto_refresh_secs = 5;
        settings.doc.default_view = DocView::Json;
        settings.doc.max_columns = 20;
        t.store.save(&settings).unwrap();
        let back = t.store.load_report().settings;
        assert_eq!(back, settings);
        assert_eq!(
            back.kv.auto_refresh_interval(),
            Some(std::time::Duration::from_secs(5))
        );
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
    fn each_threshold_silences_strictly_less_than_the_one_below() {
        use ConfirmThreshold as T;
        let levels = [
            RiskLevel::Safe,
            RiskLevel::Write,
            RiskLevel::Risky,
            RiskLevel::Critical,
        ];
        // A read is never confirmed, whatever the threshold.
        for t in [T::Write, T::Risky, T::Critical, T::Never] {
            assert!(!t.requires(RiskLevel::Safe), "{t:?} confirmed a read");
        }
        // Each threshold asks about a suffix of the levels, and each step right asks
        // about strictly fewer. This is the property the whole design rests on: no
        // setting can silence a worse statement while still asking about a milder one.
        let asks = |t: T| levels.iter().filter(|&&l| t.requires(l)).count();
        assert!(asks(T::Write) > asks(T::Risky));
        assert!(asks(T::Risky) > asks(T::Critical));
        assert!(asks(T::Critical) > asks(T::Never));
        assert_eq!(asks(T::Never), 0);
        // The default asks about the two grades that reach past what they name.
        assert!(T::default().requires(RiskLevel::Risky));
        assert!(T::default().requires(RiskLevel::Critical));
        // ...and not about a filtered write, which is what made the old gate noisy.
        assert!(!T::default().requires(RiskLevel::Write));
        // `Critical` still confirms a drop: that is the floor a modal checkbox can
        // lower the setting to, and the reason it can never reach `Never`.
        assert!(T::Critical.requires(RiskLevel::Critical));
        assert!(!T::Critical.requires(RiskLevel::Risky));
    }

    #[test]
    fn non_sql_deletes_follow_the_risky_grade() {
        // Deleting a Redis key or a Mongo document destroys named data: `Risky`, not
        // `Critical`. That grading is what lets their "Don't ask again" silence
        // themselves without reaching past to `DROP TABLE`.
        for (threshold, expected) in [
            (ConfirmThreshold::Write, true),
            (ConfirmThreshold::Risky, true),
            (ConfirmThreshold::Critical, false),
            (ConfirmThreshold::Never, false),
        ] {
            let policy = ConfirmPolicy::resolve(threshold, ConnEnv::Unset);
            assert_eq!(policy.confirms_delete(), expected, "{threshold:?}");
        }
    }

    #[test]
    fn local_only_relaxes_and_prod_only_tightens() {
        use ConfirmThreshold as T;
        for setting in [T::Write, T::Risky, T::Critical, T::Never] {
            // An unmarked connection is exactly the setting, so adding the marker to
            // `ConnectionConfig` changes nothing for a saved connection.
            let unset = ConfirmPolicy::resolve(setting, ConnEnv::Unset);
            assert_eq!(unset.threshold, setting, "{setting:?}");
            assert_eq!(unset, ConfirmPolicy::resolve(setting, ConnEnv::Dev));
            assert_eq!(unset, ConfirmPolicy::resolve(setting, ConnEnv::Staging));

            // Local never asks about more than the setting would.
            let local = ConfirmPolicy::resolve(setting, ConnEnv::Local);
            assert!(local.threshold >= setting, "local tightened {setting:?}");
            // Prod never asks about less.
            let prod = ConfirmPolicy::resolve(setting, ConnEnv::Prod);
            assert!(prod.threshold <= setting, "prod relaxed {setting:?}");
        }
    }

    #[test]
    fn prod_asks_from_risky_and_wont_be_switched_off_from_a_dialog() {
        let prod = ConfirmPolicy::resolve(ConfirmThreshold::Never, ConnEnv::Prod);
        // Even with confirmations globally off, production still stops.
        assert!(prod.requires(RiskLevel::Risky));
        assert!(prod.requires(RiskLevel::Critical));
        assert!(!prod.requires(RiskLevel::Safe));
        // And every one of those is typed out, not clicked through.
        assert!(prod.requires_typing(RiskLevel::Risky));
        assert!(prod.requires_typing(RiskLevel::Critical));
        // No dialog offers a way out; that decision belongs in the connection.
        assert!(!prod.allow_quiet);
    }

    #[test]
    fn local_stops_asking_about_everything_short_of_destroying_an_object() {
        let local = ConfirmPolicy::resolve(ConfirmThreshold::Write, ConnEnv::Local);
        assert!(!local.requires(RiskLevel::Write));
        assert!(!local.requires(RiskLevel::Risky));
        // A drop is still a drop, even on a scratch database.
        assert!(local.requires(RiskLevel::Critical));
        assert!(local.requires_typing(RiskLevel::Critical));
        assert!(!local.requires_typing(RiskLevel::Risky));
        assert!(local.allow_quiet);
        // Someone who turned confirmations off entirely keeps that here.
        let off = ConfirmPolicy::resolve(ConfirmThreshold::Never, ConnEnv::Local);
        assert!(!off.requires(RiskLevel::Critical));
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
        assert_eq!(report.settings.data.density, Density::Compact);
        // The old boolean was all-or-nothing, so `false` becomes "confirm only what
        // destroys an object", not "never confirm": see `apply_legacy`.
        assert_eq!(
            report.settings.safety.confirm_from,
            ConfirmThreshold::Critical
        );
    }

    #[test]
    fn migrates_the_query_section_confirm_boolean() {
        // The shape almost every existing file is in: the boolean had moved under
        // `[query]` before it became a threshold.
        let t = temp_store();
        write(
            &t.store,
            "[query]\nconfirm_destructive = true\nauto_limit = 500\n",
        );
        let report = t.store.load_report();
        assert!(report.migrated);
        assert_eq!(report.settings.safety.confirm_from, ConfirmThreshold::Risky);
        // Migrating one key must not disturb its neighbours.
        assert_eq!(report.settings.sql.auto_limit, 500);
    }

    #[test]
    fn a_current_file_is_not_treated_as_legacy() {
        let t = temp_store();
        write(&t.store, "[safety]\nconfirm_from = \"never\"\n");
        let report = t.store.load_report();
        assert!(!report.migrated);
        assert_eq!(report.settings.safety.confirm_from, ConfirmThreshold::Never);
    }

    // --- gen 3: the 0.20 section renames ---

    /// The whole point of the rename migration: a file written by 0.19 keeps
    /// every setting it had, under the new names, without the user touching it.
    #[test]
    fn migrates_the_pre_020_section_names() {
        let t = temp_store();
        write(
            &t.store,
            "[grid]\n\
             density = \"compact\"\n\
             page_size = 500\n\
             null_display = \"∅\"\n\
             row_numbers = false\n\
             \n[query]\n\
             auto_limit = 250\n\
             statement_timeout = 30\n\
             confirm_from = \"write\"\n\
             confirm_close_tab = false\n\
             ai_review = true\n\
             \n[redis]\n\
             auto_refresh_secs = 10\n",
        );
        let report = t.store.load_report();
        assert!(report.migrated);
        let s = report.settings;

        // [grid] -> [data], whole table.
        assert_eq!(s.data.density, Density::Compact);
        assert_eq!(s.data.page_size, 500);
        assert_eq!(s.data.null_display, "∅");
        assert!(!s.data.row_numbers);

        // [query] -> [sql] (execution) + [safety] (the cross-engine guards).
        assert_eq!(s.sql.auto_limit, 250);
        assert_eq!(s.sql.statement_timeout, 30);
        assert_eq!(s.safety.confirm_from, ConfirmThreshold::Write);
        assert!(!s.safety.confirm_close_tab);
        assert!(s.safety.ai_review);

        // [redis] -> [kv], with the section's new knobs at their defaults.
        assert_eq!(s.kv.auto_refresh_secs, 10);
        assert_eq!(s.kv.default_query_mode, KvQueryMode::default());
        assert_eq!(
            s.kv.max_resident_keys,
            KvSettings::default().max_resident_keys
        );

        // The migrated file re-saves in the new shape and then reads back clean.
        t.store.save(&s).unwrap();
        let again = t.store.load_report();
        assert!(!again.migrated, "a migrated file must not re-migrate");
        assert_eq!(again.settings, s);
    }

    /// A half-migrated file (new section present, stale old one left behind by
    /// hand) must not have the old table fight the one the user is editing.
    #[test]
    fn a_present_new_section_wins_over_a_stale_old_one() {
        let t = temp_store();
        write(
            &t.store,
            "[grid]\npage_size = 100\n\n[data]\npage_size = 1000\n\
             \n[redis]\nauto_refresh_secs = 2\n\n[kv]\nauto_refresh_secs = 30\n\
             \n[query]\nauto_limit = 1\n\n[sql]\nauto_limit = 777\n",
        );
        let s = t.store.load_report().settings;
        assert_eq!(s.data.page_size, 1000);
        assert_eq!(s.kv.auto_refresh_secs, 30);
        assert_eq!(s.sql.auto_limit, 777);
    }

    /// `[query]` fed two destinations, so migrating it has to be field-wise: a
    /// file that already has `[safety]` but no `[sql]` still gets its SQL keys.
    #[test]
    fn a_split_section_migrates_field_wise() {
        let t = temp_store();
        write(
            &t.store,
            "[query]\nauto_limit = 42\nconfirm_from = \"write\"\n\
             \n[safety]\nconfirm_from = \"never\"\n",
        );
        let s = t.store.load_report().settings;
        // The half that has already moved is left alone...
        assert_eq!(s.safety.confirm_from, ConfirmThreshold::Never);
        // ...and the half that hasn't still comes across.
        assert_eq!(s.sql.auto_limit, 42);
    }

    /// The oldest boolean must not clobber a threshold the same file also
    /// carries: `confirm_from` is the newer, more specific statement of intent.
    #[test]
    fn an_explicit_threshold_beats_the_legacy_boolean() {
        let t = temp_store();
        write(
            &t.store,
            "[query]\nconfirm_destructive = false\nconfirm_from = \"write\"\n",
        );
        let s = t.store.load_report().settings;
        assert_eq!(s.safety.confirm_from, ConfirmThreshold::Write);
    }

    /// A malformed legacy table degrades to that section's defaults with a
    /// warning, exactly like a malformed current one: a bad `[grid]` must not
    /// take the rest of the file down with it.
    #[test]
    fn an_unreadable_legacy_section_warns_and_keeps_the_rest() {
        let t = temp_store();
        write(
            &t.store,
            "[grid]\ndensity = 7\n\n[query]\nauto_limit = 50\n",
        );
        let report = t.store.load_report();
        assert_eq!(report.settings.data, DataSettings::default());
        assert_eq!(report.settings.sql.auto_limit, 50);
        assert_eq!(report.warnings.len(), 1);
    }
}
