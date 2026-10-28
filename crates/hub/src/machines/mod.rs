//! Machines (M2B-002): the hub side of the runner tunnel (`docs/specs/tunnel.md`).
//!
//! - [`MachineCore`] over a [`MachineStore`]: tunnel bookkeeping (resume tokens, the durable
//!   command outbox, up/down `seq`, presence) on `protocol::tunnel`'s `Sender` / `Receiver`;
//! - [`driver`]: the async frame handling around it (validation order of tunnel.md §6, append
//!   to the Thread DO, presence fan-out), generic over a [`driver::ThreadHost`];
//! - [`admin`]: `POST /v1/machines`, `DELETE /v1/machines/{id}`, `GET /v1/machines` over `Db`;
//! - [`credential`]: `smm_` credentials and `rt_` resume tokens (only hashes are stored);
//! - `sqlite`, `internal` and `routes` (wasm32 only): the DO's SQLite store, the stub calls to
//!   the other Durable Objects, and the Worker routes. The Durable Object class is
//!   `do/machine.rs`.
//!
//! Everything but the wasm32 glue builds and is unit-tested natively (`tests.rs`).

pub mod admin;
pub mod core;
pub mod credential;
pub mod driver;
pub mod store;

#[cfg(test)]
mod tests;

#[cfg(target_arch = "wasm32")]
pub mod internal;
#[cfg(target_arch = "wasm32")]
pub mod routes;
#[cfg(target_arch = "wasm32")]
pub mod sqlite;

pub use self::core::{
    Auth, COMMAND_TTL_MS, Effects, HEARTBEAT_MS, HelloInfo, MAX_INFLIGHT, MachineCore, Out,
    PresenceChange, Provisioned, PushBody, PushCommand, RESUME_TOKEN_TTL_MS, UpCheck,
};
pub use self::store::{HostKind, MachineState, MachineStore, MemoryStore};
