//! `agents`: the agents table, "add BYO agent" (disabled until the A2A edge) and the managed-agent
//! launch form (plan §3 surface 5, M3-012).
//!
//! - [`model`]: plain Rust. [`agents_from_machines`] turns `GET /v1/machines` (capabilities
//!   harnesses + presence) into [`AgentRow`]s; [`LaunchForm::validate`] and [`submit`] drive the
//!   launch form through an injectable [`ManagedAgentClient`].
//! - [`AgentsView`] (`view.rs`) renders them.
//!
//! Mounting: the shell constructs `AgentsView::new(machines, LaunchConfig { model_key_configured },
//! client, window, cx)` for the Agents section, with `machines` from `GET /v1/machines` (parse with
//! [`MachinesResponse::parse`]) and calls [`AgentsView::set_machines`] on every refresh. There is
//! no real managed-agent route yet (M4), so the shell passes its own `ManagedAgentClient`.

mod model;
mod view;

use std::sync::OnceLock;

use crate::shell::i18n::Catalog;

pub use model::{
    A2A_EDGE_AVAILABLE, AgentKind, AgentRow, Capabilities, FakeManagedClient, FormError, Health,
    LaunchConfig, LaunchError, LaunchForm, LaunchState, LaunchedAgent, MANAGED_HARNESSES,
    MachineInfo, MachinesResponse, ManagedAgentClient, ManagedLaunchRequest, Presence,
    PresenceInfo, UnavailableManagedClient, agents_from_machines, byo_disabled_reason, submit,
};
pub use view::AgentsView;

/// The agents strings (`locales/en/agents.ftl`).
pub const EN_FTL: &str = include_str!("../../locales/en/agents.ftl");

pub(crate) fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG
        .get_or_init(|| Catalog::from_source("en", EN_FTL).expect("locales/en/agents.ftl parses"))
}

pub(crate) fn tr(id: &str) -> String {
    catalog().tr(id, &[])
}

pub(crate) fn tr_args(id: &str, args: &[(&str, &str)]) -> String {
    catalog().tr(id, args)
}
