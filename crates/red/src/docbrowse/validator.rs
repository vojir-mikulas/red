//! The validation-rule editor: a collection's `validator`, editable.
//!
//! MongoDB's validator is the closest thing it has to a constraint, and RED has
//! always *read* it (the catalog carries one per collection). This is the other
//! half.
//!
//! What makes it more than a text box is the test: a validator document is also a
//! query (`$jsonSchema` is a query operator, and an expression validator already
//! is one), so the dialog can count how many stored documents would pass before
//! anything is written. The server does not re-check existing documents when a
//! validator is set, so without that count you would find out from the first
//! rejected write, weeks later.

use flint::prelude::*;
use gpui::{Context, Entity, div, prelude::*, px};
use red_core::doc::DocWrite;
use red_service::{Command, Epoch, SessionId};

use crate::app::{AppState, Phase};

/// A starting point for a collection that has no validator yet: the shape of a
/// `$jsonSchema` rule, with nothing actually required.
const VALIDATOR_TEMPLATE: &str = r#"{
  "$jsonSchema": {
    "bsonType": "object",
    "required": [],
    "properties": {
    }
  }
}"#;

/// The "Validation rules" dialog's state.
pub(crate) struct DocValidatorForm {
    pub(crate) db: String,
    pub(crate) coll: String,
    pub(crate) epoch: Epoch,
    pub(crate) editor: Entity<CodeEditor>,
    /// Whether the collection had a validator when the dialog opened, which is
    /// what decides whether "Remove" means anything.
    pub(crate) had_validator: bool,
    /// The last test's `(matching, total)`, once one has run.
    pub(crate) tested: Option<(u64, u64)>,
    /// Whether a test is in flight.
    pub(crate) testing: bool,
    pub(crate) error: Option<String>,
}

impl AppState {
    /// Open the validator editor for a collection, seeded with its current rule
    /// (or a template when it has none).
    pub(crate) fn doc_open_validator(
        &mut self,
        session: SessionId,
        db: String,
        coll: String,
        cx: &mut Context<Self>,
    ) {
        self.doc_close_actions_menu(session, cx);
        self.doc_close_coll_menu(session, cx);
        let Some(view) = self
            .conn_for(Some(session))
            .and_then(|a| a.doc_view.as_ref())
        else {
            return;
        };
        if view.read_only {
            return;
        }
        let existing = view
            .collections
            .get(&db)
            .and_then(|cs| cs.iter().find(|c| c.name == coll))
            .and_then(|c| c.validator.clone());
        let epoch = view
            .tabs
            .iter()
            .find_map(|t| match &t.state {
                super::MongoTabState::Collection(c) if c.db == db && c.coll == coll => {
                    Some(c.epoch)
                }
                _ => None,
            })
            .unwrap_or(view.epoch);
        let had_validator = existing.is_some();
        let seed = existing.unwrap_or_else(|| VALIDATOR_TEMPLATE.to_string());
        let editor = cx.new(|cx| {
            CodeEditor::new(cx)
                .soft_wrap(false)
                .with_content(seed)
                .edit_menu_labels(crate::editor::edit_menu_labels())
                .a11y_label(crate::i18n::tr!(
                    "doc.validation_rule",
                    "MongoDB validation rule"
                ))
        });
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.validator = Some(DocValidatorForm {
                db,
                coll,
                epoch,
                editor,
                had_validator,
                tested: None,
                testing: false,
                error: None,
            });
        }
        self.focus_modal = true;
        cx.notify();
    }

    pub(crate) fn doc_close_validator(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.validator = None;
        }
        self.refocus_root = true;
        cx.notify();
    }

    /// Count how many stored documents the rule in the editor would accept.
    pub(crate) fn doc_test_validator(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(form) = self
            .conn_for(Some(session))
            .and_then(|a| a.doc_view.as_ref())
            .and_then(|v| v.validator.as_ref())
        else {
            return;
        };
        let (db, coll, epoch) = (form.db.clone(), form.coll.clone(), form.epoch);
        let validator = form.editor.read(cx).content().to_string();
        if validator.trim().is_empty() {
            return;
        }
        if let Some(form) = self.doc_validator_mut(session) {
            form.testing = true;
            form.error = None;
            form.tested = None;
        }
        self.service.send_to(
            session,
            Command::DocValidatorTest {
                epoch,
                db,
                coll,
                validator,
            },
        );
        cx.notify();
    }

    /// `DocValidatorTested`: land the score (or the failure) on the dialog.
    pub(crate) fn on_doc_validator_tested(
        &mut self,
        session: Option<SessionId>,
        epoch: Epoch,
        matching: u64,
        total: u64,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(form) = self
            .conn_mut(session)
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.validator.as_mut())
            .filter(|f| f.epoch == epoch)
        {
            form.testing = false;
            form.error = error;
            form.tested = Some((matching, total));
        }
        cx.notify();
    }

    /// Propose the rule in the editor (or, with `remove`, no rule at all). Both go
    /// through the destructive confirm: a validator decides whether every future
    /// write is accepted.
    pub(crate) fn doc_submit_validator(
        &mut self,
        session: SessionId,
        remove: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self
            .conn_for(Some(session))
            .and_then(|a| a.doc_view.as_ref())
            .and_then(|v| v.validator.as_ref())
        else {
            return;
        };
        let (db, coll, epoch) = (form.db.clone(), form.coll.clone(), form.epoch);
        let validator = if remove {
            None
        } else {
            let text = form.editor.read(cx).content().trim().to_string();
            if text.is_empty() {
                if let Some(form) = self.doc_validator_mut(session) {
                    form.error = Some("Write a rule, or use Remove.".into());
                }
                cx.notify();
                return;
            }
            Some(text)
        };
        self.service.send_to(
            session,
            Command::DocApplyWrite {
                epoch,
                write: DocWrite::SetValidator {
                    db,
                    coll,
                    validator,
                },
                confirmed: false,
            },
        );
        self.doc_close_validator(session, cx);
    }

    fn doc_validator_mut(&mut self, session: SessionId) -> Option<&mut DocValidatorForm> {
        self.conn_mut(Some(session))?
            .doc_view
            .as_mut()?
            .validator
            .as_mut()
    }

    /// The "Validation rules" modal.
    pub(crate) fn render_doc_validator_modal(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let session = active.session;
        let form = active.doc_view.as_ref()?.validator.as_ref()?;
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();

        let score = form.tested.map(|(matching, total)| {
            let failing = total.saturating_sub(matching);
            let (color, text) = if total == 0 {
                (theme.text_faint, "The collection is empty.".to_string())
            } else if failing == 0 {
                (
                    theme.green,
                    format!("All {total} stored document(s) would pass."),
                )
            } else {
                (
                    theme.yellow,
                    format!(
                        "{failing} of {total} stored document(s) would NOT pass. They stay as \
                         they are: a validator governs future writes only."
                    ),
                )
            };
            div()
                .text_size(theme.scale(11.))
                .text_color(color)
                .child(text)
        });
        let error = form.error.as_ref().map(|why| {
            div()
                .text_size(theme.scale(11.))
                .text_color(theme.red)
                .child(why.clone())
        });

        let (test_view, save_view, remove_view, cancel_view, close_view) = (
            view.clone(),
            view.clone(),
            view.clone(),
            view.clone(),
            view.clone(),
        );
        let footer = div()
            .flex()
            .flex_1()
            .items_center()
            .gap_2()
            .child(
                Button::new("doc-validator-test", "Test against stored documents")
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .disabled(form.testing)
                    .on_click(move |_, _, cx| {
                        test_view
                            .update(cx, |this, cx| this.doc_test_validator(session, cx))
                            .ok();
                    }),
            )
            .child(div().flex_1())
            .when(form.had_validator, |d| {
                d.child(
                    Button::new("doc-validator-remove", "Remove")
                        .variant(ButtonVariant::Danger)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            remove_view
                                .update(cx, |this, cx| this.doc_submit_validator(session, true, cx))
                                .ok();
                        }),
                )
            })
            .child(
                Button::new("doc-validator-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        cancel_view
                            .update(cx, |this, cx| this.doc_close_validator(session, cx))
                            .ok();
                    }),
            )
            .child(
                Button::new("doc-validator-save", "Apply")
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        save_view
                            .update(cx, |this, cx| this.doc_submit_validator(session, false, cx))
                            .ok();
                    }),
            );

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child(format!("On {}.{}", form.db, form.coll)),
            )
            .child(
                div()
                    .h(px(280.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(theme.border)
                    .child(form.editor.clone()),
            )
            .children(error)
            .children(score)
            .child(
                div()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_faint)
                    .child(
                        "Setting a rule does not re-check documents already stored; it applies \
                         to writes from here on.",
                    ),
            );

        Some(
            Modal::new("doc-validator")
                .title(crate::i18n::tr!("doc.validation_rules", "Validation rules"))
                .width(px(720.))
                .focus_handle(self.modal_focus.clone())
                .on_close(move |_, cx| {
                    close_view
                        .update(cx, |this, cx| this.doc_close_validator(session, cx))
                        .ok();
                })
                .footer(footer)
                .child(body)
                .into_any_element(),
        )
    }
}
