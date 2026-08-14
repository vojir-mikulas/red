//! The aggregation stage builder: the Query panel's second mode, beside the raw
//! pipeline editor.
//!
//! The raw text stays the source of truth. Switching to Stages splits it (a
//! shallow, byte-preserving split in `red-core`), switching back joins it, and a
//! run always joins first -- so the two modes are two views of one pipeline rather
//! than two pipelines that can disagree.
//!
//! "Preview at stage N" is the feature the mode exists for: it runs the pipeline
//! truncated after that stage with a `$limit` appended, which is how you find the
//! stage that emptied your result.

use flint::prelude::*;
use gpui::{Context, Entity, div, prelude::*, px};
use red_service::SessionId;

use crate::app::AppState;

/// How the Query panel is edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DocQueryMode {
    /// One editor holding the whole pipeline.
    Text,
    /// One editor per stage, reorderable, with a per-stage preview.
    Stages,
}

impl DocQueryMode {
    pub(crate) const ALL: [(DocQueryMode, &'static str); 2] = [
        (DocQueryMode::Text, "Text"),
        (DocQueryMode::Stages, "Stages"),
    ];
}

/// Documents a stage preview returns. Small on purpose: a preview answers "what
/// shape comes out of here", and reading it is the point, not paging it.
pub(crate) const PREVIEW_LIMIT: usize = 20;

/// One stage in the builder: its own editor over that stage's text.
pub(crate) struct DocStage {
    pub(crate) editor: Entity<CodeEditor>,
}

/// The stage operators the palette offers, with the template each inserts.
///
/// Ordered the way a pipeline usually reads (narrow, then reshape, then combine),
/// because the palette is also a reminder of what a pipeline can do.
pub(crate) const STAGE_PALETTE: [(&str, &str); 12] = [
    ("$match", "{ \"$match\": {  } }"),
    ("$project", "{ \"$project\": { \"_id\": 1 } }"),
    ("$sort", "{ \"$sort\": {  } }"),
    ("$limit", "{ \"$limit\": 20 }"),
    ("$skip", "{ \"$skip\": 0 }"),
    (
        "$group",
        "{ \"$group\": { \"_id\": null, \"n\": { \"$sum\": 1 } } }",
    ),
    ("$unwind", "{ \"$unwind\": \"$field\" }"),
    (
        "$lookup",
        "{ \"$lookup\": { \"from\": \"\", \"localField\": \"\", \"foreignField\": \"_id\", \"as\": \"joined\" } }",
    ),
    ("$addFields", "{ \"$addFields\": {  } }"),
    ("$count", "{ \"$count\": \"total\" }"),
    ("$sample", "{ \"$sample\": { \"size\": 100 } }"),
    ("$facet", "{ \"$facet\": {  } }"),
];

/// The stages worth suggesting first at `position`. A pipeline almost always
/// narrows before it reshapes, so an empty pipeline leads with `$match`, and a
/// later slot leads with the stages that consume what came before.
pub(crate) fn palette_for(position: usize) -> Vec<(&'static str, &'static str)> {
    let lead: &[&str] = if position == 0 {
        &["$match", "$sort", "$limit"]
    } else {
        &["$group", "$project", "$lookup", "$unwind"]
    };
    // Driven by `lead`'s order, not the palette's: the first entry is the one
    // this position most likely wants, and the palette's own order is only the
    // fallback for everything else.
    let mut out: Vec<(&'static str, &'static str)> = lead
        .iter()
        .filter_map(|want| STAGE_PALETTE.iter().find(|(op, _)| op == want).copied())
        .collect();
    out.extend(
        STAGE_PALETTE
            .iter()
            .filter(|(op, _)| !lead.contains(op))
            .copied(),
    );
    out
}

impl AppState {
    /// Switch the Query panel's mode, converting the pipeline as it goes so the
    /// two views never drift.
    pub(crate) fn doc_set_query_mode(
        &mut self,
        session: SessionId,
        mode: DocQueryMode,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        if current.query_mode == mode {
            return;
        }
        match mode {
            DocQueryMode::Stages => {
                let text = current.query_editor.read(cx).content().to_string();
                let Some(stages) = red_core::doc::split_pipeline_stages(&text) else {
                    // Half-written text cannot be split into stages without
                    // guessing. Say so and stay in the editor that holds it.
                    current.query_error = Some(
                        "This pipeline is not a complete `[ … ]` array yet, so it cannot be \
                         split into stages."
                            .into(),
                    );
                    cx.notify();
                    return;
                };
                current.query_error = None;
                current.stages = stages
                    .into_iter()
                    .map(|stage| DocStage {
                        editor: cx.new(|cx| stage_editor(cx).with_content(stage)),
                    })
                    .collect();
                current.query_mode = mode;
            }
            DocQueryMode::Text => {
                let text = current.pipeline_text(cx);
                let editor = current.query_editor.clone();
                editor.update(cx, |editor, cx| editor.set_content(text, cx));
                current.query_mode = mode;
                current.query_error = None;
            }
        }
        cx.notify();
    }

    /// Insert a stage from the palette at `position`.
    pub(crate) fn doc_add_stage(
        &mut self,
        session: SessionId,
        position: usize,
        template: &'static str,
        cx: &mut Context<Self>,
    ) {
        let editor = cx.new(|cx| stage_editor(cx).with_content(template));
        if let Some(current) = self.doc_focused_coll_mut(session) {
            let at = position.min(current.stages.len());
            current.stages.insert(at, DocStage { editor });
            current.stage_menu = None;
        }
        cx.notify();
    }

    pub(crate) fn doc_remove_stage(
        &mut self,
        session: SessionId,
        ix: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(current) = self.doc_focused_coll_mut(session)
            && ix < current.stages.len()
        {
            current.stages.remove(ix);
            // A preview pinned past the end would silently become another stage's.
            if current
                .preview_stage
                .is_some_and(|p| p >= current.stages.len())
            {
                current.preview_stage = None;
            }
        }
        cx.notify();
    }

    /// Move a stage one place earlier or later. Order is the pipeline's meaning,
    /// so this is the builder's most-used control.
    pub(crate) fn doc_move_stage(
        &mut self,
        session: SessionId,
        ix: usize,
        delta: i32,
        cx: &mut Context<Self>,
    ) {
        if let Some(current) = self.doc_focused_coll_mut(session) {
            let target = ix as i32 + delta;
            if target >= 0 && (target as usize) < current.stages.len() {
                current.stages.swap(ix, target as usize);
            }
        }
        cx.notify();
    }

    /// Open / close the "add stage" operator palette at `position`.
    pub(crate) fn doc_open_stage_menu(
        &mut self,
        session: SessionId,
        position: usize,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(current) = self.doc_focused_coll_mut(session) {
            current.stage_menu = Some((position, pos));
        }
        cx.notify();
    }

    pub(crate) fn doc_close_stage_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(current) = self.doc_focused_coll_mut(session) {
            current.stage_menu = None;
        }
        cx.notify();
    }

    /// Preview the pipeline as it stands after stage `ix` (or clear the preview
    /// when it is already pinned there), then run it.
    pub(crate) fn doc_preview_stage(
        &mut self,
        session: SessionId,
        ix: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(current) = self.doc_focused_coll_mut(session) {
            current.preview_stage = if current.preview_stage == Some(ix) {
                None
            } else {
                Some(ix)
            };
        }
        self.doc_run_aggregate(session, cx);
    }

    /// The "add stage" palette, floating at its trigger.
    pub(crate) fn render_doc_stage_menu(
        &self,
        session: SessionId,
        position: usize,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut menu = ContextMenu::new("doc-stage-palette");
        for (i, (op, template)) in palette_for(position).into_iter().enumerate() {
            menu = menu.item(
                ContextMenuItem::new(gpui::SharedString::from(format!("doc-stage-op-{i}")), op)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.doc_add_stage(session, position, template, cx);
                    })),
            );
        }
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.doc_close_stage_menu(session, cx)),
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, _, _, cx| this.doc_close_stage_menu(session, cx)),
            )
            .child(
                floating(div().occlude().child(menu.into_any_element()))
                    .at(pos)
                    .anchor(gpui::Anchor::TopLeft),
            )
            .into_any_element()
    }

    /// The stage list: one row per stage, each with its own editor, its position
    /// controls, and the preview toggle.
    pub(crate) fn render_doc_stages(
        &self,
        session: SessionId,
        current: &super::CollView,
        theme: &Theme,
        view: &gpui::WeakEntity<AppState>,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        let mut list = div().flex().flex_col().gap_2().p_2();
        for (ix, stage) in current.stages.iter().enumerate() {
            let op = red_core::doc::stage_operator(&stage.editor.read(cx).content())
                .unwrap_or("(no operator)")
                .to_string();
            let previewing = current.preview_stage == Some(ix);
            let (up_view, down_view, del_view, prev_view) =
                (view.clone(), view.clone(), view.clone(), view.clone());
            let header = div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .w(px(24.))
                        .text_color(theme.text_faint)
                        .text_size(theme.scale(11.))
                        .child(format!("{}.", ix + 1)),
                )
                .child(
                    div()
                        .flex_1()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.accent)
                        .text_size(theme.scale(12.))
                        .child(op),
                )
                .child(
                    Button::new(format!("doc-stage-up-{ix}"), "\u{2191}")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Ghost)
                        .disabled(ix == 0)
                        .on_click(move |_, _, cx| {
                            up_view
                                .update(cx, |this, cx| this.doc_move_stage(session, ix, -1, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new(format!("doc-stage-down-{ix}"), "\u{2193}")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Ghost)
                        .disabled(ix + 1 == current.stages.len())
                        .on_click(move |_, _, cx| {
                            down_view
                                .update(cx, |this, cx| this.doc_move_stage(session, ix, 1, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new(
                        format!("doc-stage-preview-{ix}"),
                        if previewing { "Previewing" } else { "Preview" },
                    )
                    .size(ButtonSize::Sm)
                    .variant(if previewing {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Ghost
                    })
                    .on_click(move |_, _, cx| {
                        prev_view
                            .update(cx, |this, cx| this.doc_preview_stage(session, ix, cx))
                            .ok();
                    }),
                )
                .child(
                    Button::new(format!("doc-stage-del-{ix}"), "\u{2715}")
                        .size(ButtonSize::Sm)
                        .variant(ButtonVariant::Ghost)
                        .on_click(move |_, _, cx| {
                            del_view
                                .update(cx, |this, cx| this.doc_remove_stage(session, ix, cx))
                                .ok();
                        }),
                );
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(if previewing {
                        theme.accent
                    } else {
                        theme.border
                    })
                    .bg(theme.bg_panel)
                    .child(header)
                    .child(
                        div()
                            .h(px(72.))
                            .border_t_1()
                            .border_color(theme.border)
                            .child(stage.editor.clone()),
                    ),
            );
        }

        let add_view = view.clone();
        let at_end = current.stages.len();
        list = list.child(
            div().child(
                Button::new("doc-stage-add", "Add stage\u{2026}")
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Secondary)
                    .on_click(move |ev: &gpui::ClickEvent, _, cx| {
                        let pos = ev.position();
                        add_view
                            .update(cx, |this, cx| {
                                this.doc_open_stage_menu(session, at_end, pos, cx)
                            })
                            .ok();
                    }),
            ),
        );
        div()
            .id("doc-stage-list")
            .size_full()
            .overflow_y_scroll()
            .child(list)
            .into_any_element()
    }
}

/// A stage editor: one small, gutterless code surface per stage.
fn stage_editor(cx: &mut Context<CodeEditor>) -> CodeEditor {
    CodeEditor::new(cx)
        .gutter(false)
        .soft_wrap(true)
        .edit_menu_labels(crate::editor::edit_menu_labels())
        .a11y_label(crate::i18n::tr!(
            "doc.aggregation_stage",
            "Aggregation stage"
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_palette_leads_with_what_fits_the_position() {
        // An empty pipeline narrows first.
        assert_eq!(palette_for(0)[0].0, "$match");
        // A later slot consumes what came before.
        assert_eq!(palette_for(3)[0].0, "$group");
        // Either way the whole palette is reachable, just reordered.
        assert_eq!(palette_for(0).len(), STAGE_PALETTE.len());
        assert_eq!(palette_for(3).len(), STAGE_PALETTE.len());
    }
}
