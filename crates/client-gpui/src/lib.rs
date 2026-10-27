//! Desktop UI on gpui-kit: thin views over client-core.

pub mod agents;
pub mod approval;
pub mod compose;
pub mod diff;
pub mod flows;
pub mod inbox;
pub mod loadout;
pub mod local;
pub mod machines;
pub mod notify;
pub mod palette;
pub mod settings;
pub mod shell;
pub mod task;
pub mod usage;
pub mod workspace;

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "client-gpui");
    }
}
