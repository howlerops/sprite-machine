//! A2A 1.0 edge adapter: client for BYO agents (in) and server for our agents and flows (out).

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "a2a-edge");
    }
}
