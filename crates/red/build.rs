//! Build provenance baked into the binary at compile time. Emits two env vars,
//! `RED_GIT_SHA` (short commit) and `RED_BUILD_DATE` (UTC `YYYY-MM-DD`), that the
//! About tab surfaces so a build is unambiguous in bug reports and update checks
//! (Phase 2 of docs/plans/self-update.md). Best-effort: a source archive with no
//! git degrades the SHA to "unknown" rather than failing the build.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

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

    // Computed from the system clock rather than shelling `date` so the stamp is
    // identical on every OS (Windows has no `date -u`), and dependency-free (no
    // chrono just for a build stamp).
    println!("cargo:rustc-env=RED_BUILD_DATE={}", build_date_utc());

    // Re-run when HEAD moves so the SHA tracks the working commit. Best-effort;
    // a missing path (e.g. a git worktree or tarball) just means "always re-run".
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}

/// Today's UTC date as `YYYY-MM-DD`. Converts the Unix day count to a civil date
/// with Howard Hinnant's `days_from_civil` inverse, exact for any date and free
/// of leap-year edge cases. Falls back to "unknown" only if the clock predates
/// the epoch (unreachable in practice).
fn build_date_utc() -> String {
    let Ok(dur) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return "unknown".into();
    };
    let days = (dur.as_secs() / 86_400) as i64;

    // civil_from_days: shift the epoch to 0000-03-01 so leap days land at the end
    // of the 400-year era, then peel off era / year-of-era / day-of-year.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}")
}
