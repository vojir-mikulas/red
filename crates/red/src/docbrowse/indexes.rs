//! The index dialog: creating and dropping a collection's indexes from the
//! Indexes panel.
//!
//! Both writes ride the ordinary `DocApplyWrite` path, so a drop gets the same
//! confirm dance a collection drop does -- which is the point, because dropping an
//! index writes no documents and is still the write most likely to take a
//! deployment down.

use flint::prelude::*;
use gpui::{Context, Entity, div, prelude::*, px};
use red_core::doc::{DocWrite, IndexKey, IndexSpec};
use red_service::{Command, Epoch, SessionId};

use crate::app::{AppState, Phase};

/// One key row in the create-index dialog: the field path and how it is indexed.
pub(crate) struct DocIndexKeyRow {
    pub(crate) field: Entity<TextInput>,
    pub(crate) kind: IndexKey,
}

/// The "New index" dialog's state.
pub(crate) struct DocIndexForm {
    pub(crate) db: String,
    pub(crate) coll: String,
    /// The originating tab's epoch, so the write and its index refresh land there.
    pub(crate) epoch: Epoch,
    pub(crate) keys: Vec<DocIndexKeyRow>,
    pub(crate) name: Entity<TextInput>,
    pub(crate) unique: bool,
    pub(crate) sparse: bool,
    /// `expireAfterSeconds`, as typed. Empty is no TTL.
    pub(crate) ttl: Entity<TextInput>,
    /// `partialFilterExpression` as extended JSON, as typed. Empty is none.
    pub(crate) partial: Entity<TextInput>,
    /// An ICU collation locale, as typed. Empty is none.
    pub(crate) collation: Entity<TextInput>,
    /// Why the last submit was refused, shown in the dialog.
    pub(crate) error: Option<String>,
}

/// Which switch a toggle targets. An enum rather than a `bool` argument so a call
/// site cannot silently flip the wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DocIndexSwitch {
    Unique,
    Sparse,
}

impl DocIndexForm {
    /// Build the spec the dialog describes, or say why it cannot.
    fn to_spec(&self, cx: &gpui::App) -> Result<IndexSpec, String> {
        let keys: Vec<(String, IndexKey)> = self
            .keys
            .iter()
            .filter_map(|row| {
                let field = row.field.read(cx).content().trim().to_string();
                (!field.is_empty()).then_some((field, row.kind))
            })
            .collect();
        if keys.is_empty() {
            return Err("An index needs at least one field.".into());
        }
        let text = |input: &Entity<TextInput>| {
            let value = input.read(cx).content().trim().to_string();
            (!value.is_empty()).then_some(value)
        };
        let ttl_seconds = match text(&self.ttl) {
            None => None,
            Some(raw) => Some(
                raw.parse::<i64>()
                    .map_err(|_| format!("`{raw}` is not a number of seconds."))?,
            ),
        };
        // A TTL index expires documents by the value of its one key; on a compound
        // index the server refuses it, and refusing here names the reason.
        if ttl_seconds.is_some() && keys.len() > 1 {
            return Err("A TTL applies to a single-field index only.".into());
        }
        Ok(IndexSpec {
            keys,
            unique: self.unique,
            sparse: self.sparse,
            name: text(&self.name),
            ttl_seconds,
            partial_filter: text(&self.partial),
            collation_locale: text(&self.collation),
        })
    }
}

impl AppState {
    /// Open the create-index dialog for the focused collection. `seed` pre-fills
    /// the key rows, which is how the panel's suggested index becomes one click.
    pub(crate) fn doc_open_index_form(
        &mut self,
        session: SessionId,
        seed: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some((db, coll, epoch)) = self
            .conn_for(Some(session))
            .and_then(|a| a.doc_view.as_ref())
            .and_then(|v| v.focused_coll())
            .map(|c| (c.db.clone(), c.coll.clone(), c.epoch))
        else {
            return;
        };
        if self
            .conn_for(Some(session))
            .and_then(|a| a.doc_view.as_ref())
            .is_some_and(|v| v.read_only)
        {
            return;
        }
        let fields = if seed.is_empty() {
            vec![String::new()]
        } else {
            seed
        };
        let keys = fields
            .into_iter()
            .map(|field| DocIndexKeyRow {
                field: cx.new(|cx| {
                    TextInput::new(cx)
                        .with_content(field)
                        .with_placeholder("field, or user.$** for a wildcard")
                }),
                kind: IndexKey::Asc,
            })
            .collect();
        let form = DocIndexForm {
            db,
            coll,
            epoch,
            keys,
            name: cx
                .new(|cx| TextInput::new(cx).with_placeholder("optional; the server derives one")),
            unique: false,
            sparse: false,
            ttl: cx.new(|cx| TextInput::new(cx).with_placeholder("seconds; empty for no TTL")),
            partial: cx.new(|cx| {
                TextInput::new(cx).with_placeholder("optional filter, e.g. { \"archived\": false }")
            }),
            collation: cx.new(|cx| TextInput::new(cx).with_placeholder("optional locale, e.g. en")),
            error: None,
        };
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.index_form = Some(form);
        }
        self.focus_modal = true;
        cx.notify();
    }

    pub(crate) fn doc_close_index_form(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.index_form = None;
        }
        self.refocus_root = true;
        cx.notify();
    }

    pub(crate) fn doc_add_index_key(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let row = DocIndexKeyRow {
            field: cx.new(|cx| TextInput::new(cx).with_placeholder("field")),
            kind: IndexKey::Asc,
        };
        if let Some(form) = self.doc_index_form_mut(session) {
            form.keys.push(row);
        }
        cx.notify();
    }

    pub(crate) fn doc_remove_index_key(
        &mut self,
        session: SessionId,
        ix: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(form) = self.doc_index_form_mut(session)
            // The last row stays: a dialog with no key rows offers no way back.
            && form.keys.len() > 1
        {
            form.keys.remove(ix);
        }
        cx.notify();
    }

    pub(crate) fn doc_set_index_key_kind(
        &mut self,
        session: SessionId,
        ix: usize,
        kind: IndexKey,
        cx: &mut Context<Self>,
    ) {
        if let Some(form) = self.doc_index_form_mut(session)
            && let Some(row) = form.keys.get_mut(ix)
        {
            row.kind = kind;
        }
        cx.notify();
    }

    pub(crate) fn doc_toggle_index_switch(
        &mut self,
        session: SessionId,
        switch: DocIndexSwitch,
        cx: &mut Context<Self>,
    ) {
        if let Some(form) = self.doc_index_form_mut(session) {
            match switch {
                DocIndexSwitch::Unique => form.unique = !form.unique,
                DocIndexSwitch::Sparse => form.sparse = !form.sparse,
            }
        }
        cx.notify();
    }

    /// Validate the dialog and propose the index. Creating one is an ordinary
    /// write, so it needs no confirm; the toast reports the outcome.
    pub(crate) fn doc_submit_index(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(form) = self
            .conn_for(Some(session))
            .and_then(|a| a.doc_view.as_ref())
            .and_then(|v| v.index_form.as_ref())
        else {
            return;
        };
        let (db, coll, epoch) = (form.db.clone(), form.coll.clone(), form.epoch);
        match form.to_spec(cx) {
            Ok(spec) => {
                self.service.send_to(
                    session,
                    Command::DocApplyWrite {
                        epoch,
                        write: DocWrite::CreateIndex { db, coll, spec },
                        confirmed: false,
                    },
                );
                self.doc_close_index_form(session, cx);
            }
            Err(why) => {
                if let Some(form) = self.doc_index_form_mut(session) {
                    form.error = Some(why);
                }
                cx.notify();
            }
        }
    }

    /// Propose dropping an index by name. Destructive, so the service replies with
    /// a confirm the shell's modal shows before anything happens.
    pub(crate) fn doc_drop_index(
        &mut self,
        session: SessionId,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let Some((db, coll, epoch)) = self
            .conn_for(Some(session))
            .and_then(|a| a.doc_view.as_ref())
            .and_then(|v| v.focused_coll())
            .map(|c| (c.db.clone(), c.coll.clone(), c.epoch))
        else {
            return;
        };
        self.service.send_to(
            session,
            Command::DocApplyWrite {
                epoch,
                write: DocWrite::DropIndex { db, coll, name },
                confirmed: false,
            },
        );
        cx.notify();
    }

    fn doc_index_form_mut(&mut self, session: SessionId) -> Option<&mut DocIndexForm> {
        self.conn_mut(Some(session))?
            .doc_view
            .as_mut()?
            .index_form
            .as_mut()
    }

    /// The "New index" modal.
    pub(crate) fn render_doc_index_modal(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use crate::connect::labeled_field;

        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let session = active.session;
        let form = active.doc_view.as_ref()?.index_form.as_ref()?;
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();

        let mut key_rows = div().flex().flex_col().gap_2();
        for (ix, row) in form.keys.iter().enumerate() {
            let kind_view = view.clone();
            let remove_view = view.clone();
            let selected = IndexKey::ALL
                .iter()
                .position(|k| *k == row.kind)
                .unwrap_or(0);
            key_rows = key_rows.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w(px(0.)).child(row.field.clone()))
                    .child(
                        IndexKey::ALL
                            .iter()
                            .fold(Segmented::new(format!("doc-index-kind-{ix}")), |seg, k| {
                                seg.segment(k.label())
                            })
                            .selected(selected)
                            .on_select(move |pick, _, cx| {
                                let Some(kind) = IndexKey::ALL.get(pick).copied() else {
                                    return;
                                };
                                kind_view
                                    .update(cx, |this, cx| {
                                        this.doc_set_index_key_kind(session, ix, kind, cx)
                                    })
                                    .ok();
                            }),
                    )
                    .child(
                        Button::new(format!("doc-index-drop-key-{ix}"), "\u{2715}")
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Ghost)
                            .disabled(form.keys.len() == 1)
                            .on_click(move |_, _, cx| {
                                remove_view
                                    .update(cx, |this, cx| {
                                        this.doc_remove_index_key(session, ix, cx)
                                    })
                                    .ok();
                            }),
                    ),
            );
        }
        let add_view = view.clone();
        key_rows = key_rows.child(
            div().child(
                Button::new("doc-index-add-key", "Add field")
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Secondary)
                    .on_click(move |_, _, cx| {
                        add_view
                            .update(cx, |this, cx| this.doc_add_index_key(session, cx))
                            .ok();
                    }),
            ),
        );

        let (unique_view, sparse_view) = (view.clone(), view.clone());
        let switches = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                Checkbox::new("doc-index-unique", form.unique)
                    .label("Unique (reject a second document with the same key)")
                    .on_change(move |_, _, cx| {
                        unique_view
                            .update(cx, |this, cx| {
                                this.doc_toggle_index_switch(session, DocIndexSwitch::Unique, cx)
                            })
                            .ok();
                    }),
            )
            .child(
                Checkbox::new("doc-index-sparse", form.sparse)
                    .label("Sparse (skip documents that lack the field)")
                    .on_change(move |_, _, cx| {
                        sparse_view
                            .update(cx, |this, cx| {
                                this.doc_toggle_index_switch(session, DocIndexSwitch::Sparse, cx)
                            })
                            .ok();
                    }),
            );

        let error = form.error.as_ref().map(|why| {
            div()
                .text_size(theme.scale(11.))
                .text_color(theme.red)
                .child(why.clone())
        });

        let (submit_view, cancel_view, close_view) = (view.clone(), view.clone(), view.clone());
        let footer = div()
            .flex()
            .flex_1()
            .justify_end()
            .gap_2()
            .child(
                Button::new("doc-index-cancel", "Cancel")
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        cancel_view
                            .update(cx, |this, cx| this.doc_close_index_form(session, cx))
                            .ok();
                    }),
            )
            .child(
                Button::new("doc-index-create", "Create index")
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        submit_view
                            .update(cx, |this, cx| this.doc_submit_index(session, cx))
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
            .child(labeled_field("Keys", &theme).child(key_rows))
            .child(
                div()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_faint)
                    .child(
                        "Key order matters: only a leading subset of a compound index can \
                         serve a query.",
                    ),
            )
            .child(labeled_field("Name", &theme).child(form.name.clone()))
            .child(switches)
            .child(labeled_field("Expire after", &theme).child(form.ttl.clone()))
            .child(labeled_field("Partial filter", &theme).child(form.partial.clone()))
            .child(labeled_field("Collation", &theme).child(form.collation.clone()))
            .children(error);

        Some(
            Modal::new("doc-index")
                .title(crate::i18n::tr!("doc.new_index", "New index"))
                .width(px(640.))
                .focus_handle(self.modal_focus.clone())
                .on_close(move |_, cx| {
                    close_view
                        .update(cx, |this, cx| this.doc_close_index_form(session, cx))
                        .ok();
                })
                .footer(footer)
                .child(body)
                .into_any_element(),
        )
    }
}
