//! Small, app-managed local state that isn't a user *preference*; it lives apart
//! from `settings.toml` (which the user edits) in `<config>/red/state.json`.
//!
//! Today it holds a few facts: the last app version we showed the user, the last
//! set of session config selectors (model / reasoning / mode) each AI agent
//! advertised, and the last agent a chat was started on. The version drives the
//! one-shot "RED updated to X" toast (see `AppState::new`). The per-agent config
//! cache lets the assistant show the model/reasoning dropdowns *before* a chat
//! opens its session (the agent only advertises them once a session is live), so a
//! returning user can preselect a model without sending a message first, and the
//! switches (fast mode) they explicitly flipped are re-applied to each new session
//! so a pick made before the first message isn't lost. The last agent is the
//! new-chat default, so a fresh chat starts on whatever you last used.
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
    /// An on/off switch rather than a dropdown. Defaulted so files written before
    /// switches existed still load (as selectors, which is what they were).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub boolean: bool,
}

/// One choice within a [`StoredConfigOption`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StoredConfigChoice {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// How the welcome screen's saved-connection list was last arranged: the sort
/// key and direction plus the engine / environment facets.
///
/// Every field is a *string* key rather than the domain enum. A facet written by
/// a newer build then degrades to "not selected" on load (see
/// `ConnectFilter::from_keys`) instead of failing the whole file and taking the
/// unrelated state down with it.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StoredConnectView {
    /// `ConnectSortField::key`: `"name"` or `"recent"`.
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub ascending: bool,
    /// Engine keys (`DbKind::url_scheme`).
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Environment keys (`app::env_key`).
    #[serde(default)]
    pub envs: Vec<String>,
}

/// One restored query tab: what the user typed and how the tab was arranged.
///
/// The *result* is deliberately absent. Rows are the database's to give, not
/// ours to cache: re-running on restore would fire a write-shaped statement at a
/// server the user has not looked at yet, and persisting rows would put query
/// output on disk outside the export path. A restored browse re-opens (a plain
/// `SELECT`, which the browse path already grades as safe); a restored editor tab
/// comes back with its SQL and an empty grid, waiting for ⌘↵.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StoredTab {
    pub title: String,
    /// The editor buffer verbatim, which is the part that is otherwise lost.
    #[serde(default)]
    pub sql: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// The pane this tab belonged to, as the raw [`PaneId`](crate::panes::PaneId)
    /// index. Meaningless without [`StoredWorkspace::layout`], and defaulted to
    /// the first pane so a tab whose pane vanished still lands somewhere.
    #[serde(default)]
    pub pane: u32,
    /// `(schema, table)` when the tab was a table browse rather than editor SQL,
    /// so it re-opens as a browse (with its FK affordances and keyset paging)
    /// rather than as a tab holding the equivalent `SELECT` text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browse: Option<(String, String)>,
}

/// One connection's restored workspace.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StoredWorkspace {
    pub tabs: Vec<StoredTab>,
    /// Which tab had focus, as an index into `tabs`. Out-of-range (a file written
    /// by a build that stored more tabs) simply focuses the first.
    #[serde(default)]
    pub active: usize,
    /// The pane geometry, absent when the workspace was a single unsplit pane
    /// (the overwhelming case, and the one that costs nothing to rebuild).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<StoredLayout>,
}

/// A persisted pane tree: the geometry plus which pane held focus.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StoredLayout {
    pub root: StoredNode,
    pub focus: u32,
    /// The id counter, carried so restored panes keep their ids and a later
    /// split cannot mint an id a restored tab already claims.
    pub next: u32,
}

/// A node of the persisted pane tree, mirroring `panes::Node`.
///
/// Untagged-free (serde's default externally-tagged form) so an unreadable
/// variant fails this field alone and drops the layout, leaving the tabs to
/// restore into one pane rather than taking the whole workspace down.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum StoredNode {
    Leaf(u32),
    Split {
        /// `true` stacks children as rows, matching `SplitAxis::Vertical`.
        vertical: bool,
        children: Vec<StoredChild>,
    },
}

/// One slot of a persisted split: its fraction and what sits in it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StoredChild {
    /// Serialized as its bit pattern: `f32` has no `Eq`, and the weights are
    /// only ever round-tripped, never compared.
    pub weight_bits: u32,
    pub node: StoredNode,
}

impl StoredChild {
    pub(crate) fn new(weight: f32, node: StoredNode) -> Self {
        Self {
            weight_bits: weight.to_bits(),
            node,
        }
    }

    /// The slot's fraction. A non-finite or non-positive weight (a corrupt file)
    /// reads as an equal share, which `normalize` then rebalances, rather than
    /// as a pane with zero or NaN width that the renderer cannot lay out.
    pub(crate) fn weight(&self) -> f32 {
        let w = f32::from_bits(self.weight_bits);
        if w.is_finite() && w > 0.0 { w } else { 0.5 }
    }
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
    /// The on/off config switches (fast mode) the user has *explicitly* set, keyed
    /// by agent id then option id. Only an explicit flip is recorded: an untouched
    /// switch is left to the agent's own memory rather than re-asserted from here.
    #[serde(default)]
    ai_switches: HashMap<String, HashMap<String, bool>>,
    /// How the welcome screen's connection list was last sorted and filtered, so
    /// a roster opens the way it was left. Absent until the user changes either.
    #[serde(default)]
    connect_view: Option<StoredConnectView>,
    /// Each connection's last workspace, keyed by `conn_id`. Per connection
    /// rather than one global workspace because RED parks several sessions at
    /// once and ⌘P flips between them: restoring one connection's tabs onto
    /// another would be worse than restoring nothing.
    #[serde(default)]
    workspaces: HashMap<String, StoredWorkspace>,
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

    /// The switches the user has explicitly set for `agent`, or an empty map when
    /// they've never touched one. Applied to each fresh session of that agent.
    pub(crate) fn ai_switches(&self, agent: &str) -> Option<&HashMap<String, bool>> {
        self.file.ai_switches.get(agent)
    }

    /// Record an explicit switch flip, persisting only when it changed.
    pub(crate) fn set_ai_switch(&mut self, agent: &str, config_id: &str, on: bool) {
        let switches = self.file.ai_switches.entry(agent.to_string()).or_default();
        if switches.get(config_id) == Some(&on) {
            return;
        }
        switches.insert(config_id.to_string(), on);
        self.persist();
    }

    /// How the welcome screen's connection list was last arranged, or `None`
    /// before the user has touched the sort or the filters.
    pub(crate) fn connect_view(&self) -> Option<&StoredConnectView> {
        self.file.connect_view.as_ref()
    }

    /// Record the welcome screen's list arrangement, persisting only when it
    /// changed (so re-selecting the active sort does no disk write).
    pub(crate) fn set_connect_view(&mut self, view: StoredConnectView) {
        if self.file.connect_view.as_ref() == Some(&view) {
            return;
        }
        self.file.connect_view = Some(view);
        self.persist();
    }

    /// The workspace last saved for `conn_id`, or `None` when that connection has
    /// never been open (or its workspace held nothing worth restoring).
    pub(crate) fn workspace(&self, conn_id: &str) -> Option<&StoredWorkspace> {
        self.file.workspaces.get(conn_id)
    }

    /// Record `conn_id`'s workspace, persisting only when it actually changed.
    ///
    /// The equality check is what makes it safe to call this on every tab event
    /// and on a debounced editor tick: a workspace that has not moved writes
    /// nothing, so an idle app does no disk I/O.
    pub(crate) fn set_workspace(&mut self, conn_id: &str, workspace: StoredWorkspace) {
        // An empty workspace is a removal, not a stored emptiness: otherwise
        // closing every tab would persist a blank workspace that then "restores"
        // over the tabs a later session opened before quitting uncleanly.
        if workspace.tabs.is_empty() {
            if self.file.workspaces.remove(conn_id).is_some() {
                self.persist();
            }
            return;
        }
        if self.file.workspaces.get(conn_id) == Some(&workspace) {
            return;
        }
        self.file.workspaces.insert(conn_id.to_string(), workspace);
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
            boolean: false,
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
            ai_switches: HashMap::new(),
            connect_view: Some(StoredConnectView {
                sort: "name".into(),
                ascending: true,
                kinds: vec!["postgres".into()],
                envs: vec!["prod".into()],
            }),
            workspaces: HashMap::from([(
                "conn-1".to_string(),
                StoredWorkspace {
                    tabs: vec![StoredTab {
                        title: "query 1".into(),
                        sql: "SELECT 1".into(),
                        pinned: true,
                        namespace: Some("public".into()),
                        pane: 2,
                        browse: Some(("public".into(), "users".into())),
                    }],
                    active: 0,
                    layout: Some(StoredLayout {
                        root: StoredNode::Split {
                            vertical: false,
                            children: vec![
                                StoredChild::new(0.5, StoredNode::Leaf(0)),
                                StoredChild::new(0.5, StoredNode::Leaf(2)),
                            ],
                        },
                        focus: 2,
                        next: 3,
                    }),
                },
            )]),
        })
        .unwrap();
        let back: StateFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_seen_version.as_deref(), Some("1.2.3"));
        let ws = back
            .workspaces
            .get("conn-1")
            .expect("workspace round-trips");
        assert_eq!(ws.tabs[0].sql, "SELECT 1");
        assert_eq!(ws.tabs[0].pane, 2);
        assert_eq!(
            ws.tabs[0]
                .browse
                .as_ref()
                .map(|(s, t)| (s.as_str(), t.as_str())),
            Some(("public", "users"))
        );
        assert_eq!(ws.layout.as_ref().map(|l| l.focus), Some(2));
        assert_eq!(back.last_agent.as_deref(), Some("codex"));
        let view = back.connect_view.expect("the connect view round-trips");
        assert_eq!(view.sort, "name");
        assert_eq!(view.kinds, vec!["postgres".to_string()]);
    }

    #[test]
    fn connect_view_records_and_updates() {
        let mut s = in_memory();
        assert!(s.connect_view().is_none());
        let view = StoredConnectView {
            sort: "recent".into(),
            ascending: false,
            kinds: vec!["redis".into()],
            envs: Vec::new(),
        };
        s.set_connect_view(view.clone());
        assert_eq!(s.connect_view(), Some(&view));
        s.set_connect_view(StoredConnectView {
            sort: "name".into(),
            ..view
        });
        assert_eq!(s.connect_view().map(|v| v.sort.as_str()), Some("name"));
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
        assert!(back.ai_switches.is_empty());
        assert_eq!(back.connect_view, None);
    }

    #[test]
    fn ai_switches_record_per_agent() {
        let mut s = in_memory();
        assert!(s.ai_switches("subscription").is_none());
        s.set_ai_switch("subscription", "fast-mode", true);
        assert_eq!(
            s.ai_switches("subscription")
                .and_then(|m| m.get("fast-mode")),
            Some(&true)
        );
        s.set_ai_switch("subscription", "fast-mode", false);
        assert_eq!(
            s.ai_switches("subscription")
                .and_then(|m| m.get("fast-mode")),
            Some(&false)
        );
        // Another agent's switches are its own.
        assert!(s.ai_switches("codex").is_none());
    }
}
