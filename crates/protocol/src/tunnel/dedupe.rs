//! Dedupe windows for `event_id` (hub, 24 h) and `idempotency_key` (runner, 7 days), §3.5 and §4.4.

use std::collections::BTreeMap;

/// How long the hub remembers an `event_id` (§4.2, §4.4).
pub const EVENT_ID_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// How long the runner remembers an `idempotency_key` (§3.5).
pub const IDEMPOTENCY_KEY_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Whether a key was seen for the first time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// First sighting: the key was recorded, so apply the effect.
    New,
    /// Seen within the window: skip the effect (still ack the frame).
    Duplicate,
}

/// A set of recently seen keys, each kept for `ttl_ms` after it was first recorded.
///
/// Times come from the caller as `now_ms`. This is an in-memory model; the real store is
/// durable and keyed the same way.
#[derive(Debug, Clone)]
pub struct DedupeWindow {
    ttl_ms: u64,
    seen: BTreeMap<String, u64>,
}

impl DedupeWindow {
    /// A window that keeps keys for `ttl_ms`.
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            ttl_ms,
            seen: BTreeMap::new(),
        }
    }

    /// The hub's `event_id` window (24 h).
    pub fn event_ids() -> Self {
        Self::new(EVENT_ID_TTL_MS)
    }

    /// The runner's `idempotency_key` window (7 days).
    pub fn idempotency_keys() -> Self {
        Self::new(IDEMPOTENCY_KEY_TTL_MS)
    }

    /// Records `key` at `now_ms` if it is new. Expired keys are pruned first, so an
    /// expired key counts as new again.
    pub fn observe(&mut self, key: &str, now_ms: u64) -> Disposition {
        self.prune(now_ms);
        if self.seen.contains_key(key) {
            return Disposition::Duplicate;
        }
        self.seen.insert(key.to_owned(), now_ms);
        Disposition::New
    }

    /// Drops keys whose window has passed at `now_ms`.
    pub fn prune(&mut self, now_ms: u64) {
        let ttl_ms = self.ttl_ms;
        self.seen
            .retain(|_, recorded_at| now_ms.saturating_sub(*recorded_at) < ttl_ms);
    }

    /// Keys currently remembered (not yet pruned).
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// `true` when no keys are remembered.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}
