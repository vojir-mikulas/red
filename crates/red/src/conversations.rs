//! Conversation history: persisted AI chats, one JSON file per conversation (M-S5).
//!
//! The assistant panel streams an ephemeral transcript; saved conversations are
//! what the user keeps. Each lives as one file under
//! `<config>/red/conversations/*.json`, beside `queries/`, `themes/`, and
//! `settings.toml`. JSON (not Markdown) because a chat carries structured turns
//! (roles, summarized thinking, the provider it ran on) that round-trip cleanly,
//! and the file stays plain enough to read, hand-edit, or delete in a file manager.
//!
//! Like the saved-queries loader in `queries.rs`, this is just the filesystem:
//! there is no database and nothing is read at startup. The history picker calls
//! [`load`] on demand, so external edits/deletions show up on the next open and
//! saved chats cost the budget nothing at idle. **No secrets ever land here.**
//! The subscription path's tokens stay with the agent; this stores only the
//! transcript, title, and which backend produced it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One persisted turn: who spoke, the visible text, and (assistant turns only) any
/// summarized thinking. Mirrors the panel's `ChatMessage` without depending on it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct StoredMessage {
    /// `"user"` or `"assistant"`.
    pub role: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub thinking: String,
    /// The turn's activity timeline (tool calls, subagents, writes). Defaulted so
    /// conversations saved before the timeline existed still load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity: Vec<red_core::ActivityNode>,
    /// The turn's plan checklist, if the agent published one. Defaulted for old files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan: Vec<red_core::PlanStep>,
}

/// A saved conversation: its transcript, a display title, the provider binding it
/// ran on, and timestamps for ordering. `path`/`stem` are filled in on [`load`]
/// and skipped in the file itself (the filename *is* the stem).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Conversation {
    /// Display title, derived from the first user message at save time.
    pub title: String,
    /// Which backend produced it (`"subscription"`, `"anthropic"`, …), recorded
    /// for display and for a future per-chat binding (M-S6). Informational in M-S5,
    /// where a single active chat runs on the current `[ai] provider`.
    pub provider: String,
    /// Unix seconds when first saved.
    #[serde(default)]
    pub created_unix: u64,
    /// Unix seconds of the most recent save; the picker's sort key.
    #[serde(default)]
    pub updated_unix: u64,
    /// The transcript, oldest first.
    pub messages: Vec<StoredMessage>,
    /// The backing file (set on load; not serialized).
    #[serde(skip)]
    pub path: PathBuf,
    /// The file stem, reused so re-saving the same chat overwrites in place (set on
    /// load; not serialized).
    #[serde(skip)]
    pub stem: String,
}

/// `<config>/red/conversations`, the saved-conversations directory.
pub(crate) fn conversations_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("conversations"))
}

/// Seconds since the Unix epoch, saturating to 0 before it (clock skew only).
pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read every `*.json` in the conversations dir, skipping (with a warning) any that
/// won't parse, so one bad file never blocks the others. Sorted by `updated_unix`
/// descending (most recent first) for the picker. A missing dir is an empty list.
pub(crate) fn load() -> Vec<Conversation> {
    let Some(dir) = conversations_dir() else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<Conversation>(&contents) {
                Ok(mut conv) => {
                    conv.path = path.clone();
                    conv.stem = stem.to_string();
                    out.push(conv);
                }
                Err(e) => tracing::warn!("ignoring conversation {}: {e}", path.display()),
            },
            Err(e) => tracing::warn!("ignoring conversation {}: {e}", path.display()),
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.updated_unix));
    out
}

/// Write `conv` to `<dir>/<stem>.json` atomically (temp file + rename) so a crash
/// can't leave a partial file. Re-saving the same stem overwrites in place; the
/// caller keeps the stem stable across a conversation's turns.
pub(crate) fn save(stem: &str, conv: &Conversation) -> Result<PathBuf> {
    use std::io::Write;

    let dir = conversations_dir().context("no config directory for conversations")?;
    std::fs::create_dir_all(&dir).context("creating the conversations directory")?;
    let dest = dir.join(format!("{stem}.json"));

    let contents = serde_json::to_string_pretty(conv).context("serializing the conversation")?;
    let tmp = dest.with_extension(format!("json.tmp.{}", std::process::id()));
    // Owner-only (0o600) on Unix: a transcript can quote query results the user
    // asked about, so it's confidential even though no credentials land here; the
    // same posture as the AI report/audit files. `rename` preserves the mode.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .context("creating the conversation temp file")?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, &dest).context("renaming the conversation temp file")?;
    Ok(dest)
}

/// Delete a saved conversation's file (the panel's Delete action). A missing file
/// is fine, since the user may have removed it by hand.
pub(crate) fn delete(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("deleting the conversation file"),
    }
}

/// A free filename stem for a new conversation: a slug of `title`, suffixed
/// `-2`, `-3`, … on collision so two chats with the same title don't overwrite
/// each other. Chosen once, when a chat is first saved, then reused.
pub(crate) fn unique_stem(title: &str) -> String {
    let base = slug(title);
    let Some(dir) = conversations_dir() else {
        return base;
    };
    if !dir.join(format!("{base}.json")).exists() {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !dir.join(format!("{candidate}.json")).exists() {
            return candidate;
        }
    }
    base
}

/// A filesystem-safe stem for a title: lowercased, non-alphanumerics folded to
/// `-`, edges trimmed, capped so a long first message doesn't make a long filename.
/// Mirrors `queries.rs`'s `slug`.
fn slug(title: &str) -> String {
    let s: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // Collapse runs of `-` and trim, then cap to keep filenames sane.
    let mut collapsed = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_dash {
                collapsed.push('-');
            }
            prev_dash = true;
        } else {
            collapsed.push(c);
            prev_dash = false;
        }
    }
    let trimmed = collapsed.trim_matches('-');
    let capped: String = trimmed.chars().take(48).collect();
    let capped = capped.trim_matches('-');
    if capped.is_empty() {
        "chat".to_string()
    } else {
        capped.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_filesystem_safe_and_capped() {
        assert_eq!(slug("How many active users?"), "how-many-active-users");
        assert_eq!(slug("  !!!  "), "chat");
        assert_eq!(slug("a / b \\ c"), "a-b-c");
        // Runs of separators collapse to a single dash.
        assert_eq!(slug("a---b"), "a-b");
        // Long titles are capped (and never end on a dash).
        let long = "x".repeat(100);
        assert!(slug(&long).len() <= 48);
    }

    #[test]
    fn conversation_round_trips_through_json() {
        let conv = Conversation {
            title: "Active users".into(),
            provider: "subscription".into(),
            created_unix: 100,
            updated_unix: 200,
            messages: vec![
                StoredMessage {
                    role: "user".into(),
                    text: "how many users?".into(),
                    thinking: String::new(),
                    ..Default::default()
                },
                StoredMessage {
                    role: "assistant".into(),
                    text: "1,234".into(),
                    thinking: "counting".into(),
                    ..Default::default()
                },
            ],
            path: PathBuf::new(),
            stem: String::new(),
        };
        let json = serde_json::to_string_pretty(&conv).unwrap();
        // Transient fields stay out of the file.
        assert!(!json.contains("\"path\""));
        assert!(!json.contains("\"stem\""));
        let back: Conversation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.title, "Active users");
        assert_eq!(back.provider, "subscription");
        assert_eq!(back.messages.len(), 2);
        assert_eq!(back.messages[1].thinking, "counting");
    }
}
