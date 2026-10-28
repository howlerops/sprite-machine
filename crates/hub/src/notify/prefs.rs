//! Notification preferences (plan §4.2 "Notifications"): event classes x mute x quiet hours.
//!
//! The wire shape of `GET` and `PUT /v1/push/prefs`:
//!
//! ```json
//! { "classes": { "approval_needed": true, "task_done": true, "task_failed": false },
//!   "muted_threads": ["th_1"],
//!   "quiet_hours": { "start": "22:00", "end": "07:00", "tz": "Europe/Berlin" } }
//! ```
//!
//! `PUT` replaces everything; a class left out is on, `quiet_hours: null` turns them off. Quiet
//! hours apply to every class: inside the window a notification is skipped, not delayed.

use std::collections::{BTreeMap, BTreeSet};

use protocol::turn_event::is_valid_id;
use serde_json::{Value, json};

use super::job::Kind;
use super::tz;

/// Most muted threads one user can keep.
pub const MAX_MUTED_THREADS: usize = 500;

/// A daily window in the user's time zone. `start == end` is not allowed; `start > end` wraps
/// past midnight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuietHours {
    /// Minutes after midnight, 0..=1439.
    pub start_min: u16,
    pub end_min: u16,
    /// An IANA zone name (or a fixed offset, see [`tz`]).
    pub tz: String,
}

impl QuietHours {
    /// Whether `minute` (local minutes after midnight) falls inside the window.
    pub fn contains(&self, minute: u16) -> bool {
        if self.start_min < self.end_min {
            (self.start_min..self.end_min).contains(&minute)
        } else {
            minute >= self.start_min || minute < self.end_min
        }
    }
}

/// One user's preferences in one org.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefs {
    pub enabled: BTreeMap<Kind, bool>,
    pub muted_threads: BTreeSet<String>,
    pub quiet_hours: Option<QuietHours>,
}

impl Default for Prefs {
    /// Everything on, nothing muted, no quiet hours: what a user without stored prefs gets.
    fn default() -> Self {
        Self {
            enabled: Kind::ALL.into_iter().map(|kind| (kind, true)).collect(),
            muted_threads: BTreeSet::new(),
            quiet_hours: None,
        }
    }
}

/// Why a notification is not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    ClassOff,
    ThreadMuted,
    QuietHours,
}

impl Prefs {
    pub fn is_enabled(&self, kind: Kind) -> bool {
        self.enabled.get(&kind).copied().unwrap_or(true)
    }

    /// Applies the prefs to one notification. `now_ms` is the clock for the quiet-hours check.
    /// A zone that cannot be resolved does not silence anything (fail open: a missed approval
    /// costs more than an untimely buzz).
    pub fn decide(&self, kind: Kind, thread_id: &str, now_ms: u64) -> Result<(), Skip> {
        if !self.is_enabled(kind) {
            return Err(Skip::ClassOff);
        }
        if self.muted_threads.contains(thread_id) {
            return Err(Skip::ThreadMuted);
        }
        if let Some(quiet) = &self.quiet_hours
            && let Some(minute) = tz::local_minute(&quiet.tz, now_ms)
            && quiet.contains(minute)
        {
            return Err(Skip::QuietHours);
        }
        Ok(())
    }

    /// The JSON the API returns.
    pub fn to_json(&self) -> Value {
        let classes: serde_json::Map<String, Value> = Kind::ALL
            .into_iter()
            .map(|kind| (kind.as_str().to_owned(), json!(self.is_enabled(kind))))
            .collect();
        let quiet = self.quiet_hours.as_ref().map(|quiet| {
            json!({
                "start": format_hhmm(quiet.start_min),
                "end": format_hhmm(quiet.end_min),
                "tz": quiet.tz,
            })
        });
        json!({
            "classes": classes,
            "muted_threads": self.muted_threads,
            "quiet_hours": quiet,
        })
    }

    /// Parses and validates a `PUT` body. `Err` is the message for a `400 invalid_request`.
    pub fn from_json(body: &Value, now_ms: u64) -> Result<Self, String> {
        let object = body.as_object().ok_or("body must be a JSON object")?;
        for key in object.keys() {
            if !matches!(key.as_str(), "classes" | "muted_threads" | "quiet_hours") {
                return Err(format!("unknown field {key}"));
            }
        }
        let mut prefs = Prefs::default();
        match object.get("classes") {
            None | Some(Value::Null) => {}
            Some(Value::Object(classes)) => {
                for (name, value) in classes {
                    let kind = Kind::parse(name).ok_or_else(|| {
                        format!(
                            "unknown event class {name}; use approval_needed, task_done or task_failed"
                        )
                    })?;
                    let on = value
                        .as_bool()
                        .ok_or_else(|| format!("classes.{name} must be true or false"))?;
                    prefs.enabled.insert(kind, on);
                }
            }
            Some(_) => return Err("classes must be an object".to_owned()),
        }
        match object.get("muted_threads") {
            None | Some(Value::Null) => {}
            Some(Value::Array(ids)) => {
                if ids.len() > MAX_MUTED_THREADS {
                    return Err(format!(
                        "muted_threads holds at most {MAX_MUTED_THREADS} threads"
                    ));
                }
                for id in ids {
                    let id = id.as_str().filter(|id| is_valid_id(id)).ok_or(
                        "muted_threads must be thread ids (1-128 characters of [A-Za-z0-9_:.-])",
                    )?;
                    prefs.muted_threads.insert(id.to_owned());
                }
            }
            Some(_) => return Err("muted_threads must be an array of thread ids".to_owned()),
        }
        match object.get("quiet_hours") {
            None | Some(Value::Null) => {}
            Some(Value::Object(quiet)) => {
                let field = |name: &str| {
                    quiet
                        .get(name)
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("quiet_hours.{name} must be a string"))
                };
                let start_min = parse_hhmm(field("start")?)
                    .ok_or("quiet_hours.start must look like 22:00 (HH:MM, 00:00-23:59)")?;
                let end_min = parse_hhmm(field("end")?)
                    .ok_or("quiet_hours.end must look like 07:00 (HH:MM, 00:00-23:59)")?;
                if start_min == end_min {
                    return Err("quiet_hours.start and end must differ".to_owned());
                }
                let zone = field("tz")?;
                if tz::local_minute(zone, now_ms).is_none() {
                    return Err(format!("quiet_hours.tz {zone:?} is not a known time zone"));
                }
                prefs.quiet_hours = Some(QuietHours {
                    start_min,
                    end_min,
                    tz: zone.to_owned(),
                });
            }
            Some(_) => return Err("quiet_hours must be an object or null".to_owned()),
        }
        Ok(prefs)
    }
}

fn parse_hhmm(text: &str) -> Option<u16> {
    let (hours, minutes) = text.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let (hours, minutes) = (hours.parse::<u16>().ok()?, minutes.parse::<u16>().ok()?);
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

fn format_hhmm(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOON_UTC: u64 = 12 * 3_600_000;
    const THREE_AM_UTC: u64 = 3 * 3_600_000;

    #[test]
    fn defaults_are_everything_on() {
        let prefs = Prefs::default();
        for kind in Kind::ALL {
            assert_eq!(prefs.decide(kind, "th_1", NOON_UTC), Ok(()));
        }
    }

    #[test]
    fn class_off_and_thread_mute_skip() {
        let mut prefs = Prefs::default();
        prefs.enabled.insert(Kind::TaskDone, false);
        prefs.muted_threads.insert("th_muted".to_owned());
        assert_eq!(
            prefs.decide(Kind::TaskDone, "th_1", NOON_UTC),
            Err(Skip::ClassOff)
        );
        assert_eq!(prefs.decide(Kind::TaskFailed, "th_1", NOON_UTC), Ok(()));
        assert_eq!(
            prefs.decide(Kind::TaskFailed, "th_muted", NOON_UTC),
            Err(Skip::ThreadMuted)
        );
    }

    #[test]
    fn quiet_hours_wrap_midnight_and_follow_the_time_zone() {
        let body = json!({"quiet_hours": {"start": "22:00", "end": "07:00", "tz": "UTC"}});
        let prefs = Prefs::from_json(&body, NOON_UTC).expect("valid");
        assert_eq!(
            prefs.decide(Kind::ApprovalNeeded, "th_1", THREE_AM_UTC),
            Err(Skip::QuietHours)
        );
        assert_eq!(prefs.decide(Kind::ApprovalNeeded, "th_1", NOON_UTC), Ok(()));

        // 03:00 UTC is 22:00 the evening before at UTC-05:00: quiet there, not at noon UTC+0.
        let body = json!({"quiet_hours": {"start": "21:30", "end": "23:00", "tz": "UTC-05:00"}});
        let prefs = Prefs::from_json(&body, NOON_UTC).expect("valid");
        assert_eq!(
            prefs.decide(Kind::TaskDone, "th_1", THREE_AM_UTC),
            Err(Skip::QuietHours)
        );
        assert_eq!(prefs.decide(Kind::TaskDone, "th_1", NOON_UTC), Ok(()));
    }

    #[test]
    fn quiet_window_edges_are_start_inclusive_end_exclusive() {
        let quiet = QuietHours {
            start_min: 60,
            end_min: 120,
            tz: "UTC".into(),
        };
        assert!(!quiet.contains(59));
        assert!(quiet.contains(60));
        assert!(quiet.contains(119));
        assert!(!quiet.contains(120));
    }

    #[test]
    fn json_round_trip_and_validation() {
        let body = json!({
            "classes": {"task_failed": false},
            "muted_threads": ["th_a", "th_b"],
            "quiet_hours": {"start": "22:00", "end": "07:00", "tz": "UTC"},
        });
        let prefs = Prefs::from_json(&body, NOON_UTC).expect("valid");
        let out = prefs.to_json();
        assert_eq!(out["classes"]["task_failed"], false);
        assert_eq!(out["classes"]["task_done"], true);
        assert_eq!(out["muted_threads"], json!(["th_a", "th_b"]));
        assert_eq!(out["quiet_hours"]["end"], "07:00");
        assert_eq!(Prefs::from_json(&out, NOON_UTC), Ok(prefs));

        for bad in [
            json!([]),
            json!({"classes": {"nope": true}}),
            json!({"classes": {"task_done": "yes"}}),
            json!({"muted_threads": ["has space"]}),
            json!({"muted_threads": "th_a"}),
            json!({"quiet_hours": {"start": "25:00", "end": "07:00", "tz": "UTC"}}),
            json!({"quiet_hours": {"start": "7:00", "end": "08:00", "tz": "UTC"}}),
            json!({"quiet_hours": {"start": "07:00", "end": "07:00", "tz": "UTC"}}),
            json!({"quiet_hours": {"start": "07:00", "end": "08:00", "tz": "Mars/Base"}}),
            json!({"extra": 1}),
        ] {
            assert!(Prefs::from_json(&bad, NOON_UTC).is_err(), "{bad}");
        }
    }
}
