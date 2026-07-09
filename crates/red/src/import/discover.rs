//! Locate the default on-disk data directories for DBeaver and DBGate so the UI
//! can offer "we found an install here" without the user hunting for paths. Pure
//! path construction + existence checks; the parsers ([`super::dbeaver`],
//! [`super::dbgate`]) take a directory, so a user can always point at a custom
//! location the auto-probe missed (a `-data` workspace, a moved `.dbgate`).

use std::path::{Path, PathBuf};

use super::ImportSource;

/// A discovered source directory ready to hand to [`super::run`].
#[derive(Debug, Clone)]
pub struct Found {
    pub source: ImportSource,
    /// The directory to import from (a `.dbeaver` project folder, or `.dbgate`).
    pub dir: PathBuf,
    /// A short human label, e.g. the DBeaver project name.
    pub label: String,
}

/// Probe the conventional locations for both tools and return every source that
/// exists (has its primary file). Empty when nothing is installed.
pub fn detect() -> Vec<Found> {
    let mut found = Vec::new();
    found.extend(detect_dbeaver());
    found.extend(detect_dbgate());
    found
}

/// DBeaver: `<DBeaverData>/workspace6/<project>/.dbeaver/data-sources.json`, one
/// entry per project (the default project is `General`; users can add more).
/// Probes the native data dir and every Flatpak sandbox that holds a DBeaverData.
fn detect_dbeaver() -> Vec<Found> {
    let mut out = Vec::new();
    for (root, tag) in dbeaver_roots() {
        let workspace = root.join("workspace6");
        let Ok(entries) = std::fs::read_dir(&workspace) else {
            continue;
        };
        for entry in entries.flatten() {
            let project = entry.path();
            if !project.is_dir() {
                continue;
            }
            let dir = project.join(".dbeaver");
            if dir.join("data-sources.json").is_file() {
                let name = project
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let label = match (&name.is_empty(), tag) {
                    (false, Some(tag)) => format!("DBeaver ({name}, {tag})"),
                    (false, None) => format!("DBeaver ({name})"),
                    (true, Some(tag)) => format!("DBeaver ({tag})"),
                    (true, None) => "DBeaver".to_string(),
                };
                out.push(Found {
                    source: ImportSource::DBeaver,
                    dir,
                    label,
                });
            }
        }
    }
    out
}

/// DBGate: `os.homedir()/.dbgate/connections.jsonl`. Probes the native home and
/// every Flatpak sandbox home (where DBGate's redirected `$HOME` puts it, so a
/// Flatpak install isn't invisible to the import).
fn detect_dbgate() -> Vec<Found> {
    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push((home.join(".dbgate"), "DBGate".to_string()));
    }
    for root in flatpak_homes() {
        candidates.push((root.join(".dbgate"), "DBGate (Flatpak)".to_string()));
    }
    candidates
        .into_iter()
        .filter(|(dir, _)| dir.join("connections.jsonl").is_file())
        .map(|(dir, label)| Found {
            source: ImportSource::DBGate,
            dir,
            label,
        })
        .collect()
}

/// Every `DBeaverData` root to probe: the native one plus each Flatpak sandbox's
/// (paired with a short tag for the label; `None` for the native install).
fn dbeaver_roots() -> Vec<(PathBuf, Option<&'static str>)> {
    let mut roots = Vec::new();
    #[cfg(target_os = "macos")]
    {
        // macOS keeps DBeaverData under ~/Library, not the XDG data dir.
        if let Some(h) = dirs::home_dir() {
            roots.push((h.join("Library").join("DBeaverData"), None));
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        // dirs::data_dir() is %APPDATA% on Windows and $XDG_DATA_HOME (~/.local/share)
        // on Linux, both where a native DBeaver puts DBeaverData.
        if let Some(d) = dirs::data_dir() {
            roots.push((d.join("DBeaverData"), None));
        }
    }
    // Flatpak maps $XDG_DATA_HOME to <sandbox home>/data, so DBeaverData lands there.
    for home in flatpak_homes() {
        roots.push((home.join("data").join("DBeaverData"), Some("Flatpak")));
    }
    roots
}

/// The per-app home directories Flatpak creates under `~/.var/app/<app-id>/`.
/// A sandboxed app's `$HOME` (and thus `os.homedir()`) is redirected here, so a
/// store the tool writes to `~/…` actually lives under one of these. Empty off
/// Linux (the directory won't exist), so callers pay only a failed `read_dir`.
fn flatpak_homes() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(home.join(".var").join("app")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

/// Whether a user-supplied path looks like a valid source directory for `source`,
/// so a "Browse…" picker can validate before importing.
pub fn looks_valid(source: ImportSource, dir: &Path) -> bool {
    match source {
        ImportSource::DBeaver => dir.join("data-sources.json").is_file(),
        ImportSource::DBGate => dir.join("connections.jsonl").is_file(),
    }
}
