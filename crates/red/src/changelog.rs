//! The in-app "What's New" panel and the changelog slicer behind it.
//!
//! The single source of truth is the workspace `CHANGELOG.md` (Keep a Changelog
//! format), baked into the binary as [`crate::assets::CHANGELOG`]. The same file
//! drives the GitHub Release body (sliced in CI) and the website, so the notes a
//! user reads in-app match the ones published everywhere else.
//!
//! [`section_for`] pulls one `## [label]` section out of that file; [`current`]
//! picks the section to show — this build's version once the changelog is cut for
//! it, the in-progress `[Unreleased]` section before then, or the whole file as a
//! last resort. The panel renders the result through [`crate::markdown`].

use flint::prelude::*;
use gpui::{prelude::*, px, Context};

use crate::app::AppState;
use crate::assets::CHANGELOG;

/// This build's version (`CARGO_PKG_VERSION`) — the heading we look for in the
/// changelog, and what `local_state` records as "last seen".
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Slice the `## [label] …` section out of the bundled changelog: the heading
/// line plus every line below it up to (but not including) the next `## [`
/// heading, trailing blank lines trimmed. `None` if there's no such section.
///
/// Keys off the `## [x.y.z]`/`## [Unreleased]` bracket form the file already
/// uses, so a heading like `## [0.12.0] - 2026-06-30` matches `label = "0.12.0"`.
pub(crate) fn section_for(label: &str) -> Option<String> {
    section_in(CHANGELOG, label)
}

/// The pure slicer behind [`section_for`], split out so it can be tested against
/// a fixed sample rather than the ever-growing real changelog.
fn section_in(src: &str, label: &str) -> Option<String> {
    let header = format!("## [{label}]");
    let mut out = String::new();
    let mut in_section = false;
    for line in src.lines() {
        if in_section {
            // The next version heading ends this section.
            if line.starts_with("## [") {
                break;
            }
            out.push_str(line);
            out.push('\n');
        } else if line.starts_with(&header) {
            in_section = true;
            out.push_str(line);
            out.push('\n');
        }
    }
    in_section.then(|| out.trim_end().to_string())
}

/// The notes the "What's New" panel shows: this build's version section once the
/// changelog has been cut for it, else the in-progress `[Unreleased]` section,
/// else the whole file. Always returns something renderable.
pub(crate) fn current() -> String {
    section_for(VERSION)
        .or_else(|| section_for("Unreleased"))
        .unwrap_or_else(|| CHANGELOG.to_string())
}

impl AppState {
    /// The "What's New" overlay: this build's changelog section rendered as
    /// Markdown inside a scrollable modal. Reached from the post-update toast, the
    /// Help menu, and the `help: what's new` palette command.
    pub(crate) fn render_whats_new(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let close_view = cx.entity().downgrade();
        Modal::new("whats-new")
            .title("What's New")
            .width(px(560.))
            .focus_handle(self.modal_focus.clone())
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.toggle_whats_new(cx))
                    .ok();
            })
            .child(crate::markdown::render(&current(), &theme))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# Changelog

## [Unreleased]
Intro line.

### Added
- New thing.

## [0.12.0] - 2026-06-30
Older release.

### Fixed
- Old bug.
";

    /// Slice against the fixed SAMPLE through the real (pure) implementation.
    fn slice(label: &str) -> Option<String> {
        section_in(SAMPLE, label)
    }

    #[test]
    fn slices_a_versioned_section_with_its_heading() {
        let s = slice("0.12.0").expect("section present");
        assert!(s.starts_with("## [0.12.0] - 2026-06-30"));
        assert!(s.contains("Old bug."));
        // Stops before the previous section's neighbour — here there's nothing
        // after, but the unreleased intro must not leak in.
        assert!(!s.contains("New thing."));
    }

    #[test]
    fn slices_the_unreleased_section() {
        let s = slice("Unreleased").expect("section present");
        assert!(s.starts_with("## [Unreleased]"));
        assert!(s.contains("New thing."));
        // The next `## [` heading ends it.
        assert!(!s.contains("Older release."));
    }

    #[test]
    fn returns_none_for_a_missing_section() {
        assert!(slice("9.9.9").is_none());
    }

    /// The real bundled changelog always yields *some* current notes — the panel
    /// must never render empty.
    #[test]
    fn current_notes_are_never_empty() {
        assert!(!current().trim().is_empty());
    }
}
