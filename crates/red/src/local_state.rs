//! Small, app-managed local state that isn't a user *preference*; it lives apart
//! from `settings.toml` (which the user edits) in `<config>/red/state.json`.
//!
//! Today it holds a few facts: the last app version we showed the user, the last
//! set of session config selectors (model / reasoning / mode) each AI agent
//! advertised, and the last agent a chat was started on. The version drives the
//! one-shot "RED updated to X" toast (see `AppState::new`). The per-agent config
//! cache lets the assistant show the model/reasoning dropdowns *before* a chat
//! opens its session (the agent only advertises them once a session is live), so a
//! returning user can preselect a model without sending a message first. The last
//! agent is the new-chat default, so a fresh chat starts on whatever you last used.
//! The on-disk shape is a wrapper object so future app state can be added without
//! breaking older files.
//!
//! Persistence mirrors `history.rs`: a missing or corrupt file is simply empty
//! state (never blocks startup), and writes go through a temp file + rename,
//! owner-only on Unix.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// `<config>/red/state.json`.
fn state_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("state.json"))
}

/// One cached config selector (a serde mirror of `red_service::AiConfigOption`, which
/// carries no serde derives). Persisted so the composer can draw the model/reasoning
/// dropdowns before a chat has opened its live session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StoredConfigOption {
    pub id: String,
    pub name: String,
    /// The selector's category (`"model"` / `"reasoning"` / `"mode"` / `"other"`),
    /// stored as a lowercase string so a future category doesn't break older files.
    pub category: String,
    pub current_value: String,
    pub choices: Vec<StoredConfigChoice>,
}

/// One choice within a [`StoredConfigOption`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StoredConfigChoice {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The on-disk shape: a wrapper object (not a bare value) so new fields can be
/// added later without breaking older files.
#[derive(Default, Serialize, Deserialize)]
struct StateFile {
    /// The app version the user last saw, or absent on a first-ever launch.
    #[serde(default)]
    last_seen_version: Option<String>,
    /// The last config selectors each agent advertised, keyed by agent id. Empty
    /// until the first session of that agent has ever run.
    #[serde(default)]
    ai_config: HashMap<String, Vec<StoredConfigOption>>,
    /// The agent id a new chat should start on: the last one the user actually ran
    /// a chat on, so a fresh chat picks up where they left off (no settings
    /// detour). Absent until they've picked one.
    #[serde(default)]
    last_agent: Option<String>,
}

/// The app-state store. Loaded once at startup; mutations persist immediately.
pub(crate) struct LocalState {
    file: StateFile,
    path: Option<PathBuf>,
}

impl LocalState {
    /// Read state from disk, or start empty. Never fails: a missing file is empty
    /// state; a corrupt one is warned about and dropped (fail-open, like the other
    /// persisted-data loaders).
    pub(crate) fn load() -> Self {
        let path = state_path();
        let file = match path.as_ref().map(std::fs::read_to_string) {
            Some(Ok(contents)) => match serde_json::from_str::<StateFile>(&contents) {
                Ok(file) => file,
                Err(e) => {
                    tracing::warn!("ignoring corrupt app state: {e}");
                    StateFile::default()
                }
            },
            // Missing file or unreadable dir means empty state, not an error.
            _ => StateFile::default(),
        };
        Self { file, path }
    }

    /// The version the user last saw, or `None` on a first-ever launch (no file).
    pub(crate) fn last_seen(&self) -> Option<&str> {
        self.file.last_seen_version.as_deref()
    }

    /// Record `version` as the last one seen, persisting only when it changed (so
    /// an unchanged launch does no disk write). Best-effort: a write failure is
    /// logged, never fatal.
    pub(crate) fn mark_seen(&mut self, version: &str) {
        if self.file.last_seen_version.as_deref() == Some(version) {
            return;
        }
        self.file.last_seen_version = Some(version.to_string());
        self.persist();
    }

    /// The whole per-agent config cache, so the panel can seed its in-memory map on
    /// open without a lookup per agent.
    pub(crate) fn ai_config_all(&self) -> &HashMap<String, Vec<StoredConfigOption>> {
        &self.file.ai_config
    }

    /// The agent id a new chat should default to (the last one used), or `None`
    /// before the user has ever picked one.
    pub(crate) fn last_agent(&self) -> Option<&str> {
        self.file.last_agent.as_deref()
    }

    /// Record `agent` as the last one a chat was started on, persisting only when it
    /// changed (so re-selecting the same agent does no disk write).
    pub(crate) fn set_last_agent(&mut self, agent: &str) {
        if self.file.last_agent.as_deref() == Some(agent) {
            return;
        }
        self.file.last_agent = Some(agent.to_string());
        self.persist();
    }

    /// Cache `options` as `agent`'s last-advertised selectors, persisting only when
    /// they actually changed (so re-advertising an unchanged set does no disk write).
    pub(crate) fn set_ai_config(&mut self, agent: &str, options: Vec<StoredConfigOption>) {
        if self.file.ai_config.get(agent).map(Vec::as_slice) == Some(options.as_slice()) {
            return;
        }
        self.file.ai_config.insert(agent.to_string(), options);
        self.persist();
    }

    /// Serialize the whole state to disk. Best-effort: a write failure is logged,
    /// never fatal (local state is a convenience, not correctness).
    fn persist(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if let Err(e) = save(&path, &self.file) {
            tracing::warn!("failed to save app state: {e}");
        }
    }
}

/// Serialize the state to `path` via a temp file + rename, owner-only on Unix.
fn save(path: &PathBuf, file: &StateFile) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the config directory")?;
    }
    let contents = serde_json::to_string_pretty(file).context("serializing app state")?;
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

    /// In-memory store (no disk) so mutations exercise the change logic without
    /// touching the real config dir.
    fn in_memory() -> LocalState {
        LocalState {
            file: StateFile::default(),
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
    fn ai_config_round_trips_and_dedups() {
        let mut s = in_memory();
        assert!(s.ai_config_all().get("subscription").is_none());
        let opts = vec![StoredConfigOption {
            id: "model".into(),
            name: "Model".into(),
            category: "model".into(),
            current_value: "opus".into(),
            choices: vec![StoredConfigChoice {
                value: "opus".into(),
                name: "Opus".into(),
                description: None,
            }],
        }];
        s.set_ai_config("subscription", opts.clone());
        assert_eq!(s.ai_config_all().get("subscription"), Some(&opts));
        // A different agent is cached independently.
        assert!(s.ai_config_all().get("codex").is_none());
    }

    #[test]
    fn round_trips_through_json() {
        let json = serde_json::to_string_pretty(&StateFile {
            last_seen_version: Some("1.2.3".into()),
            ai_config: HashMap::new(),
            last_agent: Some("codex".into()),
        })
        .unwrap();
        let back: StateFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_seen_version.as_deref(), Some("1.2.3"));
        assert_eq!(back.last_agent.as_deref(), Some("codex"));
    }

    #[test]
    fn last_agent_records_and_updates() {
        let mut s = in_memory();
        assert_eq!(s.last_agent(), None);
        s.set_last_agent("subscription");
        assert_eq!(s.last_agent(), Some("subscription"));
        s.set_last_agent("codex");
        assert_eq!(s.last_agent(), Some("codex"));
    }

    /// An older/empty file (no keys) loads as absent, not an error; the
    /// forward-compat guarantee of the wrapper shape.
    #[test]
    fn missing_field_loads_as_absent() {
        let back: StateFile = serde_json::from_str("{}").unwrap();
        assert_eq!(back.last_seen_version, None);
        assert!(back.ai_config.is_empty());
        assert_eq!(back.last_agent, None);
    }
}
