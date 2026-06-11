//! Embedded fonts and the [`gpui::AssetSource`] that serves them. Everything
//! under the workspace `assets/` dir is baked into the binary at compile time,
//! so the shipped app needs no sidecar files.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{App, AssetSource, SharedString};
use rust_embed::RustEmbed;

/// The UI (sans) font family, as registered with the text system.
pub const FONT_UI: &str = "IBM Plex Sans";
/// The monospace font family (for SQL, values, sizes) — IBM Plex Mono, the
/// family the design specifies (`--mono`).
pub const FONT_MONO: &str = "IBM Plex Mono";

const FONT_FILES: &[&str] = &[
    "fonts/IBMPlexSans-Regular.ttf",
    "fonts/IBMPlexSans-Medium.ttf",
    "fonts/IBMPlexSans-SemiBold.ttf",
    "fonts/IBMPlexSans-Bold.ttf",
    "fonts/IBMPlexMono-Regular.ttf",
    "fonts/IBMPlexMono-Medium.ttf",
    "fonts/IBMPlexMono-SemiBold.ttf",
];

#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../assets"]
#[include = "fonts/*"]
#[include = "icons/*"]
pub struct Assets;

/// The bundled, fully-commented reference settings — RED's settings docs. Baked
/// into the binary so "open default settings" works in a shipped app, and copied
/// into a fresh `settings.toml` on first open.
pub const DEFAULT_SETTINGS: &str = include_str!("../../../assets/default-settings.toml");

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(Self::get(path).map(|file| file.data))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Self::iter()
            .filter(|p| p.starts_with(path))
            .map(|p| p.as_ref().into())
            .collect())
    }
}

impl Assets {
    /// Register the vendored fonts with the text system. Call once at startup.
    pub fn load_fonts(cx: &App) -> Result<()> {
        let fonts = FONT_FILES
            .iter()
            .filter_map(|path| Self::get(path).map(|file| file.data))
            .collect();
        cx.text_system().add_fonts(fonts)
    }
}
