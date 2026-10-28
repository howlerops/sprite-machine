//! Threads as tasks (M2-010): the D1 index behind `GET /v1/threads`, the v1 auto-title, and the
//! Thread DO's sync that keeps the index in step.
//!
//! - [`title`]: the auto-title from the first prompt (pure).
//! - [`index`]: the index row the Thread DO builds, and when a request must refresh it (pure).
//! - [`query`]: the list's query parameters and the keyset cursor (pure).
//! - [`store`]: the D1 upsert and the org-scoped, filtered, paginated list over [`crate::db::Db`].
//! - `sync` and `routes` (wasm32 only): the Thread DO's D1 write and the `GET /v1/threads` handler.
//!
//! Titles: the client's title (create body or `PATCH`) wins; otherwise the auto-title. Deletion
//! is soft (`deleted_at`); archiving is a timestamp (`archived_at`). See the migrations.

pub mod index;
pub mod query;
pub mod store;
pub mod title;

#[cfg(test)]
mod tests;

#[cfg(target_arch = "wasm32")]
pub mod routes;
#[cfg(target_arch = "wasm32")]
pub mod sync;
