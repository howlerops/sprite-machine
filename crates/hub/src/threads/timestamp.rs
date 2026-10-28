//! RFC 3339 parsing for `expires_at` (the inverse of `protocol::turn_event::format_timestamp`),
//! without a date crate.

/// Parses `YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)` to milliseconds since the Unix epoch.
/// Returns `None` for anything else (and for dates before 1970).
pub fn parse_timestamp(text: &str) -> Option<u64> {
    if !text.is_ascii() {
        return None;
    }
    let bytes = text.as_bytes();
    let digits = |from: usize, len: usize| -> Option<u64> {
        let part = bytes.get(from..from + len)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    let (year, month, day) = (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?);
    if bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return None;
    }
    if !matches!(bytes.get(10), Some(b'T' | b't' | b' ')) {
        return None;
    }
    let (hour, minute, second) = (digits(11, 2)?, digits(14, 2)?, digits(17, 2)?);
    if bytes.get(13) != Some(&b':') || bytes.get(16) != Some(&b':') {
        return None;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    if second > 60 {
        return None;
    }
    let mut at = 19;
    let mut millis = 0;
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        let start = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == start {
            return None;
        }
        // Keep millisecond precision: first three fraction digits, zero padded.
        let padded = format!("{:0<3}", &text[start..at]);
        millis = padded.get(..3)?.parse::<u64>().ok()?;
    }
    let offset_minutes: i64 = match bytes.get(at) {
        Some(b'Z' | b'z') if at + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) if at + 6 == bytes.len() => {
            let (oh, om) = (digits(at + 1, 2)?, digits(at + 4, 2)?);
            if bytes.get(at + 3) != Some(&b':') || oh > 23 || om > 59 {
                return None;
            }
            let minutes = (oh * 60 + om) as i64;
            if *sign == b'+' { minutes } else { -minutes }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day)?;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    let utc_ms = (secs * 1000 + millis) as i64 - offset_minutes * 60_000;
    u64::try_from(utc_ms).ok()
}

/// Days from 1970-01-01 to the given proleptic Gregorian date (H. Hinnant), `None` before 1970.
fn days_from_civil(year: u64, month: u64, day: u64) -> Option<u64> {
    let y = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe).checked_sub(719_468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::turn_event::format_timestamp;

    #[test]
    fn round_trips_format_timestamp() {
        for ms in [0, 951_782_400_123, 1_793_138_403_000, 1_793_138_403_456] {
            assert_eq!(parse_timestamp(&format_timestamp(ms)), Some(ms));
        }
    }

    #[test]
    fn accepts_offsets_and_missing_millis() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(parse_timestamp("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_timestamp("1970-01-01T00:00:00.5Z"), Some(500));
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "tomorrow",
            "2026-13-01T00:00:00Z",
            "2026-10-27T22:00:03",
            "2026-10-27",
        ] {
            assert_eq!(parse_timestamp(bad), None, "{bad}");
        }
    }
}
