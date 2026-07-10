//! Find (Track B2, Tier 1): a small strip that highlights and steps through
//! matches of a substring, in whichever pane is focused: the **result grid**
//! (loaded rows) or the **query editor** (its text). Distinct from the filter bar
//! (⌘⇧F), which re-opens the result with a SQL predicate over the *whole* set:
//! find never touches the backend.
//!
//! One bar, two targets ([`FindTarget`]). ⌘F opens it against the focused pane;
//! the bar renders above that pane (see `result::render` and `editor`). The
//! focused match is revealed by *selecting* it, so the grid's / editor's own
//! selection highlight marks "current"; in the grid the other matches also get a
//! soft tint via the table's `cell_bg` hook.

use std::ops::Range;

use flint::prelude::*;
use gpui::{div, prelude::*, px, AnyElement, Context, Entity, Focusable, Window};

use crate::app::{AppState, Phase};

/// Which surface the open find bar searches.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FindTarget {
    /// The loaded rows of the active result grid.
    Grid,
    /// The active query editor's text.
    Editor,
}

/// State for the open find bar (present iff the bar is showing). The input's
/// event `Subscription` is held here, not detached, so closing the bar (nulling
/// the owning `Option`) drops the subscription with it.
pub(crate) struct FindBarState {
    pub(crate) input: Entity<TextInput>,
    #[allow(dead_code)]
    pub(crate) sub: gpui::Subscription,
    pub(crate) target: FindTarget,
    /// Index into the active target's matches of the focused match (meaningless
    /// when there are none).
    pub(crate) current: usize,
    /// Grid matches: `(absolute ordinal, data column)`, sorted row-major. Empty
    /// for an editor find.
    pub(crate) grid_matches: Vec<(usize, usize)>,
    /// Editor matches: byte ranges into the editor content. Empty for a grid find.
    pub(crate) editor_matches: Vec<Range<usize>>,
}

impl FindBarState {
    /// How many matches the active target currently has.
    fn len(&self) -> usize {
        match self.target {
            FindTarget::Grid => self.grid_matches.len(),
            FindTarget::Editor => self.editor_matches.len(),
        }
    }
}

/// Byte ranges of `needle_lower` (already lower-cased) within `content`,
/// case-insensitive, non-overlapping. The offsets index `content.to_lowercase()`;
/// for ASCII SQL (the overwhelming case) they coincide with `content`, and
/// `CodeEditor::select_range` snaps to char boundaries, so exotic unicode
/// degrades the selection but never panics.
fn editor_match_ranges(content: &str, needle_lower: &str) -> Vec<Range<usize>> {
    if needle_lower.is_empty() {
        return Vec::new();
    }
    content
        .to_lowercase()
        .match_indices(needle_lower)
        .map(|(i, m)| i..i + m.len())
        .collect()
}

impl AppState {
    /// ⌘F over a focused grid or editor: toggle the find bar against that pane.
    pub(crate) fn toggle_find_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_bar.is_some() {
            self.close_find_bar(cx);
            return;
        }
        let editor_focused = matches!(&self.phase, Phase::Connected(active)
            if active.active().is_some_and(|t| t.editor.focus_handle(cx).contains_focused(window, cx)));
        let target = if editor_focused {
            FindTarget::Editor
        } else {
            FindTarget::Grid
        };
        let placeholder = match target {
            FindTarget::Grid => "Find in loaded rows…",
            FindTarget::Editor => "Find in query…",
        };
        let input = cx.new(|cx| TextInput::new(cx).with_placeholder(placeholder));
        // Typing rescans; Enter steps to the next match; Esc closes the bar.
        let sub = cx.subscribe(&input, |this, _input, evt: &TextInputEvent, cx| match evt {
            TextInputEvent::Change => this.recompute_find(cx),
            TextInputEvent::Submit => this.find_step(true, cx),
            TextInputEvent::Cancel => this.close_find_bar(cx),
            TextInputEvent::Tab
            | TextInputEvent::BackTab
            | TextInputEvent::Up
            | TextInputEvent::Down => {}
        });
        self.find_bar = Some(FindBarState {
            input,
            sub,
            target,
            current: 0,
            grid_matches: Vec::new(),
            editor_matches: Vec::new(),
        });
        // The Window isn't in hand on the next render path; focus the input then.
        self.focus_find = true;
        cx.notify();
    }

    /// Close the find bar, clearing its highlight. Returns focus to root.
    pub(crate) fn close_find_bar(&mut self, cx: &mut Context<Self>) {
        if self.find_bar.take().is_some() {
            self.refocus_root = true;
            cx.notify();
        }
    }

    /// Rescan the active target for the bar's current term, resetting the focused
    /// match to the first and revealing it. An empty term clears the matches.
    fn recompute_find(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = self.find_bar.as_ref() else {
            return;
        };
        let term = bar.input.read(cx).content().trim().to_lowercase();
        let target = bar.target;
        let (grid_matches, editor_matches) = if term.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            match target {
                FindTarget::Grid => {
                    let m = match &self.phase {
                        Phase::Connected(active) => active
                            .active_result()
                            .map(|g| g.find_matches(&term))
                            .unwrap_or_default(),
                        _ => Vec::new(),
                    };
                    (m, Vec::new())
                }
                FindTarget::Editor => {
                    let m = match &self.phase {
                        Phase::Connected(active) => active
                            .active()
                            .map(|t| editor_match_ranges(&t.editor.read(cx).content(), &term))
                            .unwrap_or_default(),
                        _ => Vec::new(),
                    };
                    (Vec::new(), m)
                }
            }
        };
        if let Some(bar) = self.find_bar.as_mut() {
            bar.grid_matches = grid_matches;
            bar.editor_matches = editor_matches;
            bar.current = 0;
        }
        self.reveal_find_match(cx);
    }

    /// Step to the next/previous match (wrapping) and reveal it.
    pub(crate) fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some(bar) = self.find_bar.as_mut() {
            let n = bar.len();
            if n == 0 {
                return;
            }
            bar.current = if forward {
                (bar.current + 1) % n
            } else {
                (bar.current + n - 1) % n
            };
        }
        self.reveal_find_match(cx);
    }

    /// Select the focused match and scroll it into view, so the grid's / editor's
    /// own selection highlight marks "current". A no-op (bar a repaint) with no
    /// matches.
    fn reveal_find_match(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = self.find_bar.as_ref() else {
            return;
        };
        match bar.target {
            FindTarget::Grid => {
                let row_height = f32::from(self.settings.grid.density.row_height());
                let gutter = self.gutter();
                let target = bar.grid_matches.get(bar.current).copied();
                if let Some((ord, dcol)) = target {
                    if let Phase::Connected(active) = &mut self.phase {
                        if let Some(grid) = active.active_result_mut() {
                            grid.reveal_cell(ord, dcol + gutter, row_height);
                        }
                    }
                }
            }
            FindTarget::Editor => {
                let range = bar.editor_matches.get(bar.current).cloned();
                let editor = match &self.phase {
                    Phase::Connected(active) => active.active().map(|t| t.editor.clone()),
                    _ => None,
                };
                if let (Some(range), Some(editor)) = (range, editor) {
                    editor.update(cx, |e, cx| e.select_range(range.start, range.end, cx));
                }
            }
        }
        cx.notify();
    }

    /// The find-bar editing strip, rendered above the pane it targets, so it's
    /// drawn by `result::render` for [`FindTarget::Grid`] and by `editor` for
    /// [`FindTarget::Editor`], each passing the surface it owns. Returns `None`
    /// when the bar is closed or targets the *other* pane.
    pub(crate) fn render_find_bar(
        &self,
        here: FindTarget,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let bar = self.find_bar.as_ref()?;
        if bar.target != here {
            return None;
        }
        let theme = cx.theme();
        let (border, muted, bg, size) = (
            theme.border,
            theme.text_muted,
            theme.bg_bar,
            theme.scale(11.),
        );
        let ui_family = theme.font_family.clone();

        let has_term = !bar.input.read(cx).content().trim().is_empty();
        let n = bar.len();
        // "in loaded rows" only applies to the grid, where the scan is windowed.
        let scope = match bar.target {
            FindTarget::Grid => " in loaded rows",
            FindTarget::Editor => "",
        };
        let count = if !has_term {
            String::new()
        } else if n == 0 {
            format!("No matches{scope}")
        } else {
            format!("{} of {}{}", bar.current + 1, n, scope)
        };
        let can_step = n > 1;

        let row = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py(px(4.))
            .border_b_1()
            .border_color(border)
            .bg(bg)
            .font_family(ui_family)
            .text_size(size)
            .child(div().text_color(muted).child("Find"))
            .child(div().flex_1().min_w(px(120.)).child(bar.input.clone()))
            .child(
                Button::new("find-prev", "‹")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .disabled(!can_step)
                    .on_click(cx.listener(|this, _, _, cx| this.find_step(false, cx))),
            )
            .child(
                Button::new("find-next", "›")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .disabled(!can_step)
                    .on_click(cx.listener(|this, _, _, cx| this.find_step(true, cx))),
            )
            .child(div().text_color(muted).child(count))
            .child(
                Button::new("find-done", "Done")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.close_find_bar(cx))),
            );
        Some(row.into_any_element())
    }
}
