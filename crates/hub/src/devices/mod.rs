//! Device-code pairing and trusted devices (M2B-005, RFC 8628, plan §4.10).
//!
//! - [`codes`]: the device code (secret, only its SHA-256 is stored) and the human-typable user
//!   code (8 characters of an unambiguous alphabet);
//! - [`limits`]: rate limits on start and on failed user-code attempts, and the purge;
//! - [`service`]: the endpoints' logic over the `Db` trait and `MachineControl`, so it runs
//!   natively in the unit tests: start, approve, deny, poll, list, revoke, policy;
//! - [`trust`]: how the `require_trusted_devices` org setting applies to a machine;
//! - `routes` (wasm32 only): the axum glue.
//!
//! A paired machine is a normal `machines` row (and Machine DO), created the way
//! `POST /v1/machines` creates one, with `origin = 'device'`. Revoking goes through the same
//! `machines::admin::revoke_machine`, which closes the live tunnel with `auth_revoked`.

pub mod codes;
pub mod limits;
pub mod service;
pub mod trust;

#[cfg(test)]
mod tests;

#[cfg(target_arch = "wasm32")]
pub mod routes;
