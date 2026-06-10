// SPDX-License-Identifier: GPL-3.0-or-later

//! Persisted UI preferences — theme, result-grid density, and the
//! destructive-statement safety rail.
//!
//! These are app-wide presentation settings, not per-connection data, so they
//! live in their own `settings.toml` beside `connections.toml` in the platform
//! config dir. Values are stored as provider-agnostic primitives (a theme
//! *name*, a density *index*) and mapped to concrete UI types in the app.
//!
//! Mirrors Nyx's settings store: writes go through a temp-file + atomic rename,
//! but reads **never** fail — a missing or malformed file degrades to
//! [`Settings::default`], because preferences are convenience, not user data,
//! and a bad file must never block launch.

use std::path::PathBuf;

use anyhow::{Context, Result};
use gpui::{px, Pixels};
use serde::{Deserialize, Serialize};

/// Persisted UI preferences.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The active theme's human-readable name (e.g. `"One Dark"`); mapped to a
    /// concrete `Theme` via [`crate::theme::by_name`].
    pub theme: String,
    /// Result-grid row density as an index into [`Density::ALL`] (0/1/2).
    pub density: u8,
    /// Whether destructive statements (DROP/TRUNCATE/…) prompt for confirmation
    /// before they run — RED's read-mostly safety rail.
    pub confirm_destructive: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "One Dark".to_string(),
            density: 1,
            confirm_destructive: true,
        }
    }
}

impl Settings {
    /// The configured row density, clamped to a valid variant.
    pub fn density(&self) -> Density {
        Density::ALL[(self.density as usize).min(Density::ALL.len() - 1)]
    }
}

/// Result-grid row spacing. Stored as the index of its position in [`Self::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    Compact,
    Comfortable,
    Spacious,
}

impl Density {
    pub const ALL: [Density; 3] = [Density::Compact, Density::Comfortable, Density::Spacious];

    /// Index into [`Self::ALL`] — what gets persisted and drives the segmented control.
    pub fn index(self) -> usize {
        match self {
            Density::Compact => 0,
            Density::Comfortable => 1,
            Density::Spacious => 2,
        }
    }

    /// The grid row height for this density.
    pub fn row_height(self) -> Pixels {
        match self {
            Density::Compact => px(22.),
            Density::Comfortable => px(25.),
            Density::Spacious => px(30.),
        }
    }
}

/// Local on-disk settings store over a single `settings.toml`.
///
/// Reads never fail (missing/malformed → [`Settings::default`]); writes are
/// atomic (temp file + rename on the same volume).
#[derive(Debug, Clone)]
pub struct FileSettingsStore {
    path: PathBuf,
}

impl FileSettingsStore {
    /// Open the store at `<config_dir>/red/settings.toml`, beside the connection
    /// list. Returns `None` when the platform has no config dir.
    pub fn open_default() -> Option<Self> {
        let path = dirs::config_dir()?.join("red").join("settings.toml");
        Some(Self { path })
    }

    /// Open a store backed by an explicit file path (used in tests).
    #[cfg(test)]
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Read the settings, falling back to [`Settings::default`] for a missing or
    /// malformed file (preferences must never block startup).
    pub fn load(&self) -> Settings {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }

    /// Serialize and write atomically: a sibling temp file, flushed, then renamed
    /// over the target so a crash can't leave a partial file.
    pub fn save(&self, settings: &Settings) -> Result<()> {
        use std::io::Write;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).context("creating the config directory")?;
        }
        let serialized = toml::to_string_pretty(settings)?;

        let tmp = self
            .path
            .with_extension(format!("toml.tmp.{}", std::process::id()));
        let mut file = std::fs::File::create(&tmp).context("creating the settings temp file")?;
        file.write_all(serialized.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &self.path).context("renaming the settings temp file")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway store under a unique temp dir, dropped via [`TempStore`] which
    /// cleans up the directory on `Drop` (no `tempfile` dependency in the tree).
    struct TempStore {
        dir: PathBuf,
        store: FileSettingsStore,
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn temp_store() -> TempStore {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("red-settings-test-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = FileSettingsStore::with_path(dir.join("settings.toml"));
        TempStore { dir, store }
    }

    #[test]
    fn missing_file_is_default() {
        let t = temp_store();
        assert_eq!(t.store.load(), Settings::default());
    }

    #[test]
    fn round_trip() {
        let t = temp_store();
        let settings = Settings {
            theme: "GitHub Dark".to_string(),
            density: 0,
            confirm_destructive: false,
        };
        t.store.save(&settings).unwrap();
        assert_eq!(t.store.load(), settings);
    }

    #[test]
    fn malformed_file_is_default() {
        let t = temp_store();
        std::fs::write(t.store.path.as_path(), "this is = not valid toml ][").unwrap();
        assert_eq!(t.store.load(), Settings::default());
    }

    #[test]
    fn partial_file_takes_field_defaults() {
        // A file with only `theme` set keeps the default density / confirm flag.
        let t = temp_store();
        std::fs::write(t.store.path.as_path(), "theme = \"GitHub Dark\"\n").unwrap();
        let loaded = t.store.load();
        assert_eq!(loaded.theme, "GitHub Dark");
        assert_eq!(loaded.density, Settings::default().density);
        assert_eq!(
            loaded.confirm_destructive,
            Settings::default().confirm_destructive
        );
    }
}
