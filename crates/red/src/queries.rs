//! Saved queries: named SQL snippets persisted as plain `.sql` files (B3).
//!
//! Query *history* (per-tab, ephemeral; see `editor.rs`) remembers what you ran;
//! saved queries are what you choose to **keep**. Each lives as one file under
//! `<config>/red/queries/*.sql`, beside `themes/`, `settings.toml`, and
//! `connections.toml`. The file body **is** the query (greppable, editable in any
//! editor with SQL highlighting, runnable verbatim), with optional metadata in a
//! leading SQL-comment header (`-- name:` / `-- description:` / `-- tags:`) that
//! keeps the file valid SQL.
//!
//! There is **no** database and no bespoke format: this module mirrors the
//! user-themes loader in `theme.rs` (read the dir, skip a bad file with a warning,
//! slug a name on save, write atomically). Nothing is read at startup; the picker
//! calls [`load`] on demand, so saved queries cost the budget nothing at idle.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// One saved query: its display name, optional metadata, the **full** file body
/// (runnable verbatim, header included), and the file backing it.
#[derive(Clone, Debug)]
pub(crate) struct SavedQuery {
    /// The header `name:` if present, else the un-slugged filename stem.
    pub name: String,
    /// The header `description:`, shown as a hint in the picker.
    pub description: Option<String>,
    /// The header `tags:` (comma-separated), retained for a future filter.
    #[allow(dead_code)]
    pub tags: Vec<String>,
    /// The complete file contents: what drops into the editor, runnable as-is.
    pub sql: String,
    /// The backing `.sql` file, for a future rename / delete.
    #[allow(dead_code)]
    pub path: PathBuf,
}

/// `<config>/red/queries`, the saved-queries directory.
fn queries_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("queries"))
}

/// Read every `*.sql` in the queries dir into a [`SavedQuery`], skipping (with a
/// warning) any that won't read, so one bad file never blocks the others. Sorted by
/// name (case-insensitive) for a stable picker order. A missing dir is an empty
/// list, never an error.
pub(crate) fn load() -> Vec<SavedQuery> {
    let Some(dir) = queries_dir() else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(contents) => out.push(parse_saved_query(stem, &contents, path.clone())),
            Err(e) => tracing::warn!("ignoring saved query {}: {e}", path.display()),
        }
    }
    out.sort_by_key(|q| q.name.to_lowercase());
    out
}

/// Save `sql` under `name` (with an optional `description`), returning the file
/// written. Writes the managed `-- name:` / `-- description:` header (stripping any
/// the body already carries, so re-saving an opened query doesn't stack headers)
/// over the body, atomically (temp file + rename) so a crash can't leave a partial
/// file. The file stem is a slug of the name, so re-saving the same name overwrites
/// in place.
pub(crate) fn save(name: &str, description: Option<&str>, sql: &str) -> Result<PathBuf> {
    use std::io::Write;

    let dir = queries_dir().context("no config directory for saved queries")?;
    std::fs::create_dir_all(&dir).context("creating the queries directory")?;
    let dest = dir.join(format!("{}.sql", slug(name)));

    let body = strip_managed_header(sql);
    let header = match description.map(str::trim).filter(|d| !d.is_empty()) {
        Some(desc) => format!("-- name: {}\n-- description: {desc}\n\n", name.trim()),
        None => format!("-- name: {}\n\n", name.trim()),
    };
    let contents = format!("{header}{}\n", body.trim_end());

    let tmp = dest.with_extension(format!("sql.tmp.{}", std::process::id()));
    // Owner-only on Unix: a saved snippet can embed literal credentials or PII in a
    // `WHERE` clause, the same content class as the query history (`history.rs`).
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp).context("creating the query temp file")?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, &dest).context("renaming the query temp file")?;
    Ok(dest)
}

/// Parse a file's contents into a [`SavedQuery`]. The leading run of managed
/// `-- key: value` comment lines (`name` / `description` / `tags`, blank lines
/// allowed between) is read as metadata; the **first** non-managed line ends the
/// header. The whole file is kept as `sql` so the snippet stays runnable verbatim.
/// A missing/empty `name:` falls back to the un-slugged filename stem.
fn parse_saved_query(stem: &str, contents: &str, path: PathBuf) -> SavedQuery {
    let mut name = None;
    let mut description = None;
    let mut tags = Vec::new();

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(comment) = trimmed.strip_prefix("--") else {
            break; // first SQL line; header is done.
        };
        let comment = comment.trim();
        if let Some(v) = comment.strip_prefix("name:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = comment.strip_prefix("description:") {
            description = Some(v.trim().to_string()).filter(|s: &String| !s.is_empty());
        } else if let Some(v) = comment.strip_prefix("tags:") {
            tags = v
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
        } else {
            break; // a non-managed comment ends the header.
        }
    }

    let name = name
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| unslug(stem));
    SavedQuery {
        name,
        description,
        tags,
        sql: contents.to_string(),
        path,
    }
}

/// Drop the leading run of managed header lines (and the blank lines among them)
/// so [`save`] can rewrite exactly one. Everything from the first non-managed line
/// on is kept verbatim, so a user's own leading comment survives.
fn strip_managed_header(sql: &str) -> String {
    let mut body = Vec::new();
    let mut in_header = true;
    for line in sql.lines() {
        if in_header {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(comment) = trimmed.strip_prefix("--") {
                let comment = comment.trim();
                if comment.starts_with("name:")
                    || comment.starts_with("description:")
                    || comment.starts_with("tags:")
                {
                    continue;
                }
            }
            in_header = false;
        }
        body.push(line);
    }
    body.join("\n")
}

/// A filesystem-safe stem for a query name: lowercased, non-alphanumerics folded
/// to `-`, edges trimmed. Mirrors `theme.rs`'s `slug`.
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = s.trim_matches('-');
    if trimmed.is_empty() {
        "query".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Turn a filename stem back into a display name when the file has no `name:`
/// header (`-`/`_` become spaces). Predictable, if not capitalization-perfect.
fn unslug(stem: &str) -> String {
    let name: String = stem
        .chars()
        .map(|c| if c == '-' || c == '_' { ' ' } else { c })
        .collect();
    let trimmed = name.trim();
    if trimmed.is_empty() {
        stem.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(stem: &str, contents: &str) -> SavedQuery {
        parse_saved_query(stem, contents, PathBuf::from(format!("{stem}.sql")))
    }

    #[test]
    fn parses_full_header() {
        let q = parse(
            "whatever",
            "-- name: Active users\n-- description: by region\n-- tags: analytics, users\nSELECT 1;",
        );
        assert_eq!(q.name, "Active users");
        assert_eq!(q.description.as_deref(), Some("by region"));
        assert_eq!(q.tags, vec!["analytics", "users"]);
        // The body stays runnable verbatim, header included.
        assert!(q.sql.contains("SELECT 1;"));
    }

    #[test]
    fn name_falls_back_to_unslugged_stem() {
        let q = parse("active-users-by_region", "SELECT 1;");
        assert_eq!(q.name, "active users by region");
        assert_eq!(q.description, None);
        assert!(q.tags.is_empty());
    }

    #[test]
    fn non_managed_leading_comment_ends_header() {
        // A plain comment isn't metadata: name still comes from the stem, and the
        // comment is preserved in the body.
        let q = parse("my-query", "-- just a note\nSELECT 1;");
        assert_eq!(q.name, "my query");
        assert!(q.sql.starts_with("-- just a note"));
    }

    #[test]
    fn empty_name_header_falls_back() {
        let q = parse("fallback-name", "-- name:\nSELECT 1;");
        assert_eq!(q.name, "fallback name");
    }

    #[test]
    fn strip_then_compose_is_idempotent() {
        // Saving an opened query (which carries our header) must not stack headers.
        let opened = "-- name: First\n\nSELECT 1;";
        let body = strip_managed_header(opened);
        assert_eq!(body.trim(), "SELECT 1;");
        let recomposed = format!("-- name: {}\n\n{}\n", "Second", body.trim_end());
        // Exactly one managed header line survives.
        assert_eq!(recomposed.matches("-- name:").count(), 1);
        assert!(recomposed.contains("-- name: Second"));
        // And it round-trips back to a clean body.
        assert_eq!(strip_managed_header(&recomposed).trim(), "SELECT 1;");
    }

    #[test]
    fn strip_keeps_user_leading_comment() {
        let body = strip_managed_header("-- count rows\nSELECT count(*) FROM t;");
        assert_eq!(body, "-- count rows\nSELECT count(*) FROM t;");
    }

    #[test]
    fn slug_is_filesystem_safe() {
        assert_eq!(slug("Active Users by Region"), "active-users-by-region");
        assert_eq!(slug("  !!!  "), "query");
        assert_eq!(slug("a/b\\c"), "a-b-c");
    }
}
