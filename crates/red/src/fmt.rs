//! Small shared human-readable formatters (durations, byte sizes) used across the
//! Redis panels, the health report, and the analysis report. Consolidated here so
//! the same relative time and the same byte units are shown everywhere — the copies
//! that lived per-file had drifted (one printed `KB/MB`, another `KiB/MiB`).

/// A compact relative time from two epoch-second timestamps, terse form:
/// `"now"`, `"45s"`, `"3m"`, `"2h"`. For a dense column where the unit alone
/// reads clearly (pub/sub, keyspace event ages).
pub(crate) fn fmt_ago(now: i64, then: i64) -> String {
    let d = (now - then).max(0);
    if d < 1 {
        "now".to_string()
    } else if d < 60 {
        format!("{d}s")
    } else if d < 3_600 {
        format!("{}m", d / 60)
    } else {
        format!("{}h", d / 3_600)
    }
}

/// A relative time from a whole-seconds delta, sentence form:
/// `"just now"`, `"45s ago"`, `"3m ago"`, `"2h ago"`, `"5d ago"`. For a
/// standalone "as of …"/"last seen …" line (monitor, health, analysis report).
pub(crate) fn fmt_ago_secs(secs: i64) -> String {
    let d = secs.max(0);
    if d < 60 {
        "just now".to_string()
    } else if d < 3_600 {
        format!("{}m ago", d / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3_600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

/// A byte size in IEC units (`"512 B"`, `"1.5 KiB"`, `"3.2 MiB"`, `"1.1 GiB"`),
/// one decimal past the bytes tier. The single byte formatter for the whole app,
/// so no two views disagree on whether a kilobyte is 1000 or 1024.
pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_ago_is_terse() {
        assert_eq!(fmt_ago(100, 100), "now");
        assert_eq!(fmt_ago(145, 100), "45s");
        assert_eq!(fmt_ago(100 + 180, 100), "3m");
        assert_eq!(fmt_ago(100 + 7200, 100), "2h");
        // Never negative (a clock skew reads as "now").
        assert_eq!(fmt_ago(100, 200), "now");
    }

    #[test]
    fn fmt_ago_secs_is_a_sentence() {
        assert_eq!(fmt_ago_secs(10), "just now");
        assert_eq!(fmt_ago_secs(180), "3m ago");
        assert_eq!(fmt_ago_secs(7200), "2h ago");
        assert_eq!(fmt_ago_secs(2 * 86_400), "2d ago");
    }

    #[test]
    fn human_bytes_uses_iec_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(human_bytes(1024u64.pow(3)), "1.0 GiB");
    }
}
