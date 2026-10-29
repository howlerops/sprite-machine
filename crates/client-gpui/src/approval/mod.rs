//! `approval`: approval cards (plan §3 "approval cards", turn-event.md `Interrupt{approval}`).
//!
//! - [`model`]: [`ApprovalCard`], plain data built from a `client-core` interrupt view, its
//!   transcript (the pending diff or command) and an injected clock (time left, resolver).
//! - [`controller`]: answers go through `client-core`'s `Outbox` (idempotency key, the option's
//!   own id); a 409 becomes "already resolved by <who>"; `allow_always` can be saved as the
//!   workspace preset (recorded on this device only until the hub has a preset API).
//! - [`view`]: the card widgets, which turn clicks into [`CardAction`]s.
//!
//! Strings live in `locales/en/approval.ftl`.

pub mod controller;
pub mod model;
pub mod view;

use std::sync::OnceLock;

use crate::shell::i18n::Catalog;
use crate::task::transcript::{Row, RowKind, Tone};

pub use controller::{AnswerError, ApprovalController, LocalPresets, PresetEntry};
pub use model::{
    ApprovalCard, CardContext, CardState, DiffLine, OptionButton, Pending, PresetOffer, build_card,
    format_clock, format_remaining, parse_rfc3339_ms,
};
pub use view::{CardAction, CardHandler, render_card, render_cards};

/// The card's English strings (`locales/en/approval.ftl`).
pub const EN_FTL: &str = include_str!("../../locales/en/approval.ftl");

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG
        .get_or_init(|| Catalog::from_source("en", EN_FTL).expect("locales/en/approval.ftl parses"))
}

pub(crate) fn tr(id: &str) -> String {
    catalog().tr(id, &[])
}

pub(crate) fn tr_args(id: &str, args: &[(&str, &str)]) -> String {
    catalog().tr(id, args)
}

/// The transcript rows that stand in for an approval once it has a card: its heading, and,
/// when it is answered, who answered and when. An unanswered request has its pinned card.
pub fn card_rows(card: &ApprovalCard, out: &mut Vec<Row>) {
    let (tone, summary) = match &card.state {
        CardState::Resolved { summary, .. } => (Tone::Success, Some(summary)),
        CardState::AlreadyResolved { summary, .. } => (Tone::Warning, Some(summary)),
        _ => (Tone::Warning, None),
    };
    out.push(Row::new(
        RowKind::Interrupt,
        if summary.is_some() { Tone::Muted } else { tone },
        1,
        tr_args(
            "approval-heading-detail",
            &[("detail", card.title.as_str())],
        ),
    ));
    if let Some(summary) = summary {
        out.push(Row::new(RowKind::Interrupt, tone, 2, summary.clone()));
    }
}
