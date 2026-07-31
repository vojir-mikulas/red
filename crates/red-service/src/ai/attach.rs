//! Turning the files a user attached into the blocks a model reads.
//!
//! The classification, the size limits and the refusals all happen UI-side,
//! before a byte crosses the channel: by the time an attachment reaches here it
//! is known-good and this module only decides its *shape* on the wire. That split
//! is deliberate — the service never opens a file for user content, so no path
//! the model produced can turn into a read.
//!
//! Two rules the wire cares about, both asserted below:
//!
//! - **Documents and images come before the text.** The API expects a message's
//!   attachments ahead of the prose that talks about them.
//! - **Base64 carries no newlines.** A wrapping encoder is a 400, and it is a
//!   one-line mistake to make.

use base64::Engine as _;
use red_ai::{ContentBlock, DocumentSource, Message, Role};

use crate::protocol::{AttachmentBody, TurnAttachment};

/// Below this, a text file is inlined as a fenced block in the prose instead of
/// riding as a `document`.
///
/// For a CSV or a log the fence buys everything a document block would except a
/// citation anchor, and it keeps small attachments legible in the transcript. The
/// threshold is where "paste it in" stops being reasonable.
const INLINE_TEXT_LIMIT: usize = 64 * 1024;

/// Build the user turn's message: attachments first, the user's own words last.
///
/// Small text files are folded into `text` as fenced blocks (see
/// [`INLINE_TEXT_LIMIT`]); everything else becomes its own content block.
pub(crate) fn user_message(text: String, attachments: &[TurnAttachment]) -> Message {
    let mut blocks = Vec::new();
    let mut prose = String::new();

    for attachment in attachments {
        match &attachment.body {
            AttachmentBody::Text(body) if body.len() <= INLINE_TEXT_LIMIT => {
                prose.push_str(&fenced(&attachment.name, body));
            }
            AttachmentBody::Text(body) => blocks.push(ContentBlock::Document {
                source: DocumentSource::Text { data: body.clone() },
                title: Some(attachment.name.clone()),
            }),
            AttachmentBody::Bytes(bytes) => match binary_block(attachment, bytes) {
                Ok(block) => blocks.push(block),
                Err(note) => prose.push_str(&note),
            },
        }
    }

    prose.push_str(&text);
    blocks.push(ContentBlock::Text { text: prose });
    Message {
        role: Role::User,
        content: blocks,
    }
}

/// Build an ACP prompt: attachments first, the user's own words last.
///
/// The subscription path has neither a document block RED can rely on nor a
/// system role, so every text attachment inlines as a fence regardless of size.
/// `images` is the agent's advertised capability: without it an image becomes a
/// named line in the prose rather than a block the agent would reject, because a
/// turn that fails outright is worse than one that says what it could not read.
pub(crate) fn acp_blocks(
    text: String,
    attachments: &[TurnAttachment],
    images: bool,
) -> Vec<red_acp::AcpPromptBlock> {
    let mut blocks = Vec::new();
    let mut prose = String::new();

    for attachment in attachments {
        match &attachment.body {
            AttachmentBody::Text(body) => prose.push_str(&fenced(&attachment.name, body)),
            AttachmentBody::Bytes(bytes)
                if images && attachment.media_type.starts_with("image/") =>
            {
                blocks.push(red_acp::AcpPromptBlock::Image {
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                    media_type: attachment.media_type.clone(),
                });
            }
            AttachmentBody::Bytes(_) => prose.push_str(&format!(
                "[the user attached `{}` ({}), which this agent cannot read]\n\n",
                attachment.name, attachment.media_type
            )),
        }
    }

    prose.push_str(&text);
    blocks.push(red_acp::AcpPromptBlock::Text(prose));
    blocks
}

/// An image or a PDF as its content block, or `Err(note)` with a line for the
/// prose when the media type is not one the API takes.
///
/// A type that slipped past classification is described rather than guessed at:
/// sent as an image it is a 400, and sent as a document it is worse — the model
/// reads the noise as content.
fn binary_block(attachment: &TurnAttachment, bytes: &[u8]) -> Result<ContentBlock, String> {
    // `STANDARD` does not wrap. `STANDARD_NO_PAD` would also not wrap but drops
    // the padding the API expects, so the two are not interchangeable.
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    match attachment.media_type.as_str() {
        "application/pdf" => Ok(ContentBlock::Document {
            source: DocumentSource::Pdf { data },
            title: Some(attachment.name.clone()),
        }),
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" => Ok(ContentBlock::Image {
            media_type: attachment.media_type.clone(),
            data,
        }),
        other => Err(format!(
            "[the user attached `{}` ({other}), which this agent cannot read]\n\n",
            attachment.name
        )),
    }
}

/// A small text file as a fenced block, labelled with its name and size so the
/// model can tell the attachment apart from the user's own words.
fn fenced(name: &str, body: &str) -> String {
    let language = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    // A body containing ``` would end the fence early and spill the rest into
    // the prose as though the user had written it; a longer fence cannot be
    // closed by anything inside.
    let fence = if body.contains("```") { "````" } else { "```" };
    format!(
        "Attached `{name}` ({} bytes):\n{fence}{language}\n{}\n{fence}\n\n",
        body.len(),
        body.trim_end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(name: &str, body: &str) -> TurnAttachment {
        TurnAttachment {
            name: name.into(),
            media_type: "text/plain".into(),
            body: AttachmentBody::Text(body.into()),
        }
    }

    fn bytes(name: &str, media_type: &str, len: usize) -> TurnAttachment {
        TurnAttachment {
            name: name.into(),
            media_type: media_type.into(),
            body: AttachmentBody::Bytes(vec![0xAB; len]),
        }
    }

    /// The ordering the API requires, and the one a reader would want anyway:
    /// what was attached, then what was said about it.
    #[test]
    fn attachments_precede_the_text_block() {
        let msg = user_message(
            "does this match?".into(),
            &[
                bytes("shot.png", "image/png", 8),
                bytes("spec.pdf", "application/pdf", 8),
            ],
        );
        assert!(matches!(msg.content[0], ContentBlock::Image { .. }));
        assert!(matches!(msg.content[1], ContentBlock::Document { .. }));
        match &msg.content[2] {
            ContentBlock::Text { text } => assert_eq!(text, "does this match?"),
            other => panic!("the prose comes last, got {other:?}"),
        }
        assert_eq!(msg.content.len(), 3);
    }

    /// One assertion that prevents a whole class of 400s: a wrapping encoder
    /// produces base64 with newlines in it and the API rejects the request.
    #[test]
    fn base64_carries_no_newlines() {
        // Long enough that a 76-column MIME encoder would have wrapped it.
        let msg = user_message(String::new(), &[bytes("big.png", "image/png", 4096)]);
        match &msg.content[0] {
            ContentBlock::Image { data, .. } => {
                assert!(data.len() > 76, "long enough to expose wrapping");
                assert!(!data.contains('\n') && !data.contains('\r'), "no newlines");
            }
            other => panic!("expected an image, got {other:?}"),
        }
    }

    /// A small text file reads as part of the message; a large one becomes a
    /// document block so the prose stays legible.
    #[test]
    fn small_text_inlines_and_large_text_becomes_a_document() {
        let msg = user_message(
            "any duplicates?".into(),
            &[text("vendor.csv", "sku,price\na,1\n")],
        );
        assert_eq!(msg.content.len(), 1, "nothing but the prose");
        match &msg.content[0] {
            ContentBlock::Text { text } => {
                assert!(
                    text.starts_with("Attached `vendor.csv` (14 bytes):\n```csv\n"),
                    "{text}"
                );
                assert!(text.ends_with("any duplicates?"));
            }
            other => panic!("expected text, got {other:?}"),
        }

        let big = "x,y\n".repeat(INLINE_TEXT_LIMIT);
        let msg = user_message("summarize".into(), &[text("huge.csv", &big)]);
        assert!(matches!(
            &msg.content[0],
            ContentBlock::Document {
                source: DocumentSource::Text { .. },
                title: Some(t),
            } if t == "huge.csv"
        ));
    }

    /// A file whose own content closes the fence would otherwise spill into the
    /// prose, where the model reads it as the user talking.
    #[test]
    fn a_fence_inside_the_file_does_not_end_the_block() {
        let msg = user_message(String::new(), &[text("notes.md", "before\n```\nafter")]);
        match &msg.content[0] {
            ContentBlock::Text { text } => {
                assert!(
                    text.starts_with("Attached `notes.md` (16 bytes):\n````md\n"),
                    "{text}"
                );
                assert!(text.contains("\n````\n"));
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// A media type that slipped past classification is described, not guessed
    /// at: sent as an image it is a 400, sent as a document it is noise the model
    /// reads as content.
    #[test]
    fn an_unknown_binary_type_is_described_rather_than_sent() {
        let msg = user_message("what is this?".into(), &[bytes("odd.bmp", "image/bmp", 8)]);
        assert_eq!(msg.content.len(), 1);
        match &msg.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("odd.bmp"));
                assert!(text.contains("cannot read"));
            }
            other => panic!("expected text, got {other:?}"),
        }
    }
}
