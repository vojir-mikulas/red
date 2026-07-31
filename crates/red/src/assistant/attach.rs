//! Classifying, bounding and reading the files a user attaches to a chat.
//!
//! Everything that can say *no* lives here, and it says no **before** anything is
//! sent: an unsupported type or an oversized file is refused in the composer,
//! where the user can do something about it, rather than at 32 MB by a provider
//! whose error they cannot act on. Classification reads the path, never the file,
//! so a refusal costs nothing.
//!
//! Reading happens separately, at send time and off the UI thread — attaching a
//! 20 MB PDF must not stall a frame, and the user may remove it before sending.

use std::path::{Path, PathBuf};

use red_service::{AttachmentBody, TurnAttachment};

/// What an attachment is, which decides both its limit and how it reaches the
/// model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachmentKind {
    /// Anything textual: CSV, SQL, JSON, a log, Markdown.
    Text,
    Image,
    Pdf,
}

impl AttachmentKind {
    /// The icon name for this kind's chip.
    pub(crate) fn icon(self) -> &'static str {
        match self {
            AttachmentKind::Text => "file-text",
            AttachmentKind::Image => "image",
            AttachmentKind::Pdf => "file",
        }
    }
}

impl Attachment {
    /// Whether RED's own import pipeline could load this file into a table.
    ///
    /// Tabular data is usually better asked *about* than read: a table the agent
    /// can query beats a fence it has to hold in context, and gets more accurate
    /// the bigger the file is.
    pub(crate) fn is_importable(&self) -> bool {
        self.kind == AttachmentKind::Text
            && matches!(
                self.path
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase())
                    .as_deref(),
                Some("csv" | "tsv" | "json" | "jsonl" | "ndjson")
            )
    }
}

/// One file staged for the next turn. Holds the path and the facts a chip needs;
/// the contents are read at send time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attachment {
    pub(crate) path: PathBuf,
    /// The file's display name. Never the full path: where it sat on disk is not
    /// the model's business, and it is not what the user is asking about.
    pub(crate) name: String,
    pub(crate) kind: AttachmentKind,
    pub(crate) media_type: String,
    pub(crate) bytes: u64,
}

/// Text past this is not something to paste into a chat. A CSV this size belongs
/// in a table, which is a thing RED can already do.
const MAX_TEXT_BYTES: u64 = 1024 * 1024;
/// The API takes 5 MB of image; past that the answer is to export a smaller one.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
/// Comfortably inside the API's 32 MB request ceiling with room for the rest of
/// the conversation.
const MAX_PDF_BYTES: u64 = 20 * 1024 * 1024;
/// How many files may ride on one turn. The request ceiling is a total, not a
/// per-file limit, so a count bound is what keeps ten large-but-legal files from
/// adding up past it.
pub(crate) const MAX_ATTACHMENTS: usize = 10;

/// Extensions RED will read as text, with the language tag the fenced block gets.
/// An allowlist rather than a UTF-8 sniff, because sniffing means reading the
/// file to find out whether we are allowed to read it.
const TEXT_EXTENSIONS: &[&str] = &[
    "csv", "tsv", "sql", "json", "jsonl", "ndjson", "log", "txt", "md", "markdown", "yaml", "yml",
    "toml", "ini", "conf", "xml", "html", "css", "js", "ts", "py", "rs", "go", "rb", "sh", "env",
];

/// The four image formats the Messages API accepts. A `.bmp` or `.svg` is refused
/// in the picker rather than sent and 400'd.
const IMAGE_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

/// Classify `path` and check it against its kind's limit, without opening it.
///
/// The `Err` is what the composer shows, so it says what is wrong **and what to
/// do instead**: "too big" on its own leaves the user stuck.
pub(crate) fn classify(path: &Path) -> Result<Attachment, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    // Metadata, not contents: enough to enforce the limit, and it stays true for
    // a file we have decided not to read.
    let bytes = std::fs::metadata(path)
        .map_err(|e| format!("{name} could not be read: {e}"))?
        .len();

    let (kind, media_type, limit) =
        if let Some((_, media)) = IMAGE_TYPES.iter().find(|(ext, _)| *ext == extension) {
            (AttachmentKind::Image, (*media).to_string(), MAX_IMAGE_BYTES)
        } else if extension == "pdf" {
            (
                AttachmentKind::Pdf,
                "application/pdf".to_string(),
                MAX_PDF_BYTES,
            )
        } else if TEXT_EXTENSIONS.contains(&extension.as_str()) {
            (
                AttachmentKind::Text,
                "text/plain".to_string(),
                MAX_TEXT_BYTES,
            )
        } else if extension.is_empty() {
            return Err(format!(
                "{name} has no file extension, so RED cannot tell what it is. Rename it with the \
             right extension and try again."
            ));
        } else {
            return Err(format!(
                "RED cannot attach a .{extension} file. It takes text (CSV, SQL, JSON, logs, \
             Markdown), images (PNG, JPEG, GIF, WebP) and PDFs."
            ));
        };

    if bytes > limit {
        return Err(over_limit(&name, kind, bytes, limit));
    }
    if bytes == 0 {
        return Err(format!("{name} is empty."));
    }

    Ok(Attachment {
        path: path.to_path_buf(),
        name,
        kind,
        media_type,
        bytes,
    })
}

/// Why a file is too big, and the thing to do about it. The CSV answer is the
/// one that matters: importing it into a table and asking about *that* is both
/// better and a workflow RED already has.
fn over_limit(name: &str, kind: AttachmentKind, bytes: u64, limit: u64) -> String {
    let (size, cap) = (human_bytes(bytes), human_bytes(limit));
    match kind {
        AttachmentKind::Text => format!(
            "{name} is {size}, over the {cap} limit for a text file. Import it into a table \
             (Import in the connection's menu) and ask about that instead - the agent can then \
             query it rather than read it."
        ),
        AttachmentKind::Image => format!(
            "{name} is {size}, over the {cap} limit for an image. Export it at a smaller size \
             or crop it to the part you are asking about."
        ),
        AttachmentKind::Pdf => format!(
            "{name} is {size}, over the {cap} limit for a PDF. Split it, or extract the pages \
             you are asking about."
        ),
    }
}

/// A file size as a person would say it.
pub(crate) fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Read every staged attachment into the form the turn ships.
///
/// Blocking, and meant to run on the background executor. An unreadable file
/// fails the whole batch rather than being silently dropped: a turn that quietly
/// answered without the screenshot is worse than one that did not send.
pub(crate) fn read_all(attachments: &[Attachment]) -> Result<Vec<TurnAttachment>, String> {
    attachments.iter().map(read_one).collect()
}

fn read_one(attachment: &Attachment) -> Result<TurnAttachment, String> {
    let body = match attachment.kind {
        AttachmentKind::Text => AttachmentBody::Text(
            std::fs::read_to_string(&attachment.path)
                .map_err(|e| format!("{} could not be read as text: {e}", attachment.name))?,
        ),
        AttachmentKind::Image | AttachmentKind::Pdf => AttachmentBody::Bytes(
            std::fs::read(&attachment.path)
                .map_err(|e| format!("{} could not be read: {e}", attachment.name))?,
        ),
    };
    Ok(TurnAttachment {
        name: attachment.name.clone(),
        media_type: attachment.media_type.clone(),
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture file of `bytes` bytes in its own directory, so two tests never
    /// collide on a name.
    fn write(name: &str, bytes: usize) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "red-attach-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, vec![b'a'; bytes]).unwrap();
        path
    }

    /// A type RED cannot send is refused here, where the user can act on it,
    /// rather than by the provider after a multi-megabyte upload.
    #[test]
    fn an_unsupported_type_is_refused_by_name() {
        let path = write("diagram.bmp", 16);
        let why = classify(&path).expect_err("a .bmp is not one of the four formats");
        assert!(why.contains(".bmp"), "{why}");
        assert!(
            why.contains("PNG"),
            "the message says what IS accepted: {why}"
        );

        // A file with no extension cannot be classified without reading it, and
        // reading it to decide whether we may read it is not a rule.
        let path = write("dump", 16);
        assert!(classify(&path).is_err());
    }

    /// Each limit refuses with its own message, and the text one points at the
    /// thing to do instead.
    #[test]
    fn each_limit_refuses_with_something_to_do_about_it() {
        let path = write("vendor.csv", MAX_TEXT_BYTES as usize + 1);
        let why = classify(&path).expect_err("over the text limit");
        assert!(why.contains("1.0 MB"), "{why}");
        assert!(why.contains("Import it into a table"), "{why}");

        // A file at the limit is fine; the check is strictly "over".
        let path = write("ok.csv", MAX_TEXT_BYTES as usize);
        assert!(classify(&path).is_ok());
    }

    /// Classification settles the media type, so the service never has to guess.
    #[test]
    fn classification_settles_the_kind_and_media_type() {
        let path = write("shot.PNG", 8);
        let a = classify(&path).unwrap();
        assert_eq!(a.kind, AttachmentKind::Image);
        assert_eq!(a.media_type, "image/png");
        assert_eq!(a.name, "shot.PNG", "the name is shown as the user wrote it");

        let path = write("report.pdf", 8);
        assert_eq!(classify(&path).unwrap().kind, AttachmentKind::Pdf);

        let path = write("query.sql", 8);
        let a = classify(&path).unwrap();
        assert_eq!(a.kind, AttachmentKind::Text);
        assert_eq!(a.media_type, "text/plain");
    }

    /// An empty file would reach the model as an attachment with nothing in it,
    /// which reads as "the user showed me a file and it said nothing".
    #[test]
    fn an_empty_file_is_refused() {
        let path = write("empty.csv", 0);
        assert!(classify(&path).unwrap_err().contains("empty"));
    }

    #[test]
    fn reading_produces_the_shipped_form() {
        let path = write("notes.txt", 4);
        let read = read_all(&[classify(&path).unwrap()]).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].name, "notes.txt");
        assert_eq!(read[0].body, AttachmentBody::Text("aaaa".into()));
    }
}
