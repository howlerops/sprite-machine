//! `onboarding`: first run after sign-in (plan §3 surface 0). Three ways to get a first agent:
//!
//! - (a) a model API key for cloud workspaces and (b) a bring-your-own A2A URL: both are shown
//!   but disabled, each with a Fluent reason, until the hub APIs exist (M2-002 P4, M2-004 P5);
//! - (c) run `sm rc` on your computer: the steps with the exact commands and what to expect. The
//!   page completes by itself when the shell's machines feed reports a machine.
//!
//! [`model`] is plain Rust (what is shown, and when); [`OnboardingView`] renders it. The shell
//! owns the machines feed and decides when to mount the view (see [`needed`]).

mod model;
mod view;

use std::sync::OnceLock;

use crate::shell::i18n::Catalog;

pub use model::{
    BYO_URL_API_AVAILABLE, FeedState, MODEL_KEY_API_AVAILABLE, PathId, PathInfo, RC_COMMAND,
    login_command, needed, paths,
};
pub use view::{
    BYO_ID, BYO_REASON_ID, COPY_LOGIN_ID, COPY_RC_ID, EXPECT_ID, LOGIN_COMMAND_ID, MODEL_KEY_ID,
    MODEL_KEY_REASON_ID, OnboardingEvent, OnboardingView, RC_COMMAND_ID, SKIP_ID, VIEW_ID,
    WAITING_ID,
};

/// The onboarding strings (`locales/en/onboarding.ftl`).
pub const EN_FTL: &str = include_str!("../../locales/en/onboarding.ftl");

pub(crate) fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        Catalog::from_source("en", EN_FTL).expect("locales/en/onboarding.ftl parses")
    })
}

pub(crate) fn tr(id: &str) -> String {
    catalog().tr(id, &[])
}
