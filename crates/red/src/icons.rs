//! Vector icons. The design uses lucide-style line icons; the SVGs are vendored
//! under `assets/icons/` and rendered through GPUI's `svg()`, which masks each
//! icon to the current `text_color` — so the icon files are monochrome templates
//! and the call site picks the color. Names map 1:1 to the design's `Icon` set
//! (`db`, `schema`, `table`, `view`, `col`, `key`, `link`, `play`, `search`,
//! `lock`, `plus`, `close`, `edit`, `trash`, `chevron`, `sort-asc`/`sort-desc`).

use gpui::{prelude::*, svg, Hsla, Pixels, Svg};

/// A `size`×`size` icon tinted `color`, loaded from `assets/icons/<name>.svg`.
pub fn icon(name: &str, size: Pixels, color: Hsla) -> Svg {
    svg()
        .path(format!("icons/{name}.svg"))
        .size(size)
        .flex_none()
        .text_color(color)
}
