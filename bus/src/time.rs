//! Timestamps as RFC 3339 UTC strings with milliseconds, without a date crate.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `2026-11-24T20:05:11.123Z` for a Unix time in milliseconds.
pub fn rfc3339(ms: u64) -> String {
    let secs = ms / 1000;
    let milli = ms % 1000;
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let sod = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{milli:03}Z",
        sod / 3600,
        sod / 60 % 60,
        sod % 60
    )
}

/// The `HH:MM:SS` part of a timestamp made by [`rfc3339`], or the input if it has another shape.
pub fn clock(ts: &str) -> &str {
    ts.get(11..19).unwrap_or(ts)
}

/// Milliseconds for a timestamp made by [`rfc3339`], `None` for any other shape.
pub fn parse_ms(ts: &str) -> Option<u64> {
    let b = ts.as_bytes();
    if b.len() != 24 || b[4] != b'-' || b[10] != b'T' || b[23] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| ts.get(r)?.parse::<u64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s, ms) = (num(11..13)?, num(14..16)?, num(17..19)?, num(20..23)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y as i64, mo as u32, d as u32);
    let secs = u64::try_from(days).ok()? * 86_400 + h * 3600 + mi * 60 + s;
    Some(secs * 1000 + ms)
}

/// Inverse of [`civil_from_days`], after Howard Hinnant's `days_from_civil`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Days since 1970-01-01 to (year, month, day), after Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe as i64 + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn parse_round_trip() {
        for ms in [0, 951_782_400_000, 1_700_000_000_123, 1_795_000_000_999] {
            assert_eq!(parse_ms(&rfc3339(ms)), Some(ms));
        }
        assert_eq!(parse_ms("2026-11-24 20:05:11"), None);
    }

    #[test]
    fn clock_part() {
        assert_eq!(clock("2026-11-24T20:05:11.123Z"), "20:05:11");
        assert_eq!(clock("short"), "short");
    }
}
