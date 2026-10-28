// TRACER STUB: replaced by M2-008 + M2B-002 (the whole tracer hub module goes away)
//! The tracer bullet hub (MVP-000): every file in this module is a TRACER STUB that a later
//! ledger item replaces. See each file's first line.
//!
//! - [`thread`]: a thin wrapper over the real `threads::ThreadCore` (M2-008 replaced the in-memory stub).
//! - [`machine`]: in-memory tunnel endpoint on the `protocol::tunnel` state machines (→ M2B-002).
//! - [`state`]: both, plus the HTTP API logic, in one object (→ M2-008 + M2B-002).
//! - `worker` (wasm32 only): routes and the `TracerHub` Durable Object (→ M2-008 + M2B-002).

pub mod machine;
pub mod state;
pub mod thread;
#[cfg(target_arch = "wasm32")]
mod worker;

#[cfg(target_arch = "wasm32")]
pub use worker::TracerHub;
