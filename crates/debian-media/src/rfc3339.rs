//! RFC 3339 UTC timestamps for provenance manifests (MVP-610).
//!
//! Formatting a `SystemTime` needs nothing but integer arithmetic, so this
//! avoids pulling a date/time crate (and its licence surface) into the host
//! binary. Leap seconds do not exist in Unix time, so the conversion is exact.

use std::time::{SystemTime, UNIX_EPOCH};

/// Formats "now" as an RFC 3339 timestamp in UTC, e.g. `2026-08-18T12:34:56Z`.
///
/// A host clock set before 1970 yields the epoch rather than an error: the
/// timestamp is provenance metadata, not a security control.
pub fn now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_utc(secs)
}

/// Formats a Unix timestamp (seconds since the epoch, UTC) as RFC 3339.
pub fn format_unix_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (h, m, s) = (
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 → (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11] with March = 0
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_timestamps() {
        assert_eq!(format_unix_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix_utc(1), "1970-01-01T00:00:01Z");
        // 2000-02-29 — leap year divisible by 400.
        assert_eq!(format_unix_utc(951_782_400), "2000-02-29T00:00:00Z");
        // 2100-03-01 — 2100 is *not* a leap year.
        assert_eq!(format_unix_utc(4_107_542_400), "2100-03-01T00:00:00Z");
        assert_eq!(format_unix_utc(1_755_475_200), "2025-08-18T00:00:00Z");
        assert_eq!(format_unix_utc(1_755_519_296), "2025-08-18T12:14:56Z");
    }

    #[test]
    fn now_has_the_right_shape() {
        let s = now_utc();
        assert_eq!(s.len(), 20, "{s}");
        assert!(s.ends_with('Z'), "{s}");
        assert_eq!(s.as_bytes()[10], b'T', "{s}");
        // Sanity: the clock is somewhere in the third millennium.
        assert!(s.starts_with('2'), "{s}");
    }

    #[test]
    fn month_and_day_boundaries_roundtrip() {
        // Walk every day of four consecutive years and check monotonicity plus
        // valid field ranges — cheap protection against off-by-one in the
        // civil-from-days conversion.
        let mut previous = String::new();
        for day in 18_000..19_461u64 {
            let s = format_unix_utc(day * 86_400);
            assert!(s > previous, "{s} <= {previous}");
            let month: u32 = s[5..7].parse().unwrap();
            let dom: u32 = s[8..10].parse().unwrap();
            assert!((1..=12).contains(&month), "{s}");
            assert!((1..=31).contains(&dom), "{s}");
            previous = s;
        }
    }
}
