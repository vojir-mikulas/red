//! The "Database knowledge" editor: the in-app half of [`crate::knowledge`].
//!
//! A root-mounted modal holding one Flint [`MarkdownEditor`] over the active
//! connection's `knowledge/<id>.md`. There is deliberately no form, no field set,
//! and no validation: the file is the interface, and the agent reads prose. ⌘↵
//! saves, Esc closes.
//!
//! The module also owns the two paths that don't start with a click:
//! [`AppState::refresh_knowledge`], which re-reads the file before every
//! assistant turn so an edit made in any other editor still lands, and
//! [`AppState::on_ai_knowledge_draft`], which puts the agent's "Learn this
//! database" draft **in the editor for review** rather than on disk. That second
//! one is the load-bearing choice: an unreviewed inferred glossary is worse than
//! no glossary, because the system prompt presents it as authoritative.

use flint::prelude::*;
use flint::{MarkdownEditor, MarkdownEditorEvent};
use gpui::{AnyElement, Context, Entity, Focusable, SharedString, div, prelude::*, px};

use crate::app::{AppState, Phase};

/// The open knowledge editor. Present iff the modal is showing.
pub(crate) struct KnowledgeEditor {
    /// Whose file this is. Held rather than re-derived from `phase` so a save can
    /// never land on a connection the user switched to meanwhile.
    conn_id: String,
    /// The connection's display name, for the modal title.
    name: SharedString,
    /// The markdown surface. Owns the buffer; saving reads it back out.
    editor: Entity<MarkdownEditor>,
    /// RAII: keeps the ⌘↵-saves subscription alive. Never read.
    _sub: gpui::Subscription,
    /// Set when the body came from the agent's `save_knowledge` and the user
    /// hasn't accepted it yet, which puts the review banner at the top.
    draft: bool,
}

impl KnowledgeEditor {
    /// The focus handle to put the caret in when the modal opens: the modal *is*
    /// the editor, so there is nothing else worth focusing.
    pub(crate) fn focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl AppState {
    /// Re-read the active connection's knowledge file into [`ActiveConn::knowledge`]
    /// so the next turn (and the composer's chip) reflects what is on disk right
    /// now, including an edit made outside RED.
    ///
    /// [`ActiveConn::knowledge`]: crate::app::ActiveConn::knowledge
    pub(crate) fn refresh_knowledge(&mut self) {
        if let Phase::Connected(active) = &mut self.phase {
            active.knowledge = crate::knowledge::load(&active.conn_id);
        }
    }

    /// Open the knowledge editor on the active connection's file, seeded with what
    /// is on disk (verbatim: [`crate::knowledge::read`], never the prompt-capped
    /// form, so a truncation note can't be edited back into the file). A no-op
    /// while disconnected, since the file is per-connection.
    pub(crate) fn open_knowledge_editor(&mut self, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let body = crate::knowledge::read(&active.conn_id).unwrap_or_default();
        self.show_knowledge_editor(body, false, cx);
    }

    /// Show the editor over `body`. `draft` marks a body the agent inferred, which
    /// adds the review banner and leaves the file on disk untouched until the user
    /// saves.
    fn show_knowledge_editor(&mut self, body: String, draft: bool, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let (conn_id, name) = (
            active.conn_id.clone(),
            SharedString::from(active.config.name.clone()),
        );
        let editor = cx.new(|cx| {
            let mut e = MarkdownEditor::new(cx).placeholder(crate::i18n::tr!(
                "knowledge.placeholder",
                "What would you tell a new colleague about this database?"
            ));
            e.set_content(body, cx);
            e
        });
        // ⌘↵ saves, matching "run the query" / "submit staged changes" everywhere
        // else in RED. Esc has to be handled here too: the editor claims the key in
        // its own context and relays it as an event, so without this the modal's own
        // Esc never fires while the caret is in the body, which is always.
        let sub = cx.subscribe(&editor, |this, _editor, event, cx| match event {
            MarkdownEditorEvent::Run => this.save_knowledge_editor(cx),
            MarkdownEditorEvent::Escape => this.close_knowledge_editor(cx),
            _ => {}
        });
        self.knowledge_editor = Some(KnowledgeEditor {
            conn_id,
            name,
            editor,
            _sub: sub,
            draft,
        });
        // Put the caret in the editor on the next paint (see `render`).
        self.focus_modal = true;
        cx.notify();
    }

    /// Write the editor's body to disk, refresh the cached copy the next turn will
    /// send, and close. An empty body deletes the file rather than leaving one
    /// behind: "no knowledge" and "a file containing nothing" should not be two
    /// different states.
    pub(crate) fn save_knowledge_editor(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.knowledge_editor.as_ref() else {
            return;
        };
        let (conn_id, name) = (open.conn_id.clone(), open.name.clone());
        let body = open.editor.read(cx).content();
        let outcome = if body.trim().is_empty() {
            crate::knowledge::delete(&conn_id).map(|()| None)
        } else {
            crate::knowledge::save(&conn_id, &body).map(Some)
        };
        match outcome {
            Ok(Some(_)) => {
                self.knowledge_editor = None;
                self.notify(
                    flint::ToastVariant::Success,
                    crate::i18n::tr!(
                        "knowledge.saved",
                        "Saved what the agent knows about {name}.",
                        name = name.to_string()
                    ),
                    cx,
                );
            }
            Ok(None) => {
                self.knowledge_editor = None;
                self.notify(
                    flint::ToastVariant::Info,
                    crate::i18n::tr!(
                        "knowledge.cleared",
                        "Cleared the knowledge file for {name}.",
                        name = name.to_string()
                    ),
                    cx,
                );
            }
            // The editor stays open on failure, holding the text: a read-only
            // config dir or a full disk must not silently swallow prose the user
            // just wrote (or, worse, an agent draft that can't be regenerated
            // without spending another turn).
            Err(e) => {
                self.notify(
                    flint::ToastVariant::Error,
                    format!("Couldn't save the knowledge file: {e}"),
                    cx,
                );
            }
        }
        self.refresh_knowledge();
        cx.notify();
    }

    /// Close the editor without writing anything.
    pub(crate) fn close_knowledge_editor(&mut self, cx: &mut Context<Self>) {
        self.knowledge_editor = None;
        cx.notify();
    }

    /// The agent's `save_knowledge` tool fired: open its draft **for review**
    /// instead of writing it.
    ///
    /// The agent inferred this from structure and sampling, so it is right about
    /// shape and wrong about intent, and the system prompt hands whatever is in the
    /// file to every future turn as authoritative. Saving it unread would launder a
    /// guess into a fact, so the user sees it first.
    pub(crate) fn on_ai_knowledge_draft(
        &mut self,
        _conversation_id: red_service::ConversationId,
        body: String,
        cx: &mut Context<Self>,
    ) {
        self.show_knowledge_editor(body, true, cx);
    }

    /// Ask the agent to draft a knowledge file for this connection, as an ordinary
    /// turn: it inherits the tier, the resource limits, the activity timeline, and
    /// the cancel path rather than growing a second path that has to re-implement
    /// all four.
    ///
    /// Only offered at `read` tier or above ([`Self::can_learn_database`]): with
    /// schema-only tools the agent cannot sample a single value, so its "glossary"
    /// would be column-name inference, which is the failure mode the knowledge file
    /// exists to fix.
    pub(crate) fn learn_this_database(&mut self, cx: &mut Context<Self>) {
        if !self.can_learn_database() {
            return;
        }
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let prompt = learn_prompt(engine_noun(active.config.kind));
        self.send_turn(prompt, cx);
    }

    /// Whether "Learn this database" is on offer: a live connection, an assistant
    /// panel to stream into, and a tier that can actually read data.
    pub(crate) fn can_learn_database(&self) -> bool {
        matches!(self.phase, Phase::Connected(_))
            && self.assistant.is_some()
            && self.ai_configured
            && matches!(
                self.ai_tier_effective(),
                red_core::AiTier::Read | red_core::AiTier::Write
            )
    }

    /// Whether the active connection has a knowledge file in play, and how many
    /// bytes of it the agent is being sent. Drives the composer's chip.
    pub(crate) fn knowledge_bytes(&self) -> Option<usize> {
        match &self.phase {
            Phase::Connected(active) => active.knowledge.as_ref().map(String::len),
            _ => None,
        }
    }

    /// The knowledge-editor modal, root-mounted like the other modals so it
    /// overlays the whole shell. `None` when it's closed.
    pub(crate) fn render_knowledge_modal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.knowledge_editor.as_ref()?;
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let (save_view, cancel_view, close_view) = (view.clone(), view.clone(), view);

        // The banner only rides a draft: on a file the user wrote themselves it
        // would be noise, and the whole point of the wording is that *this* text
        // has not been checked by a human yet.
        let banner = open.draft.then(|| {
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .p_2()
                .rounded(theme.radius_sm)
                .bg(theme.yellow.opacity(0.1))
                .border_1()
                .border_color(theme.yellow.opacity(0.35))
                .child(
                    div()
                        .text_size(theme.scale(12.))
                        .text_color(theme.text)
                        .child(crate::i18n::tr!(
                            "knowledge.draft_title",
                            "Draft written by the agent"
                        )),
                )
                .child(
                    div()
                        .text_size(theme.scale(11.))
                        .text_color(theme.text_muted)
                        .child(crate::i18n::tr!(
                            "knowledge.draft_hint",
                            "Review it: it inferred this from structure and sampling, and it will \
                             be wrong about intent. Nothing is saved until you save."
                        )),
                )
        });

        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .children(banner)
            .child(
                div()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_muted)
                    .child(crate::i18n::tr!(
                        "knowledge.hint",
                        "Plain markdown, folded into the agent's prompt for every chat on this \
                         connection: what a metric means, which join path is the real one, which \
                         table not to count."
                    )),
            )
            .child(
                div()
                    .id("knowledge-scroll")
                    .h(px(420.))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.bg_input)
                    .child(open.editor.clone()),
            );

        let footer = div()
            .flex()
            .flex_1()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_faint)
                    .child(crate::i18n::tr!("knowledge.save_hint", "⌘↵ to save")),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("knowledge-cancel", "Cancel")
                            .variant(ButtonVariant::Secondary)
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                cancel_view
                                    .update(cx, |this, cx| this.close_knowledge_editor(cx))
                                    .ok();
                            }),
                    )
                    .child(
                        Button::new("knowledge-save", "Save")
                            .variant(ButtonVariant::Primary)
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                save_view
                                    .update(cx, |this, cx| this.save_knowledge_editor(cx))
                                    .ok();
                            }),
                    ),
            );

        Some(
            Modal::new("knowledge-editor")
                .title(crate::i18n::tr!(
                    "knowledge.title",
                    "Database knowledge - {name}",
                    name = open.name.to_string()
                ))
                .width(px(720.))
                .focus_handle(self.modal_focus.clone())
                .on_close(move |_, cx| {
                    close_view
                        .update(cx, |this, cx| this.close_knowledge_editor(cx))
                        .ok();
                })
                .footer(footer)
                .child(body)
                .into_any_element(),
        )
    }
}

/// What this connection is, in the words the prompt should use. The three seams
/// hold genuinely different things, and a prompt that calls a Redis keyspace a
/// "database" invites the agent to go looking for tables.
fn engine_noun(kind: red_core::DbKind) -> &'static str {
    match kind {
        red_core::DbKind::Redis => "Redis server",
        red_core::DbKind::Mongo => "MongoDB deployment",
        _ => "database",
    }
}

/// The canned "Learn this database" prompt. Written as an instruction to the
/// agent rather than as a question from the user, because it is one: the turn
/// exists to produce a file, and the last step is the tool call that hands the
/// draft back.
///
/// `noun` names what the connection actually is ("database" / "Redis server" /
/// "MongoDB deployment"), so the same prompt reads correctly on all three seams;
/// the tools it names are the ones that seam's catalog offers.
fn learn_prompt(noun: &str) -> String {
    format!(
        "Draft a knowledge file for this {noun}: the semantic layer a new colleague would need \
         and that the schema cannot tell you.\n\n\
         Work from the live connection, not from names. Map the structure first, then profile the \
         tables/collections/key namespaces that matter most (the largest and the most referenced) \
         so you can see which fields are enums, which are unique keys, and which are dead. Sample \
         real values where a name is ambiguous.\n\n\
         Then write markdown with these sections, keeping only what you actually have evidence \
         for:\n\
         - `## Glossary` - what the domain terms mean in terms of concrete predicates.\n\
         - `## Joins` (or `## References`) - the real relationship paths, and any that look \
         plausible but are wrong.\n\
         - `## Tables` (or `## Collections` / `## Key namespaces`) - size, shape, the columns or \
         fields worth knowing about, and what the enum-like values mean.\n\
         - `## Gotchas` - units, timezones, soft-delete flags, anything that silently produces a \
         wrong number.\n\n\
         Be concrete and short: a line the reader can act on beats a paragraph of hedging. Where \
         you are inferring intent rather than observing structure, say so in the line itself \
         (\"looks like...\") - a human is going to review this. Do not invent business rules you \
         have no evidence for; leave a section out rather than filling it with guesses.\n\n\
         Finish by calling save_knowledge with the whole document. Do not print it in the chat as \
         well; the tool opens it in the user's editor for review."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learn_prompt_names_the_engine_and_ends_at_the_tool() {
        let sql = learn_prompt("database");
        assert!(sql.starts_with("Draft a knowledge file for this database:"));
        // The turn's whole purpose is the tool call; a prompt that let the agent
        // "answer" instead would leave the file empty and the feature dead.
        assert!(sql.trim_end().ends_with("for review."));
        assert!(sql.contains("save_knowledge"));
        assert!(learn_prompt("Redis server").contains("for this Redis server:"));
    }
}
