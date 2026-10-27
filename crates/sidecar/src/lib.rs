//! Runner core library: ACP client, bridge, durable store, host modes, keepalive, checkpoints, previews.

pub mod acp;
pub mod bridge;
pub mod checkpoint;
pub mod config;
pub mod host;
pub mod keepalive;
pub mod preview;
pub mod server;
pub mod store;

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "sidecar");
    }
}
