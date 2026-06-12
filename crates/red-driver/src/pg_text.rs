//! Dependency-free decoders from PostgreSQL's **binary** wire format to display
//! text, for the types `tokio-postgres` won't decode without an optional crate
//! (chrono/uuid/serde_json/rust_decimal). Without these a `numeric`, `timestamp`,
//! `date`, `time`, `uuid`, or `json(b)` cell decode-fails and surfaces as a silent
//! NULL — the "wait, where's my data?" bug. We render the wire bytes ourselves
//! instead. Every function is pure (bytes in, `String` out), so the formats are
//! unit-tested without a live server.

/// Microseconds per day — the unit of PostgreSQL's binary `timestamp`/`time`.
const USECS_PER_DAY: i64 = 86_400_000_000;
/// Days between the Unix epoch (1970-01-01) and the PostgreSQL epoch (2000-01-01),
/// which is the zero point of the binary `timestamp`/`date` encodings.
const PG_EPOCH_UNIX_DAYS: i64 = 10_957;

/// `numeric` (variable-length, base-10000) → its canonical decimal string, or
/// `None` if the buffer is malformed. Handles the special `NaN`/`Infinity` sigils.
pub(crate) fn numeric_to_string(raw: &[u8]) -> Option<String> {
    if raw.len() < 8 {
        return None;
    }
    let be16 = |o: usize| i16::from_be_bytes([raw[o], raw[o + 1]]);
    let ndigits = be16(0).max(0) as usize;
    let weight = be16(2) as i32;
    let sign = u16::from_be_bytes([raw[4], raw[5]]);
    let dscale = be16(6).max(0) as usize;

    match sign {
        0xC000 => return Some("NaN".to_string()),
        0xD000 => return Some("Infinity".to_string()),
        0xF000 => return Some("-Infinity".to_string()),
        _ => {}
    }
    if raw.len() < 8 + ndigits * 2 {
        return None;
    }
    // Each base-10000 group at index `i` carries the exponent `weight - i`.
    let digit = |i: usize| -> i32 {
        let o = 8 + i * 2;
        i16::from_be_bytes([raw[o], raw[o + 1]]) as i32
    };

    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    // Integer part: groups with a non-negative exponent (`0..=weight`). The most
    // significant group is printed bare; the rest are zero-padded to four digits.
    if weight < 0 {
        s.push('0');
    } else {
        for i in 0..=weight {
            let g = if (i as usize) < ndigits {
                digit(i as usize)
            } else {
                0
            };
            if i == 0 {
                s.push_str(&g.to_string());
            } else {
                s.push_str(&format!("{g:04}"));
            }
        }
    }
    // Fractional part: `dscale` decimal places, from the groups after `weight`
    // (leading groups before the first stored digit pad with zeros).
    if dscale > 0 {
        s.push('.');
        let mut frac = String::new();
        let mut i = weight + 1;
        while frac.len() < dscale {
            let g = if i >= 0 && (i as usize) < ndigits {
                digit(i as usize)
            } else {
                0
            };
            frac.push_str(&format!("{g:04}"));
            i += 1;
        }
        frac.truncate(dscale);
        s.push_str(&frac);
    }
    Some(s)
}

/// `timestamp` micros-since-2000 → `YYYY-MM-DD HH:MM:SS[.ffffff]`.
pub(crate) fn timestamp_to_string(micros: i64) -> String {
    let days = micros.div_euclid(USECS_PER_DAY);
    let rem = micros.rem_euclid(USECS_PER_DAY);
    let (y, m, d) = civil_from_days(days + PG_EPOCH_UNIX_DAYS);
    format!("{} {}", ymd(y, m, d), hms(rem))
}

/// `timestamptz` is stored as UTC micros-since-2000; render with an explicit
/// `+00` so a reader never mistakes it for a local wall-clock time.
pub(crate) fn timestamptz_to_string(micros: i64) -> String {
    format!("{}+00", timestamp_to_string(micros))
}

/// `date` days-since-2000 → `YYYY-MM-DD`.
pub(crate) fn date_to_string(days: i32) -> String {
    let (y, m, d) = civil_from_days(days as i64 + PG_EPOCH_UNIX_DAYS);
    ymd(y, m, d)
}

/// `time` micros-since-midnight → `HH:MM:SS[.ffffff]`.
pub(crate) fn time_to_string(micros: i64) -> String {
    hms(micros)
}

/// `timetz` (8-byte micros + 4-byte zone offset west of UTC, in seconds) →
/// `HH:MM:SS[.ffffff]±HH:MM`, or `None` if the buffer is the wrong length.
pub(crate) fn timetz_to_string(raw: &[u8]) -> Option<String> {
    if raw.len() != 12 {
        return None;
    }
    let micros = i64::from_be_bytes(raw[0..8].try_into().ok()?);
    let zone_west = i32::from_be_bytes(raw[8..12].try_into().ok()?);
    Some(format!("{}{}", hms(micros), zone(zone_west)))
}

/// `uuid` (16 raw bytes) → the canonical `8-4-4-4-12` hyphenated hex form, or
/// `None` if the buffer is the wrong length.
pub(crate) fn uuid_to_string(raw: &[u8]) -> Option<String> {
    if raw.len() != 16 {
        return None;
    }
    let hex = |r: &[u8]| -> String { r.iter().map(|b| format!("{b:02x}")).collect() };
    Some(format!(
        "{}-{}-{}-{}-{}",
        hex(&raw[0..4]),
        hex(&raw[4..6]),
        hex(&raw[6..8]),
        hex(&raw[8..10]),
        hex(&raw[10..16]),
    ))
}

/// `(year, month, day)` from a day count relative to the Unix epoch. Howard
/// Hinnant's `civil_from_days` — exact across the proleptic Gregorian calendar
/// with no lookup table.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d)
}

fn ymd(year: i64, month: u32, day: u32) -> String {
    format!("{year:04}-{month:02}-{day:02}")
}

/// `HH:MM:SS`, with a trailing-zero-trimmed `.ffffff` fraction only when non-zero.
fn hms(micros_in_day: i64) -> String {
    let secs = micros_in_day.div_euclid(1_000_000);
    let frac = micros_in_day.rem_euclid(1_000_000);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if frac == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        let f = format!("{frac:06}");
        format!("{h:02}:{m:02}:{s:02}.{}", f.trim_end_matches('0'))
    }
}

/// A zone offset in `±HH:MM`, from the wire's "seconds west of UTC" (so the
/// conventional east-positive sign is the negation).
fn zone(seconds_west: i32) -> String {
    let east = -seconds_west;
    let sign = if east < 0 { '-' } else { '+' };
    let a = east.unsigned_abs();
    format!("{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `numeric` wire buffer from its header fields and base-10000 digits.
    fn numeric_bytes(weight: i16, sign: u16, dscale: i16, digits: &[i16]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(digits.len() as i16).to_be_bytes());
        b.extend_from_slice(&weight.to_be_bytes());
        b.extend_from_slice(&sign.to_be_bytes());
        b.extend_from_slice(&dscale.to_be_bytes());
        for d in digits {
            b.extend_from_slice(&d.to_be_bytes());
        }
        b
    }

    #[test]
    fn numeric_renders_integers_and_fractions() {
        assert_eq!(
            numeric_to_string(&numeric_bytes(0, 0x0000, 0, &[])).as_deref(),
            Some("0")
        );
        // 1234.567 → groups [1234, 5670], weight 0, scale 3.
        assert_eq!(
            numeric_to_string(&numeric_bytes(0, 0x0000, 3, &[1234, 5670])).as_deref(),
            Some("1234.567")
        );
        // 0.01 → group [100], weight -1, scale 2.
        assert_eq!(
            numeric_to_string(&numeric_bytes(-1, 0x0000, 2, &[100])).as_deref(),
            Some("0.01")
        );
        // Trailing zeros are kept to the display scale: 1.50.
        assert_eq!(
            numeric_to_string(&numeric_bytes(0, 0x0000, 2, &[1, 5000])).as_deref(),
            Some("1.50")
        );
        // Negative and the NaN sigil.
        assert_eq!(
            numeric_to_string(&numeric_bytes(0, 0x4000, 0, &[42])).as_deref(),
            Some("-42")
        );
        assert_eq!(
            numeric_to_string(&numeric_bytes(0, 0xC000, 0, &[])).as_deref(),
            Some("NaN")
        );
    }

    #[test]
    fn timestamp_and_date_pivot_on_the_pg_epoch() {
        assert_eq!(timestamp_to_string(0), "2000-01-01 00:00:00");
        assert_eq!(timestamp_to_string(USECS_PER_DAY), "2000-01-02 00:00:00");
        assert_eq!(timestamp_to_string(-USECS_PER_DAY), "1999-12-31 00:00:00");
        // 2000-01-01 13:45:30.5
        let micros = (13 * 3600 + 45 * 60 + 30) * 1_000_000 + 500_000;
        assert_eq!(timestamp_to_string(micros), "2000-01-01 13:45:30.5");
        assert_eq!(timestamptz_to_string(0), "2000-01-01 00:00:00+00");

        assert_eq!(date_to_string(0), "2000-01-01");
        assert_eq!(date_to_string(-1), "1999-12-31");
        assert_eq!(date_to_string(366), "2001-01-01");
    }

    #[test]
    fn time_trims_the_fraction() {
        assert_eq!(
            time_to_string((13 * 3600 + 45 * 60 + 30) * 1_000_000),
            "13:45:30"
        );
        assert_eq!(time_to_string(500_000), "00:00:00.5");
    }

    #[test]
    fn uuid_is_canonical_hex() {
        let raw: Vec<u8> = (0u8..16).collect();
        assert_eq!(
            uuid_to_string(&raw).as_deref(),
            Some("00010203-0405-0607-0809-0a0b0c0d0e0f")
        );
        assert_eq!(uuid_to_string(&[0u8; 4]), None);
    }
}
