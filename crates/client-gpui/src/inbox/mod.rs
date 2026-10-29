//! `inbox`: the thread list (plan §3, surface 2). [`InboxModel`] (in `model.rs`) holds every
//! decision (sections, debounced search, filters, optimistic archive / rename / quick answer);
//! `api.rs` holds the requests; [`InboxView`] renders a virtualized list and forwards input.
//!
//! Thread titles, agent names and workspace paths come from the hub and are untrusted: they only
//! ever reach `div().child(String)`, never a markup parser.

mod api;
mod model;
mod view;

use std::sync::OnceLock;

use crate::shell::i18n::Catalog;

pub use api::{answer_interrupt, fetch_threads, patch_thread};
pub use model::{
    AnswerRequest, Clock, Failure, Filters, InboxModel, InterruptOption, Item, ListRequest,
    PatchRequest, PendingInterrupt, SEARCH_DEBOUNCE, Section, Status, SystemClock, ThreadRow,
    Verdict, parse_threads,
};
pub use view::{InboxEvent, InboxView, Target};

/// The inbox's English strings (`locales/en/inbox.ftl`).
pub const EN_FTL: &str = include_str!("../../locales/en/inbox.ftl");

pub(crate) fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| Catalog::from_source("en", EN_FTL).expect("locales/en/inbox.ftl parses"))
}

pub(crate) fn tr(id: &str) -> String {
    catalog().tr(id, &[])
}

pub(crate) fn tr_args(id: &str, args: &[(&str, &str)]) -> String {
    catalog().tr(id, args)
}
