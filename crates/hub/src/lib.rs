//! The hub Cloudflare Worker (wasm32, no tokio): routes and Durable Objects (thread, machine, flow), D1, R2, queues.
//!
//! Platform-neutral code (`router`, the tracer's state machines) builds and tests natively. The
//! Worker glue (`worker`, the Durable Object skeletons, and the tracer's `TracerHub`) is
//! wasm32 only, so `cargo test -p hub` runs on the host.

pub mod router;
pub mod threads;
pub mod tracer;

/// The Worker entries: axum router, `fetch` and `queue`.
#[cfg(target_arch = "wasm32")]
pub mod worker;

/// Durable Object skeletons (THREAD, MACHINE, FLOW).
#[cfg(target_arch = "wasm32")]
#[path = "do/mod.rs"]
pub mod durable_objects;

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "hub");
    }
}
