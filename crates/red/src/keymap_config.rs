//! On-disk keymap overrides — the user's `keymap.toml`.
//!
//! Keybindings ship as code-defined defaults (see [`crate::keymap`]); this module
//! is the seam that lets a user *override* them from a hand-edited file beside
//! `settings.toml`. The file is a list of context-scoped blocks, each mapping a
//! keystroke to an action name — Zed's keymap shape, in TOML:
//!
//! ```toml
//! [[keymap]]
//! context = "RedRoot"          # omit for a true global
//! bindings = { "cmd-l" = "ToggleFilter", "cmd-shift-f" = "unbind" }
//! ```
//!
//! Reads **never** fail: a missing file means "no overrides", and a malformed file
//! degrades to no overrides plus a warning — a bad keymap must never strip the
//! defaults or block launch. Parsing here is only TOML-shape validation; whether
//! each keystroke and action name is *valid* is decided when the blocks are turned
//! into bindings in [`crate::keymap::apply`], which collects its own warnings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// One context-scoped block of overrides. `context = None` binds a true global
/// (fires from any focus); otherwise the bindings are scoped to that key-context.
/// `bindings` maps a keystroke string to an action name (or the reserved
/// `"unbind"` / `"none"`, which removes the default for that keystroke).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct KeymapBlock {
    // Omit `context` when writing a true global, so a serialized block reads the
    // same way a hand-written one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// `BTreeMap` so the order bindings are applied is deterministic (a map can't
    /// hold a duplicate keystroke, so intra-block precedence never matters).
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

/// The file shape: a top-level `keymap` array of blocks.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct KeymapFile {
    #[serde(default)]
    keymap: Vec<KeymapBlock>,
}

/// The outcome of a load: the parsed override blocks plus any non-fatal warning to
/// surface (only ever a single TOML-parse error here; per-binding problems are
/// reported later, when the blocks are compiled into bindings).
#[derive(Debug, Clone, Default)]
pub struct KeymapLoadReport {
    pub blocks: Vec<KeymapBlock>,
    pub warnings: Vec<String>,
}

/// Local on-disk store over a single `keymap.toml`, beside `settings.toml`.
#[derive(Debug, Clone)]
pub struct KeymapStore {
    path: PathBuf,
}

impl KeymapStore {
    /// Open the store at `<config_dir>/red/keymap.toml`. `None` when the platform
    /// has no config dir.
    pub fn open_default() -> Option<Self> {
        let path = dirs::config_dir()?.join("red").join("keymap.toml");
        Some(Self { path })
    }

    /// The backing file path, for the "open keymap file" workflow and the watcher.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the overrides. A missing file is the common case (no overrides); a
    /// malformed file keeps the defaults and reports the parse error.
    pub fn load_report(&self) -> KeymapLoadReport {
        let Ok(contents) = std::fs::read_to_string(&self.path) else {
            return KeymapLoadReport::default();
        };
        match toml::from_str::<KeymapFile>(&contents) {
            Ok(file) => KeymapLoadReport {
                blocks: file.keymap,
                warnings: Vec::new(),
            },
            Err(e) => KeymapLoadReport {
                blocks: Vec::new(),
                warnings: vec![format!(
                    "keymap.toml isn't valid TOML ({e}) — keeping the default keybindings"
                )],
            },
        }
    }

    /// Serialize override blocks to the `keymap.toml` text the editor will write.
    /// An empty override set serializes to an empty file (a reset-to-defaults), not
    /// a bare `keymap = []`, so the file stays clean. Shared with the save path so
    /// the watcher can be told the exact bytes about to land (self-write suppress).
    pub fn serialize(blocks: &[KeymapBlock]) -> Result<String> {
        if blocks.is_empty() {
            return Ok(String::new());
        }
        let file = KeymapFile {
            keymap: blocks.to_vec(),
        };
        toml::to_string_pretty(&file).context("serializing the keymap")
    }

    /// Write override blocks atomically — a sibling temp file, flushed, then
    /// renamed over `keymap.toml` so a crash can't leave a partial file. Mirrors
    /// [`crate::settings::FileSettingsStore::save`].
    pub fn save(&self, blocks: &[KeymapBlock]) -> Result<()> {
        use std::io::Write;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).context("creating the config directory")?;
        }
        let serialized = Self::serialize(blocks)?;

        let tmp = self
            .path
            .with_extension(format!("toml.tmp.{}", std::process::id()));
        let mut file = std::fs::File::create(&tmp).context("creating the keymap temp file")?;
        file.write_all(serialized.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &self.path).context("renaming the keymap temp file")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_parses_to_no_blocks() {
        let file: KeymapFile = toml::from_str("").expect("empty is valid");
        assert!(file.keymap.is_empty());
    }

    #[test]
    fn parses_blocks_with_and_without_context() {
        let file: KeymapFile = toml::from_str(
            r#"
            [[keymap]]
            context = "RedRoot"
            bindings = { "cmd-l" = "ToggleFilter" }

            [[keymap]]
            bindings = { "cmd-shift-p" = "SwitchConnection", "cmd-x" = "unbind" }
            "#,
        )
        .expect("valid keymap");
        assert_eq!(file.keymap.len(), 2);
        assert_eq!(file.keymap[0].context.as_deref(), Some("RedRoot"));
        assert_eq!(
            file.keymap[0].bindings.get("cmd-l").map(String::as_str),
            Some("ToggleFilter")
        );
        assert_eq!(file.keymap[1].context, None);
        assert_eq!(
            file.keymap[1].bindings.get("cmd-x").map(String::as_str),
            Some("unbind")
        );
    }

    #[test]
    fn serialize_round_trips_through_load() {
        let blocks = vec![
            KeymapBlock {
                context: None,
                bindings: [("cmd-shift-k".to_string(), "SwitchConnection".to_string())]
                    .into_iter()
                    .collect(),
            },
            KeymapBlock {
                context: Some("RedRoot".to_string()),
                bindings: [
                    ("cmd-l".to_string(), "ToggleFilter".to_string()),
                    ("cmd-shift-f".to_string(), "unbind".to_string()),
                ]
                .into_iter()
                .collect(),
            },
        ];
        let text = KeymapStore::serialize(&blocks).expect("serialize");
        let parsed: KeymapFile = toml::from_str(&text).expect("re-parse");
        assert_eq!(parsed.keymap, blocks);
    }

    #[test]
    fn empty_blocks_serialize_to_empty_file() {
        assert_eq!(KeymapStore::serialize(&[]).expect("serialize"), "");
    }
}
