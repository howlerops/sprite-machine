//! Attachments (M2-009, client-api.md §5.6, PLAN-v2 §4.2 "R2"): `POST /v1/attachments` makes two
//! HMAC-signed URLs for one attachment in R2 (`BLOBS`). The client `PUT`s the bytes to
//! `upload_url`, then puts `url` in a `{type:"file"}` part; the runner fetches `url`
//! (`crates/sidecar/src/bridge/files.rs`), so the URLs are https and public like any other.
//!
//! - [`sign`]: the signature, HMAC-SHA256 with a key from the `ATTACHMENT_SIGNING_KEY` secret
//!   or var. Verification is constant-time and checks the expiry.
//! - [`store`]: the [`Blobs`](store::Blobs) trait: R2 in the Worker, an in-memory fake natively.
//! - [`service`]: create, upload and content over `Db` and `Blobs`. Natively tested.
//! - [`routes`] (wasm32 only): the axum glue and the R2 binding.

pub mod service;
pub mod sign;
pub mod store;

#[cfg(target_arch = "wasm32")]
pub mod routes;

#[cfg(test)]
mod tests;

/// The largest attachment accepted, in bytes (client-api.md §5.6: 20 MB).
pub const MAX_BYTES: u64 = 20 * 1024 * 1024;

/// How long both signed URLs stay valid, in milliseconds. The runner fetches `url` when a prompt
/// is dispatched, which can be later than the upload, so this is a day.
pub const URL_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// The var or secret that holds the URL signing key (at least 32 bytes). Never logged.
pub const SIGNING_KEY_VAR: &str = "ATTACHMENT_SIGNING_KEY";
