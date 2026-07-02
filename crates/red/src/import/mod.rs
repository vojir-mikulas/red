//! Import saved connections from other database tools (DBeaver, DBGate).
//!
//! Both tools store their connections in local, documented, locally-decryptable
//! formats, so importing is a pure parse + decrypt + map into RED's
//! [`ConnectionConfig`] — no external process, no network. This is Phase 1: the
//! parsers and credential decryptors behind [`ImportReport`], with **no UI and no
//! writes to RED's store** (Phase 2 commits a report; Phase 3 wires the UI).
//!
//! SECURITY: decrypted passwords live transiently in the returned
//! [`ImportedConnection`] configs (same exposure as any freshly-entered password);
//! the commit step (Phase 2) routes them into the OS keychain and strips them from
//! the config that reaches `connections.toml`, exactly like the connection form.
//!
//! **Nothing is imported silently.** Every source connection either lands in
//! [`ImportReport::imported`] or appears in [`ImportReport::skipped`] with a
//! human-readable reason (unsupported engine, decryption failure, …), so an import
//! can never quietly drop a connection.

// Phase 1 lands the parse/decrypt core ahead of the UI that consumes it; the
// public surface is exercised by this module's tests. Removed in Phase 3 when the
// welcome-screen / ⌘K entry points call `run`.
#![allow(dead_code)]

use std::path::Path;

use anyhow::Result;
use red_core::ConnectionConfig;

pub mod dbeaver;
pub mod dbgate;
pub mod discover;

/// A tool RED can import connections from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    DBeaver,
    DBGate,
}

impl ImportSource {
    /// Human label for the source picker.
    pub fn label(self) -> &'static str {
        match self {
            ImportSource::DBeaver => "DBeaver",
            ImportSource::DBGate => "DBGate",
        }
    }
}

/// One connection successfully parsed from a foreign tool, mapped onto RED's
/// model. `config.password` (and any SSH secret) is materialized here transiently;
/// the commit step moves it to the keychain. `warning` flags a partial import
/// (e.g. a credential that couldn't be decrypted) that still produced a usable
/// connection.
#[derive(Debug, Clone)]
pub struct ImportedConnection {
    pub config: ConnectionConfig,
    /// The label as it appeared in the source tool, for the preview list.
    pub source_name: String,
    /// The source tool's folder/group, if any. RED has no folder model yet; kept
    /// for the preview and possible name-prefixing, never silently dropped.
    pub folder: Option<String>,
    /// A non-fatal caveat surfaced in the preview (e.g. "password unavailable").
    pub warning: Option<String>,
}

/// The outcome of an import pass: what mapped cleanly and what was skipped, each
/// skip paired with a reason. A caller renders both halves — the skip list is a
/// feature, not an error channel.
#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub imported: Vec<ImportedConnection>,
    /// `(source connection name, reason)` for every connection not imported.
    pub skipped: Vec<(String, String)>,
}

impl ImportReport {
    /// Merge another report into this one (used when a tool exposes several source
    /// files — e.g. DBeaver's multiple workspace projects).
    pub fn extend(&mut self, other: ImportReport) {
        self.imported.extend(other.imported);
        self.skipped.extend(other.skipped);
    }
}

/// Parse and decrypt every connection under a source tool's data directory.
///
/// - `DBeaver`: `dir` is a `.dbeaver` folder (holding `data-sources.json` +
///   `credentials-config.json`).
/// - `DBGate`: `dir` is the `.dbgate` folder (holding `connections.jsonl` +
///   `.key`).
///
/// Returns a report; per-connection failures land in `skipped` rather than
/// aborting the whole import. Errors are reserved for "couldn't read the source at
/// all" (missing/unreadable primary file).
pub fn run(source: ImportSource, dir: &Path) -> Result<ImportReport> {
    match source {
        ImportSource::DBeaver => dbeaver::import(dir),
        ImportSource::DBGate => dbgate::import(dir),
    }
}
