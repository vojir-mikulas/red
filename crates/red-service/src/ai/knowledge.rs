//! `save_knowledge`: the agent drafting the connection's knowledge file.
//!
//! Engine-agnostic, like [`super::report`] -- "write down what you learned about
//! this connection" is the same request on all three seams -- and, like
//! `open_query`, it is a **UI-only** tool: it writes nothing itself, it announces
//! the draft through the [`ReportSink`] and the app decides what to do with it.
//! What the app does is open it for review, never save it: the file it would
//! become is folded into every later system prompt as authoritative, and an
//! unreviewed inferred glossary is worse than no glossary.
//!
//! That also makes it meaningless over the headless `red mcp` stdio transport,
//! which has no editor to open a draft in, so it is named in `UI_ONLY_TOOLS` and
//! `is_headless_tool` drops it there. The in-app ACP grounding server keeps it:
//! that one runs inside RED with a live `ReportSink`, exactly like `open_query`
//! and `save_query`.

use red_ai::ToolDef;
use serde_json::{Value as Json, json};

use super::state::ReportSink;

/// Cap on a drafted knowledge file. Must stay equal to the UI's own load cap
/// (`red::knowledge::MAX_BYTES`): a draft the loader would truncate on the way
/// back in is a draft that was never worth writing, and telling the model to cut
/// it is better than letting the user discover the loss later.
const MAX_KNOWLEDGE_BYTES: usize = 32 * 1024;

/// The `save_knowledge` tool definition, shared by all three seam catalogs.
pub(in crate::ai) fn knowledge_tool_def() -> ToolDef {
    ToolDef {
        name: "save_knowledge".into(),
        description: "Hand the user a draft knowledge file for this connection: the semantic \
            layer that the schema cannot carry (glossary, metric definitions, join rules, \
            per-table notes, gotchas). It opens in an editor for the user to review and save; \
            it is NOT written to disk by this call, and nothing you write here takes effect \
            until they save it. Once saved, it is folded into the system prompt of every later \
            chat on this connection, so write only what you have evidence for and mark anything \
            you are inferring as an inference. Plain markdown, no front matter. Call this once, \
            with the whole document; a second call replaces the first draft."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "body": {
                    "type": "string",
                    "description": "The whole knowledge file as markdown.",
                },
            },
            "required": ["body"],
            "additionalProperties": false,
        }),
    }
}

/// Run one `save_knowledge` call: validate the draft and announce it. Returns the
/// `(content, ok)` pair the tool loop expects.
pub(in crate::ai) fn run_save_knowledge(input: &Json, report: &ReportSink) -> (String, bool) {
    let body = input
        .get("body")
        .and_then(Json::as_str)
        .unwrap_or("")
        .trim();
    if body.is_empty() {
        return (
            "error: save_knowledge needs a non-empty `body`".into(),
            false,
        );
    }
    if body.len() > MAX_KNOWLEDGE_BYTES {
        return (
            format!(
                "error: the draft is {}KB; the knowledge file is capped at {}KB. Cut it to the \
                 lines that change an answer and call save_knowledge again.",
                body.len() / 1024,
                MAX_KNOWLEDGE_BYTES / 1024
            ),
            false,
        );
    }
    report.announce_knowledge_draft(body);
    (
        "Opened the draft in the user's knowledge editor for review. It is not saved yet: say \
         so, and tell them what you were unsure about so they know where to look."
            .into(),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;
    use futures::StreamExt;

    #[tokio::test]
    async fn announces_a_draft_and_refuses_an_empty_or_oversized_one() {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(
            tx,
            None,
            crate::protocol::ConversationId::new(7),
            None,
            None,
        );

        let (msg, ok) = run_save_knowledge(&json!({}), &sink);
        assert!(!ok, "{msg}");
        let (msg, ok) = run_save_knowledge(&json!({ "body": "   " }), &sink);
        assert!(!ok, "{msg}");
        // Over the cap the model is told to cut it, not silently truncated: the
        // user would otherwise find out by losing half their glossary.
        let huge = "x".repeat(MAX_KNOWLEDGE_BYTES + 1);
        let (msg, ok) = run_save_knowledge(&json!({ "body": huge }), &sink);
        assert!(!ok);
        assert!(msg.contains("capped at 32KB"), "{msg}");

        let (msg, ok) = run_save_knowledge(&json!({ "body": "# Acme\n\nMRR is in cents." }), &sink);
        assert!(ok, "{msg}");
        // The tool result has to say it is unsaved, or the agent reports success
        // for a file that does not exist yet.
        assert!(msg.contains("not saved yet"));
        let (_session, event) = rx.next().await.expect("an AiKnowledgeDraft event");
        let Event::AiKnowledgeDraft {
            conversation_id,
            body,
        } = event
        else {
            panic!("expected AiKnowledgeDraft, got {event:?}");
        };
        assert_eq!(conversation_id.get(), 7);
        assert_eq!(body, "# Acme\n\nMRR is in cents.");
    }
}
