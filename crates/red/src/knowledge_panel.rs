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
//!
//! # This is a view
//!
//! [`KnowledgeEditor`] is a real gpui view: an `Entity<KnowledgeEditor>` that
//! renders itself, owns its subscriptions, and reports what happened by emitting
//! [`KnowledgeEditorEvent`] rather than by reaching into [`AppState`]. It is the
//! first surface in RED extracted this way (see
//! `docs/plans/todo/zed-architecture-inspiration.md`, Tier 1), and the pattern
//! the rest should copy:
//!
//! - **State and behaviour live together.** Saving is `KnowledgeEditor::save`,
//!   not `AppState::save_knowledge_editor` reaching in through an `Option`.
//! - **The app learns by subscribing.** The view knows how to write the file; it
//!   does not know that RED has a toast stack. `AppState::on_knowledge_event`
//!   maps the outcome onto notifications and the cached copy.
//! - **`cx.notify()` re-renders this modal**, not the whole window.
//!
//! One dependency is deliberately left behind: the modal root still tracks
//! [`AppState::modal_focus`], the single shared handle that RED's focus trap is
//! registered on. Giving each modal its own handle is a change to all twelve of
//! them at once and belongs with the Tier 2 panel work, so the handle is passed
//! in at construction. That parameter is the whole of what remains coupled.

use flint::prelude::*;
use flint::{MarkdownEditor, MarkdownEditorEvent};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Window, div,
    prelude::*, px,
};

use crate::app::{AppState, Phase};

/// What the knowledge editor did, for whoever is hosting it. The view writes the
/// file itself (it owns the body); the host decides what that means on screen.
#[derive(Debug, Clone)]
pub(crate) enum KnowledgeEditorEvent {
    /// The body was written, or (when `cleared`) the file was removed because the
    /// body was empty. Carries the connection's display name for the message.
    Saved { name: SharedString, cleared: bool },
    /// The write failed. The editor stays open holding the text: a read-only
    /// config dir or a full disk must not silently swallow prose the user just
    /// wrote (or, worse, an agent draft that costs a turn to regenerate).
    SaveFailed(String),
    /// Closed without writing anything.
    Dismissed,
}

/// The open knowledge editor.
pub(crate) struct KnowledgeEditor {
    /// Whose file this is. Held rather than re-derived from `phase` so a save can
    /// never land on a connection the user switched to meanwhile.
    conn_id: String,
    /// The connection's display name, for the modal title.
    name: SharedString,
    /// The markdown surface. Owns the buffer; saving reads it back out.
    editor: Entity<MarkdownEditor>,
    /// The shared modal focus handle RED's focus trap is registered on. Passed in
    /// rather than owned; see the module docs.
    modal_focus: FocusHandle,
    /// RAII: keeps the ⌘↵-saves subscription alive. Never read.
    _sub: gpui::Subscription,
    /// Set when the body came from the agent's `save_knowledge` and the user
    /// hasn't accepted it yet, which puts the review banner at the top.
    draft: bool,
}

impl EventEmitter<KnowledgeEditorEvent> for KnowledgeEditor {}

/// The caret goes into the body: the modal *is* the editor, so there is nothing
/// else worth focusing.
impl Focusable for KnowledgeEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl KnowledgeEditor {
    /// Build the editor over `body`. `draft` marks a body the agent inferred,
    /// which adds the review banner; nothing is written until [`Self::save`].
    pub(crate) fn new(
        conn_id: String,
        name: SharedString,
        body: String,
        draft: bool,
        modal_focus: FocusHandle,
        cx: &mut Context<Self>,
    ) -> Self {
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
            MarkdownEditorEvent::Run => this.save(cx),
            MarkdownEditorEvent::Escape => this.cancel(cx),
            _ => {}
        });
        Self {
            conn_id,
            name,
            editor,
            modal_focus,
            _sub: sub,
            draft,
        }
    }

    /// Write the body to disk and report the outcome. An empty body deletes the
    /// file rather than leaving one behind: "no knowledge" and "a file containing
    /// nothing" should not be two different states.
    fn save(&mut self, cx: &mut Context<Self>) {
        let body = self.editor.read(cx).content();
        let outcome = if body.trim().is_empty() {
            crate::knowledge::delete(&self.conn_id).map(|()| true)
        } else {
            crate::knowledge::save(&self.conn_id, &body).map(|_| false)
        };
        match outcome {
            Ok(cleared) => cx.emit(KnowledgeEditorEvent::Saved {
                name: self.name.clone(),
                cleared,
            }),
            Err(e) => cx.emit(KnowledgeEditorEvent::SaveFailed(e.to_string())),
        }
    }

    /// Close without writing anything.
    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(KnowledgeEditorEvent::Dismissed);
    }
}

impl Render for KnowledgeEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        // The banner only rides a draft: on a file the user wrote themselves it
        // would be noise, and the whole point of the wording is that *this* text
        // has not been checked by a human yet.
        let banner = self.draft.then(|| {
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
                    .child(self.editor.clone()),
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
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        Button::new("knowledge-save", "Save")
                            .variant(ButtonVariant::Primary)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            );

        // `Modal::on_close` fires from the scrim and the ✕ with no event argument,
        // so it takes a plain `(&mut Window, &mut App)` closure that `cx.listener`
        // (which threads an event) cannot satisfy. A weak handle it is.
        let close_view = cx.entity().downgrade();

        // The wrapper carries the key context so bindings can scope to this
        // surface. It repeats the scrim's own `absolute().inset_0()` geometry so
        // the modal it contains lays out exactly as it did when the app rendered
        // it directly.
        // `absolute()` is paired with an explicit `size_full()`, not with
        // `inset_0()`: insets alone leave the box zero-height (the layout test
        // catches exactly that), which would collapse the absolutely-positioned
        // scrim inside it. Same pairing Zed uses for its own overlays.
        div()
            .absolute()
            .size_full()
            .key_context("KnowledgeEditor")
            // Lets the layout test assert this wrapper still covers the window.
            // A no-op outside test builds (see `InteractiveElement::debug_selector`).
            .debug_selector(|| "knowledge-editor-root".to_string())
            .child(
                Modal::new("knowledge-editor")
                    .title(crate::i18n::tr!(
                        "knowledge.title",
                        "Database knowledge - {name}",
                        name = self.name.to_string()
                    ))
                    .width(px(720.))
                    .focus_handle(self.modal_focus.clone())
                    .on_close(move |_, cx| {
                        close_view.update(cx, |this, cx| this.cancel(cx)).ok();
                    })
                    .footer(footer)
                    .child(body),
            )
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
        let modal_focus = self.modal_focus.clone();
        let view = cx.new(|cx| KnowledgeEditor::new(conn_id, name, body, draft, modal_focus, cx));
        let sub = cx.subscribe(&view, Self::on_knowledge_event);
        self.knowledge_editor = Some((view, sub));
        // Put the caret in the editor on the next paint (see `render`).
        self.focus_modal = true;
        cx.notify();
    }

    /// React to what the editor did. The view owns the file; this owns the toast
    /// stack and the cached copy the next assistant turn will send.
    fn on_knowledge_event(
        &mut self,
        _view: Entity<KnowledgeEditor>,
        event: &KnowledgeEditorEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            KnowledgeEditorEvent::Saved { name, cleared } => {
                self.knowledge_editor = None;
                let (variant, message) = if *cleared {
                    (
                        flint::ToastVariant::Info,
                        crate::i18n::tr!(
                            "knowledge.cleared",
                            "Cleared the knowledge file for {name}.",
                            name = name.to_string()
                        ),
                    )
                } else {
                    (
                        flint::ToastVariant::Success,
                        crate::i18n::tr!(
                            "knowledge.saved",
                            "Saved what the agent knows about {name}.",
                            name = name.to_string()
                        ),
                    )
                };
                self.notify(variant, message, cx);
                self.refresh_knowledge();
            }
            // The editor stays open on failure, holding the text: a read-only
            // config dir or a full disk must not silently swallow prose the user
            // just wrote (or, worse, an agent draft that can't be regenerated
            // without spending another turn).
            KnowledgeEditorEvent::SaveFailed(e) => {
                self.notify(
                    flint::ToastVariant::Error,
                    format!("Couldn't save the knowledge file: {e}"),
                    cx,
                );
            }
            KnowledgeEditorEvent::Dismissed => {
                self.knowledge_editor = None;
            }
        }
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
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Build the editor over `body` and collect everything it emits.
    ///
    /// The first view-level test harness in RED (see the module docs). It needs no
    /// window: the view's behaviour is "read the body, touch the file, emit an
    /// outcome", and none of that paints. A render test wants `cx.add_window`.
    fn open(
        cx: &mut gpui::TestAppContext,
        conn_id: &str,
        body: &str,
        draft: bool,
    ) -> (
        Entity<KnowledgeEditor>,
        Rc<RefCell<Vec<KnowledgeEditorEvent>>>,
        gpui::Subscription,
    ) {
        let (conn_id, body) = (conn_id.to_string(), body.to_string());
        cx.update(|cx| {
            let modal_focus = cx.focus_handle();
            let view = cx.new(|cx| {
                KnowledgeEditor::new(
                    conn_id,
                    SharedString::from("Acme"),
                    body,
                    draft,
                    modal_focus,
                    cx,
                )
            });
            let events = Rc::new(RefCell::new(Vec::new()));
            let sub = cx.subscribe(&view, {
                let events = events.clone();
                move |_, event: &KnowledgeEditorEvent, _| events.borrow_mut().push(event.clone())
            });
            (view, events, sub)
        })
    }

    /// A connection id unique to this process, so the real config dir the
    /// knowledge store writes to can't collide with a live connection's file.
    /// Matches the convention in [`crate::knowledge`]'s own round-trip test.
    fn test_conn_id(tag: &str) -> String {
        format!("red-test-view-{tag}-{}", std::process::id())
    }

    #[gpui::test]
    fn saving_a_body_writes_the_file_and_reports_it(cx: &mut gpui::TestAppContext) {
        let id = test_conn_id("save");
        let (view, events, _sub) = open(cx, &id, "# Acme\n\nMRR is in cents.", false);

        cx.update(|cx| view.update(cx, |this, cx| this.save(cx)));

        assert!(
            matches!(
                events.borrow().as_slice(),
                [KnowledgeEditorEvent::Saved { cleared: false, .. }]
            ),
            "a non-empty body saves and says so, once"
        );
        assert_eq!(
            crate::knowledge::read(&id).as_deref(),
            Some("# Acme\n\nMRR is in cents.\n"),
            "the body reached disk"
        );

        crate::knowledge::delete(&id).expect("cleanup");
    }

    #[gpui::test]
    fn saving_an_empty_body_clears_the_file_rather_than_writing_nothing(
        cx: &mut gpui::TestAppContext,
    ) {
        let id = test_conn_id("clear");
        crate::knowledge::save(&id, "# Stale").expect("seed a file to clear");

        // Whitespace only: "no knowledge" and "a file containing nothing" must not
        // be two different states.
        let (view, events, _sub) = open(cx, &id, "   \n\n  ", false);
        cx.update(|cx| view.update(cx, |this, cx| this.save(cx)));

        assert!(
            matches!(
                events.borrow().as_slice(),
                [KnowledgeEditorEvent::Saved { cleared: true, .. }]
            ),
            "an empty body reports a clear, not a save"
        );
        assert_eq!(crate::knowledge::read(&id), None, "the file is gone");
    }

    #[gpui::test]
    fn cancelling_emits_dismissed_and_leaves_the_file_alone(cx: &mut gpui::TestAppContext) {
        let id = test_conn_id("cancel");
        crate::knowledge::save(&id, "# Kept").expect("seed");

        let (view, events, _sub) = open(cx, &id, "# Edited but abandoned", false);
        cx.update(|cx| view.update(cx, |this, cx| this.cancel(cx)));

        assert!(
            matches!(
                events.borrow().as_slice(),
                [KnowledgeEditorEvent::Dismissed]
            ),
            "cancelling reports a dismissal"
        );
        assert_eq!(
            crate::knowledge::read(&id).as_deref(),
            Some("# Kept\n"),
            "cancelling writes nothing: the file on disk is untouched"
        );

        crate::knowledge::delete(&id).expect("cleanup");
    }

    #[gpui::test]
    fn the_modal_lays_out_in_a_window(cx: &mut gpui::TestAppContext) {
        // A render smoke test, and the reason it exists: the view wraps Flint's
        // `Modal` in its own `absolute().inset_0()` div to carry the key context.
        // The modal's scrim is itself absolutely positioned, so a wrapper of the
        // wrong geometry would silently collapse it. Drawing it here is what says
        // the wrapper is transparent to layout.
        let id = test_conn_id("layout");
        cx.update(|cx| cx.set_global(crate::theme::one_dark()));

        let window = cx.add_window(|_window, cx| {
            let modal_focus = cx.focus_handle();
            KnowledgeEditor::new(
                id,
                SharedString::from("Acme"),
                "# Acme".to_string(),
                true, // draft: exercises the banner branch too
                modal_focus,
                cx,
            )
        });

        let viewport = window
            .update(cx, |_this, window, _cx| window.viewport_size())
            .expect("the window is live");

        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        let root = cx
            .debug_bounds("knowledge-editor-root")
            .expect("the view's root drew");
        assert_eq!(
            root.origin,
            gpui::Point::default(),
            "the key-context wrapper starts at the window origin"
        );
        assert_eq!(
            root.size, viewport,
            "and covers the whole window, exactly as the bare scrim did before it \
             was wrapped: an absolutely-positioned child lays out against this box, \
             so any other size would collapse the modal inside it"
        );
    }

    #[gpui::test]
    fn a_draft_is_not_on_disk_until_it_is_saved(cx: &mut gpui::TestAppContext) {
        // The load-bearing property of the agent-draft path: `save_knowledge`
        // opens the editor, it does not write. An unreviewed inferred glossary
        // that reached disk would be handed to every later turn as authoritative.
        let id = test_conn_id("draft");
        let (_view, events, _sub) = open(cx, &id, "# Inferred glossary", true);

        assert!(crate::knowledge::read(&id).is_none(), "nothing was written");
        assert!(events.borrow().is_empty(), "and nothing was reported");
    }

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
