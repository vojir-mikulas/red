//! RED's theme palette. The surfaces, text, and state colors come straight from
//! Flint's stock themes (the cross-repo contract: same tokens Nyx uses), but RED
//! has its own identity — a **blue** accent, not Flint's green. The design's
//! defining trait is that blue: the Run button, the active tab underline, the
//! selected tree row, cell selection. So RED installs these blue-accented
//! variants as its `Theme` global instead of editing Flint (which Nyx shares).
//!
//! Only the four accent tokens are overridden; everything else is Flint's, so
//! RED stays a faithful Flint consumer and tracks its surface palette for free.

use flint::Theme;
use gpui::{rgb, Hsla};

/// The built-in themes the settings panel offers, in display order. Each name
/// round-trips through [`by_name`] and matches the underlying Flint theme's name.
pub const THEMES: &[&str] = &["One Dark", "GitHub Dark"];

/// Resolve a persisted theme name to its blue-accented RED variant, defaulting to
/// One Dark for an unknown name (e.g. a settings file from a newer build).
pub fn by_name(name: &str) -> Theme {
    match name {
        "GitHub Dark" => github_dark(),
        _ => one_dark(),
    }
}

/// Hex (`0xRRGGBB`) → opaque [`Hsla`].
fn h(hex: u32) -> Hsla {
    rgb(hex).into()
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
