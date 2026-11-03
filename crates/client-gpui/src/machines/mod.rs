//! `machines`: the Machines screen (plan §3 surface 8, M2B-013). It lists the org's machines
//! with their presence and lets an admin rename one (`PATCH /v1/machines/{id}`) or revoke it
//! (`DELETE /v1/machines/{id}`, after a confirmation), client-api.md §5.1.
//!
//! - [`model`]: plain Rust (rows, name rules, the rename / confirm-revoke state);
//! - [`api`]: the two requests;
//! - [`MachinesView`] (`view.rs`) renders it. The shell feeds it the machines of its one
//!   `GET /v1/machines` feed ([`MachinesView::set_machines`]) and refetches on
//!   [`MachinesEvent::Changed`].
//!
//! The same catalog (`locales/en/machines.ftl`) holds the "waiting for <machine>" strings of the
//! task view (`task/waiting.rs`).

pub mod api;
pub mod model;
mod view;

use std::sync::OnceLock;

use crate::shell::i18n::Catalog;

pub use model::{
    Action, Failure, MAX_NAME_CHARS, MachineRow, NameError, PresenceState, Screen, rows_from,
    validate_name,
};
pub use view::{MachinesEvent, MachinesView};

/// The English strings of the Machines screen and the waiting banner.
pub const EN_FTL: &str = include_str!("../../locales/en/machines.ftl");

pub(crate) fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG
        .get_or_init(|| Catalog::from_source("en", EN_FTL).expect("locales/en/machines.ftl parses"))
}

pub(crate) fn tr(id: &str) -> String {
    catalog().tr(id, &[])
}

pub(crate) fn tr_args(id: &str, args: &[(&str, &str)]) -> String {
    catalog().tr(id, args)
}
