//! The AI write-audit log (Feature B). Every data-modifying statement the
//! assistant executes — already gated by tier, the read-only check, the
//! destructive-shape blocklist, and explicit per-call user approval — is also
//! appended here, so there's a durable, after-the-fact record of exactly what the
//! agent changed and when. It's a trust/forensics aid, not a control: the gates do
//! the gating; this just remembers.
//!
//! Tab-separated, append-only, one line per executed write:
//! `<unix_seconds>\t<rows_affected>\t<sql>` (newlines in the SQL flattened to
//! spaces so each write stays one grep-able line). Best-effort — a logging failure
//! never blocks or fails the write.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// `<config>/red/ai-writes.log` — the audit file, beside `settings.toml` and the
/// conversations. `None` if no config dir resolves on this platform.
fn log_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("ai-writes.log"))
}

/// Append one executed write to the audit log. Best-effort: any failure is logged
/// at `warn` and swallowed — the write already happened and the user approved it.
pub(crate) fn record_write(sql: &str, affected: u64) {
    let Some(path) = log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Flatten newlines/tabs so one write is one line with stable columns.
    let flat = sql.replace(['\n', '\r', '\t'], " ");
    let line = format!("{now}\t{affected}\t{flat}\n");
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut file) => {
            if let Err(e) = file.write_all(line.as_bytes()) {
                tracing::warn!("failed to append to AI write-audit log: {e}");
            }
        }
        Err(e) => tracing::warn!("failed to open AI write-audit log: {e}"),
    }
}
