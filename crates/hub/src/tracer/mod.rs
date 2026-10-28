// TRACER STUB: replaced by M2B-002 follow-ups (the tracer's thread store and HTTP routes go away when the cli tracer uses the Thread DO routes)
//! The tracer bullet hub (MVP-000): the files in this module are TRACER STUBS that a later
//! ledger item replaces. See each file's first line.
//!
//! - [`thread`]: a thin wrapper over the real `threads::ThreadCore` (M2-008 replaced the in-memory stub).
//! - [`state`]: the tracer's threads and its HTTP API logic in one object. The tunnel is not
//!   here any more: M2B-002 replaced `tracer/machine.rs` with the real Machine DO
//!   (`crate::machines`), which this object pushes commands to and takes events from.
//! - `worker` (wasm32 only): routes and the `TracerHub` Durable Object.

pub mod state;
pub mod thread;
#[cfg(target_arch = "wasm32")]
mod worker;

#[cfg(target_arch = "wasm32")]
pub use worker::TracerHub;
