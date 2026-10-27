//! Shared wire types: the TurnEvent envelope, ACP, A2A, AG-UI, tunnel frames and the mappings between them. Runtime-agnostic (builds for wasm32).

pub mod a2a;
pub mod acp;
pub mod agui;
pub mod mapping;
pub mod tunnel;
pub mod turn_event;

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "protocol");
    }
}
