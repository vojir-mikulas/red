//! The result filter bar (Track B2): a small input strip above the grid that
//! narrows the whole result by pushing a predicate into the query. Distinct from
//! find-in-result (which only highlights loaded rows), a filter **re-opens** the
//! result under a new epoch with the predicate wrapped in — so the count, the
//! keyset seek key, sort, and export all operate on the filtered set, never
//! materializing it (the wrap keeps `SELECT *`, so the key column survives).
//!
//! The bar is the transient editing UI; the *applied* filter lives on the grid
//! (`ResultGrid::filter`) and rides every (re)open. Two modes: a portable
//! "contains" term (rendered per engine to a safe `LIKE`/`ILIKE` OR-chain) and a
//! raw SQL `WHERE` expression for power users (trusted like editor SQL).

use flint::prelude::*;
use gpui::{div, prelude::*, px, AnyElement, Context, Entity};
use red_core::ResultFilter;

use crate::app::AppState;

/// Whether the filter input is read as a portable "contains" term or a raw SQL
/// `WHERE` expression.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilterMode {
    Contains,
    Where,
}

/// State for the open filter bar (present iff the bar is showing). The input's
/// event `Subscription` is held here, not detached, so closing the bar (nulling
/// the owning `Option`) drops the subscription with it rather than orphaning it.
pub(crate) struct FilterBarState {
    pub(crate) input: Entity<TextInput>,
    pub(crate) mode: FilterMode,
    #[allow(dead_code)]
    pub(crate) sub: gpui::Subscription,
}

impl AppState {
    /// ⌘⇧F / the toolbar Filter button: toggle the filter bar. Opening seeds the
    /// input + mode from the active result's current filter so it's editable.
    pub(crate) fn toggle_filter_bar(&mut self, cx: &mut Context<Self>) {
        if self.filter_bar.is_some() {
            self.close_filter_bar(cx);
            return;
        }
        let (mode, text) = match self.active_result_filter() {
            Some(ResultFilter::Contains(t)) => (FilterMode::Contains, t),
            Some(ResultFilter::Where(t)) => (FilterMode::Where, t),
            // An FK-follow `Eq` filter (Track B7) isn't text-editable; opening the
            // bar starts a fresh contains filter (applying it replaces the FK one).
            Some(ResultFilter::Eq(_)) | None => (FilterMode::Contains, String::new()),
        };
        let input = cx.new(|cx| TextInput::new(cx).with_placeholder("Filter rows…"));
        if !text.is_empty() {
            input.update(cx, |i, cx| i.set_content(text, cx));
        }
        // Enter applies; Esc (the input's Cancel) closes the bar without clearing
        // any already-applied filter.
        let sub = cx.subscribe(&input, |this, _input, evt: &TextInputEvent, cx| match evt {
            TextInputEvent::Submit => this.submit_filter(cx),
            TextInputEvent::Cancel => this.close_filter_bar(cx),
            TextInputEvent::Change => {}
        });
        self.filter_bar = Some(FilterBarState { input, mode, sub });
        // The Window isn't in hand here — focus the input on the next render.
        self.focus_filter = true;
        cx.notify();
    }

    /// Close the bar, leaving any applied filter in place. Returns focus to root.
    pub(crate) fn close_filter_bar(&mut self, cx: &mut Context<Self>) {
        if self.filter_bar.take().is_some() {
            self.refocus_root = true;
            cx.notify();
        }
    }

    /// Switch the bar between contains / raw-`WHERE` modes.
    pub(crate) fn set_filter_mode(&mut self, mode: FilterMode, cx: &mut Context<Self>) {
        if let Some(bar) = &mut self.filter_bar {
            bar.mode = mode;
            cx.notify();
        }
    }

    /// Apply the bar's current text as the result filter (Enter / the Apply
    /// button). An empty term clears the filter. The bar stays open (focus kept in
    /// the input) so the filter can be refined or re-run; Esc / the ✕ closes it.
    pub(crate) fn submit_filter(&mut self, cx: &mut Context<Self>) {
        let Some((text, mode)) = self
            .filter_bar
            .as_ref()
            .map(|bar| (bar.input.read(cx).content().trim().to_string(), bar.mode))
        else {
            return;
        };
        let filter = if text.is_empty() {
            None
        } else {
            Some(match mode {
                FilterMode::Contains => ResultFilter::Contains(text),
                FilterMode::Where => ResultFilter::Where(text),
            })
        };
        self.apply_result_filter(filter, cx);
        // Keep the bar open with focus back in its input (covers the Apply button,
        // where focus was on the button), so the filter can be tweaked and re-run.
        self.focus_filter = true;
        cx.notify();
    }

    /// Clear the applied filter and close the bar (the chip's ✕ / the Clear button).
    pub(crate) fn clear_result_filter(&mut self, cx: &mut Context<Self>) {
        self.apply_result_filter(None, cx);
        self.filter_bar = None;
        self.refocus_root = true;
        cx.notify();
    }

    /// The filter-bar editing strip, rendered above the grid when the bar is open.
    pub(crate) fn render_filter_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bar = self.filter_bar.as_ref()?;
        let theme = cx.theme();
        let (border, muted, bg, size) = (
            theme.border,
            theme.text_muted,
            theme.bg_bar,
            theme.scale(11.),
        );
        let ui_family = theme.font_family.clone();
        let has_filter = self.active_result_filter().is_some();

        // A segmented mode toggle — the active mode reads as "filled".
        let seg = |id: &'static str, label: &'static str, mode: FilterMode| {
            let active = bar.mode == mode;
            Button::new(id, label)
                .variant(if active {
                    ButtonVariant::Secondary
                } else {
                    ButtonVariant::Ghost
                })
                .size(ButtonSize::Sm)
                .on_click(cx.listener(move |this, _, _, cx| this.set_filter_mode(mode, cx)))
        };

        let mut row = div()
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
            .child(div().text_color(muted).child("Filter"))
            .child(seg(
                "filter-mode-contains",
                "Contains",
                FilterMode::Contains,
            ))
            .child(seg("filter-mode-where", "WHERE", FilterMode::Where))
            .child(div().flex_1().min_w(px(120.)).child(bar.input.clone()))
            .child(
                Button::new("filter-apply", "Apply")
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.submit_filter(cx))),
            );
        if has_filter {
            row = row.child(
                Button::new("filter-clear", "Clear")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.clear_result_filter(cx))),
            );
        }
        Some(row.into_any_element())
    }
}
