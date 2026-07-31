//! The per-connection knowledge file: a `RED.md` for a database.
//!
//! Schema is syntax. What a metric *means*, which join path is the real one, and
//! which column is dead since 2024 lives in people's heads, not in
//! `information_schema` — and it is where a database agent's wrong answers come
//! from. This module is the smallest thing that writes it down: one markdown file
//! per connection at `<config>/red/knowledge/<conn-id>.md`, folded into the
//! agent's system prompt.
//!
//! Deliberately schema-less. No front-matter, no metric DSL, no validation: the
//! model reads prose, and a five-minute edit should stay a five-minute edit. The
//! storage mirrors `red_config::queries` (read on demand, write atomically,
//! owner-only on Unix) because the contents are the same class of secret: they
//! name internal systems and business logic.
//!
//! Reading happens **UI-side**, like the schema summary and the report folder,
//! and rides to the backend in `AiContext`. The context is rebuilt per turn, so
//! an edit — in the app or in any other editor — takes effect on the next
//! message, with no cache to invalidate.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The most of a knowledge file that is ever folded into the system prompt.
///
/// The file rides in the prompt-cached prefix, so its cost is one cache write per
/// conversation rather than a re-read per turn — but the cache is not free and a
/// context window is not infinite. Past this, [`load`] keeps the first 32 KiB and
/// says so ([`truncation_note`]) rather than silently dropping half of it: a
/// database that needs more prose than this wants a real semantic layer, and RED
/// should say so instead of pretending.
///
/// `red-service`'s `ai::knowledge::MAX_KNOWLEDGE_BYTES` refuses an agent draft
/// larger than this, and must stay equal to it: a draft the loader would truncate
/// on the way back in is one the agent should have been told to cut instead.
const MAX_BYTES: usize = 32 * 1024;

/// `<config>/red/knowledge`, beside `queries/` and `connections.toml`.
fn knowledge_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("knowledge"))
}

/// The knowledge file backing `conn_id`, or `None` when the platform has no
/// config directory.
fn knowledge_path(conn_id: &str) -> Option<PathBuf> {
    Some(knowledge_dir()?.join(format!("{}.md", file_stem(conn_id))))
}

/// Read `conn_id`'s knowledge file verbatim, or `None` when there isn't one.
///
/// Fail-open, like `QueryHistory::load` and the user-theme loader: a missing file
/// is "no knowledge" and an unreadable one is warned about and dropped, because
/// the agent losing a glossary is a worse outcome than the agent not starting.
///
/// This is what the **editor** opens. It is deliberately uncapped: [`load`] can
/// append a truncation note, and editing that would write the note into the file.
pub(crate) fn read(conn_id: &str) -> Option<String> {
    let path = knowledge_path(conn_id)?;
    match std::fs::read_to_string(&path) {
        Ok(body) => Some(body),
        // `NotFound` is the overwhelmingly common case (most connections have no
        // knowledge file); only a real failure is worth a line in the log.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!("ignoring knowledge file {}: {e}", path.display());
            None
        }
    }
}

/// Read `conn_id`'s knowledge file as the **prompt** should see it: capped at
/// [`MAX_BYTES`], or `None` when there isn't one worth sending.
///
/// A file that is only whitespace is `None` — an empty heading left behind by a
/// cleared editor should not occupy a block of the system prompt.
pub(crate) fn load(conn_id: &str) -> Option<String> {
    let body = read(conn_id)?;
    let path = knowledge_path(conn_id)?;
    (!body.trim().is_empty()).then(|| cap(body, &path))
}

/// Write `body` as `conn_id`'s knowledge file, returning the file written.
///
/// Atomic (temp file + rename) so a crash can't leave a half-written file that
/// would then be fed to the model as authoritative, and owner-only on Unix for
/// the same reason `queries.rs` is: the contents describe internal systems.
pub(crate) fn save(conn_id: &str, body: &str) -> Result<PathBuf> {
    use std::io::Write;

    let dir = knowledge_dir().context("no config directory for the knowledge file")?;
    std::fs::create_dir_all(&dir).context("creating the knowledge directory")?;
    let dest = dir.join(format!("{}.md", file_stem(conn_id)));

    let tmp = dest.with_extension(format!("md.tmp.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .context("creating the knowledge temp file")?;
    file.write_all(body.trim_end().as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, &dest).context("renaming the knowledge temp file")?;
    Ok(dest)
}

/// Delete `conn_id`'s knowledge file. A missing file is success: the caller asked
/// for "no knowledge here", and that is already true.
pub(crate) fn delete(conn_id: &str) -> Result<()> {
    let Some(path) = knowledge_path(conn_id) else {
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("deleting the knowledge file"),
    }
}

/// Keep the first [`MAX_BYTES`] and append [`truncation_note`], cutting on a char
/// boundary so a multi-byte character is never split (the same bug class the
/// agent's own result cap guards against).
fn cap(mut body: String, path: &Path) -> String {
    if body.len() <= MAX_BYTES {
        return body;
    }
    let mut cut = MAX_BYTES;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    body.truncate(cut);
    body.push_str(&truncation_note(path));
    body
}

/// The note appended when a knowledge file is too large to load whole. It names
/// the file so the user can go and shorten it, and tells the model that what it
/// is reading is partial rather than letting it reason over half a glossary as
/// if it were the whole one.
fn truncation_note(path: &Path) -> String {
    format!(
        "\n\n(Truncated at {} KiB: the rest of {} was not loaded. Treat the notes \
         above as incomplete, and tell the user their knowledge file is too long \
         for the prompt.)",
        MAX_BYTES / 1024,
        path.display()
    )
}

/// A filesystem-safe stem for a connection id.
///
/// Ids are minted as `conn-<hex>-<hex>`, so in practice this is the identity — but
/// an id also arrives from an imported DBeaver/DBGate profile, and a stem is
/// pasted straight into a path. Anything outside `[A-Za-z0-9._-]` folds to `-`,
/// which keeps `..` from ever addressing a parent directory, and an id that folds
/// away entirely gets a constant name rather than an empty one.
fn file_stem(conn_id: &str) -> String {
    let mut stem: String = conn_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // A stem of dots is still a relative path (`.`, `..`); a leading dot only
    // makes the file hidden, so it's the all-dots case that has to go.
    if stem.is_empty() || stem.chars().all(|c| c == '.') {
        stem = "connection".to_string();
    }
    stem
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_is_filesystem_safe_and_never_escapes_the_directory() {
        assert_eq!(file_stem("conn-18f3a2b1c-0"), "conn-18f3a2b1c-0");
        assert_eq!(file_stem("../../etc/passwd"), "..-..-etc-passwd");
        assert_eq!(file_stem(".."), "connection");
        assert_eq!(file_stem(""), "connection");
        // A stem never contains a separator, so the join can only ever land in
        // the knowledge directory.
        for id in ["a/b", "a\\b", "..", "", "c:\\x"] {
            let stem = file_stem(id);
            assert!(!stem.contains('/') && !stem.contains('\\'), "{stem}");
            assert_ne!(stem, "..");
        }
    }

    #[test]
    fn cap_truncates_on_a_char_boundary_and_names_the_file() {
        let path = PathBuf::from("/tmp/red-knowledge.md");
        // Under the cap: returned verbatim, no note.
        let small = cap("# Acme\n".to_string(), &path);
        assert_eq!(small, "# Acme\n");
        assert!(!small.contains("Truncated"));
        // Multi-byte content capped mid-codepoint cuts at the boundary below the
        // cap rather than splitting the character (which would not be valid UTF-8
        // and, before the boundary walk, was a panic).
        let long = "é".repeat(MAX_BYTES); // 2 bytes each, so twice the cap
        let capped = cap(long, &path);
        let body = capped
            .split("\n\n(Truncated")
            .next()
            .expect("the body precedes the note");
        assert!(body.len() <= MAX_BYTES);
        assert!(body.chars().all(|c| c == 'é'));
        assert!(capped.contains("/tmp/red-knowledge.md"));
        assert!(capped.contains("32 KiB"));
    }

    #[test]
    fn round_trips_through_the_config_dir() {
        // `dirs::config_dir()` is process-wide, so this exercises the real path
        // rather than a temp one; the id is unique per run so it can't collide
        // with a real connection's file.
        let Some(dir) = knowledge_dir() else {
            return; // no config dir on this platform: nothing to assert
        };
        let id = format!("red-test-{}", std::process::id());
        assert_eq!(load(&id), None, "a missing file is no knowledge");

        let path = save(&id, "# Test\n\nMRR is in cents.").expect("save writes");
        assert_eq!(path, dir.join(format!("{id}.md")));
        assert_eq!(
            load(&id).as_deref(),
            Some("# Test\n\nMRR is in cents.\n"),
            "the body round-trips, newline-terminated"
        );

        // Owner-only: a knowledge file names internal systems, like a saved query.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "knowledge files are owner-only");
        }

        // Whitespace-only is "no knowledge", not an empty prompt block.
        save(&id, "   \n\n  ").expect("save writes");
        assert_eq!(load(&id), None);

        delete(&id).expect("delete removes it");
        assert_eq!(load(&id), None);
        // Deleting again is success, not an error.
        delete(&id).expect("deleting a missing file is a no-op");
    }
}
