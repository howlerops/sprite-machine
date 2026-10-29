//! `notify`: desktop notifications, deep links and notification settings (plan §4.2, M3-013).
//!
//! - [`Notifier`] (`model.rs`) is fed the thread status of every inbox / thread refresh and decides
//!   which transitions notify: a thread *becoming* `needs_you`, `done` or `failed`, once each,
//!   never for the thread on screen, never for a kind the prefs switch off. It hands the result to
//!   a [`NotificationSink`]; clicking one yields [`NotifyEvent::Open`].
//! - [`ToastSink`] (`view.rs`) is the real sink: gpui-component notifications, delivered in-app and
//!   to the OS notification center (`NotificationDelivery::InAppAndSystem`).
//! - [`hub_link_thread`] (`link.rs`) turns `https://<hub>/t/<id>` into a thread id.
//! - [`NotifyPrefs`] / [`NotifySettingsView`]: per-kind toggles persisted to a JSON file.
//!
//! Titles are generic ("Task finished"): never a machine name, prompt or agent output.

mod link;
mod model;
mod view;

use std::sync::OnceLock;

use crate::shell::i18n::Catalog;

pub use link::hub_link_thread;
pub use model::{
    Notification, NotificationSink, Notifier, NotifyConfig, NotifyEvent, NotifyKind, NotifyPrefs,
    PrefsHandle,
};
pub use view::{EventQueue, NotifySettingsView, ToastSink};

/// The notification strings (`locales/en/notify.ftl`).
pub const EN_FTL: &str = include_str!("../../locales/en/notify.ftl");

pub(crate) fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG
        .get_or_init(|| Catalog::from_source("en", EN_FTL).expect("locales/en/notify.ftl parses"))
}

pub(crate) fn tr(id: &str) -> String {
    catalog().tr(id, &[])
}

pub(crate) fn tr_args(id: &str, args: &[(&str, &str)]) -> String {
    catalog().tr(id, args)
}
