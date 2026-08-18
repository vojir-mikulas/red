//! The transfer wizard's chrome: the modal shell, the step rail, and one body
//! per step.
//!
//! The rail is spiked here rather than in Flint. Per `CONTRIBUTING.md` a
//! component is proved in RED first and pushed down once its API is known to be
//! domain-free; this one is not there yet (see the plan's P1 note), and pushing
//! it down needs a coordinated Flint publish plus a `rev` bump, which is a
//! cross-repo event rather than a file edit.

use flint::prelude::*;
use gpui::{Context, Div, ElementId, InteractiveElement, Stateful, div, prelude::*, px};
use red_core::CopyMode;
use red_core::transfer::{ItemAction, ItemContent, ItemSource, OnError};

use super::{
    RunOutcome, Step, TransferBulk, TransferOption, TransferWizard, action_label, outcome_label,
    row_choice,
};
use crate::app::AppState;

/// How one step of the rail reads: only the current one carries weight.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StepState {
    Done,
    Current,
    Pending,
    Issue,
}

impl AppState {
    /// Render the wizard: one modal, one body per step, one rail across the top
    /// and a live plan summary in the footer, so the consequence of the last
    /// click is visible without reaching the end.
    pub(crate) fn render_transfer(
        &self,
        w: &TransferWizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();
        let step = w.step();

        let body = match step {
            Step::Destination => self.render_transfer_destination(w, cx).into_any_element(),
            Step::Objects => self.render_transfer_objects(w, cx).into_any_element(),
            Step::Content => self.render_transfer_content(w, cx).into_any_element(),
            Step::Review => self.render_transfer_review(w, cx).into_any_element(),
            Step::Progress => self.render_transfer_progress(w, cx).into_any_element(),
        };

        let note = w.note.clone().map(|note| {
            div()
                .text_size(theme.scale(11.5))
                .text_color(theme.text_muted)
                .child(note)
        });

        Modal::new("transfer-wizard")
            .title(crate::i18n::tr!("transfer.title", "Transfer"))
            .width(px(720.))
            .focus_handle(self.modal_focus.clone())
            .footer(self.render_transfer_footer(w, cx))
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.close_transfer(cx))
                    .ok();
            })
            .on_confirm(move |_, cx| {
                confirm_view
                    .update(cx, |this, cx| this.transfer_next(cx))
                    .ok();
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(self.render_step_rail(w, cx))
                    .children(note)
                    .child(
                        div()
                            .id("transfer-body")
                            .min_h(px(200.))
                            .max_h(px(440.))
                            .overflow_y_scroll()
                            .child(body),
                    ),
            )
    }

    /// The step rail: where you are, what is behind you, and which step owns a
    /// problem. Clickable, because the rail is navigation and not a gate.
    ///
    /// Only the *current* step carries weight. A row of four equally-solid chips
    /// reads as a toolbar of buttons; a rail should read as a path, so the steps
    /// behind you are quiet ticks, the ones ahead are outlines, and the connector
    /// between them is filled in as far as you have got.
    fn render_step_rail(
        &self,
        w: &TransferWizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let current = w.current;
        let mut rail = div().flex().items_center().flex_wrap().gap_y_1();
        for (i, step) in w.steps.iter().copied().enumerate() {
            let issues = w.issues_for(step);
            let state = if issues > 0 && i != current {
                StepState::Issue
            } else if i == current {
                StepState::Current
            } else if i < current {
                StepState::Done
            } else {
                StepState::Pending
            };
            // A step is reachable once it is the current one or behind it; the
            // Progress step only once there is a run to look at.
            let reachable =
                (i <= current || w.runnable()) && (step != Step::Progress || w.run.is_some());

            if i > 0 {
                // Filled behind you, hairline ahead: the connector is the part
                // that makes a row of chips read as a sequence.
                let done = i <= current;
                rail = rail.child(
                    div()
                        .w(px(20.))
                        .h(px(1.))
                        .mx_1()
                        .flex_shrink_0()
                        .bg(if done { theme.accent } else { theme.border }),
                );
            }

            let (badge_bg, badge_fg, badge_border) = match state {
                StepState::Current => (theme.accent, theme.on_accent, theme.accent),
                StepState::Done => (theme.accent_ghost, theme.accent, theme.accent_ghost),
                StepState::Issue => (theme.red, theme.on_accent, theme.red),
                StepState::Pending => (theme.bg_input, theme.text_faint, theme.border),
            };
            let badge = div()
                .w(px(20.))
                .h(px(20.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_1()
                .border_color(badge_border)
                .bg(badge_bg)
                .text_size(theme.scale(10.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(badge_fg)
                .child(match state {
                    StepState::Done => "✓".to_string(),
                    StepState::Issue => "!".to_string(),
                    _ => (i + 1).to_string(),
                });

            let label_color = match state {
                StepState::Current => theme.text,
                StepState::Issue => theme.red,
                StepState::Done => theme.text_muted,
                StepState::Pending => theme.text_faint,
            };
            let mut chip = div()
                .id(ElementId::from(("transfer-step", i)))
                .flex()
                .items_center()
                .gap_1p5()
                .px_1p5()
                .py_1()
                .rounded(theme.radius_sm)
                .child(badge)
                .child(
                    div()
                        .text_size(theme.scale(12.))
                        .font_weight(if state == StepState::Current {
                            gpui::FontWeight::MEDIUM
                        } else {
                            gpui::FontWeight::NORMAL
                        })
                        .text_color(label_color)
                        .child(step.label()),
                );
            if reachable {
                chip = chip
                    .cursor_pointer()
                    .tab_index(0)
                    .hover(|s| s.bg(theme.bg_hover))
                    .focus(|s| s.bg(theme.bg_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.goto_transfer_step(i, cx)));
            }
            rail = rail.child(chip);
        }
        div()
            .flex()
            .items_center()
            .pb_2()
            .border_b_1()
            .border_color(theme.border)
            .child(rail)
    }

    /// The footer: the live plan summary on the left, the actions on the right.
    fn render_transfer_footer(
        &self,
        w: &TransferWizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let running = w.run.as_ref().is_some_and(|r| r.outcome.is_none());
        let finished = w.run.as_ref().is_some_and(|r| r.outcome.is_some());
        let summary = red_core::transfer::summarize(&w.plan);
        let can_run = w.runnable();
        let first = w.current == 0;
        let last_input_step = w.current + 1 >= w.steps.len().saturating_sub(1);

        let mut actions = div().flex().items_center().gap_2();
        if !first && !running && !finished {
            actions = actions.child(
                Button::new("transfer-back", "Back")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.transfer_back(cx))),
            );
        }
        if running {
            actions = actions.child(
                Button::new("transfer-cancel-run", "Cancel")
                    .variant(ButtonVariant::Danger)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_transfer(cx))),
            );
        } else if finished {
            actions = actions.child(
                Button::new("transfer-done", "Close")
                    .variant(ButtonVariant::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.close_transfer(cx))),
            );
        } else {
            if w.step() == Step::Review {
                actions = actions.child(
                    Button::new("transfer-dry-run", "Dry run")
                        .variant(ButtonVariant::Secondary)
                        .disabled(!can_run)
                        .on_click(cx.listener(|this, _, _, cx| this.dry_run_transfer(cx))),
                );
            }
            if !last_input_step {
                actions = actions.child(
                    Button::new("transfer-next", "Next")
                        .variant(ButtonVariant::Secondary)
                        .on_click(cx.listener(|this, _, _, cx| this.transfer_next(cx))),
                );
            }
            // Live as soon as the plan is valid, from any step: someone who
            // right-clicked a table and wants the default presses Enter twice.
            actions = actions.child(
                Button::new("transfer-run", "Transfer")
                    .variant(ButtonVariant::Primary)
                    .disabled(!can_run)
                    .on_click(cx.listener(|this, _, _, cx| this.start_transfer(cx))),
            );
        }

        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(theme.scale(11.5))
                    .text_color(theme.text_muted)
                    .child(summary),
            )
            .child(actions)
    }

    /// Step 1: where it lands. Each row is a namespace on an open, writable
    /// connection; a read-only one is never listed, because planning a transfer
    /// that fails on item one is worse than not offering it.
    fn render_transfer_destination(
        &self,
        w: &TransferWizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let selected = w.destination;
        let mut list = div().flex().flex_col();
        for (i, dest) in w.destinations.iter().enumerate() {
            let is_selected = selected == Some(i);
            let here = dest.session == w.source
                && w.plan
                    .source_namespace
                    .as_deref()
                    .is_some_and(|ns| ns.eq_ignore_ascii_case(&dest.namespace));
            // What each row is worth knowing: the engine, how full it is, and
            // whether it is the source itself.
            let mut facts: Vec<String> = vec![
                dest.kind.to_string(),
                match dest.objects.len() {
                    0 => "empty".to_string(),
                    1 => "1 object".to_string(),
                    n => format!("{n} objects"),
                },
            ];
            if here {
                facts.push("the source".to_string());
            }
            let lossy = (!dest.same_connection).then(|| {
                format!(
                    "another connection: types are mapped into {} and column defaults are dropped",
                    dest.kind
                )
            });
            let mut info = div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .min_w_0()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(theme.text)
                                .child(dest.namespace.clone()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(theme.scale(11.))
                                .text_color(theme.text_faint)
                                .child(dest.conn_name.clone()),
                        ),
                )
                .child(
                    div()
                        .text_size(theme.scale(11.))
                        .text_color(theme.text_muted)
                        .truncate()
                        .child(facts.join(" · ")),
                );
            if let Some(lossy) = lossy {
                info = info.child(
                    div()
                        .text_size(theme.scale(11.))
                        .text_color(theme.orange)
                        .truncate()
                        .child(format!("⚠ {lossy}")),
                );
            }
            list = list.child(
                pick_row(("transfer-dest", i), i, is_selected, &theme)
                    .child(radio_dot(is_selected, &theme))
                    .child(info)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_transfer_destination(i, cx)),
                    ),
            );
        }

        let new_namespace =
            w.new_namespace.as_ref().map(|input| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(field_caption("Or create a new database", &theme))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(grow_field(input.clone()))
                            .child(
                                Button::new("transfer-create-ns", "Create")
                                    .variant(ButtonVariant::Secondary)
                                    .size(ButtonSize::Sm)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.create_transfer_namespace(cx)
                                    })),
                            ),
                    )
            });

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_color(theme.text_muted)
                    .text_size(theme.scale(12.))
                    .child(format!(
                        "Reading from {} · {}",
                        w.source_label,
                        w.plan.source_namespace.clone().unwrap_or_default()
                    )),
            )
            .child(list_frame(&theme).child(list))
            .children(new_namespace)
    }

    /// Step 2: the checklist. One row per source table with a three-way control,
    /// because that is the whole ask and it should not need a second step.
    ///
    /// The right-hand `Create` / `Existing` column is the **resolved** action,
    /// not a choice made here: present on the target means `Existing` (with a
    /// warning glyph, because that writes into something that already has rows),
    /// absent means `Create`. That is what the old migrate job decided silently.
    fn render_transfer_objects(
        &self,
        w: &TransferWizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let filter = w.filter.read(cx).content().to_string();
        let visible = w.visible_items(&filter);
        let active = w.plan.items.iter().filter(|i| i.is_active()).count();

        let mut list = div().flex().flex_col();
        for (row, &index) in visible.iter().enumerate() {
            let Some(item) = w.plan.items.get(index) else {
                continue;
            };
            let (label, warn) = action_label(item.action);
            let choice = row_choice(item);
            let picker = Segmented::new(format!("transfer-row-{index}"))
                .segment("Data")
                .segment("Structure")
                .segment("Skip")
                .selected(choice)
                .on_select({
                    let view = cx.entity().downgrade();
                    move |ix, _, cx| {
                        view.update(cx, |this, cx| this.set_transfer_row(index, ix, cx))
                            .ok();
                    }
                });
            let name_color = if item.is_active() {
                theme.text
            } else {
                theme.text_faint
            };
            list = list.child(
                dense_row(("transfer-obj", index), row, &theme)
                    .child(
                        div()
                            .w(px(150.))
                            .min_w_0()
                            .truncate()
                            .text_color(name_color)
                            .child(item.source_label().to_string()),
                    )
                    .child(picker)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.scale(11.5))
                            .text_color(theme.text_muted)
                            .child(format!("→ {}", item.target_name)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(theme.scale(11.))
                            .text_color(if warn { theme.orange } else { theme.text_faint })
                            .child(if warn {
                                format!("{label} ⚠")
                            } else {
                                label.to_string()
                            }),
                    ),
            );
        }

        let bulk = |id: &'static str, label: &'static str| {
            Button::new(id, label)
                .variant(ButtonVariant::Ghost)
                .size(ButtonSize::Sm)
        };

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(grow_field(w.filter.clone()))
                    .child(
                        bulk("transfer-all", "All").on_click(cx.listener(|this, _, _, cx| {
                            this.transfer_bulk(TransferBulk::SelectAll, cx)
                        })),
                    )
                    .child(
                        bulk("transfer-none", "None").on_click(cx.listener(|this, _, _, cx| {
                            this.transfer_bulk(TransferBulk::SelectNone, cx)
                        })),
                    )
                    .child(bulk("transfer-invert", "Invert").on_click(
                        cx.listener(|this, _, _, cx| this.transfer_bulk(TransferBulk::Invert, cx)),
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(theme.scale(11.5))
                            .text_color(theme.text_muted)
                            .child(format!("{active} of {} selected", w.plan.items.len())),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(bulk("transfer-bulk-data", "All data").on_click(cx.listener(
                                |this, _, _, cx| this.transfer_bulk(TransferBulk::AllData, cx),
                            )))
                            .child(
                                bulk("transfer-bulk-structure", "All structure only").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.transfer_bulk(TransferBulk::AllStructure, cx)
                                    }),
                                ),
                            ),
                    ),
            )
            .child(list_frame(&theme).child(list))
    }

    /// Step 3: per-item depth. A left rail of the selected items, a right pane
    /// for the one in focus, and `Apply to all selected` so a filter can be
    /// written once.
    fn render_transfer_content(
        &self,
        w: &TransferWizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let Some(item) = w.plan.items.get(w.focused) else {
            return div().into_any_element();
        };

        // The item rail, hidden for a single-item transfer where it would be a
        // list of one.
        let rail = (w.plan.items.len() > 1).then(|| {
            let mut rail = div().flex().flex_col().w(px(160.)).flex_shrink_0();
            for (i, it) in w.plan.items.iter().enumerate() {
                let focused = i == w.focused;
                let marker = if !it.is_active() {
                    "○"
                } else if it.content.moves_rows() {
                    ""
                } else {
                    "◍"
                };
                rail = rail.child(
                    pick_row(("transfer-item", i), i, focused, &theme)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(theme.scale(12.))
                                .text_color(if it.is_active() {
                                    theme.text
                                } else {
                                    theme.text_faint
                                })
                                .child(it.source_label().to_string()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(theme.scale(10.))
                                .text_color(theme.text_faint)
                                .child(marker),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.focus_transfer_item(i, cx)),
                        ),
                );
            }
            list_frame(&theme).flex_shrink_0().child(rail)
        });

        let action_ix = match item.action {
            ItemAction::Create => 0,
            ItemAction::Existing { .. } => 1,
            ItemAction::Recreate => 2,
            ItemAction::Skip => 3,
        };
        let action_picker = Segmented::new("transfer-action")
            .segment("Create")
            .segment("Existing")
            .segment("Recreate")
            .segment("Skip")
            .selected(action_ix)
            .on_select({
                let view = cx.entity().downgrade();
                move |ix, _, cx| {
                    let action = match ix {
                        0 => ItemAction::Create,
                        1 => ItemAction::Existing {
                            mode: CopyMode::Append,
                        },
                        2 => ItemAction::Recreate,
                        _ => ItemAction::Skip,
                    };
                    view.update(cx, |this, cx| this.set_transfer_action(action, cx))
                        .ok();
                }
            });

        let content_ix = match item.content {
            ItemContent::AllRows => 0,
            ItemContent::StructureOnly => 1,
            ItemContent::Where(_) => 2,
            ItemContent::Limit(_) => 3,
        };
        let content_picker = Segmented::new("transfer-content")
            .segment("All")
            .segment("None (structure only)")
            .segment("Where")
            .segment("First N")
            .selected(content_ix)
            .on_select({
                let view = cx.entity().downgrade();
                move |ix, _, cx| {
                    let content = match ix {
                        0 => ItemContent::AllRows,
                        1 => ItemContent::StructureOnly,
                        2 => ItemContent::Where(String::new()),
                        _ => ItemContent::Limit(1000),
                    };
                    view.update(cx, |this, cx| this.set_transfer_content(content, cx))
                        .ok();
                }
            });

        let shaping = match &item.content {
            ItemContent::Where(_) => Some(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(48.))
                            .text_size(theme.scale(11.5))
                            .text_color(theme.text_muted)
                            .child("where"),
                    )
                    .child(grow_field(w.where_expr.clone())),
            ),
            ItemContent::Limit(_) => Some(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(48.))
                            .text_size(theme.scale(11.5))
                            .text_color(theme.text_muted)
                            .child("rows"),
                    )
                    .child(div().w(px(140.)).flex().flex_col().child(w.limit.clone())),
            ),
            _ => None,
        };

        // On-existing mode, only where it means something.
        let existing_mode = matches!(item.action, ItemAction::Existing { .. }).then(|| {
            let selected = matches!(
                item.action,
                ItemAction::Existing {
                    mode: CopyMode::TruncateInsert
                }
            );
            labeled("On existing", &theme).child(
                Segmented::new("transfer-existing-mode")
                    .segment("Append")
                    .segment("Truncate + insert")
                    .selected(usize::from(selected))
                    .on_select({
                        let view = cx.entity().downgrade();
                        move |ix, _, cx| {
                            let mode = if ix == 0 {
                                CopyMode::Append
                            } else {
                                CopyMode::TruncateInsert
                            };
                            view.update(cx, |this, cx| {
                                this.set_transfer_action(ItemAction::Existing { mode }, cx)
                            })
                            .ok();
                        }
                    }),
            )
        });

        let disclosure = self.render_transfer_disclosure(w, cx);

        let pane = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_3()
            .child(labeled("Target name", &theme).child(w.rename.clone()))
            .child(labeled("Action", &theme).child(action_picker))
            .child(
                labeled("Rows", &theme)
                    .child(content_picker)
                    .children(shaping)
                    .children((w.plan.items.len() > 1).then(|| {
                        Button::new("transfer-apply-all", "Apply to all selected")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sync_transfer_editors(cx);
                                this.apply_transfer_content_to_all(cx);
                            }))
                    })),
            )
            .children(existing_mode)
            .child(disclosure);

        div()
            .flex()
            .items_start()
            .gap_3()
            .children(rail)
            .child(pane)
            .into_any_element()
    }

    /// The `Columns and DDL` disclosure: the column list and the generated
    /// `CREATE`, collapsed by default.
    ///
    /// Nobody should have to open it for a same-engine one-to-one copy, and
    /// nobody should be able to miss it when the mapping is imperfect - so it
    /// opens itself, with the reason spelled out, when there is one.
    fn render_transfer_disclosure(
        &self,
        w: &TransferWizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let Some(item) = w.plan.items.get(w.focused) else {
            return div().into_any_element();
        };
        let detail = match &item.source {
            ItemSource::Table { schema, name } => self.transfer_item_detail(schema, name, cx),
            _ => None,
        };
        let target_objects = w.target().map(|d| d.objects.clone()).unwrap_or_default();
        let collision = w
            .plan
            .items
            .iter()
            .enumerate()
            .any(|(i, other)| {
                i != w.focused
                    && other.is_active()
                    && other.target_name.eq_ignore_ascii_case(&item.target_name)
            })
            .then(|| format!("another item also writes into “{}”", item.target_name));
        let cross_engine = w
            .target()
            .filter(|d| !d.same_connection)
            .map(|d| format!("target is {}: column defaults will be dropped", d.kind));
        let existing = target_objects
            .iter()
            .any(|o| o.eq_ignore_ascii_case(&item.target_name))
            .then(|| "the target already exists; its own columns decide the mapping".to_string());
        let reasons: Vec<String> = [collision, cross_engine, existing]
            .into_iter()
            .flatten()
            .collect();
        // Auto-open when the plan has something worth looking at.
        let open = w.disclosure || !reasons.is_empty();

        let mut header = div()
            .id("transfer-disclosure")
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .tab_index(0)
            .text_size(theme.scale(12.))
            .text_color(theme.text_muted)
            .child(if open { "▾" } else { "▸" })
            .child("Columns and DDL")
            .on_click(cx.listener(|this, _, _, cx| this.toggle_transfer_disclosure(cx)));
        if !reasons.is_empty() {
            header = header.child(
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.orange)
                    .child(format!("⚠ {}", reasons.len())),
            );
        }

        let mut panel = div().flex().flex_col().gap_2();
        if open {
            for reason in &reasons {
                panel = panel.child(
                    div()
                        .text_size(theme.scale(11.))
                        .text_color(theme.orange)
                        .child(format!("⚠ {reason}")),
                );
            }
            match &detail {
                Some(detail) => {
                    let mut columns = div().flex().flex_col().gap_0p5();
                    for column in &detail.columns {
                        columns = columns.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_size(theme.scale(11.5))
                                .child(
                                    div()
                                        .w(px(160.))
                                        .truncate()
                                        .text_color(theme.text)
                                        .child(column.name.clone()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(theme.text_faint)
                                        .child(
                                            column.type_name.clone().unwrap_or_else(|| "—".into()),
                                        ),
                                ),
                        );
                    }
                    // The `CREATE` is shown only when the action actually
                    // creates. Rendering one under `Existing` claims a statement
                    // that will never run, against a table that is already there.
                    let kind = w
                        .target()
                        .map(|d| d.kind)
                        .unwrap_or(red_core::DbKind::Sqlite);
                    let ddl = matches!(item.action, ItemAction::Create | ItemAction::Recreate)
                        .then(|| {
                            // The same builder the driver runs, so the preview cannot
                            // drift from the statement. Identifiers are quoted
                            // generically here; the live one uses the engine's.
                            let create = red_core::ddl::create_table_sql(
                                &red_core::TableRef {
                                    schema: w.plan.target_namespace.clone(),
                                    name: item.target_name.clone(),
                                },
                                &detail.columns,
                                kind,
                                red_core::ddl::quote_generic,
                            );
                            match item.action {
                                ItemAction::Recreate => format!(
                                    "DROP TABLE IF EXISTS {};\n{create};",
                                    red_core::ddl::qualify_table(
                                        &red_core::TableRef {
                                            schema: w.plan.target_namespace.clone(),
                                            name: item.target_name.clone(),
                                        },
                                        red_core::ddl::quote_generic,
                                    )
                                ),
                                _ => format!("{create};"),
                            }
                        });
                    panel = panel.child(columns).children(ddl.map(|ddl| {
                        div()
                            .id("transfer-item-ddl")
                            .max_h(px(120.))
                            .overflow_y_scroll()
                            .p_2()
                            .rounded(theme.radius_sm)
                            .bg(theme.bg_input)
                            .border_1()
                            .border_color(theme.border_soft)
                            .font_family(theme.mono_family.clone())
                            .text_size(theme.scale(11.5))
                            .text_color(theme.text_muted)
                            .child(ddl)
                    }));
                }
                None => {
                    panel = panel.child(
                        div()
                            .text_size(theme.scale(11.5))
                            .text_color(theme.text_faint)
                            .child(
                                "Expand this table in the schema tree to see its columns here. \
                                 The transfer reads the real shape either way.",
                            ),
                    );
                }
            }
        }

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(header)
            .child(panel)
            .into_any_element()
    }

    /// Step 4: the job-wide options and the summary that has to be read before
    /// anything is written. Destructive lines are called out; they route through
    /// the destructive confirm once for the whole plan.
    fn render_transfer_review(
        &self,
        w: &TransferWizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let target = w
            .target()
            .map(|d| format!("{} ({}, {})", d.namespace, d.kind, d.conn_name))
            .unwrap_or_else(|| "no destination chosen".into());

        let count = |f: &dyn Fn(&red_core::transfer::TransferItem) -> bool| {
            w.plan.items.iter().filter(|i| f(i)).count()
        };
        let created = count(&|i| matches!(i.action, ItemAction::Create) && i.content.moves_rows());
        let empty = count(&|i| i.is_active() && !i.content.moves_rows());
        let appended = count(&|i| {
            matches!(
                i.action,
                ItemAction::Existing {
                    mode: CopyMode::Append
                }
            )
        });
        let destructive = count(&|i| i.is_destructive());
        let skipped = count(&|i| !i.is_active());

        // Only the non-zero lines, so a plain duplicate reads as one sentence
        // rather than a table of zeroes.
        let mut lines = div().flex().flex_col().gap_1();
        for (n, what, danger) in [
            (created, "table(s) created and filled", false),
            (empty, "table(s) created empty", false),
            (appended, "table(s) appended into", false),
            (
                destructive,
                "table(s) cleared or dropped before load (irreversible)",
                true,
            ),
            (skipped, "table(s) skipped", false),
        ] {
            if n == 0 {
                continue;
            }
            lines = lines.child(
                div()
                    .flex()
                    .gap_2()
                    .text_size(theme.scale(12.))
                    .text_color(if danger { theme.orange } else { theme.text })
                    .child(div().w(px(28.)).child(n.to_string()))
                    .child(what),
            );
        }

        let estimates = w.dry_run.as_ref().map(|dry| {
            let mut rows = div().flex().flex_col().gap_0p5();
            for (table, estimate) in &dry.estimates {
                rows = rows.child(
                    div()
                        .flex()
                        .gap_2()
                        .text_size(theme.scale(11.5))
                        .text_color(theme.text_muted)
                        .child(div().w(px(180.)).truncate().child(table.clone()))
                        .child(match estimate {
                            Some(n) => format!("~{n} row(s)"),
                            None => "not counted".to_string(),
                        }),
                );
            }
            let script = dry.script.clone();
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(field_caption("Dry run · nothing was written", &theme))
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .child(
                                    Button::new("transfer-copy-script", "Copy script")
                                        .variant(ButtonVariant::Ghost)
                                        .size(ButtonSize::Sm)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.copy_to_clipboard(
                                                script.clone(),
                                                "Script copied",
                                                cx,
                                            )
                                        })),
                                )
                                .child(
                                    Button::new("transfer-open-script", "Open in editor")
                                        .variant(ButtonVariant::Ghost)
                                        .size(ButtonSize::Sm)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.open_transfer_script(cx)
                                        })),
                                ),
                        ),
                )
                .child(rows)
                // The script reads here, in the step that asked for it. Opening
                // a query tab behind the modal was the old behaviour and it read
                // as "it ran".
                .child(
                    div()
                        .id("transfer-script")
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded(theme.radius_sm)
                        .bg(theme.bg_input)
                        .border_1()
                        .border_color(theme.border_soft)
                        .font_family(theme.mono_family.clone())
                        .text_size(theme.scale(11.5))
                        .text_color(theme.text_muted)
                        .child(dry.script.clone()),
                )
        });

        let toggle = |id: &'static str, label: &'static str, on: bool, option: TransferOption| {
            let mut row = div()
                .id(id)
                .flex()
                .items_center()
                .gap_2()
                .cursor_pointer()
                .tab_index(0)
                .child(Checkbox::new(id, on).mark(crate::icons::icon(
                    "check",
                    px(12.),
                    theme.on_accent,
                )))
                .child(
                    div()
                        .text_size(theme.scale(12.))
                        .text_color(theme.text)
                        .child(label),
                );
            row = row
                .on_click(cx.listener(move |this, _, _, cx| this.set_transfer_option(option, cx)));
            row
        };

        let on_error = matches!(w.plan.options.on_error, OnError::SkipItem);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text_muted)
                    .child(format!("Into {target}")),
            )
            .child(lines)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(field_caption("Carry", &theme))
                    .child(toggle(
                        "transfer-opt-pk",
                        "Primary keys",
                        w.plan.options.primary_keys,
                        TransferOption::PrimaryKeys(!w.plan.options.primary_keys),
                    ))
                    .child(toggle(
                        "transfer-opt-ix",
                        "Indexes",
                        w.plan.options.indexes,
                        TransferOption::Indexes(!w.plan.options.indexes),
                    ))
                    .child(toggle(
                        "transfer-opt-fk",
                        "Foreign keys",
                        w.plan.options.foreign_keys,
                        TransferOption::ForeignKeys(!w.plan.options.foreign_keys),
                    )),
            )
            .child(
                labeled("On error", &theme).child(
                    Segmented::new("transfer-on-error")
                        .segment("Stop")
                        .segment("Skip table")
                        .selected(usize::from(on_error))
                        .on_select({
                            let view = cx.entity().downgrade();
                            move |ix, _, cx| {
                                let mode = if ix == 0 {
                                    OnError::Stop
                                } else {
                                    OnError::SkipItem
                                };
                                view.update(cx, |this, cx| {
                                    this.set_transfer_option(TransferOption::OnError(mode), cx)
                                })
                                .ok();
                            }
                        }),
                ),
            )
            .children(estimates)
            .child(
                labeled("Save this plan", &theme).child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(grow_field(w.plan_name.clone()))
                        .child(
                            Button::new("transfer-save-plan", "Save plan")
                                .variant(ButtonVariant::Secondary)
                                .size(ButtonSize::Sm)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.save_transfer_plan(cx)),
                                ),
                        ),
                ),
            )
    }

    /// Step 5: the run, and then the report. One line per item, filled in as
    /// `TransferItemDone` arrives, so a forty-table job says where it is.
    fn render_transfer_progress(
        &self,
        w: &TransferWizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let Some(run) = w.run.as_ref() else {
            return div().into_any_element();
        };
        let done = run.reports.iter().filter(|r| r.is_some()).count();
        let total = run.reports.len().max(1);

        let mut list = div().flex().flex_col();
        for (i, item) in w.plan.items.iter().enumerate() {
            let report = run.reports.get(i).and_then(|r| r.as_ref());
            let running = run
                .current
                .as_ref()
                .is_some_and(|(table, _)| *table == item.target_name);
            let (status, color) = match report {
                Some(report) if report.outcome.is_problem() => {
                    (outcome_label(&report.outcome), theme.red)
                }
                Some(report) => (
                    format!(
                        "{} · {} row(s)",
                        outcome_label(&report.outcome),
                        report.rows
                    ),
                    theme.text_muted,
                ),
                None if running => (
                    format!(
                        "streaming… {} row(s)",
                        run.current.as_ref().map(|(_, n)| *n).unwrap_or(0)
                    ),
                    theme.accent,
                ),
                None => ("waiting".to_string(), theme.text_faint),
            };
            let warnings = report
                .filter(|r| !r.warnings.is_empty())
                .map(|r| r.warnings.join("; "));
            list = list.child(
                dense_row(("transfer-run", i), i, &theme)
                    .child(
                        div()
                            .w(px(180.))
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text)
                            .child(item.target_name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.scale(11.5))
                                    .text_color(color)
                                    .child(status),
                            )
                            .children(warnings.map(|warnings| {
                                div()
                                    .truncate()
                                    .text_size(theme.scale(11.))
                                    .text_color(theme.orange)
                                    .child(format!("⚠ {warnings}"))
                            })),
                    ),
            );
        }

        let headline = match &run.outcome {
            None => format!("Transferring… item {done} of {total}, {} row(s)", run.rows),
            Some(RunOutcome::Finished(summary)) => {
                let failures = summary.failures();
                if failures > 0 {
                    format!(
                        "Finished with {failures} failure(s). {} row(s) moved.",
                        summary.rows
                    )
                } else {
                    format!("Finished. {} row(s) moved.", summary.rows)
                }
            }
            Some(RunOutcome::Failed { message }) => format!("Stopped: {message}"),
            Some(RunOutcome::Cancelled) => format!(
                "Cancelled. {} row(s) already committed were kept.",
                run.rows
            ),
        };

        let report_actions = run.outcome.as_ref().map(|_| {
            let report = transfer_report_text(w);
            div().flex().gap_2().child(
                Button::new("transfer-copy-report", "Copy report")
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.copy_to_clipboard(report.clone(), "Report copied", cx)
                    })),
            )
        });

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(headline),
            )
            .child(ProgressBar::new(
                "transfer-progress",
                done as f32 / total as f32,
            ))
            .child(list_frame(&theme).child(list))
            .children(report_actions)
            .into_any_element()
    }

    /// The cached `TableDetail` for one source table, if the tree has it. The
    /// disclosure is a preview: an absent detail means "not expanded yet", not
    /// "no columns", and it says so rather than fetching on render.
    fn transfer_item_detail(
        &self,
        schema: &Option<String>,
        name: &str,
        cx: &gpui::App,
    ) -> Option<red_core::TableDetail> {
        let crate::app::Phase::Connected(active) = &self.phase else {
            return None;
        };
        let schema = schema.clone()?;
        active
            .schema
            .read(cx)
            .details
            .get(&(schema, name.to_string()))
            .cloned()
    }
}

/// The finished run as plain text, for "Copy report".
fn transfer_report_text(w: &TransferWizard) -> String {
    let Some(run) = w.run.as_ref() else {
        return String::new();
    };
    let mut out = String::new();
    for (i, item) in w.plan.items.iter().enumerate() {
        let line = match run.reports.get(i).and_then(|r| r.as_ref()) {
            Some(report) => {
                let mut line = format!(
                    "{}\t{}\t{} row(s)",
                    item.target_name,
                    outcome_label(&report.outcome),
                    report.rows
                );
                for warning in &report.warnings {
                    line.push_str(&format!("\n\t⚠ {warning}"));
                }
                line
            }
            None => format!("{}\tnot reached", item.target_name),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&format!("total\t{} row(s)\n", run.rows));
    out
}

/// A dense list row, hairline-separated from the one above.
fn dense_row(id: impl Into<ElementId>, index: usize, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_1p5()
        .when(index > 0, |d| d.border_t_1().border_color(theme.border))
}

/// A selectable row (destination, item rail): a dense row that also carries a
/// selected background and a click target.
fn pick_row(
    id: impl Into<ElementId>,
    index: usize,
    selected: bool,
    theme: &Theme,
) -> Stateful<Div> {
    dense_row(id, index, theme)
        .cursor_pointer()
        .tab_index(0)
        .hover(|s| s.bg(theme.bg_hover))
        .focus(|s| s.bg(theme.bg_hover))
        .when(selected, |d| d.bg(theme.accent_ghost))
}

/// The selection control on a destination row: a ring with a filled centre, so
/// it reads as "one of these" rather than as a status dot.
fn radio_dot(selected: bool, theme: &Theme) -> Div {
    div()
        .w(px(14.))
        .h(px(14.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(if selected { theme.accent } else { theme.border })
        .when(selected, |d| {
            d.child(div().w(px(6.)).h(px(6.)).rounded_full().bg(theme.accent))
        })
}

/// The framed container the dense rows sit in.
fn list_frame(theme: &Theme) -> Div {
    div()
        .rounded(theme.radius_sm)
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg_input)
        .overflow_hidden()
}

/// A small uppercase caption above a group of controls.
fn field_caption(label: &str, theme: &Theme) -> Div {
    div()
        .text_size(theme.scale(10.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text_faint)
        .child(label.to_uppercase())
}

/// A field that fills the remaining width of its row.
///
/// A `TextInput` sizes to its content, so a bare `flex_1` wrapper leaves it a
/// stub: it needs a *column* to stretch across. This is that column.
fn grow_field(input: gpui::Entity<TextInput>) -> Div {
    div().flex_1().min_w_0().flex().flex_col().child(input)
}

/// A caption plus its control, stacked.
fn labeled(label: &str, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1p5()
        .child(field_caption(label, theme))
}
