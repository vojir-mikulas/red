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
fn detect_dbeaver() -> Vec<Found> {
    let Some(root) = dbeaver_root() else {
        return Vec::new();
    };
    let workspace = root.join("workspace6");
    let Ok(entries) = std::fs::read_dir(&workspace) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let project = entry.path();
        if !project.is_dir() {
            continue;
        }
        let dir = project.join(".dbeaver");
        if dir.join("data-sources.json").is_file() {
            let label = project
                .file_name()
                .map(|n| format!("DBeaver ({})", n.to_string_lossy()))
                .unwrap_or_else(|| "DBeaver".to_string());
            out.push(Found {
                source: ImportSource::DBeaver,
                dir,
                label,
            });
        }
    }
    out
}

/// DBGate: `~/.dbgate/connections.jsonl`.
fn detect_dbgate() -> Vec<Found> {
    let Some(dir) = dbgate_dir() else {
        return Vec::new();
    };
    if dir.join("connections.jsonl").is_file() {
        vec![Found {
            source: ImportSource::DBGate,
            dir,
            label: "DBGate".to_string(),
        }]
    } else {
        Vec::new()
    }
}

/// The `DBeaverData` root. macOS keeps it under `~/Library`, not the XDG data dir;
/// Windows uses `%APPDATA%` (roaming); Linux uses `$XDG_DATA_HOME`.
fn dbeaver_root() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| h.join("Library").join("DBeaverData"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        // dirs::data_dir() is %APPDATA% on Windows and $XDG_DATA_HOME (~/.local/share)
        // on Linux, both where DBeaver puts DBeaverData.
        dirs::data_dir().map(|d| d.join("DBeaverData"))
    }
}

/// The `.dbgate` directory (`os.homedir()/.dbgate` in DBGate).
fn dbgate_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".dbgate"))
}

/// Whether a user-supplied path looks like a valid source directory for `source`,
/// so a "Browse…" picker can validate before importing.
pub fn looks_valid(source: ImportSource, dir: &Path) -> bool {
    match source {
        ImportSource::DBeaver => dir.join("data-sources.json").is_file(),
        ImportSource::DBGate => dir.join("connections.jsonl").is_file(),
    }
}
