//! The bundled "Sample database" preview.
//!
//! A tiny read-only SQLite database ships with Red so a first-time user can open
//! the app and immediately browse real data — schema, joins, a view, filtering —
//! without configuring a connection first. The database bytes are embedded at
//! build time and written out to the app data directory on the very first launch,
//! then seeded as a normal (read-only) saved connection so the existing welcome
//! card and connect path handle the rest.

use std::path::PathBuf;

use red_core::{ConnectionConfig, DbKind};

use crate::config::{self, StoredConnection};

/// The sample database, embedded at build time. Regenerate the binary with
/// `sqlite3 sample/sample.db < sample/sample.sql` (the schema + seed live there).
const SAMPLE_DB: &[u8] = include_bytes!("../../../sample/sample.db");

/// Display name of the seeded connection.
const SAMPLE_NAME: &str = "Sample database";

/// Write the embedded database to `<data_dir>/red/sample.db` if it isn't already
/// there, returning its path. `None` when the platform has no data directory or
/// the write fails — the caller then simply skips the preview.
fn materialize() -> Option<PathBuf> {
    let dir = dirs::data_dir()?.join("red");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("could not create data dir for the sample database: {e}");
        return None;
    }
    let path = dir.join("sample.db");
    if !path.exists() {
        if let Err(e) = std::fs::write(&path, SAMPLE_DB) {
            tracing::warn!("could not write the sample database: {e}");
            return None;
        }
    }
    Some(path)
}

/// Build the read-only "Sample database" connection seeded on a first run.
/// Materializes the database file and returns a [`StoredConnection`] pointing at
/// it, or `None` if the file could not be written.
pub(crate) fn first_run_connection() -> Option<StoredConnection> {
    let path = materialize()?;
    Some(StoredConnection {
        id: config::new_id(),
        config: ConnectionConfig {
            name: SAMPLE_NAME.into(),
            kind: DbKind::Sqlite,
            database: path.to_string_lossy().into_owned(),
            // Read-only: the preview is meant to be browsed, not mutated, and it
            // shows off the read-only safety badge on the welcome card.
            read_only: true,
            color: 3,
            ..Default::default()
        },
        last_accessed: None,
        pinned: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_db_is_a_valid_sqlite_file() {
        // The SQLite file format always starts with this 16-byte magic string.
        assert!(
            SAMPLE_DB.starts_with(b"SQLite format 3\0"),
            "embedded sample.db is not a SQLite database"
        );
        // A non-trivial database — guards against an accidentally-empty fixture.
        assert!(
            SAMPLE_DB.len() > 16 * 1024,
            "sample.db looks suspiciously small"
        );
    }
}
