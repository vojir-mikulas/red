//! Small, app-managed local state that isn't a user *preference*; it lives apart
//! from `settings.toml` (which the user edits) in `<config>/red/state.json`.
//!
//! Today it holds one fact: the last app version we showed the user. Comparing it
//! to [`crate::changelog::VERSION`] on startup is how we raise the "RED updated
//! to X" toast exactly once after an update (see `AppState::new`). The on-disk
//! shape is a wrapper object so future app state can be added without breaking
//! older files.
//!
//! Persistence mirrors `history.rs`: a missing or corrupt file is simply empty
//! state (never blocks startup), and writes go through a temp file + rename,
//! owner-only on Unix.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// `<config>/red/state.json`.
fn state_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("state.json"))
}

/// The on-disk shape: a wrapper object (not a bare value) so new fields can be
/// added later without breaking older files.
#[derive(Default, Serialize, Deserialize)]
struct StateFile {
    /// The app version the user last saw, or absent on a first-ever launch.
    #[serde(default)]
    last_seen_version: Option<String>,
}

/// The app-state store. Loaded once at startup; `mark_seen` persists immediately.
pub(crate) struct LocalState {
    last_seen_version: Option<String>,
    path: Option<PathBuf>,
}

impl LocalState {
    /// Read state from disk, or start empty. Never fails: a missing file is empty
    /// state; a corrupt one is warned about and dropped (fail-open, like the other
    /// persisted-data loaders).
    pub(crate) fn load() -> Self {
        let path = state_path();
        let last_seen_version = match path.as_ref().map(std::fs::read_to_string) {
            Some(Ok(contents)) => match serde_json::from_str::<StateFile>(&contents) {
                Ok(file) => file.last_seen_version,
                Err(e) => {
                    tracing::warn!("ignoring corrupt app state: {e}");
                    None
                }
            },
            // Missing file or unreadable dir means empty state, not an error.
            _ => None,
        };
        Self {
            last_seen_version,
            path,
        }
    }

    /// The version the user last saw, or `None` on a first-ever launch (no file).
    pub(crate) fn last_seen(&self) -> Option<&str> {
        self.last_seen_version.as_deref()
    }

    /// Record `version` as the last one seen, persisting only when it changed (so
    /// an unchanged launch does no disk write). Best-effort: a write failure is
    /// logged, never fatal.
    pub(crate) fn mark_seen(&mut self, version: &str) {
        if self.last_seen_version.as_deref() == Some(version) {
            return;
        }
        self.last_seen_version = Some(version.to_string());
        let Some(path) = self.path.clone() else {
            return;
        };
        if let Err(e) = save(&path, &self.last_seen_version) {
            tracing::warn!("failed to save app state: {e}");
        }
    }
}

/// Serialize the state to `path` via a temp file + rename, owner-only on Unix.
fn save(path: &PathBuf, last_seen_version: &Option<String>) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the config directory")?;
    }
    let file = StateFile {
        last_seen_version: last_seen_version.clone(),
    };
    let contents = serde_json::to_string_pretty(&file).context("serializing app state")?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).context("creating the state temp file")?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).context("renaming the state temp file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory store (no disk) so `mark_seen` exercises the change logic without
    /// touching the real config dir.
    fn in_memory() -> LocalState {
        LocalState {
            last_seen_version: None,
            path: None,
        }
    }

    #[test]
    fn fresh_state_has_no_last_seen() {
        assert_eq!(in_memory().last_seen(), None);
    }

    #[test]
    fn mark_seen_records_and_updates() {
        let mut s = in_memory();
        s.mark_seen("0.12.0");
        assert_eq!(s.last_seen(), Some("0.12.0"));
        s.mark_seen("0.13.0");
        assert_eq!(s.last_seen(), Some("0.13.0"));
    }

    #[test]
    fn round_trips_through_json() {
        let json = serde_json::to_string_pretty(&StateFile {
            last_seen_version: Some("1.2.3".into()),
        })
        .unwrap();
        let back: StateFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_seen_version.as_deref(), Some("1.2.3"));
    }

    /// An older/empty file (no `last_seen_version` key) loads as absent, not an
    /// error; the forward-compat guarantee of the wrapper shape.
    #[test]
    fn missing_field_loads_as_absent() {
        let back: StateFile = serde_json::from_str("{}").unwrap();
        assert_eq!(back.last_seen_version, None);
    }
}
