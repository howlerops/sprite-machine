//! Thread logic (M2-008): everything one thread does, runtime-agnostic and natively tested.
//!
//! - [`ThreadCore`] over a [`ThreadStore`]: `seq` assignment, the §5 validator, state
//!   snapshots / deltas, the subscription catch-up, interrupts, the pending-task queue, cancel
//!   and idempotency. [`MemoryStore`] backs native tests.
//! - [`api`]: the Thread DO's HTTP surface as a pure function over a core.
//! - [`auth`]: the dev-token auth placeholder (client-api.md §2).
//! - [`redact`]: secret redaction before an envelope is stored or broadcast (turn-event.md §2).
//! - `sqlite` and `routes` (wasm32 only): the SQLite-backed store and the Worker routes. The
//!   Durable Object class itself is `do/thread.rs`.

pub mod api;
pub mod auth;
mod core;
mod error;
mod patch;
pub mod redact;
mod store;
mod timestamp;

#[cfg(test)]
mod tests;

#[cfg(target_arch = "wasm32")]
pub mod routes;
#[cfg(target_arch = "wasm32")]
pub mod sqlite;

pub use self::core::{
    CATCH_UP_PAGE, CatchUp, CreateThreadBody, EVENT_ID_TTL_MS, IDEMPOTENCY_TTL_MS, PromptBody,
    RETAINED_RUNS, Reply, Resolved, SCOPES, SessionBody, SessionConfigBody, ThreadCore,
    event_message, request_hash, scope_name, valid_idempotency_key,
};
pub use self::error::ApiError;
pub use self::patch::apply_patch;
pub use self::store::{
    CommandRecord, IdempotencyRecord, InterruptRecord, MemoryStore, Resolution, RunRecord,
    StoreError, StoreResult, TaskRecord, TaskStatus, ThreadStore,
};
pub use self::timestamp::parse_timestamp;
