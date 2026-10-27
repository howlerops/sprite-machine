//! The hub Cloudflare Worker (wasm32, no tokio): routes and Durable Objects (thread, machine, flow), D1, R2, queues.

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
