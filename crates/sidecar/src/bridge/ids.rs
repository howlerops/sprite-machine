//! Id generation for the bridge: `int_` interrupt ids and the mapper's `msg_`, `rsn_`, `run_`.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::mapping::acp_turn::{IdSource, SequentialIds};

/// Source of fresh ids. `prefix` is `"int_"`, `"msg_"`, `"rsn_"` or `"run_"`.
pub trait IdGenerator: Send + Sync + 'static {
    /// A valid turn-event `Id` that starts with `prefix` and was never returned before.
    fn next_id(&self, prefix: &str) -> String;
}

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Production ids: `<prefix><ULID>`, 26 Crockford base32 characters: a 48-bit millisecond
/// timestamp, a per-process counter (so ids sort in creation order) and hashed random bits.
pub struct UlidIds {
    counter: AtomicU64,
    random: RandomState,
}

impl Default for UlidIds {
    fn default() -> Self {
        Self {
            counter: AtomicU64::new(0),
            random: RandomState::new(),
        }
    }
}

impl UlidIds {
    pub fn new() -> Self {
        Self::default()
    }
}

fn push_base32(out: &mut String, mut value: u64, chars: usize) {
    let mut digits = vec![0_u8; chars];
    for digit in digits.iter_mut().rev() {
        let index = usize::try_from(value & 31).unwrap_or(0);
        *digit = ALPHABET[index];
        value >>= 5;
    }
    out.extend(digits.into_iter().map(char::from));
}

impl IdGenerator for UlidIds {
    fn next_id(&self, prefix: &str) -> String {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let count = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut hasher = self.random.build_hasher();
        hasher.write_u64(ms);
        hasher.write_u64(count);
        let mut id = String::with_capacity(prefix.len() + 26);
        id.push_str(prefix);
        push_base32(&mut id, ms, 10);
        push_base32(&mut id, count, 6);
        push_base32(&mut id, hasher.finish(), 10);
        id
    }
}

/// Deterministic ids for tests: `<prefix><n>`, counted per prefix (`run_1`, `int_1`, ...).
#[derive(Default)]
pub struct SequentialGenerator(Mutex<SequentialIds>);

impl SequentialGenerator {
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdGenerator for SequentialGenerator {
    fn next_id(&self, prefix: &str) -> String {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next_id(prefix)
    }
}

/// Adapts the shared generator to the mapper's [`IdSource`].
pub(crate) struct SharedIds(pub Arc<dyn IdGenerator>);

impl IdSource for SharedIds {
    fn next_id(&mut self, prefix: &str) -> String {
        self.0.next_id(prefix)
    }
}
