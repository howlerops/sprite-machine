//! RFC 3339 timestamps for `ts`, `expires_at`, `since` (§2), without a clock or a date crate.

/// Formats milliseconds since the Unix epoch as RFC 3339 UTC with milliseconds,
/// e.g. `2026-10-27T22:00:03.000Z`. The caller reads the clock (native: `SystemTime`;
/// Workers: `Date.now()`).
pub fn format_timestamp(unix_ms: u64) -> String {
    let millis = unix_ms % 1000;
    let secs = unix_ms / 1000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (H. Hinnant's algorithm,
/// restricted to non-negative day counts).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::format_timestamp;

    #[test]
    fn formats_epoch_and_known_dates() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00.000Z");
        // 2026-10-27T22:00:03.000Z
        assert_eq!(
            format_timestamp(1_793_138_403_000),
            "2026-10-27T22:00:03.000Z"
        );
        // Leap day.
        assert_eq!(
            format_timestamp(951_782_400_123),
            "2000-02-29T00:00:00.123Z"
        );
    }
}
