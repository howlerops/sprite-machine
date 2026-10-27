//! The TurnEvent envelope (SPEC-001, `docs/specs/turn-event.md`).
//!
//! - [`Envelope`]: the one event format inside the system (§2).
//! - [`Event`]: every variant from §4, tagged by `type`. Unknown types parse as
//!   [`Event::Unknown`] so older consumers ignore newer events instead of failing (§2).
//! - [`ThreadSeq`]: the per-thread `seq` stamper the Thread DO uses (§3).
//! - [`format_timestamp`]: RFC 3339 UTC with milliseconds, for `ts` (§2).
//!
//! Pure types only: no IO and no clock, so this builds for wasm32 (the hub).

mod event;
mod seq;
mod time;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub use crate::tunnel::{Actor, ActorKind, ContentPart};
pub use event::*;
pub use seq::{StampError, ThreadSeq};
pub use time::format_timestamp;

/// Envelope version carried in `v` (§2).
pub const ENVELOPE_VERSION: u8 = 1;

/// Largest serialized envelope, in bytes (§2).
pub const MAX_ENVELOPE_BYTES: usize = 256 * 1024;

/// One TurnEvent (§2).
///
/// `task_id` and `run_id` are always serialized (as `null` when absent), as the schema
/// requires. `seq` is `0` on the producer side; only the Thread DO assigns it ([`ThreadSeq`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: Version,
    pub seq: u64,
    pub thread_id: String,
    pub task_id: Option<String>,
    pub run_id: Option<String>,
    /// Producer time, RFC 3339 UTC with milliseconds. Informational only.
    pub ts: String,
    pub event: Event,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<RawPayload>,
}

impl Envelope {
    /// A producer-side envelope (`seq: 0`, no `raw`).
    pub fn new(
        thread_id: impl Into<String>,
        task_id: Option<String>,
        run_id: Option<String>,
        ts: impl Into<String>,
        event: impl Into<Event>,
    ) -> Self {
        Self {
            v: Version,
            seq: 0,
            thread_id: thread_id.into(),
            task_id,
            run_id,
            ts: ts.into(),
            event: event.into(),
            raw: None,
        }
    }
}

/// The envelope version. Serializes as `1`; any other value fails to parse (§2: "a consumer
/// rejects any other value").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Version;

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(ENVELOPE_VERSION)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v = u64::deserialize(deserializer)?;
        if v == u64::from(ENVELOPE_VERSION) {
            Ok(Version)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported envelope version {v}, expected {ENVELOPE_VERSION}"
            )))
        }
    }
}

/// The source payload an event was mapped from, kept verbatim (§2 `raw`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawPayload {
    pub source: RawSource,
    pub payload: Value,
}

/// Where a `raw` payload came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawSource {
    Acp,
    A2a,
    Agui,
    Flow,
    Hub,
}

/// `true` when `id` is a valid `Id` (§2): 1–128 chars of `[A-Za-z0-9_:.-]`.
pub fn is_valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'.' | b'-'))
}

/// Deserializes a field that is optional but whose JSON `null` is a real value (for
/// `raw_output`, `result`, patch `value`): absent → `None`, present (even `null`) → `Some`.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}
