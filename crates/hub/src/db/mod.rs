//! The hub's D1 layer (plan §4.2): org-wide metadata behind a small async `Db` trait.
//!
//! - [`Db`] is the only thing the typed helpers ([`queries`]) talk to. It has two methods, both
//!   returning futures without a runtime: `execute` (writes, returns the changed row count) and
//!   `query` (rows as JSON objects). No tokio, no `Send` bounds (Workers are single-threaded).
//! - The Worker implements it over the `DB` binding ([`d1`], wasm32 only).
//! - Tests implement it over an in-memory SQLite that runs the same migration files
//!   ([`MIGRATIONS`]), so the unit and integration tests exercise the real schema.
//!
//! Migrations live in `crates/hub/migrations/` and are applied by
//! `wrangler d1 migrations apply DB` (wrangler.toml `migrations_dir`). Keep [`MIGRATIONS`] in
//! step with that directory: `tests/d1_schema.rs` fails if a file is missing from it.

use std::fmt;
use std::future::Future;

use serde_json::{Map, Value};

pub mod queries;
mod roles;

#[cfg(target_arch = "wasm32")]
pub mod d1;

pub use roles::{Role, UnknownRole};

/// The migration files, in apply order, embedded for the native fake and for checks. wrangler
/// applies the same files from `migrations/` on the real database.
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_init.sql",
        include_str!("../../migrations/0001_init.sql"),
    ),
    (
        "0002_org_metadata.sql",
        include_str!("../../migrations/0002_org_metadata.sql"),
    ),
    (
        "0003_auth_sessions.sql",
        include_str!("../../migrations/0003_auth_sessions.sql"),
    ),
    (
        "0004_machines.sql",
        include_str!("../../migrations/0004_machines.sql"),
    ),
    (
        "0005_devices.sql",
        include_str!("../../migrations/0005_devices.sql"),
    ),
    (
        "0006_thread_hosts.sql",
        include_str!("../../migrations/0006_thread_hosts.sql"),
    ),
    (
        "0007_device_limits.sql",
        include_str!("../../migrations/0007_device_limits.sql"),
    ),
];

/// One result row: column name to JSON value (what D1 returns for an object row).
pub type Row = Map<String, Value>;

/// A bound statement parameter. D1 and SQLite both take these four storage classes.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

impl Param {
    /// `Some(text)` binds as text, `None` as NULL.
    pub fn opt<S: Into<String>>(value: Option<S>) -> Self {
        value.map_or(Param::Null, |text| Param::Text(text.into()))
    }
}

impl From<&str> for Param {
    fn from(value: &str) -> Self {
        Param::Text(value.to_owned())
    }
}

impl From<String> for Param {
    fn from(value: String) -> Self {
        Param::Text(value)
    }
}

impl From<i64> for Param {
    fn from(value: i64) -> Self {
        Param::Int(value)
    }
}

impl From<bool> for Param {
    fn from(value: bool) -> Self {
        Param::Int(i64::from(value))
    }
}

/// A failed statement or an unreadable row. Carries the backend's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    /// The database rejected the statement (constraint, syntax, trigger abort, ...).
    Backend(String),
    /// A row came back with a missing or wrong-typed column.
    Decode(String),
    /// A value stored in the database is not one this code knows (e.g. an unknown role).
    Invalid(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Backend(message) => write!(f, "database error: {message}"),
            DbError::Decode(message) => write!(f, "cannot read row: {message}"),
            DbError::Invalid(message) => write!(f, "invalid stored value: {message}"),
        }
    }
}

impl std::error::Error for DbError {}

/// Async access to one database. Implementations own their connection or binding.
pub trait Db {
    /// Run one write statement. Returns the number of rows it changed.
    fn execute(&self, sql: &str, params: &[Param]) -> impl Future<Output = Result<u64, DbError>>;

    /// Run one read statement. Each row is a JSON object keyed by column name.
    fn query(&self, sql: &str, params: &[Param])
    -> impl Future<Output = Result<Vec<Row>, DbError>>;
}

/// Read a text column; a NULL or missing column is a decode error.
pub(crate) fn text(row: &Row, column: &str) -> Result<String, DbError> {
    match row.get(column) {
        Some(Value::String(text)) => Ok(text.clone()),
        other => Err(DbError::Decode(format!(
            "column {column} is {other:?}, expected text"
        ))),
    }
}

/// Read a nullable text column.
pub(crate) fn opt_text(row: &Row, column: &str) -> Result<Option<String>, DbError> {
    match row.get(column) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        other => Err(DbError::Decode(format!(
            "column {column} is {other:?}, expected text or null"
        ))),
    }
}

/// Read an integer column.
pub(crate) fn int(row: &Row, column: &str) -> Result<i64, DbError> {
    row.get(column)
        .and_then(Value::as_i64)
        .ok_or_else(|| DbError::Decode(format!("column {column} is not an integer")))
}
