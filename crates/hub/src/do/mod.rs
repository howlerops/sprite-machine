//! Durable Object skeletons (wasm32 only, M2-001). Each class is SQLite-backed (wrangler.toml
//! `new_sqlite_classes`) and answers `501` until its ledger item fills it in:
//!
//! - [`Thread`]: one per thread / flow run (M2-008, `thread.rs`, implemented).
//! - [`Machine`]: one per runner, the tunnel endpoint (M2B-002, `machine.rs`, implemented).
//! - [`Flow`]: one per flow run, the durable state machine with alarms (§4.9).

// `#[durable_object]` expands to wasm-bindgen code that expects `wasm_bindgen` in scope.
use worker::wasm_bindgen;
use worker::{DurableObject, Env, Request, Response, State, durable_object};

mod machine;
mod thread;

pub use machine::Machine;
pub use thread::Thread;

/// The Flow DO (binding `FLOW`). Skeleton: no state yet.
#[durable_object]
pub struct Flow {
    _state: State,
    _env: Env,
}

impl DurableObject for Flow {
    fn new(state: State, env: Env) -> Self {
        Self {
            _state: state,
            _env: env,
        }
    }

    async fn fetch(&self, _req: Request) -> worker::Result<Response> {
        Response::error("flow: not implemented yet", 501)
    }
}
