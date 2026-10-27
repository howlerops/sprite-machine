//! The fleet MCP server: list_agents, delegate, await_task, cancel_task, start_flow.

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "fleet-mcp");
    }
}
