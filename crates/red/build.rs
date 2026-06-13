//! Build provenance baked into the binary at compile time. Emits two env vars —
//! `RED_GIT_SHA` (short commit) and `RED_BUILD_DATE` (UTC `YYYY-MM-DD`) — that the
//! About tab surfaces so a build is unambiguous in bug reports and update checks
//! (Phase 2 of docs/plans/self-update.md). Best-effort: a source archive with no
//! git, or a missing `git`/`date`, degrades to "unknown" rather than failing the
//! build.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=RED_GIT_SHA={sha}");

    // `date -u` keeps this dependency-free (no chrono just for a build stamp).
    let date = Command::new("date")
        .args(["-u", "+%Y-%m-%d"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=RED_BUILD_DATE={date}");

    // Re-run when HEAD moves so the SHA tracks the working commit. Best-effort —
    // a missing path (e.g. a git worktree or tarball) just means "always re-run".
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
