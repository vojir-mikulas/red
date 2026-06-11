//! RED's theme registry. Surfaces, text, and state colors come from Flint's stock
//! themes (the cross-repo contract: same tokens Nyx uses), but RED has its own
//! identity — a **blue** accent, not Flint's green: the Run button, the active tab
//! underline, the selected tree row, cell selection. So RED ships blue-accented
//! built-in variants and installs the chosen one as the `Theme` global.
//!
//! On top of the built-ins, users can drop theme files into
//! `<config>/red/themes/*.toml` — a small `base` (a built-in to start from) plus
//! `#RRGGBB` token overrides — and import / remove them from the settings panel.
//! The [`ThemeRegistry`] resolves a [`ThemeSetting`] (a named theme, or a
//! light/dark pair driven by OS appearance) to a concrete [`Theme`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use flint::Theme;
use gpui::{rgb, Hsla};
use serde::Deserialize;

use crate::settings::ThemeSetting;

/// One resolvable theme: its display name, light/dark family, whether it's a
/// removable user theme (and the file backing it), and the built [`Theme`].
#[derive(Clone)]
pub struct ThemeEntry {
    pub name: String,
    pub is_light: bool,
    /// `false` for built-ins (not removable); `true` for imported user themes.
    pub user: bool,
    /// The backing file, for removal — `Some` only for user themes.
    pub path: Option<PathBuf>,
    theme: Theme,
}

/// The set of themes RED can apply: the built-ins plus any imported user themes,
/// the latter overriding a built-in of the same name.
pub struct ThemeRegistry {
    entries: Vec<ThemeEntry>,
}

impl ThemeRegistry {
    /// Load the built-ins and merge in user themes from `<config>/red/themes`.
    /// Unreadable/malformed user files are skipped (logged), never fatal.
    pub fn load() -> Self {
        let mut entries = builtin_entries();
        for entry in load_user_themes() {
            // A user theme replaces a built-in of the same name (re-skinning).
            entries.retain(|e| e.name != entry.name);
            entries.push(entry);
        }
        Self { entries }
    }

    /// Resolve a [`ThemeSetting`] to a concrete theme for the current OS appearance.
    pub fn resolve(&self, setting: &ThemeSetting, os_dark: bool) -> Theme {
        self.by_name(setting.resolve(os_dark))
    }

    /// The built theme for `name`, falling back to One Dark for an unknown name
    /// (e.g. a removed user theme still referenced in settings).
    pub fn by_name(&self, name: &str) -> Theme {
        self.entries
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.theme.clone())
            .unwrap_or_else(one_dark)
    }

    /// Whether `name` is a light theme (defaults to dark for an unknown name).
    pub fn is_light(&self, name: &str) -> bool {
        self.entries
            .iter()
            .find(|e| e.name == name)
            .is_some_and(|e| e.is_light)
    }

    /// Theme names of the given family, in registry order — for the pickers.
    pub fn names(&self, light: bool) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.is_light == light)
            .map(|e| e.name.clone())
            .collect()
    }

    /// The first theme of a family, or a built-in default — the seed for a pair.
    pub fn default_name(&self, light: bool) -> String {
        self.entries
            .iter()
            .find(|e| e.is_light == light)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| {
                if light {
                    "Ayu Light".to_string()
                } else {
                    "One Dark".to_string()
                }
            })
    }

    /// Every theme, for the manager list.
    pub fn entries(&self) -> &[ThemeEntry] {
        &self.entries
    }

    /// Validate `source` as a theme file and copy it into the user themes dir.
    /// Returns the imported theme's name. Errors (bad file, no config dir, name
    /// clashing a built-in) are surfaced to the caller, not applied.
    pub fn import(source: &Path) -> Result<String> {
        let text = std::fs::read_to_string(source).context("reading the theme file")?;
        let file: ThemeFile = toml::from_str(&text).context("parsing the theme file")?;
        if file.name.trim().is_empty() {
            bail!("the theme file needs a `name`");
        }
        if is_builtin(&file.name) {
            bail!("\"{}\" is a built-in theme name — rename it", file.name);
        }
        let dir = themes_dir().context("no config directory for themes")?;
        std::fs::create_dir_all(&dir).context("creating the themes directory")?;
        let dest = dir.join(format!("{}.toml", slug(&file.name)));
        std::fs::write(&dest, &text).context("saving the theme file")?;
        Ok(file.name)
    }

    /// Delete the user theme named `name` (a no-op for a built-in or unknown name).
    pub fn remove(&self, name: &str) -> Result<()> {
        if let Some(path) = self
            .entries
            .iter()
            .find(|e| e.name == name && e.user)
            .and_then(|e| e.path.clone())
        {
            std::fs::remove_file(&path).context("removing the theme file")?;
        }
        Ok(())
    }
}

/// `<config>/red/themes`, the user-themes directory.
fn themes_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("themes"))
}

/// A filesystem-safe slug for a theme name (the file stem on import).
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = s.trim_matches('-');
    if trimmed.is_empty() {
        "theme".to_string()
    } else {
        trimmed.to_string()
    }
}

const BUILTINS: &[&str] = &["One Dark", "GitHub Dark", "Ayu Light"];

fn is_builtin(name: &str) -> bool {
    BUILTINS.contains(&name)
}

fn builtin_entries() -> Vec<ThemeEntry> {
    vec![
        ThemeEntry {
            name: "One Dark".into(),
            is_light: false,
            user: false,
            path: None,
            theme: one_dark(),
        },
        ThemeEntry {
            name: "GitHub Dark".into(),
            is_light: false,
            user: false,
            path: None,
            theme: github_dark(),
        },
        ThemeEntry {
            name: "Ayu Light".into(),
            is_light: true,
            user: false,
            path: None,
            theme: ayu_light(),
        },
    ]
}

/// Read every `*.toml` in the themes dir into an entry, skipping (with a warning)
/// any that won't parse — one bad file never blocks the others.
fn load_user_themes() -> Vec<ThemeEntry> {
    let Some(dir) = themes_dir() else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match std::fs::read_to_string(&path).ok().and_then(|t| {
            toml::from_str::<ThemeFile>(&t)
                .map_err(|e| tracing::warn!("ignoring theme {}: {e}", path.display()))
                .ok()
        }) {
            Some(file) if !file.name.trim().is_empty() => out.push(entry_from_file(file, path)),
            _ => {}
        }
    }
    out
}

/// The on-disk theme format: a `name`, a light/dark `appearance`, an optional
/// `base` built-in to start from, and any number of `#RRGGBB` token overrides.
/// Unknown token keys are ignored, so a sparse file overriding just the accent is
/// valid.
#[derive(Deserialize)]
struct ThemeFile {
    name: String,
    #[serde(default)]
    appearance: ThemeAppearance,
    /// A built-in to inherit unspecified tokens from; defaults by `appearance`.
    #[serde(default)]
    base: Option<String>,
    /// Token name → `#RRGGBB`. Captures every key besides the three above.
    #[serde(flatten)]
    tokens: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ThemeAppearance {
    Light,
    #[default]
    Dark,
}

fn entry_from_file(file: ThemeFile, path: PathBuf) -> ThemeEntry {
    let is_light = file.appearance == ThemeAppearance::Light;
    let mut theme = match file.base.as_deref() {
        Some(base) if is_builtin(base) => builtin_by_name(base),
        _ if is_light => ayu_light(),
        _ => one_dark(),
    };
    theme.name = file.name.clone();
    for (key, value) in &file.tokens {
        if let Some(color) = parse_hex(value) {
            apply_token(&mut theme, key, color);
        }
    }
    ThemeEntry {
        name: file.name,
        is_light,
        user: true,
        path: Some(path),
        theme,
    }
}

/// Override one named token on `theme`. Unknown keys are ignored so the format can
/// grow without breaking older files.
fn apply_token(theme: &mut Theme, key: &str, color: Hsla) {
    match key {
        "bg_app" => theme.bg_app = color,
        "bg_panel" => theme.bg_panel = color,
        "bg_panel_2" => theme.bg_panel_2 = color,
        "bg_elevated" => theme.bg_elevated = color,
        "bg_bar" => theme.bg_bar = color,
        "bg_hover" => theme.bg_hover = color,
        "bg_active" => theme.bg_active = color,
        "bg_selected" => theme.bg_selected = color,
        "bg_input" => theme.bg_input = color,
        "border" => theme.border = color,
        "border_soft" => theme.border_soft = color,
        "border_strong" => theme.border_strong = color,
        "text" => theme.text = color,
        "text_muted" => theme.text_muted = color,
        "text_faint" => theme.text_faint = color,
        "text_dim" => theme.text_dim = color,
        "accent" => theme.accent = color,
        "accent_hover" => theme.accent_hover = color,
        "accent_ghost" => theme.accent_ghost = color,
        "on_accent" => theme.on_accent = color,
        "green" => theme.green = color,
        "red" => theme.red = color,
        "blue" => theme.blue = color,
        "purple" => theme.purple = color,
        "yellow" => theme.yellow = color,
        "orange" => theme.orange = color,
        "cyan" => theme.cyan = color,
        _ => {}
    }
}

/// Parse `#RRGGBB` (or `RRGGBB`) into an opaque [`Hsla`]; `None` if malformed.
fn parse_hex(s: &str) -> Option<Hsla> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    Some(h(u32::from_str_radix(hex, 16).ok()?))
}

/// Hex (`0xRRGGBB`) → opaque [`Hsla`].
fn h(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn builtin_by_name(name: &str) -> Theme {
    match name {
        "GitHub Dark" => github_dark(),
        "Ayu Light" => ayu_light(),
        _ => one_dark(),
    }
}

/// One Dark with RED's blue accent (`#74ade8`), on the dark navy `on_accent`
/// the design uses for text atop the blue (`#11161d`).
pub fn one_dark() -> Theme {
    Theme {
        accent: h(0x74ade8),
        accent_hover: h(0x88bcf0),
        accent_ghost: h(0x74ade8).opacity(0.16),
        on_accent: h(0x11161d),
        ..Theme::one_dark()
    }
}

/// GitHub Dark with the design's brighter blue accent (`#58a6ff`).
pub fn github_dark() -> Theme {
    Theme {
        accent: h(0x58a6ff),
        accent_hover: h(0x79b8ff),
        accent_ghost: h(0x58a6ff).opacity(0.18),
        on_accent: h(0x0d1117),
        ..Theme::github_dark()
    }
}

/// Ayu Light with RED's blue accent — the light counterpart for `mode = light`
/// (or `mode = system` on a light OS). White text sits atop the blue.
pub fn ayu_light() -> Theme {
    Theme {
        accent: h(0x399ee6),
        accent_hover: h(0x55b4f0),
        accent_ghost: h(0x399ee6).opacity(0.14),
        on_accent: h(0xffffff),
        ..Theme::ayu_light()
    }
}
