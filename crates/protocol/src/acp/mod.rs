//! `acp`: ACP v1 wire types, re-exported from the official schema crate.
//!
//! The types come from `agent-client-protocol-schema` (`v1` module), the crate the
//! official `agent-client-protocol` 3.x SDK builds on. The SDK itself is not a
//! dependency of `protocol`: it pulls in `uuid` (getrandom) and process/stdio code,
//! which the hub (wasm32) must not link. The runner depends on the SDK directly.

pub use agent_client_protocol_schema::v1::*;
