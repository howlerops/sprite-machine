//! Local time of day in a user's time zone, for quiet hours.
//!
//! - `UTC`, `Etc/UTC`, `GMT` and fixed offsets (`+05:30`, `UTC-8`, `GMT+01:00`) resolve everywhere.
//! - Any other IANA name (`Europe/Berlin`) resolves on the Worker through `Intl.DateTimeFormat`
//!   (the runtime's own tz database, daylight saving included). Natively there is no tz
//!   database, so such a name does not resolve and the native tests use fixed offsets.

/// Minutes after local midnight (0..=1439) at `now_ms` in `zone`, or `None` for an unknown zone.
pub fn local_minute(zone: &str, now_ms: u64) -> Option<u16> {
    let zone = zone.trim();
    if let Some(offset) = fixed_offset_minutes(zone) {
        let minutes_of_day = i64::try_from(now_ms / 60_000).ok()? + i64::from(offset);
        return u16::try_from(minutes_of_day.rem_euclid(24 * 60)).ok();
    }
    intl_local_minute(zone, now_ms)
}

/// `UTC`/`GMT`/`Z` and `[UTC|GMT]±H[H][:MM]` as an offset in minutes.
fn fixed_offset_minutes(zone: &str) -> Option<i32> {
    if matches!(zone, "UTC" | "Etc/UTC" | "GMT" | "Etc/GMT" | "Z") {
        return Some(0);
    }
    let rest = zone
        .strip_prefix("UTC")
        .or_else(|| zone.strip_prefix("GMT"))
        .unwrap_or(zone);
    let sign = match rest.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = &rest[1..];
    let (hours, minutes) = match digits.split_once(':') {
        Some((hours, minutes)) if minutes.len() == 2 => (hours, minutes.parse::<i32>().ok()?),
        Some(_) => return None,
        None => (digits, 0),
    };
    if !(1..=2).contains(&hours.len()) || !hours.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours = hours.parse::<i32>().ok()?;
    (hours <= 14 && minutes < 60).then_some(sign * (hours * 60 + minutes))
}

#[cfg(not(target_arch = "wasm32"))]
fn intl_local_minute(_zone: &str, _now_ms: u64) -> Option<u16> {
    None
}

/// `new Intl.DateTimeFormat("en-US", {timeZone, hourCycle: "h23", hour, minute}).format(date)`.
/// The constructor throws a `RangeError` for an unknown zone, which `Reflect::construct` turns
/// into an `Err` (a plain `Intl::DateTimeFormat::new` would trap).
#[cfg(target_arch = "wasm32")]
fn intl_local_minute(zone: &str, now_ms: u64) -> Option<u16> {
    use worker::js_sys::{Array, Date, Function, Object, Reflect};
    use worker::wasm_bindgen::{JsCast, JsValue};

    let global = worker::js_sys::global();
    let intl = Reflect::get(&global, &"Intl".into()).ok()?;
    let ctor: Function = Reflect::get(&intl, &"DateTimeFormat".into())
        .ok()?
        .dyn_into()
        .ok()?;
    let options = Object::new();
    for (key, value) in [
        ("timeZone", zone),
        ("hourCycle", "h23"),
        ("hour", "2-digit"),
        ("minute", "2-digit"),
    ] {
        Reflect::set(&options, &key.into(), &value.into()).ok()?;
    }
    let args = Array::of2(&"en-US".into(), &options);
    let format = Reflect::construct(&ctor, &args).ok()?;
    let format_fn: Function = Reflect::get(&format, &"format".into())
        .ok()?
        .dyn_into()
        .ok()?;
    let date = Date::new(&JsValue::from_f64(now_ms as f64));
    let text = format_fn.call1(&format, &date).ok()?.as_string()?;
    let (hours, minutes) = text.trim().split_once(':')?;
    let (hours, minutes) = (hours.parse::<u16>().ok()?, minutes.parse::<u16>().ok()?);
    (minutes < 60).then_some((hours % 24) * 60 + minutes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_and_fixed_offsets_resolve() {
        let noon = 12 * 3_600_000;
        assert_eq!(local_minute("UTC", noon), Some(12 * 60));
        assert_eq!(local_minute("Etc/UTC", noon), Some(12 * 60));
        assert_eq!(local_minute("+05:30", noon), Some(17 * 60 + 30));
        assert_eq!(local_minute("UTC-8", noon), Some(4 * 60));
        assert_eq!(
            local_minute("GMT+01:00", 23 * 3_600_000 + 30 * 60_000),
            Some(30)
        );
        // Crossing midnight backwards.
        assert_eq!(local_minute("UTC-05:00", 3 * 3_600_000), Some(22 * 60));
    }

    #[test]
    fn nonsense_does_not_resolve() {
        for zone in ["", "Mars/Base", "UTC+25", "UTC+1:5", "+aa:bb", "UTC+"] {
            assert_eq!(local_minute(zone, 0), None, "{zone}");
        }
    }
}
