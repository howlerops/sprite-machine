//! `local`: the runner inside the desktop app (M2B-015, plan §4.10 "the desktop app embeds it").
//!
//! - [`LocalRunner`] runs `runner::embed::Embedded` in this process: the machine tunnel to the
//!   hub, signed in with the credential `sm login` stored (the runner's own flow; the desktop
//!   never copies, logs or writes it), and the local sessions on it.
//! - [`PhoneLink`] (plain Rust) is the connect-to-phone model: the global "Connect to phone
//!   automatically" setting and one switch per local session. A connected session is known to the
//!   hub, so the phone lists it, reads its transcript and can send prompts and answers; a
//!   disconnected one is local only.
//!
//! The setting is persisted by the shell with the other preferences
//! (`Preferences::connect_to_phone_auto`). Strings are in `locales/en/local.ftl`.

mod model;
mod runner;
mod view;

use std::sync::OnceLock;

use crate::shell::i18n::Catalog;

pub use model::{Action, LinkState, PhoneLink, SessionRow};
pub use runner::{LocalConfig, LocalRunner, RunnerStatus, Snapshot};
pub use view::{AUTO_ID, PhoneEvent, PhoneSettingsView, session_toggle_id};

/// The strings of this module (`locales/en/local.ftl`).
pub const EN_FTL: &str = include_str!("../../locales/en/local.ftl");

pub(crate) fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| Catalog::from_source("en", EN_FTL).expect("locales/en/local.ftl parses"))
}

pub(crate) fn tr(id: &str) -> String {
    catalog().tr(id, &[])
}

pub(crate) fn tr_args(id: &str, args: &[(&str, &str)]) -> String {
    catalog().tr(id, args)
}
