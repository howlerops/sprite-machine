//! Client core: TurnEvent subscription, view models, cache, outbound queue.

pub mod tracer;

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "client-core");
    }
}
